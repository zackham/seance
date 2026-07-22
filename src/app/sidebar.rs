//! Left rail for the SeanceApp view: the workspace/pane sidebar (auto-sorted
//! workspaces, pane drag-and-drop, per-row context menus, inline rename) and
//! the host-bridge widget strip (claude accounts) above the summon footer.

use gpui::{div, prelude::*, px, Context, SharedString, Window};
use gpui_component::{
    input::Input, menu::ContextMenuExt as _, Colorize as _, StyledExt as _, WindowExt as _,
};

use crate::pane::Pane;
use crate::theme::SeancePalette;

use super::actions::*;
use super::util::{selected_row_fill, sidebar_press_no_select, tip, tip_s, ui_debug, DraggedPane};
use super::{RenameTarget, SeanceApp};

impl SeanceApp {
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
        match self.host.select(widget_id, item_id) {
            Ok(raw) => {
                // Prefer host JSON message when present.
                let msg = serde_json::from_str::<serde_json::Value>(&raw)
                    .ok()
                    .and_then(|v| {
                        let email = v.get("email").and_then(|e| e.as_str());
                        let id = v.get("id").and_then(|e| e.as_str()).unwrap_or(item_id);
                        Some(match email {
                            Some(e) if !e.is_empty() && e != "unknown" => {
                                format!("claude → {id} ({e})")
                            }
                            _ => format!("claude → {id}"),
                        })
                    })
                    .unwrap_or_else(|| format!("claude → {item_id}"));
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
    }

    pub(super) fn render_sidebar(
        &self,
        window_active: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // Ordered groups, INCLUDING empty workspaces (they render with 0 panes).
        let ordered = self.workspaces(cx);
        let by_workspace: Vec<(String, Vec<&Pane>)> = ordered
            .into_iter()
            .map(|ws| {
                let panes: Vec<&Pane> = self.panes.iter().filter(|p| p.workspace == ws).collect();
                (ws, panes)
            })
            .collect();

        let _ = window_active; // focus chrome reserved for future empty-window dimming

        div()
            .id("sidebar")
            .flex_none()
            .w(px(232.))
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
                            .text_color(SeancePalette::flame())
                            .text_lg()
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
                    .flex_1()
                    .overflow_y_scroll()
                    // No horizontal pad — selected workspace fill is full-bleed.
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(by_workspace.into_iter().map(|(workspace, panes)| {
                        let selected = self.selected_workspace.as_deref() == Some(workspace.as_str());
                        let pane_n = panes.len();
                        let ws_for_click = workspace.clone();
                        let ws_for_group_drop = workspace.clone();
                        let ws_for_pane_drop = workspace.clone();
                        let ws_for_menu = workspace.clone();
                        let renaming_this_ws = matches!(
                            &self.renaming,
                            Some((RenameTarget::Workspace(w), _)) if *w == workspace
                        );
                        let rename_input = self.renaming.as_ref().map(|(_, i)| i.clone());
                        // Collapsed workspaces: header only (no pane rows).
                        let header: gpui::AnyElement = if renaming_this_ws {
                            div()
                                .px_2()
                                .py_1p5()
                                .children(rename_input.map(|i| Input::new(&i)))
                                .into_any_element()
                        } else {
                            div()
                                .id(SharedString::from(format!("ws-{workspace}")))
                                .px_2()
                                .py_1p5()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .cursor_pointer()
                                .when(selected, |d| d.bg(selected_row_fill()))
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
                                .drag_over::<DraggedPane>(|style, _, _, _| {
                                    style.bg(SeancePalette::violet_dim())
                                })
                                .on_drop(cx.listener(move |this, drag: &DraggedPane, _, cx| {
                                    ui_debug(&format!(
                                        "drop pane '{}' on workspace header '{}'",
                                        drag.slug, ws_for_pane_drop
                                    ));
                                    this.reorder_pane(&drag.slug, &ws_for_pane_drop, None, cx);
                                }))
                                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                    if event.click_count() == 2 {
                                        this.start_rename(
                                            RenameTarget::Workspace(ws_for_click.clone()),
                                            &ws_for_click.clone(),
                                            window,
                                            cx,
                                        );
                                    } else {
                                        this.select_workspace(&ws_for_click, window, cx);
                                    }
                                }))
                                .context_menu({
                                    let ws_m = ws_for_menu.clone();
                                    let peers: Vec<(String, String)> = self
                                        .windows
                                        .iter()
                                        .filter(|w| Some(w.id.as_str()) != self.window_id.as_deref())
                                        .map(|w| (w.id.clone(), w.label.clone()))
                                        .collect();
                                    move |menu, _, _| {
                                        let mut m = menu
                                            .menu(
                                                "touch (bump recency)",
                                                Box::new(ActTouchWorkspace(ws_m.clone())),
                                            )
                                            .menu(
                                                "rename workspace",
                                                Box::new(ActRenameWorkspace(ws_m.clone())),
                                            )
                                            .menu(
                                                "fork workspace ⑂",
                                                Box::new(ActForkWorkspace(ws_m.clone())),
                                            )
                                            .separator()
                                            .menu(
                                                "send to new window",
                                                Box::new(ActTransferWorkspaceNewWindow(
                                                    ws_m.clone(),
                                                )),
                                            );
                                        for (id, label) in &peers {
                                            m = m.menu(
                                                format!("send to {label}"),
                                                Box::new(ActTransferWorkspace {
                                                    workspace: ws_m.clone(),
                                                    to_window: id.clone(),
                                                }),
                                            );
                                        }
                                        m = m
                                            .menu(
                                                "collect all windows here",
                                                Box::new(ActCollectAllWindows),
                                            )
                                            .separator()
                                            .menu(
                                                "banish workspace (kill all panes)",
                                                Box::new(ActKillWorkspace(ws_m.clone())),
                                            );
                                        m
                                    }
                                })
                                .child(
                                    div()
                                        .flex_none()
                                        .text_sm()
                                        .text_color(if selected {
                                            SeancePalette::flame()
                                        } else {
                                            SeancePalette::text_faint()
                                        })
                                        .child(if selected { "◆" } else { "◈" }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_sm()
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
                                        .child(workspace.clone()),
                                )
                                .children({
                                    // Live badge (working/needs/done) for inactive circles.
                                    let att = if selected {
                                        None
                                    } else {
                                        self.workspace_attention_cx(&workspace, cx)
                                    };
                                    att.map(|a| {
                                        div()
                                            .flex_none()
                                            .px_1()
                                            .rounded_sm()
                                            .text_xs()
                                            .text_color(a.color())
                                            .child(a.label())
                                    })
                                })
                                .child(
                                    // Hover × to banish (only when selected header shows count otherwise).
                                    div()
                                        .id(SharedString::from(format!("ws-banish-{workspace}")))
                                        .flex_none()
                                        .px_1()
                                        .rounded_sm()
                                        .text_xs()
                                        .text_color(SeancePalette::text_faint())
                                        .hover(|s| {
                                            s.text_color(SeancePalette::danger())
                                                .bg(SeancePalette::surface())
                                        })
                                        .cursor_pointer()
                                        .on_click({
                                            let ws = workspace.clone();
                                            cx.listener(move |this, _, _, cx| {
                                                this.kill_workspace(&ws, cx);
                                            })
                                        })
                                        .tooltip(tip("banish workspace (kill all panes)"))
                                        .child("×"),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(if selected {
                                            SeancePalette::text_dim()
                                        } else {
                                            SeancePalette::text_faint()
                                        })
                                        .child(format!("{pane_n}")),
                                )
                                .into_any_element()
                        };
                        div()
                            .id(SharedString::from(format!("wsgroup-{workspace}")))
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .mb_0p5()
                            .drag_over::<DraggedPane>(|style, _, _, _| {
                                style.bg(SeancePalette::surface())
                            })
                            .on_drop(cx.listener(move |this, drag: &DraggedPane, _, cx| {
                                ui_debug(&format!(
                                    "drop pane '{}' on workspace group '{}'",
                                    drag.slug, ws_for_group_drop
                                ));
                                this.reorder_pane(&drag.slug, &ws_for_group_drop, None, cx);
                            }))
                            .child(header)
                    }))
                    // Flex filler: only *blank* sidebar area gets pull/collect menu
                    // (workspace rows have their own menus — don't nest on the scroller).
                    .child({
                        let foreign = self.foreign_workspaces.clone();
                        div()
                            .id("sidebar-empty-hit")
                            .flex_1()
                            .min_h(px(48.))
                            .w_full()
                            .context_menu(move |menu, _, _| {
                                let mut m = menu.menu(
                                    "collect all windows here",
                                    Box::new(ActCollectAllWindows),
                                );
                                if !foreign.is_empty() {
                                    m = m.separator();
                                    for f in &foreign {
                                        m = m.menu(
                                            format!(
                                                "pull «{}» from {}",
                                                f.workspace, f.window_label
                                            ),
                                            Box::new(ActPullWorkspace(f.workspace.clone())),
                                        );
                                    }
                                }
                                m
                            })
                    })
            })
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
