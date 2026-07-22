//! Scratchpad / pad drawer and the one-button telegram "phone" spine for a
//! pane: reading the pad + task sidecar off disk, the phone-bind sidecar, and
//! rendering the pad inspector drawer.

use std::path::PathBuf;

use gpui::{div, prelude::*, Context, SharedString};

use crate::theme::SeancePalette;

use super::util::status_color;
use super::{Drawer, SeanceApp};

impl SeanceApp {
    pub(super) fn open_pad_drawer(&mut self, slug: &str, cx: &mut Context<Self>) {
        self.drawer = Drawer::Pad {
            slug: slug.to_string(),
        };
        cx.notify();
    }

    /// Read pad body + task sidecar from disk (daemon-owned paths).
    fn load_pad_bundle(slug: &str) -> (String, Option<String>, Option<serde_json::Value>) {
        let base = PathBuf::from(shellexpand::tilde("~/.local/share/seance/scratch").into_owned());
        let pad_path = base.join(format!("{slug}.md"));
        let pad = std::fs::read_to_string(&pad_path).unwrap_or_else(|_| String::new());
        let task_id = std::fs::read_to_string(base.join(format!("{slug}.taskid")))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let task_json = std::fs::read_to_string(base.join(format!("{slug}.task.json")))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        (pad, task_id, task_json)
    }

    fn phone_bind_path(slug: &str) -> PathBuf {
        PathBuf::from(
            shellexpand::tilde(&format!(
                "~/.local/share/seance/scratch/{slug}.telegram.json"
            ))
            .into_owned(),
        )
    }

    fn phone_bind_json(slug: &str) -> Option<serde_json::Value> {
        let p = Self::phone_bind_path(slug);
        let bytes = std::fs::read_to_string(&p).ok()?;
        serde_json::from_str(&bytes).ok()
    }

    pub(super) fn phone_linked(slug: &str) -> Option<String> {
        Self::phone_bind_json(slug).and_then(|v| {
            v.get("topic_id")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
    }

    fn phone_link(slug: &str) -> Option<String> {
        Self::phone_bind_json(slug).and_then(|v| {
            v.get("link")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
    }

    /// One-button telegram topic for a pane (shells `seance ctl phone`).
    pub(super) fn phone_pane(&mut self, slug: &str, cx: &mut Context<Self>) {
        let slug = slug.to_string();
        // If already linked, open telegram if we have a link + pad drawer.
        if let Some(tid) = Self::phone_linked(&slug) {
            if let Some(link) = Self::phone_link(&slug) {
                let _ = std::process::Command::new("xdg-open")
                    .arg(&link)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
            crate::desktop_notify::notify(
                "seance · already phoned",
                &format!("{slug} → topic {tid}"),
            );
            self.open_pad_drawer(&slug, cx);
            return;
        }
        // Off UI thread — vita open_topic can take seconds.
        let slug_bg = slug.clone();
        cx.spawn(async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move {
                    std::process::Command::new("seance")
                        .args(["ctl", "phone", &slug_bg])
                        .output()
                })
                .await;
            let Some(this) = this.upgrade() else { return };
            this.update(cx, |app, cx| {
                match out {
                    Ok(o) if o.status.success() => {
                        let topic = Self::phone_linked(&slug).unwrap_or_else(|| {
                            String::from_utf8_lossy(&o.stdout).trim().to_string()
                        });
                        if let Some(link) = Self::phone_link(&slug) {
                            let _ = std::process::Command::new("xdg-open")
                                .arg(&link)
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn();
                        }
                        crate::desktop_notify::notify(
                            "seance · phone linked",
                            &format!("{slug} → {topic}"),
                        );
                        app.open_pad_drawer(&slug, cx);
                    }
                    Ok(o) => {
                        let err = String::from_utf8_lossy(&o.stderr);
                        crate::desktop_notify::notify(
                            "seance · phone failed",
                            if err.trim().is_empty() {
                                "ctl phone failed (is vita up?)"
                            } else {
                                err.trim()
                            },
                        );
                    }
                    Err(e) => {
                        crate::desktop_notify::notify("seance · phone failed", &format!("{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_pad_drawer(&self, slug: &str, cx: &Context<Self>) -> impl IntoElement {
        // Include tick so GPUI re-renders when pad_refresh_tick advances.
        let _tick = self.pad_refresh_tick;
        let _ = cx;
        let name = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| slug.to_string());
        let st = self.statuses.get(slug);
        let (pad, task_id, task_json) = Self::load_pad_bundle(slug);
        let phone = Self::phone_linked(slug);
        let status_line = match st {
            Some(s) => match &s.note {
                Some(n) if !n.is_empty() => format!("{} · {n}", s.state),
                _ => s.state.clone(),
            },
            None => "—".into(),
        };
        let task_body = task_json
            .as_ref()
            .and_then(|v| v.get("body").and_then(|b| b.as_str()))
            .unwrap_or("")
            .to_string();
        let task_status = task_json
            .as_ref()
            .and_then(|v| v.get("status").and_then(|b| b.as_str()))
            .unwrap_or("-");
        let task_id_s = task_id.unwrap_or_else(|| "—".into());

        let pad_display = if pad.trim().is_empty() {
            "(empty pad)".to_string()
        } else {
            // Show tail so latest finish is visible without scroll thrash.
            let lines: Vec<&str> = pad.lines().collect();
            if lines.len() > 80 {
                let mut s = String::from("…\n");
                s.push_str(&lines[lines.len() - 80..].join("\n"));
                s
            } else {
                pad
            }
        };
        let task_display = if task_body.trim().is_empty() {
            "(no active/recent inject body)".to_string()
        } else {
            if task_body.len() > 2500 {
                let mut end = 2500;
                while end > 0 && !task_body.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &task_body[..end])
            } else {
                task_body
            }
        };

        let slug_phone = slug.to_string();
        let slug_flip = slug.to_string();

        div()
            .id(SharedString::from(format!("pad-drawer-{slug}")))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(SeancePalette::flame())
                            .child(format!("{name}")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .child(format!("`{slug}`")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(status_color(st.map(|s| s.state.as_str()).unwrap_or("-")))
                            .child(status_line),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(SeancePalette::text_faint())
                    .child(match &phone {
                        Some(t) => format!("☎ topic {t}"),
                        None => "☎ not phoned".into(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("pad-phone-{slug}")))
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_xs()
                            .text_color(SeancePalette::violet())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.phone_pane(&slug_phone, cx);
                            }))
                            .child(if phone.is_some() {
                                "☎ re-show"
                            } else {
                                "☎ phone"
                            }),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("pad-flip-{slug}")))
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_xs()
                            .text_color(SeancePalette::flame())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.flip_notes_for(&slug_flip, window, cx);
                            }))
                            .child("✎ edit notes"),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(SeancePalette::violet())
                    .child(format!("task {task_id_s} · {task_status}")),
            )
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(SeancePalette::bg())
                    .border_1()
                    .border_color(SeancePalette::border())
                    .text_xs()
                    .text_color(SeancePalette::text_dim())
                    .child(task_display),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(SeancePalette::violet())
                    .child("pad (tail)"),
            )
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(SeancePalette::bg())
                    .border_1()
                    .border_color(SeancePalette::border())
                    .text_xs()
                    .text_color(SeancePalette::text())
                    .child(pad_display),
            )
            .into_any_element()
    }
}
