//! Host-provided menus: a launch chip that asks its question only when clicked.
//!
//! Config: the `menus[]` array of `~/.config/seance/host.json` on the DAEMON
//! machine (read over the fs bridge, mtime-watched, same as quicklaunch):
//!
//! ```json
//! {"menus": [{
//!   "id": "meetings",
//!   "title": "meeting",
//!   "list_cmd": "python3 ~/work/vita/scripts/seance_host_meetings.py list",
//!   "select_cmd": "python3 ~/work/vita/scripts/seance_host_meetings.py select {id}",
//!   "empty": "no meetings in the next 7 days"
//! }]}
//! ```
//!
//! Why this is a separate shape from the polled `sidebar[]` widgets: those are
//! ambient state that must always be true, so they cost a poll every few
//! seconds and every item gets a permanent chip. A menu is a *question* — the
//! week's meetings, a list of hosts, whatever a host wants to offer — asked
//! once, when clicked, and answered into a dropdown. Twenty rows in the rail
//! would be a wall; twenty rows in a dropdown is a list.
//!
//! Seance knows nothing about what a menu lists. It runs `list_cmd`, paints
//! `items[]`, and runs `select_cmd` with `{id}` when a row is chosen. The host
//! does the work — including, if it wants, spawning a whole circle via
//! `seance ctl` and naming it back in `workspace` so the rail jumps there.
//! That is the entire contract, and it is why a menu can add a workflow to
//! seance without seance learning the workflow.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, Context, SharedString, Window};
use gpui_component::WindowExt as _;

use crate::host::{HostItem, HostSelectResult};
use crate::theme::SeancePalette;

use super::util::tip_s;
use super::SeanceApp;

/// The open dropdown. Only one menu is open at a time — it is a chip's popover,
/// not a panel.
pub(super) struct HostMenuOpen {
    pub id: String,
    pub title: String,
    /// `None` while `list_cmd` is still running (the dropdown says so).
    pub items: Option<Vec<HostItem>>,
    pub error: Option<String>,
    /// Item id currently in `select_cmd`. A select can take seconds — it may be
    /// spawning a circle — so the row says it's working and a second click is
    /// swallowed. Without this a slow host reads as a dropped click.
    pub busy: Option<String>,
    /// Bumped on every open. A `list_cmd` result carrying a stale token is
    /// dropped rather than painted over a newer menu.
    pub token: u64,
}

/// Rows to draw: each item, preceded by its group heading wherever the host's
/// group changes. Groups are *runs*, not buckets — seance never reorders what
/// a host handed it, so a host that wants clean groups emits them adjacent.
pub(super) fn grouped_rows(items: &[HostItem]) -> Vec<(Option<&str>, &HostItem)> {
    let mut out = Vec::with_capacity(items.len());
    let mut current = "";
    for item in items {
        let head = if !item.group.is_empty() && item.group != current {
            current = item.group.as_str();
            Some(item.group.as_str())
        } else {
            None
        };
        out.push((head, item));
    }
    out
}

/// `{id}` substitution for `select_cmd`. Raw, like the sidebar-widget path —
/// ids are validated shell-safe at parse (`host::safe_item_id`), which is the
/// half of the contract that lets this stay a plain template.
pub(super) fn select_command(template: &str, id: &str) -> String {
    template.replace("{id}", id)
}

/// First non-empty line of command output, for a one-line failure message.
fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(160)
        .collect()
}

/// Row tint, matching the polled-widget strip's vocabulary.
fn item_color(state: &str) -> gpui::Hsla {
    match state {
        "busy" => SeancePalette::danger(),
        "warm" => SeancePalette::flame(),
        "auth" => SeancePalette::violet(),
        "ok" => SeancePalette::success(),
        _ => SeancePalette::text(),
    }
}

