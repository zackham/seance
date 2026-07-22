//! seance — candlelit theme + branding module.
//!
//! Aesthetic: a dark room lit by one candle. Deep warm charcoal grounds,
//! amber candle-glow accents, muted violet highlights. Elegant, understated —
//! not gothic kitsch.
//!
//! ── palette (hex, for human reference — computed from the HSL below) ────────
//!   bg           #131111   deepest warm charcoal (window/pane ground)
//!   bg_elevated  #1C1718   slightly lifted charcoal (panels, popovers)
//!   surface      #211C1D   interactive surface (buttons, inputs, tabs)
//!   border       #352C2E   hairline separators, warm-tinted
//!   text         #EBE3DB   warm off-white (primary text)
//!   text_dim     #A69A91   muted parchment (secondary text)
//!   text_faint   #69605D   faint (disabled / tertiary)
//!   flame        #E9A03A   amber candle-glow accent (primary)
//!   flame_dim    #A97328   dimmed ember (hover/active amber)
//!   violet       #A790D5   muted violet highlight (secondary accent)
//!   violet_dim   #6C5B95   dimmed violet
//!   success      #86B67C   soft sage (calm, not neon)
//!   danger       #D0675D   warm muted red (does not scream)
//!   status_running #E9A03A amber — a spirit is present / working
//!   status_idle    #A69A91 dim  — summoned but quiet
//!   status_exited  #69605D gray — the circle closed
//! ───────────────────────────────────────────────────────────────────────────
//!
//! All colors are `gpui::Hsla`. We author them with `gpui_component::hsl`
//! (h: 0..360, s: 0..100, l: 0..100) — this is the crate's own helper
//! (crates/ui/src/theme/color.rs:16) so degrees/percent match the table above.

use gpui::{App, Hsla};
use gpui_component::{hsl, Colorize as _, Theme, ThemeColor, ThemeMode, ThemeTokens};

/// The candlelit palette. Each associated fn returns a `gpui::Hsla`.
///
/// These are the load-bearing colors the seance app references directly for
/// its own chrome (pane borders, status dots, the summon glow). gpui-component
/// widgets get the same palette applied to their `Theme` in [`init`].
pub struct SeancePalette;

impl SeancePalette {
    // ── grounds ────────────────────────────────────────────────────────────
    /// Deepest warm charcoal — the room. Window + terminal pane background.
    #[inline]
    pub fn bg() -> Hsla {
        hsl(345., 6., 7.) // #131111
    }
    /// Slightly lifted charcoal — panels, popovers, the layer above the room.
    #[inline]
    pub fn bg_elevated() -> Hsla {
        hsl(345., 8., 10.) // #1C1718
    }
    /// Interactive surface — button/input/tab fills sitting on the ground.
    #[inline]
    pub fn surface() -> Hsla {
        hsl(345., 8., 12.) // #211C1D
    }
    /// Warm-tinted hairline border between surfaces.
    #[inline]
    pub fn border() -> Hsla {
        hsl(345., 10., 19.) // #352C2E
    }

    // ── text ───────────────────────────────────────────────────────────────
    /// Primary text — warm off-white, easy on a dark ground.
    #[inline]
    pub fn text() -> Hsla {
        hsl(30., 27., 89.) // #EBE3DB
    }
    /// Secondary text — muted parchment.
    #[inline]
    pub fn text_dim() -> Hsla {
        hsl(25., 11., 61.) // #A69A91
    }
    /// Tertiary / disabled text — faint.
    #[inline]
    pub fn text_faint() -> Hsla {
        hsl(15., 6., 39.) // #69605D
    }

