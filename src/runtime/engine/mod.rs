//! Session engine: panes, control plane, layout state. gpui-free.

mod control;
pub(crate) mod helpers;

#[cfg(test)]
mod tests;

// Re-export helpers used by sibling modules / tests.
pub use helpers::OwnedFdAdopt;

pub(crate) use helpers::{
    assert_self_or_cross, atomic_append_pad, atomic_write_pad, base64_decode, chrono_lite_stamp,
    now_ms, shell_rc_path, task_json, validate_status, write_task_sidecar,
};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::Result;
use serde_json::json;

use super::protocol::*;
use super::pty_session::{AdoptedPty, PtySession, SessionEvent, SpawnConfig};
use super::snapshot::{
    dirty_rows, encode_grid_bin, encode_grid_bin_ex, CellSnap, GhostSnap, GridSnapshot,
};
use crate::cmdlog::CommandLog;
use crate::control::{ControlRequest, ControlResponse};
use crate::events;
use crate::scratchpad::ScratchpadStore;
use crate::state::{slugify, unique_slug, AppState, PersistedPane};

const DEFAULT_COMMAND: &str = "bash -l";
const DEFAULT_WORKSPACE: &str = "main";

pub struct SpawnSpec {
    pub name: String,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub workspace: Option<String>,
    pub tiled: bool,
    pub resume: bool,
    pub file: Option<String>,
}

/// One pane record in the engine.
pub struct EnginePane {
    pub kind: String, // "terminal" | "file"
    pub name: String,
    pub slug: String,
    pub workspace: String,
    pub cwd: String,
    pub command: String,
    pub tiled: bool,
    pub resume_on_restore: bool,
    pub scratch_path: PathBuf,
    pub file: Option<String>,
    pub session: Option<PtySession>,
    /// Co-presence: who holds the keys.
    pub agency: crate::agency::Agency,
}

pub struct PendingAsk {
    pub id: String,
    pub from: String,
    pub workspace: Option<String>,
    pub question: String,
    pub choices: Vec<String>,
    pub answer: Option<String>,
}

pub struct Engine {
    pub panes: Vec<EnginePane>,
    pub selected_workspace: Option<String>,
    pub focused_pane: Option<String>,
    pub extra_workspaces: Vec<String>,
    pub workspace_order: Vec<String>,
    pub store: ScratchpadStore,
    pub cmd_log: CommandLog,
    pub asks: Vec<PendingAsk>,
    /// status slug → (state, note)
    pub statuses: HashMap<String, (String, Option<String>)>,
    /// Scratchpad revision per pane (bumped on note/finish/atomic pad write).
    pub pad_revs: HashMap<String, u64>,
    /// At last inject: (pad_rev, pad_bytes) — evidence for wait --since-inject.
    pub inject_baselines: HashMap<String, (u64, u64)>,
    /// task_id → record (dispatch envelope + durable inbox body).
    pub tasks: HashMap<String, TaskRecord>,
    /// pane slug → active open task_id.
    pub active_tasks: HashMap<String, String>,
    pub task_counter: u64,
    pub proposals: HashMap<String, (String, Option<String>)>,
    pub proposal_counter: u64,
    pub ask_counter: u64,
    /// Capability / consent store (daemon-enforced policy).
    pub caps: crate::caps::CapStore,
    event_tx: Sender<SessionEvent>,
    /// Live GUI windows (one connection = one window).
    gui_conns: Vec<GuiConn>,
    /// workspace name → owning window id (exclusive).
    workspace_window: HashMap<String, String>,
    next_window_seq: u64,
    /// Per-pane last full-grid push — TUIs with spinners wake the PTY dozens of
    /// times per second; unthrottled snapshots peg the GUI.
    last_grid_push: HashMap<String, Instant>,
    /// FlushGrid already scheduled for this slug (avoid timer storms).
    grid_flush_pending: HashSet<String>,
    /// Last cells we broadcast per pane — enables row-damage frames + skip
    /// when nothing changed.
    last_grid_cells: HashMap<String, LastGridFrame>,
}

/// One GUI window connection.
struct GuiConn {
    id: String,
    tx: Sender<GuiEvent>,
    selected_workspace: Option<String>,
    focused_pane: Option<String>,
    overview: bool,
}

/// Cached last broadcast for damage detection (Arc so we don't clone every push).
struct LastGridFrame {
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
    /// Empty engine with no panes and no disk state load — unit tests only.
    /// Uses an isolated scratch directory and open (unrestricted) caps.
    #[cfg(test)]
    pub fn bare_for_test(scratch_dir: PathBuf) -> (Self, Receiver<SessionEvent>) {
        let store = ScratchpadStore::with_dir(scratch_dir).expect("test scratch dir");
        let (event_tx, event_rx) = mpsc::channel();
        let eng = Self {
            panes: Vec::new(),
            selected_workspace: Some(DEFAULT_WORKSPACE.into()),
            focused_pane: None,
            extra_workspaces: Vec::new(),
            workspace_order: vec![DEFAULT_WORKSPACE.into()],
            store,
            cmd_log: CommandLog::default(),
            asks: Vec::new(),
            statuses: HashMap::new(),
            pad_revs: HashMap::new(),
            inject_baselines: HashMap::new(),
            tasks: HashMap::new(),
            active_tasks: HashMap::new(),
            task_counter: 0,
            proposals: HashMap::new(),
            proposal_counter: 0,
            ask_counter: 0,
            caps: crate::caps::CapStore::default(), // PolicyMode::Open
            event_tx,
            gui_conns: Vec::new(),
            workspace_window: HashMap::new(),
            next_window_seq: 1,
            last_grid_push: HashMap::new(),
            grid_flush_pending: HashSet::new(),
            last_grid_cells: HashMap::new(),
        };
        (eng, event_rx)
    }

    /// Register a no-PTY placeholder pane (tests / file-like control paths).
    #[cfg(test)]
    pub fn push_stub_pane(&mut self, name: &str, workspace: &str) -> String {
        let taken: Vec<&str> = self.panes.iter().map(|p| p.slug.as_str()).collect();
        let slug = unique_slug(name, &taken);
        let scratch_path = self.store.path_for(&slug);
        self.panes.push(EnginePane {
            kind: "terminal".into(),
            name: name.into(),
            slug: slug.clone(),
            workspace: workspace.into(),
            cwd: "/tmp".into(),
            command: DEFAULT_COMMAND.into(),
            tiled: true,
            resume_on_restore: false,
            scratch_path,
            file: None,
            session: None,
            agency: crate::agency::Agency::default(),
        });
        slug
    }

