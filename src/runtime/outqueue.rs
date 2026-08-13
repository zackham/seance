//! Per-connection outbound queue, with grid frames that **coalesce**.
//!
//! The daemon used to hand every push straight to an unbounded `mpsc` feeding
//! a blocking socket write. On a healthy link that is fine — the queue is
//! always empty. On a degraded one it is the whole problem: the write blocks,
//! the engine keeps generating grid frames at up to 62fps, and they pile up
//! without a ceiling. Every frame the client then receives is *stale*; it is
//! watching a backlog drain rather than the present. Measured on a bad wifi
//! association: `key→grid-apply` p50 1053ms, max 8355ms, while every
//! GUI-side stage stayed under a millisecond.
//!
//! A grid frame is worthless the moment a newer one for that pane exists, so
//! this queue keeps **one pending frame per pane** and merges into it.
//!
//! Merging is exact rather than lossy, which is the part that makes this
//! cheap. Damage is a list of dirty *row* indices, and the payload is the
//! **current** snapshot restricted to those rows — not a diff against a base.
//! So composing N queued frames is: union the row sets, encode against the
//! newest snapshot. No delta chain to break, and therefore no full-frame
//! resync, which would have meant sending *more* bytes exactly when the link
//! is least able to carry them.
//!
//! Consequences worth stating:
//!
//! - **Healthy link**: the queue is empty when each frame arrives, nothing
//!   ever merges, behavior is byte-identical to before.
//! - **Degraded link**: the backlog collapses to the newest state per pane.
//!   The client renders as often as the link can carry, always showing the
//!   present.
//! - **Recovery**: immediate and automatic. There is no threshold, no
//!   hysteresis, and no rate controller to mistune — the effective frame rate
//!   *is* the link's measured capacity.
//!
//! Only grids coalesce. `State`, `Ack`, `PaneSpawned`, `RailPrefs` and friends
//! are semantic: dropping one is a correctness bug, so they hold strict FIFO
//! order. A grid merges into its existing slot's *position*, so ordering
//! relative to those events is preserved too.
//!
//! Encoding happens at drain time, not at push time. A backed-up connection
//! therefore encodes once per frame it actually sends instead of once per
//! frame it never sends — the CPU saving lands on the machine already
//! struggling.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use crate::runtime::protocol::GuiEvent;
use crate::runtime::snapshot::{encode_grid_bin_ex, GridSnapshot};

/// What a connection still owes the wire for one pane.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Damage {
    /// Dirty row indices. `BTreeSet` because the encoder wants them ascending
    /// and merging is a set union.
    Rows(BTreeSet<u16>),
    /// Send the whole grid. Absorbing: merging anything into `Full` is `Full`.
    Full,
}

impl Damage {
    fn from_dirty(dirty: Option<Vec<u16>>) -> Self {
        match dirty {
            None => Damage::Full,
            Some(rows) => Damage::Rows(rows.into_iter().collect()),
        }
    }

    /// `None` means "encode the full grid" to [`encode_grid_bin_ex`].
    fn into_dirty(self) -> Option<Vec<u16>> {
        match self {
            Damage::Full => None,
            Damage::Rows(rows) => Some(rows.into_iter().collect()),
        }
    }
}

struct PendingGrid {
    /// Always the newest snapshot: it carries current cells, cursor and title,
    /// and the older ones have nothing left to contribute.
    snap: Arc<GridSnapshot>,
    damage: Damage,
}

/// A queue slot. Grids are indirected through `grids` so a merge updates the
/// payload in place while keeping this position in the FIFO.
enum Slot {
    Event(GuiEvent),
    Grid(String),
}

/// One item ready for the wire. Encoding is the caller's job, off the lock.
pub enum Outgoing {
    Event(GuiEvent),
    Grid {
        snap: Arc<GridSnapshot>,
        dirty: Option<Vec<u16>>,
    },
}

