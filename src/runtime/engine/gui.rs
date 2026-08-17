//! GUI-connection handling + grid push. Owns the live `GuiConn` window
//! registry, per-window state events, the damage/throttle grid-push path, and
//! `handle_gui` (GuiRequest dispatch).

use std::collections::HashSet;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use super::helpers::{base64_decode, now_ms};
use super::{Engine, EnginePane, SpawnSpec};
use crate::events;
use crate::runtime::outqueue::{OutQueue, Push};
use crate::runtime::protocol::*;
use crate::runtime::pty_session::SessionEvent;
use crate::runtime::snapshot::{
    compress_frame, dirty_rows, encode_grid_bin, encode_grid_bin_ex, row_hashes, scroll_shift,
    CellSnap, GridSnapshot,
};

/// One GUI window connection.
///
/// Workspaces are global and unowned (0.12): a connection holds a
/// *subscription set* — the workspaces it wants grid streams for — plus its own
/// selection/focus. Two windows may subscribe to the same workspace.
pub(super) struct GuiConn {
    id: String,
    out: Arc<OutQueue>,
    selected_workspace: Option<String>,
    focused_pane: Option<String>,
    overview: bool,
    subscriptions: HashSet<String>,
    /// Panes this connection has been sent a full frame for, and can therefore
    /// have damage applied against.
    ///
    /// Damage is only meaningful to a receiver holding the base it names. That
    /// is a per-connection fact, and it used to be answered globally: if *any*
    /// interested window lacked the base, *everyone* got a full grid. On a
    /// local window that costs nothing; on a remote one it was 74KB to move a
    /// cursor. Tracking it here means each connection gets the framing it
    /// actually needs.
    ///
    /// Self-maintaining: a pane enters the set when its first full frame is
    /// queued and leaves on reflow or death. If it were ever wrong, the
    /// receiver's decode fails and it asks for a refresh — the error path is a
    /// resync, not a corrupted screen.
    based: HashSet<String>,
}

/// Cached last broadcast for damage detection (Arc so we don't clone every push).
pub(super) struct LastGridFrame {
    cols: u16,
    rows: u16,
    cursor_col: u16,
    cursor_row: u16,
    /// OSC title — spinner-only title flips must still reach the GUI (sidebar
    /// "working" badges) even when cells/cursor are unchanged.
    title: Option<String>,
    cells: std::sync::Arc<Vec<CellSnap>>,
}

impl Engine {
    pub fn register_gui(&mut self, out: Arc<OutQueue>) -> String {
        let id = format!("w{}", self.next_window_seq);
        self.next_window_seq = self.next_window_seq.wrapping_add(1).max(1);
        self.gui_conns.push(GuiConn {
            id: id.clone(),
            out,
            selected_workspace: None,
            focused_pane: None,
            overview: false,
            subscriptions: HashSet::new(),
            based: HashSet::new(),
        });
        id
    }

    /// Test-only: this window's subscription set, ordered like the wire.
    #[cfg(test)]
    pub(super) fn subscriptions_of(&self, window_id: &str) -> Vec<String> {
        self.workspaces_for_window(window_id)
    }

    /// Test-only: is a GUI window with this id currently registered?
    #[cfg(test)]
    pub(super) fn has_gui_window(&self, window_id: &str) -> bool {
        self.gui_conns.iter().any(|c| c.id == window_id)
    }

    /// Test-only accessor for the pure grid-interval selection logic (no clocks).
    /// `None` = no connection is interested in this pane right now.
    #[cfg(test)]
    pub(super) fn grid_interval_ms_for(&self, slug: &str) -> Option<u64> {
        self.grid_interval_for(slug).map(|d| d.as_millis() as u64)
    }

    /// Drop a window from the registry. Workspaces are global — nothing is
    /// reassigned; the survivors only need a fresh roster.
    pub fn unregister_gui(&mut self, window_id: &str) {
        let was_registered = self.gui_conns.iter().any(|c| c.id == window_id);
        if !was_registered {
            return;
        }
        self.gui_conns.retain(|c| c.id != window_id);
        if self.gui_conns.is_empty() {
            return;
        }
        // Push without prune (avoid re-entrant unregister).
        let ids: Vec<String> = self.gui_conns.iter().map(|c| c.id.clone()).collect();
        for id in ids {
            let st = self.state_for_window(&id);
            self.send_to(&id, st);
        }
    }

    /// Drop connections whose send channel is dead.
    pub fn prune_dead_guis(&mut self) {
        let alive: Vec<String> = self
            .gui_conns
            .iter()
            .filter(|c| c.out.push_event(GuiEvent::Pong))
            .map(|c| c.id.clone())
            .collect();
        let dead: Vec<String> = self
            .gui_conns
            .iter()
            .filter(|c| !alive.iter().any(|a| a == &c.id))
            .map(|c| c.id.clone())
            .collect();
        for id in dead {
            self.unregister_gui(&id);
        }
    }

    pub fn broadcast(&mut self, ev: GuiEvent) {
        self.gui_conns.retain(|c| c.out.push_event(ev.clone()));
    }

    fn send_to(&mut self, window_id: &str, ev: GuiEvent) {
        self.gui_conns.retain(|c| {
            if c.id == window_id {
                c.out.push_event(ev.clone())
            } else {
                true
            }
        });
    }

    /// Fan a grid frame out to every connection currently streaming this pane
    /// (i.e. one with a `grid_interval_for` rate). Nobody interested → nobody
    /// gets it; there is no broadcast fallback, an unsubscribed workspace is
    /// deliberately silent on the wire (the recorder tap still runs — see
    /// `handle_session_event`).
    fn send_grid_to_subscribers(&mut self, pane: &str, snap: Arc<GridSnapshot>, push: Push) {
        // Queued unencoded: each connection coalesces independently (a slow
        // link merges, a fast one doesn't) and the winning frame is encoded
        // once, at drain time, by that connection's writer.
        let ids = self.conns_streaming(pane);
        // The engine already decided nobody can take a partial frame (first
        // frame for the pane, or a reflow that renamed every row).
        let global_full = matches!(push, Push::Full);
        self.gui_conns.retain_mut(|c| {
            if !ids.contains(&c.id) {
                // Missing this frame is exactly what makes a base stale, so a
                // connection that is not streaming this pane right now loses
                // its claim to one. Without this, a window that stopped
                // watching a circle (deselected it, closed the overview) and
                // came back would be sent damage against a screen that had
                // moved on underneath it.
                c.based.remove(pane);
                return true;
            }
            // A connection without the base can only be sent a whole grid,
            // whatever the others are getting.
            let mine = if c.based.contains(pane) && !global_full {
                push.clone()
            } else {
                c.based.insert(pane.to_string());
                Push::Full
            };
            c.out.push_grid(Arc::clone(&snap), mine)
        });
    }

