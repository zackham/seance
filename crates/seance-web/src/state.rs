//! Client-side session state: the single store every module reads.
//!
//! [`ClientState::apply_event`] folds daemon pushes into the store and reports
//! what changed as a [`Applied`] so the caller repaints exactly what's dirty —
//! grid frames repaint one canvas, structure changes rebuild chrome.

use std::collections::HashMap;

use base64::Engine as _;
use seance_core::protocol::{AskInfo, GuiEvent, PaneInfo, PrLink, StatusInfo, WindowInfo};
use seance_core::snapshot::{decode_grid_bin_onto, GridSnapshot};

use crate::subs::SubPrefs;

/// What a folded event dirtied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Nothing a renderer cares about (Pong, FsResult handled elsewhere, …).
    Nothing,
    /// One pane's grid changed → repaint that canvas only.
    Grid { pane: String },
    /// A grid frame could not be applied (damage desync) → send RefreshGrid.
    NeedRefresh { pane: String },
    /// Pane list / workspace structure / focus changed → rebuild chrome.
    Structure,
    /// Asks or statuses changed → refresh badges/ask UI.
    Badges,
    /// Daemon error message to surface.
    Error { message: String },
    /// This window was remote-closed (✦ popover) — stop reconnecting.
    Kicked { by: String },
}

/// Per-pane co-presence state (from Agency events).
#[derive(Clone, Debug, Default)]
pub struct AgencyState {
    pub owner: String,
    pub drive_mode: String,
    pub human_idle: bool,
    pub exited: bool,
    pub exit_code: Option<i32>,
}

#[derive(Default)]
pub struct ClientState {
    pub panes: Vec<PaneInfo>,
    pub grids: HashMap<String, GridSnapshot>,
    pub selected_workspace: Option<String>,
    pub focused_pane: Option<String>,
    pub extra_workspaces: Vec<String>,
    pub workspace_order: Vec<String>,
    pub asks: Vec<AskInfo>,
    pub statuses: HashMap<String, StatusInfo>,
    pub agency: HashMap<String, AgencyState>,
    pub window_id: Option<String>,
    pub windows: Vec<WindowInfo>,
    /// This connection's subscription set (daemon order). State arrives and is
    /// KEPT global; the sidebar splits it into active/parked from [`subs`].
    pub subscriptions: Vec<String>,
    /// Per-GUI active/parked split (localStorage-backed; see [`crate::subs`]).
    pub subs: SubPrefs,
    /// Set when a fold changed [`subs`] — the app persists and clears it.
    pub subs_dirty: bool,
    /// Who last wrote stdin per pane (`human` / `agent:x` / `cli` / `propose`).
    pub input_origin: HashMap<String, String>,
    /// Monotonic revision bumped on every Structure-level change.
    pub structure_rev: u64,
    /// Host-bridge widgets (claude accounts strip) — daemon-polled, pushed on
    /// attach + every poll tick.
    pub host_widgets: Vec<HostWidget>,
    /// Client-local: last human-touch ms per workspace (sidebar auto-sort key;
    /// mirrors the native app — selecting alone does NOT bump).
    pub workspace_touch: HashMap<String, f64>,
    /// Sticky unread attention per non-selected workspace (cleared on select).
    pub workspace_unread: HashMap<String, Attention>,
    /// Workspaces observed live-working last check (finish detection).
    pub workspace_was_working: std::collections::HashSet<String>,
    /// Client-local zoomed pane (fills the tile area; esc restores).
    pub zoomed: Option<String>,
    /// Recent activity (Touch/status/spawn/kill/ask), newest last, capped.
    pub activity: std::collections::VecDeque<ActivityItem>,
    /// Slug of the most recent PaneSpawned (summon's rename-on-arrival hook).
    pub last_spawned: Option<String>,
    /// Last pane output per workspace (ms) — sidebar shows time-since-update.
    pub workspace_activity: HashMap<String, f64>,
    /// When each workspace entered the working band (stable sort key there).
    pub workspace_working_since: HashMap<String, f64>,
    /// Per-pane deadline until which grid content changes count as resize
    /// reflow, not output. Armed by any frame that arrives at new dims.
    pub resize_settle: HashMap<String, f64>,
    /// PR links per workspace, daemon-owned (`WorkspaceMeta.pr_links`),
    /// most-recently-seen LAST. Statuses come from the external poller.
    pub workspace_pr_links: HashMap<String, Vec<PrLink>>,
    /// `Date.now() - performance.now()` at boot. Both local clocks above live
    /// in the `performance.now()` domain; the daemon's are unix ms, so every
    /// ingested daemon stamp is converted with `perf = unix - offset`. Set
    /// once by `lib.rs`; tests leave it at 0 so conversion is identity.
    pub clock_offset_ms: f64,
}

/// One row of the host-bridge widget strip (native `HostWidgetSnap` shape).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct HostWidget {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub items: Vec<HostItem>,
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct HostItem {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub detail2: String,
    #[serde(default)]
    pub selected: bool,
}

/// Sidebar badge for a workspace (native `WorkspaceAttention` mirror).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attention {
    Working,
    NeedsHuman,
    Done,
}

impl Attention {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsHuman => "needs",
            Self::Done => "done",
        }
    }
    fn priority(self) -> u8 {
        match self {
            Self::NeedsHuman => 3,
            Self::Working => 2,
            Self::Done => 1,
        }
    }
}

/// One line of the activity drawer.
#[derive(Clone, Debug)]
pub struct ActivityItem {
    /// performance.now-ish ms stamped by the caller via [`ClientState::note_activity`].
    pub at_ms: f64,
    pub actor: String,
    pub pane: Option<String>,
    pub text: String,
}

const ACTIVITY_CAP: usize = 200;

