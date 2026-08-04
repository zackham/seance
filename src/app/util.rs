//! Small free helpers and drag-payload types for the SeanceApp view:
//! grid decode, tooltips, selection/DnD hygiene, status colors, time, and the
//! best-effort telegram status bridge.

use gpui::{div, prelude::*, Context, Render, Window};
use gpui_component::{GlobalState, WindowExt as _};

use crate::runtime::snapshot::GridSnapshot;
use crate::theme::SeancePalette;

pub(super) fn decode_grid_b64(
    data_b64: &str,
    base: Option<&GridSnapshot>,
) -> Result<GridSnapshot, String> {
    use crate::runtime::snapshot::decode_grid_bin_onto;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| e.to_string())?;
    decode_grid_bin_onto(&bytes, base)
}

/// Payload for dragging a sidebar pane row onto a workspace header.
#[derive(Clone)]
pub(super) struct DraggedPane {
    pub slug: String,
}

/// Payload for dragging a quicklaunch chip (reorder chips, insert-before).
#[derive(Clone)]
pub(super) struct DraggedQuickLaunch {
    pub name: String,
}

/// Tooltip helper: `.tooltip(tip("..."))` on any interactive element.
pub(super) fn tip(
    text: &'static str,
) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
    move |window, cx| gpui_component::tooltip::Tooltip::new(text).build(window, cx)
}

/// Owned-string tooltip (host chip labels, errors, …).
pub(super) fn tip_s(
    text: impl Into<String>,
) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
    let text = text.into();
    move |window, cx| gpui_component::tooltip::Tooltip::new(text.clone()).build(window, cx)
}

/// Standard selected-row fill for sidebar lists (workspaces, host chips, panes).
/// High-contrast on `bg_elevated` — not `surface` (too close to the panel).
#[inline]
pub(super) fn selected_row_fill() -> gpui::Hsla {
    SeancePalette::border()
}

pub(super) fn ui_debug(msg: &str) {
    if std::env::var("SEANCE_DEBUG_UI").is_ok() {
        eprintln!("[seance:ui] {msg}");
    }
}

/// Kill in-progress platform text selection (markdown file panes are
/// `.selectable(true)`). Same fix as the face chip: sidebar drag-and-drop
/// keeps the mouse button down while the cursor crosses the tile region, and
/// without this the markdown body treats that as a text drag-select.
///
/// Cheap when idle: `has_text_selection` short-circuits. Never call this from
/// `on_drag_move` — GPUI refreshes the whole window every drag move already,
/// and clear/end walks every selectable TextView. Continuous kill was the
/// sidebar DnD frame limiter.
pub(super) fn kill_text_selection(window: &mut Window, cx: &mut gpui::App) {
    if !window.has_text_selection(cx) {
        return;
    }
    window.end_text_selection(cx);
    window.clear_text_selection(cx);
}

/// Sidebar rows own their press/drag. Suppress window text selection for this
/// mouse-down (Button/Input pattern) so a reorder never starts a markdown
/// highlight — even before the drag threshold, and without per-move clears.
pub(super) fn sidebar_press_no_select(window: &mut Window, cx: &mut gpui::App) {
    GlobalState::suppress_text_selection(cx);
    kill_text_selection(window, cx);
}

/// The little pill that follows the cursor during a drag.
pub(super) struct DragPill {
    pub(super) label: String,
}

impl Render for DragPill {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(SeancePalette::surface())
            .border_1()
            .border_color(SeancePalette::flame_dim())
            .text_sm()
            .text_color(SeancePalette::text())
            .child(self.label.clone())
    }
}

pub(super) fn status_color(state: &str) -> gpui::Hsla {
    match state {
        "blocked" | "risky" => SeancePalette::danger(),
        "needs-human" => SeancePalette::violet(),
        "done" => SeancePalette::success(),
        "idle" => SeancePalette::text_faint(),
        _ => SeancePalette::flame(), // planning/working
    }
}

