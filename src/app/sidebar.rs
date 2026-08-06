//! Left rail for the SeanceApp view: the workspace/pane sidebar (auto-sorted
//! workspaces, pane drag-and-drop, per-row context menus, inline rename) and
//! the host-bridge widget strip (claude accounts) above the summon footer.

use gpui::{div, prelude::*, px, Context, SharedString, Window};
use gpui_component::{
    input::Input, menu::ContextMenuExt as _, Colorize as _, StyledExt as _, WindowExt as _,
};

use crate::runtime::protocol::GuiRequest;
use crate::theme::SeancePalette;
use seance_core::grouping::{Section, SectionRow};

/// Rail metrics. One left axis and one right edge: every glyph slot, every
/// name, and every time/count line up regardless of row kind.
const ROW_H: f32 = 28.;
/// Glyph column — fixed so a row with no glyph still starts its name on the
/// same line as one that has a spinner.
const GLYPH_W: f32 = 15.;
/// Time / count column, right-aligned.
const TIME_W: f32 = 34.;
/// How far a cluster's members sit inside their header.
const CLUSTER_INDENT: f32 = 14.;
/// The left rail's width. Anything drawn beside the rail (a host menu's panel)
/// needs it too, so it lives here rather than as a literal in the layout.
pub(super) const RAIL_WIDTH: f32 = 232.;

use super::actions::*;
use super::util::{
    selected_row_fill, sidebar_press_no_select, tip, tip_s, ui_debug, working_spinner_glyph,
    DraggedPane,
};
use super::workspaces::WorkspaceAttention;
use super::{RenameTarget, SeanceApp};

