//! Workspace state operations: sidebar auto-sort (live-working agents first,
//! then last human touch), attention/unread bookkeeping, pane drag-reorder,
//! and workspace lifecycle (create / select / cycle / move / fork / kill).
//! Pure state — no rendering lives here (the sidebar/overview views call
//! these to compute their layout).

use gpui::{Context, Window};

use crate::events;

use super::util::{now_ms, title_looks_busy};
use super::{RenameTarget, SeanceApp};

/// Badge on an *inactive* workspace header in the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkspaceAttention {
    /// Observed live-busy (TUI title spinner / agent actively driving).
    Working,
    /// Blocked or needs-human.
    NeedsHuman,
    /// Finished work while the human was elsewhere — sticky until select.
    Done,
}

impl WorkspaceAttention {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsHuman => "needs",
            Self::Done => "done",
        }
    }
    pub(super) fn color(self) -> gpui::Hsla {
        match self {
            Self::Working => crate::theme::SeancePalette::flame(),
            Self::NeedsHuman => crate::theme::SeancePalette::violet(),
            Self::Done => crate::theme::SeancePalette::success(),
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

impl SeanceApp {
    /// Unsorted set of workspace names this window knows about.
    pub(super) fn known_workspace_names(&self) -> std::collections::HashSet<String> {
        self.panes
            .iter()
            .map(|s| s.workspace.clone())
            .chain(self.extra_workspaces.iter().cloned())
            .chain(self.selected_workspace.iter().cloned())
            .collect()
    }

    /// All workspaces in sidebar display order.
    ///
    /// 1. Circles with an actively working agent float to the top.
    /// 2. Within / outside that band, sort by last *human touch* (typing into
    ///    a terminal in the circle, or right-click → "touch"). Selecting a
    ///    workspace alone does not bump touch. No manual drag-reorder.
    pub(super) fn workspaces(&self, cx: &gpui::App) -> Vec<String> {
        let mut out: Vec<String> = self.known_workspace_names().into_iter().collect();
        out.sort_by_key(|ws| self.workspace_sort_key(ws, cx));
        out
    }

    fn workspace_sort_key(&self, ws: &str, cx: &gpui::App) -> (u8, std::cmp::Reverse<u64>, String) {
        // 0 = has a live-working agent, 1 = everyone else.
        let band = if self.workspace_has_working_agent(ws, cx) {
            0
        } else {
            1
        };
        let touch = self.workspace_touch.get(ws).copied().unwrap_or(0);
        (band, std::cmp::Reverse(touch), ws.to_string())
    }

    /// Any pane in this circle currently shows agent work in progress.
    fn workspace_has_working_agent(&self, workspace: &str, cx: &gpui::App) -> bool {
        self.panes
            .iter()
            .any(|p| p.workspace == workspace && self.pane_is_live_working(&p.slug, cx))
    }

    /// Observed live-busy: braille OSC title spinner, or agent-owned status.
    fn pane_is_live_working(&self, slug: &str, cx: &gpui::App) -> bool {
        if let Some(title) = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| p.title(cx))
        {
            if title_looks_busy(&title) {
                return true;
            }
        }
        let owner = self.owners.get(slug);
        let st = self.statuses.get(slug).map(|s| s.state.as_str());
        match (owner, st) {
            // Human-owned sticky "working" is often stale inject chrome — ignore.
            (Some(o), Some("working") | Some("planning")) if o.owner == "human" => false,
            (_, Some("working") | Some("planning")) => true,
            (Some(o), _) if o.owner.starts_with("agent:") && !o.exited => {
                // Agent holds keys without status-set — still "live" if title busy already handled.
                false
            }
            _ => false,
        }
    }

    /// Bump this circle's recency so it sorts above idle peers (working agents
    /// still float above everything). Sources: human typing into a terminal
    /// here, right-click → touch, newly created circles, and the moment a
    /// workspace *finishes* working (falls out of the live-working band).
    pub(super) fn touch_workspace(&mut self, ws: &str) {
        if ws.is_empty() {
            return;
        }
        self.workspace_touch.insert(ws.to_string(), now_ms());
    }

    /// Recompute live-working per workspace. When a circle stops having any
    /// working agent, bump its touch so it lands at the top of the
    /// non-working band (freshly finished work is what you want next).
    pub(super) fn sync_workspace_working_touches(&mut self, cx: &gpui::App) {
        let names: Vec<String> = self.known_workspace_names().into_iter().collect();
        for ws in names {
            let now = self.workspace_has_working_agent(&ws, cx);
            let was = self.workspace_was_working.contains(&ws);
            if was && !now {
                self.touch_workspace(&ws);
            }
            if now {
                self.workspace_was_working.insert(ws);
            } else {
                self.workspace_was_working.remove(&ws);
            }
        }
    }

    /// Track a newly known workspace name and give it a fresh touch so it
    /// appears near the top of the non-working band.
    pub(super) fn ensure_workspace_at_bottom(&mut self, ws: &str) {
        if self.workspace_order.iter().any(|w| w == ws) {
            return;
        }
        self.workspace_order.push(ws.to_string());
        self.touch_workspace(ws);
    }

    pub(super) fn note_workspace_status_event(&mut self, slug: &str, state: &str) {
        let Some(ws) = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| p.workspace.clone())
        else {
            return;
        };
        // Status changes do *not* bump touch — only human typing / explicit
        // touch menu. Working agents re-sort via live-busy detection.
        // Sticky unread only when the human is *not* looking at this circle.
        if self.selected_workspace.as_deref() == Some(ws.as_str()) {
            self.workspace_unread.remove(&ws);
            return;
        }
        let att = match state {
            "needs-human" | "blocked" | "risky" => Some(WorkspaceAttention::NeedsHuman),
            "done" => Some(WorkspaceAttention::Done),
            "working" | "planning" => Some(WorkspaceAttention::Working),
            _ => None,
        };
        if let Some(a) = att {
            let cur = self.workspace_unread.get(&ws).copied();
            if cur.map(|c| a.priority() > c.priority()).unwrap_or(true) {
                self.workspace_unread.insert(ws, a);
            }
        }
    }

    /// Live attention with title spinners (needs `&App`) — badges only;
    /// sidebar order uses [`Self::workspace_has_working_agent`].
    pub(super) fn workspace_attention_cx(
        &self,
        workspace: &str,
        cx: &gpui::App,
    ) -> Option<WorkspaceAttention> {
        let needs = self.panes.iter().any(|p| {
            p.workspace == workspace
                && matches!(
                    self.statuses.get(&p.slug).map(|s| s.state.as_str()),
                    Some("needs-human") | Some("blocked") | Some("risky")
                )
        });
        if needs {
            return Some(WorkspaceAttention::NeedsHuman);
        }
        if self.workspace_has_working_agent(workspace, cx) {
            return Some(WorkspaceAttention::Working);
        }
        self.workspace_unread.get(workspace).copied()
    }

    /// Move `slug` into `workspace`, positioned before pane `before_slug`
    /// (or appended when `before_slug` is None). Optimistic local reorder;
    /// daemon reorders + persists and pushes State back.
    pub(super) fn reorder_pane(
        &mut self,
        slug: &str,
        workspace: &str,
        before_slug: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if Some(slug) == before_slug {
            return;
        }
        let Some(from_idx) = self.panes.iter().position(|p| p.slug == slug) else {
            return;
        };
        let mut pane = self.panes.remove(from_idx);
        pane.workspace = workspace.to_string();
        let insert_at = before_slug
            .and_then(|b| self.panes.iter().position(|p| p.slug == b))
            .unwrap_or(self.panes.len());
        events::log(
            "human",
            Some(workspace),
            Some(slug),
            "pane_moved",
            format!("moved '{}' into {} (reorder)", pane.name, workspace),
        );
        self.panes.insert(insert_at, pane);
        self.selected_workspace = Some(workspace.to_string());
        let _ = self.client.move_pane(slug, workspace, before_slug);
        cx.notify();
    }

    pub(super) fn create_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.known_workspace_names();
        let mut n = existing.len() + 1;
        let name = loop {
            let candidate = format!("circle-{n}");
            if !existing.contains(&candidate) {
                break candidate;
            }
            n += 1;
        };
        let _ = self.client.create_workspace(&name);
        if !self.extra_workspaces.contains(&name) {
            self.extra_workspaces.push(name.clone());
        }
        self.ensure_workspace_at_bottom(&name);
        self.selected_workspace = Some(name.clone());
        // Immediate inline rename — name is known up front.
        self.start_rename(RenameTarget::Workspace(name.clone()), &name, window, cx);
    }

    pub(super) fn select_workspace(
        &mut self,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.selected_workspace.as_deref() != Some(workspace);
        // Remember which pane was active in the circle we're leaving.
        if changed {
            if let (Some(old_ws), Some(slug)) =
                (self.selected_workspace.clone(), self.active_slug.clone())
            {
                if self
                    .panes
                    .iter()
                    .any(|p| p.slug == slug && p.workspace == old_ws)
                {
                    self.workspace_focus.insert(old_ws, slug);
                }
            }
        }
        self.selected_workspace = Some(workspace.to_string());
        // Selecting a circle clears sticky "done/needs" unread — does NOT bump touch.
        self.workspace_unread.remove(workspace);
        // When entering a circle that was off-screen, zero local revs for its
        // panes so the daemon's full flush can't be dropped as "stale". The
        // daemon also sends FULL frames on workspace change.
        if changed {
            let slugs: Vec<String> = self
                .panes
                .iter()
                .filter(|p| p.workspace == workspace)
                .map(|p| p.slug.clone())
                .collect();
            for slug in slugs {
                if let Some(rt) = self
                    .panes
                    .iter()
                    .find(|p| p.slug == slug)
                    .and_then(|p| p.remote_terminal())
                    .cloned()
                {
                    // Keep last pixels until the full frame lands — only reset
                    // the rev gate, not the cells (avoids a blank flash).
                    rt.update(cx, |t, _| t.open_rev_gate());
                }
            }
        }
        // Invariant: workspace with panes always has an active pane.
        // Keep current active if it's already in this workspace; else restore
        // remembered / first tiled / any.
        let restore = self
            .active_slug
            .clone()
            .filter(|s| {
                self.panes
                    .iter()
                    .any(|p| p.slug == *s && p.workspace == workspace)
            })
            .or_else(|| self.preferred_pane_in_workspace(workspace));
        if let Some(slug) = restore {
            if self.active_slug.as_deref() != Some(slug.as_str()) {
                self.set_active(&slug, window, cx);
                return;
            }
            let _ = self
                .client
                .set_focus(Some(slug), Some(workspace.to_string()));
        } else {
            // Empty workspace — no pane to activate.
            self.active_slug = None;
            let _ = self.client.set_focus(None, Some(workspace.to_string()));
        }
        self.persist(cx);
        cx.notify();
    }

    /// Cycle the selected workspace in sidebar order. `delta` is +1 (next /
    /// PageDown) or -1 (prev / PageUp). Wraps. Focuses a pane in the target
    /// workspace when one exists so keyboard goes there.
    pub(super) fn cycle_workspace(
        &mut self,
        delta: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let list = self.workspaces(cx);
        if list.is_empty() {
            return;
        }
        let cur = self
            .selected_workspace
            .as_deref()
            .and_then(|w| list.iter().position(|x| x == w))
            .unwrap_or(0);
        let n = list.len() as i32;
        let next = (cur as i32 + delta).rem_euclid(n) as usize;
        let ws = list[next].clone();
        if self.selected_workspace.as_deref() == Some(ws.as_str()) {
            return;
        }
        events::log(
            "human",
            Some(&ws),
            None,
            "workspace_selected",
            format!("cycled to workspace '{ws}'"),
        );
        // Restores last active pane for `ws` (or first tiled/any).
        self.select_workspace(&ws, window, cx);
    }

    pub(super) fn move_to_workspace(
        &mut self,
        slug: &str,
        workspace: &str,
        cx: &mut Context<Self>,
    ) {
        // Append into target workspace (no before-slug) — same path as drag
        // onto a workspace header, so order persists via the daemon.
        self.reorder_pane(slug, workspace, None, cx);
    }

    /// Fork a workspace via the daemon (sole owner of PTYs + scratch copy).
    /// GUI never spawns local PTYs post-daemon-split.
    pub(super) fn fork_workspace(
        &mut self,
        src: &str,
        name: Option<String>,
        actor: &str,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if !self.panes.iter().any(|p| p.workspace == src) {
            return None;
        }
        if let Err(e) = self.client.fork_workspace(src, name.clone()) {
            eprintln!("[seance] fork_workspace via daemon failed: {e:#}");
            return None;
        }
        let new_ws = name
            .as_ref()
            .map(|n| crate::state::slugify(n))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("{src}-fork"));
        events::log(
            actor,
            Some(&new_ws),
            None,
            "workspace_forked",
            format!("fork requested '{src}' -> '{new_ws}' (daemon)"),
        );
        cx.notify();
        Some(new_ws)
    }

    /// Kill every pane in a workspace, then drop the workspace itself.
    pub(super) fn kill_workspace(&mut self, workspace: &str, cx: &mut Context<Self>) {
        let _ = self.client.kill_workspace(workspace);
        cx.notify();
    }
}
