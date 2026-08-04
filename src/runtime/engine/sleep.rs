//! Sleep / wake: a circle you're done with for now costs no RAM.
//!
//! A sleeping pane has **no process** — the PTY is gone and so is everything
//! the agent was holding (a claude pane and its MCP children run ~350MB; forty
//! of them is most of a workstation). What survives is everything that makes
//! the pane *itself*: slug, name, circle, cwd, command, its claude conversation
//! id, its scratchpad, its task and status — plus the **last frame it rendered**,
//! frozen to disk and served in place of a live snapshot so the circle still
//! reads after the process is gone.
//!
//! Waking is the restore path that already exists: `claude --resume <id>` in
//! the same cwd. That is why sleep is only offered for panes that can be put
//! back exactly — see [`Engine::pane_restorable`]. A shell pane can't (its cwd
//! drift, its history, its running children are not reconstructible), so one
//! shell vetoes its whole circle.
//!
//! The deliberate crash. Sleeping does to a pane exactly what the 2026-08-04
//! OOM did to all of them — and the reason it is safe now is the same fix that
//! made that OOM survivable: the pane owns its conversation id.

use std::path::PathBuf;

use anyhow::{bail, Result};

use super::helpers::{atomic_write_bytes, now_ms};
use super::spawn::{claude_session_arg, is_claude_cmd, transcript_path};
use super::Engine;
use crate::events;
use crate::runtime::snapshot::{decode_grid_bin, encode_grid_bin, GridSnapshot};

/// Idle before a circle sleeps itself, when every pane in it is restorable.
pub const AUTO_SLEEP_IDLE_MS: u64 = 12 * 60 * 60 * 1000;

/// `<state-dir>/frozen/<slug>.scg` — the last frame of a sleeping pane.
fn frozen_dir() -> PathBuf {
    match crate::state::state_dir() {
        Ok(dir) => dir.join("frozen"),
        Err(_) => PathBuf::from(shellexpand::tilde("~/.local/share/seance/frozen").into_owned()),
    }
}

fn frozen_path(slug: &str) -> PathBuf {
    frozen_dir().join(format!("{slug}.scg"))
}

impl Engine {
    /// Can this pane be put back exactly as it is?
    ///
    /// * file pane — yes, it's a path and a viewer, there is no process.
    /// * claude pane with a session id **and a transcript on disk** — yes,
    ///   `--resume` lands on the same conversation.
    /// * anything else (a shell, a non-claude agent, a claude pane that was
    ///   never prompted) — no. Sleeping it would throw away state nothing can
    ///   rebuild, so it isn't offered.
    pub fn pane_restorable(&self, slug: &str) -> bool {
        let Some(p) = self.panes.iter().find(|p| p.slug == slug) else {
            return false;
        };
        if p.kind == "file" {
            return true;
        }
        if !is_claude_cmd(&p.command) {
            return false;
        }
        p.claude_session
            .as_ref()
            .is_some_and(|id| transcript_path(&p.cwd, id).is_file())
    }

    /// Every pane in the circle is restorable (and there is at least one).
    /// This is the gate on both the manual verb and the automatic sweep.
    pub fn workspace_restorable(&self, workspace: &str) -> bool {
        let mut any = false;
        for p in self.panes.iter().filter(|p| p.workspace == workspace) {
            any = true;
            if !self.pane_restorable(&p.slug) {
                return false;
            }
        }
        any
    }

    /// The panes that block a circle from sleeping, for a message worth reading.
    pub fn workspace_sleep_blockers(&self, workspace: &str) -> Vec<String> {
        self.panes
            .iter()
            .filter(|p| p.workspace == workspace && !self.pane_restorable(&p.slug))
            .map(|p| p.slug.clone())
            .collect()
    }

    /// Freeze the pane's current frame, kill its process, keep the pane.
    ///
    /// Idempotent: sleeping a sleeping pane is a no-op, not an error, so a
    /// sweep and a right-click can race harmlessly.
    pub fn sleep_pane(&mut self, slug: &str) -> Result<bool> {
        let Some(idx) = self.panes.iter().position(|p| p.slug == slug) else {
            bail!("no pane '{slug}'");
        };
        if self.panes[idx].asleep {
            return Ok(false);
        }
        if !self.pane_restorable(slug) {
            bail!("pane '{slug}' can't be restored, so it won't be slept");
        }
        // Freeze BEFORE the process dies — after shutdown the grid is gone.
        if let Some(snap) = self.snapshot_pane(slug) {
            self.freeze_grid(slug, snap);
        }
        let pane = &mut self.panes[idx];
        pane.asleep = true;
        // `asleep` is set first on purpose: shutting the PTY down fires
        // `SessionEvent::Exited`, whose handler auto-closes panes. That guard
        // reads this flag.
        if let Some(s) = pane.session.take() {
            s.shutdown();
        }
        self.pane_busy.remove(slug);
        self.broadcast(crate::runtime::protocol::GuiEvent::PaneBusy {
            pane: slug.to_string(),
            busy: false,
        });
        events::log("daemon", None, Some(slug), "pane_slept", "slept".into());
        Ok(true)
    }

