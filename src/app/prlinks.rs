//! PR links surface: the header chip row for the selected workspace's scraped
//! GitHub PRs (one chip per link, most recent first — open on click, details
//! on hover, actions on right-click) and the pure helpers the sidebar
//! attention machinery folds in.
//!
//! The daemon owns the URL list (scraped from pane output) and an external
//! poller fills in `PrStatus` through `pr_watch.json`; this module is a pure
//! mirror + renderer — it never touches the network or the filesystem.

use gpui::{div, prelude::*, Context, SharedString};

use gpui_component::menu::ContextMenuExt as _;

use seance_core::protocol::PrLink;

use crate::runtime::protocol::GuiRequest;
use crate::theme::SeancePalette;

use super::actions::{ActClearPrLinks, ActOpenPrLink, ActRemovePrLink};
use super::prboard::{ci_glyph, latest_touch, repo_ref, since};
use super::util::now_ms;
use super::workspaces::WorkspaceAttention;
use super::SeanceApp;

/// Tooltip on every per-row remove ✕ (popover and board share it).
pub(super) const PR_REMOVE_TIP: &str =
    "remove this PR ref (stays removed; new links still tracked)";

/// The mirror after dropping one URL. Pure so the optimistic local update is
/// testable without a daemon.
pub(super) fn without_url(links: &[PrLink], url: &str) -> Vec<PrLink> {
    links.iter().filter(|l| l.url != url).cloned().collect()
}

/// PR number out of a canonical `…/pull/N` URL (trailing `/files`, `#anchor`
/// and `?query` tolerated). None when the URL isn't a PR link.
pub(super) fn pr_number(url: &str) -> Option<u64> {
    let tail = url.split("/pull/").nth(1)?;
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Attention contributed by a workspace's PR links: any `needs` wins, else any
/// `done`. Merging with pane/status attention happens in `workspaces.rs`.
pub(super) fn pr_attention(links: &[PrLink]) -> Option<WorkspaceAttention> {
    let mut done = false;
    for l in links {
        match l.status.as_ref().and_then(|s| s.attention.as_deref()) {
            Some("needs") => return Some(WorkspaceAttention::NeedsHuman),
            Some("done") => done = true,
            _ => {}
        }
    }
    done.then_some(WorkspaceAttention::Done)
}

/// Chip color for one link's attention (neutral when the poller hasn't spoken).
fn link_color(link: &PrLink) -> gpui::Hsla {
    match pr_attention(std::slice::from_ref(link)) {
        Some(a) => a.color(),
        None => SeancePalette::text_dim(),
    }
}

/// `repo#123 label` chip text — org-prefixed only when the client's current
/// link set spans more than one org.
fn chip_label(link: &PrLink, with_org: bool) -> String {
    let head = super::prboard::repo_ref(&link.url, with_org);
    let label = link.status.as_ref().map(|s| s.label.as_str()).unwrap_or("");
    if label.is_empty() {
        head
    } else {
        format!("{head} {label}")
    }
}

/// Multi-line hover tooltip for one PR chip: full URL, then the poller's read
/// (state/draft/ci/review) and the two clocks (age, last human touch). Pure —
/// the render half just paints these lines.
pub(super) fn pr_tooltip_lines(link: &PrLink, with_org: bool, now: u64) -> Vec<String> {
    let mut out = vec![repo_ref(&link.url, with_org), link.url.clone()];
    let Some(st) = link.status.as_ref() else {
        out.push("no poller status yet".into());
        return out;
    };
    let mut state = if st.state.is_empty() {
        "state unknown".to_string()
    } else {
        st.state.clone()
    };
    if st.is_draft {
        state.push_str(" · draft");
    }
    out.push(state);
    if let Some(ci) = st.ci.as_deref() {
        out.push(format!("ci {ci} {}", ci_glyph(Some(ci))).trim_end().into());
    }
    if let Some(review) = st.review.as_deref() {
        out.push(format!("review {review}"));
    }
    if let Some(age) = since(now, st.opened_ms) {
        out.push(format!("opened {age} ago"));
    }
    if let Some((word, ms)) = latest_touch(st.last_review_ms, st.last_comment_ms) {
        if let Some(rel) = since(now, ms) {
            out.push(format!("last {word} {rel} ago"));
        }
    }
    out
}

/// Tooltip builder for a set of lines (gpui-component `Tooltip::element`).
fn tip_lines_view(
    lines: Vec<String>,
) -> impl Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView + 'static {
    move |window, cx| {
        let lines = lines.clone();
        gpui_component::tooltip::Tooltip::element(move |_, _| {
            div()
                .flex()
                .flex_col()
                .children(lines.iter().map(|l| div().child(l.clone())))
        })
        .build(window, cx)
    }
}

