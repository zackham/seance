//! The PR board: a full-content overlay (same pattern as `overview.rs`) that
//! sweeps every circle's PR links in one view — circle-first, needs-first,
//! stale PRs marked so "push or close" is obvious at a glance.
//!
//! Everything that decides *what* is shown (grouping, ordering, staleness,
//! age/activity wording, duplicate annotation, org-span detection) is pure and
//! unit-tested below; the render half only paints the result.

use gpui::{div, prelude::*, px, Context, SharedString};

use seance_core::protocol::PrLink;

use gpui_component::StyledExt as _;

use crate::theme::SeancePalette;

use super::util::{now_ms, tip, tip_s};
use super::workspaces::rel_label;
use super::SeanceApp;

/// Four days of quiet is the "push or close" cue.
const STALE_MS: u64 = 4 * 86_400_000;

/// `(org, repo)` out of a github PR URL — the two path segments before
/// `/pull/`. None when the URL isn't shaped like one.
pub(super) fn repo_slug(url: &str) -> Option<(&str, &str)> {
    let (head, _) = url.split_once("/pull/")?;
    let mut it = head.rsplit('/');
    let repo = it.next().filter(|s| !s.is_empty())?;
    let org = it.next().filter(|s| !s.is_empty())?;
    if org.contains(':') || repo.contains(':') {
        return None;
    }
    Some((org, repo))
}

/// True when the client's current link set spans more than one org — the only
/// case where chips/rows earn the `org/` prefix.
pub(super) fn spans_multiple_orgs<'a>(urls: impl IntoIterator<Item = &'a str>) -> bool {
    let mut seen: Option<&str> = None;
    for u in urls {
        let Some((org, _)) = repo_slug(u) else {
            continue;
        };
        match seen {
            Some(s) if s != org => return true,
            Some(_) => {}
            None => seen = Some(org),
        }
    }
    false
}

/// `repo#N` (or `org/repo#N` when the set spans orgs). Falls back to `#N`, then
/// `PR`, when the URL doesn't parse.
pub(super) fn repo_ref(url: &str, with_org: bool) -> String {
    let num = super::prlinks::pr_number(url);
    match (repo_slug(url), num) {
        (Some((org, repo)), Some(n)) if with_org => format!("{org}/{repo}#{n}"),
        (Some((_, repo)), Some(n)) => format!("{repo}#{n}"),
        (_, Some(n)) => format!("#{n}"),
        _ => "PR".into(),
    }
}

/// A link is "live" (counts toward the sidebar button) unless the poller has
/// called it merged or closed.
pub(super) fn is_live(link: &PrLink) -> bool {
    !matches!(state_of(link), "merged" | "closed")
}

fn state_of(link: &PrLink) -> &str {
    link.status.as_ref().map(|s| s.state.as_str()).unwrap_or("")
}

/// CI glyph for the row: ✓ pass, ✗ fail, … running, blank when no checks.
pub(super) fn ci_glyph(ci: Option<&str>) -> &'static str {
    match ci {
        Some("pass") => "✓",
        Some("fail") => "✗",
        Some("running") => "…",
        _ => "",
    }
}

/// `now - t` as a coarse label; None when the stamp is unknown (0) or ahead.
fn since(now: u64, t: u64) -> Option<String> {
    (t != 0 && now >= t).then(|| rel_label(now - t))
}

/// Latest human touch on the PR: `("review" | "comment", ms)`, review winning
/// ties. None when both stamps are unknown.
fn latest_touch(review_ms: u64, comment_ms: u64) -> Option<(&'static str, u64)> {
    match (review_ms, comment_ms) {
        (0, 0) => None,
        (r, c) if r >= c => Some(("review", r)),
        (_, c) => Some(("comment", c)),
    }
}

/// One PR under one circle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BoardRow {
    pub url: String,
    /// `repo#N` (org-prefixed only when the set spans orgs).
    pub reference: String,
    pub label: String,
    pub state: String,
    pub attention: Option<String>,
    pub is_draft: bool,
    pub ci: Option<String>,
    pub review: Option<String>,
    /// Age since open, e.g. `3d`. None when the poller didn't say.
    pub age: Option<String>,
    /// Last human touch, e.g. `review 2h`. None when unknown.
    pub activity: Option<String>,
    /// Open, unreviewed, and quiet for >4d.
    pub stale: bool,
    /// Merged/closed — muted, sorted to the bottom of its section.
    pub done: bool,
    /// Other circles pinning the same URL.
    pub also_in: Vec<String>,
    /// Sort key within the section (most recent signal first).
    recency_ms: u64,
}