/// Claude Code / ink TUIs put a braille spinner in the OSC title while streaming.
/// Idle Claude uses `✳` (U+2733) — that is *not* busy.
///
/// The daemon runs this same predicate over every pane and pushes the verdict
/// (`GuiEvent::PaneBusy`); prefer `SeanceApp::busy_panes` for anything the
/// sidebar reads. This local copy is for panes whose grid we're painting.
pub(super) fn title_looks_busy(title: &str) -> bool {
    seance_core::util::title_looks_busy(title)
}

/// Braille spinner glyph for the sidebar workspace icon (replaces the word
/// "working" so names get more room). Phase is wall-clock so any re-render
/// advances the frame (terminal paint / status / pad tick).
pub(super) fn working_spinner_glyph() -> &'static str {
    // Classic CLI spinner frames (same family as Claude/ink titles).
    // 240ms cadence: the animation notify forces a FULL window re-render —
    // at 80ms that saturated the main thread on many-workspace sessions
    // (~55ms frames × 12.5/s = pegged CPU, laggy typing).
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let i = ((now_ms() / 240) as usize) % FRAMES.len();
    FRAMES[i]
}

/// Grace window after a grid frame arrives at NEW dimensions. The PTY resize
/// we ourselves requested makes the TUI redraw (SIGWINCH), and that redraw is
/// a content change indistinguishable from real output — it must not reset the
/// circle's activity clock.
pub(super) const RESIZE_SETTLE_MS: u64 = 400;

