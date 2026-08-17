//! The clipboard **write** seam, shared by every surface that copies (terminal
//! selections, file panes).
//!
//! It lives on its own because the interesting part isn't the write, it's the
//! Wayland detour: GPUI's in-process `write_to_clipboard` has taken the whole
//! GUI down with no Rust panic to show for it (compositor/libwayland inside the
//! call), so on Wayland we hand ownership to a `wl-copy` child instead. One
//! copy path means one place that knows that.
//!
//! Reading is deliberately not here — the only read is the terminal's
//! PRIMARY-selection paste, which has its own hang-avoidance rules and stays
//! next to the pane that uses it.

use std::io::Write;
use std::process::{Command, Stdio};

use gpui::App;

/// Bytes we're willing to hand to the compositor in one copy. Shared cap so a
/// pathological file or selection can't leave the GUI (or the clipboard owner)
/// holding unbounded data.
pub(crate) const MAX_COPY_BYTES: usize = 2 * 1024 * 1024;

/// Put `text` on the system clipboard. Returns a short "how" label for logs.
///
/// On Wayland, prefer the external `wl-copy` binary so a compositor bug can't
/// SIGSEGV the GUI process. Falls back to GPUI's in-process path (X11 / no
/// wl-copy). Panics from the GPUI path are caught; hard crashes are not.
pub(crate) fn copy_text_to_clipboard(text: &str, cx: &mut App) -> Result<&'static str, String> {
    // Wayland: never touch GPUI's in-process clipboard write if `wl-copy`
    // works. We've seen GUI deaths on ctrl+shift+c with zero Rust panic —
    // almost certainly compositor/libwayland inside `write_to_clipboard`.
    // Owning the selection via a child process keeps the crash out of seance.
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        if let Some(how) = try_wl_copy(text) {
            return Ok(how);
        }
        eprintln!("[seance gui] wl-copy unavailable or failed; falling back to gpui clipboard");
    }

    let t = text.to_string();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(t));
    }));
    match r {
        Ok(()) => Ok("gpui"),
        Err(_) => Err("gpui clipboard write panicked".into()),
    }
}

/// Trim `text` to [`MAX_COPY_BYTES`] on a char boundary, marking the cut so a
/// truncated paste can't pass for the whole thing. Returns true if it cut.
pub(crate) fn cap_copy_len(text: &mut String) -> bool {
    if text.len() <= MAX_COPY_BYTES {
        return false;
    }
    text.truncate(MAX_COPY_BYTES);
    // Avoid cutting mid-char.
    while !text.is_char_boundary(text.len()) {
        text.pop();
    }
    text.push_str("\n… [truncated]");
    true
}

/// Spawn `wl-copy` with the text on stdin. Does not wait for exit (wl-copy
/// often stays alive as the clipboard owner until replaced); a side thread
/// reaps the child to avoid zombies.
fn try_wl_copy(text: &str) -> Option<&'static str> {
    // Clipboard (Ctrl+C / Ctrl+V path).
    if !spawn_wl_copy(text, false) {
        return None;
    }
    // Primary selection too — middle-click paste after a mouse-drag select.
    let _ = spawn_wl_copy(text, true);
    Some("wl-copy")
}

fn spawn_wl_copy(text: &str, primary: bool) -> bool {
    let mut cmd = Command::new("wl-copy");
    if primary {
        cmd.arg("--primary");
    }
    // --foreground keeps it simple for small pastes in some versions; we still
    // don't wait. Prefer default (background) ownership behavior of wl-clipboard.
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        // Close stdin so wl-copy knows the payload is complete.
        drop(stdin);
    }
    // Reap in the background — wl-copy may exit immediately or linger as owner.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    true
}

/// "copied · 40 chars · 3 lines" — the toast every copy surface shows. Lines
/// are only mentioned when there's more than one (a path is one line).
pub(crate) fn copied_toast(text: &str) -> String {
    let chars = text.chars().count();
    let lines = text.lines().count().max(1);
    if lines > 1 {
        format!("copied · {chars} chars · {lines} lines")
    } else {
        format!("copied · {chars} chars")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_line_copy_doesnt_talk_about_lines() {
        assert_eq!(copied_toast("/home/zack/notes.md"), "copied · 19 chars");
    }

    #[test]
    fn a_document_reports_its_line_count() {
        assert_eq!(
            copied_toast("# hi\n\nbody\n"),
            "copied · 11 chars · 3 lines"
        );
    }

    #[test]
    fn an_empty_copy_still_counts_as_one_line() {
        assert_eq!(copied_toast(""), "copied · 0 chars");
    }

    #[test]
    fn a_short_copy_is_left_alone() {
        let mut s = "small".to_string();
        assert!(!cap_copy_len(&mut s));
        assert_eq!(s, "small");
    }

    #[test]
    fn an_oversized_copy_is_cut_on_a_char_boundary_and_says_so() {
        // Multi-byte chars straddling the cap: truncation must not split one.
        let mut s = "é".repeat(MAX_COPY_BYTES);
        assert!(cap_copy_len(&mut s));
        assert!(s.ends_with("\n… [truncated]"));
        // Still valid UTF-8 by construction (String), and we didn't grow.
        assert!(s.len() <= MAX_COPY_BYTES + "\n… [truncated]".len());
    }
}
