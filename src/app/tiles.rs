//! Tile grid for the selected workspace: the auto-tiling terminal region with
//! resizable sashes (2-pane split, multi-pane horizontal pairs, inter-row
//! vertical sashes), plus focus-zoom (single pane fills the region).

use gpui::{div, prelude::*, px, relative, Context, SharedString};

use crate::pane::Pane;
use crate::theme::SeancePalette;

use super::chrome::render_pane;
use super::util::tip;
use super::{SashDrag, SeanceApp};

impl SeanceApp {
    pub(super) fn toggle_zoom(&mut self, slug: &str, cx: &mut Context<Self>) {
        if self.zoomed_slug.as_deref() == Some(slug) {
            self.zoomed_slug = None;
        } else {
            self.zoomed_slug = Some(slug.to_string());
            self.active_slug = Some(slug.to_string());
        }
        cx.notify();
    }

    /// Full-bleed single pane with a persistent zoom bar so the mode is obvious.
    fn render_zoomed_pane(
        &self,
        pane: &Pane,
        window_active: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let name = pane.name.clone();
        let slug = pane.slug.clone();
        let slug_unzoom = slug.clone();
        let whisper = self
            .whisper
            .as_ref()
            .filter(|(ws, _)| *ws == pane.slug)
            .map(|(_, i)| i);
        let flipped = self
            .flipped
            .as_ref()
            .filter(|(ws, _)| *ws == pane.slug)
            .map(|(_, d)| d);
        // Zoom chrome stays (mode is sticky); focus ring only when window is active.
        let active = if window_active {
            Some(pane.slug.as_str())
        } else {
            None
        };
        div()
            .flex_1()
            .h_full()
            .w_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(SeancePalette::bg())
            .child(
                // Zoom mode strip — flame bar so you never forget you're zoomed.
                div()
                    .flex_none()
                    .h(px(28.))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(SeancePalette::flame().opacity(if window_active { 0.18 } else { 0.10 }))
                    .border_b_2()
                    .border_color(if window_active {
                        SeancePalette::flame()
                    } else {
                        SeancePalette::flame_dim()
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(if window_active {
                                        SeancePalette::flame()
                                    } else {
                                        SeancePalette::text_faint()
                                    })
                                    .child("⛶ zoomed"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(SeancePalette::text())
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(SeancePalette::text_faint())
                                    .child(format!("`{slug}`")),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(SeancePalette::text_faint())
                                    .child("esc · ctrl+shift+z"),
                            )
                            .child(
                                div()
                                    .id("zoom-unzoom-btn")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(SeancePalette::flame())
                                    .bg(SeancePalette::surface())
                                    .border_1()
                                    .border_color(SeancePalette::flame_dim())
                                    .hover(|s| s.bg(SeancePalette::flame().opacity(0.25)))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_zoom(&slug_unzoom, cx);
                                    }))
                                    .tooltip(tip("unzoom (esc)"))
                                    .child("unzoom"),
                            ),
                    ),
            )
            .child(
                // Must be a flex container — render_pane roots with flex_1 and
                // only expands when the parent is flex (pre-chrome zoom path
                // used .flex() on the tile wrapper; without it the terminal
                // body collapses to 0 height and looks blank).
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .p_1()
                    .flex()
                    .child(render_pane(
                        pane,
                        active,
                        self.statuses.get(&pane.slug),
                        self.owners.get(&pane.slug),
                        self.touches.get(&pane.slug),
                        None,
                        flipped,
                        true, // is_zoomed
                        cx,
                    )),
            )
            .into_any_element()
    }

    pub(super) fn render_tiles(&self, window_active: bool, cx: &Context<Self>) -> impl IntoElement {
        // The tiling region shows only the SELECTED workspace's tiled panes.
        let mut tiled: Vec<&Pane> = self
            .panes
            .iter()
            .filter(|s| {
                s.tiled
                    && s.popped.is_none()
                    && self
                        .selected_workspace
                        .as_deref()
                        .is_none_or(|ws| s.workspace == ws)
            })
            .collect();
        // Focus-zoom: single pane fills the region with unmistakable chrome.
        if let Some(z) = self.zoomed_slug.as_deref() {
            if let Some(p) = tiled
                .iter()
                .find(|p| p.slug == z)
                .copied()
                .or_else(|| self.panes.iter().find(|p| p.slug == z))
            {
                return self.render_zoomed_pane(p, window_active, cx);
            }
        }
        let n = tiled.len();
        // Focus ring only while the OS window is active.
        let active = if window_active {
            self.active_slug.clone()
        } else {
            None
        };

        if n == 0 {
            let ws = self
                .selected_workspace
                .clone()
                .unwrap_or_else(|| "this workspace".into());
            return div()
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_color(SeancePalette::flame_dim())
                                .text_2xl()
                                .child("✦"),
                        )
                        .child(
                            div()
                                .text_color(SeancePalette::text_faint())
                                .text_sm()
                                .child(format!("{ws} is empty — summon a spirit (ctrl+shift+n)")),
                        ),
                )
                .into_any_element();
        }

        // 2-pane resizable split (horizontal sash).
        if n == 2 && self.zoomed_slug.is_none() {
            let left = tiled[0];
            let right = tiled[1];
            let ratio = self.split_ratio.clamp(0.2, 0.8);
            let left_pct = (ratio * 100.0) as u32;
            let right_pct = 100 - left_pct;
            let flipped_l = self
                .flipped
                .as_ref()
                .filter(|(ws, _)| *ws == left.slug)
                .map(|(_, d)| d);
            let flipped_r = self
                .flipped
                .as_ref()
                .filter(|(ws, _)| *ws == right.slug)
                .map(|(_, d)| d);
            return div()
                .flex_1()
                .h_full()
                .w_full()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_row()
                .p_1()
                .gap_0()
                .child(
                    div()
                        .h_full()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .flex()
                        .w(relative(left_pct as f32 / 100.0))
                        .child(render_pane(
                            left,
                            active.as_deref(),
                            self.statuses.get(&left.slug),
                            self.owners.get(&left.slug),
                            self.touches.get(&left.slug),
                            None,
                            flipped_l,
                            false,
                            cx,
                        )),
                )
                .child(
                    div()
                        .id("sash")
                        .flex_none()
                        .w(px(5.))
                        .h_full()
                        .cursor_col_resize()
                        .bg(SeancePalette::border())
                        .hover(|s| s.bg(SeancePalette::flame_dim()))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, ev: &gpui::MouseDownEvent, _, cx| {
                                this.sash_drag = Some(SashDrag::TwoPane {
                                    start_x: ev.position.x.into(),
                                });
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .h_full()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .flex()
                        .w(relative(right_pct as f32 / 100.0))
                        .child(render_pane(
                            right,
                            active.as_deref(),
                            self.statuses.get(&right.slug),
                            self.owners.get(&right.slug),
                            self.touches.get(&right.slug),
                            None,
                            flipped_r,
                            false,
                            cx,
                        )),
                )
                .into_any_element();
        }

        // Weighted auto-grid with inter-pane + inter-row sashes (n≠2, or zoomed).
        let cols = (n as f32).sqrt().ceil() as usize;
        let rows = n.div_ceil(cols);

        // Pre-slice into rows so we can hang vertical sashes between them.
        let mut row_lists: Vec<Vec<&Pane>> = Vec::new();
        {
            let mut it = tiled.into_iter();
            for _ in 0..rows {
                let mut row_panes: Vec<&Pane> = Vec::new();
                for _ in 0..cols {
                    if let Some(pane) = it.next() {
                        row_panes.push(pane);
                    }
                }
                if !row_panes.is_empty() {
                    row_lists.push(row_panes);
                }
            }
        }

        let mut grid = div()
            .flex_1()
            .h_full()
            .w_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap_0()
            .p_1();
        for (ri, row_panes) in row_lists.iter().enumerate() {
            let row_key = format!(
                "row-{}",
                row_panes.first().map(|p| p.slug.as_str()).unwrap_or("x")
            );
            let row_w = self
                .row_weights
                .get(&row_key)
                .copied()
                .unwrap_or(1.0)
                .max(0.15);
            let mut row = div()
                .min_h_0()
                .min_w_0()
                .w_full()
                .overflow_hidden()
                .flex()
                .flex_row()
                .gap_0()
                .flex_grow(row_w);
            for (i, pane) in row_panes.iter().enumerate() {
                let w = self
                    .pane_weights
                    .get(&pane.slug)
                    .copied()
                    .unwrap_or(1.0)
                    .max(0.15);
                let flipped = self
                    .flipped
                    .as_ref()
                    .filter(|(ws, _)| *ws == pane.slug)
                    .map(|(_, d)| d);
                row = row.child(
                    div()
                        .h_full()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .flex()
                        .flex_grow(w)
                        .child(render_pane(
                            pane,
                            active.as_deref(),
                            self.statuses.get(&pane.slug),
                            self.owners.get(&pane.slug),
                            self.touches.get(&pane.slug),
                            None, // whisper retired
                            flipped,
                            false,
                            cx,
                        )),
                );
                if i + 1 < row_panes.len() {
                    let left = pane.slug.clone();
                    let right = row_panes[i + 1].slug.clone();
                    let left_w = self.pane_weights.get(&left).copied().unwrap_or(1.0);
                    let right_w = self.pane_weights.get(&right).copied().unwrap_or(1.0);
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("sash-{left}-{right}")))
                            .flex_none()
                            .w(px(5.))
                            .h_full()
                            .cursor_col_resize()
                            .bg(SeancePalette::border())
                            .hover(|s| s.bg(SeancePalette::flame_dim()))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                    this.sash_drag = Some(SashDrag::Pair {
                                        left: left.clone(),
                                        right: right.clone(),
                                        start_x: ev.position.x.into(),
                                        left_w,
                                        right_w,
                                    });
                                    cx.notify();
                                }),
                            ),
                    );
                }
            }
            grid = grid.child(row);
            // Vertical sash between rows.
            if ri + 1 < row_lists.len() {
                let above_key = row_key.clone();
                let below_key = format!(
                    "row-{}",
                    row_lists[ri + 1]
                        .first()
                        .map(|p| p.slug.as_str())
                        .unwrap_or("x")
                );
                let above_w = self.row_weights.get(&above_key).copied().unwrap_or(1.0);
                let below_w = self.row_weights.get(&below_key).copied().unwrap_or(1.0);
                grid = grid.child(
                    div()
                        .id(SharedString::from(format!("rsash-{above_key}-{below_key}")))
                        .flex_none()
                        .h(px(5.))
                        .w_full()
                        .cursor_row_resize()
                        .bg(SeancePalette::border())
                        .hover(|s| s.bg(SeancePalette::flame_dim()))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                this.sash_drag = Some(SashDrag::RowPair {
                                    above_key: above_key.clone(),
                                    below_key: below_key.clone(),
                                    start_y: ev.position.y.into(),
                                    above_w,
                                    below_w,
                                });
                                cx.notify();
                            }),
                        ),
                );
            }
        }
        grid.into_any_element()
    }
}
