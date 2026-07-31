//! Client-side session state: the single store every module reads.
//!
//! [`ClientState::apply_event`] folds daemon pushes into the store and reports
//! what changed as a [`Applied`] so the caller repaints exactly what's dirty —
//! grid frames repaint one canvas, structure changes rebuild chrome.

use std::collections::HashMap;

use base64::Engine as _;
use seance_core::protocol::{
    AskInfo, ForeignWorkspace, GuiEvent, PaneInfo, StatusInfo, WindowInfo,
};
use seance_core::snapshot::{decode_grid_bin_onto, GridSnapshot};

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
    pub foreign_workspaces: Vec<ForeignWorkspace>,
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
                let band = if self.workspace_has_working_agent(ws) { 0u8 } else { 1 };
                let touch = self.workspace_touch.get(ws).copied().unwrap_or(0.0);
                (band, std::cmp::Reverse(touch as u64))
            };
            key(a).cmp(&key(b)).then_with(|| a.cmp(b))
        });
        out
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
        self.workspace_unread.get(workspace).copied()
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
            }
            if now_working {
                self.workspace_was_working.insert(ws);
            } else {
                self.workspace_was_working.remove(&ws);
            }
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
                foreign_workspaces,
            } => {
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
                    .map(|s| (s.slug.clone(), s))
                    .collect();
                self.window_id = window_id;
                self.windows = windows;
                self.foreign_workspaces = foreign_workspaces;
                self.structure_rev += 1;
                Applied::Structure
            }
            GuiEvent::Grid(snap) => {
                let pane = snap.pane.clone();
                self.grids.insert(pane.clone(), snap);
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
                        self.grids.insert(pane.clone(), snap);
                        Applied::Grid { pane }
                    }
                    Err(_) => Applied::NeedRefresh { pane },
                }
            }
            GuiEvent::PaneSpawned { pane } => {
                self.last_spawned = Some(pane.slug.clone());
                self.touch_workspace(&pane.workspace.clone(), now_ms);
                self.note_activity(now_ms, "daemon", Some(&pane.slug), format!("pane spawned: {}", pane.name));
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
            GuiEvent::Error { message } => Applied::Error { message },
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
            Applied::Grid {
                pane: "w-1".into()
            }
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
            Applied::NeedRefresh {
                pane: "w-1".into()
            }
        );
    }

    #[test]
    fn kill_prunes_everything() {
        let mut st = ClientState::default();
        st.apply_event(state_event(), 0.0);
        st.grids.insert("w-1".into(), GridSnapshot::empty("w-1"));
        st.apply_event(GuiEvent::PaneKilled { slug: "w-1".into() }, 0.0);
        assert!(st.panes.is_empty() && st.grids.is_empty());
    }
}
