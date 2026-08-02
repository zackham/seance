//! PR links surface: the header chip for the selected workspace's most recent
//! scraped GitHub PR, its all-links popover ("clear PR links" lives there),
//! and the pure helpers the sidebar attention machinery folds in.
//!
//! The daemon owns the URL list (scraped from pane output) and an external
//! poller fills in `PrStatus` through `pr_watch.json`; this module is a pure
//! mirror + renderer — it never touches the network or the filesystem.

use gpui::{div, prelude::*, px, Context, SharedString};

use seance_core::protocol::PrLink;

use crate::runtime::protocol::GuiRequest;
use crate::theme::SeancePalette;

use super::util::tip_s;
use super::workspaces::WorkspaceAttention;
use super::SeanceApp;

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

/// `#123 label` chip text.
fn chip_label(link: &PrLink) -> String {
    let num = pr_number(&link.url)
        .map(|n| format!("#{n}"))
        .unwrap_or_else(|| "PR".into());
    let label = link.status.as_ref().map(|s| s.label.as_str()).unwrap_or("");
    if label.is_empty() {
        num
    } else {
        format!("{num} {label}")
    }
}

impl SeanceApp {
    /// This window's mirror of the daemon-owned link list for `ws`.
    pub(super) fn pr_links_for(&self, ws: &str) -> &[PrLink] {
        self.pr_links.get(ws).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Header strip: chip for the selected workspace's most recent PR link,
    /// plus (when >1) a `▾` popover listing every link and a clear affordance.
    /// Renders nothing at all when the circle has no links.
    pub(super) fn render_pr_chip(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let Some(ws) = self.selected_workspace.clone() else {
            return div().flex_none().into_any_element();
        };
        let links = self.pr_links_for(&ws);
        let Some(latest) = links.last() else {
            return div().flex_none().into_any_element();
        };
        let count = links.len();
        let url = latest.url.clone();
        let open_url = url.clone();
        let expanded = self.pr_menu_open;
        let mut row = div()
            .id("pr-strip")
            .flex_none()
            .w_full()
            .px_1()
            .pt_1()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                div()
                    .id("pr-chip")
                    .flex_none()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(SeancePalette::border())
                    .bg(SeancePalette::surface())
                    .text_xs()
                    .text_color(link_color(latest))
                    .cursor_pointer()
                    .hover(|s| s.bg(SeancePalette::border()))
                    .tooltip(tip_s(url.clone()))
                    .on_click(cx.listener(move |_this, _, _, _| {
                        crate::sysopen::open_detached(&open_url);
                    }))
                    .child(chip_label(latest)),
            );
        if count > 1 {
            row = row.child(
                div()
                    .id("pr-more")
                    .flex_none()
                    .px_1()
                    .py_0p5()
                    .rounded_md()
                    .text_xs()
                    .text_color(SeancePalette::text_dim())
                    .cursor_pointer()
                    .hover(|s| s.bg(SeancePalette::border()))
                    .tooltip(tip_s(format!("{count} PR links")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.pr_menu_open = !this.pr_menu_open;
                        cx.notify();
                    }))
                    .child(if expanded { "▴" } else { "▾" }),
            );
        }
        if !(expanded && count > 1) {
            return row.into_any_element();
        }
        let ws_for_clear = ws.clone();
        let rows: Vec<gpui::AnyElement> = links
            .iter()
            .rev()
            .map(|l| {
                let target = l.url.clone();
                let state = l
                    .status
                    .as_ref()
                    .map(|s| s.state.clone())
                    .unwrap_or_default();
                div()
                    .id(SharedString::from(format!("pr-item-{}", l.url)))
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .text_xs()
                    .text_color(link_color(l))
                    .cursor_pointer()
                    .hover(|s| s.bg(SeancePalette::border()))
                    .tooltip(tip_s(l.url.clone()))
                    .on_click(cx.listener(move |_this, _, _, _| {
                        crate::sysopen::open_detached(&target);
                    }))
                    .child(if state.is_empty() {
                        chip_label(l)
                    } else {
                        format!("{} · {state}", chip_label(l))
                    })
                    .into_any_element()
            })
            .chain(std::iter::once(
                div()
                    .id("pr-clear")
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .text_xs()
                    .text_color(SeancePalette::text_faint())
                    .cursor_pointer()
                    .hover(|s| s.bg(SeancePalette::border()))
                    .tooltip(tip_s("drop every PR link on this circle"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.clear_pr_links(&ws_for_clear, cx);
                    }))
                    .child("clear PR links")
                    .into_any_element(),
            ))
            .collect();
        div()
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .child(row)
            .child(
                div()
                    .id("pr-menu")
                    .flex_none()
                    .mx_1()
                    .mt_0p5()
                    .w(px(320.))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .p_1()
                    .rounded_lg()
                    .border_1()
                    .border_color(SeancePalette::border())
                    .bg(SeancePalette::bg_elevated())
                    .children(rows),
            )
            .into_any_element()
    }

    /// `pr-link clear <workspace>` over the GUI's ctl seam. The daemon persists
    /// and pushes State back, which re-seeds our mirror.
    fn clear_pr_links(&mut self, ws: &str, cx: &mut Context<Self>) {
        let _ = self.client.send(GuiRequest::Ctl(
            seance_core::control::ControlRequest::PrLinkClear {
                url: None,
                workspace: Some(ws.to_string()),
                scope: None,
                from: Some("gui".into()),
            },
        ));
        self.pr_links.remove(ws);
        self.pr_menu_open = false;
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
    fn chip_label_falls_back_without_a_poller_label() {
        let mut l = link("https://github.com/o/r/pull/42", Some("needs"));
        assert_eq!(chip_label(&l), "#42 CI ✗");
        l.status.as_mut().unwrap().label.clear();
        assert_eq!(chip_label(&l), "#42");
        assert_eq!(
            chip_label(&link("https://github.com/o/r/pull/42", None)),
            "#42"
        );
    }
}