    pub fn new() -> Result<(Self, Receiver<SessionEvent>)> {
        let store = ScratchpadStore::new()?;
        let (event_tx, event_rx) = mpsc::channel();
        let state = AppState::load();

        let mut eng = Self {
            panes: Vec::new(),
            selected_workspace: state.selected_workspace.clone(),
            focused_pane: state.active_slug.clone(),
            extra_workspaces: state.extra_workspaces.clone(),
            workspace_order: state.workspace_order.clone(),
            store,
            cmd_log: state.cmd_log.clone(),
            asks: Vec::new(),
            statuses: HashMap::new(),
            pad_revs: HashMap::new(),
            inject_baselines: HashMap::new(),
            tasks: HashMap::new(),
            active_tasks: HashMap::new(),
            task_counter: state.task_counter,
            proposals: HashMap::new(),
            proposal_counter: 0,
            ask_counter: 0,
            caps: crate::caps::CapStore::load(),
            event_tx,
            gui_conns: Vec::new(),
            workspace_window: HashMap::new(),
            next_window_seq: 1,
            last_grid_push: HashMap::new(),
            grid_flush_pending: HashSet::new(),
            last_grid_cells: HashMap::new(),
        };

        for t in state.tasks {
            eng.tasks.insert(t.id.clone(), t);
        }
        for (slug, tid) in state.active_tasks {
            eng.active_tasks.insert(slug, tid);
        }

        for p in &state.panes {
            // Drop legacy tombstones — exited panes are auto-closed now.
            if p.exited {
                continue;
            }
            let slug = p.slug.clone();
            if let Some(st) = &p.status {
                eng.statuses
                    .insert(slug.clone(), (st.clone(), p.status_note.clone()));
            }
            if p.pad_rev > 0 {
                eng.pad_revs.insert(slug.clone(), p.pad_rev);
            }
            if let (Some(r), Some(b)) = (p.inject_pad_rev, p.inject_pad_bytes) {
                eng.inject_baselines.insert(slug.clone(), (r, b));
            }
            let _ = eng.spawn_from_persisted(p);
            // Restore agency onto the pane we just spawned.
            if let Some(pane) = eng.panes.iter_mut().find(|x| x.slug == slug) {
                let snap = crate::agency::AgencySnap {
                    owner: p.owner.clone().unwrap_or_else(|| "none".into()),
                    drive_mode: p.drive_mode.clone().unwrap_or_else(|| "pair".into()),
                    exited: false,
                    exit_code: None,
                };
                pane.agency = crate::agency::Agency::from_snap(&snap);
            }
        }

        if eng.panes.is_empty() {
            let _ = eng.spawn(SpawnSpec {
                name: "familiar".into(),
                cwd: None,
                command: None,
                workspace: None,
                tiled: true,
                resume: false,
                file: None,
            });
        }

        if eng.selected_workspace.is_none() {
            eng.selected_workspace = eng.panes.first().map(|p| p.workspace.clone());
        }

        Ok((eng, event_rx))
    }