impl SeanceApp {
    /// `✦` popover: every GUI window attached to this daemon, with a kill
    /// affordance for the others (`CloseWindow` — the daemon unregisters the
    /// window and the client quits on `Kicked`). Anchored under the brand
    /// header; a transparent full-window backdrop behind it eats the
    /// click-away, same trick as the quicklaunch editor overlay.
    pub(super) fn render_gui_menu(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        if !self.gui_menu_open {
            return None;
        }
        let self_id = self.window_id.clone();
        let rows: Vec<(String, String, usize)> = self
            .windows
            .iter()
            .map(|w| (w.id.clone(), w.label.clone(), w.workspace_count))
            .collect();
        Some(
            div()
                .id("gui-menu-backdrop")
                .absolute()
                .inset_0()
                // Swallow mouse events so a click-away doesn't also land in a
                // terminal underneath (and steal focus).
                .occlude()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.gui_menu_open = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("gui-menu-card")
                        .absolute()
                        .left(px(8.))
                        .top(px(48.))
                        .w(px(260.))
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .p_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(SeancePalette::flame_dim())
                        .bg(SeancePalette::bg_elevated())
                        // Clicks inside the card must not reach the backdrop.
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .px_1()
                                .text_xs()
                                .text_color(SeancePalette::text_faint())
                                .child("connected guis"),
                        )
                        .children(rows.into_iter().map(|(id, label, count)| {
                            let is_self = self_id.as_deref() == Some(id.as_str());
                            let circles = if count == 1 {
                                "1 circle".to_string()
                            } else {
                                format!("{count} circles")
                            };
                            div()
                                .px_1()
                                .py_0p5()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_sm()
                                        .text_color(SeancePalette::text())
                                        .child(SharedString::from(label)),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(SeancePalette::text_faint())
                                        .child(SharedString::from(circles)),
                                )
                                .child(if is_self {
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(SeancePalette::text_faint())
                                        .child("(this window)")
                                        .into_any_element()
                                } else {
                                    div()
                                        .id(SharedString::from(format!("gui-kill-{id}")))
                                        .flex_none()
                                        .px_1p5()
                                        .rounded_md()
                                        .text_xs()
                                        .cursor_pointer()
                                        .text_color(SeancePalette::text_faint())
                                        .hover(|s| {
                                            s.text_color(SeancePalette::danger())
                                                .bg(SeancePalette::surface())
                                        })
                                        .tooltip(tip("close that window"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let _ = this.client.send(GuiRequest::CloseWindow {
                                                window: id.clone(),
                                            });
                                            this.gui_menu_open = false;
                                            cx.notify();
                                        }))
                                        .child("kill")
                                        .into_any_element()
                                })
                        }))
                        .child(div().my_1().h(px(1.)).bg(SeancePalette::border()))
                        .child(
                            div()
                                .px_1()
                                .text_xs()
                                .text_color(SeancePalette::text_faint())
                                .child(concat!("seance ", env!("CARGO_PKG_VERSION"))),
                        )
                        .child(
                            div()
                                .id("gui-menu-help")
                                .px_1()
                                .py_0p5()
                                .rounded_md()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .cursor_pointer()
                                .text_xs()
                                .text_color(SeancePalette::text_dim())
                                .hover(|s| {
                                    s.text_color(SeancePalette::flame())
                                        .bg(SeancePalette::surface())
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.gui_menu_open = false;
                                    this.open_help_window(cx);
                                    cx.notify();
                                }))
                                .child("grimoire")
                                .child("?"),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Host-bridge strip(s) above the summon footer. Empty when no host or poll failed.
    ///
    /// Collapsed (default): only the current/selected account. Click expands
    /// the full list; click an account to select it and collapse. Clicking the
    /// already-selected account collapses without re-running select.
    pub(super) fn render_host_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        if self.host.widgets.is_empty() {
            return div().flex_none().into_any_element();
        }
        div()
            .flex_none()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(SeancePalette::border())
            .children(self.host.widgets.iter().map(|w| {
                let title = if w.title.is_empty() {
                    w.id.clone()
                } else {
                    w.title.clone()
                };
                let widget_id = w.id.clone();
                let expanded = self.host_expanded.contains(&widget_id);
                let caret = if expanded { "▾" } else { "▸" };
                // Prefer explicit selected flag, then host `active`, then first.
                let current_id = w
                    .items
                    .iter()
                    .find(|i| i.selected)
                    .map(|i| i.id.clone())
                    .or_else(|| w.active.clone())
                    .or_else(|| w.items.first().map(|i| i.id.clone()));
                let visible: Vec<_> = if expanded {
                    w.items.iter().collect()
                } else {
                    w.items
                        .iter()
                        .filter(|i| current_id.as_deref() == Some(i.id.as_str()) || i.selected)
                        .collect()
                };
                div()
                    .flex()
                    .flex_col()
                    .py_1p5()
                    .gap_0p5()
                    .child(
                        div()
                            .px_2()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .id(SharedString::from(format!("host-title-{widget_id}")))
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .cursor_pointer()
                                    .tooltip(tip(if expanded {
                                        "collapse accounts"
                                    } else {
                                        "expand accounts"
                                    }))
                                    .on_click({
                                        let wid = widget_id.clone();
                                        cx.listener(move |this, _, _, cx| {
                                            if this.host_expanded.contains(&wid) {
                                                this.host_expanded.remove(&wid);
                                            } else {
                                                this.host_expanded.insert(wid.clone());
                                            }
                                            cx.notify();
                                        })
                                    })
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(SeancePalette::text_faint())
                                            .child(format!("{caret} {title}")),
                                    ),
                            )
                            .when_some(w.error.as_ref(), |d, err| {
                                d.child(
                                    div()
                                        .id(SharedString::from(format!("host-err-{}", widget_id)))
                                        .text_xs()
                                        .text_color(SeancePalette::danger())
                                        .tooltip(tip_s(err.clone()))
                                        .child("!"),
                                )
                            }),
                    )
                    .children(visible.into_iter().map(|item| {
                        let wid = widget_id.clone();
                        let iid = item.id.clone();
                        let selected =
                            item.selected || current_id.as_deref() == Some(item.id.as_str());
                        let state = item.state.as_str();
                        let color = match state {
                            "busy" => SeancePalette::danger(),
                            "warm" => SeancePalette::flame(),
                            "auth" => SeancePalette::violet(),
                            _ if selected => SeancePalette::success(),
                            _ => SeancePalette::text_faint(),
                        };
                        let mark = if selected { "●" } else { "○" };
                        let label = item.label.clone();
                        let detail = item.detail.clone();
                        let detail2 = item.detail2.clone();
                        let tip_text = if !expanded {
                            format!("{label} · click to show all accounts")
                        } else if selected {
                            format!("{label} · current · click to collapse")
                        } else {
                            format!("switch to {label}")
                        };
                        // Full-bleed selected row (same fill as workspaces).
                        div()
                            .id(SharedString::from(format!("host-{wid}-{iid}")))
                            .flex()
                            .items_start()
                            .gap_1p5()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(selected_row_fill()))
                            .hover(|s| {
                                if selected {
                                    s.bg(selected_row_fill().lighten(0.04))
                                } else {
                                    s.bg(SeancePalette::surface())
                                }
                            })
                            .tooltip(tip_s(tip_text))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.host_item_click(&wid, &iid, window, cx);
                            }))
                            .child(
                                div()
                                    .flex_none()
                                    .pt(px(1.))
                                    .text_xs()
                                    .text_color(color)
                                    .child(mark),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_xs()
                                            .font_weight(if selected {
                                                gpui::FontWeight::SEMIBOLD
                                            } else {
                                                gpui::FontWeight::NORMAL
                                            })
                                            .text_color(if selected {
                                                SeancePalette::text()
                                            } else {
                                                SeancePalette::text_dim()
                                            })
                                            .child(label),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_xs()
                                            .text_color(SeancePalette::text_faint())
                                            .child(detail),
                                    )
                                    .when(!detail2.is_empty(), |d| {
                                        d.child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_xs()
                                                .text_color(SeancePalette::text_faint())
                                                .child(detail2),
                                        )
                                    }),
                            )
                    }))
                    .into_any_element()
            }))
            .into_any_element()
    }

    /// Collapsed → expand. Expanded → select clicked account and collapse.
    /// Already-selected while expanded → collapse only (no re-switch).
    pub(super) fn host_item_click(
        &mut self,
        widget_id: &str,
        item_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let expanded = self.host_expanded.contains(widget_id);
        if !expanded {
            self.host_expanded.insert(widget_id.to_string());
            cx.notify();
            return;
        }

        let already = self
            .host
            .widgets
            .iter()
            .find(|w| w.id == widget_id)
            .map(|w| {
                w.items.iter().any(|i| i.id == item_id && i.selected)
                    || w.active.as_deref() == Some(item_id)
            })
            .unwrap_or(false);

        // Always collapse on the second click.
        self.host_expanded.remove(widget_id);
        if already {
            // No-op for selection — already current, don't re-run select_cmd.
            cx.notify();
            return;
        }
        self.host_select(widget_id, item_id, window, cx);
    }

    pub(super) fn host_select(
        &mut self,
        widget_id: &str,
        item_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Select runs daemon-side (thin client: the daemon machine's account
        // switches, not this one's) and can take seconds — keep it off the UI
        // thread and notify when it lands.
        let client = self.client.clone();
        let widget = widget_id.to_string();
        let item = item_id.to_string();
        let item_for_msg = item.clone();
        let _ = window; // notification lands via the async update below
        let task = cx
            .background_executor()
            .spawn(async move { client.host_select(&widget, &item) });
        cx.spawn_in(window, async move |this: gpui::WeakEntity<Self>, cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |_, cx| {
                        match result {
                            Ok(raw) => {
                                // Prefer host JSON message when present.
                                let msg = serde_json::from_str::<serde_json::Value>(&raw)
                                    .ok()
                                    .and_then(|v| {
                                        let email = v.get("email").and_then(|e| e.as_str());
                                        let id = v
                                            .get("id")
                                            .and_then(|e| e.as_str())
                                            .unwrap_or(&item_for_msg);
                                        Some(match email {
                                            Some(e) if !e.is_empty() && e != "unknown" => {
                                                format!("claude → {id} ({e})")
                                            }
                                            _ => format!("claude → {id}"),
                                        })
                                    })
                                    .unwrap_or_else(|| format!("claude → {item_for_msg}"));
                                window.push_notification(
                                    gpui_component::notification::Notification::success(msg),
                                    cx,
                                );
                            }
                            Err(e) => {
                                window.push_notification(
                                    gpui_component::notification::Notification::error(format!(
                                        "switch failed: {e}"
                                    )),
                                    cx,
                                );
                            }
                        }
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }

    /// A band header. Deliberately a different species from a cluster header:
    /// uppercase, letterspaced, quiet — a landmark you navigate by, not a line
    /// you read. Its caret sits in the same column as every row's glyph, so
    /// the rail has ONE left axis instead of three.
    fn render_section_header(
        &self,
        section: Section,
        rows: &[String],
        first: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let key = section.key();
        let collapsed = self.subs_pref.is_collapsed(key);
        let count = rows.len();
        let att = if collapsed {
            rows.iter()
                .filter_map(|ws| self.workspace_attention_cx(ws))
                .max_by_key(|a| a.priority())
        } else {
            None
        };
        let title = section.title().to_uppercase();
        div()
            .id(SharedString::from(format!("section-{key}")))
            // Air above, tight below: a header belongs to what follows it.
            .when(!first, |d| d.mt(px(12.)))
            .mb(px(2.))
            .pl(px(5.))
            .pr_2()
            .h(px(18.))
            .flex()
            .items_center()
            .gap_1p5()
            .cursor_pointer()
            .hover(|s| s.bg(SeancePalette::surface()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.subs_pref.toggle_collapsed(key);
                this.save_subscriptions();
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(GLYPH_W))
                    .text_xs()
                    .text_color(SeancePalette::text_faint())
                    .child(if collapsed { "\u{25b8}" } else { "\u{25be}" }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(SeancePalette::text_faint())
                    .child(title),
            )
            .children(att.map(|a| {
                div().flex_none().text_xs().text_color(a.color()).child(
                    if matches!(a, WorkspaceAttention::Working) {
                        working_spinner_glyph()
                    } else {
                        "\u{25cf}"
                    },
                )
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(TIME_W))
                    .text_xs()
                    .text_right()
                    .text_color(SeancePalette::text_faint())
                    .child(count.to_string()),
            )
            .into_any_element()
    }

    /// A cluster header. Reads like a row, not like a band — same size and
    /// axis as the circles it holds, because conceptually it is one of them.
    fn render_group_header(
        &self,
        section: Section,
        prefix: &str,
        members: &[String],
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let key = crate::subscriptions_pref::group_key(section.key(), prefix);
        let collapsed = self.subs_pref.is_collapsed(&key);
        let count = members.len();
        let att = if collapsed {
            members
                .iter()
                .filter_map(|ws| self.workspace_attention_cx(ws))
                .max_by_key(|a| a.priority())
        } else {
            None
        };
        let label = prefix.to_string();
        let toggle_key = key.clone();
        div()
            .id(SharedString::from(format!("group-{key}")))
            .h(px(ROW_H))
            .pl(px(5.))
            .pr_2()
            .flex()
            .items_center()
            .gap_1p5()
            .cursor_pointer()
            .hover(|s| s.bg(SeancePalette::surface()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.subs_pref.toggle_collapsed(&toggle_key);
                this.save_subscriptions();
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(GLYPH_W))
                    .text_xs()
                    .text_color(SeancePalette::text_faint())
                    .child(if collapsed { "\u{25b8}" } else { "\u{25be}" }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(SeancePalette::text_dim())
                    .child(label),
            )
            .children(att.map(|a| {
                div().flex_none().text_xs().text_color(a.color()).child(
                    if matches!(a, WorkspaceAttention::Working) {
                        working_spinner_glyph()
                    } else {
                        "\u{25cf}"
                    },
                )
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(TIME_W))
                    .text_xs()
                    .text_right()
                    .text_color(SeancePalette::text_faint())
                    .child(count.to_string()),
            )
            .into_any_element()
    }

    /// One band, fully rendered: header, then its rows — loose circles at the
    /// band's indent, clustered ones under their prefix header.
    fn render_section(
        &self,
        section: Section,
        circles: Vec<String>,
        first: bool,
        cx: &Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        if circles.is_empty() {
            return out;
        }
        let parked = matches!(section, Section::Parked);
        out.push(self.render_section_header(section, &circles, first, cx));
        if self.subs_pref.is_collapsed(section.key()) {
            return out;
        }
        for row in self.section_rows(&circles) {
            match row {
                SectionRow::Circle(ws) => {
                    out.push(self.render_workspace_group(ws, parked, cx));
                }
                SectionRow::Group { prefix, members } => {
                    out.push(self.render_group_header(section, &prefix, &members, cx));
                    let key = crate::subscriptions_pref::group_key(section.key(), &prefix);
                    if self.subs_pref.is_collapsed(&key) {
                        continue;
                    }
                    // A hairline down the members ties them into one object,
                    // so a cluster reads as a block rather than as rows that
                    // happen to be indented.
                    for ws in members {
                        out.push(
                            div()
                                .ml(px(CLUSTER_INDENT))
                                .border_l_1()
                                .border_color(SeancePalette::border())
                                .child(self.render_workspace_row(ws, parked, true, cx))
                                .into_any_element(),
                        );
                    }
                }
            }
        }
        out
    }

    /// One sidebar workspace group. Parked rows use the same builder, sort and
    /// badges as active ones — only muted, and with a different menu verb.
    fn render_workspace_group(
        &self,
        workspace: String,
        parked: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        self.render_workspace_row(workspace, parked, false, cx)
    }

    /// `in_cluster` dims the shared prefix — see the name column below.
    fn render_workspace_row(
        &self,
        workspace: String,
        parked: bool,
        in_cluster: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let selected = self.selected_workspace.as_deref() == Some(workspace.as_str());
        // Drives the menu verb only — the pinned *section* is composed by
        // `render_sidebar`, so the row itself needs no other special-casing.
        let pinned = self.subs_pref.is_pinned(&workspace);
        let ws_for_click = workspace.clone();
        let ws_for_group_drop = workspace.clone();
        let ws_for_pane_drop = workspace.clone();
        let ws_for_menu = workspace.clone();
        let renaming_this_ws = matches!(
            &self.renaming,
            Some((RenameTarget::Workspace(w), _)) if *w == workspace
        );
        let rename_input = self.renaming.as_ref().map(|(_, i)| i.clone());
        // Sleep verbs: only offered when the daemon says every pane in the
        // circle can be put back exactly (it checks the filesystem, we can't).
        let asleep = self.workspace_asleep(&workspace);
        let sleepable = !asleep && self.workspace_sleepable(&workspace);
        // Attention: parked rows additionally badge `needs` until first looked at.
        let attention = if parked {
            self.parked_attention(&workspace)
        } else {
            self.workspace_attention_cx(&workspace)
        };
        let header: gpui::AnyElement = if renaming_this_ws {
            div()
                .px_2()
                .py_1p5()
                .children(rename_input.map(|i| Input::new(&i)))
                .into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("ws-{workspace}")))
                .group(SharedString::from(format!("wsgrp-{workspace}")))
                .h(px(ROW_H))
                .pr_2()
                // 3px of the left inset is the selection anchor, so the text
                // never shifts when a row becomes selected.
                .border_l_3()
                .border_color(if selected {
                    SeancePalette::flame()
                } else {
                    gpui::transparent_black()
                })
                .pl(px(5.))
                .flex()
                .items_center()
                .gap_1p5()
                .cursor_pointer()
                .when(asleep && !selected, |d| d.opacity(0.62))
                .when(selected, |d| d.bg(SeancePalette::surface()))
                .hover(|s| {
                    if selected {
                        s.bg(selected_row_fill().lighten(0.04))
                    } else {
                        s.bg(SeancePalette::surface())
                    }
                })
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|_this, _, window, cx| {
                        sidebar_press_no_select(window, cx);
                    }),
                )
                // Drop a pane onto the header → move into this circle.
                // Workspace-vs-workspace drag-reorder is intentionally gone;
                // order is auto (working agents, then last human touch).
                .drag_over::<DraggedPane>(|style, _, _, _| style.bg(SeancePalette::violet_dim()))
                .on_drop(cx.listener(move |this, drag: &DraggedPane, _, cx| {
                    ui_debug(&format!(
                        "drop pane '{}' on workspace header '{}'",
                        drag.slug, ws_for_pane_drop
                    ));
                    this.reorder_pane(&drag.slug, &ws_for_pane_drop, None, cx);
                }))
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if event.click_count() == 2 {
                            let label = this.workspace_label(&ws_for_click);
                            this.start_rename(
                                RenameTarget::Workspace(ws_for_click.clone()),
                                &label,
                                window,
                                cx,
                            );
                        } else {
                            // Selecting a parked circle promotes it to active.
                            this.select_workspace(&ws_for_click, window, cx);
                        }
                    }),
                )
                .context_menu({
                    let ws_m = ws_for_menu.clone();
                    move |menu, _, _| {
                        let m = menu
                            .menu(
                                "touch (bump recency)",
                                Box::new(ActTouchWorkspace(ws_m.clone())),
                            )
                            .menu(
                                "rename workspace",
                                Box::new(ActRenameWorkspace(ws_m.clone())),
                            )
                            .menu("fork workspace ⑂", Box::new(ActForkWorkspace(ws_m.clone())))
                            .menu("share replay…", Box::new(ActShareReplay(ws_m.clone())));
                        let m = if pinned {
                            m.menu("unpin", Box::new(ActUnpinWorkspace(ws_m.clone())))
                        } else {
                            // Pinning a parked circle activates it too.
                            m.menu("pin to top", Box::new(ActPinWorkspace(ws_m.clone())))
                        };
                        let m = if parked {
                            m.menu(
                                "add to active",
                                Box::new(ActActivateWorkspace(ws_m.clone())),
                            )
                        } else {
                            m.menu("park circle", Box::new(ActParkWorkspace(ws_m.clone())))
                        };
                        let m = if asleep {
                            m.menu("awaken circle", Box::new(ActWakeWorkspace(ws_m.clone())))
                        } else if sleepable {
                            m.menu("sleep circle", Box::new(ActSleepWorkspace(ws_m.clone())))
                        } else {
                            m
                        };
                        m.separator().menu(
                            "banish workspace (kill all panes)",
                            Box::new(ActKillWorkspace(ws_m.clone())),
                        )
                    }
                })
                .child({
                    // The glyph slot earns its ink or stays empty. A diamond
                    // on every idle row is a dozen identical marks that say
                    // nothing; leaving them off is what makes the two rows
                    // that ARE doing something impossible to miss.
                    let working = matches!(attention, Some(WorkspaceAttention::Working));
                    let needs = matches!(attention, Some(WorkspaceAttention::NeedsHuman));
                    // Selection is carried by the flame anchor down the left
                    // edge, which frees the glyph to keep saying what the
                    // circle is DOING — the selected row can be working too.
                    let (glyph, color) = if needs {
                        ("●", SeancePalette::violet())
                    } else if working {
                        (working_spinner_glyph(), SeancePalette::flame())
                    } else if selected {
                        ("◆", SeancePalette::flame())
                    } else if asleep {
                        ("☾", SeancePalette::text_faint())
                    } else {
                        ("", SeancePalette::text_faint())
                    };
                    // Fixed width: every name in the rail starts on the same
                    // vertical line whether or not its row has a glyph.
                    div()
                        .flex_none()
                        .w(px(GLYPH_W))
                        .text_sm()
                        .text_color(color)
                        .child(glyph)
                })
                .child({
                    // Inside a cluster the shared prefix is already in the
                    // header above; repeating it at full strength makes you
                    // re-read it on every row, so it recedes.
                    let label = self.workspace_label(&workspace);
                    let needs = matches!(attention, Some(WorkspaceAttention::NeedsHuman));
                    let name_color = if selected {
                        SeancePalette::text()
                    } else if needs {
                        SeancePalette::violet()
                    } else {
                        SeancePalette::text_dim()
                    };
                    let (head, tail) = match in_cluster {
                        true => match label.split_once('-') {
                            Some((h, t)) => (format!("{h}-"), t.to_string()),
                            None => (String::new(), label.clone()),
                        },
                        false => (String::new(), label.clone()),
                    };
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .text_sm()
                        .font_weight(if selected {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .when(!head.is_empty(), |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_color(SeancePalette::text_faint())
                                    .child(head),
                            )
                        })
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(name_color)
                                .child(tail),
                        )
                })
                .children({
                    // `done` is worth a mark but not a shout; `needs` already
                    // owns the glyph and the name colour, so it needs no pill.
                    let att = if selected {
                        None
                    } else {
                        attention.filter(|a| matches!(a, WorkspaceAttention::Done))
                    };
                    att.map(|a| {
                        div()
                            .flex_none()
                            .px_1()
                            .rounded_sm()
                            .text_xs()
                            .bg(a.color().opacity(0.14))
                            .text_color(a.color())
                            .child(a.label())
                    })
                })
                .child(
                    // Banish ×: revealed only while the row is hovered
                    // (group-hover), so idle rows stay quiet.
                    div()
                        .id(SharedString::from(format!("ws-banish-{workspace}")))
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .text_xs()
                        .text_color(gpui::transparent_black())
                        .group_hover(SharedString::from(format!("wsgrp-{workspace}")), |s| {
                            s.text_color(SeancePalette::text_faint())
                        })
                        .hover(|s| {
                            s.text_color(SeancePalette::danger())
                                .bg(SeancePalette::surface())
                        })
                        .cursor_pointer()
                        .on_click({
                            let ws = workspace.clone();
                            cx.listener(move |this, _, window, cx| {
                                this.kill_workspace(&ws, window, cx);
                            })
                        })
                        .tooltip(tip("banish workspace (kill all panes)"))
                        .child("×"),
                )
                .child({
                    // Fixed width, right-aligned — headers put their counts in
                    // the same column, so the whole rail has one hard right
                    // edge instead of a ragged gutter.
                    let label = self.workspace_activity_label(&workspace);
                    div()
                        .flex_none()
                        .w(px(TIME_W))
                        .text_xs()
                        .text_right()
                        .text_color(if selected {
                            SeancePalette::text_dim()
                        } else {
                            SeancePalette::text_faint()
                        })
                        .child(label.unwrap_or_default())
                })
                .into_any_element()
        };
        div()
            .id(SharedString::from(format!("wsgroup-{workspace}")))
            .flex()
            .flex_col()
            .gap_0p5()
            .mb_0p5()
            // Parked rows read as a quieter band without a second palette.
            .when(parked, |d| d.opacity(0.72))
            .drag_over::<DraggedPane>(|style, _, _, _| style.bg(SeancePalette::surface()))
            .on_drop(cx.listener(move |this, drag: &DraggedPane, _, cx| {
                ui_debug(&format!(
                    "drop pane '{}' on workspace group '{}'",
                    drag.slug, ws_for_group_drop
                ));
                this.reorder_pane(&drag.slug, &ws_for_group_drop, None, cx);
            }))
            .child(header)
            .into_any_element()
    }

    pub(super) fn render_sidebar(
        &self,
        window_active: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // Ordered groups, INCLUDING empty workspaces (they render with 0 panes).
        // State is global now: the active band renders as it always did, and
        // everything else lands in the collapsed parked group below it.
        // Pinned circles get their own section at the very top, separated by a
        // hairline rule; each band carries the same sort.
        // Four folding bands — pinned, active, sleeping, parked — each
        // grouping its own circles by name prefix, independently.
        let section_rows: Vec<gpui::AnyElement> = self
            .workspace_sections()
            .into_iter()
            .enumerate()
            .flat_map(|(i, (section, circles))| self.render_section(section, circles, i == 0, cx))
            .collect();

        let _ = window_active; // focus chrome reserved for future empty-window dimming

        div()
            .id("sidebar")
            .flex_none()
            .w(px(RAIL_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .bg(SeancePalette::bg_elevated())
            .border_r_1()
            .border_color(SeancePalette::border())
            .child(
                // Brand header.
                div()
                    .flex_none()
                    .h(px(44.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(SeancePalette::border())
                    .child(
                        div()
                            .id("gui-census")
                            .px_1()
                            .rounded_md()
                            .text_color(SeancePalette::flame_dim())
                            .text_lg()
                            .cursor_pointer()
                            .hover(|s| {
                                s.text_color(SeancePalette::flame())
                                    .bg(SeancePalette::surface())
                            })
                            .tooltip(tip("connected guis"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.gui_menu_open = !this.gui_menu_open;
                                cx.notify();
                            }))
                            .child("✦"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(SeancePalette::text())
                            .text_sm()
                            .font_semibold()
                            .child("seance"),
                    )
                    .child(
                        div()
                            .id("new-workspace")
                            .flex_none()
                            .px_1p5()
                            .rounded_md()
                            .text_xs()
                            .text_color(SeancePalette::violet_dim())
                            .hover(|s| s.text_color(SeancePalette::violet()).bg(SeancePalette::surface()))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.create_workspace(window, cx);
                            }))
                            .tooltip(tip("new empty workspace (name it immediately)"))
                            .child("◈+"),
                    ),
            )
            .child({
                // Workspace list only — context menus live on *rows*, not the scroller.
                // Empty-area multi-window menu is a separate flex filler (avoids double menus).
                div()
                    .id("pane-list")
                    .track_scroll(&self.sidebar_scroll)
                    .flex_1()
                    .overflow_y_scroll()
                    // No horizontal pad — selected workspace fill is full-bleed.
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(section_rows)
                    // Flex filler below the rows. The parked/subscribe menu
                    // that used to live here (pull / collect) went with the
                    // ownership model; phase 2 puts the parked group here.
                    .child(div().id("sidebar-empty-hit").flex_1().min_h(px(48.)).w_full())
            })
            // `PRs (N)` sweep button — sits just above the quicklaunch strip so
            // it reads as a rail-wide affordance, not a per-circle one.
            .children(self.render_pr_board_button(cx))
            .child(self.render_quicklaunch(cx))
            .child(self.render_host_sidebar(cx))
            .child(
                // Footer: summon + help.
                div()
                    .flex_none()
                    .p_2()
                    .border_t_1()
                    .border_color(SeancePalette::border())
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("summon")
                            .flex_1()
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .text_sm()
                            .text_color(SeancePalette::flame())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.new_default_session(cx);
                            }))
                            .tooltip(tip(
                                "new shell pane in this workspace (ctrl+shift+n) — name it in the sidebar",
                            ))
                            .child("+ summon"),
                    )
                    .child(
                        div()
                            .id("activity")
                            .flex_none()
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .flex()
                            .items_center()
                            .text_sm()
                            .text_color(SeancePalette::text_dim())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.drawer = if matches!(this.drawer, super::Drawer::Activity) {
                                    super::Drawer::Closed
                                } else {
                                    super::Drawer::Activity
                                };
                                cx.notify();
                            }))
                            .tooltip(tip("activity feed — who did what, live"))
                            .child("≋"),
                    )
                    .child(
                        div()
                            .id("help")
                            .flex_none()
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .flex()
                            .items_center()
                            .text_sm()
                            .text_color(SeancePalette::violet())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_help_window(cx);
                            }))
                            .tooltip(tip("open the grimoire — full guide to seance"))
                            .child("?"),
                    ),
            )
    }
}
