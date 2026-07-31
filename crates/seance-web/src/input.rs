//! DOM events → protocol input. Thin adapter only: the byte encoding itself
//! lives once in `seance_core::input`, so web and native emit identical bytes.
//!
//! Routing (chrome shortcut vs PTY) is the app layer's call — this module
//! reports the key faithfully, including `platform` (cmd/win). The one
//! encoding-relevant rule here: with ctrl or meta held we do NOT populate
//! `key_char`, so core takes the C0 path for ctrl+letter instead of echoing
//! the literal character.

use seance_core::input::{key_to_bytes, sgr_mouse, KeyInput, Modifiers, MouseButton, TermModes};
use seance_core::snapshot::GridSnapshot;

/// What a wheel event should produce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WheelAction {
    /// Daemon-side scrollback, in `GuiRequest::Scroll` convention
    /// (positive = back into history, i.e. wheel up) — note this is the
    /// INVERSE of the row sign below, which follows the DOM (positive = down).
    Scroll(i32),
    /// Bytes for the PTY (alternate-scroll arrows or SGR wheel reports).
    Bytes(Vec<u8>),
    /// Sub-row movement accumulated; nothing to send yet.
    None,
}

/// Rows per notch when the browser reports page-granularity deltas.
const ROWS_PER_PAGE: f64 = 20.0;

/// DOM `KeyboardEvent.key` → gpui-style key name for named keys.
/// `None` means "not a named key" (character key, or ignorable).
pub fn map_key_name(key: &str) -> Option<&'static str> {
    Some(match key {
        "Enter" => "enter",
        "Backspace" => "backspace",
        "Tab" => "tab",
        "Escape" | "Esc" => "escape",
        "ArrowUp" => "up",
        "ArrowDown" => "down",
        "ArrowLeft" => "left",
        "ArrowRight" => "right",
        "Home" => "home",
        "End" => "end",
        "PageUp" => "pageup",
        "PageDown" => "pagedown",
        "Delete" => "delete",
        "Insert" => "insert",
        "F1" => "f1",
        "F2" => "f2",
        "F3" => "f3",
        "F4" => "f4",
        "F5" => "f5",
        "F6" => "f6",
        "F7" => "f7",
        "F8" => "f8",
        "F9" => "f9",
        "F10" => "f10",
        "F11" => "f11",
        "F12" => "f12",
        _ => return None,
    })
}

/// Keys that carry no input: bare modifiers and IME/compat placeholders.
pub fn is_ignored_key(key: &str) -> bool {
    matches!(
        key,
        "Shift"
            | "Control"
            | "Alt"
            | "Meta"
            | "CapsLock"
            | "NumLock"
            | "ScrollLock"
            | "AltGraph"
            | "Dead"
            | "Unidentified"
            | "Process"
    )
}

/// Pure half of [`keyboard_to_keyinput`] — testable without a DOM.
pub fn key_input_from_parts(key: &str, mods: Modifiers) -> Option<KeyInput> {
    if is_ignored_key(key) {
        return None;
    }
    if let Some(name) = map_key_name(key) {
        return Some(KeyInput {
            key: name.to_string(),
            key_char: None,
            mods,
        });
    }
    let mut chars = key.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        // Multi-char, unmapped name (e.g. "AudioVolumeUp") — not PTY input.
        return None;
    };
    // ctrl/meta held → no key_char, so core emits the C0 code (ctrl) and the
    // app can claim cmd-shortcuts without a stray character escaping to the PTY.
    let key_char = if mods.control || mods.platform {
        None
    } else {
        Some(ch.to_string())
    };
    Some(KeyInput {
        key: ch.to_lowercase().to_string(),
        key_char,
        mods,
    })
}

pub fn keyboard_to_keyinput(ev: &web_sys::KeyboardEvent) -> Option<KeyInput> {
    let mods = Modifiers {
        shift: ev.shift_key(),
        control: ev.ctrl_key(),
        alt: ev.alt_key(),
        platform: ev.meta_key(),
    };
    key_input_from_parts(&ev.key(), mods)
}

/// deltaY (+ delta mode) → whole rows, positive = wheel DOWN. Fractional
/// trackpad pixels accumulate in `acc` across calls, otherwise slow scrolls
/// never move at all.
pub fn wheel_rows(delta_y: f64, delta_mode: u32, cell_h_css: f32, acc: &mut f64) -> i32 {
    let cell_h = if cell_h_css > 0.0 { cell_h_css as f64 } else { 1.0 };
    let rows_f = match delta_mode {
        web_sys::WheelEvent::DOM_DELTA_LINE => delta_y,
        web_sys::WheelEvent::DOM_DELTA_PAGE => delta_y * ROWS_PER_PAGE,
        // DOM_DELTA_PIXEL and anything unknown: treat as pixels.
        _ => delta_y / cell_h,
    };
    *acc += rows_f;
    let rows = acc.trunc();
    *acc -= rows;
    rows as i32
}

/// Repeat `key` (`"up"`/`"down"`) n times through the core encoder.
fn repeat_key(name: &str, n: usize, modes: TermModes) -> Vec<u8> {
    let input = KeyInput {
        key: name.to_string(),
        key_char: None,
        mods: Modifiers::default(),
    };
    let one = key_to_bytes(&input, modes).unwrap_or_default();
    let mut out = Vec::with_capacity(one.len() * n);
    for _ in 0..n {
        out.extend_from_slice(&one);
    }
    out
}