    // ── accents ──────────────────────────────────────────────────────────────
    /// Amber candle-glow — the primary accent. The flame.
    #[inline]
    pub fn flame() -> Hsla {
        hsl(35., 80., 57.) // #E9A03A
    }
    /// Dimmed ember — amber hover / pressed / low-emphasis accent.
    #[inline]
    pub fn flame_dim() -> Hsla {
        hsl(35., 62., 41.) // #A97328
    }
    /// Muted violet — secondary highlight. The cool counterpoint to the flame.
    #[inline]
    pub fn violet() -> Hsla {
        hsl(260., 45., 70.) // #A790D5
    }
    /// Dimmed violet — violet hover / low-emphasis.
    #[inline]
    pub fn violet_dim() -> Hsla {
        hsl(258., 24., 47.) // #6C5B95
    }

    // ── semantic ─────────────────────────────────────────────────────────────
    /// Soft sage — success, calm affirmation (not neon green).
    #[inline]
    pub fn success() -> Hsla {
        hsl(110., 28., 60.) // #86B67C
    }
    /// Warm muted red — danger that doesn't scream in a candlelit room.
    #[inline]
    pub fn danger() -> Hsla {
        hsl(5., 55., 59.) // #D0675D
    }

    // ── session status dots ──────────────────────────────────────────────────
    /// A spirit is present / actively working — amber, alive.
    #[inline]
    pub fn status_running() -> Hsla {
        Self::flame()
    }
    /// Summoned but quiet — dim parchment.
    #[inline]
    pub fn status_idle() -> Hsla {
        Self::text_dim()
    }
    /// The circle closed — gray, spent.
    #[inline]
    pub fn status_exited() -> Hsla {
        Self::text_faint()
    }
}

/// A custom 16-color ANSI palette, candlelit-tuned.
///
/// Claude Code's TUI renders inside the embedded terminal using these colors,
/// so they must feel native to the app: readable on the deep charcoal ground,
/// amber-warm yellows, violet-tinted magentas/blues. Order is standard ANSI:
///
/// ```text
///  0 black     1 red      2 green    3 yellow
///  4 blue      5 magenta  6 cyan     7 white
///  8 bright-black (gray)  9 bright-red   10 bright-green  11 bright-yellow
/// 12 bright-blue  13 bright-magenta  14 bright-cyan  15 bright-white
/// ```
///
/// # Hex reference
/// ```text
///  0 #1C1718   1 #C7594D   2 #7CA871   3 #E9A03A
///  4 #7C91CB   5 #A790D5   6 #71ADAB   7 #C9BDB1
///  8 #494143   9 #E1776B  10 #94C487  11 #F4BE62
/// 12 #94A8E0  13 #BFA8E6  14 #8FCCCA  15 #EBE3DB
/// ```
pub fn ansi_palette() -> [Hsla; 16] {
    [
        // ── normal ──────────────────────────────────────────────────────────
        hsl(345., 8., 10.),  //  0 black — matches bg_elevated, not pure black
        hsl(6., 52., 54.),   //  1 red — warm brick
        hsl(108., 24., 55.), //  2 green — muted sage
        hsl(35., 80., 57.),  //  3 yellow — the flame (amber, not lemon)
        hsl(224., 43., 64.), //  4 blue — dusty periwinkle
        hsl(260., 45., 70.), //  5 magenta — the violet highlight
        hsl(178., 27., 56.), //  6 cyan — muted teal
        hsl(30., 18., 74.),  //  7 white — soft parchment
        // ── bright ──────────────────────────────────────────────────────────
        hsl(345., 6., 27.),  //  8 bright-black — warm gray (dim text on bg)
        hsl(6., 66., 65.),   //  9 bright-red — lifted brick
        hsl(108., 34., 65.), // 10 bright-green — lifted sage
        hsl(38., 87., 67.),  // 11 bright-yellow — bright candle
        hsl(224., 55., 73.), // 12 bright-blue — lifted periwinkle
        hsl(262., 55., 78.), // 13 bright-magenta — lifted violet
        hsl(178., 37., 68.), // 14 bright-cyan — lifted teal
        hsl(30., 27., 89.),  // 15 bright-white — the primary text warm off-white
    ]
}

