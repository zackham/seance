//! Shared terminal font — match ghostty on this machine.
//!
//! Ghostty config: `font-family = monospace` which resolves (fc-match) to
//! **JetBrainsMono Nerd Font**, size 9, ligatures off. Seance uses the same
//! family so Claude's block-drawing logo aligns the same way.

use gpui::{font, Font, FontFeatures, SharedString};

/// Family ghostty's `monospace` resolves to on this host.
pub const FONT_FAMILY: &str = "JetBrainsMono Nerd Font";

/// Ghostty uses 9; we keep a slightly larger default for multi-pane grids.
/// Still the same face — only the size differs for readability.
pub const FONT_SIZE: f32 = 12.0;

/// Tight line height so half-blocks stack cleanly (ghostty-ish).
pub const LINE_HEIGHT_FACTOR: f32 = 1.2;

/// Terminal font with ligatures disabled (matches ghostty's -liga/-calt).
pub fn term_font() -> Font {
    Font {
        family: SharedString::from(FONT_FAMILY),
        features: FontFeatures(std::sync::Arc::new(vec![
            ("calt".into(), 0),
            ("liga".into(), 0),
            ("clig".into(), 0),
            ("dlig".into(), 0),
            ("hlig".into(), 0),
        ])),
        weight: Default::default(),
        style: Default::default(),
        fallbacks: None,
    }
}

pub fn term_font_bold() -> Font {
    term_font().bold()
}

/// Convenience when only family is needed (legacy call sites).
#[allow(dead_code)]
pub fn term_font_plain() -> Font {
    font(FONT_FAMILY)
}
