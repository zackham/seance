//! Launch picker: choose local daemon vs remote host at startup.
//!
//! Shown only when launch can't be resolved silently — no saved preference
//! and no local daemon running, or a saved remote preference whose tunnel
//! failed. Keyboard-driven: ↑/↓/tab to switch option, type to edit the host
//! (remote row), enter to confirm, esc quits. The choice is persisted via
//! [`crate::launch`] and prefilled on the next run.

use gpui::{div, prelude::*, px, App, Context, FocusHandle, Focusable, SharedString, Window};

use crate::launch::{self, LaunchMode, LaunchPref};
use crate::theme::SeancePalette;

/// Outcome of a successful pick, handed back to main to boot the real app.
pub enum PickOutcome {
    Local,
    Remote(crate::tunnel::Tunnel),
}

enum PickerState {
    Choosing,
    Connecting(String),
    Failed(String),
}

pub struct LaunchPicker {
    focus_handle: FocusHandle,
    /// 0 = remote, 1 = local.
    selected: usize,
    host: String,
    state: PickerState,
    /// Called on the main thread once a choice succeeds.
    on_done: Option<Box<dyn FnOnce(PickOutcome, &mut Window, &mut App) + 'static>>,
}

impl LaunchPicker {
    pub fn new(
        prefill: Option<LaunchPref>,
        error: Option<String>,
        cx: &mut Context<Self>,
        on_done: Box<dyn FnOnce(PickOutcome, &mut Window, &mut App) + 'static>,
    ) -> Self {
        let (selected, host) = match &prefill {
            Some(p) => (
                if p.mode == LaunchMode::Remote { 0 } else { 1 },
                p.host.clone().unwrap_or_default(),
            ),
            None => (0, String::new()),
        };
        Self {
            focus_handle: cx.focus_handle(),
            selected,
            host,
            state: match error {
                Some(e) => PickerState::Failed(e),
                None => PickerState::Choosing,
            },
            on_done: Some(on_done),
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.state, PickerState::Connecting(_)) {
            return;
        }
        if self.selected == 0 {
            let host = self.host.trim().to_string();
            if host.is_empty() {
                self.state = PickerState::Failed("enter a host to connect to".into());
                cx.notify();
                return;
            }
            launch::save(&LaunchPref {
                mode: LaunchMode::Remote,
                host: Some(host.clone()),
            });
            self.state = PickerState::Connecting(host.clone());
            cx.notify();
            let task = cx
                .background_executor()
                .spawn(async move { crate::tunnel::Tunnel::establish(&host) });
            cx.spawn_in(window, async move |this: gpui::WeakEntity<Self>, cx| {
                let result = task.await;
                let _ = cx.update(|window, cx| {
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |this, cx| match result {
                            Ok(tunnel) => this.finish(PickOutcome::Remote(tunnel), window, cx),
                            Err(e) => {
                                this.state = PickerState::Failed(format!("{e:#}"));
                                cx.notify();
                            }
                        });
                    }
                });
            })
            .detach();
        } else {
            launch::save(&LaunchPref {
                mode: LaunchMode::Local,
                host: if self.host.trim().is_empty() {
                    None
                } else {
                    Some(self.host.trim().to_string())
                },
            });
            self.state = PickerState::Connecting("local daemon".into());
            cx.notify();
            let task = cx
                .background_executor()
                .spawn(async move { crate::daemon::ensure_daemon() });
            cx.spawn_in(window, async move |this: gpui::WeakEntity<Self>, cx| {
                let result = task.await;
                let _ = cx.update(|window, cx| {
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |this, cx| match result {
                            Ok(_) => this.finish(PickOutcome::Local, window, cx),
                            Err(e) => {
                                this.state = PickerState::Failed(format!("local daemon: {e:#}"));
                                cx.notify();
                            }
                        });
                    }
                });
            })
            .detach();
        }
    }

    fn finish(&mut self, outcome: PickOutcome, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(done) = self.on_done.take() {
            done(outcome, window, cx);
        }
    }

    fn on_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let key = ks.key.as_str();
        if matches!(self.state, PickerState::Connecting(_)) {
            return;
        }
        match key {
            "up" | "down" | "tab" => {
                self.selected = 1 - self.selected;
                self.state = PickerState::Choosing;
            }
            "enter" => {
                self.confirm(window, cx);
            }
            "escape" => {
                cx.quit();
                return;
            }
            "backspace" => {
                if self.selected == 0 {
                    self.host.pop();
                    if matches!(self.state, PickerState::Failed(_)) {
                        self.state = PickerState::Choosing;
                    }
                }
            }
            _ => {
                if self.selected == 0 {
                    if let Some(ch) = ks.key_char.as_deref() {
                        if !ch.chars().any(char::is_control) {
                            self.host.push_str(ch);
                            if matches!(self.state, PickerState::Failed(_)) {
                                self.state = PickerState::Choosing;
                            }
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    fn row(&self, idx: usize, label: SharedString, detail: SharedString) -> impl IntoElement {
        let active = self.selected == idx;
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_md()
            .when(active, |d| d.bg(SeancePalette::surface()))
            .child(
                div()
                    .text_color(if active {
                        SeancePalette::flame()
                    } else {
                        SeancePalette::text_faint()
                    })
                    .child(if active { "●" } else { "○" }),
            )
            .child(
                div()
                    .text_color(if active {
                        SeancePalette::text()
                    } else {
                        SeancePalette::text_dim()
                    })
                    .child(label),
            )
            .child(
                div()
                    .text_color(SeancePalette::text_dim())
                    .text_sm()
                    .child(detail),
            )
    }
}

impl Focusable for LaunchPicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LaunchPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.focus(&self.focus_handle, cx);

        let host_line: SharedString = if self.host.is_empty() {
            "type a host…".into()
        } else {
            format!("{}▏", self.host).into()
        };
        let status: Option<(SharedString, gpui::Hsla)> = match &self.state {
            PickerState::Choosing => None,
            PickerState::Connecting(what) => Some((
                format!("connecting to {what}…").into(),
                SeancePalette::text_dim(),
            )),
            PickerState::Failed(e) => Some((e.clone().into(), SeancePalette::danger())),
        };

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(SeancePalette::bg())
            .track_focus(&self.focus_handle)
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w(px(560.))
                    .p_6()
                    .rounded_lg()
                    .bg(SeancePalette::bg_elevated())
                    .border_1()
                    .border_color(SeancePalette::border())
                    .child(
                        div()
                            .text_lg()
                            .text_color(SeancePalette::text())
                            .child("seance — where is your daemon?"),
                    )
                    .child(self.row(0, "connect to remote host".into(), host_line))
                    .child(self.row(1, "run locally".into(), "daemon on this machine".into()))
                    .children(
                        status.map(|(msg, color)| div().text_sm().text_color(color).child(msg)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(SeancePalette::text_faint())
                            .child("↑/↓ switch · type host · enter connect · esc quit"),
                    ),
            )
    }
}
