//! Shared terminal primitives used by the remote-terminal path.
//!
//! These items were extracted from the retired local-PTY `terminal` module
//! (deleted with `terminal.rs`/`terminal_view.rs`) because they are still
//! consumed by the live daemon-backed path: `RemoteTerminal` emits
//! [`TerminalEvent`] and holds a [`Ghost`], and `RemoteTerminalView` maps key
//! events to PTY bytes via [`keystroke_bytes`]. Bodies are verbatim moves.

use alacritty_terminal::term::TermMode;

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

/// Key event -> PTY bytes. Compact xterm mapping covering what agent TUIs use.
pub fn keystroke_bytes(keystroke: &gpui::Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let mods = keystroke.modifiers;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    // Named/control keys first.
    let named: Option<&[u8]> = match keystroke.key.as_str() {
        "enter" => {
            if mods.shift {
                // Newline-without-submit for agent TUIs (ink treats \n as meta-enter).
                Some(b"\n".as_slice())
            } else {
                Some(b"\r".as_slice())
            }
        }
        "backspace" => Some(if mods.control { b"\x08" } else { b"\x7f" }),
        "tab" => Some(if mods.shift { b"\x1b[Z" } else { b"\t" }),
        "escape" => Some(b"\x1b"),
        "up" => Some(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }),
        "down" => Some(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }),
        "right" => Some(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }),
        "left" => Some(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }),
        "home" => Some(if app_cursor { b"\x1bOH" } else { b"\x1b[H" }),
        "end" => Some(if app_cursor { b"\x1bOF" } else { b"\x1b[F" }),
        // Bare / shift page keys only — ctrl+page* is seance workspace cycle.
        "pageup" if !mods.control => Some(b"\x1b[5~"),
        "pagedown" if !mods.control => Some(b"\x1b[6~"),
        "delete" => Some(b"\x1b[3~"),
        "insert" => Some(b"\x1b[2~"),
        "f1" => Some(b"\x1bOP"),
        "f2" => Some(b"\x1bOQ"),
        "f3" => Some(b"\x1bOR"),
        "f4" => Some(b"\x1bOS"),
        "f5" => Some(b"\x1b[15~"),
        "f6" => Some(b"\x1b[17~"),
        "f7" => Some(b"\x1b[18~"),
        "f8" => Some(b"\x1b[19~"),
        "f9" => Some(b"\x1b[20~"),
        "f10" => Some(b"\x1b[21~"),
        "f11" => Some(b"\x1b[23~"),
        "f12" => Some(b"\x1b[24~"),
        _ => None,
    };
    if let Some(bytes) = named {
        return Some(bytes.to_vec());
    }

    // Ctrl+letter -> C0 control codes.
    if mods.control {
        let key = keystroke.key.as_str();
        if key.len() == 1 {
            let ch = key.chars().next().unwrap().to_ascii_lowercase();
            let byte = match ch {
                'a'..='z' => Some(ch as u8 - b'a' + 1),
                '@' | ' ' => Some(0),
                '[' => Some(27),
                '\\' => Some(28),
                ']' => Some(29),
                '^' => Some(30),
                '_' | '/' => Some(31),
                _ => None,
            };
            if let Some(b) = byte {
                return Some(vec![b]);
            }
        }
    }

    // Plain characters (IME-composed or direct); alt prefixes ESC.
    if let Some(key_char) = &keystroke.key_char {
        let mut bytes = Vec::with_capacity(key_char.len() + 1);
        if mods.alt {
            bytes.push(0x1b);
        }
        bytes.extend_from_slice(key_char.as_bytes());
        return Some(bytes);
    }

    None
}