    /// Send one connection the whole grid for `pane`, out of band.
    ///
    /// For the moments when a *single* window needs a base — it just attached,
    /// subscribed, selected the circle, or asked for a refresh. It used to be
    /// done by dropping the daemon's shared last-frame cache, which forced a
    /// full grid to every other window too.
    fn send_full_to(&mut self, window_id: &str, slug: &str) {
        let Some(snap) = self.snapshot_pane(slug) else {
            return;
        };
        let snap = Arc::new(snap);
        let slug = slug.to_string();
        self.gui_conns.retain_mut(|c| {
            if c.id != window_id {
                return true;
            }
            c.based.insert(slug.clone());
            c.out.push_grid(Arc::clone(&snap), Push::Full)
        });
    }

    /// Drop every connection's base for `pane` — the next frame each one gets
    /// will be a full grid. For death and respawn, where a reused slug must
    /// not inherit the old pane's base.
    pub(super) fn invalidate_bases(&mut self, pane: &str) {
        for c in &mut self.gui_conns {
            c.based.remove(pane);
        }
    }

    /// Window ids whose subscription/selection makes them want `pane`'s frames.
    fn conns_streaming(&self, pane: &str) -> Vec<String> {
        let Some(ws) = self
            .panes
            .iter()
            .find(|p| p.slug == pane)
            .map(|p| p.workspace.clone())
        else {
            return Vec::new();
        };
        self.gui_conns
            .iter()
            .filter(|c| Self::conn_rate_for(c, &ws).is_some())
            .map(|c| c.id.clone())
            .collect()
    }

    /// One connection's grid rate for a workspace: selected → 16ms, subscribed
    /// while the overview is open → 66ms (thumb rate), otherwise not streamed.
    fn conn_rate_for(conn: &GuiConn, workspace: &str) -> Option<Duration> {
        if conn.selected_workspace.as_deref() == Some(workspace) {
            return Some(Duration::from_millis(16));
        }
        if conn.overview && conn.subscriptions.contains(workspace) {
            return Some(Duration::from_millis(66));
        }
        None
    }

    pub(crate) fn push_state_to_all(&mut self) {
        self.prune_dead_guis();
        let ids: Vec<String> = self.gui_conns.iter().map(|c| c.id.clone()).collect();
        for id in ids {
            let st = self.state_for_window(&id);
            self.send_to(&id, st);
        }
    }

    fn window_infos(&self) -> Vec<WindowInfo> {
        self.gui_conns
            .iter()
            .map(|c| {
                let n = self.workspaces_for_window(&c.id).len();
                WindowInfo {
                    id: c.id.clone(),
                    label: self.window_label(&c.id),
                    workspace_count: n,
                }
            })
            .collect()
    }

    fn all_workspace_names(&self) -> Vec<String> {
        let mut set: HashSet<String> = self.panes.iter().map(|p| p.workspace.clone()).collect();
        for w in &self.extra_workspaces {
            set.insert(w.clone());
        }
        for w in &self.workspace_order {
            set.insert(w.clone());
        }
        let mut v: Vec<String> = set.into_iter().collect();
        v.sort();
        v
    }

    /// Every known workspace, ordered by `workspace_order` then alpha for the
    /// leftovers. This is the global `workspace_order` clients receive.
    fn all_workspace_names_ordered(&self) -> Vec<String> {
        let mut rest = self.all_workspace_names();
        let mut ordered = Vec::new();
        for w in &self.workspace_order {
            if rest.iter().any(|o| o == w) {
                ordered.push(w.clone());
                rest.retain(|o| o != w);
            }
        }
        ordered.extend(rest);
        ordered
    }

    /// This window's subscription set, ordered by `workspace_order` with the
    /// leftovers appended alphabetically.
    fn workspaces_for_window(&self, window_id: &str) -> Vec<String> {
        let Some(conn) = self.gui_conns.iter().find(|c| c.id == window_id) else {
            return Vec::new();
        };
        let mut subs: Vec<String> = conn.subscriptions.iter().cloned().collect();
        let mut ordered = Vec::new();
        for w in &self.workspace_order {
            if subs.iter().any(|o| o == w) {
                ordered.push(w.clone());
                subs.retain(|o| o != w);
            }
        }
        subs.sort();
        ordered.extend(subs);
        ordered
    }

    fn window_label(&self, window_id: &str) -> String {
        let ws = self.workspaces_for_window(window_id);
        match ws.len() {
            0 => "(empty)".into(),
            1 => ws[0].clone(),
            n => format!("{} +{}", ws[0], n - 1),
        }
    }

    /// The State push for one window. Everything except `selected_workspace` /
    /// `focused_pane` / `subscriptions` is GLOBAL — clients render their own
    /// active/parked split from the subscription set.
    fn state_for_window(&self, window_id: &str) -> GuiEvent {
        let subs = self.workspaces_for_window(window_id);
        let panes: Vec<PaneInfo> = self.pane_infos();
        let conn = self.gui_conns.iter().find(|c| c.id == window_id);
        // Selection is per-connection and must stay inside its subscriptions —
        // never fall back to another window's choice.
        let selected = conn
            .and_then(|c| c.selected_workspace.clone())
            .filter(|w| subs.iter().any(|s| s == w))
            .or_else(|| subs.first().cloned());
        let focused = conn
            .and_then(|c| c.focused_pane.clone())
            .filter(|s| panes.iter().any(|p| p.slug == *s));
        let extra: Vec<String> = self.extra_workspaces.clone();
        let order: Vec<String> = self.all_workspace_names_ordered();
        let statuses: Vec<StatusInfo> = self
            .statuses
            .iter()
            .filter(|(slug, _)| panes.iter().any(|p| p.slug == **slug))
            .map(|(slug, (state, note))| StatusInfo {
                slug: slug.clone(),
                state: state.clone(),
                note: note.clone(),
                pad_rev: self.pad_revs.get(slug).copied().unwrap_or(0),
            })
            .collect();
        let asks: Vec<AskInfo> = self
            .asks
            .iter()
            .filter(|a| a.answer.is_none())
            .map(|a| AskInfo {
                id: a.id.clone(),
                from: a.from.clone(),
                workspace: a.workspace.clone(),
                question: a.question.clone(),
                choices: a.choices.clone(),
                answer: a.answer.clone(),
            })
            .collect();
        // Daemon-owned activity clocks for EVERY known workspace, subscribed or
        // not — a parked circle must still show its real "time since update".
        let mut meta_names: Vec<String> = order.clone();
        for ws in self
            .workspace_output
            .keys()
            .chain(self.workspace_touch_ms.keys())
            .chain(self.pr_links.keys())
            .cloned()
        {
            if !meta_names.iter().any(|m| *m == ws) {
                meta_names.push(ws);
            }
        }
        let workspace_meta: Vec<WorkspaceMeta> = meta_names
            .into_iter()
            .map(|ws| WorkspaceMeta {
                last_output_ms: self.workspace_output.get(&ws).copied().unwrap_or(0),
                last_touch_ms: self.workspace_touch_ms.get(&ws).copied().unwrap_or(0),
                pr_links: self.pr_links.get(&ws).cloned().unwrap_or_default(),
                name: self.workspace_names.get(&ws).cloned(),
                workspace: ws,
            })
            .collect();
        GuiEvent::State {
            panes,
            selected_workspace: selected,
            focused_pane: focused,
            extra_workspaces: extra,
            workspace_order: order,
            asks,
            statuses,
            window_id: Some(window_id.to_string()),
            windows: self.window_infos(),
            subscriptions: subs,
            workspace_meta,
        }
    }

