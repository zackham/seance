//! Pane chrome, overlays, and strips for the SeanceApp view: the per-pane
//! tile (`render_pane`), the drawer close bar, the grimoire help window, and
//! the minimize / asks / activity / stage strips.

use std::time::Duration;

use gpui::{
    div, ease_in_out, prelude::*, px, Animation, AnimationExt as _, Context, Entity, Render,
    SharedString, Window,
};
use gpui_component::{input::InputState, Colorize as _, StyledExt as _, WindowExt as _};

use crate::events;
use crate::pane::Pane;
use crate::scratchpad::ScratchpadDrawer;
use crate::theme::SeancePalette;

use super::util::{selected_row_fill, status_color, tip};
use super::{Drawer, OwnerChrome, PaneStatus, SeanceApp};

/// The grimoire in its own window.
pub struct HelpWindow;

impl Render for HelpWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("grimoire-window")
            .size_full()
            .overflow_y_scroll()
            .bg(SeancePalette::bg())
            .child(render_help())
    }
}

impl SeanceApp {
    /// Minimize shelf: chips for shelved panes in the selected circle only.
    /// Hidden entirely when nothing is minimized.
    pub(super) fn render_minimize_shelf(
        &self,
        window_active: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let ws = self.selected_workspace.clone();
        let shelved: Vec<&Pane> = self
            .panes
            .iter()
            .filter(|p| {
                !p.tiled && p.popped.is_none() && ws.as_ref().is_none_or(|w| p.workspace == *w)
            })
            .collect();
        if shelved.is_empty() {
            return div().into_any_element();
        }
        let active = if window_active {
            self.active_slug.clone()
        } else {
            None
        };
        div()
            .id("minimize-shelf")
            .flex_none()
            .px_2()
            .py_1()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_1()
            .border_b_1()
            .border_color(SeancePalette::border())
            .bg(SeancePalette::bg_elevated())
            .children(shelved.into_iter().map(|pane| {
                let slug = pane.slug.clone();
                let name = pane.name.clone();
                let is_active = active.as_deref() == Some(slug.as_str());
                div()
                    .id(SharedString::from(format!("shelf-{slug}")))
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .text_xs()
                    .cursor_pointer()
                    .bg(if is_active {
                        selected_row_fill()
                    } else {
                        SeancePalette::surface()
                    })
                    .text_color(if is_active {
                        SeancePalette::flame()
                    } else {
                        SeancePalette::text_dim()
                    })
                    .hover(|s| s.bg(SeancePalette::surface().lighten(0.05)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // Click-to-show: re-tile shelved pane.
                        this.toggle_tiled(&slug, cx);
                        this.set_active(&slug, window, cx);
                    }))
                    .child(name)
                    .into_any_element()
            }))
            .into_any_element()
    }

    /// Unanswered agent questions for the selected workspace, as a toast strip.
    pub(super) fn render_asks(&self, cx: &Context<Self>) -> Vec<gpui::AnyElement> {
        self.asks
            .iter()
            .filter(|a| a.answer.is_none())
            .filter(|a| {
                a.workspace.is_none()
                    || self.selected_workspace.is_none()
                    || a.workspace == self.selected_workspace
            })
            .map(|ask| {
                let id = ask.id.clone();
                let mut row = div()
                    .flex_none()
                    .mx_1()
                    .mt_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(SeancePalette::violet_dim())
                    .bg(SeancePalette::bg_elevated())
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .text_color(SeancePalette::violet())
                            .child("❓"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(SeancePalette::text())
                            .child(format!("{} asks: {}", ask.from, ask.question)),
                    );
                let choices: Vec<String> = if ask.choices.is_empty() {
                    vec!["ok".to_string(), "no".to_string()]
                } else {
                    ask.choices.clone()
                };
                for choice in choices {
                    let id2 = id.clone();
                    let label = choice.clone();
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("ask-{id2}-{label}")))
                            .flex_none()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_sm()
                            .text_color(SeancePalette::flame())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.answer_ask(&id2, label.clone(), cx);
                            }))
                            .child(choice),
                    );
                }
                row.into_any_element()
            })
            .collect()
    }

    pub(super) fn render_activity(&self) -> gpui::AnyElement {
        let entries = events::read(0, self.selected_workspace.as_deref(), None, None, 60);
        div()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .children(entries.into_iter().rev().map(|e| {
                let actor_color = if e.actor == "human" {
                    SeancePalette::flame()
                } else if e.actor.starts_with("agent:") {
                    SeancePalette::violet()
                } else {
                    SeancePalette::text_faint()
                };
                div()
                    .flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .flex_none()
                            .text_color(SeancePalette::text_faint())
                            .child(events::fmt_time(e.ts)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(90.))
                            .overflow_hidden()
                            .text_color(actor_color)
                            .child(e.actor.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(SeancePalette::text_dim())
                            .child(e.detail.clone()),
                    )
            }))
            .into_any_element()
    }
    /// Stage strip — only when something needs the human.
    /// Human-only shells stay clean (no second roster). Shows chips for
    /// needs-human / blocked / risky in the selected workspace.
    pub(super) fn render_stage_strip(
        &self,
        window_active: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let ws = self.selected_workspace.clone();
        let mut rows: Vec<(&Pane, Option<&PaneStatus>)> = self
            .panes
            .iter()
            .filter(|p| ws.as_ref().is_none_or(|w| p.workspace == *w))
            .map(|p| (p, self.statuses.get(&p.slug)))
            .filter(|(_, st)| {
                matches!(
                    st.map(|s| s.state.as_str()),
                    Some("needs-human" | "blocked" | "risky")
                )
            })
            .collect();
        rows.sort_by_key(|(p, st)| {
            let urgency = match st.map(|s| s.state.as_str()) {
                Some("needs-human") => 0,
                Some("blocked") | Some("risky") => 1,
                _ => 2,
            };
            (urgency, p.name.clone())
        });
        if rows.is_empty() {
            return div().flex_none().into_any_element();
        }
        let active = if window_active {
            self.active_slug.clone()
        } else {
            None
        };
        div()
            .id("stage-strip")
            .flex_none()
            .w_full()
            .px_1()
            .pt_1()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_1()
            .children(rows.into_iter().map(|(pane, st)| {
                let slug = pane.slug.clone();
                let is_active = active.as_deref() == Some(slug.as_str());
                let state = st.map(|s| s.state.as_str()).unwrap_or("-");
                let note = st.and_then(|s| s.note.as_deref()).unwrap_or("");
                let color = status_color(state);
                let label = if note.is_empty() {
                    format!("{} · {state}", pane.name)
                } else {
                    let n = if note.len() > 28 {
                        let mut end = 28;
                        while end > 0 && !note.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}…", &note[..end])
                    } else {
                        note.to_string()
                    };
                    format!("{} · {state} · {n}", pane.name)
                };
                div()
                    .id(SharedString::from(format!("stage-{slug}")))
                    .flex_none()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_active {
                        SeancePalette::flame()
                    } else {
                        SeancePalette::border()
                    })
                    .bg(SeancePalette::surface())
                    .text_xs()
                    .text_color(color)
                    .cursor_pointer()
                    .hover(|s| s.bg(SeancePalette::border()))
                    .tooltip(tip("click focus + pad drawer · double-click zoom"))
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            if event.click_count() >= 2 {
                                this.toggle_zoom(&slug, cx);
                            } else {
                                this.focus_pane_slug(&slug, window, cx);
                                this.open_pad_drawer(&slug, cx);
                            }
                        }),
                    )
                    .child(label)
                    .into_any_element()
            }))
            .into_any_element()
    }
}