impl Outgoing {
    /// Encode into the event that goes on the wire. Mirrors the daemon's
    /// previous inline encode, including the JSON fallback.
    pub fn into_event(self) -> GuiEvent {
        match self {
            Outgoing::Event(ev) => ev,
            Outgoing::Grid { snap, dirty } => {
                match encode_grid_bin_ex(&snap, dirty.as_deref()) {
                    Ok(bytes) => {
                        use base64::Engine as _;
                        GuiEvent::GridBin {
                            pane: snap.pane.clone(),
                            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[seance daemon] grid_bin encode failed: {e}; falling back to JSON"
                        );
                        // Unwrap the Arc when we can; the fallback is cold.
                        GuiEvent::Grid(Arc::try_unwrap(snap).unwrap_or_else(|a| (*a).clone()))
                    }
                }
            }
        }
    }
}

struct Inner {
    slots: VecDeque<Slot>,
    grids: HashMap<String, PendingGrid>,
    /// Set when the consumer is gone. Pushes then report dead so the engine
    /// prunes the connection, exactly as a failed `mpsc::send` used to.
    closed: bool,
}

/// Shared between the engine (producer) and one connection's writer thread.
pub struct OutQueue {
    inner: Mutex<Inner>,
    ready: Condvar,
}

impl Default for OutQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl OutQueue {
    pub fn new() -> Self {
        OutQueue {
            inner: Mutex::new(Inner {
                slots: VecDeque::new(),
                grids: HashMap::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// Queue a semantic event. Never coalesced. `false` = connection is gone.
    pub fn push_event(&self, ev: GuiEvent) -> bool {
        let mut q = self.inner.lock().unwrap();
        if q.closed {
            return false;
        }
        q.slots.push_back(Slot::Event(ev));
        self.ready.notify_one();
        true
    }

    /// Queue a grid frame, merging into this pane's pending frame if the
    /// writer hasn't taken it yet. `dirty: None` means a full frame.
    ///
    /// `false` = connection is gone.
    pub fn push_grid(&self, snap: Arc<GridSnapshot>, dirty: Option<Vec<u16>>) -> bool {
        let mut q = self.inner.lock().unwrap();
        if q.closed {
            return false;
        }
        let pane = snap.pane.clone();
        let incoming = Damage::from_dirty(dirty);
        match q.grids.get_mut(&pane) {
            Some(pending) => {
                // A reflow invalidates row indices on both sides — rows in the
                // pending set no longer name the same cells. Only a full frame
                // is honest here.
                let reflowed = pending.snap.cols != snap.cols || pending.snap.rows != snap.rows;
                let rows = snap.rows;
                pending.snap = snap;
                pending.damage = if reflowed {
                    Damage::Full
                } else {
                    merge(
                        std::mem::replace(&mut pending.damage, Damage::Full),
                        incoming,
                        rows,
                    )
                };
                // No new slot and no notify: the existing slot already holds
                // this pane's place in the FIFO, and the writer will pick up
                // the merged payload when it gets there.
            }
            None => {
                q.grids.insert(
                    pane.clone(),
                    PendingGrid {
                        snap,
                        damage: incoming,
                    },
                );
                q.slots.push_back(Slot::Grid(pane));
                self.ready.notify_one();
            }
        }
        true
    }

    /// Block until there is something to send, then take all of it.
    ///
    /// Returns empty only when the queue is closed, which is the writer's
    /// signal to stop.
    pub fn drain_blocking(&self) -> Vec<Outgoing> {
        let mut q = self.inner.lock().unwrap();
        while q.slots.is_empty() && !q.closed {
            q = self.ready.wait(q).unwrap();
        }
        Self::take_all(&mut q)
    }

    /// Non-blocking variant. Test-only: the daemon's writer always blocks.
    #[cfg(test)]
    pub fn drain_now(&self) -> Vec<Outgoing> {
        let mut q = self.inner.lock().unwrap();
        Self::take_all(&mut q)
    }

    fn take_all(q: &mut Inner) -> Vec<Outgoing> {
        let inner = &mut *q;
        let mut out = Vec::with_capacity(inner.slots.len());
        while let Some(slot) = inner.slots.pop_front() {
            match slot {
                Slot::Event(ev) => out.push(Outgoing::Event(ev)),
                Slot::Grid(pane) => {
                    if let Some(p) = inner.grids.remove(&pane) {
                        let dirty = p.damage.into_dirty();
                        // An empty row set means nothing changed. The engine
                        // skips those before they ever get here; if one slips
                        // through, sending it would encode a frame the paint
                        // path reads as a no-op.
                        if dirty.as_ref().is_some_and(|d| d.is_empty()) {
                            continue;
                        }
                        out.push(Outgoing::Grid {
                            snap: p.snap,
                            dirty,
                        });
                    }
                }
            }
        }
        out
    }

    /// Mark the consumer gone. Idempotent; wakes a blocked writer.
    pub fn close(&self) {
        let mut q = self.inner.lock().unwrap();
        q.closed = true;
        q.slots.clear();
        q.grids.clear();
        self.ready.notify_all();
    }

    /// Slots still waiting. Test/diagnostic only — a coalesced pane counts once.
    #[cfg(test)]
    pub fn depth(&self) -> usize {
        self.inner.lock().unwrap().slots.len()
    }
}

/// Union two damage sets, promoting to `Full` once the union stops being a
/// saving. The half-the-rows threshold is the same one `broadcast_grid` uses
/// to decide damage-vs-full in the first place, applied to the union.
fn merge(a: Damage, b: Damage, rows: u16) -> Damage {
    match (a, b) {
        (Damage::Full, _) | (_, Damage::Full) => Damage::Full,
        (Damage::Rows(mut x), Damage::Rows(y)) => {
            x.extend(y);
            if x.len() * 2 >= (rows.max(1)) as usize {
                Damage::Full
            } else {
                Damage::Rows(x)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pane: &str, cols: u16, rows: u16) -> Arc<GridSnapshot> {
        Arc::new(GridSnapshot {
            pane: pane.to_string(),
            rev: 0,
            cols,
            rows,
            cursor_col: 0,
            cursor_row: 0,
            cursor_shape_block: true,
            title: None,
            running: true,
            cells: Vec::new(),
            ghost: None,
            text: String::new(),
            alt_screen: false,
            alternate_scroll: false,
            app_cursor: false,
            mouse_mode: false,
            sgr_mouse: false,
            last_input_origin: None,
            hyperlinks: Vec::new(),
        })
    }

    fn dirty_of(out: &Outgoing) -> Option<Vec<u16>> {
        match out {
            Outgoing::Grid { dirty, .. } => dirty.clone(),
            _ => panic!("expected a grid"),
        }
    }

    /// The point of the whole module: frames that pile up behind a stalled
    /// writer leave as ONE frame carrying the union of what changed.
    #[test]
    fn backed_up_frames_collapse_to_one_carrying_the_union() {
        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), Some(vec![1, 2]));
        q.push_grid(snap("p", 80, 40), Some(vec![2, 3]));
        q.push_grid(snap("p", 80, 40), Some(vec![9]));
        assert_eq!(q.depth(), 1, "three frames should occupy one slot");

        let out = q.drain_now();
        assert_eq!(out.len(), 1);
        assert_eq!(dirty_of(&out[0]), Some(vec![1, 2, 3, 9]));
    }

    /// Merging keeps the NEWEST snapshot — the older ones have nothing left to
    /// contribute, and the payload is current values for the dirty rows.
    #[test]
    fn merge_keeps_the_newest_snapshot() {
        let q = OutQueue::new();
        let mut first = snap("p", 80, 40);
        Arc::get_mut(&mut first).unwrap().rev = 1;
        let mut second = snap("p", 80, 40);
        Arc::get_mut(&mut second).unwrap().rev = 7;
        q.push_grid(first, Some(vec![1]));
        q.push_grid(second, Some(vec![2]));

        let out = q.drain_now();
        match &out[0] {
            Outgoing::Grid { snap, .. } => assert_eq!(snap.rev, 7),
            _ => panic!("expected a grid"),
        }
    }

    /// A reflow mid-backlog invalidates row indices on both sides, so the
    /// merged frame must be full rather than a union naming the wrong cells.
    #[test]
    fn resize_mid_backlog_promotes_to_full() {
        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), Some(vec![1, 2]));
        q.push_grid(snap("p", 100, 50), Some(vec![3]));

        let out = q.drain_now();
        assert_eq!(out.len(), 1);
        assert_eq!(dirty_of(&out[0]), None, "reflow must send a full frame");
    }

    /// Full is absorbing in both directions — a title-only frame (which is
    /// sent full) merged with row damage stays full.
    #[test]
    fn full_absorbs_in_both_directions() {
        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), None);
        q.push_grid(snap("p", 80, 40), Some(vec![4]));
        assert_eq!(dirty_of(&q.drain_now()[0]), None);

        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), Some(vec![4]));
        q.push_grid(snap("p", 80, 40), None);
        assert_eq!(dirty_of(&q.drain_now()[0]), None);
    }

    /// Past half the rows, damage stops paying for itself — the same rule the
    /// engine applies when it first chooses damage over full.
    #[test]
    fn union_past_half_the_rows_becomes_full() {
        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 10), Some(vec![0, 1, 2]));
        q.push_grid(snap("p", 80, 10), Some(vec![3, 4]));
        assert_eq!(
            dirty_of(&q.drain_now()[0]),
            None,
            "5 of 10 rows should promote to full"
        );
    }

    /// Different panes never merge into each other.
    #[test]
    fn panes_coalesce_independently() {
        let q = OutQueue::new();
        q.push_grid(snap("a", 80, 40), Some(vec![1]));
        q.push_grid(snap("b", 80, 40), Some(vec![2]));
        q.push_grid(snap("a", 80, 40), Some(vec![5]));
        assert_eq!(q.depth(), 2);

        let out = q.drain_now();
        assert_eq!(out.len(), 2);
        assert_eq!(dirty_of(&out[0]), Some(vec![1, 5]));
        assert_eq!(dirty_of(&out[1]), Some(vec![2]));
    }

    /// Semantic events are never dropped or reordered, and a grid merging
    /// behind them keeps its original place in the FIFO.
    #[test]
    fn semantic_events_keep_strict_order_around_a_merging_grid() {
        let q = OutQueue::new();
        q.push_event(GuiEvent::Pong);
        q.push_grid(snap("p", 80, 40), Some(vec![1]));
        q.push_event(GuiEvent::PaneKilled {
            slug: "p".to_string(),
        });
        q.push_grid(snap("p", 80, 40), Some(vec![2]));

        let out = q.drain_now();
        assert_eq!(out.len(), 3, "the second grid merged, the events did not");
        assert!(matches!(out[0], Outgoing::Event(GuiEvent::Pong)));
        assert_eq!(dirty_of(&out[1]), Some(vec![1, 2]));
        assert!(matches!(
            out[2],
            Outgoing::Event(GuiEvent::PaneKilled { .. })
        ));
    }

    /// On a link that keeps up, every frame is taken before the next arrives,
    /// so nothing ever merges and the stream is what it always was.
    #[test]
    fn a_keeping_up_consumer_sees_every_frame_unmerged() {
        let q = OutQueue::new();
        for row in 0..5u16 {
            q.push_grid(snap("p", 80, 40), Some(vec![row]));
            let out = q.drain_now();
            assert_eq!(out.len(), 1);
            assert_eq!(dirty_of(&out[0]), Some(vec![row]));
        }
    }

    /// A closed queue reports dead so the engine prunes the connection, the
    /// way a failed `mpsc::send` used to.
    #[test]
    fn closed_queue_reports_dead_to_the_producer() {
        let q = OutQueue::new();
        assert!(q.push_event(GuiEvent::Pong));
        q.close();
        assert!(!q.push_event(GuiEvent::Pong));
        assert!(!q.push_grid(snap("p", 80, 40), Some(vec![1])));
        assert!(q.drain_now().is_empty());
    }
}
