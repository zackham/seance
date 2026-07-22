//! Configurable quicklaunch strip: one-click "terminal in DIR running CMD"
//! buttons in the sidebar, above the host-bridge (claude accounts) strip.
//!
//! Config: `~/.config/seance/quicklaunch.json` — a JSON array:
//! ```json
//! [
//!   {"name": "vita", "cwd": "~/work/vita", "command": "claude"},
//!   {"name": "scratch", "cwd": "~"}
//! ]
//! ```
//! `command` omitted/empty = plain shell in `cwd`. Optional `"workspace"`
//! spawns into (and implicitly creates) that workspace instead of the
//! selected one. The file is mtime-watched with a 2s throttle — edits show
//! up without restarting the GUI; a parse error keeps the previous entries.

use std::time::{Duration, Instant};

use gpui::{div, prelude::*, Context, SharedString};
use serde::Deserialize;

use crate::pane::SpawnRequest;
use crate::theme::SeancePalette;

use super::util::tip_s;
use super::SeanceApp;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(super) struct QuickLaunchEntry {
    pub name: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

fn quicklaunch_path() -> std::path::PathBuf {
    std::path::PathBuf::from(shellexpand::tilde("~/.config/seance/quicklaunch.json").into_owned())
}

fn parse_quicklaunch(s: &str) -> Result<Vec<QuickLaunchEntry>, serde_json::Error> {
    serde_json::from_str(s)
}

impl SeanceApp {
    /// Cheap hot-reload: stat at most every 2s, re-parse only on mtime change.
    /// Called from render() — a bad edit keeps the last good entries.
    pub(super) fn reload_quicklaunch_if_stale(&mut self) {
        let now = Instant::now();
        if self
            .quicklaunch_checked
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(2))
        {
            return;
        }
        self.quicklaunch_checked = Some(now);
        let path = quicklaunch_path();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime == self.quicklaunch_mtime {
            return;
        }
        self.quicklaunch_mtime = mtime;
        if mtime.is_none() {
            self.quicklaunch.clear();
            return;
        }
        match std::fs::read_to_string(&path) {
            Ok(s) => match parse_quicklaunch(&s) {
                Ok(v) => self.quicklaunch = v,
                Err(e) => {
                    eprintln!("[seance gui] quicklaunch.json parse error: {e} (keeping previous)")
                }
            },
            Err(e) => eprintln!("[seance gui] quicklaunch.json read error: {e}"),
        }
    }

    /// Chip strip above the host-bridge widgets. Hidden when no entries.
    pub(super) fn render_quicklaunch(&self, cx: &Context<Self>) -> impl IntoElement {
        if self.quicklaunch.is_empty() {
            return div().flex_none().into_any_element();
        }
        div()
            .flex_none()
            .flex()
            .flex_col()
            .py_1p5()
            .gap_1()
            .border_t_1()
            .border_color(SeancePalette::border())
            .child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(SeancePalette::text_faint())
                    .child("── quicklaunch ──"),
            )
            .child(div().px_2().flex().flex_row().flex_wrap().gap_1().children(
                self.quicklaunch.iter().map(|e| {
                    let entry = e.clone();
                    let cmd_desc = entry
                        .command
                        .clone()
                        .filter(|c| !c.trim().is_empty())
                        .unwrap_or_else(|| "shell".into());
                    let cwd_desc = entry.cwd.clone().unwrap_or_else(|| "~".into());
                    div()
                        .id(SharedString::from(format!("ql-{}", e.name)))
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .text_xs()
                        .cursor_pointer()
                        .bg(SeancePalette::surface())
                        .text_color(SeancePalette::flame())
                        .hover(|s| s.bg(SeancePalette::border()))
                        .tooltip(tip_s(format!("{cwd_desc} $ {cmd_desc}")))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let cwd = entry
                                .cwd
                                .as_ref()
                                .map(|c| shellexpand::tilde(c).into_owned());
                            let command = entry.command.clone().filter(|c| !c.trim().is_empty());
                            this.spawn_internal(
                                SpawnRequest {
                                    name: entry.name.clone(),
                                    cwd,
                                    command,
                                    workspace: entry.workspace.clone(),
                                    file: None,
                                },
                                cx,
                            );
                        }))
                        .child(e.name.clone())
                        .into_any_element()
                }),
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_entry() {
        let v = parse_quicklaunch(
            r#"[{"name":"vita","cwd":"~/work/vita","command":"claude","workspace":"vita"}]"#,
        )
        .unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "vita");
        assert_eq!(v[0].cwd.as_deref(), Some("~/work/vita"));
        assert_eq!(v[0].command.as_deref(), Some("claude"));
        assert_eq!(v[0].workspace.as_deref(), Some("vita"));
    }

    #[test]
    fn parse_name_only_defaults_rest() {
        let v = parse_quicklaunch(r#"[{"name":"scratch"}]"#).unwrap();
        assert_eq!(v[0].name, "scratch");
        assert!(v[0].cwd.is_none() && v[0].command.is_none() && v[0].workspace.is_none());
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_quicklaunch(r#"{"name":"not-an-array"}"#).is_err());
        assert!(parse_quicklaunch("[{]").is_err());
    }

    #[test]
    fn parse_empty_array_ok() {
        assert!(parse_quicklaunch("[]").unwrap().is_empty());
    }
}
