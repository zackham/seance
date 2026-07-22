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
        if spec.resume && command.starts_with("claude") && !command.contains("--continue") {
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
                scratch_path: self.store.path_for(&p.slug),
                file: Some(path.to_string_lossy().to_string()),
                session: None,
                agency: crate::agency::Agency::default(),
            });
            return Ok(());
        }

        let mut command = p.command.clone();
        if p.resume_on_restore && command.starts_with("claude") && !command.contains("--continue") {
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
