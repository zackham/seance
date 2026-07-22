//! Overlay command palette (ctrl+shift+k precanned prompts / ctrl+shift+j
//! fuzzy jump) plus the ctrl+shift+f "last failed command" flash. Selection
//! movement, activation, and the overlay render live here; the key-capture
//! that drives them stays in `mod.rs`.

use gpui::{div, prelude::*, px, Context, SharedString, Window};

use crate::theme::SeancePalette;

use super::{Drawer, PaletteMode, PaneStatus, SeanceApp};

impl SeanceApp {
    pub(super) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = PaletteMode::Closed;
        // Return keys to the active terminal after overlay.
        if let Some(slug) = self.active_slug.clone() {
            if let Some(pane) = self.panes.iter().find(|p| p.slug == slug) {
                pane.focus_content(window, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn palette_move(&mut self, delta: i32) {
        let n = match &self.palette {
            PaletteMode::Prompts { query, .. } => {
                crate::prompts::filter(&crate::prompts::load_all(), query).len()
            }
            PaletteMode::Jump { query, .. } => {
                let q = query.trim().to_ascii_lowercase();
                let pane_n = self
                    .panes
                    .iter()
                    .filter(|p| {
                        if q.is_empty() {
                            return true;
                        }
                        let hay = format!("{} {} {} {}", p.name, p.slug, p.command, p.workspace)
                            .to_ascii_lowercase();
                        q.split_whitespace().all(|t| hay.contains(t))
                    })
                    .count();
                pane_n + self.workspaces().len()
            }
            PaletteMode::Closed => 0,
        };
        match &mut self.palette {
            PaletteMode::Prompts { selected, .. } | PaletteMode::Jump { selected, .. } => {
                if n == 0 {
                    *selected = 0;
                    return;
                }
                let cur = *selected as i32;
                *selected = ((cur + delta).rem_euclid(n as i32)) as usize;
            }
            PaletteMode::Closed => {}
        }
    }

    /// Query daemon command log for last failed command; flash as a status note
    /// and open activity drawer so the human can see context.
    pub(super) fn show_last_failed(&mut self, slug: &str, cx: &mut Context<Self>) {
        let slug = slug.to_string();
        let out = std::process::Command::new("seance")
            .args(["ctl", "last-command", &slug, "--failed", "--json"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout);
                let cmd = serde_json::from_str::<serde_json::Value>(&s)
                    .ok()
                    .and_then(|v| {
                        v.pointer("/data/command")
                            .or_else(|| v.get("command"))
                            .and_then(|c| c.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| s.trim().to_string());
                let exit = serde_json::from_str::<serde_json::Value>(&s)
                    .ok()
                    .and_then(|v| {
                        v.pointer("/data/exit")
                            .or_else(|| v.get("exit"))
                            .and_then(|e| e.as_i64())
                    });
                let note = match exit {
                    Some(e) => format!("last failed (exit {e}): {cmd}"),
                    None => format!("last failed: {cmd}"),
                };
                self.statuses.insert(
                    slug.clone(),
                    PaneStatus {
                        state: "needs-human".into(),
                        note: Some(note.clone()),
                    },
                );
                crate::desktop_notify::notify("seance · last failed", &note);
                self.drawer = Drawer::Activity;
                cx.notify();
            }
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                crate::desktop_notify::notify(
                    "seance · last failed",
                    if err.trim().is_empty() {
                        "no failed command on this pane"
                    } else {
                        err.trim()
                    },
                );
            }
            Err(e) => {
                crate::desktop_notify::notify("seance · last failed", &format!("ctl error: {e}"));
            }
        }
    }

    pub(super) fn activate_palette_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.palette {
            PaletteMode::Closed => {}
            PaletteMode::Prompts { query, selected } => {
                let hits = crate::prompts::filter(&crate::prompts::load_all(), query);
                if let Some(p) = hits.get(*selected) {
                    let body = p.body.clone();
                    self.inject_prompt_into_active(&body, cx);
                }
            }
            PaletteMode::Jump { query, selected } => {
                let q = query.trim().to_ascii_lowercase();
                let mut items: Vec<String> = self
                    .panes
                    .iter()
                    .filter(|p| {
                        if q.is_empty() {
                            return true;
                        }
                        let hay = format!("{} {} {} {}", p.name, p.slug, p.command, p.workspace)
                            .to_ascii_lowercase();
                        q.split_whitespace().all(|t| hay.contains(t))
                    })
                    .map(|p| p.slug.clone())
                    .collect();
                for ws in self.workspaces() {
                    if q.is_empty() || ws.to_ascii_lowercase().contains(&q) {
                        items.push(format!("ws:{ws}"));
                    }
                }
                if let Some(id) = items.get(*selected).cloned() {
                    if let Some(ws) = id.strip_prefix("ws:") {
                        self.select_workspace(ws, window, cx);
                        self.close_palette(window, cx);
                    } else {
                        self.focus_pane_slug(&id, window, cx);
                        self.palette = PaletteMode::Closed;
                        cx.notify();
                    }
                }
            }
        }
    }

    pub(super) fn render_palette(&self, _cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let (title, query, selected, items): (String, String, usize, Vec<(String, String)>) =
            match &self.palette {
                PaletteMode::Closed => return None,
                PaletteMode::Prompts { query, selected } => {
                    let all = crate::prompts::load_all();
                    let hits = crate::prompts::filter(&all, query);
                    let items: Vec<_> = hits
                        .into_iter()
                        .map(|p| {
                            (
                                p.id,
                                format!(
                                    "{} — {}",
                                    p.title,
                                    p.body.chars().take(60).collect::<String>()
                                ),
                            )
                        })
                        .collect();
                    (
                        "precanned prompts · ctrl+shift+k".into(),
                        query.clone(),
                        *selected,
                        items,
                    )
                }
                PaletteMode::Jump { query, selected } => {
                    let q = query.trim().to_ascii_lowercase();
                    let mut items: Vec<(String, String)> = self
                        .panes
                        .iter()
                        .filter(|p| {
                            if q.is_empty() {
                                return true;
                            }
                            let hay =
                                format!("{} {} {} {}", p.name, p.slug, p.command, p.workspace)
                                    .to_ascii_lowercase();
                            q.split_whitespace().all(|t| hay.contains(t))
                        })
                        .map(|p| {
                            let st = self
                                .statuses
                                .get(&p.slug)
                                .map(|s| s.state.as_str())
                                .unwrap_or("-");
                            (
                                p.slug.clone(),
                                format!("{} · {st} · {}", p.name, p.workspace),
                            )
                        })
                        .collect();
                    // Also offer workspaces as jump targets with ws: prefix
                    for ws in self.workspaces() {
                        if q.is_empty() || ws.to_ascii_lowercase().contains(&q) {
                            items.push((format!("ws:{ws}"), format!("workspace · {ws}")));
                        }
                    }
                    (
                        "jump · ctrl+shift+j".into(),
                        query.clone(),
                        *selected,
                        items,
                    )
                }
            };
        let n = items.len();
        let sel = if n == 0 { 0 } else { selected.min(n - 1) };
        Some(
            div()
                .id("palette-overlay")
                .absolute()
                .top(px(48.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .w(px(520.))
                        .max_h(px(360.))
                        .rounded_lg()
                        .border_1()
                        .border_color(SeancePalette::flame_dim())
                        .bg(SeancePalette::bg_elevated())
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(SeancePalette::border())
                                .text_xs()
                                .text_color(SeancePalette::text_faint())
                                .child(title),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(SeancePalette::border())
                                .text_sm()
                                .text_color(SeancePalette::flame())
                                .child(format!("› {query}█")),
                        )
                        .child(
                            div()
                                .id("palette-list")
                                .flex_1()
                                .overflow_y_scroll()
                                .py_1()
                                .children(items.into_iter().enumerate().map(|(i, (id, label))| {
                                    let active = i == sel;
                                    div()
                                        .id(SharedString::from(format!("pal-{i}-{id}")))
                                        .px_3()
                                        .py_1()
                                        .text_sm()
                                        .bg(if active {
                                            SeancePalette::surface()
                                        } else {
                                            gpui::transparent_black()
                                        })
                                        .text_color(if active {
                                            SeancePalette::flame()
                                        } else {
                                            SeancePalette::text()
                                        })
                                        .child(label)
                                        .into_any_element()
                                })),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(SeancePalette::text_faint())
                                .child("enter select · esc close · type to filter"),
                        ),
                )
                .into_any_element(),
        )
    }
}
