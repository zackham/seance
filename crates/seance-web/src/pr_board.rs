//! PR board — the circle-first sweep view over every PR link the daemon knows.
//!
//! Two halves, deliberately separated:
//!
//! * **pure** — [`Board::build`] folds [`ClientState`]'s per-workspace PR links
//!   into ordered sections + rows (grouping, ordering, staleness, age labels,
//!   duplicate annotation, org-span). No DOM, no clocks of its own; the caller
//!   passes `now` in the daemon's **unix ms** domain, which is what every PR
//!   stamp on the wire uses. Every rule here has a hermetic test.
//! * **DOM** — a dimmed full-viewport overlay with one centered card, exactly
//!   the grimoire pattern ([`crate::help`]): built once, cached in a
//!   thread-local, content re-rendered on open, dismissed by backdrop click,
//!   the ✕, or Escape via `App::escape_topmost`.
//!
//! Circle-first is the product decision: a circle pinning several PRs is the
//! normal case, the same PR in two circles is a mistake — so duplicates get an
//! "also in <circle>" annotation rather than being deduplicated away.

// NEEDS web-sys feature: Node
//   (`Node::append_child`, via `Element: Deref<Target = Node>`, to mount the
//   overlay into `document.body`.) Everything else used here — `Window`,
//   `Document`, `Element`, `HtmlElement`, `CssStyleDeclaration`, `MouseEvent`
//   — is already enabled in crates/seance-web/Cargo.toml.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use seance_core::protocol::PrLink;

use crate::app_api::Actions;
use crate::state::{pr_number, ClientState};

/// A row is stale ("push or close") past this much silence.
const STALE_MS: f64 = 4.0 * 86_400_000.0;

// ── pure model ──────────────────────────────────────────────────────────────

/// One PR under one circle.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoardRow {
    pub url: String,
    /// `repo#12`, org-prefixed (`org/repo#12`) only when the client's links
    /// span more than one org.
    pub head: String,
    /// Poller's chip label (may be empty).
    pub label: String,
    /// `open` | `merged` | `closed` | … (empty = unknown, treated as open).
    pub state: String,
    pub is_draft: bool,
    /// `pass` | `fail` | `running` | None.
    pub ci: Option<String>,
    /// `required` | `approved` | `changes` | None.
    pub review: Option<String>,
    /// `needs` | `done` | None.
    pub attention: Option<String>,
    /// `3d` since open; empty when `opened_ms` is unknown.
    pub age: String,
    /// `review 2h` / `comment 5h`; empty when neither stamp is known.
    pub last_activity: String,
    /// Open, never reviewed, and quiet > 4d.
    pub stale: bool,
    /// merged/closed — muted, sorted to the bottom of its section.
    pub done: bool,
    /// Other circles carrying the same URL.
    pub also_in: Vec<String>,
}

impl BoardRow {
    /// `✓` pass · `✗` fail · `…` running · empty when the PR has no checks.
    pub fn ci_glyph(&self) -> &'static str {
        match self.ci.as_deref() {
            Some("pass") => "✓",
            Some("fail") => "✗",
            Some("running") => "…",
            _ => "",
        }
    }
}

/// One circle's section.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoardSection {
    pub circle: String,
    pub parked: bool,
    /// Any row wants a human — sorts this section to the top.
    pub needs: bool,
    /// Most recent PR activity in this circle (unix ms), the secondary sort.
    pub last_ms: u64,
    pub rows: Vec<BoardRow>,
}

/// The whole sweep.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Board {
    pub sections: Vec<BoardSection>,
    pub open: usize,
    pub drafts: usize,
    pub closed: usize,
}

