//! Overlay command palette (ctrl+shift+k precanned prompts / ctrl+shift+j
//! fuzzy jump) plus the ctrl+shift+f "last failed command" flash. Selection
//! movement, activation, and the overlay render live here; the key-capture
//! that drives them stays in `mod.rs`.

use gpui::{div, prelude::*, px, Context, SharedString, Window};

use crate::theme::SeancePalette;

use super::{Drawer, PaletteMode, PaneStatus, SeanceApp};

/// Filter + order the jump list: every query token must appear in the slug or
/// the label, then most-recently-active first.
///
/// Name breaks ties so circles that have never been live (both clocks zero)
/// hold a stable order at the bottom instead of reshuffling every keystroke.
pub(super) fn rank_jump_items(
    rows: Vec<(String, String, u64)>,
    query: &str,
) -> Vec<(String, String)> {
    let q = query.trim().to_ascii_lowercase();
    let mut hits: Vec<(String, String, u64)> = rows
        .into_iter()
        .filter(|(slug, label, _)| {
            if q.is_empty() {
                return true;
            }
            let hay = format!("{slug} {label}").to_ascii_lowercase();
            q.split_whitespace().all(|t| hay.contains(t))
        })
        .collect();
    hits.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    hits.into_iter()
        .map(|(slug, label, _)| (slug, label))
        .collect()
}

impl SeanceApp {
    /// Prompt library, render-safe: builtins merged with the DAEMON-side user
    /// file via the remote cache (seeded at boot, refreshed every ~2s — never
    /// a blocking read on the UI thread).
    fn prompt_entries(&self) -> Vec<crate::prompts::PromptEntry> {
        let user = self.remote_cache.get(&crate::prompts::remote_config_path());
        crate::prompts::merge_with_user(user.as_deref())
    }

    /// The jump list: circles only, most-recently-active first.
    ///
    /// ONE source for the three things that must agree — the row count arrow
    /// keys wrap against, the item activated on Enter, and what is drawn. They
    /// used to be computed separately and had drifted apart: two of them still
    /// included panes after the list became circles-only, so the highlighted
    /// row and the thing Enter jumped to were different entries.
    ///
    /// Ordering is recency, not the rail's. The rail groups by band so it can
    /// stay still while agents come and go; jumping wants the opposite — the
    /// circle you were just in, then the one before that, so the hotkey plus
    /// arrows walks back through where you've been.
    pub(super) fn jump_items(&self, query: &str) -> Vec<(String, String)> {
        let rows: Vec<(String, String, u64)> = self
            .known_workspace_names()
            .into_iter()
            .map(|ws| {
                let label = self.workspace_label(&ws);
                let at = self.jump_recency(&ws);
                (ws, label, at)
            })
            .collect();
        rank_jump_items(rows, query)
    }

    /// When a circle was last live: real output, or human touch, whichever is
    /// later. Same pair the rail's idle band sorts on.
    fn jump_recency(&self, ws: &str) -> u64 {
        self.workspace_activity
            .get(ws)
            .copied()
            .max(self.workspace_touch.get(ws).copied())
            .unwrap_or(0)
    }

    pub(super) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = PaletteMode::Closed;
        // Return keys to the active terminal after overlay — eager, plus the
        // render-time backstop for targets whose tiles aren't mounted yet.
        if let Some(slug) = self.active_slug.clone() {
            if let Some(pane) = self.panes.iter().find(|p| p.slug == slug) {
                pane.focus_content(window, cx);
            }
            self.pending_focus = Some(slug);
        }
        cx.notify();
    }

    pub(super) fn palette_move(&mut self, delta: i32) {
        let n = match &self.palette {
            PaletteMode::Prompts { query, .. } => {
                crate::prompts::filter(&self.prompt_entries(), query).len()
            }
            PaletteMode::Jump { query, .. } => self.jump_items(query).len(),
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
                let hits = crate::prompts::filter(&self.prompt_entries(), query);
                if let Some(p) = hits.get(*selected) {
                    let body = p.body.clone();
                    self.inject_prompt_into_active(&body, cx);
                }
            }
            PaletteMode::Jump { query, selected } => {
                let items = self.jump_items(query);
                if let Some((ws, _)) = items.get(*selected).cloned() {
                    // `select_workspace` reveals the row in the rail, so a jump
                    // lands the same way ctrl+page cycling does.
                    self.select_workspace(&ws, window, cx);
                    self.close_palette(window, cx);
                }
            }
        }
    }

    pub(super) fn render_palette(&self, _cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let (title, query, selected, items): (String, String, usize, Vec<(String, String)>) =
            match &self.palette {
                PaletteMode::Closed => return None,
                PaletteMode::Prompts { query, selected } => {
                    let all = self.prompt_entries();
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
                // Circles only (owner decision 2026-08-02): pane entries made
                // the list mostly noise — jumping means switching circles.
                // Most-recently-active first, from the one shared source.
                PaletteMode::Jump { query, selected } => (
                    "jump · ctrl+shift+j · most recent first".into(),
                    query.clone(),
                    *selected,
                    self.jump_items(query),
                ),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<(String, String, u64)> {
        vec![
            ("mtg-growth".into(), "mtg-growth".into(), 300),
            ("seance".into(), "seance".into(), 900),
            ("wbr".into(), "wbr".into(), 700),
            ("never-opened".into(), "never-opened".into(), 0),
            ("also-never".into(), "also-never".into(), 0),
        ]
    }

    fn slugs(v: Vec<(String, String)>) -> Vec<String> {
        v.into_iter().map(|(s, _)| s).collect()
    }

    #[test]
    fn most_recently_active_comes_first() {
        // The point of the ordering: hotkey then arrows walks back through
        // where you have actually been, newest first.
        assert_eq!(
            slugs(rank_jump_items(rows(), "")),
            ["seance", "wbr", "mtg-growth", "also-never", "never-opened"]
        );
    }

    #[test]
    fn circles_that_were_never_live_hold_a_stable_order_at_the_bottom() {
        // Both clocks zero: name decides, so the tail doesn't reshuffle.
        let a = slugs(rank_jump_items(rows(), ""));
        let b = slugs(rank_jump_items(rows().into_iter().rev().collect(), ""));
        assert_eq!(a, b);
    }

    #[test]
    fn every_query_token_must_match_slug_or_label() {
        assert_eq!(slugs(rank_jump_items(rows(), "mtg")), ["mtg-growth"]);
        // Tokens are ANDed, and order between them doesn't matter.
        assert_eq!(slugs(rank_jump_items(rows(), "growth mtg")), ["mtg-growth"]);
        assert!(rank_jump_items(rows(), "mtg wbr").is_empty());
        assert!(rank_jump_items(rows(), "zzz").is_empty());
    }

    #[test]
    fn filtering_keeps_the_recency_order() {
        let v = vec![
            ("fix-a".into(), "fix-a".into(), 10),
            ("fix-b".into(), "fix-b".into(), 50),
            ("other".into(), "other".into(), 99),
        ];
        assert_eq!(slugs(rank_jump_items(v, "fix")), ["fix-b", "fix-a"]);
    }

    #[test]
    fn a_label_is_searchable_not_just_the_slug() {
        // Circle identity: slug is the id, label is the text. Either finds it.
        let v = vec![("term-2-3".into(), "seance".into(), 5)];
        assert_eq!(slugs(rank_jump_items(v.clone(), "seance")), ["term-2-3"]);
        assert_eq!(slugs(rank_jump_items(v, "term-2")), ["term-2-3"]);
    }
}
