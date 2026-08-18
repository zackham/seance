//! Chrome keymap — the native chord table (src/app/mod.rs
//! `on_global_key_capture`) with web-required fallbacks.
//!
//! Browsers reserve ctrl+shift+n (incognito) and ctrl+shift+w (close window)
//! at the UI level — a page never sees them — and often swallow ctrl+shift+
//! j/k/r for devtools/reload. So every native chord ALSO binds its `alt+`
//! twin, and alt is the reliable spelling on web. The help overlay documents
//! both. Native chords that have no web surface yet (overview, palettes,
//! notes flip, popout, last-failed) are listed in help as native-only.

use web_sys::KeyboardEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeCommand {
    CycleWorkspace(i32),
    CyclePane(i32),
    /// New shell pane in this workspace (summon).
    Summon,
    /// Kill active pane; last pane (or empty circle) banishes the workspace.
    KillActive,
    ToggleZoom,
    RenameWorkspace,
    ToggleHelp,
    ToggleActivity,
    ToggleProbe,
    /// Jump to the rail's top row — first pinned circle if any, else the top
    /// of active, and so on down the bands.
    SelectTopWorkspace,
    /// Escape: close topmost overlay (menu > help > activity > zoom > selection).
    Escape,
}

/// Map a keydown to a chrome command. Pure; PTY routing happens only when
/// this returns `None` (or `execute` declines).
pub fn command_for(ev: &KeyboardEvent) -> Option<ChromeCommand> {
    let key = ev.key();
    let ctrl = ev.ctrl_key();
    let shift = ev.shift_key();
    let alt = ev.alt_key();
    let meta = ev.meta_key();
    if meta {
        return None; // cmd combos belong to the browser
    }

    if key == "Escape" && !ctrl && !alt {
        return Some(ChromeCommand::Escape);
    }

    // ctrl+page = cycle workspace; +shift = cycle pane (native, web-safe).
    if ctrl && !alt && (key == "PageUp" || key == "PageDown") {
        let d = if key == "PageUp" { -1 } else { 1 };
        return Some(if shift {
            ChromeCommand::CyclePane(d)
        } else {
            ChromeCommand::CycleWorkspace(d)
        });
    }

    // Native ctrl+shift chords + their alt twins.
    let chord = (ctrl && shift && !alt) || (alt && !ctrl && !shift);
    if !chord {
        return None;
    }
    let k = key.to_ascii_lowercase();
    match k.as_str() {
        "n" => Some(ChromeCommand::Summon),
        "w" => Some(ChromeCommand::KillActive),
        "z" | "m" => Some(ChromeCommand::ToggleZoom),
        "r" => Some(ChromeCommand::RenameWorkspace),
        "p" => Some(ChromeCommand::ToggleProbe),
        "?" | "/" => Some(ChromeCommand::ToggleHelp),
        "a" => Some(ChromeCommand::ToggleActivity),
        "home" => Some(ChromeCommand::SelectTopWorkspace),
        _ => None,
    }
}

/// Run a command. Returns false when the command didn't apply (key falls
/// through to the PTY — e.g. Escape with nothing open).
pub fn execute(
    cmd: ChromeCommand,
    actions: &dyn crate::app_api::Actions,
    app: &std::rc::Rc<crate::App>,
) -> bool {
    match cmd {
        ChromeCommand::CycleWorkspace(d) => {
            actions.cycle_workspace(d);
            true
        }
        ChromeCommand::CyclePane(d) => {
            actions.cycle_pane(d);
            true
        }
        ChromeCommand::Summon => {
            actions.summon();
            true
        }
        ChromeCommand::KillActive => {
            actions.kill_active();
            true
        }
        ChromeCommand::ToggleZoom => {
            if let Some(slug) = app.focused_pane_pub() {
                actions.toggle_zoom(&slug);
                true
            } else {
                false
            }
        }
        ChromeCommand::RenameWorkspace => app.begin_selected_workspace_rename(),
        ChromeCommand::ToggleHelp => {
            actions.toggle_help();
            true
        }
        ChromeCommand::ToggleActivity => {
            actions.toggle_activity();
            true
        }
        ChromeCommand::ToggleProbe => {
            actions.toggle_probe();
            true
        }
        ChromeCommand::SelectTopWorkspace => app.select_top_workspace(),
        ChromeCommand::Escape => app.escape_topmost(),
    }
}