    /// Drop a workspace from every window's subscription/selection state —
    /// used when the workspace ceases to exist (kill or empty-prune).
    pub(super) fn drop_workspace_subs(&mut self, workspace: &str) {
        for c in &mut self.gui_conns {
            c.subscriptions.remove(workspace);
            if c.selected_workspace.as_deref() == Some(workspace) {
                c.selected_workspace = None;
            }
        }
    }

    /// Add `workspace` to this connection's subscription set.
    fn subscribe_conn(&mut self, window_id: &str, workspace: &str) {
        if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
            c.subscriptions.insert(workspace.to_string());
        }
    }

    /// Pack a grid as compact `grid_bin` (SCG3 full or row-damage).
    ///
    /// This is the one-off path (refresh, attach), pushed as a semantic event
    /// rather than through the coalescing grid queue — so it carries `seq: 0`,
    /// which clients read as "outside the flow-controlled stream, nothing to
    /// ack".
    fn grid_event(snap: GridSnapshot, dirty: Option<&[u16]>) -> GuiEvent {
        let enc = match dirty {
            Some(d) => encode_grid_bin_ex(&snap, Some(d)),
            None => encode_grid_bin(&snap),
        };
        match enc {
            Ok(bytes) => {
                use base64::Engine as _;
                let data_b64 =
                    base64::engine::general_purpose::STANDARD.encode(compress_frame(bytes));
                GuiEvent::GridBin {
                    pane: snap.pane.clone(),
                    data_b64,
                    seq: 0,
                }
            }
            Err(e) => {
                eprintln!("[seance daemon] grid_bin encode failed: {e}; falling back to JSON");
                GuiEvent::Grid(snap)
            }
        }
    }

    pub(super) fn broadcast_grid(&mut self, snap: GridSnapshot) {
        let cols = snap.cols as usize;
        let rows = snap.rows as usize;

        let mut push = Push::Full;
        let mut skip = false;
        if let Some(prev) = self.last_grid_cells.get(&snap.pane) {
            if prev.cols == snap.cols
                && prev.rows == snap.rows
                && prev.cells.len() == snap.cells.len()
            {
                let d = dirty_rows(prev.cells.as_ref(), &snap.cells, cols, rows);
                if d.is_empty() {
                    if prev.cursor_col == snap.cursor_col && prev.cursor_row == snap.cursor_row {
                        // Cells + cursor unchanged — still send if OSC title
                        // flipped (Claude spinner ↔ idle ✳). GUI working badges
                        // depend on title; dropping these left stale chrome.
                        if prev.title == snap.title {
                            skip = true;
                        } else {
                            // Title-only. This used to send a FULL grid: 55KB
                            // to move one spinner glyph, measured at 7% of all
                            // full-frame bytes on a busy pane.
                            push = Push::HeaderOnly;
                        }
                    } else {
                        push = Push::Damage(vec![snap.cursor_row]);
                    }
                } else if d.len() * 2 < rows.max(1) {
                    push = Push::Damage(d);
                } else {
                    // Too much changed for row damage — but "everything moved
                    // up N rows" is what scrolling output looks like, and it
                    // is the dominant case on an agent pane. Try to describe it
                    // as a shift before falling back to a whole grid.
                    let prev_h = row_hashes(prev.cells.as_ref(), cols, rows);
                    let next_h = row_hashes(&snap.cells, cols, rows);
                    if let Some((delta, rows_after)) = scroll_shift(&prev_h, &next_h) {
                        if rows_after.len() < d.len() {
                            push = Push::Scroll {
                                delta,
                                rows: rows_after,
                            };
                        }
                    }
                }
            }
        }

        self.last_grid_cells.insert(
            snap.pane.clone(),
            LastGridFrame {
                cols: snap.cols,
                rows: snap.rows,
                cursor_col: snap.cursor_col,
                cursor_row: snap.cursor_row,
                title: snap.title.clone(),
                cells: std::sync::Arc::new(snap.cells.clone()),
            },
        );

        if skip {
            return;
        }
        crate::latency_probe::complete("d_input", &snap.pane, "daemon input→gridpush");
        let pane = snap.pane.clone();
        self.send_grid_to_subscribers(&pane, Arc::new(snap), push);
    }

    /// The pane's push rate = the FASTEST any interested connection wants
    /// (selected somewhere → 16ms; only subscribed-with-overview → 66ms;
    /// nobody interested → None, the pane simply isn't streamed).
    fn grid_interval_for(&self, slug: &str) -> Option<Duration> {
        let ws = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| p.workspace.as_str())?;
        self.gui_conns
            .iter()
            .filter_map(|c| Self::conn_rate_for(c, ws))
            .min()
    }

    fn push_grid_throttled(&mut self, slug: &str) {
        let Some(min_interval) = self.grid_interval_for(slug) else {
            self.grid_flush_pending.remove(slug);
            return;
        };
        let now = Instant::now();
        if let Some(last) = self.last_grid_push.get(slug) {
            let elapsed = now.duration_since(*last);
            if elapsed < min_interval {
                if self.grid_flush_pending.insert(slug.to_string()) {
                    let tx = self.event_tx.clone();
                    let s = slug.to_string();
                    let wait = min_interval.saturating_sub(elapsed);
                    thread::spawn(move || {
                        thread::sleep(wait.max(Duration::from_millis(1)));
                        let _ = tx.send(SessionEvent::FlushGrid { slug: s });
                    });
                }
                return;
            }
        }
        // Which recipients can take damage is now a per-connection question,
        // answered in `send_grid_to_subscribers` from what each one has
        // actually been sent. This used to promote the frame to FULL for
        // *everyone* whenever any recipient (an overview thumb watcher) lacked
        // the base — which is how a remote window ended up receiving whole
        // 74KB grids to move a cursor.
        self.push_grid_now(slug);
    }

    fn push_grid_now(&mut self, slug: &str) {
        self.grid_flush_pending.remove(slug);
        self.last_grid_push.insert(slug.to_string(), Instant::now());
        if let Some(s) = self.session_mut(slug) {
            s.bump_rev();
        }
        if let Some(snap) = self.snapshot_pane(slug) {
            self.broadcast_grid(snap);
        }
    }

    /// Force a FULL frame (never damage). Used after workspace switch / attach
    /// so the GUI never applies damage against a base it never received while
    /// the circle was hidden.
    fn push_grid_full(&mut self, slug: &str) {
        self.grid_flush_pending.remove(slug);
        self.last_grid_push.insert(slug.to_string(), Instant::now());
        self.last_grid_cells.remove(slug);
        if let Some(s) = self.session_mut(slug) {
            s.bump_rev();
        }
        if let Some(snap) = self.snapshot_pane(slug) {
            // last_grid_cells empty → broadcast_grid encodes FULL.
            self.broadcast_grid(snap);
        }
    }

    /// Give one window whole grids for a circle it just brought on screen.
    ///
    /// Sleeping panes are included: they have no session, but they do have a
    /// frozen frame, and selecting the circle is exactly when you want to read
    /// it. Full frames only — a pane may have redrawn heavily while the circle
    /// was off-screen, and damage against a stale base leaves a corrupt grid
    /// until the next resize. Only *this* window pays for that; the others
    /// have been following the pane all along.
    fn flush_workspace_grids(&mut self, window_id: &str, workspace: &str) {
        let slugs: Vec<String> = self
            .panes
            .iter()
            .filter(|p| p.workspace == workspace && (p.session.is_some() || p.asleep))
            .map(|p| p.slug.clone())
            .collect();
        for slug in slugs {
            self.send_full_to(window_id, &slug);
        }
    }

    /// Arm the replay recorder (daemon startup; tests leave it unarmed).
    pub fn set_recorder(&mut self, handle: crate::runtime::recorder::RecorderHandle) {
        self.recorder = Some(handle);
    }

    /// Recorder-side grid tap: ship a snapshot clone at most every 33ms per
    /// pane. `force` bypasses the gate (human input / title / exit moments).
    pub(crate) fn record_grid_tap(&mut self, slug: &str, force: bool) {
        // Recording is pre-fan-out and subscription-blind on purpose: the DVR
        // must capture panes no GUI is watching. Log the entry BEFORE any
        // early return so tests can pin that invariant.
        #[cfg(test)]
        self.record_tap_log.push(slug.to_string());
        let Some(rec) = self.recorder.as_ref() else {
            return;
        };
        let now = Instant::now();
        if !force {
            if let Some(last) = self.last_record_grid.get(slug) {
                if now.duration_since(*last).as_millis() < 33 {
                    return;
                }
            }
        }
        if let Some(snap) = self.snapshot_pane(slug) {
            let rec = rec.clone();
            self.last_record_grid.insert(slug.to_string(), now);
            rec.grid(snap);
        }
    }

    /// Human input landed in this pane's workspace — bump the daemon-owned
    /// recency clock (agent/ctl sends deliberately do NOT, matching the
    /// client semantics this replaced).
    pub(super) fn stamp_workspace_touch(&mut self, pane: &str) {
        if let Some(ws) = self
            .panes
            .iter()
            .find(|p| p.slug == pane)
            .map(|p| p.workspace.clone())
        {
            if !ws.is_empty() {
                self.workspace_touch_ms.insert(ws, now_ms());
            }
        }
    }

    /// Recorder event tap (no-op until armed).
    pub(crate) fn record_event(&self, slug: &str, ev: seance_core::replay::ReplayEvent) {
        if let Some(rec) = self.recorder.as_ref() {
            rec.event(slug, ev);
        }
    }

    pub fn handle_session_event(&mut self, ev: SessionEvent) {
        match &ev {
            SessionEvent::Wakeup { slug } => {
                let slug_r = slug.clone();
                self.push_grid_throttled(slug);
                self.record_grid_tap(&slug_r, false);
            }
            SessionEvent::FlushGrid { slug } => {
                // Force-send the coalesced frame (timer already waited).
                let slug_r = slug.clone();
                self.push_grid_now(slug);
                self.record_grid_tap(&slug_r, false);
            }
            SessionEvent::ForceFullGrid { slug, window } => match window.clone() {
                Some(w) => self.send_full_to(&w, slug),
                None => self.push_grid_full(slug),
            },
            SessionEvent::ActivityNote { slug, t_ms } => {
                // Recorder observed real output for this pane — the daemon
                // owns the clock, clients only mirror it.
                let t = *t_ms;
                if let Some(ws) = self
                    .panes
                    .iter()
                    .find(|p| p.slug == *slug)
                    .map(|p| p.workspace.clone())
                {
                    let cur = self.workspace_output.get(&ws).copied().unwrap_or(0);
                    if t > cur {
                        self.workspace_output.insert(ws.clone(), t);
                        self.broadcast(GuiEvent::Activity {
                            workspace: ws,
                            last_output_ms: t,
                        });
                    }
                }
            }
            SessionEvent::PrLinkSeen { slug, url } => {
                let (slug, url) = (slug.clone(), url.clone());
                self.on_pr_link_seen(&slug, &url);
            }
            SessionEvent::Title { slug, title } => {
                self.record_event(
                    slug,
                    seance_core::replay::ReplayEvent::Title {
                        title: title.clone().unwrap_or_default(),
                    },
                );
                // Busy is BROADCAST; the grid below is not. A client only gets
                // frames for the workspace it has selected, so every other
                // circle's spinner would freeze at whatever it last saw and
                // read as "working" until you clicked it. Edge-triggered off
                // the daemon's own busy set, which is the single source both
                // sides agree on (`pane_infos` reports it too).
                let now_busy = title
                    .as_deref()
                    .map(seance_core::util::title_looks_busy)
                    .unwrap_or(false);
                let was_busy = self.pane_busy.contains(slug);
                if was_busy != now_busy {
                    if now_busy {
                        self.pane_busy.insert(slug.clone());
                    } else {
                        self.pane_busy.remove(slug);
                    }
                    self.broadcast(GuiEvent::PaneBusy {
                        pane: slug.clone(),
                        busy: now_busy,
                    });
                }
                // Title changes are rare — push immediately (also a grid).
                if let Some(s) = self.session_mut(slug) {
                    s.bump_rev();
                }
                self.grid_flush_pending.remove(slug);
                self.last_grid_push.insert(slug.clone(), Instant::now());
                if let Some(snap) = self.snapshot_pane(slug) {
                    let mut s = snap;
                    s.title = title.clone();
                    self.broadcast_grid(s);
                }
            }
            SessionEvent::Exited { slug, code } => {
                // Sleeping a pane kills its child, which lands here. That exit
                // is the point, not a death — the pane keeps its identity and
                // its frozen frame, so it must NOT be auto-closed.
                if self.panes.iter().any(|p| p.slug == *slug && p.asleep) {
                    return;
                }
                self.record_grid_tap(&slug.clone(), true);
                self.record_event(
                    slug,
                    seance_core::replay::ReplayEvent::Exited { code: *code },
                );
                if let Some(rec) = self.recorder.as_ref() {
                    rec.pane_closed(slug);
                }
                // Process died → auto-close. Dead shells/agents leave clutter;
                // re-summon if needed. No tombstone chrome.
                let code = *code;
                let slug = slug.clone();
                if let Some(tid) = self.active_tasks.remove(&slug) {
                    if let Some(t) = self.tasks.get_mut(&tid) {
                        if t.status == "open" {
                            t.status = "orphaned".into();
                            t.finished_ms = Some(now_ms());
                        }
                    }
                }
                events::log(
                    "daemon",
                    None,
                    Some(&slug),
                    "pane_exited",
                    format!("process exited ({code:?}) — auto-closed"),
                );
                self.kill_pane(&slug);
                self.broadcast(GuiEvent::PaneKilled { slug: slug.clone() });
                self.push_state_to_all();
                self.persist();
            }
        }
    }

    /// The pane's current grid — or, for a sleeping pane, the frame it was
    /// showing when it went to sleep. Every paint path runs through here, so
    /// a slept circle still reads without a process behind it.
    pub fn snapshot_pane(&self, slug: &str) -> Option<GridSnapshot> {
        let pane = self.panes.iter().find(|p| p.slug == slug)?;
        if pane.asleep {
            return self.frozen_grid(slug);
        }
        pane.session.as_ref().map(|s| s.snapshot())
    }

    /// Dense one-row summary for orchestrators (`list` / `brief` / `roster`).
    pub(super) fn pane_summary_json(&self, p: &EnginePane) -> serde_json::Value {
        let w = p.agency.to_wire();
        let running = if p.kind == "file" {
            true
        } else {
            p.session.as_ref().map(|s| s.is_running()).unwrap_or(false) && !p.agency.exited
        };
        let scratch = p.scratch_path.to_string_lossy().to_string();
        let scratchpad_bytes = std::fs::metadata(&p.scratch_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let (status, status_note) = self
            .statuses
            .get(&p.slug)
            .map(|(s, n)| (Some(s.clone()), n.clone()))
            .unwrap_or((None, None));
        let pad_rev = self.pad_revs.get(&p.slug).copied().unwrap_or(0);
        let (inject_pad_rev, inject_pad_bytes) = self
            .inject_baselines
            .get(&p.slug)
            .copied()
            .map(|(r, b)| (Some(r), Some(b)))
            .unwrap_or((None, None));
        let open_asks = self
            .asks
            .iter()
            .filter(|a| a.answer.is_none() && a.from == p.slug)
            .count();
        // Active open task, else most recent task for this pane (so wait --task
        // still sees done after complete_active_task clears the active map).
        let task_id = self.active_tasks.get(&p.slug).cloned().or_else(|| {
            self.tasks
                .values()
                .filter(|t| t.pane == p.slug)
                .max_by_key(|t| t.created_ms)
                .map(|t| t.id.clone())
        });
        let task_status = task_id
            .as_ref()
            .and_then(|id| self.tasks.get(id).map(|t| t.status.clone()));
        json!({
            "kind": p.kind,
            "name": p.name,
            "slug": p.slug,
            "workspace": p.workspace,
            "workspace_name": self.workspace_label(&p.workspace),
            "command": p.command,
            "cwd": p.cwd,
            "tiled": p.tiled,
            "running": running,
            "asleep": p.asleep,
            "exited": w.exited,
            "exit_code": w.exit_code,
            "owner": w.owner,
            "drive_mode": w.drive_mode,
            "human_idle": w.human_idle,
            "title": p.session.as_ref().and_then(|s| s.title()),
            "status": status,
            "status_note": status_note,
            "scratchpad": scratch,
            "scratchpad_bytes": scratchpad_bytes,
            "pad_rev": pad_rev,
            "inject_pad_rev": inject_pad_rev,
            "inject_pad_bytes": inject_pad_bytes,
            "open_asks": open_asks,
            "task_id": task_id,
            "task_status": task_status,
        })
    }

    pub(super) fn pane_infos(&self) -> Vec<PaneInfo> {
        self.panes
            .iter()
            .map(|p| {
                let running = if p.kind == "file" {
                    true
                } else {
                    p.session.as_ref().map(|s| s.is_running()).unwrap_or(false) && !p.agency.exited
                };
                let w = p.agency.to_wire();
                PaneInfo {
                    kind: p.kind.clone(),
                    name: p.name.clone(),
                    slug: p.slug.clone(),
                    workspace: p.workspace.clone(),
                    command: p.command.clone(),
                    cwd: p.cwd.clone(),
                    tiled: p.tiled,
                    running,
                    title: p.session.as_ref().and_then(|s| s.title()),
                    busy: self.pane_busy.contains(&p.slug),
                    asleep: p.asleep,
                    restorable: self.pane_restorable(&p.slug),
                    scratchpad: p.scratch_path.to_string_lossy().to_string(),
                    file: p.file.clone(),
                    owner: Some(w.owner),
                    drive_mode: Some(w.drive_mode),
                    exited: w.exited,
                    exit_code: w.exit_code,
                }
            })
            .collect()
    }

    pub(super) fn broadcast_agency(&mut self, slug: &str) {
        if let Some(p) = self.panes.iter().find(|p| p.slug == slug) {
            let w = p.agency.to_wire();
            self.broadcast(GuiEvent::Agency {
                pane: slug.to_string(),
                owner: w.owner,
                drive_mode: w.drive_mode,
                human_idle: w.human_idle,
                exited: w.exited,
                exit_code: w.exit_code,
            });
        }
    }

    fn human_steal_pane(&mut self, slug: &str) {
        let changed = self
            .panes
            .iter_mut()
            .find(|p| p.slug == slug)
            .map(|p| p.agency.human_steal())
            .unwrap_or(false);
        if changed {
            events::log_ex(
                "human",
                self.selected_workspace.as_deref(),
                Some(slug),
                "agency.stolen",
                "human took the keys".into(),
                events::LogOpts {
                    origin: Some("human_keystroke".into()),
                    ..Default::default()
                },
            );
            self.broadcast_agency(slug);
        } else if let Some(p) = self.panes.iter_mut().find(|p| p.slug == slug) {
            // Refresh idle timer even if already human.
            p.agency.last_human_input = Some(std::time::Instant::now());
        }
    }

    pub fn handle_gui(&mut self, req: GuiRequest, window_id: &str) -> Option<GuiEvent> {
        // Circles are addressable by slug or label here too — the web client
        // is a third client on this protocol and must not be able to send a
        // label the daemon then treats as an unknown circle.
        let req = self.normalize_workspace_keys(req);
        match req {
            // Handled at the connection layer (`daemon::serve_gui`), which has
            // the send window right there and can do it without taking the
            // engine lock — acks arrive at frame rate and must not queue
            // behind pane work.
            GuiRequest::GridAck { .. } => None,
            GuiRequest::Attach {
                selected_workspace,
                focused_pane,
                subscriptions,
            } => {
                // Seed the subscription set: an explicit list is intersected
                // with what actually exists; absent means "everything"
                // (fresh client / migration from the ownership model).
                let known = self.all_workspace_names();
                let seed: HashSet<String> = match subscriptions {
                    Some(list) => list
                        .into_iter()
                        .filter(|w| known.iter().any(|k| k == w))
                        .collect(),
                    None => known.iter().cloned().collect(),
                };
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                    c.subscriptions = seed;
                }
                let subs = self.workspaces_for_window(window_id);
                let sel = selected_workspace
                    .clone()
                    .filter(|w| subs.iter().any(|s| s == w))
                    .or_else(|| {
                        self.selected_workspace
                            .clone()
                            .filter(|w| subs.iter().any(|s| s == w))
                    })
                    .or_else(|| subs.first().cloned());
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                    c.selected_workspace = sel.clone();
                    c.focused_pane = None;
                }
                // Restore focus: the requested pane, else the engine's last
                // focused pane when it lives in the selected circle.
                let focus = focused_pane.clone().or_else(|| {
                    self.focused_pane.clone().filter(|fp| {
                        self.panes
                            .iter()
                            .any(|p| p.slug == *fp && sel.as_deref() == Some(p.workspace.as_str()))
                    })
                });
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                    c.focused_pane = focus.clone();
                }
                // The engine's remembered focus follows a window that actually
                // has a selection (a deliberately blank window must not wipe it).
                if sel.is_some() {
                    self.selected_workspace = sel.clone();
                    if focus.is_some() {
                        self.focused_pane = focus;
                    }
                }
                // A reconnecting GUI has no prior base — but that is true of
                // *this* connection only, and `GuiConn::based` already says so.
                // This used to clear the daemon's shared last-frame cache,
                // which made one window attaching cost every other window a
                // full grid for every pane it was watching.
                // Kick PTYs this window subscribes to so empty post-handoff
                // Terms repaint.
                let slugs: Vec<String> = self
                    .panes
                    .iter()
                    .filter(|p| p.session.is_some())
                    .filter(|p| subs.iter().any(|w| w == &p.workspace))
                    .map(|p| p.slug.clone())
                    .collect();
                for slug in &slugs {
                    if let Some(s) = self.session_mut(slug) {
                        s.kick_redraw();
                    }
                }
                let state = self.state_for_window(window_id);
                // Also refresh peers' rosters (window list / labels).
                self.push_state_to_all();
                for slug in &slugs {
                    self.send_full_to(window_id, slug);
                }
                let tx = self.event_tx.clone();
                let delayed = slugs.clone();
                let window = window_id.to_string();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(150));
                    for slug in delayed {
                        let _ = tx.send(SessionEvent::ForceFullGrid {
                            slug,
                            window: Some(window.clone()),
                        });
                    }
                });
                Some(state)
            }
            GuiRequest::Subscribe { workspace } => {
                self.subscribe_conn(window_id, &workspace);
                let st = self.state_for_window(window_id);
                self.send_to(window_id, st);
                // If this window now streams the workspace (selected, or
                // overview thumbs), it has no damage base for those panes.
                let streams = self
                    .gui_conns
                    .iter()
                    .find(|c| c.id == window_id)
                    .and_then(|c| Self::conn_rate_for(c, &workspace))
                    .is_some();
                if streams {
                    self.flush_workspace_grids(window_id, &workspace);
                }
                None
            }
            GuiRequest::SleepWorkspace { workspace } => {
                let res = self.sleep_workspace(&workspace);
                self.persist();
                self.push_state_to_all();
                return Some(match res {
                    Ok(n) => GuiEvent::Ack {
                        ok: true,
                        data: Some(json!({ "slept": n })),
                        error: None,
                    },
                    Err(e) => GuiEvent::Ack {
                        ok: false,
                        data: None,
                        error: Some(e.to_string()),
                    },
                });
            }
            GuiRequest::WakeWorkspace { workspace } => {
                let res = self.wake_workspace(&workspace);
                self.persist();
                self.push_state_to_all();
                if res.is_ok() {
                    self.flush_workspace_grids(window_id, &workspace);
                }
                return Some(match res {
                    Ok(n) => GuiEvent::Ack {
                        ok: true,
                        data: Some(json!({ "woke": n })),
                        error: None,
                    },
                    Err(e) => GuiEvent::Ack {
                        ok: false,
                        data: None,
                        error: Some(e.to_string()),
                    },
                });
            }
            GuiRequest::Unsubscribe { workspace } => {
                let was_selected = self.gui_conns.iter().any(|c| {
                    c.id == window_id && c.selected_workspace.as_deref() == Some(workspace.as_str())
                });
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                    c.subscriptions.remove(&workspace);
                }
                if was_selected {
                    // Dropping the circle you were looking at moves you to the
                    // next one you still subscribe to (or nothing).
                    let next_sel = self.workspaces_for_window(window_id).first().cloned();
                    if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                        c.selected_workspace = next_sel;
                        c.focused_pane = None;
                    }
                }
                let st = self.state_for_window(window_id);
                self.send_to(window_id, st);
                None
            }
            GuiRequest::Input { pane, bytes_b64 } => {
                // Typing into a sleeping pane wakes it — same rule as ctl send.
                // Scrolling deliberately does not: reading the frozen frame is
                // what it's for.
                if self.panes.iter().any(|p| p.slug == pane && p.asleep) {
                    if self.wake_pane(&pane).is_ok() {
                        self.persist();
                        self.push_state_to_all();
                    }
                    // The keystroke that woke it is dropped: the shell/agent is
                    // still booting and would eat it at a half-drawn prompt.
                    return None;
                }
                self.record_event(
                    &pane,
                    seance_core::replay::ReplayEvent::Input {
                        origin: "human".into(),
                        bytes_b64: bytes_b64.clone(),
                    },
                );
                if let Ok(bytes) = base64_decode(&bytes_b64) {
                    let n = bytes.len();
                    let is_ctrl = bytes.first().is_some_and(|b| *b < 0x20);
                    // Human always wins the keys.
                    self.human_steal_pane(&pane);
                    if let Some(s) = self.session_mut(&pane) {
                        s.set_input_origin("human");
                        s.scroll_to_bottom();
                        s.write_bytes(bytes);
                        s.bump_rev();
                    }
                    // Human keystroke: let the echo frame bypass the 16ms grid
                    // throttle — drop the last-push stamp so the next Wakeup
                    // pushes immediately. Bounded by typing rate, so this never
                    // reopens the output-storm path the throttle exists for.
                    self.last_grid_push.remove(&pane);
                    if n >= 2 || is_ctrl {
                        events::log_ex(
                            "human",
                            self.selected_workspace.as_deref(),
                            Some(&pane),
                            "terminal.input",
                            format!("{n} bytes"),
                            events::LogOpts {
                                origin: Some("human_keystroke".into()),
                                ..Default::default()
                            },
                        );
                    }
                    self.broadcast(GuiEvent::InputOrigin {
                        pane: pane.clone(),
                        origin: "human".into(),
                    });
                    self.stamp_workspace_touch(&pane);
                }
                None
            }
            GuiRequest::Resize { pane, cols, rows } => {
                self.record_event(
                    &pane,
                    seance_core::replay::ReplayEvent::Resized { cols, rows },
                );
                if let Some(s) = self.session_mut(&pane) {
                    s.resize(cols, rows);
                    s.bump_rev();
                }
                // Immediate FULL grid after resize — don't wait for PTY wakeup.
                // Size changes invalidate damage bases; without this, a
                // workspace switch that also reflows tiles can leave a blank
                // pane until the human resizes the window.
                self.last_grid_cells.remove(&pane);
                self.last_grid_push.insert(pane.clone(), Instant::now());
                if let Some(snap) = self.snapshot_pane(&pane) {
                    self.broadcast_grid(snap);
                }
                None
            }
            GuiRequest::Scroll { pane, delta } => {
                if let Some(s) = self.session_mut(&pane) {
                    s.scroll_lines(delta);
                }
                self.snapshot_pane(&pane).map(|s| Self::grid_event(s, None))
            }
            GuiRequest::ScrollBottom { pane } => {
                if let Some(s) = self.session_mut(&pane) {
                    s.scroll_to_bottom();
                }
                self.snapshot_pane(&pane).map(|s| Self::grid_event(s, None))
            }
            GuiRequest::Inject { pane, text, submit } => {
                self.record_event(
                    &pane,
                    seance_core::replay::ReplayEvent::Send {
                        from: "human".into(),
                        text: text.clone(),
                        submit,
                    },
                );
                let n = text.len();
                self.human_steal_pane(&pane);
                if let Some(s) = self.session_mut(&pane) {
                    s.set_input_origin("human");
                    s.scroll_to_bottom();
                    s.inject(text, submit);
                    s.bump_rev();
                }
                events::log_ex(
                    "human",
                    self.selected_workspace.as_deref(),
                    Some(&pane),
                    "terminal.input",
                    format!("inject {n} chars"),
                    events::LogOpts {
                        origin: Some("inject".into()),
                        ..Default::default()
                    },
                );
                self.broadcast(GuiEvent::InputOrigin {
                    pane: pane.clone(),
                    origin: "human".into(),
                });
                self.stamp_workspace_touch(&pane);
                None
            }
            GuiRequest::GhostAccept { pane } => {
                let ghost = self
                    .session_mut(&pane)
                    .and_then(|s| s.ghost.lock().unwrap().take());
                if let Some(g) = ghost {
                    let from = g.from.clone();
                    self.record_event(
                        &pane,
                        seance_core::replay::ReplayEvent::Send {
                            from: format!("propose:{from}"),
                            text: g.text.clone(),
                            submit: true,
                        },
                    );
                    if let Some(entry) = self.proposals.get_mut(&g.id) {
                        entry.1 = Some("accepted".into());
                    }
                    if let Some(s) = self.session_mut(&pane) {
                        s.set_input_origin("propose");
                        s.inject(g.text, true);
                    }
                    events::log_ex(
                        "human",
                        None,
                        Some(&pane),
                        "propose_accepted",
                        format!("accepted proposal from {from}"),
                        events::LogOpts {
                            origin: Some("propose_accepted".into()),
                            caused_by: Some(g.id.clone()),
                            ..Default::default()
                        },
                    );
                    self.broadcast(GuiEvent::InputOrigin {
                        pane: pane.clone(),
                        origin: "propose".into(),
                    });
                }
                self.broadcast(GuiEvent::Ghost {
                    pane: pane.clone(),
                    ghost: None,
                });
                None
            }
            GuiRequest::GhostReject { pane } => {
                let ghost = self
                    .session_mut(&pane)
                    .and_then(|s| s.ghost.lock().unwrap().take());
                if let Some(g) = ghost {
                    if let Some(entry) = self.proposals.get_mut(&g.id) {
                        entry.1 = Some("rejected".into());
                    }
                    events::log(
                        "human",
                        None,
                        Some(&pane),
                        "propose_rejected",
                        "rejected".into(),
                    );
                }
                self.broadcast(GuiEvent::Ghost { pane, ghost: None });
                None
            }
            GuiRequest::Spawn {
                name,
                cwd,
                command,
                workspace,
                file,
                tiled,
            } => {
                let ws = workspace.clone().unwrap_or_else(|| {
                    self.gui_conns
                        .iter()
                        .find(|c| c.id == window_id)
                        .and_then(|c| c.selected_workspace.clone())
                        .unwrap_or_else(|| "main".into())
                });
                // A GUI-requested spawn auto-subscribes the requesting window
                // to the target circle (ctl spawns subscribe nobody).
                self.subscribe_conn(window_id, &ws);
                match self.spawn(SpawnSpec {
                    name,
                    cwd,
                    command,
                    workspace: Some(ws.clone()),
                    tiled,
                    resume: false,
                    file,
                }) {
                    Ok(slug) => {
                        self.persist();
                        // A GUI-requested spawn selects its workspace on the
                        // requesting window BEFORE the State push below —
                        // otherwise State still carries the old selection and
                        // reverts the GUI's PaneSpawned-side select (visible
                        // with quicklaunch, which targets a fresh workspace).
                        if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                            c.selected_workspace = Some(ws.clone());
                            c.focused_pane = Some(slug.clone());
                        }
                        self.selected_workspace = Some(ws.clone());
                        self.focused_pane = Some(slug.clone());
                        let info = self
                            .pane_infos()
                            .into_iter()
                            .find(|p| p.slug == slug)
                            .unwrap();
                        self.send_to(window_id, GuiEvent::PaneSpawned { pane: info.clone() });
                        if let Some(snap) = self.snapshot_pane(&slug) {
                            self.broadcast_grid(snap);
                        }
                        self.push_state_to_all();
                        Some(GuiEvent::Ack {
                            ok: true,
                            data: Some(json!({"slug": slug})),
                            error: None,
                        })
                    }
                    Err(e) => Some(GuiEvent::Ack {
                        ok: false,
                        data: None,
                        error: Some(e.to_string()),
                    }),
                }
            }
            GuiRequest::Kill { pane } => {
                self.kill_pane(&pane);
                self.broadcast(GuiEvent::PaneKilled { slug: pane });
                self.persist();
                self.push_state_to_all();
                None
            }
            GuiRequest::SetTiled { pane, tiled } => {
                if let Some(p) = self.panes.iter_mut().find(|p| p.slug == pane) {
                    p.tiled = tiled;
                }
                self.persist();
                self.push_state_to_all();
                None
            }
            GuiRequest::MovePane {
                pane,
                workspace,
                before,
            } => {
                self.reorder_pane(&pane, &workspace, before.as_deref());
                self.persist();
                self.push_state_to_all();
                None
            }
            GuiRequest::ReorderWorkspace { moved, before } => {
                self.reorder_workspace(&moved, &before);
                self.persist();
                self.push_state_to_all();
                None
            }
            GuiRequest::RenamePane { pane, name } => {
                if let Some(p) = self.panes.iter_mut().find(|p| p.slug == pane) {
                    p.name = name;
                }
                self.persist();
                self.push_state_to_all();
                None
            }
            GuiRequest::RenameWorkspace { old, new } => {
                // A rename sets the LABEL. The slug — the circle's identity —
                // does not move, so there is nothing to migrate: panes,
                // activity clocks, PR links and dismissals, every window's
                // selection and subscription set, each client's pin/park
                // prefs, and every running pane's `SEANCE_WORKSPACE` all keep
                // pointing at the same circle. The eight-structure migration
                // this replaced could never reach that last one.
                self.rename_workspace(&old, &new);
                self.persist();
                self.push_state_to_all();
                None
            }
            GuiRequest::CreateWorkspace { name } => {
                // Mint a slug from what was typed and keep the typed text as
                // the label, so "Growth Work" reads as itself while being
                // keyed by `growth-work` forever after.
                let name = self.create_workspace(&name);
                self.subscribe_conn(window_id, &name);
                self.selected_workspace = Some(name.clone());
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                    c.selected_workspace = Some(name.clone());
                }
                self.persist();
                self.push_state_to_all();
                None
            }
            GuiRequest::KillWorkspace { workspace } => {
                let slugs: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|p| p.workspace == workspace)
                    .map(|p| p.slug.clone())
                    .collect();
                for s in slugs {
                    self.kill_pane(&s);
                    self.broadcast(GuiEvent::PaneKilled { slug: s });
                }
                self.extra_workspaces.retain(|w| w != &workspace);
                self.forget_workspace(&workspace);
                self.persist();
                self.push_state_to_all();
                None
            }
            GuiRequest::SetFocus { pane, workspace } => {
                let mut workspace_changed = false;
                let mut flush_ws: Option<String> = None;
                let mut flush_pane: Option<String> = None;
                // Selecting a circle auto-subscribes to it ("add to active").
                if let Some(w) = workspace.as_ref() {
                    self.subscribe_conn(window_id, w);
                }
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                    if let Some(p) = pane.clone() {
                        c.focused_pane = Some(p.clone());
                        self.focused_pane = Some(p.clone());
                        flush_pane = Some(p);
                    }
                    if let Some(w) = workspace.clone() {
                        if c.selected_workspace.as_ref() != Some(&w) {
                            workspace_changed = true;
                        }
                        c.selected_workspace = Some(w.clone());
                        self.selected_workspace = Some(w.clone());
                        flush_ws = Some(w);
                    }
                }
                self.persist();
                if workspace_changed {
                    if let Some(w) = flush_ws {
                        self.flush_workspace_grids(window_id, &w);
                    }
                } else if let Some(fp) = flush_pane {
                    self.push_grid_now(&fp);
                }
                None
            }
            GuiRequest::SetOverview { enabled } => {
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                    c.overview = enabled;
                }
                if enabled {
                    // FULL flush for this window's subscribed workspaces only.
                    let subs = self.workspaces_for_window(window_id);
                    let slugs: Vec<String> = self
                        .panes
                        .iter()
                        .filter(|p| subs.iter().any(|w| w == &p.workspace) && p.session.is_some())
                        .map(|p| p.slug.clone())
                        .collect();
                    for slug in slugs {
                        self.send_full_to(window_id, &slug);
                    }
                }
                None
            }
            GuiRequest::RefreshGrid { pane } => {
                // A refresh is one window saying "I lost my base" — the others
                // still have theirs.
                self.send_full_to(window_id, &pane);
                None
            }
            GuiRequest::Bye => {
                // Window closing — drop the connection immediately (don't wait
                // for socket EOF). serve_gui will also unregister on exit; that
                // is a no-op if already gone.
                self.unregister_gui(window_id);
                None
            }
            GuiRequest::CloseWindow { window } => {
                if window == window_id {
                    return Some(GuiEvent::Error {
                        message: "close this window locally, not via the daemon".into(),
                    });
                }
                // Tell the victim first (it must stop reconnecting), then
                // unregister — exactly like a Bye.
                self.send_to(
                    &window,
                    GuiEvent::Kicked {
                        by: window_id.to_string(),
                    },
                );
                self.unregister_gui(&window);
                None
            }
            GuiRequest::AnswerAsk { id, answer } => {
                if let Some(a) = self.asks.iter_mut().find(|a| a.id == id) {
                    a.answer = Some(answer);
                    events::log(
                        "human",
                        a.workspace.as_deref(),
                        Some(&a.from),
                        "ask_answered",
                        format!("answered: {}", a.answer.as_deref().unwrap_or("")),
                    );
                }
                self.broadcast(GuiEvent::AskResolved { id });
                None
            }
            GuiRequest::Ctl(req) => {
                let resp = self.handle_control(req);
                Some(GuiEvent::Ack {
                    ok: resp.ok,
                    data: resp.data,
                    error: resp.error,
                })
            }
            GuiRequest::Event {
                actor,
                workspace,
                pane,
                kind,
                detail,
            } => {
                crate::events::log(&actor, workspace.as_deref(), pane.as_deref(), &kind, detail);
                None
            }
            GuiRequest::Ping => Some(GuiEvent::Pong),
            // Fs ops are intercepted in serve_gui (daemon level) and never
            // reach the engine; answer defensively if one slips through.
            GuiRequest::Fs { id, .. } => Some(GuiEvent::FsResult {
                id,
                ok: false,
                data: None,
                error: Some("fs op reached engine (daemon-level handler missing)".into()),
            }),
        }
    }
}