pub(super) fn render_pane(
    pane: &Pane,
    active: Option<&str>,
    status: Option<&PaneStatus>,
    owner: Option<&OwnerChrome>,
    touch: Option<&(String, String, std::time::Instant)>,
    whisper: Option<&Entity<InputState>>,
    flipped: Option<&Entity<ScratchpadDrawer>>,
    is_zoomed: bool,
    cx: &Context<SeanceApp>,
) -> impl IntoElement {
    let is_active = active == Some(pane.slug.as_str());
    let is_flipped = flipped.is_some();
    let _ = whisper; // whisper UI retired — keep arg for call-site stability
    let slug = pane.slug.clone();
    let running = pane.is_running(cx);
    let title = pane.title(cx).unwrap_or_else(|| pane.command.clone());
    // Daemon-backed terminal panes get arm/phone chrome.
    let has_terminal = pane.remote_terminal().is_some();
    let exited = owner.map(|o| o.exited).unwrap_or(false);
    let owner_label = owner.map(|o| {
        if o.exited {
            match o.exit_code {
                Some(c) => format!("☠ exit {c}"),
                None => "☠ exited".into(),
            }
        } else if o.owner == "human" {
            "⌨ you".into()
        } else if o.owner == "none" {
            "· free".into()
        } else if o.owner.starts_with("agent:") || o.owner == "cli" {
            format!("⚡ {}", o.owner.trim_start_matches("agent:"))
        } else {
            o.owner.clone()
        }
    });
    // Frame border prioritizes focus, not ownership — ownership stays in the
    // header chip (⌨/⚡). Inactive panes share one quiet border so active is
    // obvious at a glance. Zoom mode uses a loud flame ring only while the
    // OS window is focused (`is_active` is already gated on window_active).
    let frame_border = if exited {
        SeancePalette::danger()
    } else if is_active {
        if is_flipped {
            SeancePalette::violet()
        } else {
            SeancePalette::flame()
        }
    } else if is_zoomed {
        // Zoomed but window unfocused — keep a quiet ring, not "has focus".
        SeancePalette::border()
    } else {
        SeancePalette::border()
    };

    // Body: notes face if flipped, otherwise the terminal/file content.
    // Soft fade when the notes face appears (cheap stand-in for a card flip).
    let body: gpui::AnyElement = if let Some(notes) = flipped {
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(SeancePalette::bg_elevated())
            .child(notes.clone())
            .with_animation(
                SharedString::from(format!("flip-in-{slug}")),
                Animation::new(Duration::from_millis(220)).with_easing(ease_in_out),
                |this, delta| this.opacity(0.35 + 0.65 * delta),
            )
            .into_any_element()
    } else {
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(pane.content_element())
            .into_any_element()
    };

    div()
        .id(SharedString::from(format!("pane-{slug}")))
        .flex_1()
        .min_h_0()
        // Allow shrinking below content min-size (default flex min is
        // min-content — terminal cols / long markdown lines). Soft floor
        // keeps a sliver of chrome visible without pinning panes wide.
        .min_w(px(48.))
        .w_full()
        .overflow_hidden()
        .flex()
        .flex_col()
        .rounded_md()
        // Always 2px border so focus only recolors — never reflows terminal
        // cols (border_1 → border_2 used to steal a cell of width).
        .border_2()
        .border_color(frame_border)
        .bg(SeancePalette::bg())
        .opacity(if exited {
            0.72
        } else if is_active {
            1.0
        } else {
            0.88
        })
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener({
                let slug = slug.clone();
                move |this, _, window, cx| {
                    this.set_active(&slug, window, cx);
                }
            }),
        )
        .child(
            // Pane title strip.
            div()
                .flex_none()
                .h(px(26.))
                .px_2()
                .flex()
                .items_center()
                .gap_1p5()
                .min_w_0()
                .overflow_hidden()
                .bg(if is_active {
                    SeancePalette::surface()
                } else if is_zoomed {
                    SeancePalette::bg_elevated()
                } else {
                    SeancePalette::bg_elevated()
                })
                .when(is_zoomed, |d| {
                    d.child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(if is_active {
                                SeancePalette::flame()
                            } else {
                                SeancePalette::text_faint()
                            })
                            .child("⛶"),
                    )
                })
                .children(owner_label.map(|lab| {
                    div()
                        .flex_none()
                        .text_xs()
                        .whitespace_nowrap()
                        .text_color(if exited {
                            SeancePalette::danger()
                        } else if !is_active {
                            SeancePalette::text_faint()
                        } else if lab.starts_with('⌨') {
                            SeancePalette::flame()
                        } else if lab.starts_with('⚡') {
                            SeancePalette::violet()
                        } else {
                            SeancePalette::text_faint()
                        })
                        .child(lab)
                }))
                .child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(if running {
                            if is_active {
                                SeancePalette::flame()
                            } else {
                                SeancePalette::text_faint()
                            }
                        } else {
                            SeancePalette::status_exited()
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(if is_active && is_flipped {
                            SeancePalette::violet()
                        } else if is_active {
                            SeancePalette::text()
                        } else {
                            SeancePalette::text_faint()
                        })
                        .truncate()
                        .child(if is_flipped {
                            format!("{} — notes (back)", pane.name)
                        } else {
                            format!("{} — {}", pane.name, title)
                        }),
                )
                .children(status.map(|st| {
                    div()
                        .flex_none()
                        .px_1p5()
                        .rounded_md()
                        .text_xs()
                        .text_color(status_color(&st.state))
                        .bg(SeancePalette::bg())
                        .child(st.state.clone())
                }))
                .children(touch.map(|(verb, actor, _)| {
                    div()
                        .flex_none()
                        .px_1p5()
                        .rounded_md()
                        .text_xs()
                        .text_color(SeancePalette::violet())
                        .bg(SeancePalette::bg())
                        .child(format!("{verb} by {actor}"))
                }))
                // Arm: one-click seance orientation (terminals only).
                .when(has_terminal && !is_flipped, |d| {
                    d.child(
                        div()
                            .id(SharedString::from(format!("arm-strip-{slug}")))
                            .flex_none()
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .hover(|s| s.text_color(SeancePalette::flame()))
                            .cursor_pointer()
                            .on_click(cx.listener({
                                let slug = slug.clone();
                                move |this, _, _, cx| {
                                    this.arm_pane(&slug, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .tooltip(tip(
                                "arm — inject seance orientation so the agent uses ctl / workspace",
                            ))
                            .child("⚡"),
                    )
                })
                // Phone: one-button telegram topic (vita seam).
                .when(has_terminal, |d| {
                    let linked = SeanceApp::phone_linked(&slug).is_some();
                    d.child(
                        div()
                            .id(SharedString::from(format!("phone-{slug}")))
                            .flex_none()
                            .text_xs()
                            .text_color(if linked {
                                SeancePalette::violet()
                            } else {
                                SeancePalette::text_faint()
                            })
                            .hover(|s| s.text_color(SeancePalette::violet()))
                            .cursor_pointer()
                            .on_click(cx.listener({
                                let slug = slug.clone();
                                move |this, _, _, cx| {
                                    this.phone_pane(&slug, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .tooltip(tip(
                                "phone — open a telegram topic seeded with workspace roster + seance ctl how-to",
                            ))
                            .child("☎"),
                    )
                })
                // Pad drawer (quick inspect without flip).
                .child(
                    div()
                        .id(SharedString::from(format!("pad-chip-{slug}")))
                        .flex_none()
                        .text_xs()
                        .text_color(SeancePalette::text_faint())
                        .hover(|s| s.text_color(SeancePalette::flame()))
                        .cursor_pointer()
                        .on_click(cx.listener({
                            let slug = slug.clone();
                            move |this, _, _, cx| {
                                this.open_pad_drawer(&slug, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .tooltip(tip("pad drawer — task + scratchpad tail"))
                        .child("▤"),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("popout-{slug}")))
                        .flex_none()
                        .text_xs()
                        .text_color(SeancePalette::text_faint())
                        .hover(|s| s.text_color(SeancePalette::flame()))
                        .cursor_pointer()
                        .on_click(cx.listener({
                            let slug = slug.clone();
                            move |this, _, _, cx| {
                                this.pop_out(&slug, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .tooltip(tip("pop out to its own window (ctrl+shift+p)"))
                        .child("⇱"),
                )
                // Notes flip — prominent when flipped (violet "back" affordance).
                .child(
                    div()
                        .id(SharedString::from(format!("notes-{slug}")))
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .text_xs()
                        .text_color(if is_flipped {
                            SeancePalette::bg()
                        } else {
                            SeancePalette::text_faint()
                        })
                        .when(is_flipped, |d| d.bg(SeancePalette::violet()))
                        .hover(|s| {
                            if is_flipped {
                                s.bg(SeancePalette::violet_dim())
                            } else {
                                s.text_color(SeancePalette::flame())
                            }
                        })
                        .cursor_pointer()
                        // Stop mouse-down so a drag that starts on the chip
                        // doesn't become a text selection on the face content
                        // when the flip reveals markdown underneath.
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|_this, _, window, cx| {
                                window.end_text_selection(cx);
                                window.clear_text_selection(cx);
                                cx.stop_propagation();
                            }),
                        )
                        .on_click(cx.listener({
                            let slug = slug.clone();
                            move |this, _, window, cx| {
                                this.flip_notes_for(&slug, window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .tooltip(tip(if is_flipped {
                            "flip back to the terminal (ctrl+shift+s)"
                        } else {
                            "flip pane over — notes on the back (ctrl+shift+s)"
                        }))
                        .child(if is_flipped { "↻ face" } else { "✎ notes" }),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("shelve-{slug}")))
                        .flex_none()
                        .text_xs()
                        .text_color(SeancePalette::text_faint())
                        .hover(|s| s.text_color(SeancePalette::flame()))
                        .cursor_pointer()
                        .on_click(cx.listener({
                            let slug = slug.clone();
                            move |this, _, _, cx| {
                                this.toggle_tiled(&slug, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .tooltip(tip("shelve this pane (back via sidebar click)"))
                        .child("▣"),
                ),
        )
        .child(body)
}

pub(super) fn drawer_close_bar(title: &'static str, cx: &Context<SeanceApp>) -> impl IntoElement {
    div()
        .flex_none()
        .h(px(30.))
        .px_3()
        .flex()
        .items_center()
        .justify_between()
        .border_b_1()
        .border_color(SeancePalette::border())
        .child(
            div()
                .text_xs()
                .text_color(SeancePalette::text_faint())
                .child(title),
        )
        .child(
            div()
                .id(SharedString::from(format!("close-drawer-{title}")))
                .px_1()
                .text_sm()
                .text_color(SeancePalette::text_faint())
                .hover(|s| s.text_color(SeancePalette::flame()))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.drawer = Drawer::Closed;
                    this.persist(cx);
                    cx.notify();
                }))
                .child("✕"),
        )
}

pub(super) fn render_help() -> gpui::AnyElement {
    fn h1(title: &'static str) -> gpui::Div {
        div()
            .pt_4()
            .pb_1()
            .text_sm()
            .font_semibold()
            .text_color(SeancePalette::text())
            .child(title)
    }
    fn section(title: &'static str) -> gpui::Div {
        div()
            .pt_3()
            .pb_1()
            .text_xs()
            .font_semibold()
            .text_color(SeancePalette::violet())
            .child(title)
    }
    fn row(key: &'static str, desc: &'static str) -> gpui::Div {
        div()
            .flex()
            .gap_2()
            .py_0p5()
            .text_sm()
            .child(
                div()
                    .flex_none()
                    .w(px(168.))
                    .text_color(SeancePalette::flame())
                    .child(key),
            )
            .child(div().text_color(SeancePalette::text_dim()).child(desc))
    }
    fn p(text: &'static str) -> gpui::Div {
        div()
            .text_sm()
            .text_color(SeancePalette::text_dim())
            .pb_1()
            .child(text)
    }
    fn bullet(text: &'static str) -> gpui::Div {
        div()
            .flex()
            .gap_2()
            .text_sm()
            .text_color(SeancePalette::text_dim())
            .child(
                div()
                    .flex_none()
                    .text_color(SeancePalette::flame_dim())
                    .child("·"),
            )
            .child(div().child(text))
    }

    div()
        .p_4()
        .flex()
        .flex_col()
        .gap_0p5()
        // ── title ──────────────────────────────────────────────────────────
        .child(
            div()
                .text_lg()
                .font_semibold()
                .text_color(SeancePalette::text())
                .child("✦ grimoire — seance"),
        )
        .child(p(
            "a native human+AI co-working playground. every pane is live on your \
             screen; every agent action and every human click can flow through one \
             control plane we fully own.",
        ))
        // ── what is this ───────────────────────────────────────────────────
        .child(h1("what seance is"))
        .child(bullet(
            "panes — terminal sessions (claude / codex / grok / shell) or live file viewers",
        ))
        .child(bullet(
            "workspaces — named circles; the tiling grid shows only the selected one",
        ))
        .child(bullet(
            "control plane — seance ctl over a unix socket so agents drive sibling panes",
        ))
        .child(bullet(
            "notes flip — each pane has a shared markdown scratchpad on its back",
        ))
        .child(bullet(
            "attribution — human + agent actions land in one event log (activity drawer ≋)",
        ))
        // ── pane chrome ────────────────────────────────────────────────────
        .child(h1("pane chrome (title strip)"))
        .child(row("⚡", "arm — one-click inject seance orientation into this agent"))
        .child(row("☎", "phone — telegram topic + stage card (roster/ctl how-to; no participant claim)"))
        .child(row("▤", "pad drawer — task inject body + scratchpad tail"))
        .child(row("💬", "whisper — open a compose bar; Enter injects into the agent"))
        .child(row("stage chip click", "focus pane + open pad drawer (double-click zooms)"))
        .child(row("✎ notes", "flip the pane over onto its notes face"))
        .child(row("↻ face", "flip back from notes to the terminal"))
        .child(row("⇱", "pop the pane into its own OS window (ctrl+shift+p)"))
        .child(row("▣", "shelve / tile (sidebar click re-shows a shelved pane)"))
        .child(row("status badge", "agent-reported state via ctl status-set"))
        .child(row("⚡ driven / 👁", "transient flash when another pane touches this one"))
        // ── notes flip ─────────────────────────────────────────────────────
        .child(h1("notes — the back of a pane"))
        .child(p(
            "notes are not a side drawer. click ✎ notes (or ctrl+shift+s on the \
             active pane) to flip the pane over. the face is a live markdown \
             scratchpad at ~/.local/share/seance/scratch/<slug>.md. the agent \
             in that pane sees the same file via $SEANCE_SCRATCHPAD — writes \
             appear live on both sides (1s poll, last-writer-wins).",
        ))
        .child(bullet("click ↻ face or press ctrl+shift+s again to flip back"))
        .child(bullet("violet border = notes face is up"))
        .child(bullet("right-click a sidebar row → flip notes ✎"))
        // ── whisper + arm ──────────────────────────────────────────────────
        .child(h1("whisper + arm — talking to an agent"))
        .child(p(
            "whisper is for mid-flight steers that should land in the agent's \
             prompt without you fighting its TUI. click 💬 on a terminal pane: \
             a compose bar appears at the bottom of that pane. type, press Enter \
             — seance bracketed-pastes `[whisper from zack] …` and submits. \
             empty Enter / Esc / ✕ cancels.",
        ))
        .child(p(
            "arm (⚡) is the one-click version of “you are in seance — use it.” \
             it injects a short orientation prompt that tells the agent about \
             $SEANCE_* env vars, to run `seance ctl skill`, prefer propose for \
             risky commands, and write notes to $SEANCE_SCRATCHPAD. use it the \
             moment you drop a fresh claude into a pane and want it oriented.",
        ))
        .child(bullet("arm is also available as a chip on the open whisper bar"))
        .child(bullet(
            "for durable notes the agent should keep, prefer the notes flip — not whisper",
        ))
        .child(bullet(
            "ghost propose (ctl propose) is the agent→human safe path: dimmed text, Enter/Esc",
        ))
        // ── workspaces ─────────────────────────────────────────────────────
        .child(h1("workspaces"))
        .child(row("click header", "select workspace (tiling region filters to it)"))
        .child(row("double-click", "rename workspace inline"))
        .child(row("drag header", "reorder workspaces in the sidebar"))
        .child(row("drag pane row", "move pane into another workspace / reorder"))
        .child(row("right-click header", "rename · fork ⑂ · banish (kill all panes)"))
        .child(row("+ (footer)", "new empty workspace"))
        .child(p(
            "banish workspace kills every pane under it (PTYs shut down), removes \
             the workspace from the sidebar, and selects another. irreversible \
             for the processes — scratchpad files on disk are kept.",
        ))
        // ── keys ───────────────────────────────────────────────────────────
        .child(h1("keys"))
        .child(section("global"))
        .child(row("ctrl+shift+n", "summon a new shell pane in the current workspace"))
        .child(row("ctrl+shift+w", "banish (kill) the active pane"))
        .child(row("ctrl+shift+s", "flip notes on the active pane / flip back"))
        .child(row("ctrl+shift+p", "pop active pane out / return to the circle"))
        .child(row("ctrl+shift+k", "precanned prompt palette"))
        .child(row("ctrl+shift+j", "fuzzy jump to pane / workspace"))
        .child(row("ctrl+shift+z", "focus-zoom active pane (esc unzoom)"))
        .child(row("ctrl+shift+f", "jump to last failed shell command"))
        .child(row("ctrl+pgup / pgdn", "previous / next workspace (sidebar order, wraps)"))
        .child(row("ctrl+shift+pgup / pgdn", "previous / next pane in this workspace"))
        .child(row("escape", "dismiss whisper / palette / unzoom"))
        .child(section("terminal focus"))
        .child(row("ctrl+shift+c / v", "copy selection / paste"))
        .child(row("shift+pgup/pgdn", "scrollback"))
        .child(row("ctrl+click / middle-click", "open OSC-8 / URL under cursor"))
        .child(row("mouse drag", "select text (copies on release)"))
        .child(row("wheel", "scroll scrollback"))
        .child(row("2-pane sash", "drag the vertical divider to resize"))
        .child(section("ghost command (agent proposed)"))
        .child(row("enter / tab", "accept + run the dimmed ghost command"))
        .child(row("escape", "dismiss the proposal"))
        .child(row("type", "override — typing clears the ghost"))
        // ── control plane ──────────────────────────────────────────────────
        .child(h1("control plane — seance ctl"))
        .child(p(
            "any process inside a pane (or outside, unscoped) can drive the circle \
             via `seance ctl …` over $XDG_RUNTIME_DIR/seance.sock. inside a pane, \
             calls are auto-scoped to $SEANCE_WORKSPACE; pass --all to cross.",
        ))
        .child(section("discovery + lifecycle"))
        .child(row("ctl list", "panes in scope (+ state, kind, workspace)"))
        .child(row("ctl new --name N", "spawn (--command, --cwd, --workspace, --file PATH)"))
        .child(row("ctl status P", "running/exited, title, popped"))
        .child(row("ctl kill P", "terminate a pane"))
        .child(row("ctl human", "where is the human? focus + workspace + pending asks"))
        .child(section("drive + observe"))
        .child(row("ctl send P TEXT", "bracketed-paste + submit (—no-submit stages)"))
        .child(row("ctl send-raw P $'\\x03'", "raw keys: Ctrl-C, Enter, Esc, arrows"))
        .child(row("ctl read P [--lines N]", "rendered visible screen (truth for agents)"))
        .child(row("ctl propose P CMD", "ghost text; blocks until human accepts/rejects"))
        .child(section("human↔agent surfaces"))
        .child(row("ctl ask \"Q\" --choices a,b", "toast with buttons; CLI blocks for answer"))
        .child(row("ctl status-set STATE", "planning|working|blocked|needs-human|done|idle"))
        .child(row("ctl scratchpad P", "path of that pane's shared notes file"))
        .child(row("ctl timeline --since 10m", "attributed event log (human + agent)"))
        .child(row("ctl fork [--name N]", "fork a workspace: panes respawn, notes copy"))
        .child(row("ctl skill", "print the agent-facing driving guide (paste target)"))
        .child(row("ctl commands P", "structured shell history from shell integration"))
        .child(row("ctl last-command P", "most recent {command,cwd,exit,duration_ms}"))
        .child(section("the loop that works (for agents)"))
        .child(bullet("spawn:  seance ctl new --name worker-1 --cwd /path --command claude"))
        .child(bullet("task:   seance ctl send worker-1 \"…\""))
        .child(bullet("poll:   seance ctl read worker-1 --lines 40  until idle / prompt"))
        .child(bullet("collect: echo result >> $SEANCE_SCRATCHPAD"))
        .child(bullet("clean:  seance ctl kill worker-1"))
        // ── env ────────────────────────────────────────────────────────────
        .child(h1("environment every pane gets"))
        .child(row("$SEANCE_SESSION", "this pane's slug"))
        .child(row("$SEANCE_WORKSPACE", "workspace name (auto-scopes ctl)"))
        .child(row("$SEANCE_SCRATCHPAD", "absolute path to shared notes file"))
        .child(row("$SEANCE_SOCKET", "control socket path"))
        // ── files ──────────────────────────────────────────────────────────
        .child(h1("where things live on disk"))
        .child(row("state", "~/.local/share/seance/state.json"))
        .child(row("notes", "~/.local/share/seance/scratch/<slug>.md"))
        .child(row("events", "~/.local/share/seance/events.jsonl"))
        .child(row("file history", "~/.local/share/seance/filehist/"))
        .child(row("socket", "$XDG_RUNTIME_DIR/seance.sock"))
        // ── file panes ─────────────────────────────────────────────────────
        .child(h1("file panes"))
        .child(p(
            "seance ctl new --name doc --file PATH opens a live viewer (markdown \
             rendered) with mtime poll + history snapshots (◀/▶). no PTY. use \
             when an agent is editing a file you want to watch.",
        ))
        // ── activity ───────────────────────────────────────────────────────
        .child(h1("activity + asks"))
        .child(bullet("≋ in the footer opens the activity drawer (event feed)"))
        .child(bullet(
            "agents call ctl ask → a toast with choice buttons appears above the tiles",
        ))
        .child(bullet("you click; the blocking ctl call returns the answer"))
        // ── tips ───────────────────────────────────────────────────────────
        .child(h1("tips"))
        .child(bullet(
            "fresh claude pane → hit ⚡ arm first, then give the real task via whisper or typing",
        ))
        .child(bullet(
            "prefer ghost propose (from agents) over silent send for anything destructive",
        ))
        .child(bullet(
            "two seance instances fight over the socket — only one can own the control plane",
        ))
        .child(bullet(
            "after rebuilds: cargo build --release && restart so you aren't testing stale code",
        ))
        .child(bullet(
            "deep protocol: docs/CONTROL.md · build/pinning: docs/PLAYBOOK.md · theme: docs/THEME.md",
        ))
        .child(
            div()
                .pt_4()
                .text_xs()
                .text_color(SeancePalette::text_faint())
                .child("grimoire grows with the app — if a surface isn't here, that's a bug."),
        )
        .into_any_element()
}