/// Activate the candlelit dark theme and apply the palette to gpui-component's
/// global `Theme` so its widgets (buttons, inputs, panels, tabs, scrollbars)
/// match seance's chrome.
///
/// Must be called AFTER `gpui_component::init(cx)` (which registers the
/// `Theme`/`ThemeRegistry` globals), and before opening the window.
///
/// Mechanism: `gpui_component`'s `Theme` derefs to `ThemeColor`
/// (crates/ui/src/theme/mod.rs:97-103), so we mutate its color fields in place,
/// then regenerate `theme.tokens` — widgets paint from `theme.tokens`
/// (`ThemeTokens`, crates/ui/src/theme/theme_color.rs:347-368), which is derived
/// from `colors` and does NOT auto-update when we poke individual fields.
pub fn init(cx: &mut App) {
    // 1. Switch gpui-component into dark mode. `Theme::change` lazily creates
    //    the global if missing and calls `apply_config(dark_theme)`, seeding
    //    every ThemeColor field from the built-in Default Dark before we
    //    override the ones we care about.
    //    (crates/ui/src/theme/mod.rs:163-183)
    Theme::change(ThemeMode::Dark, None, cx);

    let p = palette();
    let theme = Theme::global_mut(cx);

    // 2. Overwrite ThemeColor fields with the candlelit palette. `theme` derefs
    //    to `&mut ThemeColor`, so `theme.background` etc. address the color
    //    fields directly (crates/ui/src/theme/mod.rs:105-109).
    apply_palette(&mut theme.colors, &p);

    // 3. Regenerate the resolved tokens from the mutated colors. Widgets read
    //    `theme.tokens.<field>` (a ThemeToken = solid color + paint bg), so
    //    without this step the overrides would be invisible to components.
    //    `ThemeTokens: From<&ThemeColor>` (theme_color.rs:364-368).
    theme.tokens = ThemeTokens::from(&theme.colors);
}

// ── internal wiring ─────────────────────────────────────────────────────────

/// Snapshot of every palette color, computed once per `init`.
struct Palette {
    bg: Hsla,
    bg_elevated: Hsla,
    surface: Hsla,
    border: Hsla,
    text: Hsla,
    text_dim: Hsla,
    text_faint: Hsla,
    flame: Hsla,
    flame_dim: Hsla,
    violet: Hsla,
    violet_dim: Hsla,
    success: Hsla,
    danger: Hsla,
}

fn palette() -> Palette {
    Palette {
        bg: SeancePalette::bg(),
        bg_elevated: SeancePalette::bg_elevated(),
        surface: SeancePalette::surface(),
        border: SeancePalette::border(),
        text: SeancePalette::text(),
        text_dim: SeancePalette::text_dim(),
        text_faint: SeancePalette::text_faint(),
        flame: SeancePalette::flame(),
        flame_dim: SeancePalette::flame_dim(),
        violet: SeancePalette::violet(),
        violet_dim: SeancePalette::violet_dim(),
        success: SeancePalette::success(),
        danger: SeancePalette::danger(),
    }
}

