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
//! struggling. Deflate happens there too, for the same reason.
//!
//! # Scrolls compose (0.24)
//!
//! Damage is no longer only a row set: a frame can also carry a *shift* the
//! receiver applies first (scrolling output would otherwise change every row
//! and read as "send the whole grid"). Two queued scrolls compose by summing
//! their deltas and re-indexing the older row set through the newer shift —
//! still exact, because a row the older frame owed is either still on screen,
//! where the newest snapshot refreshes it, or has scrolled off and is nobody's
//! business. A header-only frame is the identity element: every frame carries
//! the current header, so it adds no rows to whatever it merges with.
//!
//! # The window (0.24)
//!
//! Coalescing bounds *this* queue. It does not bound the unix send buffer,
//! ssh's 2MB per-channel window or the TCP send buffer — ~2.4MB downstream
//! that the daemon can neither see nor merge, and where the 1053ms above was
//! actually sitting. Clients ack what they receive ([`MAX_UNACKED`]) and grids
//! wait here, merging, until there is room. That is what makes staleness a
//! function of frames rather than of buffer bytes.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use crate::runtime::protocol::GuiEvent;
use crate::runtime::snapshot::{compress_frame, encode_grid_bin_spec, FrameSpec, GridSnapshot};

/// What the engine hands a connection for one pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Push {
    /// The whole grid.
    Full,
    /// These rows changed.
    Damage(Vec<u16>),
    /// The screen scrolled by `delta` rows (positive = content moved up) and
    /// these rows still differ afterwards.
    Scroll { delta: i16, rows: Vec<u16> },
    /// Nothing on screen changed — only the header (title / cursor).
    HeaderOnly,
}

/// What a connection still owes the wire for one pane.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Damage {
    /// Nothing but the header. The identity element for merging: every frame
    /// carries the current header anyway, so this adds no rows to whatever it
    /// merges with.
    Header,
    /// Dirty row indices. `BTreeSet` because the encoder wants them ascending
    /// and merging is a set union.
    Rows(BTreeSet<u16>),
    /// A net shift the receiver applies before the rows. Composing two scrolls
    /// sums the deltas and re-indexes the older row set through the newer
    /// shift — see [`merge`].
    Scroll { delta: i32, rows: BTreeSet<u16> },
    /// Send the whole grid. Absorbing: merging anything into `Full` is `Full`.
    Full,
}

impl Damage {
    fn from_push(push: Push) -> Self {
        match push {
            Push::Full => Damage::Full,
            Push::HeaderOnly => Damage::Header,
            Push::Damage(rows) => Damage::Rows(rows.into_iter().collect()),
            Push::Scroll { delta, rows } => Damage::Scroll {
                delta: delta as i32,
                rows: rows.into_iter().collect(),
            },
        }
    }

    fn into_push(self) -> Push {
        match self {
            Damage::Full => Push::Full,
            Damage::Header => Push::HeaderOnly,
            Damage::Rows(rows) => Push::Damage(rows.into_iter().collect()),
            Damage::Scroll { delta, rows } => Push::Scroll {
                delta: delta as i16,
                rows: rows.into_iter().collect(),
            },
        }
    }
}

/// Re-index a row set through a later shift of `delta` rows: a row that sat at
/// `r` before the shift sits at `r - delta` after it, and rows pushed off the
/// grid drop out.
fn reindex(rows: BTreeSet<u16>, delta: i32, height: u16) -> BTreeSet<u16> {
    rows.into_iter()
        .filter_map(|r| {
            let moved = r as i32 - delta;
            (moved >= 0 && moved < height as i32).then_some(moved as u16)
        })
        .collect()
}

struct PendingGrid {
    /// Always the newest snapshot: it carries current cells, cursor and title,
    /// and the older ones have nothing left to contribute.
    snap: Arc<GridSnapshot>,
    damage: Damage,
    /// When this slot was created, for the queue-wait probe. A merge keeps the
    /// *oldest* stamp: what matters is how long the client has been waiting to
    /// see the present, not when the newest frame happened to arrive.
    queued_at: std::time::Instant,
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
        push: Push,
        /// Per-connection frame counter the client echoes back. See
        /// [`MAX_UNACKED`].
        seq: u64,
    },
}

