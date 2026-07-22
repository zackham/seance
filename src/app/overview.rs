//! Full-window live overview map (ctrl+shift+space): per-workspace cards of
//! live pane thumbnails, packed into a grid.

use gpui::{div, prelude::*, px, Context, SharedString};
use gpui_component::StyledExt as _;

use crate::remote_term::RemoteTerminal;
use crate::remote_term_view::OverviewThumb;
use crate::theme::SeancePalette;

use super::util::{status_color, tip};
use super::SeanceApp;

impl SeanceApp {
    pub(super) fn set_overview(&mut self, on: bool, cx: &mut Context<Self>) {
        self.overview = on;
        let _ = self.client.set_overview(on);
        cx.notify();
    }

    fn overview_thumb_for(
        &self,
        terminal: &gpui::Entity<RemoteTerminal>,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<OverviewThumb> {
        cx.new(|cx| OverviewThumb::new(terminal.clone(), cx))
    }

    pub(super) fn render_overview(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspaces = self.workspaces();
        let selected = self.selected_workspace.clone();
        let n = workspaces.len().max(1);
        let cols = (n as f32).sqrt().ceil() as usize;
        let cards = self.pack_overview_cards(workspaces, selected, cols.max(1), cx);
        div()
            .id("overview")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(SeancePalette::bg())
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
                            .child("overview · all workspaces"),
                    )
                    .child(
                        div()
                            .id("overview-close")
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_overview(false, cx);
                            }))
                            .tooltip(tip("exit overview (esc · ctrl+shift+space)"))
                            .child("esc · ctrl+shift+space"),
                    ),
            )
            .child(
                div()
                    .id("overview-scroll")
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(cards),
            )
    }

    fn pack_overview_cards(
        &mut self,
        workspaces: Vec<String>,
        selected: Option<String>,
        cols: usize,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut cards: Vec<gpui::AnyElement> = Vec::with_capacity(workspaces.len());
        for ws in &workspaces {
            let is_sel = selected.as_deref() == Some(ws.as_str());
            let attention = self.workspace_attention_cx(ws, cx);
            let thumbs: Vec<gpui::AnyElement> = self
                .panes
                .iter()
                .filter(|p| p.workspace == *ws && p.tiled && p.popped.is_none())
                .filter_map(|pane| {
                    let rt = pane.remote_terminal()?.clone();
                    let thumb = self.overview_thumb_for(&rt, cx);
                    let sc = self
                        .statuses
                        .get(&pane.slug)
                        .map(|s| status_color(&s.state))
                        .unwrap_or(SeancePalette::border());
                    Some(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h(px(80.))
                            .border_1()
                            .border_color(sc)
                            .rounded_md()
                            .overflow_hidden()
                            .child(thumb)
                            .into_any_element(),
                    )
                })
                .collect();
            let ws_click = ws.clone();
            let badge = attention.map(|a| {
                div()
                    .px_1p5()
                    .rounded_md()
                    .text_xs()
                    .text_color(a.color())
                    .child(a.label())
                    .into_any_element()
            });
            cards.push(
                div()
                    .id(SharedString::from(format!("ov-card-{ws}")))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(if is_sel {
                        SeancePalette::flame()
                    } else {
                        SeancePalette::border()
                    })
                    .bg(SeancePalette::bg_elevated())
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_overview(false, cx);
                        this.select_workspace(&ws_click, window, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .font_semibold()
                                    .text_color(SeancePalette::text())
                                    .child(ws.clone()),
                            )
                            .children(badge),
                    )
                    .child(div().flex().flex_row().gap_1().min_h(px(100.)).children(
                        if thumbs.is_empty() {
                            vec![div()
                                .text_xs()
                                .text_color(SeancePalette::text_faint())
                                .child("(empty)")
                                .into_any_element()]
                        } else {
                            thumbs
                        },
                    ))
                    .into_any_element(),
            );
        }
        // Pack into rows of `cols`.
        let mut rows = Vec::new();
        let mut it = cards.into_iter();
        loop {
            let mut row_kids = Vec::new();
            for _ in 0..cols {
                if let Some(c) = it.next() {
                    row_kids.push(c);
                }
            }
            if row_kids.is_empty() {
                break;
            }
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .children(row_kids)
                    .into_any_element(),
            );
        }
        rows
    }
}
