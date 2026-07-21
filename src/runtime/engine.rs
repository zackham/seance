//! Session engine: panes, control plane, layout state. gpui-free.

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
    /// Broadcast: clones of GUI event senders.
    gui_txs: Vec<Sender<GuiEvent>>,
    /// Per-pane last full-grid push — TUIs with spinners wake the PTY dozens of
    /// times per second; unthrottled snapshots peg the GUI.
    last_grid_push: HashMap<String, Instant>,
    /// FlushGrid already scheduled for this slug (avoid timer storms).
    grid_flush_pending: HashSet<String>,
    /// Last cells we broadcast per pane — enables row-damage frames + skip
    /// when nothing changed.
    last_grid_cells: HashMap<String, LastGridFrame>,
}

/// Cached last broadcast for damage detection (Arc so we don't clone every push).
struct LastGridFrame {
    cols: u16,
    rows: u16,
    cursor_col: u16,
    cursor_row: u16,
    cells: std::sync::Arc<Vec<CellSnap>>,
}

impl Engine {
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
            cmd_log: CommandLog::new(),
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
            gui_txs: Vec::new(),
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
                    exited: p.exited,
                    exit_code: p.exit_code,
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
            cmd_log: CommandLog::new(),
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
            gui_txs: Vec::new(),
            last_grid_push: HashMap::new(),
            grid_flush_pending: HashSet::new(),
            last_grid_cells: HashMap::new(),
        };

        let mut fd_map: HashMap<usize, OwnedFd> =
            adopted.into_iter().map(|(i, o)| (i, o.fd)).collect();

        for hp in bundle.panes {
            let scratch_path = eng.store.path_for(&hp.slug);
            let agency = hp
                .agency
                .as_ref()
                .map(crate::agency::Agency::from_snap)
                .unwrap_or_default();
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
            let session = if let Some(idx) = hp.fd_index {
                if let Some(fd) = fd_map.remove(&idx) {
                    let adopted = AdoptedPty {
                        master_fd: fd,
                        child_pid: hp.child_pid.unwrap_or(0),
                        cols: hp.cols,
                        rows: hp.rows,
                        title: hp.title.clone(),
                        ghost: hp.ghost.clone(),
                    };
                    PtySession::adopt(hp.slug.clone(), adopted, event_tx.clone()).ok()
                } else {
                    None
                }
            } else {
                None
            };
            let session = match session {
                Some(s) => Some(s),
                None => eng
                    .spawn_terminal_session(
                        &hp.slug,
                        &hp.command,
                        &hp.cwd,
                        &hp.workspace,
                        false,
                    )
                    .ok(),
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

    pub fn register_gui(&mut self, tx: Sender<GuiEvent>) {
        self.gui_txs.push(tx);
    }

    pub fn unregister_dead_guis(&mut self) {
        // GuiEvent send fails when disconnected — prune on broadcast.
    }

    pub fn broadcast(&mut self, ev: GuiEvent) {
        self.gui_txs.retain(|tx| tx.send(ev.clone()).is_ok());
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
                        skip = true;
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
                cells: std::sync::Arc::new(snap.cells.clone()),
            },
        );

        if skip {
            return;
        }
        let ev = Self::grid_event(snap, damage.as_deref());
        self.broadcast(ev);
    }

    /// Selected workspace (any pane, focused or not): ~60fps.
    /// Other workspaces: no live push (flushed when that workspace is selected).
    ///
    /// Pre-SCG3 / pre-batched-paint we throttled unfocused neighbors to ~4fps
    /// so spinners couldn't steal the UI thread. That cliff is gone — visible
    /// panes all get full rate; invisible workspaces stay quiet.
    fn grid_interval_for(&self, slug: &str) -> Option<Duration> {
        let ws = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| p.workspace.as_str());
        match (ws, self.selected_workspace.as_deref()) {
            (Some(w), Some(sel)) if w == sel => Some(Duration::from_millis(16)),
            (Some(_), _) => None,
            // No workspace association (edge) — push live.
            _ => Some(Duration::from_millis(16)),
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
        self.push_grid_now(slug);
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

    fn flush_workspace_grids(&mut self, workspace: &str) {
        let slugs: Vec<String> = self
            .panes
            .iter()
            .filter(|p| p.workspace == workspace && p.session.is_some())
            .map(|p| p.slug.clone())
            .collect();
        for slug in slugs {
            self.push_grid_now(&slug);
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
                // Tombstone: keep the pane so the human can read the corpse and
                // decide; do not auto-remove. Explicit `kill` clears it.
                let code = *code;
                if let Some(p) = self.panes.iter_mut().find(|p| p.slug == *slug) {
                    if let Some(s) = p.session.take() {
                        // Process already dead; drop without re-killing if possible.
                        drop(s);
                    }
                    p.agency.mark_exited(code);
                }
                // Turn-end signal: working/blocked → idle on process death so
                // waiters don't hang until timeout on a dead worker.
                let was = self
                    .statuses
                    .get(slug)
                    .map(|(s, _)| s.clone())
                    .unwrap_or_default();
                if was == "working" || was == "planning" || was == "blocked" || was.is_empty() {
                    let note = format!("exited ({code:?})");
                    self.statuses
                        .insert(slug.clone(), ("idle".into(), Some(note.clone())));
                    self.broadcast(GuiEvent::Status {
                        slug: slug.clone(),
                        state: "idle".into(),
                        note: Some(note),
                    });
                }
                if let Some(tid) = self.active_tasks.remove(slug) {
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
                    Some(slug),
                    "pane_exited",
                    format!("process exited ({code:?}) — tombstone retained"),
                );
                self.broadcast(GuiEvent::PaneExited {
                    slug: slug.clone(),
                    exit_code: code,
                });
                if let Some(p) = self.panes.iter().find(|p| p.slug == *slug) {
                    let w = p.agency.to_wire();
                    self.broadcast(GuiEvent::Agency {
                        pane: slug.clone(),
                        owner: w.owner,
                        drive_mode: w.drive_mode,
                        human_idle: w.human_idle,
                        exited: w.exited,
                        exit_code: w.exit_code,
                    });
                }
                self.broadcast(self.full_state_event());
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

    pub fn full_state_event(&self) -> GuiEvent {
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

    pub fn handle_gui(&mut self, req: GuiRequest) -> Option<GuiEvent> {
        match req {
            GuiRequest::Attach { selected_workspace, focused_pane } => {
                if selected_workspace.is_some() {
                    self.selected_workspace = selected_workspace;
                }
                if focused_pane.is_some() {
                    self.focused_pane = focused_pane;
                }
                // GUI reconnect has no prior base — force FULL frames so we
                // never send DAMAGE against a stale/missing GUI snapshot
                // (post-upgrade "damage size mismatch" spam).
                self.last_grid_cells.clear();
                // Push full state + all grids
                let state = self.full_state_event();
                self.broadcast(state.clone());
                let snaps: Vec<_> = self
                    .panes
                    .iter()
                    .filter_map(|p| p.session.as_ref().map(|s| s.snapshot()))
                    .collect();
                for s in snaps {
                    self.broadcast_grid(s);
                }
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
            } => match self.spawn(SpawnSpec {
                name,
                cwd,
                command,
                workspace,
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
                    self.broadcast(GuiEvent::PaneSpawned {
                        pane: info.clone(),
                    });
                    if let Some(snap) = self.snapshot_pane(&slug) {
                        self.broadcast_grid(snap);
                    }
                    // Same as ctl New: full State so any GUI stays in sync.
                    self.broadcast(self.full_state_event());
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
            },
            GuiRequest::Kill { pane } => {
                self.kill_pane(&pane);
                self.broadcast(GuiEvent::PaneKilled { slug: pane });
                self.persist();
                Some(self.full_state_event())
            }
            GuiRequest::SetTiled { pane, tiled } => {
                if let Some(p) = self.panes.iter_mut().find(|p| p.slug == pane) {
                    p.tiled = tiled;
                }
                self.persist();
                Some(self.full_state_event())
            }
            GuiRequest::MovePane {
                pane,
                workspace,
                before,
            } => {
                self.reorder_pane(&pane, &workspace, before.as_deref());
                self.persist();
                Some(self.full_state_event())
            }
            GuiRequest::ReorderWorkspace { moved, before } => {
                self.reorder_workspace(&moved, &before);
                self.persist();
                Some(self.full_state_event())
            }
            GuiRequest::RenamePane { pane, name } => {
                if let Some(p) = self.panes.iter_mut().find(|p| p.slug == pane) {
                    p.name = name;
                }
                self.persist();
                Some(self.full_state_event())
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
                if self.selected_workspace.as_deref() == Some(old.as_str()) {
                    self.selected_workspace = Some(new);
                }
                self.persist();
                Some(self.full_state_event())
            }
            GuiRequest::CreateWorkspace { name } => {
                let name = slugify(&name);
                if !self.extra_workspaces.contains(&name)
                    && !self.panes.iter().any(|p| p.workspace == name)
                {
                    self.extra_workspaces.push(name.clone());
                }
                self.selected_workspace = Some(name);
                self.persist();
                Some(self.full_state_event())
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
                if self.selected_workspace.as_deref() == Some(workspace.as_str()) {
                    self.selected_workspace = self.panes.first().map(|p| p.workspace.clone());
                }
                self.persist();
                Some(self.full_state_event())
            }
            GuiRequest::ForkWorkspace { workspace, name } => {
                match self.fork_workspace(&workspace, name) {
                    Ok(new_ws) => {
                        self.persist();
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
                if let Some(p) = pane {
                    self.focused_pane = Some(p);
                }
                if let Some(w) = workspace {
                    if self.selected_workspace.as_ref() != Some(&w) {
                        workspace_changed = true;
                    }
                    self.selected_workspace = Some(w);
                }
                self.persist();
                // Live grids for other workspaces are dropped — flush when
                // the human actually looks at this circle.
                if workspace_changed {
                    if let Some(w) = self.selected_workspace.clone() {
                        self.flush_workspace_grids(&w);
                    }
                } else if let Some(fp) = self.focused_pane.clone() {
                    self.push_grid_now(&fp);
                }
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
        let cwd_raw = spec.cwd.unwrap_or_else(|| "~".into());
        let scratch_path = self.store.path_for(&slug);

        if let Some(file) = spec.file {
            let path = PathBuf::from(shellexpand::tilde(&file).into_owned());
            self.panes.push(EnginePane {
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
            });
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

        self.panes.push(EnginePane {
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
        });
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
        // yet listed (from panes + extras), alphabetically.
        let mut order = self.workspace_order.clone();
        let mut known: Vec<String> = self
            .panes
            .iter()
            .map(|p| p.workspace.clone())
            .chain(self.extra_workspaces.iter().cloned())
            .collect();
        known.sort();
        known.dedup();
        for w in known {
            if !order.contains(&w) {
                order.push(w);
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
        };
        let _ = state.save();
    }

    pub fn handle_control(&mut self, request: ControlRequest) -> ControlResponse {
        use ControlRequest::*;
        let ok = |data: serde_json::Value| ControlResponse::ok(data);
        let err = |m: String| ControlResponse::err(m);
        let find = |eng: &Engine, key: &str, scope: &Option<String>| -> Result<usize, String> {
            let idx = eng
                .panes
                .iter()
                .position(|p| p.slug == key || p.name == key)
                .ok_or_else(|| format!("no pane '{key}'"))?;
            if let Some(ws) = scope {
                if eng.panes[idx].workspace != *ws {
                    return Err(format!(
                        "pane '{key}' is outside your workspace '{ws}' (use --all to cross)"
                    ));
                }
            }
            Ok(idx)
        };
        let actor = |from: &Option<String>| {
            from.as_ref()
                .map(|f| format!("agent:{f}"))
                .unwrap_or_else(|| "cli".into())
        };

        // Capability check (Watch is handled specially by the daemon).
        if !matches!(request, Watch { .. }) {
            let principal = crate::caps::principal_of(request.from_field());
            let op = crate::caps::op_name(&request);
            let ws = request.workspace_hint();
            if let Err(msg) = self.caps.check(&principal, op, ws) {
                events::log(&principal, ws, None, "cap_denied", format!("{op}: {msg}"));
                return err(msg);
            }
        }

        match request {
            List { scope, .. } => {
                let panes: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|p| scope.as_deref().is_none_or(|ws| p.workspace == ws))
                    .map(|p| self.pane_summary_json(p))
                    .collect();
                ok(json!({
                    "panes": panes,
                    "scope": scope,
                    "event_seq": events::current_seq(),
                    "focused_pane": self.focused_pane,
                    "selected_workspace": self.selected_workspace,
                }))
            }
            New {
                name,
                cwd,
                command,
                workspace,
                file,
                scope,
                from,
            } => {
                let workspace = workspace.or_else(|| scope.clone());
                if let (Some(ws), Some(sc)) = (workspace.as_deref(), scope.as_deref()) {
                    if ws != sc {
                        return err(format!(
                            "scoped to workspace '{sc}' — cannot spawn into '{ws}' (use --all)"
                        ));
                    }
                }
                match self.spawn(SpawnSpec {
                    name,
                    cwd,
                    command,
                    workspace,
                    tiled: true,
                    resume: false,
                    file,
                }) {
                    Ok(slug) => {
                        let (ws, scratch, pname) = {
                            let pane = self.panes.iter().find(|p| p.slug == slug).unwrap();
                            (
                                pane.workspace.clone(),
                                pane.scratch_path.to_string_lossy().to_string(),
                                pane.name.clone(),
                            )
                        };
                        events::log(
                            &actor(&from),
                            Some(&ws),
                            Some(&slug),
                            "ctl_new",
                            format!("spawned '{pname}'"),
                        );
                        self.persist();
                        let info = self.pane_infos().into_iter().find(|p| p.slug == slug).unwrap();
                        // PaneSpawned for snappy add + focus steal; full State
                        // so a GUI that missed the push (or just reconnected)
                        // still reconciles the complete pane list. External
                        // `seance ctl new` used to create panes the daemon
                        // owned but a disconnected GUI never painted.
                        self.broadcast(GuiEvent::PaneSpawned {
                            pane: info.clone(),
                        });
                        if let Some(snap) = self.snapshot_pane(&slug) {
                            self.broadcast_grid(snap);
                        }
                        self.broadcast(self.full_state_event());
                        ok(json!({
                            "slug": slug,
                            "workspace": ws,
                            "scratchpad": scratch,
                        }))
                    }
                    Err(e) => err(e.to_string()),
                }
            }
            Send {
                pane,
                text,
                submit,
                force,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    let ws = self.panes[idx].workspace.clone();
                    let act = actor(&from);
                    let exited = self.panes[idx].agency.exited;
                    if let Err(e) = self.panes[idx].agency.may_inject(&act, force) {
                        events::log(&act, Some(&ws), Some(&slug), "agency.denied", e.clone());
                        return err(e);
                    }
                    if self.panes[idx].session.is_none() {
                        return err(if exited {
                            "pane has exited (tombstone)".into()
                        } else {
                            "not a terminal pane".into()
                        });
                    }
                    self.panes[idx].agency.agent_claim(&act);
                    events::log_ex(
                        &act,
                        Some(&ws),
                        Some(&slug),
                        "ctl_send",
                        format!("sent {} chars", text.len()),
                        events::LogOpts {
                            origin: Some("ctl_send".into()),
                            ..Default::default()
                        },
                    );
                    // Dispatch envelope + working badge + pad baseline (before inject consumes text).
                    let task_id = self.begin_task(&slug, &text);
                    if let Some(session) = self.panes[idx].session.as_ref() {
                        session.set_input_origin(&act);
                        session.scroll_to_bottom();
                        session.inject(text, submit);
                    }
                    let (pad_rev, pad_bytes) = self
                        .inject_baselines
                        .get(&slug)
                        .copied()
                        .unwrap_or((0, 0));
                    self.statuses
                        .insert(slug.clone(), ("working".into(), Some(format!("inject from {act}"))));
                    self.broadcast(GuiEvent::Status {
                        slug: slug.clone(),
                        state: "working".into(),
                        note: Some(format!("inject from {act}")),
                    });
                    events::log(
                        &act,
                        Some(&ws),
                        Some(&slug),
                        "status_set",
                        format!("working: inject from {act} task={task_id}"),
                    );
                    events::log(
                        &act,
                        Some(&ws),
                        Some(&slug),
                        "task_open",
                        task_id.clone(),
                    );
                    self.broadcast_agency(&slug);
                    self.broadcast(GuiEvent::InputOrigin {
                        pane: slug.clone(),
                        origin: act.clone(),
                    });
                    self.broadcast(GuiEvent::Touch {
                        slug: slug.clone(),
                        verb: "⚡ driven".into(),
                        actor: act,
                    });
                    self.persist();
                    ok(json!({
                        "slug": slug,
                        "task_id": task_id,
                        "inject_pad_rev": pad_rev,
                        "inject_pad_bytes": pad_bytes,
                        "status": "working",
                    }))
                }
                Err(e) => err(e),
            },
            SendRaw {
                pane,
                bytes_b64,
                force,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => match base64_decode(&bytes_b64) {
                    Ok(bytes) => {
                        let slug = self.panes[idx].slug.clone();
                        let ws = self.panes[idx].workspace.clone();
                        let act = actor(&from);
                        if let Err(e) = self.panes[idx].agency.may_inject(&act, force) {
                            events::log(&act, Some(&ws), Some(&slug), "agency.denied", e.clone());
                            return err(e);
                        }
                        if self.panes[idx].session.is_none() {
                            return err("not a terminal pane".into());
                        }
                        self.panes[idx].agency.agent_claim(&act);
                        events::log_ex(
                            &act,
                            Some(&ws),
                            Some(&slug),
                            "ctl_send_raw",
                            format!("{} bytes", bytes.len()),
                            events::LogOpts {
                                origin: Some("ctl_send_raw".into()),
                                ..Default::default()
                            },
                        );
                        if let Some(session) = self.panes[idx].session.as_ref() {
                            session.set_input_origin(&act);
                            session.write_bytes(bytes);
                        }
                        self.broadcast_agency(&slug);
                        self.broadcast(GuiEvent::InputOrigin {
                            pane: slug.clone(),
                            origin: act,
                        });
                        ok(serde_json::Value::Null)
                    }
                    Err(e) => err(format!("bad base64: {e}")),
                },
                Err(e) => err(e),
            },
            Read {
                pane,
                lines,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    if from.as_deref() != Some(slug.as_str()) {
                        self.broadcast(GuiEvent::Touch {
                            slug: slug.clone(),
                            verb: "👁 observed".into(),
                            actor: actor(&from),
                        });
                    }
                    let text = if let Some(s) = self.panes[idx].session.as_ref() {
                        s.screen_text(lines)
                    } else if let Some(path) = &self.panes[idx].file {
                        std::fs::read_to_string(path).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    ok(json!({"screen": text}))
                }
                Err(e) => err(e),
            },
            Status { pane, scope, .. } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let p = &self.panes[idx];
                    ok(json!({
                        "kind": p.kind,
                        "name": p.name,
                        "slug": p.slug,
                        "workspace": p.workspace,
                        "command": p.command,
                        "running": p.session.as_ref().map(|s| s.is_running()).unwrap_or(true),
                        "title": p.session.as_ref().and_then(|s| s.title()),
                        "tiled": p.tiled,
                    }))
                }
                Err(e) => err(e),
            },
            Kill {
                pane,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    events::log(
                        &actor(&from),
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "ctl_kill",
                        "killed".into(),
                    );
                    self.kill_pane(&slug);
                    self.broadcast(GuiEvent::PaneKilled { slug });
                    self.persist();
                    ok(serde_json::Value::Null)
                }
                Err(e) => err(e),
            },
            Scratchpad { pane, scope, .. } => match find(self, &pane, &scope) {
                Ok(idx) => ok(json!({
                    "path": self.panes[idx].scratch_path.to_string_lossy(),
                })),
                Err(e) => err(e),
            },
            Timeline {
                since_secs,
                pane,
                actor: act,
                limit,
                scope,
                ..
            } => {
                let since_ms = since_secs
                    .map(|s| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0)
                            .saturating_sub(s * 1000)
                    })
                    .unwrap_or(0);
                let entries = events::read(
                    since_ms,
                    scope.as_deref(),
                    pane.as_deref(),
                    act.as_deref(),
                    limit.unwrap_or(100),
                );
                let rows: Vec<_> = entries
                    .iter()
                    .map(|e| {
                        json!({
                            "time": events::fmt_time(e.ts),
                            "actor": e.actor,
                            "pane": e.pane,
                            "workspace": e.workspace,
                            "kind": e.kind,
                            "detail": e.detail,
                        })
                    })
                    .collect();
                ok(json!({"events": rows}))
            }
            StatusSet {
                state,
                note,
                pane,
                scope,
                from,
            } => {
                let target = pane.or_else(|| from.clone());
                let Some(target) = target else {
                    return err("status-set: no pane".into());
                };
                if let Err(e) = validate_status(&state) {
                    return err(e);
                }
                match find(self, &target, &scope) {
                    Ok(idx) => {
                        let slug = self.panes[idx].slug.clone();
                        let ws = self.panes[idx].workspace.clone();
                        let act = actor(&from);
                        if let Err(e) = assert_self_or_cross(&slug, &from, &act) {
                            return err(e);
                        }
                        // Evidence gate: bare status-set done cannot lie without pad growth.
                        if state == "done" {
                            if let Err(e) = self.require_since_inject_evidence(&slug) {
                                return err(format!(
                                    "{e} — use `finish` with a body, or grow the pad first"
                                ));
                            }
                        }
                        self.statuses
                            .insert(slug.clone(), (state.clone(), note.clone()));
                        if state == "done" {
                            self.complete_active_task(&slug, None);
                        }
                        let detail = match &note {
                            Some(n) => format!("{state}: {n}"),
                            None => state.clone(),
                        };
                        events::log(&act, Some(&ws), Some(&slug), "status_set", detail);
                        self.broadcast(GuiEvent::Status {
                            slug: slug.clone(),
                            state: state.clone(),
                            note: note.clone(),
                        });
                        self.persist();
                        ok(json!({
                            "slug": slug,
                            "status": state,
                            "pad_rev": self.pad_revs.get(&slug).copied().unwrap_or(0),
                            "task_id": self.active_tasks.get(&slug),
                        }))
                    }
                    Err(e) => err(e),
                }
            }
            Ask {
                question,
                choices,
                scope,
                from,
            } => {
                self.ask_counter += 1;
                let id = format!("ask-{}", self.ask_counter);
                let from_label = from.clone().unwrap_or_else(|| "cli".into());
                let ask = PendingAsk {
                    id: id.clone(),
                    from: from_label.clone(),
                    workspace: scope.clone(),
                    question: question.clone(),
                    choices: choices.clone().unwrap_or_default(),
                    answer: None,
                };
                self.asks.push(ask);
                self.broadcast(GuiEvent::Ask {
                    ask: AskInfo {
                        id: id.clone(),
                        from: from_label,
                        workspace: scope,
                        question,
                        choices: choices.unwrap_or_default(),
                        answer: None,
                    },
                });
                ok(json!({"id": id}))
            }
            Propose {
                pane,
                text,
                reason,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    if self.panes[idx].session.is_none() {
                        return err("not a terminal pane".into());
                    }
                    self.proposal_counter += 1;
                    let id = format!("prop-{}", self.proposal_counter);
                    let slug = self.panes[idx].slug.clone();
                    let from_label = from.clone().unwrap_or_else(|| "cli".into());
                    let ghost = GhostSnap {
                        id: id.clone(),
                        text: text.clone(),
                        from: from_label,
                        reason,
                    };
                    *self.panes[idx]
                        .session
                        .as_ref()
                        .unwrap()
                        .ghost
                        .lock()
                        .unwrap() = Some(ghost.clone());
                    self.proposals.insert(id.clone(), (slug.clone(), None));
                    self.broadcast(GuiEvent::Ghost {
                        pane: slug,
                        ghost: Some(ghost),
                    });
                    ok(json!({"id": id}))
                }
                Err(e) => err(e),
            },
            ProposeResult { id, .. } => match self.proposals.get(&id) {
                Some((_, Some(outcome))) => {
                    let outcome = outcome.clone();
                    self.proposals.remove(&id);
                    ok(json!({"resolved": true, "outcome": outcome}))
                }
                Some((_, None)) => ok(json!({"resolved": false})),
                None => err(format!("no proposal '{id}'")),
            },
            Human { scope, .. } => {
                let panes: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|p| scope.as_deref().is_none_or(|ws| p.workspace == ws))
                    .map(|p| {
                        let w = p.agency.to_wire();
                        json!({
                            "slug": p.slug,
                            "workspace": p.workspace,
                            "owner": w.owner,
                            "drive_mode": w.drive_mode,
                            "human_idle": w.human_idle,
                            "exited": w.exited,
                            "exit_code": w.exit_code,
                            "running": p.session.as_ref().map(|s| s.is_running()).unwrap_or(false)
                                && !p.agency.exited,
                            "title": p.session.as_ref().and_then(|s| s.title()),
                        })
                    })
                    .collect();
                ok(json!({
                    "focused_pane": self.focused_pane,
                    "selected_workspace": self.selected_workspace,
                    "pending_asks": self.asks.iter().filter(|a| a.answer.is_none()).count(),
                    "panes": panes,
                }))
            }
            WorkspaceFork {
                workspace,
                name,
                scope,
                from,
            } => {
                let src = workspace
                    .or_else(|| scope.clone())
                    .or_else(|| self.selected_workspace.clone());
                let Some(src) = src else {
                    return err("fork: no source workspace".into());
                };
                match self.fork_workspace(&src, name) {
                    Ok(n) => {
                        events::log(&actor(&from), Some(&n), None, "workspace_forked", n.clone());
                        self.persist();
                        ok(json!({"workspace": n}))
                    }
                    Err(e) => err(e.to_string()),
                }
            }
            CmdBegin {
                command, cwd, from, ..
            } => {
                let Some(pane) = from else {
                    return err("cmd-begin: must run inside a pane".into());
                };
                let cwd = cwd.unwrap_or_default();
                let seq = self.cmd_log.begin(&pane, command.clone(), cwd.clone());
                let span = format!("cmd:{pane}:{seq}");
                events::log_ex(
                    &format!("agent:{pane}"),
                    None,
                    Some(&pane),
                    "cmd_start",
                    format!("$ {command}"),
                    events::LogOpts {
                        span: Some(span),
                        origin: Some("shell_hook".into()),
                        ..Default::default()
                    },
                );
                ok(serde_json::Value::Null)
            }
            CmdEnd { exit, from, .. } => {
                let Some(pane) = from else {
                    return err("cmd-end: must run inside a pane".into());
                };
                self.cmd_log.end(&pane, exit);
                let (detail, span) = match self.cmd_log.last(&pane, false) {
                    Some(rec) => (
                        format!(
                            "exit {exit} · {}ms · $ {}",
                            rec.duration_ms().unwrap_or(0),
                            rec.command
                        ),
                        Some(format!("cmd:{pane}:{}", rec.seq)),
                    ),
                    None => (format!("exit {exit}"), None),
                };
                events::log_ex(
                    &format!("agent:{pane}"),
                    None,
                    Some(&pane),
                    "cmd_end",
                    detail,
                    events::LogOpts {
                        span,
                        origin: Some("shell_hook".into()),
                        ..Default::default()
                    },
                );
                ok(serde_json::Value::Null)
            }
            Commands {
                pane, limit, scope, ..
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    let records = self.cmd_log.list(&slug, limit.unwrap_or(50));
                    ok(serde_json::to_value(records).unwrap_or(serde_json::Value::Null))
                }
                Err(e) => err(e),
            },
            LastCommand {
                pane,
                failed_only,
                scope,
                ..
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    match self.cmd_log.last(&slug, failed_only) {
                        Some(rec) => ok(serde_json::to_value(rec).unwrap_or(serde_json::Value::Null)),
                        None => err("no matching command".into()),
                    }
                }
                Err(e) => err(e),
            },
            AskResult { id, .. } => match self.asks.iter().position(|a| a.id == id) {
                Some(idx) => {
                    if let Some(answer) = self.asks[idx].answer.clone() {
                        self.asks.remove(idx);
                        ok(json!({"answered": true, "answer": answer}))
                    } else {
                        ok(json!({"answered": false}))
                    }
                }
                None => err(format!("no ask '{id}'")),
            },
            // Watch is handled by the daemon connection loop (streaming).
            Watch { .. } => ok(json!({
                "watching": true,
                "cursor": events::current_seq(),
                "note": "daemon streams events after this ack",
            })),
            Whoami { scope, from } => {
                let principal = actor(&from);
                let policy = self.caps.policy_for(scope.as_deref());
                let grants: Vec<_> = self
                    .caps
                    .grants
                    .iter()
                    .filter(|g| g.principal == principal || g.principal == "*")
                    .cloned()
                    .collect();
                let session = from.clone();
                let (task_id, task_status, task_chars) = session
                    .as_ref()
                    .and_then(|slug| {
                        let tid = self.active_tasks.get(slug).cloned().or_else(|| {
                            self.tasks
                                .values()
                                .filter(|t| t.pane == *slug)
                                .max_by_key(|t| t.created_ms)
                                .map(|t| t.id.clone())
                        })?;
                        let t = self.tasks.get(&tid)?;
                        Some((Some(tid), Some(t.status.clone()), Some(t.body.len())))
                    })
                    .unwrap_or((None, None, None));
                ok(json!({
                    "principal": principal,
                    "session": session,
                    "workspace": scope,
                    "policy": policy.as_str(),
                    "grants": grants,
                    "event_seq": events::current_seq(),
                    "task_id": task_id,
                    "task_status": task_status,
                    "task_body_chars": task_chars,
                    "hint": "seance ctl task   # re-read durable inject body for this pane",
                }))
            }
            Caps { .. } => ok(json!({
                "default_policy": self.caps.default_policy.as_str(),
                "workspace_policy": self.caps.workspace_policy.iter().map(|(k,v)| (k, v.as_str())).collect::<HashMap<_,_>>(),
                "grants": self.caps.grants,
            })),
            CapsGrant {
                principal,
                cap,
                workspace,
                ttl_secs,
                from,
                ..
            } => {
                let expires_ms = ttl_secs.map(|s| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64 + s * 1000)
                        .unwrap_or(0)
                });
                let g = crate::caps::Grant {
                    principal: principal.clone(),
                    cap: cap.clone(),
                    workspace: workspace.clone(),
                    expires_ms,
                };
                self.caps.grant(g);
                let _ = self.caps.save();
                events::log(
                    &actor(&from),
                    workspace.as_deref(),
                    None,
                    "cap_grant",
                    format!("granted {cap} to {principal}"),
                );
                ok(json!({"granted": true, "principal": principal, "cap": cap}))
            }
            CapsRevoke {
                principal,
                cap,
                workspace,
                from,
                ..
            } => {
                let n = self
                    .caps
                    .revoke(&principal, &cap, workspace.as_deref());
                let _ = self.caps.save();
                events::log(
                    &actor(&from),
                    workspace.as_deref(),
                    None,
                    "cap_revoke",
                    format!("revoked {n} grant(s) of {cap} from {principal}"),
                );
                ok(json!({"revoked": n}))
            }
            PolicyGet { workspace, scope, .. } => {
                let ws = workspace.or(scope);
                let policy = self.caps.policy_for(ws.as_deref());
                ok(json!({
                    "workspace": ws,
                    "policy": policy.as_str(),
                    "default_policy": self.caps.default_policy.as_str(),
                }))
            }
            PolicySet {
                mode,
                workspace,
                scope,
                from,
            } => {
                let Some(mode) = crate::caps::PolicyMode::parse(&mode) else {
                    return err(format!(
                        "unknown policy '{mode}' (open|propose_required|locked)"
                    ));
                };
                let ws = workspace.or(scope);
                self.caps.set_policy(ws.clone(), mode.clone());
                let _ = self.caps.save();
                events::log(
                    &actor(&from),
                    ws.as_deref(),
                    None,
                    "policy_set",
                    format!("policy -> {}", mode.as_str()),
                );
                ok(json!({"policy": mode.as_str(), "workspace": ws}))
            }
            Seize {
                pane,
                as_owner,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    let who = as_owner.unwrap_or_else(|| "human".into());
                    if who == "human" || actor(&from) == "human" {
                        self.panes[idx].agency.human_steal();
                    } else {
                        self.panes[idx].agency.agent_claim(&who);
                    }
                    events::log(
                        &actor(&from),
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "agency.seized",
                        format!("owner={}", self.panes[idx].agency.owner.as_str()),
                    );
                    self.broadcast_agency(&slug);
                    self.broadcast(self.full_state_event());
                    ok(json!(self.panes[idx].agency.to_wire()))
                }
                Err(e) => err(e),
            },
            Release {
                pane, scope, from, ..
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    self.panes[idx].agency.release();
                    events::log(
                        &actor(&from),
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "agency.released",
                        "owner=none".into(),
                    );
                    self.broadcast_agency(&slug);
                    self.broadcast(self.full_state_event());
                    ok(json!(self.panes[idx].agency.to_wire()))
                }
                Err(e) => err(e),
            },
            DriveMode {
                pane,
                mode,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let Some(dm) = crate::agency::DriveMode::parse(&mode) else {
                        return err(format!(
                            "unknown drive mode '{mode}' (pair|locked_human|agent_led)"
                        ));
                    };
                    let slug = self.panes[idx].slug.clone();
                    self.panes[idx].agency.drive_mode = dm;
                    events::log(
                        &actor(&from),
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "agency.drive_mode",
                        mode.clone(),
                    );
                    self.broadcast_agency(&slug);
                    ok(json!(self.panes[idx].agency.to_wire()))
                }
                Err(e) => err(e),
            },
            Doctor { .. } => {
                let rows = crate::agents::doctor();
                ok(serde_json::to_value(rows).unwrap_or(json!([])))
            }
            Brief { scope, .. } => {
                let panes: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|p| scope.as_deref().is_none_or(|ws| p.workspace == ws))
                    .map(|p| self.pane_summary_json(p))
                    .collect();
                ok(json!({
                    "focused_pane": self.focused_pane,
                    "selected_workspace": self.selected_workspace,
                    "pending_asks": self.asks.iter().filter(|a| a.answer.is_none()).count(),
                    "event_seq": events::current_seq(),
                    "scope": scope,
                    "panes": panes,
                }))
            }
            Note {
                pane,
                text,
                append,
                scope,
                from,
            } => {
                let target = pane.or_else(|| from.clone());
                let Some(target) = target else {
                    return err("note: need pane or $SEANCE_SESSION".into());
                };
                match find(self, &target, &scope) {
                    Ok(idx) => {
                        let path = self.panes[idx].scratch_path.clone();
                        let slug = self.panes[idx].slug.clone();
                        let ws = self.panes[idx].workspace.clone();
                        let author = actor(&from);
                        if let Err(e) = assert_self_or_cross(&slug, &from, &author) {
                            return err(e);
                        }
                        let stamp = format!(
                            "\n\n---\n<!-- {} · {} -->\n\n",
                            author,
                            chrono_lite_stamp()
                        );
                        let chunk = format!("{stamp}{text}\n");
                        let result = if append {
                            atomic_append_pad(&path, &chunk)
                        } else {
                            atomic_write_pad(&path, &format!("{text}\n"))
                        };
                        match result {
                            Ok(()) => {
                                let rev = self.bump_pad_rev(&slug);
                                let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                                events::log(
                                    &author,
                                    Some(&ws),
                                    Some(&slug),
                                    "note",
                                    format!("{} chars rev={rev}", text.len()),
                                );
                                self.persist();
                                ok(json!({
                                    "path": path.to_string_lossy(),
                                    "append": append,
                                    "pad_rev": rev,
                                    "scratchpad_bytes": bytes,
                                }))
                            }
                            Err(e) => err(e),
                        }
                    }
                    Err(e) => err(e),
                }
            }
            Finish {
                pane,
                body,
                append,
                status,
                status_note,
                empty_ok,
                task,
                scope,
                from,
            } => {
                let target = pane.or_else(|| from.clone());
                let Some(target) = target else {
                    return err("finish: need pane or $SEANCE_SESSION".into());
                };
                if let Err(e) = validate_status(&status) {
                    return err(e);
                }
                let body_empty = body.as_ref().map(|b| b.trim().is_empty()).unwrap_or(true);
                if status == "done" && body_empty && !empty_ok {
                    return err(
                        "finish: status=done requires a body (or --empty-ok). \
                         Evidence-bound completion: write the answer, then finish."
                            .into(),
                    );
                }
                match find(self, &target, &scope) {
                    Ok(idx) => {
                        let path = self.panes[idx].scratch_path.clone();
                        let slug = self.panes[idx].slug.clone();
                        let ws = self.panes[idx].workspace.clone();
                        let author = actor(&from);
                        if let Err(e) = assert_self_or_cross(&slug, &from, &author) {
                            return err(e);
                        }
                        let mut rev = self.pad_revs.get(&slug).copied().unwrap_or(0);
                        if let Some(body) = body.filter(|b| !b.trim().is_empty()) {
                            let stamp = format!(
                                "\n\n---\n<!-- {} · {} · finish -->\n\n",
                                author,
                                chrono_lite_stamp()
                            );
                            let chunk = format!("{stamp}{body}\n");
                            let write_res = if append {
                                atomic_append_pad(&path, &chunk)
                            } else {
                                atomic_write_pad(&path, &format!("{body}\n"))
                            };
                            if let Err(e) = write_res {
                                return err(format!("finish: scratchpad write failed: {e}"));
                            }
                            rev = self.bump_pad_rev(&slug);
                        }
                        let note = status_note.clone();
                        self.statuses
                            .insert(slug.clone(), (status.clone(), note.clone()));
                        let finished_task = if status == "done" {
                            self.complete_active_task(&slug, task.as_deref())
                        } else {
                            None
                        };
                        self.broadcast(GuiEvent::Status {
                            slug: slug.clone(),
                            state: status.clone(),
                            note: note.clone(),
                        });
                        events::log(
                            &author,
                            Some(&ws),
                            Some(&slug),
                            "status_set",
                            match &note {
                                Some(n) => format!("{status}: {n}"),
                                None => status.clone(),
                            },
                        );
                        events::log(
                            &author,
                            Some(&ws),
                            Some(&slug),
                            "finish",
                            format!(
                                "status={status} rev={rev} task={}",
                                finished_task.as_deref().unwrap_or("-")
                            ),
                        );
                        self.persist();
                        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        ok(json!({
                            "slug": slug,
                            "status": status,
                            "scratchpad": path.to_string_lossy(),
                            "scratchpad_bytes": bytes,
                            "pad_rev": rev,
                            "task_id": finished_task,
                        }))
                    }
                    Err(e) => err(e),
                }
            }
            Task {
                pane,
                id,
                scope,
                from,
            } => {
                // Lookup by id, else active task for pane / $SEANCE_SESSION.
                if let Some(tid) = id {
                    match self.tasks.get(&tid) {
                        Some(t) => ok(task_json(t)),
                        None => err(format!("no task '{tid}'")),
                    }
                } else {
                    let target = pane.or_else(|| from.clone());
                    let Some(target) = target else {
                        return err(
                            "task: need pane, --id, or $SEANCE_SESSION (your inject inbox)"
                                .into(),
                        );
                    };
                    match find(self, &target, &scope) {
                        Ok(idx) => {
                            let slug = self.panes[idx].slug.clone();
                            if let Some(tid) = self.active_tasks.get(&slug) {
                                match self.tasks.get(tid) {
                                    Some(t) => ok(task_json(t)),
                                    None => err(format!("active task '{tid}' missing")),
                                }
                            } else {
                                // Fall back to most recent task for this pane.
                                let mut candidates: Vec<_> = self
                                    .tasks
                                    .values()
                                    .filter(|t| t.pane == slug)
                                    .collect();
                                candidates.sort_by_key(|t| std::cmp::Reverse(t.created_ms));
                                match candidates.first() {
                                    Some(t) => ok(task_json(t)),
                                    None => err(format!("no task for pane '{slug}'")),
                                }
                            }
                        }
                        Err(e) => err(e),
                    }
                }
            }
            Roster { scope, .. } => {
                let mut panes: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|p| scope.as_deref().is_none_or(|ws| p.workspace == ws))
                    .map(|p| self.pane_summary_json(p))
                    .collect();
                // Terminals first, then by status priority (blocked/needs-human first).
                panes.sort_by(|a, b| {
                    let rank = |p: &serde_json::Value| -> u8 {
                        match p.get("status").and_then(|s| s.as_str()) {
                            Some("needs-human") => 0,
                            Some("blocked") => 1,
                            Some("working") => 2,
                            Some("planning") => 3,
                            Some("done") => 5,
                            Some("idle") => 6,
                            _ => 4,
                        }
                    };
                    rank(a).cmp(&rank(b)).then_with(|| {
                        let sa = a.get("slug").and_then(|s| s.as_str()).unwrap_or("");
                        let sb = b.get("slug").and_then(|s| s.as_str()).unwrap_or("");
                        sa.cmp(sb)
                    })
                });
                ok(json!({
                    "focused_pane": self.focused_pane,
                    "selected_workspace": self.selected_workspace,
                    "pending_asks": self.asks.iter().filter(|a| a.answer.is_none()).count(),
                    "event_seq": events::current_seq(),
                    "scope": scope,
                    "panes": panes,
                }))
            }
        }
    }

    fn bump_pad_rev(&mut self, slug: &str) -> u64 {
        let e = self.pad_revs.entry(slug.to_string()).or_insert(0);
        *e = e.saturating_add(1);
        *e
    }

    /// Open a dispatch task on inject: baseline + durable inbox body.
    fn begin_task(&mut self, slug: &str, body: &str) -> String {
        let pad_bytes = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| std::fs::metadata(&p.scratch_path).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        let pad_rev = self.pad_revs.get(slug).copied().unwrap_or(0);
        self.inject_baselines
            .insert(slug.to_string(), (pad_rev, pad_bytes));
        // Supersede prior open task on this pane.
        if let Some(old) = self.active_tasks.remove(slug) {
            if let Some(t) = self.tasks.get_mut(&old) {
                if t.status == "open" {
                    t.status = "cancelled".into();
                    t.finished_ms = Some(now_ms());
                }
            }
        }
        self.task_counter = self.task_counter.saturating_add(1);
        let id = format!("task-{}", self.task_counter);
        // Cap stored body so state.json stays sane (full inject usually fits).
        let body = if body.len() > 64_000 {
            format!("{}…\n<!-- truncated {} chars -->\n", &body[..64_000], body.len())
        } else {
            body.to_string()
        };
        let rec = TaskRecord {
            id: id.clone(),
            pane: slug.to_string(),
            inject_pad_rev: pad_rev,
            inject_pad_bytes: pad_bytes,
            body,
            status: "open".into(),
            created_ms: now_ms(),
            finished_ms: None,
        };
        // Sidecar next to scratchpad so workers can discover task_id without
        // env (agents don't re-exec on inject). Paths:
        //   <scratch>.taskid  → bare id
        //   <scratch>.task.json → id + status + body
        if let Some(p) = self.panes.iter().find(|p| p.slug == slug) {
            write_task_sidecar(&p.scratch_path, &rec);
        }
        self.tasks.insert(id.clone(), rec);
        self.active_tasks.insert(slug.to_string(), id.clone());
        id
    }

    /// Mark active (or named) task done; returns task_id if any.
    fn complete_active_task(&mut self, slug: &str, want: Option<&str>) -> Option<String> {
        let tid = want
            .map(|s| s.to_string())
            .or_else(|| self.active_tasks.get(slug).cloned());
        let Some(tid) = tid else {
            return None;
        };
        if let Some(t) = self.tasks.get_mut(&tid) {
            if t.pane != slug && want.is_some() {
                // Explicit task id for wrong pane — ignore quietly.
                return None;
            }
            t.status = "done".into();
            t.finished_ms = Some(now_ms());
            if let Some(p) = self.panes.iter().find(|p| p.slug == slug) {
                write_task_sidecar(&p.scratch_path, t);
            }
        }
        self.active_tasks.remove(slug);
        Some(tid)
    }

    /// Pad grew since last inject (rev or bytes).
    fn require_since_inject_evidence(&self, slug: &str) -> Result<(), String> {
        let Some((inj_rev, inj_bytes)) = self.inject_baselines.get(slug).copied() else {
            // No inject baseline → allow (manual status or shell pane).
            return Ok(());
        };
        let pad_rev = self.pad_revs.get(slug).copied().unwrap_or(0);
        let pad_bytes = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| std::fs::metadata(&p.scratch_path).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        if pad_rev > inj_rev || pad_bytes > inj_bytes {
            Ok(())
        } else {
            Err(format!(
                "no pad evidence since inject (rev {pad_rev}≤{inj_rev}, bytes {pad_bytes}≤{inj_bytes})"
            ))
        }
    }

    /// Build handoff bundle + list of (fd_index, raw fd) for SCM_RIGHTS.
    pub fn prepare_upgrade(&mut self) -> Result<(HandoffBundle, Vec<std::os::fd::OwnedFd>)> {
        super::set_upgrade_in_progress(true);
        let mut fds = Vec::new();
        let mut panes = Vec::new();
        for p in &self.panes {
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
        };
        Ok((bundle, fds))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn task_json(t: &TaskRecord) -> serde_json::Value {
    json!({
        "id": t.id,
        "pane": t.pane,
        "status": t.status,
        "inject_pad_rev": t.inject_pad_rev,
        "inject_pad_bytes": t.inject_pad_bytes,
        "created_ms": t.created_ms,
        "finished_ms": t.finished_ms,
        "body": t.body,
        "body_chars": t.body.len(),
    })
}

/// Write discoverable task id next to the scratchpad (worker re-orientation).
fn write_task_sidecar(scratch_path: &std::path::Path, rec: &TaskRecord) {
    let id_path = scratch_path.with_extension("taskid");
    let json_path = scratch_path.with_extension("task.json");
    let _ = std::fs::write(&id_path, format!("{}\n", rec.id));
    let summary = json!({
        "id": rec.id,
        "pane": rec.pane,
        "status": rec.status,
        "created_ms": rec.created_ms,
        "body": rec.body,
        "body_chars": rec.body.len(),
        "hint": "seance ctl task   # or: cat $SEANCE_SCRATCHPAD with .taskid extension",
    });
    if let Ok(s) = serde_json::to_string_pretty(&summary) {
        let _ = std::fs::write(&json_path, s);
    }
}

const VALID_STATUSES: &[&str] = &[
    "planning",
    "working",
    "blocked",
    "needs-human",
    "done",
    "idle",
];

fn validate_status(state: &str) -> Result<(), String> {
    if VALID_STATUSES.contains(&state) {
        Ok(())
    } else {
        Err(format!(
            "invalid status '{state}' — use one of: {}",
            VALID_STATUSES.join("|")
        ))
    }
}

/// Agents may only mutate their own pane's status/pad unless `from` is unset
/// (external cli orchestrator) or target matches session.
fn assert_self_or_cross(target_slug: &str, from: &Option<String>, actor: &str) -> Result<(), String> {
    // External orchestrator (no $SEANCE_SESSION) → cli may cross-pane.
    if from.is_none() || actor == "cli" || actor == "agent:cli" {
        return Ok(());
    }
    let principal = from.as_deref().unwrap_or("");
    let principal = principal.strip_prefix("agent:").unwrap_or(principal);
    if principal == target_slug {
        return Ok(());
    }
    Err(format!(
        "self-only: agent '{principal}' cannot status/note/finish pane '{target_slug}' \
         (orchestrators outside a pane may; or set $SEANCE_SESSION to self)"
    ))
}

/// Atomic replace via temp+rename in the same directory.
fn atomic_write_pad(path: &std::path::Path, contents: &str) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("pad"),
        std::process::id()
    ));
    std::fs::write(&tmp, contents).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Atomic append: read existing + write new via temp+rename.
fn atomic_append_pad(path: &std::path::Path, chunk: &str) -> Result<(), String> {
    let mut body = if path.exists() {
        std::fs::read_to_string(path).map_err(|e| e.to_string())?
    } else {
        String::new()
    };
    body.push_str(chunk);
    atomic_write_pad(path, &body)
}

/// Cheap local stamp without chrono dep (HH:MM:SS).
fn chrono_lite_stamp() -> String {
    events::fmt_time(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    )
}

/// Wrapper so from_handoff can take owned fds with index.
pub struct OwnedFdAdopt {
    pub fd: std::os::fd::OwnedFd,
}

fn shell_rc_path() -> PathBuf {
    PathBuf::from(shellexpand::tilde("~/.local/share/seance/seance.bash").into_owned())
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| e.to_string())
}

