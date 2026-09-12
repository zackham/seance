//! Workspace state operations: sidebar auto-sort (live-working agents first,
//! then last human touch), attention/unread bookkeeping, pane drag-reorder,
//! workspace lifecycle (create / select / cycle / move / fork / kill), and the
//! back/forward visit history the mouse's side buttons walk.
//! Pure state — no rendering lives here (the sidebar/overview views call
//! these to compute their layout).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gpui::{Context, Window};

use super::util::now_ms;
use seance_core::util::recency_rank;
use seance_core::grouping::{Section, SectionRow};

/// How long the sidebar banish × stays armed after a first click. Matches the
/// web client's `KILL_CONFIRM_MS` so both clients feel the same.
pub(super) const BANISH_ARM: Duration = Duration::from_millis(2000);

/// Is `ws` the armed circle, with the arm still live at `now`? Pure so the
/// two-click window is testable without leaning on a real clock.
pub(super) fn banish_arm_live(armed: Option<&(String, Instant)>, ws: &str, now: Instant) -> bool {
    armed.is_some_and(|(w, at)| w == ws && now.duration_since(*at) < BANISH_ARM)
}

/// Is a broadcast rail arrangement someone else's, or our own echo?
///
/// `pending > 0` means a write of ours is queued or in flight, so nothing the
/// daemon is broadcasting right now can reflect our newest state. Otherwise it
/// is ours exactly when it matches what we last sent.
pub(super) fn rail_prefs_is_foreign(pending: usize, last_sent: Option<&str>, json: &str) -> bool {
    pending == 0 && last_sent != Some(json)
}

/// Track how long each name the arrangement references has been missing from
/// the daemon's world, and return the set `prune` may keep: what the daemon
/// knows, plus anything missing for less than [`ABSENT_GRACE_MS`].
///
/// Pruning straight against `known` is destructive on a view that merely lags:
/// a quicklaunch pin placed before its spawn lands, or a circle another window
/// created a moment ago, both look identical to "killed" for one `State`.
pub(super) fn settle_absent<'a>(
    absent: &mut std::collections::HashMap<String, u64>,
    referenced: impl Iterator<Item = &'a str>,
    known: &std::collections::BTreeSet<String>,
    now: u64,
) -> std::collections::BTreeSet<String> {
    for name in referenced {
        if !known.contains(name) {
            absent.entry(name.to_string()).or_insert(now);
        }
    }
    // Back in the world → forget it ever went missing.
    absent.retain(|name, _| !known.contains(name));
    let mut protected = known.clone();
    protected.extend(
        absent
            .iter()
            .filter(|(_, since)| now.saturating_sub(**since) < ABSENT_GRACE_MS)
            .map(|(name, _)| name.clone()),
    );
    protected
}

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

/// One circle's rail position: working band first, then by the clock the row
/// displays, name as the tiebreak.
///
/// A working circle drops its clock on purpose — that is what keeps the band
/// still while agents pour out output. Rank a working circle by a live clock
/// and every row reorders on every frame.
pub(super) fn sort_key(working: bool, age_ms: u64, ws: &str) -> (u8, u64, String) {
    let band = if working { 0 } else { 1 };
    let rank = if working { 0 } else { recency_rank(age_ms) };
    (band, rank, ws.to_lowercase())
}

/// How long the arrangement may reference a circle the daemon hasn't mentioned
/// before `prune` drops it.
///
/// The asymmetry is the whole point: keeping a dead name costs one string in a
/// json file, dropping a live one destroys something he asked for. Two ways a
/// name goes briefly missing — a quicklaunch pin lands before its spawn round
/// trip completes, and any window's `State` can lag another window's fresh
/// circle. Both resolve in well under a minute.
const ABSENT_GRACE_MS: u64 = 60_000;

/// How many circles back the mouse can walk. A long day of cycling shouldn't
/// grow a list forever, and nobody navigates back past a few dozen hops.
const NAV_HISTORY_MAX: usize = 64;

