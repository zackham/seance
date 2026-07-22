//! Session engine: panes, control plane, layout state. gpui-free.

mod control;
mod gui;
pub(crate) mod helpers;
mod spawn;

#[cfg(test)]
mod tests;

// Re-export helpers used by sibling modules / tests.
pub use helpers::OwnedFdAdopt;

use gui::{GuiConn, LastGridFrame};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use std::os::fd::OwnedFd;

use anyhow::Result;

use super::protocol::*;
use super::pty_session::{AdoptedPty, PtySession, SessionEvent};
use crate::cmdlog::CommandLog;
use crate::scratchpad::ScratchpadStore;
use crate::state::{AppState, PersistedPane};

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
        let slug = crate::state::unique_slug(name, &taken);
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

    pub(super) fn session_mut(&mut self, slug: &str) -> Option<&mut PtySession> {
        self.panes
            .iter_mut()
            .find(|p| p.slug == slug)
            .and_then(|p| p.session.as_mut())
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
        let pad_revs: Vec<(String, u64)> =
            self.pad_revs.iter().map(|(k, v)| (k.clone(), *v)).collect();
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