    /// Restore from a graceful-upgrade handoff bundle (FDs already adopted).
    pub fn from_handoff(
        bundle: HandoffBundle,
        adopted: Vec<(usize, OwnedFdAdopt)>,
    ) -> Result<(Self, Receiver<SessionEvent>)> {
        let store = ScratchpadStore::new()?;
        let (event_tx, event_rx) = mpsc::channel();
        let mut eng = Self {
            panes: Vec::new(),
            selected_workspace: bundle.selected_workspace,
            focused_pane: bundle.focused_pane,
            extra_workspaces: bundle.extra_workspaces,
            workspace_order: bundle.workspace_order,
            store,
            cmd_log: bundle.cmd_log.clone(),
            asks: bundle
                .asks
                .into_iter()
                .map(|a| PendingAsk {
                    id: a.id,
                    from: a.from,
                    workspace: a.workspace,
                    question: a.question,
                    choices: a.choices,
                    answer: a.answer,
                })
                .collect(),
            statuses: {
                let mut m = HashMap::new();
                for s in &bundle.statuses {
                    m.insert(s.slug.clone(), (s.state.clone(), s.note.clone()));
                }
                m
            },
            pad_revs: {
                let mut m: HashMap<String, u64> = bundle.pad_revs.into_iter().collect();
                for s in &bundle.statuses {
                    m.entry(s.slug.clone()).or_insert(s.pad_rev);
                }
                m
            },
            inject_baselines: bundle
                .inject_baselines
                .into_iter()
                .map(|b| (b.slug, (b.pad_rev, b.pad_bytes)))
                .collect(),
            tasks: bundle
                .tasks
                .into_iter()
                .map(|t| (t.id.clone(), t))
                .collect(),
            active_tasks: bundle.active_tasks.into_iter().collect(),
            task_counter: bundle.task_counter,
            proposals: HashMap::new(),
            proposal_counter: bundle.proposal_counter,
            ask_counter: bundle.ask_counter,
            caps: crate::caps::CapStore::load(),
            event_tx: event_tx.clone(),
            gui_conns: Vec::new(),
            workspace_window: HashMap::new(),
            next_window_seq: 1,
            last_grid_push: HashMap::new(),
            grid_flush_pending: HashSet::new(),
            last_grid_cells: HashMap::new(),
        };

        let mut fd_map: HashMap<usize, OwnedFd> =
            adopted.into_iter().map(|(i, o)| (i, o.fd)).collect();

        for hp in bundle.panes {
            let agency = hp
                .agency
                .as_ref()
                .map(crate::agency::Agency::from_snap)
                .unwrap_or_default();
            // Drop legacy tombstones — process exit auto-closes panes now.
            if agency.exited {
                continue;
            }
            let scratch_path = eng.store.path_for(&hp.slug);
            if hp.kind == "file" {
                eng.panes.push(EnginePane {
                    kind: "file".into(),
                    name: hp.name,
                    slug: hp.slug,
                    workspace: hp.workspace,
                    cwd: hp.cwd,
                    command: hp.command,
                    tiled: hp.tiled,
                    resume_on_restore: false,
                    scratch_path,
                    file: hp.file,
                    session: None,
                    agency,
                });
                continue;
            }
            // Adopt the handed-off master FD. Do NOT respawn a fresh shell on
            // failure — that silently replaces a live process the old daemon
            // is about to SIGHUP when its last master FD closes, which is how
            // idle shells "disappeared" while busy Claude panes survived.
            let session = if let Some(idx) = hp.fd_index {
                if let Some(fd) = fd_map.remove(&idx) {
                    let pid = hp.child_pid.filter(|p| *p > 0).unwrap_or(0);
                    if pid == 0 {
                        eprintln!(
                            "[seance] handoff adopt {}: missing child pid — keeping FD open only",
                            hp.slug
                        );
                    }
                    let adopted = AdoptedPty {
                        master_fd: fd,
                        child_pid: pid,
                        cols: hp.cols,
                        rows: hp.rows,
                        title: hp.title.clone(),
                        ghost: hp.ghost.clone(),
                    };
                    match PtySession::adopt(hp.slug.clone(), adopted, event_tx.clone()) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            eprintln!(
                                "[seance] handoff adopt failed for {}: {e:#} (not respawning)",
                                hp.slug
                            );
                            None
                        }
                    }
                } else {
                    eprintln!(
                        "[seance] handoff missing SCM_RIGHTS fd for {} (not respawning)",
                        hp.slug
                    );
                    None
                }
            } else {
                eprintln!(
                    "[seance] handoff had no fd_index for terminal {} (prepare failed on old daemon; not respawning)",
                    hp.slug
                );
                None
            };
            eng.panes.push(EnginePane {
                kind: "terminal".into(),
                name: hp.name,
                slug: hp.slug,
                workspace: hp.workspace,
                cwd: hp.cwd,
                command: hp.command,
                tiled: hp.tiled,
                resume_on_restore: hp.resume_on_restore,
                scratch_path,
                file: None,
                session,
                agency,
            });
        }
        Ok((eng, event_rx))
    }

    /// Register a GUI window connection; returns its window id.
    pub fn register_gui(&mut self, tx: Sender<GuiEvent>) -> String {
        let id = format!("w{}", self.next_window_seq);
        self.next_window_seq = self.next_window_seq.wrapping_add(1).max(1);
        self.gui_conns.push(GuiConn {
            id: id.clone(),
            tx,
            selected_workspace: None,
            focused_pane: None,
            overview: false,
        });
        id
    }

    pub fn unregister_gui(&mut self, window_id: &str) {
        let was_registered = self.gui_conns.iter().any(|c| c.id == window_id);
        self.gui_conns.retain(|c| c.id != window_id);
        let orphans: Vec<String> = self
            .workspace_window
            .iter()
            .filter(|(_, w)| *w == window_id)
            .map(|(ws, _)| ws.clone())
            .collect();
        // Also claim workspaces still pointing at this id even if conn was
        // already pruned (Bye then EOF double-call).
        if orphans.is_empty() && !was_registered {
            return;
        }
        if orphans.is_empty() {
            // Still notify peers that this window vanished from the list.
            self.push_state_to_all();
            return;
        }
        if self.gui_conns.is_empty() {
            // Last window closed — truly orphan (no owner). Next first window
            // attach will collect everything.
            for ws in &orphans {
                self.workspace_window.remove(ws);
            }
            return;
        }
        // Survivors exist — dump into the fullest window (never orphan).
        let target = self
            .gui_conns
            .iter()
            .map(|c| {
                let n = self.workspaces_for_window(&c.id).len();
                (n, c.id.clone())
            })
            .max_by_key(|(n, id)| (*n, id.clone()))
            .map(|(_, id)| id)
            .unwrap_or_else(|| self.gui_conns[0].id.clone());
        for ws in &orphans {
            self.workspace_window.insert(ws.clone(), target.clone());
        }
        let owned_now = self.workspaces_for_window(&target);
        if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == target) {
            let sel_ok = c
                .selected_workspace
                .as_ref()
                .is_some_and(|s| owned_now.iter().any(|o| o == s));
            if !sel_ok {
                c.selected_workspace = orphans
                    .first()
                    .cloned()
                    .or_else(|| owned_now.first().cloned());
            }
            self.selected_workspace = c.selected_workspace.clone();
        }
        // Push without prune (avoid re-entrant unregister).
        let ids: Vec<String> = self.gui_conns.iter().map(|c| c.id.clone()).collect();
        for id in ids {
            let st = self.state_for_window(&id);
            self.send_to(&id, st);
        }
        for ws in orphans {
            self.flush_workspace_grids(&ws);
        }
    }

    /// Drop connections whose send channel is dead, reassigning their workspaces.
    pub fn prune_dead_guis(&mut self) {
        let alive: Vec<String> = self
            .gui_conns
            .iter()
            .filter(|c| c.tx.send(GuiEvent::Pong).is_ok())
            .map(|c| c.id.clone())
            .collect();
        let dead: Vec<String> = self
            .gui_conns
            .iter()
            .filter(|c| !alive.iter().any(|a| a == &c.id))
            .map(|c| c.id.clone())
            .collect();
        for id in dead {
            // unregister_gui retains by id — reassign/orphan correctly.
            self.unregister_gui(&id);
        }
    }

    pub fn broadcast(&mut self, ev: GuiEvent) {
        self.gui_conns.retain(|c| c.tx.send(ev.clone()).is_ok());
    }

    fn send_to(&mut self, window_id: &str, ev: GuiEvent) {
        self.gui_conns.retain(|c| {
            if c.id == window_id {
                c.tx.send(ev.clone()).is_ok()
            } else {
                true
            }
        });
    }

    fn send_grid_to_owners(&mut self, pane: &str, ev: GuiEvent) {
        let owner = self.owner_of_pane(pane).map(|s| s.to_string());
        if let Some(oid) = owner {
            self.send_to(&oid, ev);
        } else {
            // Unowned (should be rare) — broadcast so something sees it.
            self.broadcast(ev);
        }
    }

    fn push_state_to_all(&mut self) {
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

    fn workspaces_for_window(&self, window_id: &str) -> Vec<String> {
        let mut owned: Vec<String> = self
            .workspace_window
            .iter()
            .filter(|(_, w)| *w == window_id)
            .map(|(ws, _)| ws.clone())
            .collect();
        // Stable order from workspace_order then leftovers.
        let mut ordered = Vec::new();
        for w in &self.workspace_order {
            if owned.iter().any(|o| o == w) {
                ordered.push(w.clone());
                owned.retain(|o| o != w);
            }
        }
        owned.sort();
        ordered.extend(owned);
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

    fn state_for_window(&self, window_id: &str) -> GuiEvent {
        let owned = self.workspaces_for_window(window_id);
        let owned_set: HashSet<&str> = owned.iter().map(|s| s.as_str()).collect();
        let panes: Vec<PaneInfo> = self
            .pane_infos()
            .into_iter()
            .filter(|p| owned_set.contains(p.workspace.as_str()))
            .collect();
        let conn = self.gui_conns.iter().find(|c| c.id == window_id);
        // Never fall back to another window's selection — that made empty
        // windows inherit the primary's active circle in the sidebar.
        let selected = conn
            .and_then(|c| c.selected_workspace.clone())
            .filter(|w| owned_set.contains(w.as_str()))
            .or_else(|| owned.first().cloned());
        let focused = conn
            .and_then(|c| c.focused_pane.clone())
            .filter(|s| panes.iter().any(|p| p.slug == *s));
        let extra: Vec<String> = self
            .extra_workspaces
            .iter()
            .filter(|w| owned_set.contains(w.as_str()))
            .cloned()
            .collect();
        let order: Vec<String> = owned.clone();
        let foreign: Vec<ForeignWorkspace> = self
            .workspace_window
            .iter()
            .filter(|(_, w)| *w != window_id)
            .map(|(ws, wid)| ForeignWorkspace {
                workspace: ws.clone(),
                window_id: wid.clone(),
                window_label: self.window_label(wid),
            })
            .collect();
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
            .filter(|a| {
                a.workspace
                    .as_ref()
                    .map(|w| owned_set.contains(w.as_str()))
                    .unwrap_or(true)
            })
            .map(|a| AskInfo {
                id: a.id.clone(),
                from: a.from.clone(),
                workspace: a.workspace.clone(),
                question: a.question.clone(),
                choices: a.choices.clone(),
                answer: a.answer.clone(),
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
            foreign_workspaces: foreign,
        }
    }

    fn ensure_workspace_owned(&mut self, workspace: &str, window_id: &str) {
        self.workspace_window
            .entry(workspace.to_string())
            .or_insert_with(|| window_id.to_string());
    }

    fn owner_of_workspace(&self, workspace: &str) -> Option<&str> {
        self.workspace_window.get(workspace).map(|s| s.as_str())
    }

    fn owner_of_pane(&self, slug: &str) -> Option<&str> {
        let ws = self.panes.iter().find(|p| p.slug == slug)?.workspace.as_str();
        self.owner_of_workspace(ws)
    }

    /// Pack a grid as compact `grid_bin` (SCG3 full or row-damage).
    fn grid_event(snap: GridSnapshot, dirty: Option<&[u16]>) -> GuiEvent {
        let enc = match dirty {
            Some(d) => encode_grid_bin_ex(&snap, Some(d)),
            None => encode_grid_bin(&snap),
        };
        match enc {
            Ok(bytes) => {
                use base64::Engine as _;
                let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                GuiEvent::GridBin {
                    pane: snap.pane.clone(),
                    data_b64,
                }
            }
            Err(e) => {
                eprintln!("[seance daemon] grid_bin encode failed: {e}; falling back to JSON");
                GuiEvent::Grid(snap)
            }
        }
    }

    fn broadcast_grid(&mut self, snap: GridSnapshot) {
        let cols = snap.cols as usize;
        let rows = snap.rows as usize;

        let mut damage: Option<Vec<u16>> = None;
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
                        }
                        // title-only: send FULL (damage empty would look like
                        // no-op on the paint path; FULL refreshes title field).
                    } else {
                        damage = Some(vec![snap.cursor_row]);
                    }
                } else if d.len() * 2 < rows.max(1) {
                    damage = Some(d);
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
        let pane = snap.pane.clone();
        let ev = Self::grid_event(snap, damage.as_deref());
        self.send_grid_to_owners(&pane, ev);
    }

    /// Selected workspace on the owning window: ~60fps.
    /// Overview on the owning window: ~15fps for other circles on that window.
    fn grid_interval_for(&self, slug: &str) -> Option<Duration> {
        let ws = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| p.workspace.as_str())?;
        let owner = self.owner_of_workspace(ws)?;
        let conn = self.gui_conns.iter().find(|c| c.id == owner)?;
        if conn.selected_workspace.as_deref() == Some(ws) {
            return Some(Duration::from_millis(16));
        }
        if conn.overview {
            return Some(Duration::from_millis(66));
        }
        None
    }

    fn flush_all_grids(&mut self) {
        let slugs: Vec<String> = self
            .panes
            .iter()
            .filter(|p| p.session.is_some())
            .map(|p| p.slug.clone())
            .collect();
        for slug in slugs {
            self.push_grid_full(&slug);
        }
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
        // Overview thumbs: always FULL for panes not currently selected on the
        // owning window (avoids damage-base desync into permanent black).
        let use_full = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| {
                let owner = self.owner_of_workspace(&p.workspace)?;
                let conn = self.gui_conns.iter().find(|c| c.id == owner)?;
                Some(conn.overview && conn.selected_workspace.as_deref() != Some(p.workspace.as_str()))
            })
            .unwrap_or(false);
        if use_full {
            self.push_grid_full(slug);
        } else {
            self.push_grid_now(slug);
        }
    }

    fn push_grid_now(&mut self, slug: &str) {
        self.grid_flush_pending.remove(slug);
        self.last_grid_push
            .insert(slug.to_string(), Instant::now());
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
        self.last_grid_push
            .insert(slug.to_string(), Instant::now());
        self.last_grid_cells.remove(slug);
        if let Some(s) = self.session_mut(slug) {
            s.bump_rev();
        }
        if let Some(snap) = self.snapshot_pane(slug) {
            // last_grid_cells empty → broadcast_grid encodes FULL.
            self.broadcast_grid(snap);
        }
    }

    fn flush_workspace_grids(&mut self, workspace: &str) {
        let slugs: Vec<String> = self
            .panes
            .iter()
            .filter(|p| p.workspace == workspace && p.session.is_some())
            .map(|p| p.slug.clone())
            .collect();
        for slug in slugs {
            // FULL only — panes may have redrawn heavily while this workspace
            // was off-screen (Claude TUIs especially). Damage against the
            // last-pushed base leaves blank or corrupt grids until resize.
            self.push_grid_full(&slug);
        }
    }

    pub fn handle_session_event(&mut self, ev: SessionEvent) {
        match &ev {
            SessionEvent::Wakeup { slug } => {
                self.push_grid_throttled(slug);
            }
            SessionEvent::FlushGrid { slug } => {
                // Force-send the coalesced frame (timer already waited).
                self.push_grid_now(slug);
            }
            SessionEvent::ForceFullGrid { slug } => {
                self.push_grid_full(slug);
            }
            SessionEvent::Title { slug, title } => {
                // Title changes are rare — push immediately (also a grid).
                if let Some(s) = self.session_mut(slug) {
                    s.bump_rev();
                }
                self.grid_flush_pending.remove(slug);
                self.last_grid_push
                    .insert(slug.clone(), Instant::now());
                if let Some(snap) = self.snapshot_pane(slug) {
                    let mut s = snap;
                    s.title = title.clone();
                    self.broadcast_grid(s);
                }
            }
            SessionEvent::Exited { slug, code } => {
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
                self.broadcast(GuiEvent::PaneKilled {
                    slug: slug.clone(),
                });
                self.push_state_to_all();
                self.persist();
            }
        }
    }

    pub fn snapshot_pane(&self, slug: &str) -> Option<GridSnapshot> {
        self.panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| p.session.as_ref().map(|s| s.snapshot()))
    }

    /// Legacy whole-state (ctl / first-window fallback). Prefer `state_for_window`.
    pub fn full_state_event(&self) -> GuiEvent {
        let wid = self.gui_conns.first().map(|c| c.id.clone());
        if let Some(id) = wid {
            return self.state_for_window(&id);
        }
        GuiEvent::State {
            panes: self.pane_infos(),
            selected_workspace: self.selected_workspace.clone(),
            focused_pane: self.focused_pane.clone(),
            extra_workspaces: self.extra_workspaces.clone(),
            workspace_order: self.workspace_order.clone(),
            asks: self
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
                .collect(),
            statuses: self
                .statuses
                .iter()
                .map(|(slug, (state, note))| StatusInfo {
                    slug: slug.clone(),
                    state: state.clone(),
                    note: note.clone(),
                    pad_rev: self.pad_revs.get(slug).copied().unwrap_or(0),
                })
                .collect(),
            window_id: None,
            windows: self.window_infos(),
            foreign_workspaces: Vec::new(),
        }
    }

    /// Dense one-row summary for orchestrators (`list` / `brief` / `roster`).
    fn pane_summary_json(&self, p: &EnginePane) -> serde_json::Value {
        let w = p.agency.to_wire();
        let running = if p.kind == "file" {
            true
        } else {
            p.session
                .as_ref()
                .map(|s| s.is_running())
                .unwrap_or(false)
                && !p.agency.exited
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
            "command": p.command,
            "cwd": p.cwd,
            "tiled": p.tiled,
            "running": running,
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

    fn pane_infos(&self) -> Vec<PaneInfo> {
        self.panes
            .iter()
            .map(|p| {
                let running = if p.kind == "file" {
                    true
                } else {
                    p.session
                        .as_ref()
                        .map(|s| s.is_running())
                        .unwrap_or(false)
                        && !p.agency.exited
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

    fn broadcast_agency(&mut self, slug: &str) {
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
        match req {
            GuiRequest::Attach {
                selected_workspace,
                focused_pane,
                empty,
            } => {
                // Claim policy:
                //   • sole window → collect ALL workspaces (post last-close reopen)
                //   • empty=true while peers exist → start blank
                //   • otherwise → claim unowned orphans only
                let sole = self.gui_conns.len() <= 1;
                if sole {
                    // First / only window: vacuum every known circle + any stale map keys.
                    for ws in self.all_workspace_names() {
                        self.workspace_window
                            .insert(ws, window_id.to_string());
                    }
                    // Remap anything still pointing at a dead window id.
                    let dead: Vec<String> = self
                        .workspace_window
                        .iter()
                        .filter(|(_, w)| *w != window_id)
                        .map(|(ws, _)| ws.clone())
                        .collect();
                    for ws in dead {
                        self.workspace_window.insert(ws, window_id.to_string());
                    }
                } else if !empty {
                    for ws in self.all_workspace_names() {
                        self.workspace_window
                            .entry(ws)
                            .or_insert_with(|| window_id.to_string());
                    }
                }
                // Per-window focus — empty windows stay unselected.
                let default_sel = self.workspaces_for_window(window_id).first().cloned();
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                    if empty && !sole {
                        c.selected_workspace = None;
                        c.focused_pane = None;
                    } else if let Some(w) = selected_workspace.clone() {
                        if self.workspace_window.get(&w).map(|s| s.as_str()) == Some(window_id)
                            || sole
                        {
                            c.selected_workspace = Some(w);
                        } else if c.selected_workspace.is_none() {
                            c.selected_workspace = default_sel.clone();
                        }
                    } else if c.selected_workspace.is_none() {
                        c.selected_workspace = default_sel;
                    }
                    if let Some(p) = focused_pane.clone() {
                        c.focused_pane = Some(p);
                    }
                }
                // Global "human" focus tracks non-empty attaches only.
                if !empty || sole {
                    if selected_workspace.is_some() {
                        self.selected_workspace = selected_workspace;
                    } else if sole {
                        // Reopen after last-close: pick first owned circle.
                        self.selected_workspace = self
                            .workspaces_for_window(window_id)
                            .first()
                            .cloned();
                        if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                            if c.selected_workspace.is_none() {
                                c.selected_workspace = self.selected_workspace.clone();
                            }
                        }
                    }
                    if focused_pane.is_some() {
                        self.focused_pane = focused_pane;
                    }
                }
                // GUI reconnect has no prior base — force FULL frames so we
                // never send DAMAGE against a stale/missing GUI snapshot.
                self.last_grid_cells.clear();
                // Kick PTYs owned by this window so empty post-handoff Terms repaint.
                let slugs: Vec<String> = self
                    .panes
                    .iter()
                    .filter(|p| p.session.is_some())
                    .filter(|p| self.owner_of_workspace(&p.workspace) == Some(window_id))
                    .map(|p| p.slug.clone())
                    .collect();
                for slug in &slugs {
                    if let Some(s) = self.session_mut(slug) {
                        s.kick_redraw();
                    }
                }
                let state = self.state_for_window(window_id);
                // Also refresh other windows' foreign-workspace lists.
                self.push_state_to_all();
                for slug in &slugs {
                    self.push_grid_full(slug);
                }
                let tx = self.event_tx.clone();
                let delayed = slugs.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(150));
                    for slug in delayed {
                        let _ = tx.send(SessionEvent::ForceFullGrid { slug });
                    }
                });
                Some(state)
            }
            GuiRequest::Input { pane, bytes_b64 } => {
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
                }
                None
            }
            GuiRequest::Resize { pane, cols, rows } => {
                if let Some(s) = self.session_mut(&pane) {
                    s.resize(cols, rows);
                    s.bump_rev();
                }
                // Immediate FULL grid after resize — don't wait for PTY wakeup.
                // Size changes invalidate damage bases; without this, a
                // workspace switch that also reflows tiles can leave a blank
                // pane until the human resizes the window.
                self.last_grid_cells.remove(&pane);
                self.last_grid_push
                    .insert(pane.clone(), Instant::now());
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
                None
            }
            GuiRequest::GhostAccept { pane } => {
                let ghost = self
                    .session_mut(&pane)
                    .and_then(|s| s.ghost.lock().unwrap().take());
                if let Some(g) = ghost {
                    let from = g.from.clone();
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
                    events::log("human", None, Some(&pane), "propose_rejected", "rejected".into());
                }
                self.broadcast(GuiEvent::Ghost {
                    pane,
                    ghost: None,
                });
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
                let ws = workspace
                    .clone()
                    .unwrap_or_else(|| {
                        self.gui_conns
                            .iter()
                            .find(|c| c.id == window_id)
                            .and_then(|c| c.selected_workspace.clone())
                            .unwrap_or_else(|| "main".into())
                    });
                self.ensure_workspace_owned(&ws, window_id);
                match self.spawn(SpawnSpec {
                    name,
                    cwd,
                    command,
                    workspace: Some(ws),
                    tiled,
                    resume: false,
                    file,
                }) {
                    Ok(slug) => {
                        self.persist();
                        let info = self
                            .pane_infos()
                            .into_iter()
                            .find(|p| p.slug == slug)
                            .unwrap();
                        if let Some(owner) = self.owner_of_pane(&slug).map(|s| s.to_string()) {
                            self.send_to(
                                &owner,
                                GuiEvent::PaneSpawned {
                                    pane: info.clone(),
                                },
                            );
                        }
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
                self.ensure_workspace_owned(&workspace, window_id);
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
                let new = slugify(&new);
                for p in &mut self.panes {
                    if p.workspace == old {
                        p.workspace = new.clone();
                    }
                }
                for w in &mut self.extra_workspaces {
                    if *w == old {
                        *w = new.clone();
                    }
                }
                for w in &mut self.workspace_order {
                    if *w == old {
                        *w = new.clone();
                    }
                }
                if let Some(owner) = self.workspace_window.remove(&old) {
                    self.workspace_window.insert(new.clone(), owner);
                }
                if self.selected_workspace.as_deref() == Some(old.as_str()) {
                    self.selected_workspace = Some(new.clone());
                }
                for c in &mut self.gui_conns {
                    if c.selected_workspace.as_deref() == Some(old.as_str()) {
                        c.selected_workspace = Some(new.clone());
                    }
                }
                self.persist();
                self.push_state_to_all();
                None
            }
            GuiRequest::CreateWorkspace { name } => {
                let name = slugify(&name);
                if !self.extra_workspaces.contains(&name)
                    && !self.panes.iter().any(|p| p.workspace == name)
                {
                    self.extra_workspaces.push(name.clone());
                }
                if !self.workspace_order.iter().any(|w| w == &name) {
                    self.workspace_order.push(name.clone());
                }
                self.workspace_window
                    .insert(name.clone(), window_id.to_string());
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
                self.workspace_order.retain(|w| w != &workspace);
                self.workspace_window.remove(&workspace);
                if self.selected_workspace.as_deref() == Some(workspace.as_str()) {
                    self.selected_workspace = self.panes.first().map(|p| p.workspace.clone());
                }
                for c in &mut self.gui_conns {
                    if c.selected_workspace.as_deref() == Some(workspace.as_str()) {
                        c.selected_workspace = None;
                    }
                }
                self.persist();
                self.push_state_to_all();
                None
            }
            GuiRequest::ForkWorkspace { workspace, name } => {
                match self.fork_workspace(&workspace, name) {
                    Ok(new_ws) => {
                        self.workspace_window
                            .insert(new_ws.clone(), window_id.to_string());
                        self.persist();
                        self.push_state_to_all();
                        Some(GuiEvent::Ack {
                            ok: true,
                            data: Some(json!({"workspace": new_ws})),
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
            GuiRequest::SetFocus { pane, workspace } => {
                let mut workspace_changed = false;
                let mut flush_ws: Option<String> = None;
                let mut flush_pane: Option<String> = None;
                if let Some(w) = workspace.as_ref() {
                    self.ensure_workspace_owned(w, window_id);
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
                        self.flush_workspace_grids(&w);
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
                    // FULL flush for this window's workspaces only.
                    let owned = self.workspaces_for_window(window_id);
                    let slugs: Vec<String> = self
                        .panes
                        .iter()
                        .filter(|p| owned.iter().any(|w| w == &p.workspace) && p.session.is_some())
                        .map(|p| p.slug.clone())
                        .collect();
                    for slug in slugs {
                        self.push_grid_full(&slug);
                    }
                }
                None
            }
            GuiRequest::RefreshGrid { pane } => {
                self.push_grid_full(&pane);
                None
            }
            GuiRequest::TransferWorkspace {
                workspace,
                to_window,
            } => {
                if !self.gui_conns.iter().any(|c| c.id == to_window) {
                    return Some(GuiEvent::Ack {
                        ok: false,
                        data: None,
                        error: Some(format!("unknown window {to_window}")),
                    });
                }
                // Owner may push; destination may pull; unowned is free for all.
                let owner = self.owner_of_workspace(&workspace).map(|s| s.to_string());
                let allowed = match &owner {
                    None => true,
                    Some(o) => o == window_id || window_id == to_window,
                };
                if !allowed {
                    return Some(GuiEvent::Ack {
                        ok: false,
                        data: None,
                        error: Some("workspace not owned by this window".into()),
                    });
                }
                let from = owner.clone().unwrap_or_else(|| window_id.to_string());
                self.workspace_window
                    .insert(workspace.clone(), to_window.clone());
                // Clear selection on source if it pointed here.
                let next_sel = self
                    .workspaces_for_window(&from)
                    .into_iter()
                    .find(|w| w != &workspace);
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == from) {
                    if c.selected_workspace.as_deref() == Some(workspace.as_str()) {
                        c.selected_workspace = next_sel;
                    }
                }
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == to_window) {
                    c.selected_workspace = Some(workspace.clone());
                }
                // State first so the destination GUI mounts RemoteTerms.
                // Grids must follow *after* that (immediate flush races empty
                // entities and gets dropped). Destination also requests refresh
                // on State for empty snaps — delayed FULL is belt-and-suspenders.
                self.push_state_to_all();
                let slugs: Vec<String> = self
                    .panes
                    .iter()
                    .filter(|p| p.workspace == workspace && p.session.is_some())
                    .map(|p| p.slug.clone())
                    .collect();
                let tx = self.event_tx.clone();
                let slugs_d = slugs.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(100));
                    for slug in &slugs_d {
                        let _ = tx.send(SessionEvent::ForceFullGrid {
                            slug: slug.clone(),
                        });
                    }
                    thread::sleep(Duration::from_millis(250));
                    for slug in slugs_d {
                        let _ = tx.send(SessionEvent::ForceFullGrid { slug });
                    }
                });
                Some(GuiEvent::Ack {
                    ok: true,
                    data: Some(json!({"workspace": workspace, "to_window": to_window})),
                    error: None,
                })
            }
            GuiRequest::Bye => {
                // Window closing — reassign workspaces immediately (don't wait
                // for socket EOF). serve_gui will also unregister on exit; that
                // is a no-op if already gone.
                self.unregister_gui(window_id);
                None
            }
            GuiRequest::CollectAll => {
                for ws in self.all_workspace_names() {
                    self.workspace_window.insert(ws, window_id.to_string());
                }
                let pick = self.workspaces_for_window(window_id).first().cloned();
                if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == window_id) {
                    if c.selected_workspace.is_none() {
                        c.selected_workspace = pick;
                    }
                }
                // Other windows go empty.
                let others: Vec<String> = self
                    .gui_conns
                    .iter()
                    .filter(|c| c.id != window_id)
                    .map(|c| c.id.clone())
                    .collect();
                for oid in others {
                    if let Some(c) = self.gui_conns.iter_mut().find(|c| c.id == oid) {
                        c.selected_workspace = None;
                        c.focused_pane = None;
                        c.overview = false;
                    }
                }
                self.push_state_to_all();
                let slugs: Vec<String> = self
                    .panes
                    .iter()
                    .filter(|p| p.session.is_some())
                    .map(|p| p.slug.clone())
                    .collect();
                for slug in slugs {
                    self.push_grid_full(&slug);
                }
                Some(GuiEvent::Ack {
                    ok: true,
                    data: Some(json!({"window": window_id})),
                    error: None,
                })
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
            GuiRequest::Ping => Some(GuiEvent::Pong),
        }
    }

    fn session_mut(&mut self, slug: &str) -> Option<&mut PtySession> {
        self.panes
            .iter_mut()
            .find(|p| p.slug == slug)
            .and_then(|p| p.session.as_mut())
    }

    pub fn spawn(&mut self, spec: SpawnSpec) -> Result<String> {
        let name = if spec.name.trim().is_empty() {
            "session".into()
        } else {
            spec.name.trim().to_string()
        };
        let taken: Vec<&str> = self.panes.iter().map(|p| p.slug.as_str()).collect();
        let slug = unique_slug(&name, &taken);
        let workspace = spec
            .workspace
            .filter(|w| !w.trim().is_empty())
            .map(|w| slugify(&w))
            .unwrap_or_else(|| {
                self.selected_workspace
                    .clone()
                    .unwrap_or_else(|| DEFAULT_WORKSPACE.into())
            });
        // New / unlisted workspace names land at the bottom of the sidebar,
        // never alphabetically at the top.
        if !self.workspace_order.iter().any(|w| w == &workspace) {
            self.workspace_order.push(workspace.clone());
        }
        let cwd_raw = spec.cwd.unwrap_or_else(|| "~".into());
        let scratch_path = self.store.path_for(&slug);

        // Insert after the last pane of this workspace so the sidebar/tiles
        // show newest at the bottom of the group (not global-list quirks).
        let insert_at = self
            .panes
            .iter()
            .rposition(|p| p.workspace == workspace)
            .map(|i| i + 1)
            .unwrap_or(self.panes.len());

        if let Some(file) = spec.file {
            let path = PathBuf::from(shellexpand::tilde(&file).into_owned());
            self.panes.insert(
                insert_at,
                EnginePane {
                    kind: "file".into(),
                    name,
                    slug: slug.clone(),
                    workspace,
                    cwd: cwd_raw,
                    command: path.to_string_lossy().to_string(),
                    tiled: spec.tiled,
                    resume_on_restore: false,
                    scratch_path,
                    file: Some(path.to_string_lossy().to_string()),
                    session: None,
                    agency: crate::agency::Agency::default(),
                },
            );
            events::log("daemon", None, Some(&slug), "pane_spawned", "file pane".into());
            return Ok(slug);
        }

        let explicit = spec.command.filter(|c| !c.trim().is_empty());
        let mut command = match &explicit {
            Some(c) => c.clone(),
            None => {
                let rc = shell_rc_path();
                if rc.is_file() {
                    format!("bash --init-file {}", rc.to_string_lossy())
                } else {
                    DEFAULT_COMMAND.into()
                }
            }
        };
        if spec.resume && command.starts_with("claude") && !command.contains("--continue") {
            command = format!("{command} --continue");
        }

        let session =
            self.spawn_terminal_session(&slug, &command, &cwd_raw, &workspace, false)?;

        self.panes.insert(
            insert_at,
            EnginePane {
                kind: "terminal".into(),
                name,
                slug: slug.clone(),
                workspace: workspace.clone(),
                cwd: cwd_raw,
                command: explicit.unwrap_or_else(|| DEFAULT_COMMAND.into()),
                tiled: spec.tiled,
                resume_on_restore: spec.resume,
                scratch_path,
                file: None,
                session: Some(session),
                agency: crate::agency::Agency::default(),
            },
        );
        events::log(
            "daemon",
            Some(&workspace),
            Some(&slug),
            "pane_spawned",
            "terminal pane".into(),
        );
        Ok(slug)
    }

    fn spawn_from_persisted(&mut self, p: &PersistedPane) -> Result<()> {
        // Spawn with the persisted name; if slug collides, unique_slug suffixes.
        // Prefer exact slug restore when free.
        let taken: Vec<&str> = self.panes.iter().map(|x| x.slug.as_str()).collect();
        let want_slug = if taken.contains(&p.slug.as_str()) {
            unique_slug(&p.name, &taken)
        } else {
            p.slug.clone()
        };

        if p.kind == "file" {
            let path = PathBuf::from(shellexpand::tilde(&p.command).into_owned());
            self.panes.push(EnginePane {
                kind: "file".into(),
                name: p.name.clone(),
                slug: want_slug,
                workspace: p.workspace.clone(),
                cwd: p.cwd.clone(),
                command: p.command.clone(),
                tiled: p.tiled,
                resume_on_restore: false,
                scratch_path: self.store.path_for(&p.slug),
                file: Some(path.to_string_lossy().to_string()),
                session: None,
                agency: crate::agency::Agency::default(),
            });
            return Ok(());
        }

        let mut command = p.command.clone();
        if p.resume_on_restore && command.starts_with("claude") && !command.contains("--continue")
        {
            command = format!("{command} --continue");
        }
        if command == DEFAULT_COMMAND || command.starts_with("bash") {
            let rc = shell_rc_path();
            if rc.is_file() && !command.contains("--init-file") {
                command = format!("bash --init-file {}", rc.to_string_lossy());
            }
        }

        let session = self.spawn_terminal_session(
            &want_slug,
            &command,
            &p.cwd,
            &p.workspace,
            p.resume_on_restore,
        )?;
        self.panes.push(EnginePane {
            kind: "terminal".into(),
            name: p.name.clone(),
            slug: want_slug,
            workspace: p.workspace.clone(),
            cwd: p.cwd.clone(),
            command: p.command.clone(),
            tiled: p.tiled,
            resume_on_restore: p.resume_on_restore,
            scratch_path: self.store.path_for(&p.slug),
            file: None,
            session: Some(session),
            agency: crate::agency::Agency::default(),
        });
        Ok(())
    }

    fn spawn_terminal_session(
        &self,
        slug: &str,
        command: &str,
        cwd_raw: &str,
        workspace: &str,
        _resume: bool,
    ) -> Result<PtySession> {
        let cwd = PathBuf::from(shellexpand::tilde(cwd_raw).into_owned());
        let scratch_path = self.store.path_for(slug);
        let mut env = HashMap::new();
        env.insert("SEANCE_SESSION".into(), slug.to_string());
        env.insert("SEANCE_WORKSPACE".into(), workspace.to_string());
        env.insert(
            "SEANCE_SCRATCHPAD".into(),
            scratch_path.to_string_lossy().to_string(),
        );
        env.insert(
            "SEANCE_SOCKET".into(),
            crate::control::socket_path().to_string_lossy().to_string(),
        );
        PtySession::spawn(
            slug.to_string(),
            SpawnConfig {
                command: command.to_string(),
                cwd,
                env,
                cols: 100,
                rows: 30,
            },
            self.event_tx.clone(),
        )
    }

    pub fn kill_pane(&mut self, slug: &str) {
        if let Some(idx) = self.panes.iter().position(|p| p.slug == slug) {
            let mut pane = self.panes.remove(idx);
            if let Some(s) = pane.session.take() {
                s.shutdown();
            }
            self.cmd_log.remove_pane(slug);
            self.statuses.remove(slug);
            if self.focused_pane.as_deref() == Some(slug) {
                self.focused_pane = self.panes.first().map(|p| p.slug.clone());
            }
            events::log("daemon", None, Some(slug), "pane_killed", "killed".into());
        }
    }

    fn fork_workspace(&mut self, src: &str, name: Option<String>) -> Result<String> {
        let sources: Vec<_> = self
            .panes
            .iter()
            .filter(|p| p.workspace == src)
            .map(|p| {
                (
                    p.name.clone(),
                    p.cwd.clone(),
                    p.command.clone(),
                    p.kind.clone(),
                    p.file.clone(),
                    p.tiled,
                    p.scratch_path.clone(),
                )
            })
            .collect();
        if sources.is_empty() {
            anyhow::bail!("workspace '{src}' has no panes");
        }
        let base = name.unwrap_or_else(|| format!("{src}-fork"));
        let mut new_ws = slugify(&base);
        let mut n = 2;
        while self.panes.iter().any(|p| p.workspace == new_ws)
            || self.extra_workspaces.contains(&new_ws)
        {
            new_ws = format!("{}-{n}", slugify(&base));
            n += 1;
        }
        self.extra_workspaces.push(new_ws.clone());
        if !self.workspace_order.iter().any(|w| w == &new_ws) {
            self.workspace_order.push(new_ws.clone());
        }
        for (name, cwd, command, kind, file, tiled, old_scratch) in sources {
            let slug = self.spawn(SpawnSpec {
                name,
                cwd: Some(cwd),
                command: Some(command),
                workspace: Some(new_ws.clone()),
                tiled,
                resume: false,
                file: if kind == "file" { file } else { None },
            })?;
            let new_path = self.store.path_for(&slug);
            let _ = std::fs::copy(&old_scratch, &new_path);
        }
        self.selected_workspace = Some(new_ws.clone());
        Ok(new_ws)
    }

    /// Move `slug` into `workspace`, inserting immediately before `before`
    /// (another slug) or appending when `before` is None / missing. Pane-list
    /// order is the persistence key for sidebar + tile layout.
    pub fn reorder_pane(&mut self, slug: &str, workspace: &str, before: Option<&str>) {
        if Some(slug) == before {
            return;
        }
        let Some(from_idx) = self.panes.iter().position(|p| p.slug == slug) else {
            return;
        };
        let mut pane = self.panes.remove(from_idx);
        pane.workspace = slugify(workspace);
        let insert_at = before
            .and_then(|b| self.panes.iter().position(|p| p.slug == b))
            .unwrap_or(self.panes.len());
        events::log(
            "human",
            Some(workspace),
            Some(slug),
            "pane_moved",
            format!(
                "moved '{}' into {} (reorder{})",
                pane.name,
                pane.workspace,
                before.map(|b| format!(" before {b}")).unwrap_or_default()
            ),
        );
        self.panes.insert(insert_at, pane);
        self.selected_workspace = Some(slugify(workspace));
    }

    /// Place workspace `moved` immediately before `before` in the sidebar
    /// order. Builds the full display order (explicit + any extras) so a
    /// partial `workspace_order` still ends up consistent.
    pub fn reorder_workspace(&mut self, moved: &str, before: &str) {
        if moved == before {
            return;
        }
        // Full ordered list: preferred order first, then any workspaces not
        // yet listed (extras then pane order — not alphabetical).
        let mut order = self.workspace_order.clone();
        let mut seen: std::collections::HashSet<String> = order.iter().cloned().collect();
        for w in self
            .extra_workspaces
            .iter()
            .chain(self.panes.iter().map(|p| &p.workspace))
        {
            if seen.insert(w.clone()) {
                order.push(w.clone());
            }
        }
        order.retain(|w| w != moved);
        let idx = order.iter().position(|w| w == before).unwrap_or(order.len());
        order.insert(idx, moved.to_string());
        self.workspace_order = order;
        events::log(
            "human",
            Some(moved),
            None,
            "workspace_reordered",
            format!("workspace '{moved}' before '{before}'"),
        );
    }

    pub fn persist(&self) {
        let state = AppState {
            panes: self
                .panes
                .iter()
                .map(|p| {
                    let snap = p.agency.to_snap();
                    let (status, status_note) = self
                        .statuses
                        .get(&p.slug)
                        .map(|(s, n)| (Some(s.clone()), n.clone()))
                        .unwrap_or((None, None));
                    PersistedPane {
                        kind: p.kind.clone(),
                        name: p.name.clone(),
                        slug: p.slug.clone(),
                        cwd: p.cwd.clone(),
                        command: p.command.clone(),
                        tiled: p.tiled,
                        resume_on_restore: p.resume_on_restore,
                        workspace: p.workspace.clone(),
                        status,
                        status_note,
                        pad_rev: self.pad_revs.get(&p.slug).copied().unwrap_or(0),
                        owner: Some(snap.owner),
                        drive_mode: Some(snap.drive_mode),
                        exited: snap.exited,
                        exit_code: snap.exit_code,
                        inject_pad_rev: self.inject_baselines.get(&p.slug).map(|(r, _)| *r),
                        inject_pad_bytes: self.inject_baselines.get(&p.slug).map(|(_, b)| *b),
                    }
                })
                .collect(),
            sidebar_width: None,
            drawer_width: None,
            drawer_open: false,
            active_slug: self.focused_pane.clone(),
            selected_workspace: self.selected_workspace.clone(),
            extra_workspaces: self.extra_workspaces.clone(),
            workspace_order: self.workspace_order.clone(),
            window_size: None,
            // Keep open + recently finished tasks (cap body size already at create).
            tasks: self.tasks.values().cloned().collect(),
            task_counter: self.task_counter,
            active_tasks: self
                .active_tasks
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            // GUI-local for now; default keeps layout sane on cold restore.
            split_ratio: 0.5,
            pane_weights: vec![],
            cmd_log: self.cmd_log.clone(),
        };
        let _ = state.save();
    }

    pub fn prepare_upgrade(&mut self) -> Result<(HandoffBundle, Vec<std::os::fd::OwnedFd>)> {
        super::set_upgrade_in_progress(true);
        let mut fds = Vec::new();
        let mut panes = Vec::new();
        for p in &self.panes {
            // Don't hand off exited tombstones — they auto-close now.
            if p.agency.exited {
                continue;
            }
            let mut hp = HandoffPane {
                name: p.name.clone(),
                slug: p.slug.clone(),
                workspace: p.workspace.clone(),
                cwd: p.cwd.clone(),
                command: p.command.clone(),
                tiled: p.tiled,
                resume_on_restore: p.resume_on_restore,
                kind: p.kind.clone(),
                file: p.file.clone(),
                child_pid: None,
                cols: 100,
                rows: 30,
                fd_index: None,
                title: None,
                text_snapshot: String::new(),
                ghost: None,
                agency: Some(p.agency.to_snap()),
            };
            if let Some(s) = &p.session {
                let (cols, rows) = s.size();
                hp.cols = cols;
                hp.rows = rows;
                hp.title = s.title();
                hp.text_snapshot = s.screen_text(None);
                hp.ghost = s.ghost.lock().unwrap().clone();
                hp.child_pid = s.child_pid();
                match s.prepare_handoff() {
                    Ok((fd, pid)) => {
                        hp.child_pid = Some(pid);
                        hp.fd_index = Some(fds.len());
                        fds.push(fd);
                    }
                    Err(e) => {
                        eprintln!("[seance] handoff prepare failed for {}: {e:#}", p.slug);
                    }
                }
            }
            panes.push(hp);
        }
        let statuses: Vec<StatusInfo> = self
            .statuses
            .iter()
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
        let pad_revs: Vec<(String, u64)> = self.pad_revs.iter().map(|(k, v)| (k.clone(), *v)).collect();
        let inject_baselines: Vec<InjectBaseline> = self
            .inject_baselines
            .iter()
            .map(|(slug, (rev, bytes))| InjectBaseline {
                slug: slug.clone(),
                pad_rev: *rev,
                pad_bytes: *bytes,
            })
            .collect();
        let tasks: Vec<TaskRecord> = self.tasks.values().cloned().collect();
        let active_tasks: Vec<(String, String)> = self
            .active_tasks
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let bundle = HandoffBundle {
            panes,
            selected_workspace: self.selected_workspace.clone(),
            focused_pane: self.focused_pane.clone(),
            extra_workspaces: self.extra_workspaces.clone(),
            workspace_order: self.workspace_order.clone(),
            proposal_counter: self.proposal_counter,
            ask_counter: self.ask_counter,
            statuses,
            asks,
            pad_revs,
            inject_baselines,
            tasks,
            task_counter: self.task_counter,
            active_tasks,
            cmd_log: self.cmd_log.clone(),
        };
        Ok((bundle, fds))
    }
}