impl Board {
    /// `N open · M drafts · K closed/merged`.
    pub fn header(&self) -> String {
        format!(
            "{} open · {} drafts · {} closed/merged",
            self.open, self.drafts, self.closed
        )
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Fold the client's PR links into the circle-first sweep. `now_unix_ms`
    /// is the daemon clock domain (`performance.now() + clock_offset_ms`).
    pub fn build(state: &ClientState, now_unix_ms: f64) -> Self {
        let circles = state.workspaces();
        let show_org = org_span_multi(state);

        // URL → circles carrying it (duplicate annotation).
        let mut owners: Vec<(String, Vec<String>)> = Vec::new();
        for ws in &circles {
            for l in state.pr_links(ws) {
                match owners.iter_mut().find(|(u, _)| u == &l.url) {
                    Some((_, list)) => list.push(ws.clone()),
                    None => owners.push((l.url.clone(), vec![ws.clone()])),
                }
            }
        }

        let mut board = Board::default();
        for ws in &circles {
            let links = state.pr_links(ws);
            if links.is_empty() {
                continue;
            }
            let mut rows: Vec<BoardRow> = Vec::new();
            let mut last_ms = 0u64;
            let mut needs = false;
            for link in links {
                let also_in: Vec<String> = owners
                    .iter()
                    .find(|(u, _)| u == &link.url)
                    .map(|(_, list)| list.iter().filter(|c| *c != ws).cloned().collect())
                    .unwrap_or_default();
                let row = row_of(link, show_org, also_in, now_unix_ms);
                if row.done {
                    board.closed += 1;
                } else {
                    if row.attention.as_deref() == Some("needs") {
                        needs = true;
                    }
                    board.open += 1;
                    if row.is_draft {
                        board.drafts += 1;
                    }
                }
                last_ms = last_ms.max(activity_ms(link));
                rows.push(row);
            }
            // Live rows first (needs on top, freshest first), done rows muted
            // at the bottom.
            rows.sort_by(|a, b| {
                let key = |r: &BoardRow| {
                    (
                        r.done,
                        u8::from(r.attention.as_deref() != Some("needs")),
                        std::cmp::Reverse(r.stale),
                    )
                };
                key(a).cmp(&key(b))
            });
            board.sections.push(BoardSection {
                circle: ws.clone(),
                parked: !state.subs.is_active(ws),
                needs,
                last_ms,
                rows,
            });
        }

        // Circles wanting a human first, then most recent PR activity, name
        // as the stable tiebreak.
        board.sections.sort_by(|a, b| {
            (!a.needs, std::cmp::Reverse(a.last_ms), &a.circle).cmp(&(
                !b.needs,
                std::cmp::Reverse(b.last_ms),
                &b.circle,
            ))
        });
        board
    }
}

/// Count for the `PRs (N)` affordance: links that are neither merged nor
/// closed, across every circle.
pub fn open_count(state: &ClientState) -> usize {
    state
        .workspaces()
        .iter()
        .flat_map(|ws| state.pr_links(ws))
        .filter(|l| !is_done(l))
        .count()
}

fn is_done(link: &PrLink) -> bool {
    matches!(
        link.status.as_ref().map(|s| s.state.to_ascii_lowercase()),
        Some(s) if s == "merged" || s == "closed"
    )
}

/// Freshest stamp on a link (unix ms), used for section ordering.
fn activity_ms(link: &PrLink) -> u64 {
    let Some(st) = link.status.as_ref() else {
        return link.seen_ms;
    };
    st.last_review_ms
        .max(st.last_comment_ms)
        .max(st.opened_ms)
        .max(st.updated_ms)
        .max(link.seen_ms)
}

fn row_of(link: &PrLink, show_org: bool, also_in: Vec<String>, now: f64) -> BoardRow {
    let status = link.status.as_ref();
    let state = status.map(|s| s.state.clone()).unwrap_or_default();
    let done = is_done(link);
    let opened_ms = status.map(|s| s.opened_ms).unwrap_or(0);
    let review_ms = status.map(|s| s.last_review_ms).unwrap_or(0);
    let comment_ms = status.map(|s| s.last_comment_ms).unwrap_or(0);
    let review = status.and_then(|s| s.review.clone());

    let age = if opened_ms == 0 {
        String::new()
    } else {
        age_label(now - opened_ms as f64)
    };
    let last_activity = if review_ms >= comment_ms && review_ms > 0 {
        format!("review {}", age_label(now - review_ms as f64))
    } else if comment_ms > 0 {
        format!("comment {}", age_label(now - comment_ms as f64))
    } else {
        String::new()
    };

    // "push or close": open, nobody has reviewed, and quiet for > 4d — the
    // quiet clock is the last human touch when there is one, else the age.
    // Unreviewed means review absent OR "required" (github reports the
    // latter for every open PR nobody has looked at; native matches).
    let unreviewed = match review.as_deref() {
        None | Some("required") => true,
        Some(_) => false,
    };
    let quiet_from = review_ms.max(comment_ms).max(opened_ms);
    let stale = !done
        && (state.is_empty() || state.eq_ignore_ascii_case("open"))
        && unreviewed
        && quiet_from > 0
        && (now - quiet_from as f64) > STALE_MS;

    BoardRow {
        url: link.url.clone(),
        head: pr_head(&link.url, show_org),
        label: status
            .map(|s| s.label.trim().to_string())
            .unwrap_or_default(),
        state,
        is_draft: status.is_some_and(|s| s.is_draft),
        ci: status.and_then(|s| s.ci.clone()),
        review,
        attention: status.and_then(|s| s.attention.clone()),
        age,
        last_activity,
        stale,
        done,
        also_in,
    }
}

/// `…/{org}/{repo}/pull/{n}` → `repo#n` (or `org/repo#n`). Falls back to the
/// bare `#n`, then to `PR`, so a non-github URL never renders as nothing.
pub fn pr_head(url: &str, show_org: bool) -> String {
    let num = pr_number(url)
        .map(|n| format!("#{n}"))
        .unwrap_or_else(|| "PR".to_string());
    match pr_repo(url) {
        Some((org, repo)) if show_org => format!("{org}/{repo}{num}"),
        Some((_, repo)) => format!("{repo}{num}"),
        None => num,
    }
}

/// `(org, repo)` from the two path segments before `/pull/`.
pub fn pr_repo(url: &str) -> Option<(String, String)> {
    let head = url.split("/pull/").next()?;
    let mut segs = head.rsplit('/');
    let repo = segs.next().filter(|s| !s.is_empty())?;
    let org = segs.next().filter(|s| !s.is_empty())?;
    if org.contains('.') || !url.contains("/pull/") {
        // `host/repo/pull/1` — no org segment to speak of.
        return None;
    }
    Some((org.to_string(), repo.to_string()))
}

/// Do this client's links span more than one org? Only then does the chip (and
/// every board row) carry the org prefix.
pub fn org_span_multi(state: &ClientState) -> bool {
    let mut seen: Vec<String> = Vec::new();
    for ws in state.workspaces() {
        for l in state.pr_links(&ws) {
            if let Some((org, _)) = pr_repo(&l.url) {
                if !seen.iter().any(|o| *o == org) {
                    seen.push(org);
                    if seen.len() > 1 {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Coarse one-unit age (`now`, `42m`, `2h`, `3d`) — a glance, not a stopwatch.
pub fn age_label(delta_ms: f64) -> String {
    let s = (delta_ms / 1000.0).max(0.0) as u64;
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Minimal HTML escape — labels are poller-authored and must never author DOM.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The board's inner HTML (header + sections), pure so it is testable without
/// a document.
pub fn board_html(board: &Board) -> String {
    if board.is_empty() {
        return r#"<div class="prb-empty">no PR links in any circle</div>"#.to_string();
    }
    let mut out = String::with_capacity(512);
    for sec in &board.sections {
        out.push_str(r#"<div class="prb-circle"><div class="prb-circle-head" data-ws=""#);
        out.push_str(&esc(&sec.circle));
        out.push_str(r#"" title="select this circle"><span class="prb-circle-name">"#);
        out.push_str(&esc(&sec.circle));
        out.push_str("</span>");
        if sec.needs {
            out.push_str(r#"<span class="prb-circle-needs">needs</span>"#);
        }
        out.push_str(r#"<span class="prb-circle-state">"#);
        out.push_str(if sec.parked { "parked" } else { "active" });
        out.push_str("</span></div>");
        for row in &sec.rows {
            out.push_str(&row_html(row));
        }
        out.push_str("</div>");
    }
    out
}

fn row_html(row: &BoardRow) -> String {
    let mut cls = String::from("prb-row");
    match row.attention.as_deref() {
        Some("needs") => cls.push_str(" prb-needs"),
        Some("done") => cls.push_str(" prb-done-att"),
        _ => {}
    }
    if row.done {
        cls.push_str(" prb-muted");
    }
    if row.stale {
        cls.push_str(" prb-stale");
    }
    let mut out = format!(
        r#"<div class="{}" data-url="{}" title="{}"><span class="prb-head">{}</span>"#,
        cls,
        esc(&row.url),
        esc(&row.url),
        esc(&row.head)
    );
    if row.is_draft {
        out.push_str(r#"<span class="prb-draft">draft</span>"#);
    }
    let glyph = row.ci_glyph();
    if !glyph.is_empty() {
        out.push_str(&format!(
            r#"<span class="prb-ci prb-ci-{}">{glyph}</span>"#,
            row.ci.as_deref().unwrap_or("")
        ));
    }
    if let Some(rev) = row.review.as_deref() {
        out.push_str(&format!(
            r#"<span class="prb-review prb-review-{}">{}</span>"#,
            esc(rev),
            esc(rev)
        ));
    }
    if !row.label.is_empty() {
        out.push_str(&format!(
            r#"<span class="prb-label">{}</span>"#,
            esc(&row.label)
        ));
    }
    if row.done && !row.state.is_empty() && !row.state.eq_ignore_ascii_case(&row.label) {
        out.push_str(&format!(
            r#"<span class="prb-state">{}</span>"#,
            esc(&row.state)
        ));
    }
    out.push_str(r#"<span class="prb-spacer"></span>"#);
    if !row.also_in.is_empty() {
        out.push_str(&format!(
            r#"<span class="prb-also">also in {}</span>"#,
            esc(&row.also_in.join(", "))
        ));
    }
    if row.stale {
        out.push_str(r#"<span class="prb-stale-mark" title="open, unreviewed, quiet &gt; 4d">push or close</span>"#);
    }
    if !row.last_activity.is_empty() {
        out.push_str(&format!(
            r#"<span class="prb-last">{}</span>"#,
            esc(&row.last_activity)
        ));
    }
    if !row.age.is_empty() {
        out.push_str(&format!(
            r#"<span class="prb-age">{}</span>"#,
            esc(&row.age)
        ));
    }
    out.push_str("</div>");
    out
}

// ── DOM ─────────────────────────────────────────────────────────────────────

thread_local! {
    /// The one board. `None` until first open.
    static BOARD: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}

struct Overlay {
    root: web_sys::Element,
    body: web_sys::Element,
    head: web_sys::Element,
    open: bool,
    /// Row/header/backdrop click handler, kept alive for the overlay's life.
    _click: Closure<dyn FnMut(web_sys::MouseEvent)>,
    /// Latest actions handle; rebound on every toggle.
    actions: Rc<RefCell<Option<Rc<dyn Actions>>>>,
}

/// Open the board if closed, close it if open.
pub fn toggle(state: &ClientState, now_unix_ms: f64, actions: Rc<dyn Actions>) {
    let built = BOARD.with(|slot| slot.borrow().is_some());
    if !built {
        build();
    }
    let now_open = BOARD.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(b) = slot.as_mut() else { return false };
        *b.actions.borrow_mut() = Some(actions);
        b.open = !b.open;
        set_display(&b.root, b.open);
        b.open
    });
    if now_open {
        render(state, now_unix_ms);
    }
}

pub fn is_open() -> bool {
    BOARD.with(|slot| slot.borrow().as_ref().is_some_and(|b| b.open))
}

/// Re-render while open; no-op otherwise.
pub fn refresh(state: &ClientState, now_unix_ms: f64) {
    if is_open() {
        render(state, now_unix_ms);
    }
}

fn render(state: &ClientState, now_unix_ms: f64) {
    let board = Board::build(state, now_unix_ms);
    let html = board_html(&board);
    let header = board.header();
    BOARD.with(|slot| {
        let slot = slot.borrow();
        let Some(b) = slot.as_ref() else { return };
        b.head.set_text_content(Some(&header));
        b.body.set_inner_html(&html);
        b.body.set_scroll_top(0);
    });
}

/// Close if open; true when something was closed.
pub fn close() -> bool {
    BOARD.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(b) = slot.as_mut() else { return false };
        if !b.open {
            return false;
        }
        b.open = false;
        set_display(&b.root, false);
        true
    })
}

fn set_display(root: &web_sys::Element, open: bool) {
    let Some(el) = root.dyn_ref::<web_sys::HtmlElement>() else {
        return;
    };
    let _ = el
        .style()
        .set_property("display", if open { "flex" } else { "none" });
}

fn build() {
    let Some(win) = web_sys::window() else { return };
    let Some(doc) = win.document() else { return };
    let Some(body) = doc.body() else { return };

    let Ok(root) = doc.create_element("div") else {
        return;
    };
    root.set_id("pr-board");
    root.set_inner_html(
        r#"<div id="pr-board-card"><div id="pr-board-head"><span class="prb-title">PR board</span>
<span id="pr-board-counts"></span><span class="prb-hspacer"></span>
<span id="pr-board-close" title="close (escape)">✕</span></div>
<div id="pr-board-list"></div></div>"#,
    );
    set_display(&root, false);

    let (Some(list), Some(counts)) = (
        root.query_selector("#pr-board-list").ok().flatten(),
        root.query_selector("#pr-board-counts").ok().flatten(),
    ) else {
        return;
    };

    let actions: Rc<RefCell<Option<Rc<dyn Actions>>>> = Rc::new(RefCell::new(None));
    let click = {
        let actions = actions.clone();
        Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            let Some(target) = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            else {
                return;
            };
            let hit = |sel: &str| target.closest(sel).ok().flatten();
            if hit("#pr-board-close").is_some() || hit("#pr-board-card").is_none() {
                ev.stop_propagation();
                close();
                return;
            }
            if let Some(head) = hit(".prb-circle-head") {
                if let Some(ws) = head.get_attribute("data-ws") {
                    ev.stop_propagation();
                    if let Some(a) = actions.borrow().as_ref() {
                        a.select_workspace(&ws);
                    }
                    close();
                }
                return;
            }
            if let Some(row) = hit(".prb-row") {
                if let Some(url) = row.get_attribute("data-url") {
                    ev.stop_propagation();
                    crate::ui::open_url(&url);
                }
            }
        })
    };
    let _ = root.add_event_listener_with_callback("mousedown", click.as_ref().unchecked_ref());

    let node: &web_sys::Node = root.unchecked_ref();
    if body.append_child(node).is_err() {
        return;
    }

    BOARD.with(|slot| {
        *slot.borrow_mut() = Some(Overlay {
            root,
            body: list,
            head: counts,
            open: false,
            _click: click,
            actions,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use seance_core::protocol::PrStatus;

    const DAY: f64 = 86_400_000.0;
    const NOW: f64 = 100.0 * DAY;

    fn link(url: &str, st: PrStatus) -> PrLink {
        PrLink {
            url: url.into(),
            status: Some(st),
            seen_ms: 1,
        }
    }

    fn open_pr(opened_days: f64) -> PrStatus {
        PrStatus {
            state: "open".into(),
            opened_ms: (NOW - opened_days * DAY) as u64,
            ..Default::default()
        }
    }

    fn state_with(links: &[(&str, Vec<PrLink>)]) -> ClientState {
        let mut st = ClientState::default();
        for (ws, ls) in links {
            st.workspace_order.push((*ws).to_string());
            st.workspace_pr_links.insert((*ws).to_string(), ls.clone());
            st.subs.activate(ws);
        }
        st
    }

    #[test]
    fn head_uses_repo_and_org_only_when_spanning() {
        assert_eq!(pr_head("https://github.com/o/r/pull/42", false), "r#42");
        assert_eq!(pr_head("https://github.com/o/r/pull/42", true), "o/r#42");
        assert_eq!(pr_head("https://example.com/whatever", false), "PR");
    }

    #[test]
    fn org_span_flips_only_with_two_orgs() {
        let st = state_with(&[(
            "raid",
            vec![
                link("https://github.com/o/r/pull/1", open_pr(1.0)),
                link("https://github.com/o/other/pull/2", open_pr(1.0)),
            ],
        )]);
        assert!(!org_span_multi(&st));
        let st = state_with(&[
            (
                "raid",
                vec![link("https://github.com/o/r/pull/1", open_pr(1.0))],
            ),
            (
                "lab",
                vec![link("https://github.com/two/r/pull/2", open_pr(1.0))],
            ),
        ]);
        assert!(org_span_multi(&st));
    }

    #[test]
    fn age_and_last_activity_labels() {
        let mut s = open_pr(3.0);
        s.last_comment_ms = (NOW - 5.0 * 3_600_000.0) as u64;
        let st = state_with(&[("raid", vec![link("https://github.com/o/r/pull/7", s)])]);
        let row = &Board::build(&st, NOW).sections[0].rows[0];
        assert_eq!(row.age, "3d");
        assert_eq!(row.last_activity, "comment 5h");

        let mut s = open_pr(3.0);
        s.last_review_ms = (NOW - 2.0 * 3_600_000.0) as u64;
        s.last_comment_ms = (NOW - 9.0 * 3_600_000.0) as u64;
        let st = state_with(&[("raid", vec![link("https://github.com/o/r/pull/7", s)])]);
        assert_eq!(
            Board::build(&st, NOW).sections[0].rows[0].last_activity,
            "review 2h"
        );
    }

    #[test]
    fn stale_needs_open_unreviewed_and_quiet() {
        let st = state_with(&[(
            "raid",
            vec![link("https://github.com/o/r/pull/1", open_pr(6.0))],
        )]);
        assert!(Board::build(&st, NOW).sections[0].rows[0].stale);

        // Fresh enough.
        let st = state_with(&[(
            "raid",
            vec![link("https://github.com/o/r/pull/1", open_pr(2.0))],
        )]);
        assert!(!Board::build(&st, NOW).sections[0].rows[0].stale);

        // Reviewed → never stale.
        let mut s = open_pr(9.0);
        s.review = Some("approved".into());
        let st = state_with(&[("raid", vec![link("https://github.com/o/r/pull/1", s)])]);
        assert!(!Board::build(&st, NOW).sections[0].rows[0].stale);

        // review "required" = unreviewed (github reports it for every open
        // PR nobody looked at) → still stale when quiet.
        let mut s = open_pr(9.0);
        s.review = Some("required".into());
        let st = state_with(&[("raid", vec![link("https://github.com/o/r/pull/1", s)])]);
        assert!(Board::build(&st, NOW).sections[0].rows[0].stale);

        // Recent comment quiets the clock.
        let mut s = open_pr(9.0);
        s.last_comment_ms = (NOW - 3_600_000.0) as u64;
        let st = state_with(&[("raid", vec![link("https://github.com/o/r/pull/1", s)])]);
        assert!(!Board::build(&st, NOW).sections[0].rows[0].stale);

        // Merged → never stale, and muted at the bottom.
        let mut s = open_pr(9.0);
        s.state = "merged".into();
        let st = state_with(&[("raid", vec![link("https://github.com/o/r/pull/1", s)])]);
        let row = &Board::build(&st, NOW).sections[0].rows[0];
        assert!(!row.stale && row.done);
    }

    #[test]
    fn counts_split_open_drafts_and_closed() {
        let mut draft = open_pr(1.0);
        draft.is_draft = true;
        let mut merged = open_pr(1.0);
        merged.state = "merged".into();
        let st = state_with(&[(
            "raid",
            vec![
                link("https://github.com/o/r/pull/1", open_pr(1.0)),
                link("https://github.com/o/r/pull/2", draft),
                link("https://github.com/o/r/pull/3", merged),
            ],
        )]);
        let b = Board::build(&st, NOW);
        assert_eq!((b.open, b.drafts, b.closed), (2, 1, 1));
        assert_eq!(b.header(), "2 open · 1 drafts · 1 closed/merged");
        assert_eq!(open_count(&st), 2);
    }

    #[test]
    fn sections_put_needs_first_then_recent_activity() {
        let mut needy = open_pr(1.0);
        needy.attention = Some("needs".into());
        let mut fresh = open_pr(1.0);
        fresh.last_comment_ms = (NOW - 60_000.0) as u64;
        let st = state_with(&[
            (
                "quiet",
                vec![link("https://github.com/o/r/pull/1", open_pr(30.0))],
            ),
            ("busy", vec![link("https://github.com/o/r/pull/2", fresh)]),
            ("hot", vec![link("https://github.com/o/r/pull/3", needy)]),
        ]);
        let b = Board::build(&st, NOW);
        let order: Vec<&str> = b.sections.iter().map(|s| s.circle.as_str()).collect();
        assert_eq!(order, vec!["hot", "busy", "quiet"]);
        assert!(b.sections[0].needs);
    }

    #[test]
    fn rows_sort_needs_first_and_done_last() {
        let mut needy = open_pr(1.0);
        needy.attention = Some("needs".into());
        let mut merged = open_pr(1.0);
        merged.state = "merged".into();
        let st = state_with(&[(
            "raid",
            vec![
                link("https://github.com/o/r/pull/1", merged),
                link("https://github.com/o/r/pull/2", open_pr(1.0)),
                link("https://github.com/o/r/pull/3", needy),
            ],
        )]);
        let rows = &Board::build(&st, NOW).sections[0].rows;
        assert_eq!(rows[0].head, "r#3");
        assert_eq!(rows[1].head, "r#2");
        assert!(rows[2].done);
    }

    #[test]
    fn duplicate_url_annotates_both_circles() {
        let url = "https://github.com/o/r/pull/9";
        let st = state_with(&[
            ("raid", vec![link(url, open_pr(1.0))]),
            ("lab", vec![link(url, open_pr(1.0))]),
        ]);
        let b = Board::build(&st, NOW);
        for sec in &b.sections {
            assert_eq!(
                sec.rows[0].also_in.len(),
                1,
                "{} lacks annotation",
                sec.circle
            );
        }
    }

    #[test]
    fn parked_circles_are_marked_and_included() {
        let mut st = state_with(&[(
            "raid",
            vec![link("https://github.com/o/r/pull/1", open_pr(1.0))],
        )]);
        st.subs.park("raid");
        let b = Board::build(&st, NOW);
        assert!(b.sections[0].parked);
    }

    #[test]
    fn ci_glyphs_and_escaped_html() {
        let mut s = open_pr(1.0);
        s.ci = Some("fail".into());
        s.review = Some("changes".into());
        s.label = "<script>".into();
        let st = state_with(&[("raid", vec![link("https://github.com/o/r/pull/1", s)])]);
        let b = Board::build(&st, NOW);
        assert_eq!(b.sections[0].rows[0].ci_glyph(), "✗");
        let html = board_html(&b);
        assert!(html.contains("prb-ci-fail"));
        assert!(html.contains("prb-review-changes"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains(r#"data-ws="raid""#));
        assert!(html.contains(r#"data-url="https://github.com/o/r/pull/1""#));
    }

    #[test]
    fn empty_board_says_so() {
        let b = Board::build(&ClientState::default(), NOW);
        assert!(b.is_empty());
        assert!(board_html(&b).contains("no PR links"));
    }
}