impl SeanceApp {
    /// Hot-reload of the DAEMON-side `menus[]`, mirroring the quicklaunch
    /// reload: render only schedules a throttled check, the bridge stat/read
    /// runs off the UI thread, a bad edit keeps the last good list.
    pub(super) fn reload_host_menus_if_stale(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if self
            .host_menus_checked
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(2))
        {
            return;
        }
        self.host_menus_checked = Some(now);
        let client = Arc::clone(&self.client);
        let known = self.host_menus_mtime;
        let path = crate::host::remote_config_path();
        cx.spawn(async move |this, cx| {
            // (new mtime, parsed menus; menus None = keep previous).
            type Outcome = (Option<u64>, Option<Vec<crate::host::HostMenuConfig>>);
            let outcome: Option<Outcome> = cx
                .background_executor()
                .spawn(async move {
                    // File-missing = None; exists-with-unreadable-mtime = Some(0).
                    let cur = match client.fs_stat(&path) {
                        Ok(stat) => stat.map(|m| m.unwrap_or(0)),
                        Err(_) => return None, // bridge down — retry later
                    };
                    if cur == known {
                        return None;
                    }
                    if cur.is_none() {
                        return Some((None, Some(Vec::new())));
                    }
                    match client.fs_read_string(&path) {
                        Ok((s, _)) => match crate::host::parse_menus(&s) {
                            Ok(v) => Some((cur, Some(v))),
                            Err(e) => {
                                eprintln!(
                                    "[seance gui] host.json menus parse error: {e} \
                                     (keeping previous)"
                                );
                                Some((cur, None))
                            }
                        },
                        Err(e) => {
                            eprintln!("[seance gui] host.json read error: {e}");
                            None
                        }
                    }
                })
                .await;
            let Some((mtime, menus)) = outcome else {
                return;
            };
            let Some(this) = this.upgrade() else { return };
            this.update(cx, |app: &mut SeanceApp, cx| {
                app.host_menus_mtime = mtime;
                if let Some(v) = menus {
                    if app.host_menus != v {
                        // A menu that just disappeared can't stay open.
                        if app
                            .host_menu
                            .as_ref()
                            .is_some_and(|open| !v.iter().any(|m| m.id == open.id))
                        {
                            app.host_menu = None;
                        }
                        app.host_menus = v;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Close the open dropdown. Returns whether there was one (Esc's cue to
    /// stop there instead of falling through to the next handler).
    pub(super) fn close_host_menu(&mut self, cx: &mut Context<Self>) -> bool {
        if self.host_menu.is_none() {
            return false;
        }
        self.host_menu = None;
        cx.notify();
        true
    }

    /// Click on a menu chip: close if it's the one already open, else open it
    /// and run its `list_cmd` in the background.
    pub(super) fn toggle_host_menu(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.host_menu.as_ref().is_some_and(|m| m.id == id) {
            self.close_host_menu(cx);
            return;
        }
        let Some(cfg) = self.host_menus.iter().find(|m| m.id == id).cloned() else {
            return;
        };
        self.host_menu_token = self.host_menu_token.wrapping_add(1);
        let token = self.host_menu_token;
        self.host_menu = Some(HostMenuOpen {
            id: cfg.id.clone(),
            title: cfg.title.clone(),
            items: None,
            error: None,
            busy: None,
            token,
        });
        cx.notify();

        let client = Arc::clone(&self.client);
        let cmd = cfg.list_cmd.clone();
        cx.spawn(async move |this, cx| {
            let outcome: Result<Vec<HostItem>, String> = cx
                .background_executor()
                .spawn(async move {
                    let out = client.shell(&cmd).map_err(|e| format!("bridge: {e}"))?;
                    if out.status != Some(0) {
                        let why = first_line(&out.stderr);
                        let code = out.status.map(|c| c.to_string()).unwrap_or("?".into());
                        return Err(if why.is_empty() {
                            format!("list_cmd exit {code}")
                        } else {
                            format!("list_cmd exit {code}: {why}")
                        });
                    }
                    crate::host::parse_menu_items(&out.stdout)
                })
                .await;
            let Some(this) = this.upgrade() else { return };
            this.update(cx, |app: &mut SeanceApp, cx| {
                let Some(open) = app.host_menu.as_mut() else {
                    return;
                };
                if open.token != token {
                    return;
                }
                match outcome {
                    Ok(items) => open.items = Some(items),
                    Err(e) => {
                        open.items = Some(Vec::new());
                        open.error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Chose a row: run `select_cmd` daemon-side (it can take seconds), then
    /// toast the outcome and jump the rail to whatever circle the host names.
    fn host_menu_pick(
        &mut self,
        item_id: &str,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.host_menu.as_ref() else {
            return;
        };
        if open.busy.is_some() {
            return;
        }
        let Some(cfg) = self.host_menus.iter().find(|m| m.id == open.id).cloned() else {
            return;
        };
        if !crate::host::safe_item_id(item_id) {
            return;
        }
        let token = open.token;
        if let Some(open) = self.host_menu.as_mut() {
            open.busy = Some(item_id.to_string());
        }
        cx.notify();

        let client = Arc::clone(&self.client);
        let cmd = select_command(&cfg.select_cmd, item_id);
        let label = label.to_string();
        let task = cx
            .background_executor()
            .spawn(async move { client.shell(&cmd) });
        cx.spawn_in(window, async move |this: gpui::WeakEntity<Self>, cx| {
            let raw = task.await;
            let _ = cx.update(|window, cx| {
                let Some(this) = this.upgrade() else { return };
                this.update(cx, |app: &mut SeanceApp, cx| {
                    // A host may print a result envelope; exit 0 with nothing
                    // is still success. Failure keeps the dropdown open so the
                    // human can read why and pick again.
                    let outcome: Result<HostSelectResult, String> = match raw {
                        Err(e) => Err(format!("bridge: {e}")),
                        Ok(out) => {
                            let res = serde_json::from_str::<HostSelectResult>(out.stdout.trim())
                                .unwrap_or_default();
                            if out.status != Some(0) {
                                let why = res
                                    .error
                                    .clone()
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or_else(|| first_line(&out.stderr));
                                let code = out.status.map(|c| c.to_string()).unwrap_or("?".into());
                                Err(if why.is_empty() {
                                    format!("exit {code}")
                                } else {
                                    why
                                })
                            } else if res.ok == Some(false) {
                                Err(res.error.clone().unwrap_or_else(|| "host said no".into()))
                            } else {
                                Ok(res)
                            }
                        }
                    };
                    match outcome {
                        Ok(res) => {
                            app.host_menu = None;
                            let msg = res
                                .message
                                .filter(|m| !m.is_empty())
                                .unwrap_or_else(|| label.clone());
                            window.push_notification(
                                gpui_component::notification::Notification::success(msg),
                                cx,
                            );
                            if let Some(ws) = res.workspace.filter(|w| !w.is_empty()) {
                                // Pin before select: the circle is already in
                                // its band when the selection lands on it, so
                                // the rail doesn't visibly reshuffle underneath.
                                if res.pin.unwrap_or(true) {
                                    app.pin_workspace(&ws);
                                }
                                app.select_workspace(&ws, window, cx);
                            }
                        }
                        Err(e) => {
                            if let Some(open) = app.host_menu.as_mut() {
                                if open.token == token {
                                    open.busy = None;
                                    open.error = Some(e.clone());
                                }
                            }
                            window.push_notification(
                                gpui_component::notification::Notification::error(e),
                                cx,
                            );
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// One menu chip, with its dropdown when open. Rendered inside the launch
    /// strip's chip row, next to the quicklaunch chips.
    pub(super) fn render_host_menu_chip(
        &self,
        cfg: &crate::host::HostMenuConfig,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let id = cfg.id.clone();
        let open = self.host_menu.as_ref().filter(|m| m.id == id);
        let click_id = id.clone();
        div()
            .id(SharedString::from(format!("hostmenu-{id}")))
            .relative()
            .px_2()
            .py_0p5()
            .rounded_md()
            .text_xs()
            .cursor_pointer()
            .bg(SeancePalette::surface())
            .text_color(if open.is_some() {
                SeancePalette::violet()
            } else {
                SeancePalette::violet_dim()
            })
            .hover(|s| s.bg(SeancePalette::border()))
            .tooltip(tip_s(format!("{} — from the host", cfg.title)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_host_menu(&click_id, cx);
            }))
            // The panel is NOT a child of this chip. Two failed attempts said
            // why: as a chip child it needed `on_mouse_down_out` for dismissal,
            // which fires in the CAPTURE phase for anything outside the *chip's*
            // hitbox — so every click on a row read as a click-away and the row
            // died on mouse-down. Reparenting the dismissal to a scrim fixed
            // that but left the panel `absolute().bottom_full()` inside a
            // `deferred`, whose layout is detached from the chip, so it resolved
            // to nowhere visible. Both problems come from the same root: the
            // panel does not belong to the chip's layout. It is drawn beside the
            // rail, from the app root. See [`Self::render_host_menu_scrim`].
            .child(format!("▾ {}", cfg.title))
            .into_any_element()
    }

    /// The panel: positioned against the window, beside the rail and at its
    /// foot, where the launch strip is. Not anchored to the chip — see the
    /// note on [`Self::render_host_menu_chip`] for the two bugs that cost.
    fn render_host_menu_panel(
        &self,
        cfg: &crate::host::HostMenuConfig,
        open: &HostMenuOpen,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let body: gpui::AnyElement = match &open.items {
            None => div()
                .p_2()
                .text_xs()
                .text_color(SeancePalette::text_faint())
                .child("…")
                .into_any_element(),
            Some(items) if items.is_empty() => div()
                .p_2()
                .text_xs()
                .text_color(SeancePalette::text_faint())
                .child(
                    cfg.empty
                        .clone()
                        .filter(|e| !e.is_empty())
                        .unwrap_or_else(|| "nothing to show".into()),
                )
                .into_any_element(),
            Some(items) => div()
                .id(SharedString::from(format!("hostmenu-list-{}", open.id)))
                .max_h(px(400.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .py_1()
                .children(
                    grouped_rows(items)
                        .into_iter()
                        .map(|(head, item)| self.render_host_menu_row(head, item, open, cx)),
                )
                .into_any_element(),
        };
        div()
            .id(SharedString::from(format!("hostmenu-drop-{}", open.id)))
            .absolute()
            // Beside the rail, sitting on the bottom edge: the launch strip is
            // down there, so this reads as belonging to the chip that opened it
            // without depending on the chip's own layout for a single pixel.
            .left(px(super::sidebar::RAIL_WIDTH + 8.))
            .bottom(px(8.))
            .w(px(340.))
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(SeancePalette::flame_dim())
            .bg(SeancePalette::bg_elevated())
            // Own the mouse over the panel so a click inside it isn't also a
            // click on the scrim underneath.
            .occlude()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(SeancePalette::border())
                    .text_xs()
                    .text_color(SeancePalette::text_faint())
                    .child(open.title.clone()),
            )
            .child(body)
            .children(open.error.as_ref().map(|e| {
                // (error line, below the body)
                div()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(SeancePalette::border())
                    .text_xs()
                    .text_color(SeancePalette::danger())
                    .child(e.clone())
            }))
            .into_any_element()
    }

    /// The open menu, mounted from `render()` at the app root: a full-window
    /// click-away catcher with the panel inside it.
    ///
    /// Invisible and non-dimming — the scrim exists only to own the mouse
    /// everywhere the panel isn't. The panel is its child, so it paints after
    /// it and takes the clicks over its own area; everything else closes the
    /// menu. That includes the chip, which now sits *under* the scrim, so
    /// clicking it while open closes the menu rather than closing and
    /// immediately reopening it.
    pub(super) fn render_host_menu_scrim(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let open = self.host_menu.as_ref()?;
        let cfg = self.host_menus.iter().find(|m| m.id == open.id)?;
        Some(
            div()
                .id("hostmenu-scrim")
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.close_host_menu(cx);
                    }),
                )
                .on_mouse_down(
                    gpui::MouseButton::Right,
                    cx.listener(|this, _, _, cx| {
                        this.close_host_menu(cx);
                    }),
                )
                .child(self.render_host_menu_panel(cfg, open, cx))
                .into_any_element(),
        )
    }

    fn render_host_menu_row(
        &self,
        head: Option<&str>,
        item: &HostItem,
        open: &HostMenuOpen,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let busy = open.busy.as_deref() == Some(item.id.as_str());
        let pending = open.busy.is_some();
        let item_id = item.id.clone();
        let label = item.label.clone();
        let click_label = label.clone();
        let details: Vec<String> = [item.detail.clone(), item.detail2.clone()]
            .into_iter()
            .filter(|d| !d.is_empty())
            .collect();
        let row = div()
            .id(SharedString::from(format!("hostmenu-row-{}", item.id)))
            .px_2()
            .py_1()
            .flex()
            .flex_col()
            .gap_0p5()
            .when(!pending, |d| {
                d.cursor_pointer().hover(|s| s.bg(SeancePalette::surface()))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.host_menu_pick(&item_id, &click_label, window, cx);
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(if busy {
                        SeancePalette::flame()
                    } else {
                        item_color(&item.state)
                    })
                    .child(if busy {
                        format!("… {label}")
                    } else {
                        label.clone()
                    }),
            )
            .children(details.into_iter().map(|d| {
                div()
                    .text_xs()
                    .text_color(SeancePalette::text_faint())
                    .child(d)
            }));
        match head {
            None => row.into_any_element(),
            Some(h) => div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .px_2()
                        .pt_1p5()
                        .pb_0p5()
                        .text_xs()
                        .text_color(SeancePalette::text_faint())
                        .child(h.to_string()),
                )
                .child(row)
                .into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, group: &str) -> HostItem {
        HostItem {
            id: id.into(),
            label: id.into(),
            group: group.into(),
            ..Default::default()
        }
    }

    #[test]
    fn grouping_emits_a_heading_only_when_the_run_changes() {
        let items = vec![
            item("a", "wed"),
            item("b", "wed"),
            item("c", "thu"),
            item("d", ""),
        ];
        let rows = grouped_rows(&items);
        let heads: Vec<Option<&str>> = rows.iter().map(|(h, _)| *h).collect();
        assert_eq!(heads, [Some("wed"), None, Some("thu"), None]);
        // Every item is still rendered, exactly once, in host order.
        let ids: Vec<&str> = rows.iter().map(|(_, i)| i.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
    }

    #[test]
    fn grouping_reopens_a_heading_when_a_group_recurs_after_a_gap() {
        // Runs, not buckets: a host that interleaves gets what it asked for.
        let items = vec![item("a", "wed"), item("b", "thu"), item("c", "wed")];
        let heads: Vec<Option<&str>> = grouped_rows(&items).iter().map(|(h, _)| *h).collect();
        assert_eq!(heads, [Some("wed"), Some("thu"), Some("wed")]);
    }

    #[test]
    fn grouping_handles_empty_input() {
        assert!(grouped_rows(&[]).is_empty());
    }

    #[test]
    fn select_command_substitutes_every_id_placeholder() {
        assert_eq!(
            select_command("adapter select {id} --ref {id}", "mtg:2026-08-06/l10"),
            "adapter select mtg:2026-08-06/l10 --ref mtg:2026-08-06/l10"
        );
        // A template without the placeholder is left alone (host's choice).
        assert_eq!(select_command("adapter go", "x"), "adapter go");
    }

    #[test]
    fn first_line_takes_the_first_non_empty_and_clips() {
        assert_eq!(first_line("\n\n  boom  \nnext"), "boom");
        assert_eq!(first_line(""), "");
        assert_eq!(first_line(&"x".repeat(400)).len(), 160);
    }
}