/// Browser-style visit history over circles — where you've *been*, in order,
/// with a cursor at where you are now.
///
/// Deliberately not the same thing as the jump palette's recency ranking:
/// recency is a set sorted by a clock, this is a path with a position in it,
/// so back-then-forward returns you to exactly the circle you left. Per
/// window, never persisted — a fresh window starts with no history, like a
/// fresh browser tab.
///
/// The invariant that makes this work with a passive observer:
/// `entries[cursor]` is always the circle currently on screen. So a selection
/// that *isn't* `entries[cursor]` is by definition a fresh navigation, and
/// walking back/forward moves the cursor first — which is what keeps the
/// observer from mistaking our own step for a new visit and eating the
/// forward half of the history.
#[derive(Default)]
pub(super) struct NavHistory {
    entries: Vec<String>,
    cursor: usize,
}

impl NavHistory {
    /// Fold the currently-selected circle in. A no-op when it's already where
    /// the cursor sits; otherwise it's a fresh navigation, which drops
    /// whatever was ahead (same as clicking a link mid-history in a browser).
    pub(super) fn visit(&mut self, ws: &str) {
        if self.entries.get(self.cursor).map(String::as_str) == Some(ws) {
            return;
        }
        self.entries.truncate(self.cursor + 1);
        self.entries.push(ws.to_string());
        if self.entries.len() > NAV_HISTORY_MAX {
            self.entries.drain(..self.entries.len() - NAV_HISTORY_MAX);
        }
        self.cursor = self.entries.len() - 1;
    }

    /// Step the cursor back to the nearest circle that still exists, and
    /// report it. Banished circles are stepped over rather than pruned —
    /// keeping the indices stable is what lets forward retrace the same path.
    pub(super) fn back(&mut self, exists: impl Fn(&str) -> bool) -> Option<String> {
        let mut i = self.cursor;
        while i > 0 {
            i -= 1;
            if exists(&self.entries[i]) {
                self.cursor = i;
                return Some(self.entries[i].clone());
            }
        }
        None
    }

