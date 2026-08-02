//! Per-workspace PR link list: scrape ingest, watcher ingest, hygiene.
//!
//! The daemon owns only the *URLs* (scraped off pane output — see
//! `runtime/pr_scrape.rs`). Their **statuses** come from an external poller
//! that writes `<state_dir>/pr_watch.json`; the daemon re-reads it on mtime
//! change (`daemon/prwatch.rs`) and merges the verdicts in. Clients then fold
//! `attention` into the existing workspace attention machinery, so a parked
//! circle with a red PR resurfaces exactly like an agent asking for help.

use std::collections::HashMap;

use super::helpers::now_ms;
use super::Engine;
use crate::runtime::protocol::{PrLink, PrStatus};

/// Most links kept per workspace (oldest evicted). A workspace with more than
/// a handful of open PRs is a workspace, not a chip.
pub(crate) const MAX_PR_LINKS: usize = 8;

/// Most dismissals remembered per workspace (oldest evicted). Dismissals are
/// tombstones against a *re-scrape*, so the set only has to outlive the pane
/// output that keeps re-emitting the url — a few dozen is generous.
pub(crate) const MAX_PR_DISMISSED: usize = 32;

/// Does this status change count as a *verdict transition* worth resurfacing
/// the circle for?
///
/// A transition is a change in `attention` or `label` — the two fields a human
/// reads off the chip. Two deliberate non-bumps:
/// * the boot backfill, where a link that had **no** status gains a neutral one
///   (`attention: None`) — otherwise every restart reshuffles the sidebar;
/// * status loss (poller drops the URL), which is bookkeeping, not news.
///
/// A first status that already asks for attention DOES bump: that is news.
fn pr_verdict_transition(prev: Option<&PrStatus>, next: Option<&PrStatus>) -> bool {
    let Some(next) = next else {
        return false;
    };
    match prev {
        None => next.attention.is_some(),
        Some(prev) => prev.attention != next.attention || prev.label != next.label,
    }
}

impl Engine {
    /// Record a scraped (or hand-seeded) URL on a workspace.
    ///
    /// Re-seeing a URL moves it to most-recent and refreshes `seen_ms` without
    /// disturbing its poller status. Returns true when the list changed in a
    /// way clients should see.
    pub(crate) fn record_pr_link(&mut self, workspace: &str, url: &str, seen_ms: u64) -> bool {
        if self.pr_link_dismissed(workspace, url) {
            return false;
        }
        let links = self.pr_links.entry(workspace.to_string()).or_default();
        if let Some(pos) = links.iter().position(|l| l.url == url) {
            let mut link = links.remove(pos);
            // After the remove, `links.len()` is the index the element would
            // land on — equal means it was already most-recent.
            let reordered = pos != links.len();
            link.seen_ms = seen_ms;
            links.push(link);
            return reordered;
        }
        links.push(PrLink {
            url: url.to_string(),
            status: None,
            seen_ms,
        });
        while links.len() > MAX_PR_LINKS {
            links.remove(0);
        }
        true
    }

    /// Is this URL tombstoned on this workspace?
    pub(crate) fn pr_link_dismissed(&self, workspace: &str, url: &str) -> bool {
        self.pr_dismissed
            .get(workspace)
            .is_some_and(|d| d.iter().any(|u| u == url))
    }

    /// Tombstone a URL on a workspace (most recent LAST, oldest evicted).
    pub(crate) fn dismiss_pr_link(&mut self, workspace: &str, url: &str) {
        let set = self.pr_dismissed.entry(workspace.to_string()).or_default();
        if let Some(pos) = set.iter().position(|u| u == url) {
            set.remove(pos);
        }
        set.push(url.to_string());
        while set.len() > MAX_PR_DISMISSED {
            set.remove(0);
        }
    }

    /// Lift a tombstone — an explicit `pr-link add` always beats a past clear.
    pub(crate) fn undismiss_pr_link(&mut self, workspace: &str, url: &str) {
        let Some(set) = self.pr_dismissed.get_mut(workspace) else {
            return;
        };
        set.retain(|u| u != url);
        if set.is_empty() {
            self.pr_dismissed.remove(workspace);
        }
    }

