//! Shared terminal font — match ghostty on this machine.
//!
//! Ghostty config: `font-family = monospace` which resolves (fc-match) to
//! **JetBrainsMono Nerd Font**, size 9, ligatures off. Seance uses the same
//! family so Claude's block-drawing logo aligns the same way.
//! `font-family` / `font-size` in `~/.config/seance/terminal.conf` override
//! both (see `term_config`).

use gpui::{font, Font, FontFeatures, SharedString};

/// Family ghostty's `monospace` resolves to on this host.
const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font";

/// Ghostty uses 9; we keep a slightly larger default for multi-pane grids.
/// Still the same face — only the size differs for readability.
const DEFAULT_FONT_SIZE: f32 = 12.0;

pub fn font_family() -> &'static str {
    crate::term_config::get()
        .font_family
        .as_deref()
        .unwrap_or(DEFAULT_FONT_FAMILY)
}

/// Font size in GPUI pixels. The config is in points like Ghostty's: macOS
/// draws a point as one logical pixel, Linux as 96/72 of one.
pub fn font_size() -> f32 {
    match crate::term_config::get().font_size {
        Some(pt) if cfg!(target_os = "macos") => pt,
        Some(pt) => pt * 96.0 / 72.0,
        None => DEFAULT_FONT_SIZE,
    }
}

/// Tight line height so half-blocks stack cleanly (ghostty-ish).
pub const LINE_HEIGHT_FACTOR: f32 = 1.2;

/// Terminal font with ligatures disabled (matches ghostty's -liga/-calt).
pub fn term_font() -> Font {
    Font {
        family: SharedString::from(font_family()),
        features: FontFeatures(std::sync::Arc::new(vec![
            ("calt".into(), 0),
            ("liga".into(), 0),
            ("clig".into(), 0),
            ("dlig".into(), 0),
            ("hlig".into(), 0),
        ])),
        weight: Default::default(),
        style: Default::default(),
        // Sane monospace degradation when the Nerd Font isn't installed
        // (fresh thin-client machines): Menlo covers macOS, the rest Linux.
        fallbacks: Some(gpui::FontFallbacks::from_fonts(vec![
            "Menlo".into(),
            "JetBrains Mono".into(),
            "DejaVu Sans Mono".into(),
            "monospace".into(),
        ])),
    }
}

pub fn term_font_bold() -> Font {
    term_font().bold()
}

/// Convenience when only family is needed (legacy call sites).
#[allow(dead_code)]
pub fn term_font_plain() -> Font {
    font(font_family())
}
