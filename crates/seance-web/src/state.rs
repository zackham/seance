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
}

impl ClientState {
    /// Ordered workspace names: explicit order first, then any stragglers
    /// (pane-derived or extra) in first-seen order.
    pub fn workspaces(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen = |out: &mut Vec<String>, name: &str| {
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
        out
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

    /// Fold one daemon event into the store.
    pub fn apply_event(&mut self, ev: GuiEvent) -> Applied {
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
                if let Some(existing) = self.panes.iter_mut().find(|p| p.slug == pane.slug) {
                    *existing = pane;
                } else {
                    self.panes.push(pane);
                }
                self.structure_rev += 1;
                Applied::Structure
            }
            GuiEvent::PaneKilled { slug } => {
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
            GuiEvent::Touch { .. } => Applied::Nothing,
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
            GuiEvent::HostWidgets { .. } => Applied::Nothing,
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
        assert_eq!(st.apply_event(state_event()), Applied::Structure);
        assert_eq!(st.workspaces(), vec!["lab".to_string()]);

        let mut snap = GridSnapshot::empty("w-1");
        snap.rev = 1;
        let bin = encode_grid_bin(&snap).unwrap();
        let ev = GuiEvent::GridBin {
            pane: "w-1".into(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(bin),
        };
        assert_eq!(
            st.apply_event(ev),
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
            st.apply_event(ev),
            Applied::NeedRefresh {
                pane: "w-1".into()
            }
        );
    }

    #[test]
    fn kill_prunes_everything() {
        let mut st = ClientState::default();
        st.apply_event(state_event());
        st.grids.insert("w-1".into(), GridSnapshot::empty("w-1"));
        st.apply_event(GuiEvent::PaneKilled { slug: "w-1".into() });
        assert!(st.panes.is_empty() && st.grids.is_empty());
    }
}