/// Grace after a frame arrives at new dims: the SIGWINCH redraw that follows
/// the resize we requested is reflow, not output. Mirrors the native const.
const RESIZE_SETTLE_MS: f64 = 400.0;

/// Coarse one-unit relative time for sidebar labels ("now","42s","3m","2h","4d").
pub fn rel_label(delta_ms: f64) -> String {
    let s = (delta_ms / 1000.0).max(0.0) as u64;
    match s {
        0..=4 => "now".into(),
        5..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// `…/pull/123` → `123`. Tolerates trailing path/query (`/files`, `#issue`).
pub fn pr_number(url: &str) -> Option<u64> {
    let tail = url.split("/pull/").nth(1)?;
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Busy TUI title: braille spinner (U+2800..=U+28FF) as first non-space char —
/// same detector as the native `title_looks_busy`.
pub fn title_looks_busy(title: &str) -> bool {
    matches!(
        title.trim_start().chars().next(),
        Some('\u{2800}'..='\u{28FF}')
    )
}

impl ClientState {
    /// All workspaces in sidebar display order — the native auto-sort:
    /// 1. circles with an actively working agent float to the top;
    /// 2. within/outside that band, most recent human *touch* first
    ///    (typing into the circle, context-menu touch, fresh spawn —
    ///    selecting alone does not bump);
    /// 3. name as the stable tiebreak.
    pub fn workspaces(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let seen = |out: &mut Vec<String>, name: &str| {
            if !out.iter().any(|w| w == name) {
                out.push(name.to_string());
            }
        };
        for w in &self.workspace_order {
            seen(&mut out, w);
        }
        for p in &self.panes {
            seen(&mut out, &p.workspace);
        }
        for w in &self.extra_workspaces {
            seen(&mut out, w);
        }
        if let Some(sel) = &self.selected_workspace {
            seen(&mut out, sel);
        }
        out.sort_by(|a, b| {
            let key = |ws: &str| {
                let band = if self.workspace_has_working_agent(ws) {
                    0u8
                } else {
                    1
                };
                // Working band: when work STARTED (stable while working).
                // Idle band: the displayed clock (last output; touch floor).
                let at = if band == 0 {
                    self.workspace_working_since
                        .get(ws)
                        .copied()
                        .unwrap_or(f64::MAX)
                } else {
                    self.workspace_activity
                        .get(ws)
                        .copied()
                        .unwrap_or(f64::MIN)
                        .max(self.workspace_touch.get(ws).copied().unwrap_or(f64::MIN))
                };
                (
                    band,
                    std::cmp::Reverse(at.clamp(0.0, u64::MAX as f64) as u64),
                )
            };
            key(a).cmp(&key(b)).then_with(|| a.cmp(b))
        });
        out
    }

    /// Sidebar main list: [`workspaces`](Self::workspaces) restricted to the
    /// active set. Ctrl+PageUp/Down cycles exactly this.
    pub fn active_workspaces(&self) -> Vec<String> {
        self.workspaces()
            .into_iter()
            .filter(|w| self.subs.is_active(w))
            .collect()
    }

    /// The pinned section, rendered ABOVE everything else in the sidebar: the
    /// pinned subset of the active list, with the same working/idle sort
    /// applied within it (pins reorder among themselves, they don't freeze).
    pub fn pinned_workspaces(&self) -> Vec<String> {
        self.active_workspaces()
            .into_iter()
            .filter(|w| self.subs.is_pinned(w))
            .collect()
    }

    /// The normal active band under the pinned section: active, not pinned.
    pub fn unpinned_active_workspaces(&self) -> Vec<String> {
        self.active_workspaces()
            .into_iter()
            .filter(|w| !self.subs.is_pinned(w))
            .collect()
    }

    /// The collapsed "parked (N)" group: everything else, same sort.
    pub fn parked_workspaces(&self) -> Vec<String> {
        self.workspaces()
            .into_iter()
            .filter(|w| !self.subs.is_active(w))
            .collect()
    }

    /// Row badge including the parked-only rule: a circle this GUI has never
    /// seen (a `ctl` spawn with no GUI attribution) badges `needs` until it is
    /// first selected.
    pub fn row_attention(&self, ws: &str) -> Option<Attention> {
        if !self.subs.has_seen(ws) {
            return Some(Attention::NeedsHuman);
        }
        self.workspace_attention(ws)
    }

    /// Highest-priority badge among the parked rows — what the collapsed
    /// header dot shows.
    pub fn parked_attention(&self) -> Option<Attention> {
        self.parked_workspaces()
            .iter()
            .filter_map(|w| self.row_attention(w))
            .max_by_key(|a| a.priority())
    }

    /// Observed live-busy: braille title spinner, or agent-driven working
    /// status (human-owned sticky "working" is ignored — stale inject chrome).
    pub fn pane_is_live_working(&self, slug: &str) -> bool {
        let title = self
            .grids
            .get(slug)
            .and_then(|g| g.title.clone())
            .or_else(|| self.pane(slug).and_then(|p| p.title.clone()));
        if title.as_deref().is_some_and(title_looks_busy) {
            return true;
        }
        let owner = self.agency.get(slug).map(|a| a.owner.as_str());
        match (owner, self.statuses.get(slug).map(|s| s.state.as_str())) {
            (Some("human"), Some("working" | "planning")) => false,
            (_, Some("working" | "planning")) => true,
            _ => false,
        }
    }

    pub fn workspace_has_working_agent(&self, workspace: &str) -> bool {
        self.panes
            .iter()
            .any(|p| p.workspace == workspace && self.pane_is_live_working(&p.slug))
    }

    /// Live attention badge for a sidebar row (native `workspace_attention_cx`).
    pub fn workspace_attention(&self, workspace: &str) -> Option<Attention> {
        let needs = self.panes.iter().any(|p| {
            p.workspace == workspace
                && matches!(
                    self.statuses.get(&p.slug).map(|s| s.state.as_str()),
                    Some("needs-human" | "blocked" | "risky")
                )
        });
        if needs {
            return Some(Attention::NeedsHuman);
        }
        if self.workspace_has_working_agent(workspace) {
            return Some(Attention::Working);
        }
        // PR verdicts from the external poller: a red PR resurfaces a parked
        // circle exactly like an agent asking for help. Live work still wins.
        if let Some(a) = self.pr_attention(workspace) {
            return Some(a);
        }
        self.workspace_unread.get(workspace).copied()
    }

    /// PR links for a workspace, most-recently-seen LAST.
    pub fn pr_links(&self, workspace: &str) -> &[PrLink] {
        self.workspace_pr_links
            .get(workspace)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Drop ONE PR link from a workspace — the optimistic mirror of a
    /// single-url `pr-link clear` (engine-side that also sticky-dismisses the
    /// url, so the scraper won't re-add it). Returns true when a link went
    /// away; an emptied list drops its key, matching the State-fold shape
    /// (only non-empty lists are inserted there).
    pub fn remove_pr_link(&mut self, workspace: &str, url: &str) -> bool {
        let Some(list) = self.workspace_pr_links.get_mut(workspace) else {
            return false;
        };
        let before = list.len();
        list.retain(|l| l.url != url);
        let removed = list.len() != before;
        if list.is_empty() {
            self.workspace_pr_links.remove(workspace);
        }
        removed
    }

    /// The chip's link: the most recently seen one.
    pub fn latest_pr_link(&self, workspace: &str) -> Option<&PrLink> {
        self.pr_links(workspace).last()
    }

    /// Attention contributed by this workspace's PR links: any `needs` wins,
    /// else any `done`.
    pub fn pr_attention(&self, workspace: &str) -> Option<Attention> {
        let mut done = false;
        for l in self.pr_links(workspace) {
            match l.status.as_ref().and_then(|s| s.attention.as_deref()) {
                Some("needs") => return Some(Attention::NeedsHuman),
                Some("done") => done = true,
                _ => {}
            }
        }
        done.then_some(Attention::Done)
    }

    /// Ctrl+PageUp/Down walks EXACTLY the active list the sidebar displays,
    /// read live at each press — pageup/down must always correspond to what
    /// the left sidebar shows (owner decision 2026-08-02; native mirrors).
    /// With pins that is the pinned section first, then the unpinned band —
    /// i.e. render order top-to-bottom.
    pub fn displayed_active_ring(&self) -> Vec<String> {
        let mut out = self.pinned_workspaces();
        out.extend(self.unpinned_active_workspaces());
        out
    }

    /// Bump recency (human typing here / context-menu touch / fresh spawn).
    pub fn touch_workspace(&mut self, ws: &str, now_ms: f64) {
        if !ws.is_empty() {
            self.workspace_touch.insert(ws.to_string(), now_ms);
        }
    }

    /// Finish detection: a circle that stops working gets a touch so it lands
    /// at the top of the idle band (freshly finished work is what you want
    /// next). Call once per frame; cheap.
    pub fn sync_working_touches(&mut self, now_ms: f64) {
        let names = self.workspaces();
        for ws in names {
            let now_working = self.workspace_has_working_agent(&ws);
            let was = self.workspace_was_working.contains(&ws);
            if was && !now_working {
                self.touch_workspace(&ws, now_ms);
                self.workspace_working_since.remove(&ws);
            }
            if now_working && !was {
                self.workspace_working_since.insert(ws.clone(), now_ms);
            }
            if now_working {
                self.workspace_was_working.insert(ws);
            } else {
                self.workspace_was_working.remove(&ws);
            }
        }
    }

    /// Daemon unix-ms stamp → local `performance.now()` domain.
    fn to_perf(&self, unix_ms: u64) -> f64 {
        unix_ms as f64 - self.clock_offset_ms
    }

    /// MAX-merge a daemon output clock into the local mirror.
    fn merge_activity(&mut self, ws: &str, unix_ms: u64) {
        if unix_ms == 0 || ws.is_empty() {
            return;
        }
        let t = self.to_perf(unix_ms);
        let cur = self.workspace_activity.get(ws).copied().unwrap_or(f64::MIN);
        if t > cur {
            self.workspace_activity.insert(ws.to_string(), t);
        }
    }

    /// MAX-merge a daemon touch clock into the local mirror.
    fn merge_touch(&mut self, ws: &str, unix_ms: u64) {
        if unix_ms == 0 || ws.is_empty() {
            return;
        }
        let t = self.to_perf(unix_ms);
        let cur = self.workspace_touch.get(ws).copied().unwrap_or(f64::MIN);
        if t > cur {
            self.workspace_touch.insert(ws.to_string(), t);
        }
    }

    /// Does this incoming frame count as real output for the activity clock?
    ///
    /// Mirrors the native rule (`app::util::grid_frame_is_output`): first
    /// paint isn't output, a dimension change is reflow (and arms the settle
    /// window), and the redraw burst the PTY emits after the resize WE asked
    /// for — the first time a circle's tiles are sized — is reflow too.
    /// Selecting a circle must never bump its last-active time.
    fn grid_frame_is_output(&mut self, pane: &str, snap: &GridSnapshot, now_ms: f64) -> bool {
        let Some(prev) = self.grids.get(pane) else {
            return false;
        };
        let prev_empty = prev.cells.is_empty();
        let dims_match = prev.cols == snap.cols && prev.rows == snap.rows;
        let cells_changed = prev.cells != snap.cells;
        if !dims_match {
            self.resize_settle
                .insert(pane.to_string(), now_ms + RESIZE_SETTLE_MS);
        }
        if prev_empty || !dims_match || !cells_changed {
            return false;
        }
        if self
            .resize_settle
            .get(pane)
            .is_some_and(|until| now_ms < *until)
        {
            return false;
        }
        self.resize_settle.remove(pane);
        true
    }

    fn note_pane_output(&mut self, slug: &str, now_ms: f64) {
        if let Some(ws) = self.pane(slug).map(|p| p.workspace.clone()) {
            self.workspace_activity.insert(ws, now_ms);
        }
    }

    /// Sidebar right-edge label: time since last pane output; empty while a
    /// working spinner owns the slot or before any output was observed.
    pub fn activity_label(&self, ws: &str, now_ms: f64) -> String {
        if self.workspace_has_working_agent(ws) {
            return String::new();
        }
        match self.workspace_activity.get(ws) {
            Some(at) => rel_label(now_ms - at),
            None => String::new(),
        }
    }

    /// Local select bookkeeping: clear sticky unread for the circle.
    pub fn note_selected(&mut self, ws: &str) {
        self.workspace_unread.remove(ws);
    }

    pub fn note_activity(&mut self, at_ms: f64, actor: &str, pane: Option<&str>, text: String) {
        self.activity.push_back(ActivityItem {
            at_ms,
            actor: actor.to_string(),
            pane: pane.map(str::to_string),
            text,
        });
        while self.activity.len() > ACTIVITY_CAP {
            self.activity.pop_front();
        }
    }

    /// Sticky unread bookkeeping for a status event on a non-selected circle
    /// (native `note_workspace_status_event`).
    fn note_status_attention(&mut self, slug: &str, state: &str) {
        let Some(ws) = self.pane(slug).map(|p| p.workspace.clone()) else {
            return;
        };
        if self.selected_workspace.as_deref() == Some(ws.as_str()) {
            self.workspace_unread.remove(&ws);
            return;
        }
        let att = match state {
            "needs-human" | "blocked" | "risky" => Some(Attention::NeedsHuman),
            "done" => Some(Attention::Done),
            "working" | "planning" => Some(Attention::Working),
            _ => None,
        };
        if let Some(a) = att {
            let cur = self.workspace_unread.get(&ws).copied();
            if cur.map(|c| a.priority() > c.priority()).unwrap_or(true) {
                self.workspace_unread.insert(ws, a);
            }
        }
    }

    /// Tiled panes in one workspace, list order (the daemon's persistence key).
    pub fn panes_in(&self, workspace: &str) -> Vec<&PaneInfo> {
        self.panes
            .iter()
            .filter(|p| p.workspace == workspace)
            .collect()
    }

    pub fn pane(&self, slug: &str) -> Option<&PaneInfo> {
        self.panes.iter().find(|p| p.slug == slug)
    }

    /// Fold one daemon event into the store. `now_ms` stamps activity rows
    /// and touch bumps (pass `performance.now()`; tests pass 0).
    pub fn apply_event(&mut self, ev: GuiEvent, now_ms: f64) -> Applied {
        match ev {
            GuiEvent::State {
                panes,
                selected_workspace,
                focused_pane,
                extra_workspaces,
                workspace_order,
                asks,
                statuses,
                window_id,
                windows,
                subscriptions,
                workspace_meta,
            } => {
                // State is global from 0.12 and STAYS global: the sidebar
                // renders the active list plus a parked group built from the
                // same lists, so nothing is dropped at ingest.
                // Drop grids for panes that no longer exist (reattach after
                // daemon restart must not paint ghosts).
                let live: std::collections::HashSet<String> =
                    panes.iter().map(|p| p.slug.clone()).collect();
                self.grids.retain(|slug, _| live.contains(slug));
                self.panes = panes;
                self.selected_workspace = selected_workspace;
                self.focused_pane = focused_pane;
                self.extra_workspaces = extra_workspaces;
                self.workspace_order = workspace_order;
                self.asks = asks;
                self.statuses = statuses
                    .into_iter()
                    .filter(|s| live.contains(&s.slug))
                    .map(|s| (s.slug.clone(), s))
                    .collect();
                self.window_id = window_id;
                self.windows = windows;
                self.subscriptions = subscriptions;
                // Daemon-owned clocks are the durable copy — mirror them
                // (converted to the perf domain, max-merged so a local stamp
                // from a frame we already painted is never walked back).
                // pr_links ride on the same meta rows and arrive for EVERY
                // known workspace, so the map is rebuilt (not merged) — a
                // cleared list must actually clear.
                self.workspace_pr_links.clear();
                for m in workspace_meta {
                    self.merge_activity(&m.workspace, m.last_output_ms);
                    self.merge_touch(&m.workspace, m.last_touch_ms);
                    if !m.pr_links.is_empty() {
                        self.workspace_pr_links
                            .insert(m.workspace.clone(), m.pr_links);
                    }
                }
                // Active/parked bookkeeping: first State seeds the list
                // (migration), every State folds daemon-side auto-subscribes
                // in and prunes circles that are gone.
                let known = self.workspaces();
                let subscriptions = self.subscriptions.clone();
                if self.subs.seeded {
                    self.subs_dirty |= self.subs.reconcile(&subscriptions, &known);
                } else {
                    self.subs.seed(&subscriptions, &known);
                    self.subs_dirty = true;
                }
                self.structure_rev += 1;
                Applied::Structure
            }
            GuiEvent::Grid(snap) => {
                let pane = snap.pane.clone();
                let changed = self.grid_frame_is_output(&pane, &snap, now_ms);
                self.grids.insert(pane.clone(), snap);
                if changed {
                    self.note_pane_output(&pane, now_ms);
                }
                Applied::Grid { pane }
            }
            GuiEvent::GridBin { pane, data_b64 } => {
                let data = match base64::engine::general_purpose::STANDARD.decode(&data_b64) {
                    Ok(d) => d,
                    Err(_) => return Applied::NeedRefresh { pane },
                };
                let base = self.grids.get(&pane);
                match decode_grid_bin_onto(&data, base) {
                    Ok(snap) => {
                        // Stamp only real content change — full re-pushes on
                        // attach/pull must not reset the activity clock.
                        let changed = self.grid_frame_is_output(&pane, &snap, now_ms);
                        self.grids.insert(pane.clone(), snap);
                        if changed {
                            self.note_pane_output(&pane, now_ms);
                        }
                        Applied::Grid { pane }
                    }
                    Err(_) => Applied::NeedRefresh { pane },
                }
            }
            GuiEvent::PaneSpawned { pane } => {
                self.last_spawned = Some(pane.slug.clone());
                self.touch_workspace(&pane.workspace.clone(), now_ms);
                self.note_activity(
                    now_ms,
                    "daemon",
                    Some(&pane.slug),
                    format!("pane spawned: {}", pane.name),
                );
                if let Some(existing) = self.panes.iter_mut().find(|p| p.slug == pane.slug) {
                    *existing = pane;
                } else {
                    self.panes.push(pane);
                }
                self.structure_rev += 1;
                Applied::Structure
            }
            GuiEvent::PaneKilled { slug } => {
                self.note_activity(now_ms, "daemon", Some(&slug), "pane killed".into());
                if self.zoomed.as_deref() == Some(slug.as_str()) {
                    self.zoomed = None;
                }
                self.panes.retain(|p| p.slug != slug);
                self.grids.remove(&slug);
                self.statuses.remove(&slug);
                self.agency.remove(&slug);
                self.structure_rev += 1;
                Applied::Structure
            }
            GuiEvent::PaneExited { slug, exit_code } => {
                if let Some(p) = self.panes.iter_mut().find(|p| p.slug == slug) {
                    p.exited = true;
                    p.exit_code = exit_code;
                    p.running = false;
                }
                self.structure_rev += 1;
                Applied::Structure
            }
            GuiEvent::Ask { ask } => {
                self.note_activity(now_ms, &ask.from, None, format!("asks: {}", ask.question));
                if let Some(existing) = self.asks.iter_mut().find(|a| a.id == ask.id) {
                    *existing = ask;
                } else {
                    self.asks.push(ask);
                }
                Applied::Badges
            }
            GuiEvent::AskResolved { id } => {
                self.asks.retain(|a| a.id != id);
                Applied::Badges
            }
            GuiEvent::Status { slug, state, note } => {
                self.note_status_attention(&slug, &state);
                self.note_activity(
                    now_ms,
                    "agent",
                    Some(&slug),
                    match &note {
                        Some(n) => format!("status: {state} — {n}"),
                        None => format!("status: {state}"),
                    },
                );
                let entry = self.statuses.entry(slug.clone()).or_insert(StatusInfo {
                    slug: slug.clone(),
                    state: String::new(),
                    note: None,
                    pad_rev: 0,
                });
                entry.state = state;
                entry.note = note;
                Applied::Badges
            }
            GuiEvent::Touch { slug, verb, actor } => {
                self.note_activity(now_ms, &actor, Some(&slug), verb);
                Applied::Badges
            }
            GuiEvent::InputOrigin { pane, origin } => {
                self.input_origin.insert(pane, origin);
                Applied::Badges
            }
            GuiEvent::Agency {
                pane,
                owner,
                drive_mode,
                human_idle,
                exited,
                exit_code,
            } => {
                self.agency.insert(
                    pane,
                    AgencyState {
                        owner,
                        drive_mode,
                        human_idle,
                        exited,
                        exit_code,
                    },
                );
                Applied::Badges
            }
            GuiEvent::Ghost { pane, ghost } => {
                if let Some(g) = self.grids.get_mut(&pane) {
                    g.ghost = ghost;
                    Applied::Grid { pane }
                } else {
                    Applied::Nothing
                }
            }
            GuiEvent::Activity {
                workspace,
                last_output_ms,
            } => {
                self.merge_activity(&workspace, last_output_ms);
                Applied::Badges
            }
            GuiEvent::Error { message } => Applied::Error { message },
            GuiEvent::Kicked { by } => Applied::Kicked { by },
            GuiEvent::Ack { .. } | GuiEvent::FsResult { .. } => Applied::Nothing,
            GuiEvent::HostWidgets { widgets } => {
                if let Ok(parsed) = serde_json::from_value::<Vec<HostWidget>>(widgets) {
                    self.host_widgets = parsed;
                }
                Applied::Badges
            }
            GuiEvent::Pong => Applied::Nothing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seance_core::snapshot::encode_grid_bin;

    fn state_event() -> GuiEvent {
        serde_json::from_str(
            r#"{"event":"state","panes":[{"kind":"term","name":"w","slug":"w-1",
                "workspace":"lab","command":"bash","cwd":"/","tiled":true,
                "running":true,"title":null,"scratchpad":"/tmp/p"}],
                "selected_workspace":"lab","focused_pane":"w-1",
                "extra_workspaces":[],"workspace_order":["lab"],
                "asks":[],"statuses":[]}"#,
        )
        .unwrap()
    }

    #[test]
    fn state_then_grid_bin_applies() {
        let mut st = ClientState::default();
        assert_eq!(st.apply_event(state_event(), 0.0), Applied::Structure);
        assert_eq!(st.workspaces(), vec!["lab".to_string()]);

        let mut snap = GridSnapshot::empty("w-1");
        snap.rev = 1;
        let bin = encode_grid_bin(&snap).unwrap();
        let ev = GuiEvent::GridBin {
            pane: "w-1".into(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(bin),
        };
        assert_eq!(
            st.apply_event(ev, 0.0),
            Applied::Grid { pane: "w-1".into() }
        );
        assert_eq!(st.grids.get("w-1").unwrap().rev, 1);
    }

    #[test]
    fn damage_without_base_requests_refresh() {
        let mut st = ClientState::default();
        let mut snap = GridSnapshot::empty("w-1");
        snap.rev = 2;
        snap.cells = vec![seance_core::snapshot::CellSnap::blank(); 80 * 24];
        let mut next = snap.clone();
        next.rev = 3;
        next.cells[0].c = 'x';
        let dirty = seance_core::snapshot::dirty_rows(&snap.cells, &next.cells, 80, 24);
        let bin = seance_core::snapshot::encode_grid_bin_ex(&next, Some(&dirty)).unwrap();
        let ev = GuiEvent::GridBin {
            pane: "w-1".into(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(bin),
        };
        // No base grid stored → decoder fails → refresh requested.
        assert_eq!(
            st.apply_event(ev, 0.0),
            Applied::NeedRefresh { pane: "w-1".into() }
        );
    }

    fn grid_at(cols: u16, rows: u16, first: char) -> GuiEvent {
        let mut snap = GridSnapshot::empty("w-1");
        snap.cols = cols;
        snap.rows = rows;
        snap.cells = vec![seance_core::snapshot::CellSnap::blank(); cols as usize * rows as usize];
        snap.cells[0].c = first;
        GuiEvent::Grid(snap)
    }

    /// Selecting a circle for the first time lays out its tiles, which sends a
    /// PTY resize; the redraw that comes back must not bump the activity clock.
    #[test]
    fn resize_reflow_redraw_is_not_activity() {
        let mut st = ClientState::default();
        st.apply_event(state_event(), 0.0);
        // First paint at the daemon's stored size — not output.
        st.apply_event(grid_at(80, 24, 'a'), 1_000.0);
        assert!(st.workspace_activity.get("lab").is_none());
        // Our resize lands: new dims → reflow, arms the settle window.
        st.apply_event(grid_at(120, 40, 'a'), 1_010.0);
        assert!(st.workspace_activity.get("lab").is_none());
        // The SIGWINCH redraw: same dims, different cells, inside the window.
        st.apply_event(grid_at(120, 40, 'b'), 1_050.0);
        assert!(st.workspace_activity.get("lab").is_none());
        // Real output after the window → stamps.
        st.apply_event(grid_at(120, 40, 'c'), 2_000.0);
        assert_eq!(st.workspace_activity.get("lab"), Some(&2_000.0));
    }

    fn state_event_with_meta(out_ms: u64, touch_ms: u64) -> GuiEvent {
        serde_json::from_str(&format!(
            r#"{{"event":"state","panes":[{{"kind":"term","name":"w","slug":"w-1",
                "workspace":"lab","command":"bash","cwd":"/","tiled":true,
                "running":true,"title":null,"scratchpad":"/tmp/p"}}],
                "selected_workspace":"lab","focused_pane":"w-1",
                "extra_workspaces":[],"workspace_order":["lab"],
                "asks":[],"statuses":[],
                "workspace_meta":[{{"workspace":"lab","last_output_ms":{out_ms},
                                   "last_touch_ms":{touch_ms}}}]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn legacy_state_without_meta_leaves_clocks_alone() {
        let mut st = ClientState::default();
        st.workspace_activity.insert("lab".into(), 500.0);
        st.apply_event(state_event(), 0.0);
        assert_eq!(st.workspace_activity.get("lab"), Some(&500.0));
    }

    #[test]
    fn daemon_meta_seeds_clocks_and_max_merges() {
        let mut st = ClientState::default();
        // offset 0 → unix ms and perf ms are the same numbers in this test.
        st.apply_event(state_event_with_meta(900, 800), 0.0);
        assert_eq!(st.workspace_activity.get("lab"), Some(&900.0));
        assert_eq!(st.workspace_touch.get("lab"), Some(&800.0));

        // An older daemon stamp must never walk a fresher local one back.
        st.workspace_activity.insert("lab".into(), 1_500.0);
        st.apply_event(state_event_with_meta(900, 800), 0.0);
        assert_eq!(st.workspace_activity.get("lab"), Some(&1_500.0));

        // A newer one wins.
        st.apply_event(state_event_with_meta(2_000, 1_900), 0.0);
        assert_eq!(st.workspace_activity.get("lab"), Some(&2_000.0));
        assert_eq!(st.workspace_touch.get("lab"), Some(&1_900.0));
    }

    #[test]
    fn zero_clocks_are_unknown_not_epoch() {
        let mut st = ClientState::default();
        st.apply_event(state_event_with_meta(0, 0), 0.0);
        assert!(st.workspace_activity.get("lab").is_none());
        assert!(st.workspace_touch.get("lab").is_none());
    }

    #[test]
    fn activity_event_converts_unix_to_perf_domain() {
        let mut st = ClientState::default();
        // Page booted 10_000ms ago at unix 1_700_000_000_000.
        st.clock_offset_ms = 1_700_000_000_000.0 - 10_000.0;
        st.apply_event(
            GuiEvent::Activity {
                workspace: "lab".into(),
                last_output_ms: 1_700_000_005_000,
            },
            12_000.0,
        );
        // unix 1_700_000_005_000 → perf 15_000.
        assert_eq!(st.workspace_activity.get("lab"), Some(&15_000.0));
        assert_eq!(st.activity_label("lab", 25_000.0), "10s");
    }

    /// Two circles, only `lab` subscribed — the 0.12 global State shape.
    fn global_state_event() -> GuiEvent {
        serde_json::from_str(
            r#"{"event":"state","panes":[
                {"kind":"term","name":"w","slug":"w-1","workspace":"lab",
                 "command":"bash","cwd":"/","tiled":true,"running":true,
                 "title":null,"scratchpad":"/tmp/p"},
                {"kind":"term","name":"r","slug":"r-1","workspace":"raid",
                 "command":"bash","cwd":"/","tiled":true,"running":true,
                 "title":null,"scratchpad":"/tmp/r"}],
                "selected_workspace":"lab","focused_pane":"w-1",
                "extra_workspaces":[],"workspace_order":["lab","raid"],
                "asks":[],"statuses":[],"subscriptions":["lab"]}"#,
        )
        .unwrap()
    }

    /// Both circles active, `raid` carrying a PR link with `attention`.
    fn pr_state_event(attention: &str) -> GuiEvent {
        let att = if attention.is_empty() {
            "null".to_string()
        } else {
            format!("\"{attention}\"")
        };
        serde_json::from_str(&format!(
            r#"{{"event":"state","panes":[
                {{"kind":"term","name":"r","slug":"r-1","workspace":"raid",
                 "command":"bash","cwd":"/","tiled":true,"running":true,
                 "title":null,"scratchpad":"/tmp/r"}}],
                "selected_workspace":"raid","focused_pane":"r-1",
                "extra_workspaces":["lab"],"workspace_order":["lab","raid"],
                "asks":[],"statuses":[],"subscriptions":["lab","raid"],
                "workspace_meta":[{{"workspace":"raid","last_output_ms":0,
                  "last_touch_ms":0,"pr_links":[
                    {{"url":"https://github.com/o/r/pull/7","seen_ms":1,
                      "status":{{"state":"open","attention":null,
                        "label":"2 comments","updated_ms":1}}}},
                    {{"url":"https://github.com/o/r/pull/42","seen_ms":2,
                      "status":{{"state":"open","attention":{att},
                        "label":"CI ✗","updated_ms":2}}}}]}}]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn pr_number_parses_trailing_paths() {
        assert_eq!(pr_number("https://github.com/o/r/pull/42"), Some(42));
        assert_eq!(pr_number("https://github.com/o/r/pull/42/files"), Some(42));
        assert_eq!(pr_number("https://github.com/o/r/issues/42"), None);
        assert_eq!(pr_number("https://github.com/o/r/pull/x"), None);
    }

    #[test]
    fn pr_links_land_per_workspace_most_recent_last() {
        let mut st = ClientState::default();
        st.apply_event(pr_state_event("needs"), 0.0);
        assert_eq!(st.pr_links("raid").len(), 2);
        assert_eq!(
            st.latest_pr_link("raid").map(|l| l.url.as_str()),
            Some("https://github.com/o/r/pull/42")
        );
        assert!(st.pr_links("lab").is_empty());
        // A State with no links must actually clear them (rebuild, not merge).
        st.apply_event(global_state_event(), 0.0);
        assert!(st.pr_links("raid").is_empty());
    }

    #[test]
    fn pr_attention_needs_beats_done_and_live_work_beats_both() {
        let mut st = ClientState::default();
        st.apply_event(pr_state_event("needs"), 0.0);
        st.subs.mark_seen("raid");
        assert_eq!(st.pr_attention("raid"), Some(Attention::NeedsHuman));
        assert_eq!(st.row_attention("raid"), Some(Attention::NeedsHuman));

        st.apply_event(pr_state_event("done"), 0.0);
        st.subs.mark_seen("raid");
        assert_eq!(st.row_attention("raid"), Some(Attention::Done));

        // Live working outranks a done PR.
        st.statuses.insert(
            "r-1".into(),
            serde_json::from_str(r#"{"slug":"r-1","state":"working","note":null}"#).unwrap(),
        );
        assert_eq!(st.row_attention("raid"), Some(Attention::Working));

        // No verdict at all → no PR-driven badge.
        st.statuses.clear();
        st.apply_event(pr_state_event(""), 0.0);
        st.subs.mark_seen("raid");
        assert_eq!(st.pr_attention("raid"), None);
    }

    #[test]
    fn displayed_active_ring_is_the_sidebar_order_live() {
        let mut st = ClientState::default();
        st.apply_event(pr_state_event("needs"), 0.0);
        st.subs.activate("lab");
        st.touch_workspace("raid", 100.0);
        assert_eq!(
            st.displayed_active_ring(),
            vec!["raid".to_string(), "lab".to_string()]
        );
        // A resort is reflected immediately — the ring IS the display.
        st.touch_workspace("lab", 200.0);
        assert_eq!(
            st.displayed_active_ring(),
            vec!["lab".to_string(), "raid".to_string()]
        );
    }

    #[test]
    fn pinned_section_floats_above_the_active_band_and_owns_the_ring() {
        let mut st = ClientState::default();
        st.apply_event(pr_state_event("needs"), 0.0);
        st.subs.activate("lab");
        st.touch_workspace("raid", 100.0);
        st.touch_workspace("lab", 200.0);
        // Unpinned order: lab (fresher touch) then raid.
        assert_eq!(st.unpinned_active_workspaces(), vec!["lab", "raid"]);
        assert!(st.pinned_workspaces().is_empty());

        // Pin the STALER circle: it jumps the whole band.
        st.subs.pin("raid");
        assert_eq!(st.pinned_workspaces(), vec!["raid".to_string()]);
        assert_eq!(st.unpinned_active_workspaces(), vec!["lab".to_string()]);
        assert_eq!(
            st.displayed_active_ring(),
            vec!["raid".to_string(), "lab".to_string()]
        );
        // Ring == pinned ++ unpinned == render order, and nothing is dropped
        // or duplicated relative to the flat active list.
        let mut ring = st.displayed_active_ring();
        let mut active = st.active_workspaces();
        ring.sort();
        active.sort();
        assert_eq!(ring, active);
    }

    #[test]
    fn pinned_subset_keeps_the_working_idle_sort_internally() {
        let mut st = ClientState::default();
        st.apply_event(pr_state_event("needs"), 0.0);
        st.subs.activate("lab");
        st.subs.pin("lab");
        st.subs.pin("raid");
        st.touch_workspace("lab", 100.0);
        st.touch_workspace("raid", 200.0);
        assert_eq!(st.pinned_workspaces(), vec!["raid", "lab"]);
        st.touch_workspace("lab", 300.0);
        assert_eq!(st.pinned_workspaces(), vec!["lab", "raid"]);
    }

    #[test]
    fn global_state_is_kept_whole_not_filtered_to_subscriptions() {
        let mut st = ClientState::default();
        st.apply_event(global_state_event(), 0.0);
        // Both circles and both panes survive ingest — the split is a render
        // concern now, not an ingest filter.
        assert_eq!(st.panes.len(), 2);
        assert_eq!(st.workspaces(), vec!["lab".to_string(), "raid".to_string()]);
    }

    #[test]
    fn first_state_seeds_the_active_list_from_the_daemon_set() {
        let mut st = ClientState::default();
        st.apply_event(global_state_event(), 0.0);
        assert!(st.subs_dirty, "seeding must be persisted");
        assert_eq!(st.active_workspaces(), vec!["lab".to_string()]);
        assert_eq!(st.parked_workspaces(), vec!["raid".to_string()]);
        // Migration marks pre-existing circles seen: no retroactive `needs`.
        assert_eq!(st.row_attention("raid"), None);
        assert_eq!(st.parked_attention(), None);
    }

    /// `lab` only, subscribed — the pre-existing world before a ctl spawn.
    fn lab_only_event() -> GuiEvent {
        serde_json::from_str(
            r#"{"event":"state","panes":[
                {"kind":"term","name":"w","slug":"w-1","workspace":"lab",
                 "command":"bash","cwd":"/","tiled":true,"running":true,
                 "title":null,"scratchpad":"/tmp/p"}],
                "selected_workspace":"lab","focused_pane":"w-1",
                "extra_workspaces":[],"workspace_order":["lab"],
                "asks":[],"statuses":[],"subscriptions":["lab"]}"#,
        )
        .unwrap()
    }

    #[test]
    fn ctl_spawned_circle_lands_parked_and_needs_human() {
        let mut st = ClientState::default();
        // Seed with `lab` alone…
        st.apply_event(lab_only_event(), 0.0);
        assert_eq!(st.parked_workspaces(), Vec::<String>::new());
        // …then `raid` appears with nobody subscribed to it.
        st.subs_dirty = false;
        st.apply_event(global_state_event(), 0.0);
        assert_eq!(st.parked_workspaces(), vec!["raid".to_string()]);
        assert_eq!(st.row_attention("raid"), Some(Attention::NeedsHuman));
        assert_eq!(st.parked_attention(), Some(Attention::NeedsHuman));
        // First select acknowledges it.
        st.subs.mark_seen("raid");
        assert_eq!(st.row_attention("raid"), None);
    }

    #[test]
    fn parking_moves_a_circle_between_the_two_lists() {
        let mut st = ClientState::default();
        st.apply_event(global_state_event(), 0.0);
        st.subs.activate("raid");
        assert_eq!(
            st.active_workspaces(),
            vec!["lab".to_string(), "raid".to_string()]
        );
        assert!(st.parked_workspaces().is_empty());
        st.subs.park("lab");
        assert_eq!(st.active_workspaces(), vec!["raid".to_string()]);
        assert_eq!(st.parked_workspaces(), vec!["lab".to_string()]);
    }

    #[test]
    fn both_lists_share_one_sort() {
        let mut st = ClientState::default();
        st.apply_event(global_state_event(), 0.0);
        // Idle band sorts by most recent activity; park both so one call
        // proves the parked list uses the same key as the active one.
        st.subs.park("lab");
        st.workspace_activity.insert("lab".into(), 100.0);
        st.workspace_activity.insert("raid".into(), 900.0);
        assert_eq!(
            st.parked_workspaces(),
            vec!["raid".to_string(), "lab".to_string()]
        );
        assert_eq!(st.parked_workspaces(), st.workspaces());
    }

    #[test]
    fn kill_prunes_everything() {
        let mut st = ClientState::default();
        st.apply_event(state_event(), 0.0);
        st.grids.insert("w-1".into(), GridSnapshot::empty("w-1"));
        st.apply_event(GuiEvent::PaneKilled { slug: "w-1".into() }, 0.0);
        assert!(st.panes.is_empty() && st.grids.is_empty());
    }

    #[test]
    fn remove_pr_link_drops_one_and_prunes_empty_lists() {
        let mut st = ClientState::default();
        let mk = |u: &str| PrLink {
            url: u.to_string(),
            status: None,
            seen_ms: 0,
        };
        st.workspace_pr_links.insert(
            "lab".into(),
            vec![mk("https://x/pull/1"), mk("https://x/pull/2")],
        );
        assert!(st.remove_pr_link("lab", "https://x/pull/1"));
        assert_eq!(st.pr_links("lab").len(), 1);
        // Unknown url / unknown circle are no-ops.
        assert!(!st.remove_pr_link("lab", "https://x/pull/9"));
        assert!(!st.remove_pr_link("nope", "https://x/pull/2"));
        // Last one out prunes the key, so the chip disappears entirely.
        assert!(st.remove_pr_link("lab", "https://x/pull/2"));
        assert!(st.pr_links("lab").is_empty());
        assert!(!st.workspace_pr_links.contains_key("lab"));
        assert!(st.latest_pr_link("lab").is_none());
    }
}