    /// The inverse, toward the newest end.
    pub(super) fn forward(&mut self, exists: impl Fn(&str) -> bool) -> Option<String> {
        let mut i = self.cursor;
        while i + 1 < self.entries.len() {
            i += 1;
            if exists(&self.entries[i]) {
                self.cursor = i;
                return Some(self.entries[i].clone());
            }
        }
        None
    }
}

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
    pub(super) fn priority(self) -> u8 {
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

    /// Fold a fresh `State` into this window's persisted arrangement, and make
    /// sure the daemon is streaming every circle it knows about.
    ///
    /// Every non-blank window subscribes to everything (0.26, when park went
    /// away), so the only reason this still touches subscriptions is catch-up:
    /// a circle created or renamed since the last `State` isn't in the
    /// connection's set until we ask for it.
    pub(super) fn reconcile_subscriptions(&mut self, known: &std::collections::BTreeSet<String>) {
        let subs = self.subscriptions.clone();
        let mut changed = false;
        if !self.subs_seeded {
            // Fresh install: everything that already exists counts as
            // looked-at, so the rail doesn't come up all badged.
            self.subs_pref.seed_seen(known);
            self.subs_seeded = true;
            changed = true;
        }
        // Selecting is looking: never badge the circle you're in as unseen.
        if let Some(sel) = self.selected_workspace.clone() {
            changed |= self.subs_pref.mark_seen(&sel);
        }
        let referenced: Vec<String> = self
            .subs_pref
            .pinned
            .iter()
            .chain(self.subs_pref.seen.iter())
            .cloned()
            .collect();
        let protected = settle_absent(
            &mut self.absent_since,
            referenced.iter().map(String::as_str),
            known,
            now_ms(),
        );
        changed |= self.subs_pref.prune(&protected);
        // A cluster that no longer exists shouldn't leave a fold behind to
        // surprise you when that name comes back.
        let live_groups: std::collections::BTreeSet<String> = self
            .workspace_sections()
            .into_iter()
            .flat_map(|(section, circles)| {
                self.section_rows(&circles)
                    .into_iter()
                    .filter_map(move |row| match row {
                        SectionRow::Group { prefix, .. } => {
                            Some(crate::subscriptions_pref::group_key(section.key(), &prefix))
                        }
                        SectionRow::Circle(_) => None,
                    })
            })
            .collect();
        changed |= self.subs_pref.prune_collapsed(&live_groups);
        if changed {
            self.save_arrangement_local();
        }
        // Anything the daemon isn't streaming yet (reconnect, rename, a circle
        // ctl just spawned) gets subscribed so its grids flow.
        if !self.empty_window {
            let missing: Vec<String> = known
                .iter()
                .filter(|w| !subs.iter().any(|s| s == *w))
                .cloned()
                .collect();
            for ws in missing {
                let _ = self.client.subscribe(&ws);
            }
        }
    }

    /// Persist a DELIBERATE arrangement change — pin, unpin, a fold you
    /// clicked, the notes face — locally and to the daemon, which shares it
    /// with every other window.
    ///
    /// **`empty_window` must never gate anything on this path.** Blank-ness is
    /// about what a window ATTACHES to, not about whether your choices count.
    /// The gate lived in three places and removing two of them fixed nothing:
    /// the local cache was written, `push_rail_to_daemon` still bailed, and
    /// boot loads the DAEMON's copy over the local one — so every pin placed in
    /// a blank window rendered, persisted nowhere that boot reads, and was gone
    /// on restart. What protects the shared copy is the deliberate/incidental
    /// split below, not the window's blank-ness.
    pub(super) fn save_arrangement(&self) {
        crate::subscriptions_pref::save(&self.subs_pref);
        self.push_rail_to_daemon();
    }

    /// Persist incidental bookkeeping — `seen`, a prune, a fold opened just to
    /// reveal a row — to the LOCAL cache only.
    ///
    /// These fire on selection and on every `State`, so pushing them raced the
    /// deliberate changes: a window whose copy predated your pin by
    /// milliseconds would push its own arrangement over the top and the daemon
    /// would broadcast the pin away. Nothing here is worth another window's
    /// attention; the next real change carries it along.
    pub(super) fn save_arrangement_local(&self) {
        crate::subscriptions_pref::save(&self.subs_pref);
    }

    /// Hand the arrangement to the daemon, which persists it and pushes it to
    /// every other window.
    ///
    /// Off the UI thread on purpose: this is a blocking bridge round trip and
    /// every caller is a click — pinning a circle must not wait on a socket,
    /// least of all over ssh from the mac.
    ///
    /// Through one long-lived writer, NOT a thread per call. A thread per call
    /// has no ordering, so pin-then-unpin could land unpin-then-pin; the daemon
    /// would persist the loser and broadcast it back, and the pin visibly
    /// undid itself. The queue also coalesces: only the newest arrangement is
    /// worth writing.
    pub(super) fn push_rail_to_daemon(&self) {
        let Some(json) = crate::subscriptions_pref::encode(&self.subs_pref) else {
            return;
        };
        if let Ok(mut last) = self.rail_push.last_sent.lock() {
            *last = Some(json.clone());
        }
        self.rail_push.pending.fetch_add(1, Ordering::SeqCst);
        if self.rail_push.tx.send(json).is_err() {
            self.rail_push.pending.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Should an inbound `RailPrefs` be adopted, or is it our own change coming
    /// back around?
    ///
    /// The daemon broadcasts to every window INCLUDING the sender, so without
    /// this a window overwrites its own fresh state with an older copy of it.
    pub(super) fn rail_prefs_is_foreign(&self, json: &str) -> bool {
        let last = self.rail_push.last_sent.lock().ok();
        rail_prefs_is_foreign(
            self.rail_push.pending.load(Ordering::SeqCst),
            last.as_ref().and_then(|l| l.as_deref()),
            json,
        )
    }

    /// Pin a circle to the top section (context menu "pin to top", and every
    /// quicklaunch click, which pins before the spawn round trip returns —
    /// see [`ABSENT_GRACE_MS`]).
    pub(super) fn pin_workspace(&mut self, ws: &str) {
        if self.subs_pref.pin(ws) {
            self.save_arrangement();
        }
    }

    /// Drop a circle out of the pinned section. It falls back below the
    /// divider into its lifecycle band.
    pub(super) fn unpin_workspace(&mut self, ws: &str) {
        if self.subs_pref.unpin(ws) {
            self.save_arrangement();
        }
    }

    /// Flip the selected circle's pin (ctrl+shift+p). Same two calls the row
    /// menu makes — the chord is just a faster way to reach them, and the rail
    /// moving the row between bands is the feedback.
    pub(super) fn toggle_pin_workspace(&mut self, ws: &str) {
        if self.subs_pref.is_pinned(ws) {
            self.unpin_workspace(ws);
        } else {
            self.pin_workspace(ws);
        }
    }

    /// The rail's two bands in display order, each carrying the single sort
    /// from [`Self::workspaces`]: pinned, then everything else.
    pub(super) fn workspace_sections(&self) -> Vec<(Section, Vec<String>)> {
        seance_core::grouping::partition_sections(&self.workspaces(), &self.subs_pref.pinned)
    }

    /// One band's rows: loose circles and prefix clusters, in sort order.
    /// Grouping reads the LABEL, so retyping a name is how you regroup.
    pub(super) fn section_rows(&self, circles: &[String]) -> Vec<SectionRow> {
        seance_core::grouping::group_by_prefix(circles, |ws| self.workspace_label(ws))
    }

    /// Every circle the rail is actually SHOWING, top-to-bottom, in draw
    /// order. This is the ctrl+page ring and the neighbour list for kill.
    ///
    /// Folds count: a collapsed band or cluster is not on screen, so cycling
    /// skips it. That makes collapsing a way to narrow what ctrl+page walks —
    /// fold the piles you're not in and the rotation is just your working set.
    pub(super) fn visible_workspaces(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (section, circles) in self.workspace_sections() {
            if circles.is_empty() {
                continue;
            }
            for row in self.section_rows(&circles) {
                match row {
                    SectionRow::Circle(ws) => out.push(ws),
                    SectionRow::Group { prefix, members } => {
                        let key = crate::subscriptions_pref::group_key(section.key(), &prefix);
                        if !self.subs_pref.is_collapsed(&key) {
                            out.extend(members);
                        }
                    }
                }
            }
        }
        out
    }

    /// Position of a circle's row among the elements the rail emits, so
    /// scroll-to-item lands on it. Cluster headers and the pinned rule are
    /// rows too.
    fn rail_row_index(&self, workspace: &str) -> Option<usize> {
        let mut i = 0usize;
        let mut any_pinned = false;
        for (section, circles) in self.workspace_sections() {
            if circles.is_empty() {
                continue;
            }
            any_pinned |= section == Section::Pinned;
            if section == Section::Active && any_pinned {
                i += 1; // the rule under the pinned band
            }
            for row in self.section_rows(&circles) {
                match row {
                    SectionRow::Circle(ws) => {
                        if ws == workspace {
                            return Some(i);
                        }
                        i += 1;
                    }
                    SectionRow::Group { prefix, members } => {
                        i += 1; // cluster header
                        let key = crate::subscriptions_pref::group_key(section.key(), &prefix);
                        if self.subs_pref.is_collapsed(&key) {
                            continue;
                        }
                        for ws in members {
                            if ws == workspace {
                                return Some(i);
                            }
                            i += 1;
                        }
                    }
                }
            }
        }
        None
    }

    /// The cluster fold a circle's row hides under, if any. Pure lookup over
    /// the same sectioning the rail renders from, so the unfold and the row
    /// index can never disagree about where a circle is.
    fn rail_row_cluster(&self, workspace: &str) -> Option<String> {
        for (section, circles) in self.workspace_sections() {
            for row in self.section_rows(&circles) {
                if let SectionRow::Group { prefix, members } = row {
                    if members.iter().any(|m| m == workspace) {
                        return Some(crate::subscriptions_pref::group_key(section.key(), &prefix));
                    }
                }
            }
        }
        None
    }

    /// Bring a circle's row into view: unfold whatever hides it, then scroll.
    ///
    /// Called on every select, so ctrl+page cycling, a jump, and a host menu
    /// creating a circle all land the same way — the rail always shows you
    /// where you just went.
    pub(super) fn reveal_workspace_row(&mut self, workspace: &str) {
        if let Some(key) = self.rail_row_cluster(workspace) {
            if self.subs_pref.uncollapse(&key) {
                self.save_arrangement_local();
            }
        }
        // Count the elements the rail actually emits above this row: the
        // pinned rule, one per cluster header, one per circle.
        if let Some(idx) = self.rail_row_index(workspace) {
            self.sidebar_scroll.scroll_to_item(idx);
        }
    }

    /// Badge for a rail row: the normal live attention, or `needs` for a circle
    /// this window has never selected — which is how a ctl-spawned circle
    /// announces itself.
    pub(super) fn row_attention(&self, ws: &str) -> Option<WorkspaceAttention> {
        self.workspace_attention_cx(ws).or({
            if self.subs_pref.never_seen(ws) {
                Some(WorkspaceAttention::NeedsHuman)
            } else {
                None
            }
        })
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

    fn workspace_sort_key(&self, ws: &str) -> (u8, u64, String) {
        let at = self
            .workspace_activity
            .get(ws)
            .copied()
            .max(self.workspace_touch.get(ws).copied())
            .unwrap_or(0);
        // Never observed sorts last, not first.
        let age = if at == 0 {
            u64::MAX
        } else {
            now_ms().saturating_sub(at)
        };
        sort_key(self.workspace_has_working_agent(ws), age, ws)
    }

    /// Is an agent working in this circle right now?
    ///
    /// ONLY the title spinner, which in practice means claude. Output recency
    /// looks like a universal signal and is not: codex repaints its TUI on a
    /// timer, so its output clock never goes stale and every codex circle read
    /// as permanently working. codex puts no working state in its title
    /// either, so for a non-spinner agent the honest answer is "unknown" —
    /// which is why the idle band is ranked so that an unknown circle sits
    /// still instead of churning (see [`recency_rank`]).
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
            // No touch bump on the falling edge any more. Working is keyed off
            // output recency now, so a circle that just went quiet already
            // holds the freshest clock in the idle band and lands at the top
            // on its own — bumping it was a second, redundant reorder, and it
            // fired for circles whose edge nobody observed.
            if self.workspace_has_working_agent(&ws) {
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
        // Born here → looked at here; the daemon subscribes us on create.
        self.subs_pref.mark_seen(&name);
        self.save_arrangement_local();
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
        // Selecting is looking — clears the `needs` badge. The daemon
        // auto-subscribes on SetFocus, so nothing to ask for here.
        if self.subs_pref.mark_seen(workspace) {
            self.save_arrangement_local();
        }
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
        // Reveal the selection in the rail. Scrolling alone isn't enough: a
        // circle inside a folded band or cluster has no row to scroll TO, so
        // jumping into one (the sleeping band starts folded) left
        // the rail sitting wherever it was, showing no sign of where you went.
        // Unfold first, then scroll.
        self.reveal_workspace_row(workspace);
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

    /// Fold the current selection into the back/forward history. Called once
    /// per render.
    ///
    /// The selection moves from a dozen places — a rail click, ctrl+page, the
    /// jump palette, clicking a pane that lives in another circle
    /// (`set_active` sets it directly), a `ctl`
    /// spawn pulling this window across — and the daemon can move it without
    /// this window asking. Watching the value catches all of them; asking
    /// every caller to remember would catch the ones I thought of today.
    pub(super) fn sync_nav_history(&mut self) {
        if let Some(sel) = self.selected_workspace.clone() {
            self.nav.visit(&sel);
        }
    }

    /// Mouse back button: the circle you were in before this one.
    pub(super) fn nav_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let known = self.known_workspace_names();
        let Some(ws) = self.nav.back(|w| known.contains(w)) else {
            return;
        };
        self.nav_to(&ws, "back", window, cx);
    }

    /// Mouse forward button: undo a back.
    pub(super) fn nav_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let known = self.known_workspace_names();
        let Some(ws) = self.nav.forward(|w| known.contains(w)) else {
            return;
        };
        self.nav_to(&ws, "forward", window, cx);
    }

    fn nav_to(&mut self, ws: &str, dir: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.client.log_event(
            "human",
            Some(ws),
            None,
            "workspace_selected",
            format!("navigated {dir} to workspace '{ws}'"),
        );
        // The cursor already points at `ws`, so the render-time observer reads
        // this as "still where the history says we are" and leaves the forward
        // half alone.
        self.select_workspace(ws, window, cx);
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
        // point of folding them away. Cycle EXACTLY the list the sidebar shows,
        // read live at each press (owner decision 2026-08-02: pageup/down
        // must always correspond to what the left sidebar displays — no
        // snapshots, no alternate orders).
        let list = self.visible_workspaces();
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

    /// Jump to the rail's TOP row (ctrl+shift+home / ctrl+shift+1) — whatever
    /// the sort says is first: the first pinned circle if anything is pinned,
    /// else the top of active (a working agent, else most recent), and so on
    /// down the bands.
    pub(super) fn select_top_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_nth_workspace(0, window, cx);
    }

    /// Jump to rail row `idx` (0-based; ctrl+shift+1..9). Reads the same live
    /// list ctrl+page walks, so the row you count with your eye is the row you
    /// get — folded bands aren't on screen and aren't counted. Out of range is
    /// a no-op rather than a clamp: pressing 7 on a five-circle rail means you
    /// were counting a rail that isn't there, and landing somewhere arbitrary
    /// is worse than nothing happening.
    pub(super) fn select_nth_workspace(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.visible_workspaces().into_iter().nth(idx) else {
            return;
        };
        if self.selected_workspace.as_deref() == Some(ws.as_str()) {
            return;
        }
        self.client.log_event(
            "human",
            Some(&ws),
            None,
            "workspace_selected",
            format!("jumped to rail row {} — '{ws}'", idx + 1),
        );
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

    /// Kill every pane in a workspace, then drop the workspace itself.
    /// Sidebar banish × click. Banishing kills every pane's PTY and nothing
    /// brings them back, so a bare click only *arms* the ×; the kill needs a
    /// second click on the same circle inside `BANISH_ARM`.
    pub(super) fn banish_click(
        &mut self,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if banish_arm_live(self.banish_armed.as_ref(), workspace, Instant::now()) {
            self.banish_armed = None;
            self.kill_workspace(workspace, window, cx);
            return;
        }
        // Arming a different circle replaces the old arm, so only ever one ×
        // is hot.
        self.banish_armed = Some((workspace.to_string(), Instant::now()));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BANISH_ARM).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |app: &mut SeanceApp, cx| {
                    // Only clear a *stale* arm — a click that re-armed while
                    // this timer slept owns the × now.
                    if app
                        .banish_armed
                        .as_ref()
                        .is_some_and(|(_, at)| at.elapsed() >= BANISH_ARM)
                    {
                        app.banish_armed = None;
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

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
            let order = self.visible_workspaces();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the history the way the app does: every selection change is
    /// folded in by the render-time observer, including the ones our own
    /// back/forward caused.
    fn observe(nav: &mut NavHistory, ws: &str) {
        nav.visit(ws);
    }

    fn all(_: &str) -> bool {
        true
    }

    #[test]
    fn back_then_forward_returns_to_where_you_left() {
        let mut nav = NavHistory::default();
        for ws in ["a", "b", "c"] {
            observe(&mut nav, ws);
        }
        assert_eq!(nav.back(all).as_deref(), Some("b"));
        observe(&mut nav, "b");
        assert_eq!(nav.back(all).as_deref(), Some("a"));
        observe(&mut nav, "a");
        assert_eq!(nav.back(all), None, "nothing before the first circle");
        assert_eq!(nav.forward(all).as_deref(), Some("b"));
        observe(&mut nav, "b");
        assert_eq!(nav.forward(all).as_deref(), Some("c"));
        observe(&mut nav, "c");
        assert_eq!(nav.forward(all), None);
    }

    #[test]
    fn a_fresh_visit_drops_the_forward_half() {
        let mut nav = NavHistory::default();
        for ws in ["a", "b", "c"] {
            observe(&mut nav, ws);
        }
        nav.back(all);
        observe(&mut nav, "b");
        // Now go somewhere new instead of forward — "c" is gone.
        observe(&mut nav, "d");
        assert_eq!(nav.forward(all), None);
        assert_eq!(nav.back(all).as_deref(), Some("b"));
    }

    #[test]
    fn walking_back_is_not_itself_a_new_visit() {
        // The regression the cursor-first invariant exists to prevent: if the
        // observer treated our own step as a fresh navigation it would
        // truncate, and forward would be dead after one back.
        let mut nav = NavHistory::default();
        for ws in ["a", "b"] {
            observe(&mut nav, ws);
        }
        let target = nav.back(all).unwrap();
        observe(&mut nav, &target);
        assert_eq!(nav.forward(all).as_deref(), Some("b"));
    }

    #[test]
    fn reselecting_the_same_circle_records_nothing() {
        let mut nav = NavHistory::default();
        observe(&mut nav, "a");
        observe(&mut nav, "a");
        observe(&mut nav, "a");
        assert_eq!(nav.back(all), None);
    }

    #[test]
    fn revisiting_a_circle_is_a_new_entry_not_a_jump() {
        let mut nav = NavHistory::default();
        for ws in ["a", "b", "a"] {
            observe(&mut nav, ws);
        }
        assert_eq!(nav.back(all).as_deref(), Some("b"));
        observe(&mut nav, "b");
        assert_eq!(nav.back(all).as_deref(), Some("a"));
    }

    #[test]
    fn banished_circles_are_stepped_over_both_ways() {
        let mut nav = NavHistory::default();
        for ws in ["a", "gone", "c"] {
            observe(&mut nav, ws);
        }
        let alive = |w: &str| w != "gone";
        assert_eq!(nav.back(alive).as_deref(), Some("a"));
        observe(&mut nav, "a");
        assert_eq!(nav.forward(alive).as_deref(), Some("c"));
    }

    #[test]
    fn history_is_bounded_and_keeps_the_newest() {
        let mut nav = NavHistory::default();
        for i in 0..NAV_HISTORY_MAX + 10 {
            observe(&mut nav, &format!("c{i}"));
        }
        assert_eq!(nav.entries.len(), NAV_HISTORY_MAX);
        assert_eq!(
            nav.entries.last().map(String::as_str),
            Some(format!("c{}", NAV_HISTORY_MAX + 9).as_str())
        );
        // The cursor survives the trim — back still walks, forward doesn't
        // wander off the end.
        assert_eq!(
            nav.back(all).as_deref(),
            Some(format!("c{}", NAV_HISTORY_MAX + 8).as_str())
        );
    }

    #[test]
    fn banish_arm_is_per_circle_and_expires() {
        let now = Instant::now();
        let armed = Some(("circle-7".to_string(), now));

        // Live only for the circle actually armed, and only inside the window.
        assert!(banish_arm_live(armed.as_ref(), "circle-7", now));
        assert!(banish_arm_live(
            armed.as_ref(),
            "circle-7",
            now + BANISH_ARM - Duration::from_millis(1)
        ));
        assert!(!banish_arm_live(armed.as_ref(), "circle-2", now));

        // Dead on the boundary, and a never-clicked × is never live — so the
        // second click can only land on the circle the first one named.
        assert!(!banish_arm_live(
            armed.as_ref(),
            "circle-7",
            now + BANISH_ARM
        ));
        assert!(!banish_arm_live(None, "circle-7", now));
    }

    /// The daemon echoes every write back to its sender. Adopting that echo is
    /// how a pin undid itself: the window replaced fresh local state with an
    /// older copy of it.
    #[test]
    fn our_own_echo_is_never_adopted() {
        let mine = r#"{"pinned":["lab"]}"#;
        assert!(!rail_prefs_is_foreign(0, Some(mine), mine));
    }

    /// A real change from another window still lands.
    #[test]
    fn another_windows_arrangement_is_adopted() {
        assert!(rail_prefs_is_foreign(
            0,
            Some(r#"{"pinned":["lab"]}"#),
            r#"{"pinned":["lab","raid"]}"#
        ));
        // Nothing sent yet — anything inbound is foreign by definition.
        assert!(rail_prefs_is_foreign(0, None, r#"{"pinned":[]}"#));
    }

    /// While a write of ours is queued or in flight, ANY broadcast is stale by
    /// construction — including one that happens to differ from what we sent.
    #[test]
    fn nothing_is_adopted_while_our_write_is_in_flight() {
        assert!(!rail_prefs_is_foreign(1, Some("a"), "b"));
        assert!(!rail_prefs_is_foreign(3, None, "b"));
    }

    fn known(items: &[&str]) -> std::collections::BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The quicklaunch bug: the pin lands before the spawn round trip does, so
    /// the next `State` carries no such circle.
    #[test]
    fn a_name_the_daemon_has_not_mentioned_yet_survives() {
        let mut absent = std::collections::HashMap::new();
        let p = settle_absent(&mut absent, ["staff-report"].into_iter(), &known(&["lab"]), 1_000);
        assert!(p.contains("staff-report"), "a fresh pin must not be pruned");
        assert_eq!(absent.get("staff-report"), Some(&1_000));
    }

    /// Still missing a full grace period later — now it really is gone.
    #[test]
    fn a_name_missing_past_the_grace_is_dropped() {
        let mut absent = std::collections::HashMap::new();
        settle_absent(&mut absent, ["gone"].into_iter(), &known(&["lab"]), 1_000);
        let p = settle_absent(
            &mut absent,
            ["gone"].into_iter(),
            &known(&["lab"]),
            1_000 + ABSENT_GRACE_MS,
        );
        assert!(!p.contains("gone"));
    }

    /// The clock starts at FIRST absence and doesn't restart on every `State`,
    /// or a killed circle would be shielded forever.
    #[test]
    fn the_absence_clock_does_not_restart_each_state() {
        let mut absent = std::collections::HashMap::new();
        for t in [1_000, 2_000, 3_000] {
            settle_absent(&mut absent, ["gone"].into_iter(), &known(&[]), t);
        }
        assert_eq!(absent.get("gone"), Some(&1_000), "first sighting wins");
    }

    /// A circle that shows up resets: a later disappearance gets its own full
    /// grace rather than inheriting a stale clock.
    #[test]
    fn reappearing_clears_the_absence() {
        let mut absent = std::collections::HashMap::new();
        settle_absent(&mut absent, ["ws"].into_iter(), &known(&[]), 1_000);
        settle_absent(&mut absent, ["ws"].into_iter(), &known(&["ws"]), 2_000);
        assert!(absent.is_empty(), "back in the world");
        let p = settle_absent(&mut absent, ["ws"].into_iter(), &known(&[]), 3_000);
        assert!(p.contains("ws"));
        assert_eq!(absent.get("ws"), Some(&3_000));
    }

    /// THE bug: codex repaints on a timer, so its output clock never goes
    /// stale. Ranked on the raw clock, two such circles swap places on every
    /// repaint, forever. Quantized to the label they display, they tie and
    /// fall back to name order.
    /// THE bug: codex repaints on its own timer, so its age jitters by seconds
    /// and never settles. Any per-second ranking reshuffles the list forever;
    /// everything inside a minute has to tie and fall back to name.
    #[test]
    fn circles_repainting_on_a_timer_hold_still() {
        // Sampled live: these three sat at 1.7s / 1.8s / 1.9s, then 2.6 / 2.7 /
        // 2.7, drifting across the old 5s edge and reshuffling every pass.
        for (a_age, b_age) in [(300u64, 2_900u64), (3_100, 120), (6_000, 45_000), (75_000, 8_000)] {
            assert!(
                sort_key(false, a_age, "cadence-perf") < sort_key(false, b_age, "onboarding"),
                "name order must survive {a_age}ms vs {b_age}ms of jitter"
            );
        }
    }

    /// Real staleness still orders, at minute granularity and coarser.
    #[test]
    fn genuinely_older_circles_sort_lower() {
        assert!(sort_key(false, 30_000, "b") < sort_key(false, 20 * 60_000, "a"));
        assert!(sort_key(false, 20 * 60_000, "b") < sort_key(false, 4 * 3_600_000, "a"));
        assert!(sort_key(false, 4 * 3_600_000, "b") < sort_key(false, 3 * 86_400_000, "a"));
    }

    /// Working outranks idle no matter how fresh the idle one is.
    #[test]
    fn working_outranks_idle() {
        assert!(sort_key(true, u64::MAX, "zzz") < sort_key(false, 0, "aaa"));
    }

    /// A circle nobody ever observed sorts last, not first.
    #[test]
    fn never_observed_sorts_last() {
        assert!(sort_key(false, 5_000, "a") < sort_key(false, u64::MAX, "a"));
    }

    /// Monotonic across every boundary — a non-monotonic rank would reorder
    /// rows as they age.
    #[test]
    fn recency_rank_is_monotonic_across_bucket_edges() {
        let edges = [
            0,
            599_999,
            600_000,
            3_599_999,
            3_600_000,
            86_399_999,
            86_400_000,
        ];
        let ranks: Vec<u64> = edges.iter().map(|ms| recency_rank(*ms)).collect();
        assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{ranks:?}");
        assert_eq!(recency_rank(0), recency_rank(599_999), "under 10m all ties");
        assert!(recency_rank(599_999) < recency_rank(600_000), "9m < 10m");
        assert!(recency_rank(3_599_999) < recency_rank(3_600_000), "59m < 1h");
    }
}
