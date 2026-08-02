//! Keyboard / mouse / paste → PTY bytes, platform-neutral.
//!
//! The native app adapts `gpui::Keystroke` into [`KeyInput`]; the web client
//! adapts DOM `KeyboardEvent`. The mapping itself (xterm-compatible, covering
//! what agent TUIs use) lives here once, so every client emits identical bytes.

use serde::{Deserialize, Serialize};

/// Modifier state at the moment of the key event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    /// cmd on macOS / win key — never sent to the PTY, but clients use it to
    /// route chrome shortcuts before calling [`key_to_bytes`].
    pub platform: bool,
}

/// A key event, normalized to gpui-style lowercase key names: named keys as
/// `"enter"`, `"backspace"`, `"up"`, `"f5"`, …; character keys as the plain
/// character (`"a"`, `"["`). `key_char` carries the produced text for
/// character keys (IME-composed or direct), `None` for named keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyInput {
    pub key: String,
    pub key_char: Option<String>,
    pub mods: Modifiers,
}

/// Terminal input modes that affect encoding, mirrored from
/// [`crate::snapshot::GridSnapshot`] flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermModes {
    /// DECCKM: application cursor keys (`ESC O A` instead of `ESC [ A`).
    pub app_cursor: bool,
}

impl TermModes {
    pub fn from_snapshot(snap: &crate::snapshot::GridSnapshot) -> Self {
        Self {
            app_cursor: snap.app_cursor,
        }
    }
}

/// Key event -> PTY bytes. Compact xterm mapping covering what agent TUIs use.
///
/// Returns `None` when the event is not a PTY-bound key (chrome shortcut,
/// bare modifier, unmapped named key).
pub fn key_to_bytes(input: &KeyInput, modes: TermModes) -> Option<Vec<u8>> {
    let mods = input.mods;
    let app_cursor = modes.app_cursor;

    // Named/control keys first.
    let named: Option<&[u8]> = match input.key.as_str() {
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
        let key = input.key.as_str();
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
    if let Some(key_char) = &input.key_char {
        let mut bytes = Vec::with_capacity(key_char.len() + 1);
        if mods.alt {
            bytes.push(0x1b);
        }
        bytes.extend_from_slice(key_char.as_bytes());
        return Some(bytes);
    }

    None
}

/// Wrap pasted text in bracketed-paste markers (the daemon-side inject path
/// does its own wrapping — this is for clients writing straight to `Input`).
pub fn bracketed_paste(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Mouse button for SGR encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Wheel: positive rows scroll up.
    WheelUp,
    WheelDown,
}

/// SGR (1006) mouse report. `pressed=false` = release (ignored for wheel).
/// `col`/`row` are 0-based cell coordinates; the wire is 1-based.
pub fn sgr_mouse(
    button: MouseButton,
    col: u16,
    row: u16,
    pressed: bool,
    mods: Modifiers,
) -> Vec<u8> {
    let mut cb: u16 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    if mods.shift {
        cb += 4;
    }
    if mods.alt {
        cb += 8;
    }
    if mods.control {
        cb += 16;
    }
    let suffix = if pressed || matches!(button, MouseButton::WheelUp | MouseButton::WheelDown) {
        'M'
    } else {
        'm'
    };
    format!("\x1b[<{};{};{}{}", cb, col + 1, row + 1, suffix).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> KeyInput {
        KeyInput {
            key: name.into(),
            key_char: None,
            mods: Modifiers::default(),
        }
    }

    #[test]
    fn arrows_respect_app_cursor() {
        let up = key("up");
        assert_eq!(
            key_to_bytes(&up, TermModes { app_cursor: false }).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            key_to_bytes(&up, TermModes { app_cursor: true }).unwrap(),
            b"\x1bOA"
        );
    }

    #[test]
    fn shift_enter_is_newline() {
        let mut k = key("enter");
        assert_eq!(key_to_bytes(&k, TermModes::default()).unwrap(), b"\r");
        k.mods.shift = true;
        assert_eq!(key_to_bytes(&k, TermModes::default()).unwrap(), b"\n");
    }

    #[test]
    fn ctrl_c_is_etx() {
        let k = KeyInput {
            key: "c".into(),
            key_char: Some("c".into()),
            mods: Modifiers {
                control: true,
                ..Default::default()
            },
        };
        assert_eq!(key_to_bytes(&k, TermModes::default()).unwrap(), vec![3]);
    }

    #[test]
    fn alt_prefixes_escape() {
        let k = KeyInput {
            key: "b".into(),
            key_char: Some("b".into()),
            mods: Modifiers {
                alt: true,
                ..Default::default()
            },
        };
        assert_eq!(key_to_bytes(&k, TermModes::default()).unwrap(), b"\x1bb");
    }

    #[test]
    fn ctrl_pageup_reserved_for_chrome() {
        let k = KeyInput {
            key: "pageup".into(),
            key_char: None,
            mods: Modifiers {
                control: true,
                ..Default::default()
            },
        };
        assert_eq!(key_to_bytes(&k, TermModes::default()), None);
    }

    #[test]
    fn bracketed_paste_wraps() {
        assert_eq!(bracketed_paste("hi"), b"\x1b[200~hi\x1b[201~");
    }

    #[test]
    fn sgr_mouse_press_release_wheel() {
        assert_eq!(
            sgr_mouse(MouseButton::Left, 0, 0, true, Modifiers::default()),
            b"\x1b[<0;1;1M"
        );
        assert_eq!(
            sgr_mouse(MouseButton::Left, 5, 2, false, Modifiers::default()),
            b"\x1b[<0;6;3m"
        );
        assert_eq!(
            sgr_mouse(MouseButton::WheelUp, 0, 0, true, Modifiers::default()),
            b"\x1b[<64;1;1M"
        );
    }
}