/// Does this grid frame count as real pane output for the workspace activity
/// clock? Pure so the reflow-vs-output rule is testable without a window.
///
/// - first paint (no previous cells): no — attach/first-view is not output
/// - dimension change: no — reflow, not output (and arms the settle window)
/// - content change inside the settle window: no — that's the resize redraw
/// - content change otherwise: yes
pub(super) fn grid_frame_is_output(
    prev_empty: bool,
    dims_match: bool,
    cells_changed: bool,
    now_ms: u64,
    settle_until: Option<u64>,
) -> bool {
    if prev_empty || !dims_match || !cells_changed {
        return false;
    }
    settle_until.is_none_or(|until| now_ms >= until)
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Single-quote a string for `sh -c` embedding.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// If `~/.local/share/seance/scratch/<slug>.telegram.json` exists on the
/// DAEMON machine, post status to that topic via vita — bind read and vita
/// call both run daemon-side over the fs bridge (agents + sidecars live
/// there). Best-effort, fire-and-forget on a plain thread: never blocks the
/// GUI, all failures are silently dropped.
pub(super) fn telegram_status_bridge(
    client: std::sync::Arc<crate::gui_client::GuiClient>,
    slug: &str,
    state: &str,
    note: Option<&str>,
) {
    let path = super::SeanceApp::phone_bind_path(slug);
    let text = match note {
        Some(n) if !n.is_empty() => format!("seance `{slug}` → *{state}*: {n}"),
        _ => format!("seance `{slug}` → *{state}*"),
    };
    std::thread::spawn(move || {
        let Ok((bytes, _)) = client.fs_read_string(&path) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&bytes) else {
            return;
        };
        let Some(topic_id) = v.get("topic_id").and_then(|t| t.as_str()) else {
            return;
        };
        let input = serde_json::json!({"topic_id": topic_id, "text": text});
        let args = format!(
            "capabilities call vita.telegram.send --input {}",
            sh_quote(&input.to_string())
        );
        // Prefer ~/work/vita/run on the daemon box; fall back to a PATH vita.
        let cmd = format!(
            "if [ -x \"$HOME/work/vita/run\" ]; then cd \"$HOME/work/vita\" && ./run {args}; \
             else vita {args}; fi >/dev/null 2>&1"
        );
        let _ = client.shell(&cmd);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hsla has no PartialEq — compare component floats exactly (palette
    /// consts are fixed literals, so bit-equality is fine).
    fn same_color(a: gpui::Hsla, b: gpui::Hsla) -> bool {
        a.h == b.h && a.s == b.s && a.l == b.l && a.a == b.a
    }

    /// Selecting a circle must never bump its last-active time. The first
    /// select lays out its tiles → PTY resize → SIGWINCH redraw; that redraw
    /// is a content change, and treating it as output is what made a circle
    /// jump to the top the first time it was clicked in a GUI session.
    #[test]
    fn grid_frame_is_output_ignores_first_paint_and_reflow() {
        let t = 10_000;
        // First frame for a pane (attach / first view): never output.
        assert!(!grid_frame_is_output(true, true, true, t, None));
        // Dimension change: reflow, not output.
        assert!(!grid_frame_is_output(false, false, true, t, None));
        // Redraw inside the settle window the reflow armed.
        assert!(!grid_frame_is_output(
            false,
            true,
            true,
            t,
            Some(t + RESIZE_SETTLE_MS)
        ));
        // Identical cells are never output.
        assert!(!grid_frame_is_output(false, true, false, t, None));
    }

    #[test]
    fn grid_frame_is_output_accepts_real_output() {
        let t = 10_000;
        assert!(grid_frame_is_output(false, true, true, t, None));
        // Settle window has expired — this is genuine output again.
        assert!(grid_frame_is_output(false, true, true, t, Some(t - 1)));
        assert!(grid_frame_is_output(false, true, true, t, Some(t)));
    }

    #[test]
    fn title_looks_busy_detects_braille_spinner() {
        // Claude/ink stream a braille spinner (U+2800..=U+28FF) in the title.
        assert!(title_looks_busy("\u{2800} building"));
        assert!(title_looks_busy("\u{28FF} working"));
        assert!(title_looks_busy("\u{2809} running tests"));
        // Leading whitespace is trimmed before the spinner check.
        assert!(title_looks_busy("   \u{2807} thinking"));
    }

    #[test]
    fn title_looks_busy_idle_and_empty() {
        // Idle Claude uses ✳ (U+2733) — explicitly NOT busy.
        assert!(!title_looks_busy("\u{2733} idle"));
        // Plain text titles.
        assert!(!title_looks_busy("bash"));
        assert!(!title_looks_busy("vim src/main.rs"));
        // Empty / whitespace-only.
        assert!(!title_looks_busy(""));
        assert!(!title_looks_busy("   "));
    }

    #[test]
    fn status_color_maps_variants_distinctly() {
        let blocked = status_color("blocked");
        let risky = status_color("risky");
        let needs_human = status_color("needs-human");
        let done = status_color("done");
        let idle = status_color("idle");
        let unknown = status_color("planning-or-anything-else");

        // Documented pairings.
        assert!(same_color(blocked, SeancePalette::danger()));
        assert!(same_color(risky, SeancePalette::danger()));
        assert!(same_color(needs_human, SeancePalette::violet()));
        assert!(same_color(done, SeancePalette::success()));
        assert!(same_color(idle, SeancePalette::text_faint()));
        // Fallback: unknown → flame (planning/working).
        assert!(same_color(unknown, SeancePalette::flame()));

        // Distinct families map to distinct colors.
        assert!(!same_color(blocked, needs_human));
        assert!(!same_color(needs_human, done));
        assert!(!same_color(done, idle));
        assert!(!same_color(idle, unknown));
        // blocked/risky share danger by design.
        assert!(same_color(blocked, risky));
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
        // JSON payloads (double quotes) ride through untouched inside 's.
        assert_eq!(sh_quote(r#"{"k":"v"}"#), r#"'{"k":"v"}'"#);
    }

    #[test]
    fn now_ms_is_monotonic_nonzero() {
        let a = now_ms();
        let b = now_ms();
        assert!(a > 0);
        assert!(b >= a);
    }
}