impl Outgoing {
    /// Encode into the event that goes on the wire.
    ///
    /// Compression happens here, at drain time, so a connection that merged
    /// ten frames into one pays for one deflate rather than ten — and the
    /// saving lands on the link that is already the constraint.
    pub fn into_event(self) -> GuiEvent {
        match self {
            Outgoing::Event(ev) => ev,
            Outgoing::Grid { snap, push, seq } => {
                let spec = match &push {
                    Push::Full => FrameSpec::Full,
                    Push::HeaderOnly => FrameSpec::HeaderOnly,
                    Push::Damage(rows) => FrameSpec::Damage(rows),
                    Push::Scroll { delta, rows } => FrameSpec::Scroll {
                        delta: *delta,
                        rows,
                    },
                };
                match encode_grid_bin_spec(&snap, spec) {
                    Ok(bytes) => {
                        use base64::Engine as _;
                        let bytes = compress_frame(bytes);
                        GuiEvent::GridBin {
                            pane: snap.pane.clone(),
                            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
                            seq,
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

/// Grid frames allowed on the wire before the client has to catch up.
///
/// This is the ceiling on how stale what you are looking at can be: the
/// coalescer bounds the daemon's own queue, but between it and the eye sit the
/// unix send buffer, ssh's 2MB per-channel window and the TCP send buffer —
/// megabytes the daemon cannot see into and cannot merge. Acking closes that
/// loop. Eight is chosen to be invisible on a healthy link (at 62fps it is
/// 129ms of frames, and a local ack returns in microseconds) while still
/// capping a bad one at eight frames instead of a megabyte of history.
const MAX_UNACKED: u64 = 8;

/// How long to wait on an ack before deciding the client is not acking at all
/// and sending anyway. A client that never acks then behaves exactly as it did
/// before flow control existed, which is the right failure direction: a stall
/// here would freeze a pane.
const ACK_STALL: std::time::Duration = std::time::Duration::from_secs(2);

struct Inner {
    slots: VecDeque<Slot>,
    grids: HashMap<String, PendingGrid>,
    /// Set when the consumer is gone. Pushes then report dead so the engine
    /// prunes the connection, exactly as a failed `mpsc::send` used to.
    closed: bool,
    /// Grid frames handed to the writer so far.
    sent: u64,
    /// Highest seq the client has acknowledged.
    acked: u64,
    /// When the client last acked anything.
    last_ack: std::time::Instant,
}

impl Inner {
    /// Room for another grid frame?
    ///
    /// Also true once the client has been silent past [`ACK_STALL`] — we
    /// cannot tell a broken ack path from a very slow one, and of the two
    /// readings "send anyway" is the safe one.
    fn window_open(&self) -> bool {
        self.sent - self.acked < MAX_UNACKED || self.last_ack.elapsed() >= ACK_STALL
    }
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
                sent: 0,
                acked: 0,
                last_ack: std::time::Instant::now(),
            }),
            ready: Condvar::new(),
        }
    }

    /// Client reports the highest grid frame it has received.
    pub fn ack(&self, seq: u64) {
        let mut q = self.inner.lock().unwrap();
        if seq > q.acked {
            q.acked = seq.min(q.sent);
            q.last_ack = std::time::Instant::now();
            // The window may have just opened on a pane whose frame is waiting.
            self.ready.notify_one();
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
    /// writer hasn't taken it yet.
    ///
    /// `false` = connection is gone.
    pub fn push_grid(&self, snap: Arc<GridSnapshot>, push: Push) -> bool {
        let mut q = self.inner.lock().unwrap();
        if q.closed {
            return false;
        }
        let pane = snap.pane.clone();
        let incoming = Damage::from_push(push);
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
                        queued_at: std::time::Instant::now(),
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
    /// signal to stop. A batch that is only grids and no room in the ack
    /// window is *not* "nothing to send" — it waits, and the frames keep
    /// merging while it does.
    pub fn drain_blocking(&self) -> Vec<Outgoing> {
        let mut q = self.inner.lock().unwrap();
        loop {
            if q.closed {
                return Vec::new();
            }
            let batch = Self::take_ready(&mut q);
            if !batch.is_empty() {
                return batch;
            }
            q = if q.slots.is_empty() {
                self.ready.wait(q).unwrap()
            } else {
                // Held by the ack window: wake on an ack, or when the stall
                // timeout makes the window open by itself.
                let wait = ACK_STALL.saturating_sub(q.last_ack.elapsed());
                self.ready
                    .wait_timeout(q, wait.max(std::time::Duration::from_millis(1)))
                    .unwrap()
                    .0
            };
        }
    }

    /// Non-blocking variant. Test-only: the daemon's writer always blocks.
    #[cfg(test)]
    pub fn drain_now(&self) -> Vec<Outgoing> {
        let mut q = self.inner.lock().unwrap();
        Self::take_ready(&mut q)
    }

    /// Take everything the window allows. Events are never windowed — dropping
    /// or delaying one is a correctness bug, and they are small.
    fn take_ready(q: &mut Inner) -> Vec<Outgoing> {
        let mut out = Vec::with_capacity(q.slots.len());
        let mut held: VecDeque<Slot> = VecDeque::new();
        while let Some(slot) = q.slots.pop_front() {
            let inner = &mut *q;
            match slot {
                Slot::Event(ev) => out.push(Outgoing::Event(ev)),
                Slot::Grid(pane) if !inner.window_open() => {
                    // Keep this pane's place in the FIFO; its payload goes on
                    // merging in `grids` until the window opens.
                    held.push_back(Slot::Grid(pane));
                }
                Slot::Grid(pane) => {
                    if let Some(p) = inner.grids.remove(&pane) {
                        let push = p.damage.into_push();
                        // An empty row set means nothing changed at all. The
                        // engine skips those before they ever get here; if one
                        // slips through, sending it would encode a frame the
                        // paint path reads as a no-op. `HeaderOnly` is a
                        // different thing and does go out.
                        let empty = match &push {
                            Push::Damage(d) => d.is_empty(),
                            Push::Scroll { rows, delta } => rows.is_empty() && *delta == 0,
                            _ => false,
                        };
                        if empty {
                            continue;
                        }
                        inner.sent += 1;
                        let seq = inner.sent;
                        // How long this pane's newest state sat here before it
                        // got a turn on the wire. Between the daemon's
                        // `input→gridpush` probe and the GUI's `bridge age`
                        // this was the one unmeasured stage — and the one the
                        // 1053ms was hiding in.
                        crate::latency_probe::record(
                            "daemon grid queue wait",
                            p.queued_at.elapsed().as_micros() as u64,
                        );
                        out.push(Outgoing::Grid {
                            snap: p.snap,
                            push,
                            seq,
                        });
                    }
                }
            }
        }
        // Held panes go back at the front, in order, ahead of anything pushed
        // while we were draining.
        while let Some(slot) = held.pop_back() {
            q.slots.push_front(slot);
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

/// Compose two consecutive frames into one.
///
/// `a` is what the connection already owed, `b` is what just arrived. The
/// payload is always the newest snapshot restricted to the composed row set,
/// so composing is a set operation, never a delta chain.
///
/// Scrolls compose by summing deltas and re-indexing the older row set through
/// the newer shift. The result is exact: a row the older frame owed is either
/// still on screen (at its new index, refreshed from the newest snapshot) or
/// has scrolled off and is nobody's business.
fn merge(a: Damage, b: Damage, rows: u16) -> Damage {
    match (a, b) {
        // The header rides in every frame, so a header-only frame contributes
        // nothing to a merge in either direction.
        (Damage::Header, other) | (other, Damage::Header) => other,
        (Damage::Full, _) | (_, Damage::Full) => Damage::Full,
        // A scroll on either side turns the composite into a scroll.
        (x, Damage::Scroll { delta, rows: y }) => {
            let older = match x {
                Damage::Rows(r) => (0, r),
                Damage::Scroll { delta: d, rows: r } => (d, r),
                _ => unreachable!("Full and Header handled above"),
            };
            let mut set = reindex(older.1, delta, rows);
            set.extend(y);
            let total = older.0 + delta;
            if set.len() * 2 >= rows.max(1) as usize || total.unsigned_abs() >= rows.max(1) as u32 {
                Damage::Full
            } else {
                Damage::Scroll {
                    delta: total,
                    rows: set,
                }
            }
        }
        // No new shift: the pending scroll keeps its delta and absorbs the rows.
        (Damage::Scroll { delta, rows: mut x }, Damage::Rows(y)) => {
            x.extend(y);
            if x.len() * 2 >= rows.max(1) as usize {
                Damage::Full
            } else {
                Damage::Scroll { delta, rows: x }
            }
        }
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

    fn framing_of(out: &Outgoing) -> Push {
        match out {
            Outgoing::Grid { push, .. } => push.clone(),
            _ => panic!("expected a grid"),
        }
    }

    /// The point of the whole module: frames that pile up behind a stalled
    /// writer leave as ONE frame carrying the union of what changed.
    #[test]
    fn backed_up_frames_collapse_to_one_carrying_the_union() {
        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![1, 2]));
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![2, 3]));
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![9]));
        assert_eq!(q.depth(), 1, "three frames should occupy one slot");

        let out = q.drain_now();
        assert_eq!(out.len(), 1);
        assert_eq!(framing_of(&out[0]), Push::Damage(vec![1, 2, 3, 9]));
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
        q.push_grid(first, Push::Damage(vec![1]));
        q.push_grid(second, Push::Damage(vec![2]));

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
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![1, 2]));
        q.push_grid(snap("p", 100, 50), Push::Damage(vec![3]));

        let out = q.drain_now();
        assert_eq!(out.len(), 1);
        assert_eq!(
            framing_of(&out[0]),
            Push::Full,
            "reflow must send a full frame"
        );
    }

    /// Full is absorbing in both directions — a title-only frame (which is
    /// sent full) merged with row damage stays full.
    #[test]
    fn full_absorbs_in_both_directions() {
        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), Push::Full);
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![4]));
        assert_eq!(framing_of(&q.drain_now()[0]), Push::Full);

        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![4]));
        q.push_grid(snap("p", 80, 40), Push::Full);
        assert_eq!(framing_of(&q.drain_now()[0]), Push::Full);
    }

    /// Past half the rows, damage stops paying for itself — the same rule the
    /// engine applies when it first chooses damage over full.
    #[test]
    fn union_past_half_the_rows_becomes_full() {
        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 10), Push::Damage(vec![0, 1, 2]));
        q.push_grid(snap("p", 80, 10), Push::Damage(vec![3, 4]));
        assert_eq!(
            framing_of(&q.drain_now()[0]),
            Push::Full,
            "5 of 10 rows should promote to full"
        );
    }

    /// Different panes never merge into each other.
    #[test]
    fn panes_coalesce_independently() {
        let q = OutQueue::new();
        q.push_grid(snap("a", 80, 40), Push::Damage(vec![1]));
        q.push_grid(snap("b", 80, 40), Push::Damage(vec![2]));
        q.push_grid(snap("a", 80, 40), Push::Damage(vec![5]));
        assert_eq!(q.depth(), 2);

        let out = q.drain_now();
        assert_eq!(out.len(), 2);
        assert_eq!(framing_of(&out[0]), Push::Damage(vec![1, 5]));
        assert_eq!(framing_of(&out[1]), Push::Damage(vec![2]));
    }

    /// Semantic events are never dropped or reordered, and a grid merging
    /// behind them keeps its original place in the FIFO.
    #[test]
    fn semantic_events_keep_strict_order_around_a_merging_grid() {
        let q = OutQueue::new();
        q.push_event(GuiEvent::Pong);
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![1]));
        q.push_event(GuiEvent::PaneKilled {
            slug: "p".to_string(),
        });
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![2]));

        let out = q.drain_now();
        assert_eq!(out.len(), 3, "the second grid merged, the events did not");
        assert!(matches!(out[0], Outgoing::Event(GuiEvent::Pong)));
        assert_eq!(framing_of(&out[1]), Push::Damage(vec![1, 2]));
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
            q.push_grid(snap("p", 80, 40), Push::Damage(vec![row]));
            let out = q.drain_now();
            assert_eq!(out.len(), 1);
            assert_eq!(framing_of(&out[0]), Push::Damage(vec![row]));
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
        assert!(!q.push_grid(snap("p", 80, 40), Push::Damage(vec![1])));
        assert!(q.drain_now().is_empty());
    }

    // -- scroll composition ---------------------------------------------------

    /// A pane whose every row is distinct, scrolled to `first_line`.
    fn text_snap(rows: u16, first_line: usize) -> Arc<GridSnapshot> {
        use crate::runtime::snapshot::CellSnap;
        let cols = 40usize;
        let mut cells = vec![CellSnap::blank(); cols * rows as usize];
        // Full-width, varied content: a sparse grid RLE-compresses to nothing
        // and would flatter the comparison against a full frame.
        for r in 0..rows as usize {
            let line: String = (0..cols)
                .map(|i| char::from_u32(33 + ((first_line + r) * 7 + i * 3) as u32 % 90).unwrap())
                .collect();
            for (i, ch) in line.chars().enumerate() {
                cells[r * cols + i].c = ch;
            }
        }
        let mut s = (*snap("p", cols as u16, rows)).clone();
        s.rev = first_line as u64;
        s.cells = cells;
        Arc::new(s)
    }

    /// How the engine frames `next` against `prev`.
    fn framing_for(prev: &GridSnapshot, next: &GridSnapshot) -> Push {
        use crate::runtime::snapshot::{row_hashes, scroll_shift};
        let (cols, rows) = (prev.cols as usize, prev.rows as usize);
        let (a, b) = (
            row_hashes(&prev.cells, cols, rows),
            row_hashes(&next.cells, cols, rows),
        );
        let (delta, rows_after) = scroll_shift(&a, &b).expect("these grids did scroll");
        Push::Scroll {
            delta,
            rows: rows_after,
        }
    }

    fn decode_onto(out: &Outgoing, base: &GridSnapshot) -> GridSnapshot {
        use base64::Engine as _;
        let ev = match out {
            Outgoing::Grid { snap, push, seq } => Outgoing::Grid {
                snap: Arc::clone(snap),
                push: push.clone(),
                seq: *seq,
            },
            _ => panic!("expected a grid"),
        }
        .into_event();
        let GuiEvent::GridBin { data_b64, .. } = ev else {
            panic!("expected a binary grid")
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .unwrap();
        crate::runtime::snapshot::decode_grid_bin_onto(&bytes, Some(base)).unwrap()
    }

    /// The property the whole scroll path rests on: whatever the queue merges,
    /// the single frame it finally sends must reconstruct the newest state
    /// exactly when applied to what the client last had.
    #[test]
    fn merged_scrolls_reconstruct_the_newest_screen_exactly() {
        let base = text_snap(20, 0);
        let s1 = text_snap(20, 3);
        let s2 = text_snap(20, 5);
        let s3 = text_snap(20, 9);

        let q = OutQueue::new();
        q.push_grid(Arc::clone(&s1), framing_for(&base, &s1));
        q.push_grid(Arc::clone(&s2), framing_for(&s1, &s2));
        q.push_grid(Arc::clone(&s3), framing_for(&s2, &s3));
        let out = q.drain_now();
        assert_eq!(out.len(), 1, "three scrolls should coalesce into one frame");
        assert_eq!(
            framing_of(&out[0]),
            Push::Scroll {
                delta: 9,
                rows: (11..20).collect()
            },
            "deltas sum and the exposed rows re-index"
        );
        assert_eq!(decode_onto(&out[0], &base).cells, s3.cells);
    }

    /// Row damage that arrives before a scroll has to travel with it — at the
    /// index the shift moved it to.
    #[test]
    fn damage_then_scroll_reindexes_the_older_rows() {
        let base = text_snap(20, 0);
        let mut edited = (*base).clone();
        edited.cells[9 * 40].c = 'Z'; // row 9 changed in place
        let edited = Arc::new(edited);
        let scrolled = text_snap(20, 4);

        let q = OutQueue::new();
        q.push_grid(Arc::clone(&edited), Push::Damage(vec![9]));
        q.push_grid(Arc::clone(&scrolled), framing_for(&edited, &scrolled));
        let out = q.drain_now();
        assert_eq!(out.len(), 1);
        let Push::Scroll { delta, rows } = framing_of(&out[0]) else {
            panic!("expected a scroll")
        };
        assert_eq!(delta, 4);
        assert!(rows.contains(&5), "row 9 shifted up to 5: {rows:?}");
        assert_eq!(decode_onto(&out[0], &base).cells, scrolled.cells);
    }

    /// Rows that scroll off the top are nobody's business any more.
    #[test]
    fn rows_scrolled_off_the_top_drop_out_of_the_merge() {
        let mut rows: BTreeSet<u16> = BTreeSet::new();
        rows.extend([0u16, 1, 2, 8]);
        assert_eq!(
            reindex(rows, 3, 20),
            [5u16].into_iter().collect::<BTreeSet<u16>>()
        );
    }

    #[test]
    fn a_scroll_past_the_screen_height_gives_up_and_sends_full() {
        let q = OutQueue::new();
        for _ in 0..6 {
            q.push_grid(
                snap("p", 80, 20),
                Push::Scroll {
                    delta: 5,
                    rows: vec![19],
                },
            );
        }
        assert_eq!(framing_of(&q.drain_now()[0]), Push::Full);
    }

    /// A title-only frame is the identity element: it must neither promote a
    /// pending damage set to full nor lose the rows already owed.
    #[test]
    fn header_only_frames_merge_as_identity() {
        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![3]));
        q.push_grid(snap("p", 80, 40), Push::HeaderOnly);
        assert_eq!(framing_of(&q.drain_now()[0]), Push::Damage(vec![3]));

        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), Push::HeaderOnly);
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![3]));
        assert_eq!(framing_of(&q.drain_now()[0]), Push::Damage(vec![3]));

        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), Push::HeaderOnly);
        q.push_grid(snap("p", 80, 40), Push::HeaderOnly);
        assert_eq!(framing_of(&q.drain_now()[0]), Push::HeaderOnly);
    }

    /// A header-only frame carries no rows, so it must still reach the wire —
    /// the sidebar's working badge is driven by the title it carries.
    #[test]
    fn header_only_frames_are_not_dropped_as_empty() {
        let q = OutQueue::new();
        q.push_grid(text_snap(20, 0), Push::HeaderOnly);
        assert_eq!(q.drain_now().len(), 1);
    }

    // -- send window ----------------------------------------------------------

    /// The window bounds what can be in flight. Past it, frames stop going out
    /// and start merging instead — which is exactly what should happen, since
    /// the client is looking at the newest state either way.
    #[test]
    fn grids_stop_at_the_window_and_merge_behind_it() {
        let q = OutQueue::new();
        for row in 0..(MAX_UNACKED as u16 + 6) {
            q.push_grid(snap("p", 80, 40), Push::Damage(vec![row]));
            // Drain one at a time, as a writer keeping up with a quiet client
            // would; nothing is acked, so the window closes at MAX_UNACKED.
            let _ = q.drain_now();
        }
        let sent = q.inner.lock().unwrap().sent;
        assert_eq!(sent, MAX_UNACKED, "the window capped what went out");

        // Acking opens it again, and the held frames come out merged into one.
        q.ack(MAX_UNACKED);
        let out = q.drain_now();
        assert_eq!(out.len(), 1, "the backlog collapsed to a single frame");
    }

    /// Events are correctness-carrying and must never be windowed.
    #[test]
    fn semantic_events_flow_through_a_closed_window() {
        let q = OutQueue::new();
        for row in 0..(MAX_UNACKED as u16 + 4) {
            q.push_grid(snap("p", 80, 40), Push::Damage(vec![row]));
            let _ = q.drain_now();
        }
        q.push_event(GuiEvent::Pong);
        let out = q.drain_now();
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Outgoing::Event(GuiEvent::Pong)));
    }

    /// A client that never acks must degrade to the old behavior, not freeze.
    /// The stall escape is what guarantees that.
    #[test]
    fn a_client_that_never_acks_is_not_starved() {
        let q = OutQueue::new();
        for row in 0..(MAX_UNACKED as u16 + 3) {
            q.push_grid(snap("p", 80, 40), Push::Damage(vec![row]));
            let _ = q.drain_now();
        }
        assert_eq!(q.inner.lock().unwrap().sent, MAX_UNACKED, "window closed");
        // Pretend the client has been silent past the stall timeout.
        q.inner.lock().unwrap().last_ack = std::time::Instant::now() - ACK_STALL;
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![1]));
        assert_eq!(q.drain_now().len(), 1, "stall escape must let it through");
    }

    /// Held frames keep their place: a pane that was windowed off does not
    /// jump behind panes that were pushed later.
    #[test]
    fn held_frames_keep_their_place_in_the_fifo() {
        let q = OutQueue::new();
        for row in 0..MAX_UNACKED as u16 {
            q.push_grid(snap("filler", 80, 40), Push::Damage(vec![row]));
            let _ = q.drain_now();
        }
        q.push_grid(snap("a", 80, 40), Push::Damage(vec![1]));
        q.push_grid(snap("b", 80, 40), Push::Damage(vec![2]));
        assert!(q.drain_now().is_empty(), "window is closed");
        q.ack(MAX_UNACKED);
        let out = q.drain_now();
        assert_eq!(out.len(), 2);
        match (&out[0], &out[1]) {
            (Outgoing::Grid { snap: x, .. }, Outgoing::Grid { snap: y, .. }) => {
                assert_eq!((x.pane.as_str(), y.pane.as_str()), ("a", "b"));
            }
            _ => panic!("expected two grids"),
        }
    }

    /// An ack can never claim more than was sent.
    #[test]
    fn a_lying_ack_cannot_open_the_window_wider_than_it_is() {
        let q = OutQueue::new();
        q.push_grid(snap("p", 80, 40), Push::Damage(vec![1]));
        let _ = q.drain_now();
        q.ack(u64::MAX);
        let inner = q.inner.lock().unwrap();
        assert_eq!(inner.acked, inner.sent);
    }

    /// What the scroll op is responsible for: carrying 3 exposed rows instead
    /// of 70. Measured before deflate, since how well the *content* then
    /// compresses is a separate property (and a synthetic grid compresses far
    /// better than a real screen would).
    #[test]
    fn a_scroll_frame_carries_only_the_exposed_rows() {
        let base = text_snap(70, 0);
        let next = text_snap(70, 3);
        let Push::Scroll { delta, rows } = framing_for(&base, &next) else {
            panic!("expected a scroll")
        };
        assert_eq!(delta, 3);
        assert_eq!(rows.len(), 3, "only the newly exposed rows");
        let scroll = encode_grid_bin_spec(&next, FrameSpec::Scroll { delta, rows: &rows })
            .unwrap()
            .len();
        let full = encode_grid_bin_spec(&next, FrameSpec::Full).unwrap().len();
        assert!(scroll * 10 < full, "scroll {scroll} vs full {full}");
    }
}
