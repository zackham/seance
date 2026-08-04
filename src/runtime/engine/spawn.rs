//! Session spawn/kill lifecycle: PTY setup, `SEANCE_*` env, persisted restore,
//! kill/reap, workspace fork + pane/workspace reorder.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;

use super::helpers::shell_rc_path;
use super::{Engine, EnginePane, SpawnSpec, DEFAULT_COMMAND, DEFAULT_WORKSPACE};
use crate::events;
use crate::runtime::pty_session::{PtySession, SpawnConfig};
use crate::state::{slugify, unique_slug, PersistedPane};

/// True when `command` launches the claude CLI — bare (`claude …`) or by
/// absolute path (`/home/…/bin/claude …`), which is how agent profiles spawn.
pub(super) fn is_claude_cmd(command: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .map(|first| {
            first
                .rsplit('/')
                .next()
                .map(|base| base == "claude")
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// A random v4 UUID. Claude's `--session-id` requires UUID shape; we mint the
/// id ourselves so the pane owns its conversation across daemon death.
fn new_session_uuid() -> String {
    use rand::Rng;
    let b: [u8; 16] = rand::thread_rng().gen();
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    // Force version 4 / variant 10xx in the canonical positions.
    format!(
        "{}-{}-4{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

/// Where claude keeps a session transcript: `~/.claude/projects/<cwd>/<id>.jsonl`,
/// with `/` and `.` in the absolute cwd flattened to `-`.
fn transcript_path(cwd_raw: &str, session: &str) -> PathBuf {
    let cwd = PathBuf::from(shellexpand::tilde(cwd_raw).into_owned());
    let encoded: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect();
    PathBuf::from(shellexpand::tilde("~/.claude/projects").into_owned())
        .join(encoded)
        .join(format!("{session}.jsonl"))
}

/// Pull an already-explicit session id out of a command (`--resume <id>` /
/// `--session-id <id>`), so a hand-written command keeps that conversation
/// pinned to the pane across restarts instead of being re-minted.
fn extract_session_flag(command: &str) -> Option<String> {
    let toks: Vec<&str> = command.split_whitespace().collect();
    toks.iter()
        .position(|t| *t == "--resume" || *t == "--session-id")
        .and_then(|i| toks.get(i + 1))
        .filter(|id| !id.starts_with('-'))
        .map(|id| id.to_string())
}

/// The launch command for a claude pane whose session id we own.
///
/// `--resume` only works once a transcript exists; a pane that was created but
/// never prompted has none, and `--resume` on a missing id exits non-zero,
/// which closes the pane. In that case re-assert the same id with
/// `--session-id` so the pane keeps its identity either way.
fn claude_session_arg(command: &str, cwd_raw: &str, session: &str) -> String {
    if transcript_path(cwd_raw, session).is_file() {
        format!("{command} --resume {session}")
    } else {
        format!("{command} --session-id {session}")
    }
}

impl Engine {
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
        self.record_event(
            &slug,
            seance_core::replay::ReplayEvent::Spawned {
                name: name.clone(),
                workspace: workspace.clone(),
                command: spec.command.clone().unwrap_or_default(),
            },
        );
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
                    claude_session: None,
                    scratch_path,
                    file: Some(path.to_string_lossy().to_string()),
                    session: None,
                    agency: crate::agency::Agency::default(),
                },
            );
            events::log(
                "daemon",
                None,
                Some(&slug),
                "pane_spawned",
                "file pane".into(),
            );
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
        // Own the conversation: mint the session id here rather than letting
        // claude pick one, so a pane can be put back on its exact session after
        // a crash. `--continue` is deliberately NOT the restore path — it
        // resumes the most recent conversation *in the cwd*, so panes sharing a
        // cwd (the normal case) would all land on the same one.
        let mut claude_session = extract_session_flag(&command);
        if claude_session.is_none() && is_claude_cmd(&command) && !command.contains("--continue") {
            let id = new_session_uuid();
            command = format!("{command} --session-id {id}");
            claude_session = Some(id);
        } else if spec.resume && is_claude_cmd(&command) && !command.contains("--continue") {
            command = format!("{command} --continue");
        }

        let session = self.spawn_terminal_session(&slug, &command, &cwd_raw, &workspace, false)?;

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
                claude_session,
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

    pub(super) fn spawn_from_persisted(&mut self, p: &PersistedPane) -> Result<()> {
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
                claude_session: None,
                scratch_path: self.store.path_for(&p.slug),
                file: Some(path.to_string_lossy().to_string()),
                session: None,
                agency: crate::agency::Agency::default(),
            });
            return Ok(());
        }

        let mut command = p.command.clone();
        // Put the pane back on the exact conversation it had. Falls back to the
        // legacy `--continue` only for panes persisted before session ids were
        // minted (no id on record).
        let mut claude_session = p
            .claude_session
            .clone()
            .or_else(|| extract_session_flag(&command));
        if let Some(id) = claude_session.clone() {
            if is_claude_cmd(&command) && extract_session_flag(&command).is_none() {
                command = claude_session_arg(&command, &p.cwd, &id);
            }
        } else if p.resume_on_restore && is_claude_cmd(&command) && !command.contains("--continue")
        {
            command = format!("{command} --continue");
        }
        // A pane restored from a pre-session-id state file adopts one now, so
        // the *next* crash is recoverable even if this one wasn't.
        if claude_session.is_none() && is_claude_cmd(&command) && !command.contains("--continue") {
            let id = new_session_uuid();
            command = format!("{command} --session-id {id}");
            claude_session = Some(id);
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
            claude_session,
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
        let mut cwd = PathBuf::from(shellexpand::tilde(cwd_raw).into_owned());
        // A missing cwd (config typo, path from another machine) must not turn
        // the spawn into a silent dead click — fall back to home, loudly.
        if !cwd.is_dir() {
            eprintln!(
                "[seance daemon] spawn '{slug}': cwd {} missing — falling back to ~",
                cwd.display()
            );
            cwd = PathBuf::from(shellexpand::tilde("~").into_owned());
        }
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
            crate::control::bind_socket_path()
                .to_string_lossy()
                .to_string(),
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
            let workspace = pane.workspace.clone();
            if let Some(s) = pane.session.take() {
                s.shutdown();
            }
            self.cmd_log.remove_pane(slug);
            self.statuses.remove(slug);
            self.pane_busy.remove(slug);
            if self.focused_pane.as_deref() == Some(slug) {
                self.focused_pane = self.panes.first().map(|p| p.slug.clone());
            }
            // Last pane gone and nobody created this circle on purpose → drop
            // the row (order/clocks/pr_links/subscriptions) instead of leaving
            // an empty one in both sidebars.
            self.prune_workspace_if_empty(&workspace);
            events::log("daemon", None, Some(slug), "pane_killed", "killed".into());
        }
    }

    pub(super) fn fork_workspace(&mut self, src: &str, name: Option<String>) -> Result<String> {
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
        let idx = order
            .iter()
            .position(|w| w == before)
            .unwrap_or(order.len());
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
}

#[cfg(test)]
mod session_id_tests {
    use super::*;

    #[test]
    fn claude_detected_bare_and_by_path() {
        assert!(is_claude_cmd("claude --dangerously-skip-permissions"));
        assert!(is_claude_cmd("/home/zack/.local/bin/claude --resume abc"));
        assert!(!is_claude_cmd("bash -l"));
        assert!(!is_claude_cmd("bash --init-file /x/seance.bash"));
        assert!(!is_claude_cmd("claude-agent-acp"));
    }

    #[test]
    fn minted_uuid_is_v4_shaped() {
        let id = new_session_uuid();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "{id}"
        );
        assert_ne!(new_session_uuid(), new_session_uuid());
    }

    #[test]
    fn explicit_session_flags_are_adopted_not_reminted() {
        assert_eq!(
            extract_session_flag("claude --resume 9f3c1a2b --dangerously-skip-permissions")
                .as_deref(),
            Some("9f3c1a2b")
        );
        assert_eq!(
            extract_session_flag("claude --session-id 9f3c1a2b").as_deref(),
            Some("9f3c1a2b")
        );
        assert_eq!(extract_session_flag("claude --continue"), None);
        assert_eq!(extract_session_flag("claude --resume"), None); // no id follows
        assert_eq!(extract_session_flag("bash -l"), None);
    }

    #[test]
    fn restore_falls_back_to_session_id_when_no_transcript() {
        // A pane created but never prompted has no transcript; `--resume` would
        // exit non-zero and close the pane, so the id is re-asserted instead.
        let cmd = claude_session_arg("claude", "/tmp/definitely-not-a-project", "no-such-session");
        assert_eq!(cmd, "claude --session-id no-such-session");
    }
}
