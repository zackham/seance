//! Workspace state operations: sidebar auto-sort (live-working agents first,
//! then last human touch), attention/unread bookkeeping, pane drag-reorder,
//! and workspace lifecycle (create / select / cycle / move / fork / kill).
//! Pure state — no rendering lives here (the sidebar/overview views call
//! these to compute their layout).

use gpui::{Context, Window};

use super::util::now_ms;

/// Coarse one-unit relative time for sidebar labels.
pub(super) fn rel_label(delta_ms: u64) -> String {
    let s = delta_ms / 1000;
    match s {
        0..=4 => "now".into(),
        5..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}
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

    /// Fold a fresh `State` into this window's persisted active/seen sets.
    ///
    /// The daemon auto-subscribes on select / spawn / create / fork from this
    /// window, so its subscription set *is* the auto-add signal (minus
    /// anything we just parked, whose `Unsubscribe` may still be in flight).
    /// With no persisted file the first State seeds the active list outright —
    /// that's the migration from the ownership model.
    pub(super) fn reconcile_subscriptions(&mut self, known: &std::collections::BTreeSet<String>) {
        let subs = self.subscriptions.clone();
        let mut changed = false;
        if !self.subs_seeded {
            self.subs_pref.seed_from_daemon(&subs, known);
            self.subs_seeded = true;
            changed = true;
        } else {
            changed |= self
                .subs_pref
                .adopt_daemon_subscriptions(&subs, &self.park_pending);
        }
        // The park landed once the daemon stops listing the workspace.
        self.park_pending.retain(|w| subs.iter().any(|s| s == w));
        // Selecting is looking: never badge the circle you're in as unseen.
        if let Some(sel) = self.selected_workspace.clone() {
            changed |= self.subs_pref.activate(&sel);
        }
        changed |= self.subs_pref.prune(known);
        if changed {
            self.save_subscriptions();
        }
        // Anything active the daemon isn't streaming (reconnect, rename) gets
        // re-subscribed so its grids flow again.
        let missing: Vec<String> = self
            .subs_pref
            .active
            .iter()
            .filter(|w| !subs.iter().any(|s| s == *w) && known.contains(*w))
            .cloned()
            .collect();
        for ws in missing {
            let _ = self.client.subscribe(&ws);
        }
    }

    /// Persist the active/seen sets and refresh the reconnect `Attach` seed.
    /// Blank windows own no arrangement — they must not clobber the file.
    pub(super) fn save_subscriptions(&self) {
        if self.empty_window {
            return;
        }
        crate::subscriptions_pref::save(&self.subs_pref);
        self.client
            .set_subscription_seed(self.subs_pref.active.iter().cloned().collect());
    }

    /// Add a workspace to the active band (context menu "add to active", and
    /// every select of a parked circle).
    pub(super) fn activate_workspace(&mut self, ws: &str) {
        self.park_pending.remove(ws);
        if self.subs_pref.activate(ws) {
            self.save_subscriptions();
        }
        if !self.subscriptions.iter().any(|s| s == ws) {
            let _ = self.client.subscribe(ws);
        }
    }

    /// Move a workspace to the parked group. Parking the circle you're looking
    /// at moves the selection to the next active one first (the daemon would
    /// otherwise pick for us on `Unsubscribe`).
    pub(super) fn park_workspace(&mut self, ws: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_workspace.as_deref() == Some(ws) {
            let order = self.active_workspaces();
            let next = order
                .iter()
                .position(|w| w == ws)
                .and_then(|idx| {
                    order
                        .get(idx + 1)
                        .or_else(|| idx.checked_sub(1).and_then(|j| order.get(j)))
                })
                .cloned();
            if let Some(next) = next {
                self.select_workspace(&next, window, cx);
            }
        }
        self.subs_pref.park(ws);
        self.park_pending.insert(ws.to_string());
        self.save_subscriptions();
        let _ = self.client.unsubscribe(ws);
        cx.notify();
    }

    /// Pin a circle to the top section (context menu "pin to top"). Pinned
    /// implies active, so a parked circle is promoted + subscribed here.
    pub(super) fn pin_workspace(&mut self, ws: &str) {
        self.park_pending.remove(ws);
        if self.subs_pref.pin(ws) {
            self.save_subscriptions();
        }
        if !self.subscriptions.iter().any(|s| s == ws) {
            let _ = self.client.subscribe(ws);
        }
    }

    /// Drop a circle out of the pinned section. It stays active — it just
    /// falls back below the divider into the normal band.
    pub(super) fn unpin_workspace(&mut self, ws: &str) {
        if self.subs_pref.unpin(ws) {
            self.save_subscriptions();
        }
    }

    /// The three sidebar bands in display order: (pinned, active-unpinned,
    /// parked). One sort ([`Self::workspaces`]) applied inside every band.
    pub(super) fn workspace_bands(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        crate::subscriptions_pref::partition3(
            &self.workspaces(),
            &self.subs_pref.active,
            &self.subs_pref.pinned,
        )
    }

    /// Workspaces in the pinned section, sidebar display order.
    pub(super) fn pinned_workspaces(&self) -> Vec<String> {
        self.workspace_bands().0
    }

    /// Active-but-unpinned workspaces — the band below the divider.
    pub(super) fn unpinned_active_workspaces(&self) -> Vec<String> {
        self.workspace_bands().1
    }

    /// Every rendered (non-parked) workspace, top-to-bottom exactly as the
    /// rail draws it: pinned section first, then the normal active band. This
    /// is the ctrl+page ring and the neighbour list for park/kill.
    pub(super) fn active_workspaces(&self) -> Vec<String> {
        let mut out = self.pinned_workspaces();
        out.extend(self.unpinned_active_workspaces());
        out
    }

    /// Workspaces in the collapsed parked group, same sort as active rows.
    pub(super) fn parked_workspaces(&self) -> Vec<String> {
        self.workspace_bands().2
    }

    /// Badge for a parked row: the normal live attention, or `needs` for a
    /// circle this window has never looked at (ctl spawns land parked+needs).
    pub(super) fn parked_attention(&self, ws: &str) -> Option<WorkspaceAttention> {
        self.workspace_attention_cx(ws).or({
            if self.subs_pref.never_seen(ws) {
                Some(WorkspaceAttention::NeedsHuman)
            } else {
                None
            }
        })
    }

    /// Collapsed parked header summary: (count, highest-priority attention).
    pub(super) fn parked_summary(&self) -> (usize, Option<WorkspaceAttention>) {
        let parked = self.parked_workspaces();
        let att = parked
            .iter()
            .filter_map(|ws| self.parked_attention(ws))
            .max_by_key(|a| a.priority());
        (parked.len(), att)
    }

    /// All workspaces in sidebar display order.
    ///
    /// 1. Circles with an actively working agent float to the top.
    /// 2. Inside the working band, **alphabetical** — a working circle's row
    ///    must not move while you read it, and any recency clock reshuffles
    ///    the band as agents start and stop.
    /// 3. Outside it, by last *human touch* (typing into a terminal in the
    ///    circle, or right-click → "touch"). Selecting a workspace alone does
    ///    not bump touch. No manual drag-reorder.
    pub(super) fn workspaces(&self) -> Vec<String> {
        let mut out: Vec<String> = self.known_workspace_names().into_iter().collect();
        out.sort_by_key(|ws| self.workspace_sort_key(ws));
        out
    }

    fn workspace_sort_key(&self, ws: &str) -> (u8, std::cmp::Reverse<u64>, String) {
        // 0 = has a live-working agent, 1 = everyone else.
        let band = if self.workspace_has_working_agent(ws) {
            0
        } else {
            1
        };
        // Working band: no clock at all — the name is the whole key, so the
        // list is stable while a dozen agents start and finish. Idle band: by
        // the clock the row displays (last real output; human touch as floor).
        let at = if band == 0 {
            0
        } else {
            self.workspace_activity
                .get(ws)
                .copied()
                .max(self.workspace_touch.get(ws).copied())
                .unwrap_or(0)
        };
        (band, std::cmp::Reverse(at), ws.to_lowercase())
    }

    /// Any pane in this circle currently shows agent work in progress.
    fn workspace_has_working_agent(&self, workspace: &str) -> bool {
        self.panes
            .iter()
            .any(|p| p.workspace == workspace && self.pane_is_live_working(&p.slug))
    }

    /// Live-busy, as the DAEMON sees it: braille OSC title spinner, or
    /// agent-owned status.
    ///
    /// The spinner half deliberately does *not* read the local terminal title.
    /// Grid frames only arrive for the workspace this window has selected, so
    /// every other circle's title is frozen at whatever it last received —
    /// which is exactly the spinner it was wearing when you looked away. The
    /// daemon broadcasts busy flips for every pane instead.
    fn pane_is_live_working(&self, slug: &str) -> bool {
        if self.busy_panes.contains(slug) {
            return true;
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

    /// Wake a circle AND land the keyboard in it.
    ///
    /// Every awaken affordance is a click, and a click leaves focus on the
    /// thing you clicked (the bar button, the context-menu item) — so without
    /// this you'd have to click the pane before typing. The pane view already
    /// exists (sleeping never unmounted it); `pending_focus` survives the
    /// round-trip and is applied on the first render after the daemon relaunches.
    pub(super) fn wake_workspace_focused(
        &mut self,
        ws: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.client.wake_workspace(ws);
        if let Some(slug) = self.preferred_pane_in_workspace(ws) {
            self.set_active(&slug, window, cx);
            self.pending_focus = Some(slug);
        }
        cx.notify();
    }

    /// What to show for a circle. Its slug is the identity; this is the
    /// mutable label, and it falls back to the slug — which is exactly what a
    /// circle reads as until someone renames it.
    pub(super) fn workspace_label(&self, ws: &str) -> String {
        self.workspace_names
            .get(ws)
            .cloned()
            .unwrap_or_else(|| ws.to_string())
    }

    /// Any pane of this circle is asleep — the circle reads as asleep.
    pub(super) fn workspace_asleep(&self, ws: &str) -> bool {
        self.panes.iter().any(|p| p.workspace == ws && p.asleep)
    }

    /// Every pane in the circle can be put back exactly (daemon's verdict, on
    /// the wire as `PaneInfo::restorable`). Gates the "sleep circle" verb.
    pub(super) fn workspace_sleepable(&self, ws: &str) -> bool {
        let mut any = false;
        for p in self.panes.iter().filter(|p| p.workspace == ws) {
            any = true;
            if !p.restorable {
                return false;
            }
        }
        any
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
    pub(super) fn sync_workspace_working_touches(&mut self) {
        let names: Vec<String> = self.known_workspace_names().into_iter().collect();
        for ws in names {
            let now = self.workspace_has_working_agent(&ws);
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
    pub(super) fn workspace_attention_cx(&self, workspace: &str) -> Option<WorkspaceAttention> {
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
        // A live working spinner outranks PR attention — an agent actively in
        // the circle is usually already on the red PR; the chip stays visible
        // regardless. On idle circles a `needs` PR resurfaces the row exactly
        // like an agent asking for help (web client mirrors this order).
        if self.workspace_has_working_agent(workspace) {
            return Some(WorkspaceAttention::Working);
        }
        let pr = super::prlinks::pr_attention(self.pr_links_for(workspace));
        if pr == Some(WorkspaceAttention::NeedsHuman) {
            return Some(WorkspaceAttention::NeedsHuman);
        }
        self.workspace_unread.get(workspace).copied().or(pr)
    }

    /// Sidebar right-edge label: relative time since the last pane output in
    /// this circle ("now", "42s", "3m", "2h", "4d"); None while a working
    /// agent's spinner owns the slot, or when nothing was ever observed.
    pub(super) fn workspace_activity_label(&self, ws: &str) -> Option<String> {
        if self.workspace_has_working_agent(ws) {
            return None;
        }
        let at = *self.workspace_activity.get(ws)?;
        Some(rel_label(now_ms().saturating_sub(at)))
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
        self.client.log_event(
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
        // Born here → active here (the daemon subscribes us too).
        self.activate_workspace(&name);
        if !self.extra_workspaces.contains(&name) {
            self.extra_workspaces.push(name.clone());
        }
        self.ensure_workspace_at_bottom(&name);
        self.selected_workspace = Some(name.clone());
        // Empty circle: don't keep a foreign active_slug — that would route
        // focus to a pane in another workspace after rename finishes.
        self.active_slug = None;
        let _ = self.client.set_focus(None, Some(name.clone()));
        // Immediate inline rename — name is known up front. On Enter/Esc,
        // restore_keyboard_focus parks on the app root so ctrl+shift+n works
        // without an intervening click.
        self.start_rename(RenameTarget::Workspace(name.clone()), &name, window, cx);
    }

    pub(super) fn select_workspace(
        &mut self,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.selected_workspace.as_deref() != Some(workspace);
        // Selecting a parked circle promotes it: subscribe + into the active
        // band + marked seen (the daemon auto-subscribes on SetFocus too).
        self.activate_workspace(workspace);
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
        // Reveal the selection in the sidebar — ctrl+page cycling can land on
        // a row scrolled out of the rail. Row index = position in display
        // order (one child per workspace group in the list), plus one for the
        // pinned/unpinned divider element when the row sits below it.
        let (pinned, active, _) = self.workspace_bands();
        let divider = !pinned.is_empty();
        let idx = pinned.iter().position(|w| w == workspace).or_else(|| {
            active
                .iter()
                .position(|w| w == workspace)
                .map(|i| i + pinned.len() + usize::from(divider))
        });
        if let Some(idx) = idx {
            self.sidebar_scroll.scroll_to_item(idx);
        }
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
            // Empty workspace — no pane to activate. Park keyboard focus on
            // the app root: the previously focused terminal's view is still
            // ALIVE (its pane just isn't rendered in this circle), so GPUI
            // happily keeps focus on a handle that is no longer in the
            // dispatch tree — capture never runs and ctrl+page stops working
            // until you click. `window.focused()` is Some there, so
            // ensure_keyboard_focus's None-recovery can't save us either.
            self.active_slug = None;
            self.pending_focus = None;
            let fh = self.focus_handle.clone();
            window.focus(&fh, cx);
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
        // Parked circles are deliberately out of the rotation — that's the
        // point of parking them. Cycle EXACTLY the list the sidebar shows,
        // read live at each press (owner decision 2026-08-02: pageup/down
        // must always correspond to what the left sidebar displays — no
        // snapshots, no alternate orders).
        let list = self.active_workspaces();
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
        self.client.log_event(
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
        self.client.log_event(
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
    pub(super) fn kill_workspace(
        &mut self,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Banishing the ACTIVE circle: select the neighbor below (above when
        // last) in sidebar order — not the daemon's arbitrary first-pane
        // fallback — so the human lands somewhere predictable.
        if self.selected_workspace.as_deref() == Some(workspace) {
            let order = self.active_workspaces();
            if let Some(idx) = order.iter().position(|w| w == workspace) {
                let neighbor = order
                    .get(idx + 1)
                    .or_else(|| idx.checked_sub(1).and_then(|j| order.get(j)))
                    .cloned();
                if let Some(n) = neighbor {
                    let _ = self.client.kill_workspace(workspace);
                    self.select_workspace(&n, window, cx);
                    cx.notify();
                    return;
                }
            }
        }
        let _ = self.client.kill_workspace(workspace);
        cx.notify();
    }
}
