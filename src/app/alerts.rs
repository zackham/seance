//! Pane alerts (BEL, OSC 9 / OSC 777, long shell commands) turned into what a
//! terminal like Ghostty gives you: the bell sound, and a desktop notification
//! when you aren't looking at that pane. Sound and the command rule come from
//! `~/.config/seance/terminal.conf` (see `term_config`).

use super::SeanceApp;
use crate::desktop_notify;
use crate::term_config::CommandFinishNotify;

impl SeanceApp {
    pub(super) fn on_pane_alert(
        &self,
        pane: &str,
        kind: &str,
        title: &str,
        body: &str,
        duration_ms: u64,
        exit_code: Option<i32>,
    ) {
        let cfg = crate::term_config::get();
        // Looking right at the pane: like a focused terminal, sound only.
        let watching = self.window_active && self.active_slug.as_deref() == Some(pane);
        let name = self
            .panes
            .iter()
            .find(|p| p.slug == pane)
            .map(|p| format!("{} · {}", self.workspace_label(&p.workspace), p.name))
            .unwrap_or_else(|| pane.to_string());
        match kind {
            // Sound only, as in Ghostty: shells ring on every failed tab
            // completion, and a program that wants a banner sends OSC 9/777.
            "bell" => desktop_notify::bell_sound(),
            "notify" if !watching => {
                let summary = if title.is_empty() {
                    name
                } else {
                    format!("{name}: {title}")
                };
                desktop_notify::notify_alert(pane, &summary, body);
            }
            "command" => {
                let wanted = match cfg.notify_on_command_finish {
                    CommandFinishNotify::Never => false,
                    CommandFinishNotify::Unfocused => !watching,
                    CommandFinishNotify::Always => true,
                };
                if wanted && duration_ms >= cfg.notify_on_command_finish_after_ms {
                    let status = match exit_code {
                        Some(0) => "Done".to_string(),
                        Some(code) => format!("Failed ({code})"),
                        None => "Finished".to_string(),
                    };
                    desktop_notify::bell_sound();
                    desktop_notify::notify_alert(pane, &format!("{name}: {status}"), body);
                }
            }
            _ => {}
        }
    }
}
