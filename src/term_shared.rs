//! Shared terminal primitives used by the remote-terminal path.
//!
//! These items were extracted from the retired local-PTY `terminal` module
//! (deleted with `terminal.rs`/`terminal_view.rs`) because they are still
//! consumed by the live daemon-backed path: `RemoteTerminal` emits
//! [`TerminalEvent`] and holds a [`Ghost`], and `RemoteTerminalView` maps key
//! events to PTY bytes via [`keystroke_bytes`]. Bodies are verbatim moves.

use alacritty_terminal::term::TermMode;

/// "Typing hot" window: stamped on every human key/paste into a terminal;
/// the GUI event loop paints applied grids immediately while hot (echo
/// latency) and throttles to ~30fps otherwise (stream smoothness).
static TYPING_HOT_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Stamp typing activity (hot for ~250ms — covers echo round-trip + repeat).
pub fn touch_typing_hot() {
    TYPING_HOT_UNTIL.store(now_ms() + 250, std::sync::atomic::Ordering::Relaxed);
}

/// Whether a human keystroke is plausibly awaiting its echo frame.
pub fn typing_hot() -> bool {
    now_ms() < TYPING_HOT_UNTIL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Events a terminal entity emits to the session manager / views.
///
/// The live remote-terminal path only ever emits `Wakeup` (repaint on new
/// snapshot); title/bell/exit/ghost transitions travel as daemon state, not as
/// entity events. The enum stays for the `EventEmitter` seam.
#[derive(Clone, Debug)]
pub enum TerminalEvent {
    Wakeup,
}

/// An agent-proposed command rendered as ghost text at the prompt.
#[derive(Clone, Debug)]
pub struct Ghost {
    pub id: String,
    pub text: String,
    pub from: String,
    pub reason: Option<String>,
}

/// Key event -> PTY bytes. The mapping lives in `seance_core::input` (shared
/// with the web client); this adapts gpui's keystroke into the neutral form.
pub fn keystroke_bytes(keystroke: &gpui::Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let input = seance_core::input::KeyInput {
        key: keystroke.key.clone(),
        key_char: keystroke.key_char.clone(),
        mods: seance_core::input::Modifiers {
            shift: keystroke.modifiers.shift,
            control: keystroke.modifiers.control,
            alt: keystroke.modifiers.alt,
            platform: keystroke.modifiers.platform,
        },
    };
    let modes = seance_core::input::TermModes {
        app_cursor: mode.contains(TermMode::APP_CURSOR),
    };
    seance_core::input::key_to_bytes(&input, modes)
}