    /// `pr-link clear`: drop one URL, or the whole workspace list.
    ///
    /// Every removed URL is also **dismissed** for that workspace. Without
    /// that, the next TUI repaint re-scrapes the identical url and the human's
    /// clear undoes itself within seconds.
    pub(crate) fn clear_pr_links(&mut self, workspace: &str, url: Option<&str>) -> usize {
        // A clear of a url we no longer hold is still a dismissal — the human
        // said "not this one", and the scraper may be about to re-emit it.
        let dismiss: Vec<String> = match url {
            Some(u) => vec![u.to_string()],
            None => self
                .pr_links
                .get(workspace)
                .map(|l| l.iter().map(|l| l.url.clone()).collect())
                .unwrap_or_default(),
        };
        let removed = match self.pr_links.get_mut(workspace) {
            Some(links) => {
                let before = links.len();
                match url {
                    Some(u) => links.retain(|l| l.url != u),
                    None => links.clear(),
                }
                let removed = before - links.len();
                if links.is_empty() {
                    self.pr_links.remove(workspace);
                }
                removed
            }
            None => 0,
        };
        for u in dismiss {
            self.dismiss_pr_link(workspace, &u);
        }
        removed
    }

    /// Merge a poller snapshot (`pr_watch.json` body) onto the known links.
    ///
    /// Unknown URLs are ignored — the daemon's scrape list is the source of
    /// truth for *which* PRs belong to a workspace. Returns true if anything
    /// changed (the caller pushes state only then).
    pub(crate) fn ingest_pr_watch(&mut self, watch: &HashMap<String, PrStatus>) -> bool {
        let mut changed = false;
        let mut bumped: Vec<String> = Vec::new();
        for (ws, links) in self.pr_links.iter_mut() {
            for link in links.iter_mut() {
                let next = watch.get(&link.url);
                if link.status.as_ref() == next {
                    continue;
                }
                if pr_verdict_transition(link.status.as_ref(), next) && !bumped.contains(ws) {
                    bumped.push(ws.clone());
                }
                link.status = next.cloned();
                changed = true;
            }
        }
        let now = now_ms();
        for ws in bumped {
            // Same clock the sidebar's idle band sorts + displays, so a changed
            // verdict floats the circle.
            self.workspace_output.insert(ws, now);
        }
        changed
    }

    /// Handle a scraped URL from a pane: attribute it to the pane's workspace.
    pub(crate) fn on_pr_link_seen(&mut self, slug: &str, url: &str) {
        let Some(ws) = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| p.workspace.clone())
        else {
            return;
        };
        if self.record_pr_link(&ws, url, now_ms()) {
            self.persist();
            self.push_state_to_all();
        }
    }

    /// Drop a workspace's row when its last pane dies and it was never
    /// explicitly created (`extra_workspaces`). An empty circle nobody asked
    /// for is dead chrome in both sidebars.
    pub(crate) fn prune_workspace_if_empty(&mut self, workspace: &str) {
        if workspace.is_empty()
            || self.extra_workspaces.iter().any(|w| w == workspace)
            || self.panes.iter().any(|p| p.workspace == workspace)
        {
            return;
        }
        self.forget_workspace(workspace);
    }

    /// Remove every trace of a workspace: order, clocks, links, subscriptions.
    pub(crate) fn forget_workspace(&mut self, workspace: &str) {
        self.workspace_order.retain(|w| w != workspace);
        self.workspace_output.remove(workspace);
        self.workspace_touch_ms.remove(workspace);
        self.pr_links.remove(workspace);
        self.pr_dismissed.remove(workspace);
        if self.selected_workspace.as_deref() == Some(workspace) {
            self.selected_workspace = self.panes.first().map(|p| p.workspace.clone());
        }
        self.drop_workspace_subs(workspace);
    }

    /// Rename follow-through for the link list.
    pub(crate) fn rename_pr_links(&mut self, old: &str, new: &str) {
        if let Some(links) = self.pr_links.remove(old) {
            self.pr_links.insert(new.to_string(), links);
        }
        if let Some(dismissed) = self.pr_dismissed.remove(old) {
            self.pr_dismissed.insert(new.to_string(), dismissed);
        }
    }
}