/// Map the candlelit palette onto every relevant `ThemeColor` field.
///
/// Fields we intentionally leave at the Default-Dark seed: the chart_* /
/// base-color swatches (`red`, `green`, `blue`, `yellow`, `magenta`, `cyan`,
/// and their `_light` variants) — those are only used by data-viz widgets
/// seance doesn't ship, and the ANSI palette covers terminal color separately.
#[allow(clippy::field_reassign_with_default)]
fn apply_palette(c: &mut ThemeColor, p: &Palette) {
    // Grounds & text.
    c.background = p.bg;
    c.foreground = p.text;
    c.border = p.border;
    c.muted = p.surface;
    c.muted_foreground = p.text_dim;

    // Accent (hover backgrounds on menu/list items) → warm surface + amber text.
    c.accent = p.surface;
    c.accent_foreground = p.flame;

    // Focus ring / caret / selection — the candle picks out what has focus.
    c.ring = p.flame;
    c.caret = p.flame;
    c.selection = p.violet_dim.opacity(0.45);

    // Primary = the flame. Amber buttons, primary emphasis.
    c.primary = p.flame;
    c.primary_hover = p.flame.lighten(0.06);
    c.primary_active = p.flame_dim;
    c.primary_foreground = p.bg; // dark text on amber for contrast

    // Secondary = warm surface, understated.
    c.secondary = p.surface;
    c.secondary_hover = p.surface.lighten(0.04);
    c.secondary_active = p.surface.darken(0.03);
    c.secondary_foreground = p.text;

    // Default button chrome (neutral).
    c.button = p.surface;
    c.button_foreground = p.text;
    c.button_hover = p.surface.lighten(0.04);
    c.button_active = p.surface.darken(0.03);

    // Inputs — border-driven on dark (Theme::input_background mixes this with
    // transparent). Keep the input border warm; focus recolors via `ring`.
    c.input = p.border;

    // Links — violet, the cool counterpoint.
    c.link = p.violet;
    c.link_hover = p.violet.lighten(0.06);
    c.link_active = p.violet_dim;

    // Semantic states.
    c.success = p.success;
    c.success_foreground = p.bg;
    c.success_hover = p.success.lighten(0.06);
    c.success_active = p.success.darken(0.06);

    c.danger = p.danger;
    c.danger_foreground = p.text;
    c.danger_hover = p.danger.lighten(0.06);
    c.danger_active = p.danger.darken(0.06);

    // Info = violet family; warning = ember (keeps the two-accent discipline).
    c.info = p.violet;
    c.info_foreground = p.bg;
    c.info_hover = p.violet.lighten(0.06);
    c.info_active = p.violet_dim;

    c.warning = p.flame;
    c.warning_foreground = p.bg;
    c.warning_hover = p.flame.lighten(0.06);
    c.warning_active = p.flame_dim;

    // Popover / list / tab surfaces — the lifted layer.
    c.popover = p.bg_elevated;
    c.popover_foreground = p.text;

    c.list = p.bg;
    c.list_active = p.surface;
    c.list_active_border = p.flame_dim;
    c.list_hover = p.surface.opacity(0.6);
    c.list_head = p.bg_elevated;
    c.list_even = p.bg_elevated.opacity(0.4);

    c.tab_bar = p.bg;
    c.tab = p.bg;
    c.tab_active = p.surface;
    c.tab_foreground = p.text_dim;
    c.tab_active_foreground = p.text;
    c.tab_bar_segmented = p.bg_elevated;

    // Sidebar — the séance ledger; slightly lifted, amber-picked active items.
    c.sidebar = p.bg_elevated;
    c.sidebar_foreground = p.text_dim;
    c.sidebar_border = p.border;
    c.sidebar_accent = p.surface;
    c.sidebar_accent_foreground = p.flame;
    c.sidebar_primary = p.flame;
    c.sidebar_primary_foreground = p.bg;

    // Window chrome bars.
    c.title_bar = p.bg;
    c.title_bar_border = p.border;
    c.status_bar = p.bg_elevated;
    c.status_bar_border = p.border;
    c.window_border = p.border;

    // Scrollbars — barely-there on the ground, amber-warm thumb.
    c.scrollbar = p.bg;
    c.scrollbar_thumb = p.border;
    c.scrollbar_thumb_hover = p.flame_dim;

    // Overlays / drag / skeleton.
    c.overlay = p.bg.opacity(0.6);
    c.drop_target = p.flame.opacity(0.12);
    c.drag_border = p.flame;
    c.skeleton = p.surface;

    // Progress / sliders / switches — flame-driven.
    c.progress_bar = p.flame;
    c.slider_bar = p.flame;
    c.slider_thumb = p.text;
    c.switch = p.surface;
    c.switch_thumb = p.text;

    // Tiles (dock-able pane grid) — the ground.
    c.tiles = p.bg;
    c.table = p.bg;
    c.table_head = p.bg_elevated;
    c.table_head_foreground = p.text_dim;
    c.table_row_border = p.border;
    c.table_hover = p.surface.opacity(0.6);
    c.table_active = p.surface;
    c.table_active_border = p.flame_dim;
}