    /// Relaunch a sleeping pane onto the conversation it owns.
    pub fn wake_pane(&mut self, slug: &str) -> Result<bool> {
        let Some(idx) = self.panes.iter().position(|p| p.slug == slug) else {
            bail!("no pane '{slug}'");
        };
        if !self.panes[idx].asleep {
            return Ok(false);
        }
        let (kind, cwd, command, workspace, session_id) = {
            let p = &self.panes[idx];
            (
                p.kind.clone(),
                p.cwd.clone(),
                p.command.clone(),
                p.workspace.clone(),
                p.claude_session.clone(),
            )
        };

        if kind == "file" {
            self.panes[idx].asleep = false;
            self.thaw_grid(slug);
            events::log("daemon", None, Some(slug), "pane_woke", "woke".into());
            return Ok(true);
        }

        // `--resume` when a transcript exists, else re-assert `--session-id`:
        // a missing transcript makes claude exit non-zero, which would close
        // the pane we are trying to bring back.
        let launch = match session_id.as_deref() {
            Some(id) => claude_session_arg(&command, &cwd, id),
            None => command.clone(),
        };
        let session = self.spawn_terminal_session(slug, &launch, &cwd, &workspace, false)?;
        let pane = &mut self.panes[idx];
        pane.session = Some(session);
        pane.asleep = false;
        self.thaw_grid(slug);
        events::log("daemon", None, Some(slug), "pane_woke", "woke".into());
        Ok(true)
    }

    /// Sleep every pane in the circle. All-or-nothing: a circle holding one
    /// unrestorable pane stays awake, and says which one.
    pub fn sleep_workspace(&mut self, workspace: &str) -> Result<usize> {
        if !self.workspace_restorable(workspace) {
            let blockers = self.workspace_sleep_blockers(workspace);
            if blockers.is_empty() {
                bail!("circle '{workspace}' has no panes to sleep");
            }
            bail!(
                "circle '{workspace}' can't sleep — not restorable: {}",
                blockers.join(", ")
            );
        }
        let slugs: Vec<String> = self
            .panes
            .iter()
            .filter(|p| p.workspace == workspace)
            .map(|p| p.slug.clone())
            .collect();
        let mut n = 0;
        for slug in slugs {
            if self.sleep_pane(&slug)? {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Wake every sleeping pane in the circle.
    pub fn wake_workspace(&mut self, workspace: &str) -> Result<usize> {
        let slugs: Vec<String> = self
            .panes
            .iter()
            .filter(|p| p.workspace == workspace && p.asleep)
            .map(|p| p.slug.clone())
            .collect();
        let mut n = 0;
        for slug in slugs {
            if self.wake_pane(&slug)? {
                n += 1;
            }
        }
        Ok(n)
    }

    /// True while any pane of the circle is asleep.
    pub fn workspace_asleep(&self, workspace: &str) -> bool {
        self.panes
            .iter()
            .any(|p| p.workspace == workspace && p.asleep)
    }

    /// Circles idle longer than `idle_ms` whose every pane is restorable.
    ///
    /// Idle is the daemon's own clock — last real pane output, floored by last
    /// human input — the same pair the sidebar shows, so "12h since anything
    /// happened here" means what it says on the row.
    pub fn auto_sleep_candidates(&self, idle_ms: u64, now: u64) -> Vec<String> {
        let mut names: Vec<String> = self
            .panes
            .iter()
            .map(|p| p.workspace.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        names.retain(|ws| {
            if self.workspace_asleep(ws) || !self.workspace_restorable(ws) {
                return false;
            }
            let last = self
                .workspace_output
                .get(ws)
                .copied()
                .unwrap_or(0)
                .max(self.workspace_touch_ms.get(ws).copied().unwrap_or(0));
            // Never slept an unclocked circle: no observation is not evidence
            // of idleness.
            last > 0 && now.saturating_sub(last) >= idle_ms
        });
        names
    }

    /// One sweep. Returns the circles it put to sleep.
    pub fn auto_sleep_sweep(&mut self, idle_ms: u64) -> Vec<String> {
        let mut slept = Vec::new();
        for ws in self.auto_sleep_candidates(idle_ms, now_ms()) {
            match self.sleep_workspace(&ws) {
                Ok(n) if n > 0 => {
                    events::log(
                        "daemon",
                        Some(&ws),
                        None,
                        "workspace_slept",
                        format!("{n} panes slept after {}h idle", idle_ms / 3_600_000),
                    );
                    slept.push(ws);
                }
                _ => {}
            }
        }
        slept
    }

    /// Keep a pane's last frame, in memory and on disk.
    fn freeze_grid(&mut self, slug: &str, snap: GridSnapshot) {
        if let Ok(bytes) = encode_grid_bin(&snap) {
            let dir = frozen_dir();
            if std::fs::create_dir_all(&dir).is_ok() {
                let _ = atomic_write_bytes(&frozen_path(slug), &bytes);
            }
        }
        self.frozen_grids.insert(slug.to_string(), snap);
    }

    /// Drop a frozen frame once the pane is live again.
    fn thaw_grid(&mut self, slug: &str) {
        self.frozen_grids.remove(slug);
        let _ = std::fs::remove_file(frozen_path(slug));
    }

    /// The frozen frame for a sleeping pane, if we have one.
    pub(super) fn frozen_grid(&self, slug: &str) -> Option<GridSnapshot> {
        self.frozen_grids.get(slug).cloned()
    }

    /// Load frozen frames off disk for panes that came back asleep (daemon
    /// restart or `seance upgrade`). A frame that won't decode is dropped: the
    /// pane still reads as asleep, just blank, which is honest.
    pub(super) fn load_frozen_grids(&mut self) {
        let slugs: Vec<String> = self
            .panes
            .iter()
            .filter(|p| p.asleep)
            .map(|p| p.slug.clone())
            .collect();
        for slug in slugs {
            if let Ok(bytes) = std::fs::read(frozen_path(&slug)) {
                if let Ok(mut snap) = decode_grid_bin(&bytes) {
                    snap.pane = slug.clone();
                    snap.running = false;
                    self.frozen_grids.insert(slug, snap);
                }
            }
        }
    }
}