/// Wheel → alternate-scroll arrows, SGR wheel reports, or daemon scrollback,
/// in that precedence. `col`/`row` are the hovered cell (0-based).
pub fn wheel_to_action(
    ev: &web_sys::WheelEvent,
    snap: &GridSnapshot,
    cell_h_css: f32,
    col: u16,
    row: u16,
    acc: &mut f64,
) -> WheelAction {
    let rows = wheel_rows(ev.delta_y(), ev.delta_mode(), cell_h_css, acc);
    if rows == 0 {
        return WheelAction::None;
    }
    let n = rows.unsigned_abs() as usize;
    let up = rows < 0;

    if snap.alt_screen && snap.alternate_scroll {
        let modes = TermModes::from_snapshot(snap);
        return WheelAction::Bytes(repeat_key(if up { "up" } else { "down" }, n, modes));
    }
    if snap.mouse_mode && snap.sgr_mouse {
        let button = if up {
            MouseButton::WheelUp
        } else {
            MouseButton::WheelDown
        };
        let mut bytes = Vec::new();
        for _ in 0..n {
            bytes.extend_from_slice(&sgr_mouse(button, col, row, true, Modifiers::default()));
        }
        return WheelAction::Bytes(bytes);
    }
    // Daemon scrollback wants positive = back in history.
    WheelAction::Scroll(-rows)
}

/// Mouse button/release → SGR report. `None` for buttons we don't encode.
pub fn mouse_report(
    ev: &web_sys::MouseEvent,
    pressed: bool,
    col: u16,
    row: u16,
) -> Option<Vec<u8>> {
    let button = match ev.button() {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        _ => return None,
    };
    let mods = Modifiers {
        shift: ev.shift_key(),
        control: ev.ctrl_key(),
        alt: ev.alt_key(),
        platform: ev.meta_key(),
    };
    Some(sgr_mouse(button, col, row, pressed, mods))
}

/// Clipboard text → bracketed paste bytes.
pub fn paste_bytes(text: &str) -> Vec<u8> {
    seance_core::input::bracketed_paste(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    #[test]
    fn named_keys_map_to_gpui_names() {
        assert_eq!(map_key_name("Enter"), Some("enter"));
        assert_eq!(map_key_name("ArrowUp"), Some("up"));
        assert_eq!(map_key_name("PageDown"), Some("pagedown"));
        assert_eq!(map_key_name("F11"), Some("f11"));
        assert_eq!(map_key_name("a"), None);
    }

    #[test]
    fn bare_modifiers_and_ime_placeholders_are_dropped() {
        for k in ["Shift", "Control", "Alt", "Meta", "CapsLock", "Dead", "Process"] {
            assert!(key_input_from_parts(k, Modifiers::default()).is_none(), "{k}");
        }
    }

    #[test]
    fn char_keys_lowercase_key_and_keep_typed_char() {
        let k = key_input_from_parts("A", Modifiers { shift: true, ..Default::default() }).unwrap();
        assert_eq!(k.key, "a");
        assert_eq!(k.key_char.as_deref(), Some("A"));
    }

    #[test]
    fn ctrl_and_meta_suppress_key_char() {
        let k = key_input_from_parts("c", ctrl()).unwrap();
        assert_eq!(k.key, "c");
        assert!(k.key_char.is_none());
        assert_eq!(key_to_bytes(&k, TermModes::default()).unwrap(), vec![3]);

        let meta = Modifiers { platform: true, ..Default::default() };
        assert!(key_input_from_parts("v", meta).unwrap().key_char.is_none());
    }

    #[test]
    fn alt_char_still_carries_key_char() {
        let alt = Modifiers { alt: true, ..Default::default() };
        let k = key_input_from_parts("b", alt).unwrap();
        assert_eq!(k.key_char.as_deref(), Some("b"));
        assert_eq!(key_to_bytes(&k, TermModes::default()).unwrap(), b"\x1bb");
    }

    #[test]
    fn unknown_multichar_keys_are_ignored() {
        assert!(key_input_from_parts("AudioVolumeUp", Modifiers::default()).is_none());
    }

    #[test]
    fn pixel_deltas_accumulate_to_whole_rows() {
        let mut acc = 0.0;
        // 6px steps at 16px rows: nothing until the third event crosses 1 row.
        assert_eq!(wheel_rows(6.0, 0, 16.0, &mut acc), 0);
        assert_eq!(wheel_rows(6.0, 0, 16.0, &mut acc), 0);
        assert_eq!(wheel_rows(6.0, 0, 16.0, &mut acc), 1);
        assert!(acc.abs() < 0.2);
    }

    #[test]
    fn line_mode_is_rows_directly_and_up_is_negative() {
        let mut acc = 0.0;
        assert_eq!(wheel_rows(3.0, 1, 16.0, &mut acc), 3);
        assert_eq!(wheel_rows(-3.0, 1, 16.0, &mut acc), -3);
    }

    #[test]
    fn page_mode_scales() {
        let mut acc = 0.0;
        assert_eq!(wheel_rows(1.0, 2, 16.0, &mut acc), ROWS_PER_PAGE as i32);
    }

    #[test]
    fn zero_cell_height_does_not_divide_by_zero() {
        let mut acc = 0.0;
        assert_eq!(wheel_rows(32.0, 0, 0.0, &mut acc), 32);
    }

    #[test]
    fn alternate_scroll_arrows_respect_app_cursor() {
        let modes = TermModes { app_cursor: true };
        assert_eq!(repeat_key("up", 2, modes), b"\x1bOA\x1bOA");
        assert_eq!(
            repeat_key("down", 1, TermModes::default()),
            b"\x1b[B".to_vec()
        );
    }

    #[test]
    fn paste_is_bracketed() {
        assert_eq!(paste_bytes("hi"), b"\x1b[200~hi\x1b[201~");
    }
}