impl SeanceApp {
    /// This window's mirror of the daemon-owned link list for `ws`.
    pub(super) fn pr_links_for(&self, ws: &str) -> &[PrLink] {
        self.pr_links.get(ws).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Does this client's whole link set span >1 github org? Decides whether
    /// chip/popover/board refs carry the `org/` prefix.
    pub(super) fn pr_links_span_orgs(&self) -> bool {
        super::prboard::spans_multiple_orgs(
            self.pr_links
                .values()
                .flat_map(|v| v.iter())
                .map(|l| l.url.as_str()),
        )
    }

    /// Header strip: one chip per PR link on the selected circle, most recent
    /// first. Left-click opens the PR, hover shows the details tooltip,
    /// right-click opens the per-chip context menu. Renders nothing at all when
    /// the circle has no links.
    pub(super) fn render_pr_chip(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let Some(ws) = self.selected_workspace.clone() else {
            return div().flex_none().into_any_element();
        };
        let links = self.pr_links_for(&ws);
        if links.is_empty() {
            return div().flex_none().into_any_element();
        }
        let with_org = self.pr_links_span_orgs();
        let now = now_ms();
        // Most recent link leftmost (the daemon appends newest last).
        let chips: Vec<gpui::AnyElement> = links
            .iter()
            .rev()
            .map(|l| self.render_pr_link_chip(&ws, l, with_org, now, cx))
            .collect();
        div()
            .id("pr-strip")
            .flex_none()
            .w_full()
            .px_1()
            .pt_1()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_1()
            .children(chips)
            .into_any_element()
    }

    /// One chip: open on click, details on hover, actions on right-click.
    fn render_pr_link_chip(
        &self,
        ws: &str,
        link: &PrLink,
        with_org: bool,
        now: u64,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let url = link.url.clone();
        let open_url = url.clone();
        let tip_lines = pr_tooltip_lines(link, with_org, now);
        let menu_url = url.clone();
        let menu_ws = ws.to_string();
        div()
            .id(SharedString::from(format!("pr-chip-{url}")))
            .flex_none()
            .px_2()
            .py_0p5()
            .rounded_md()
            .border_1()
            .border_color(SeancePalette::border())
            .bg(SeancePalette::surface())
            .text_xs()
            .text_color(link_color(link))
            .cursor_pointer()
            .hover(|s| s.bg(SeancePalette::border()))
            .tooltip(tip_lines_view(tip_lines))
            .on_click(cx.listener(move |_this, _, _, _| {
                crate::sysopen::open_detached(&open_url);
            }))
            .context_menu(move |menu, _, _| {
                menu.menu("open PR", Box::new(ActOpenPrLink(menu_url.clone())))
                    .menu(
                        PR_REMOVE_TIP,
                        Box::new(ActRemovePrLink {
                            url: menu_url.clone(),
                            workspace: menu_ws.clone(),
                        }),
                    )
                    .separator()
                    .menu(
                        "clear all PR refs",
                        Box::new(ActClearPrLinks(menu_ws.clone())),
                    )
            })
            .child(chip_label(link, with_org))
            .into_any_element()
    }

    /// Drop one PR ref from one circle: single-URL `pr-link clear`, which the
    /// daemon treats as a sticky dismissal (the scraper won't re-add it), plus
    /// the optimistic local mirror update. When the circle's last link goes,
    /// the mirror entry goes with it — chip and popover simply stop rendering.
    pub(super) fn remove_pr_link(&mut self, ws: &str, url: &str, cx: &mut Context<Self>) {
        let _ = self.client.send(GuiRequest::Ctl(
            seance_core::control::ControlRequest::PrLinkClear {
                url: Some(url.to_string()),
                workspace: Some(ws.to_string()),
                scope: None,
                from: Some("gui".into()),
            },
        ));
        let left = without_url(self.pr_links_for(ws), url);
        if left.is_empty() {
            self.pr_links.remove(ws);
        } else {
            self.pr_links.insert(ws.to_string(), left);
        }
        cx.notify();
    }

    /// `pr-link clear <workspace>` over the GUI's ctl seam. The daemon persists
    /// and pushes State back, which re-seeds our mirror.
    pub(super) fn clear_pr_links(&mut self, ws: &str, cx: &mut Context<Self>) {
        let _ = self.client.send(GuiRequest::Ctl(
            seance_core::control::ControlRequest::PrLinkClear {
                url: None,
                workspace: Some(ws.to_string()),
                scope: None,
                from: Some("gui".into()),
            },
        ));
        self.pr_links.remove(ws);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seance_core::protocol::PrStatus;

    fn link(url: &str, attention: Option<&str>) -> PrLink {
        PrLink {
            url: url.into(),
            status: attention.map(|a| PrStatus {
                state: "open".into(),
                attention: Some(a.into()),
                label: "CI ✗".into(),
                updated_ms: 1,
                ..Default::default()
            }),
            seen_ms: 1,
        }
    }

    #[test]
    fn pr_number_parses_canonical_and_suffixed_urls() {
        assert_eq!(
            pr_number("https://github.com/zackham/vita/pull/123"),
            Some(123)
        );
        assert_eq!(
            pr_number("https://github.com/o/r/pull/9/files#diff-abc"),
            Some(9)
        );
        assert_eq!(pr_number("https://github.com/o/r/pull/7?w=1"), Some(7));
        assert_eq!(pr_number("https://github.com/o/r/issues/7"), None);
        assert_eq!(pr_number("https://github.com/o/r/pull/abc"), None);
    }

    #[test]
    fn pr_attention_prefers_needs_over_done_and_ignores_unknown() {
        assert_eq!(pr_attention(&[]), None);
        assert_eq!(pr_attention(&[link("u/pull/1", None)]), None);
        assert_eq!(
            pr_attention(&[link("u/pull/1", Some("done"))]),
            Some(WorkspaceAttention::Done)
        );
        assert_eq!(
            pr_attention(&[
                link("u/pull/1", Some("done")),
                link("u/pull/2", Some("needs")),
            ]),
            Some(WorkspaceAttention::NeedsHuman)
        );
        assert_eq!(pr_attention(&[link("u/pull/1", Some("weird"))]), None);
    }

    #[test]
    fn tooltip_lines_carry_url_state_and_clocks() {
        let mut l = link("https://github.com/o/r/pull/42", Some("needs"));
        {
            let st = l.status.as_mut().unwrap();
            st.is_draft = true;
            st.ci = Some("fail".into());
            st.review = Some("changes".into());
            st.opened_ms = 1_000;
            st.last_review_ms = 2_000;
            st.last_comment_ms = 1_500;
        }
        let now = 2_000 + 3 * 3_600_000;
        assert_eq!(
            pr_tooltip_lines(&l, false, now),
            vec![
                "r#42".to_string(),
                "https://github.com/o/r/pull/42".into(),
                "open · draft".into(),
                "ci fail ✗".into(),
                "review changes".into(),
                format!(
                    "opened {} ago",
                    super::super::workspaces::rel_label(now - 1_000)
                ),
                "last review 3h ago".into(),
            ]
        );
    }

    #[test]
    fn tooltip_lines_degrade_without_a_poller_status() {
        assert_eq!(
            pr_tooltip_lines(&link("https://github.com/o/r/pull/7", None), true, 9),
            vec![
                "o/r#7".to_string(),
                "https://github.com/o/r/pull/7".into(),
                "no poller status yet".into(),
            ]
        );
    }

    #[test]
    fn without_url_drops_only_the_named_link() {
        let links = vec![
            link("https://github.com/o/r/pull/1", None),
            link("https://github.com/o/r/pull/2", Some("needs")),
        ];
        let left = without_url(&links, "https://github.com/o/r/pull/1");
        assert_eq!(
            left.iter().map(|l| l.url.as_str()).collect::<Vec<_>>(),
            vec!["https://github.com/o/r/pull/2"]
        );
        // Unknown URL is a no-op; removing the last one empties the mirror.
        assert_eq!(without_url(&links, "nope").len(), 2);
        assert!(without_url(&left, "https://github.com/o/r/pull/2").is_empty());
    }

    #[test]
    fn chip_label_carries_repo_and_falls_back_without_a_poller_label() {
        let mut l = link("https://github.com/o/r/pull/42", Some("needs"));
        assert_eq!(chip_label(&l, false), "r#42 CI ✗");
        assert_eq!(chip_label(&l, true), "o/r#42 CI ✗");
        l.status.as_mut().unwrap().label.clear();
        assert_eq!(chip_label(&l, false), "r#42");
        assert_eq!(
            chip_label(&link("https://github.com/o/r/pull/42", None), false),
            "r#42"
        );
    }
}