/// One circle's slice of the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BoardSection {
    pub workspace: String,
    pub parked: bool,
    pub needs: bool,
    pub rows: Vec<BoardRow>,
}

fn row_stale(state: &str, review: Option<&str>, now: u64, opened: u64, touch: u64) -> bool {
    if state != "open" || matches!(review, Some("approved") | Some("changes")) {
        return false;
    }
    let quiet_from = if touch != 0 { touch } else { opened };
    if quiet_from == 0 {
        return false;
    }
    now.saturating_sub(quiet_from) > STALE_MS
}

/// Build the whole board. `input` is `(circle, parked, links)` for every circle
/// this client knows; ordering is needs-first, then most-recent PR activity.
pub(super) fn build_board(input: &[(String, bool, Vec<PrLink>)], now: u64) -> Vec<BoardSection> {
    let with_org = spans_multiple_orgs(
        input
            .iter()
            .flat_map(|(_, _, links)| links.iter().map(|l| l.url.as_str())),
    );
    // URL → circles pinning it (for the duplicate annotation).
    let mut owners: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (ws, _, links) in input {
        for l in links {
            let e = owners.entry(l.url.as_str()).or_default();
            if !e.contains(&ws.as_str()) {
                e.push(ws.as_str());
            }
        }
    }

    let mut sections: Vec<BoardSection> = Vec::new();
    for (ws, parked, links) in input {
        if links.is_empty() {
            continue;
        }
        let mut rows: Vec<BoardRow> = links
            .iter()
            .map(|l| {
                let st = l.status.as_ref();
                let state = st.map(|s| s.state.clone()).unwrap_or_default();
                let opened = st.map(|s| s.opened_ms).unwrap_or(0);
                let touch = latest_touch(
                    st.map(|s| s.last_review_ms).unwrap_or(0),
                    st.map(|s| s.last_comment_ms).unwrap_or(0),
                );
                let review = st.and_then(|s| s.review.clone());
                let touch_ms = touch.map(|(_, ms)| ms).unwrap_or(0);
                let also_in: Vec<String> = owners
                    .get(l.url.as_str())
                    .map(|v| {
                        v.iter()
                            .filter(|o| **o != ws.as_str())
                            .map(|o| o.to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                BoardRow {
                    url: l.url.clone(),
                    reference: repo_ref(&l.url, with_org),
                    label: st.map(|s| s.label.clone()).unwrap_or_default(),
                    attention: st.and_then(|s| s.attention.clone()),
                    is_draft: st.map(|s| s.is_draft).unwrap_or(false),
                    ci: st.and_then(|s| s.ci.clone()),
                    age: since(now, opened),
                    activity: touch
                        .and_then(|(word, ms)| since(now, ms).map(|rel| format!("{word} {rel}"))),
                    stale: row_stale(&state, review.as_deref(), now, opened, touch_ms),
                    done: matches!(state.as_str(), "merged" | "closed"),
                    recency_ms: touch_ms
                        .max(st.map(|s| s.updated_ms).unwrap_or(0))
                        .max(l.seen_ms),
                    review,
                    state,
                    also_in,
                }
            })
            .collect();
        // Live PRs first, then merged/closed; newest signal first within each.
        rows.sort_by_key(|r| (r.done, std::cmp::Reverse(r.recency_ms)));
        let needs = rows
            .iter()
            .any(|r| !r.done && r.attention.as_deref() == Some("needs"));
        sections.push(BoardSection {
            workspace: ws.clone(),
            parked: *parked,
            needs,
            rows,
        });
    }
    sections.sort_by_key(|s| {
        let recency = s.rows.iter().map(|r| r.recency_ms).max().unwrap_or(0);
        (!s.needs, std::cmp::Reverse(recency), s.workspace.clone())
    });
    sections
}

/// `N open · M drafts · K closed/merged` counts over a built board.
pub(super) fn board_counts(sections: &[BoardSection]) -> (usize, usize, usize) {
    let mut open = 0;
    let mut drafts = 0;
    let mut done = 0;
    for s in sections {
        for r in &s.rows {
            if r.done {
                done += 1;
            } else {
                open += 1;
                if r.is_draft {
                    drafts += 1;
                }
            }
        }
    }
    (open, drafts, done)
}

impl SeanceApp {
    /// `(circle, parked, links)` for every circle with PR links, in sidebar
    /// order — the board's raw input.
    fn pr_board_input(&self, cx: &gpui::App) -> Vec<(String, bool, Vec<PrLink>)> {
        let ordered = self.workspaces(cx);
        let (_, parked) = crate::subscriptions_pref::partition(&ordered, &self.subs_pref.active);
        ordered
            .into_iter()
            .filter_map(|ws| {
                let links = self.pr_links_for(&ws);
                (!links.is_empty()).then(|| {
                    let is_parked = parked.contains(&ws);
                    (ws, is_parked, links.to_vec())
                })
            })
            .collect()
    }

    /// Count for the sidebar button: live PRs across every circle, parked
    /// included. Zero hides the button.
    pub(super) fn pr_open_count(&self) -> usize {
        self.pr_links
            .values()
            .flat_map(|v| v.iter())
            .filter(|l| is_live(l))
            .count()
    }

    pub(super) fn set_pr_board(&mut self, on: bool, cx: &mut Context<Self>) {
        self.pr_board = on;
        cx.notify();
    }

    /// The sidebar affordance: `PRs (N)`, hidden when N is 0.
    pub(super) fn render_pr_board_button(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let n = self.pr_open_count();
        if n == 0 {
            return None;
        }
        let open = self.pr_board;
        Some(
            div()
                .id("pr-board-btn")
                .flex_none()
                .mx_2()
                .mb_1()
                .px_2()
                .py_1()
                .rounded_md()
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(if open {
                    SeancePalette::flame()
                } else {
                    SeancePalette::text_dim()
                })
                .bg(SeancePalette::surface())
                .cursor_pointer()
                .hover(|s| s.bg(SeancePalette::border()))
                .tooltip(tip("open PRs across every circle — click for the board"))
                .on_click(cx.listener(|this, _, _, cx| {
                    let on = !this.pr_board;
                    this.set_pr_board(on, cx);
                }))
                .child(format!("PRs ({n})"))
                .into_any_element(),
        )
    }

    pub(super) fn render_pr_board(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let sections = build_board(&self.pr_board_input(cx), now_ms());
        let (open, drafts, done) = board_counts(&sections);
        div()
            .id("pr-board")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(SeancePalette::bg())
            // Same guard as overview: dead-space clicks must not reach the
            // tiles underneath. A click on the backdrop closes the board.
            .occlude()
            .on_click(cx.listener(|this, _, _, cx| {
                this.set_pr_board(false, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .h(px(40.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(SeancePalette::border())
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(SeancePalette::text())
                            .child(format!(
                                "{open} open · {drafts} drafts · {done} closed/merged"
                            )),
                    )
                    .child(
                        div()
                            .id("pr-board-close")
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .cursor_pointer()
                            .tooltip(tip("close the PR board (esc · click outside)"))
                            .child("esc"),
                    ),
            )
            .child(
                div()
                    .id("pr-board-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(
                        sections
                            .into_iter()
                            .map(|s| self.render_pr_section(s, cx))
                            .collect::<Vec<_>>(),
                    ),
            )
    }

    fn render_pr_section(&self, section: BoardSection, cx: &Context<Self>) -> gpui::AnyElement {
        let ws = section.workspace.clone();
        let ws_click = ws.clone();
        let rows: Vec<gpui::AnyElement> = section.rows.into_iter().map(render_row).collect();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .id(SharedString::from(format!("pr-board-sec-{ws}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(SeancePalette::surface()))
                    .tooltip(tip("jump to this circle"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_pr_board(false, cx);
                        this.select_workspace(&ws_click, window, cx);
                        cx.stop_propagation();
                    }))
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(SeancePalette::text())
                            .child(ws.clone()),
                    )
                    .when(section.needs, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(SeancePalette::violet())
                                .child("needs"),
                        )
                    })
                    .when(section.parked, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(SeancePalette::text_faint())
                                .child("parked"),
                        )
                    }),
            )
            .children(rows)
            .into_any_element()
    }
}

/// Attention → row color; merged/closed rows are muted regardless.
fn row_color(row: &BoardRow) -> gpui::Hsla {
    if row.done {
        return SeancePalette::text_faint();
    }
    match row.attention.as_deref() {
        Some("needs") => SeancePalette::violet(),
        Some("done") => SeancePalette::success(),
        _ => SeancePalette::text_dim(),
    }
}

fn cell(text: String, color: gpui::Hsla) -> gpui::AnyElement {
    div()
        .flex_none()
        .text_xs()
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn render_row(row: BoardRow) -> gpui::AnyElement {
    let target = row.url.clone();
    let color = row_color(&row);
    let mut cells: Vec<gpui::AnyElement> = vec![cell(row.reference.clone(), color)];
    if row.is_draft {
        cells.push(cell("draft".into(), SeancePalette::text_faint()));
    }
    let glyph = ci_glyph(row.ci.as_deref());
    if !glyph.is_empty() {
        cells.push(cell(
            glyph.into(),
            match row.ci.as_deref() {
                Some("fail") => SeancePalette::danger(),
                Some("pass") => SeancePalette::success(),
                _ => SeancePalette::text_dim(),
            },
        ));
    }
    if let Some(review) = row.review.clone() {
        cells.push(cell(review, SeancePalette::text_dim()));
    }
    if !row.label.is_empty() {
        cells.push(cell(row.label.clone(), color));
    }
    if let Some(age) = row.age.clone() {
        cells.push(cell(age, SeancePalette::text_faint()));
    }
    if let Some(activity) = row.activity.clone() {
        cells.push(cell(activity, SeancePalette::text_faint()));
    }
    if row.done && !row.state.is_empty() {
        cells.push(cell(row.state.clone(), SeancePalette::text_faint()));
    }
    if row.stale {
        cells.push(cell("stale".into(), SeancePalette::danger()));
    }
    for other in &row.also_in {
        cells.push(cell(
            format!("also in {other}"),
            SeancePalette::text_faint(),
        ));
    }
    div()
        .id(SharedString::from(format!("pr-board-row-{}", row.url)))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_0p5()
        .rounded_md()
        .cursor_pointer()
        .when(row.done, |d| d.opacity(0.6))
        .hover(|s| s.bg(SeancePalette::surface()))
        .tooltip(tip_s(row.url.clone()))
        .on_click(move |_, _, cx| {
            crate::sysopen::open_detached(&target);
            cx.stop_propagation();
        })
        .children(cells)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use seance_core::protocol::PrStatus;

    const NOW: u64 = 1_000_000_000_000;
    const DAY: u64 = 86_400_000;
    const HOUR: u64 = 3_600_000;

    fn link(url: &str, st: PrStatus) -> PrLink {
        PrLink {
            url: url.into(),
            seen_ms: 1,
            status: Some(st),
        }
    }

    fn open_status() -> PrStatus {
        PrStatus {
            state: "open".into(),
            opened_ms: NOW - DAY,
            updated_ms: NOW,
            ..Default::default()
        }
    }

    #[test]
    fn repo_slug_and_org_span() {
        assert_eq!(
            repo_slug("https://github.com/zackham/vita/pull/12"),
            Some(("zackham", "vita"))
        );
        assert_eq!(
            repo_slug("https://github.com/o/r/pull/9/files"),
            Some(("o", "r"))
        );
        assert_eq!(repo_slug("https://example.com/nope"), None);
        assert!(!spans_multiple_orgs(vec![
            "https://github.com/o/a/pull/1",
            "https://github.com/o/b/pull/2",
        ]));
        assert!(spans_multiple_orgs(vec![
            "https://github.com/o/a/pull/1",
            "https://github.com/p/a/pull/2",
        ]));
        // Unparseable URLs never flip the decision on their own.
        assert!(!spans_multiple_orgs(vec![
            "https://github.com/o/a/pull/1",
            "garbage",
        ]));
    }

    #[test]
    fn repo_ref_prefixes_org_only_when_asked() {
        let u = "https://github.com/o/r/pull/7";
        assert_eq!(repo_ref(u, false), "r#7");
        assert_eq!(repo_ref(u, true), "o/r#7");
        assert_eq!(repo_ref("https://x/pull/7", false), "#7");
        assert_eq!(repo_ref("nonsense", false), "PR");
    }

    #[test]
    fn ci_glyphs_and_latest_touch() {
        assert_eq!(ci_glyph(Some("pass")), "✓");
        assert_eq!(ci_glyph(Some("fail")), "✗");
        assert_eq!(ci_glyph(Some("running")), "…");
        assert_eq!(ci_glyph(None), "");
        assert_eq!(latest_touch(0, 0), None);
        assert_eq!(latest_touch(5, 0), Some(("review", 5)));
        assert_eq!(latest_touch(0, 5), Some(("comment", 5)));
        assert_eq!(latest_touch(9, 3), Some(("review", 9)));
        assert_eq!(latest_touch(3, 9), Some(("comment", 9)));
    }

    #[test]
    fn staleness_needs_open_unreviewed_and_quiet() {
        // Quiet 5d, never reviewed → stale.
        assert!(row_stale("open", None, NOW, NOW - 5 * DAY, 0));
        assert!(row_stale("open", Some("required"), NOW, NOW - 9 * DAY, 0));
        // Recent comment resets the quiet clock even on an old PR.
        assert!(!row_stale("open", None, NOW, NOW - 9 * DAY, NOW - HOUR));
        // Reviewed → never stale.
        assert!(!row_stale("open", Some("approved"), NOW, NOW - 9 * DAY, 0));
        assert!(!row_stale("open", Some("changes"), NOW, NOW - 9 * DAY, 0));
        // Merged → never stale; unknown timestamps → never stale.
        assert!(!row_stale("merged", None, NOW, NOW - 9 * DAY, 0));
        assert!(!row_stale("open", None, NOW, 0, 0));
    }

    #[test]
    fn rows_carry_age_activity_and_duplicate_notes() {
        let mut st = open_status();
        st.opened_ms = NOW - 3 * DAY;
        st.last_comment_ms = NOW - 2 * HOUR;
        st.last_review_ms = NOW - 5 * HOUR;
        st.is_draft = true;
        let input = vec![
            (
                "alpha".into(),
                false,
                vec![link("https://github.com/o/r/pull/1", st)],
            ),
            (
                "beta".into(),
                true,
                vec![link("https://github.com/o/r/pull/1", open_status())],
            ),
        ];
        let board = build_board(&input, NOW);
        let a = board.iter().find(|s| s.workspace == "alpha").unwrap();
        let row = &a.rows[0];
        assert_eq!(row.reference, "r#1");
        assert_eq!(row.age.as_deref(), Some("3d"));
        assert_eq!(row.activity.as_deref(), Some("comment 2h"));
        assert!(row.is_draft);
        assert_eq!(row.also_in, vec!["beta".to_string()]);
        let b = board.iter().find(|s| s.workspace == "beta").unwrap();
        assert!(b.parked);
        assert_eq!(b.rows[0].also_in, vec!["alpha".to_string()]);
    }

    #[test]
    fn sections_order_needs_first_then_recency_and_skip_empties() {
        let mut needs = open_status();
        needs.attention = Some("needs".into());
        needs.updated_ms = NOW - 10 * DAY;
        let mut fresh = open_status();
        fresh.updated_ms = NOW;
        let input = vec![
            ("quiet".into(), false, vec![]),
            (
                "fresh".into(),
                false,
                vec![link("https://github.com/o/r/pull/2", fresh)],
            ),
            (
                "asks".into(),
                false,
                vec![link("https://github.com/o/r/pull/3", needs)],
            ),
        ];
        let board = build_board(&input, NOW);
        assert_eq!(
            board
                .iter()
                .map(|s| s.workspace.as_str())
                .collect::<Vec<_>>(),
            vec!["asks", "fresh"]
        );
        assert!(board[0].needs);
    }

    #[test]
    fn merged_rows_sink_and_counts_split() {
        let mut merged = open_status();
        merged.state = "merged".into();
        merged.updated_ms = NOW;
        let mut draft = open_status();
        draft.is_draft = true;
        draft.updated_ms = NOW - HOUR;
        let input = vec![(
            "one".into(),
            false,
            vec![
                link("https://github.com/o/r/pull/1", merged),
                link("https://github.com/o/r/pull/2", draft),
            ],
        )];
        let board = build_board(&input, NOW);
        let refs: Vec<&str> = board[0].rows.iter().map(|r| r.reference.as_str()).collect();
        assert_eq!(refs, vec!["r#2", "r#1"]);
        assert!(board[0].rows[1].done);
        assert!(!board[0].needs);
        assert_eq!(board_counts(&board), (1, 1, 1));
    }

    #[test]
    fn liveness_counts_unknown_status_as_open() {
        let plain = PrLink {
            url: "https://github.com/o/r/pull/1".into(),
            status: None,
            seen_ms: 1,
        };
        assert!(is_live(&plain));
        let mut closed = open_status();
        closed.state = "closed".into();
        assert!(!is_live(&link("https://github.com/o/r/pull/2", closed)));
    }
}
