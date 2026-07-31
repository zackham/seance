# seance — theme & branding

Aesthetic: **candlelit**. A dark room lit by one candle — deep warm charcoal
grounds, amber candle-glow accents, muted violet highlights. Elegant and
understated, not gothic kitsch. The séance metaphor (a circle of agents and a
human working in one shared space) is carried by two accents only: the
**flame** (amber — presence / human attention) and the **violet** counterpoint
(agent affordances: ghost propose, notes, history).

All colors are `gpui::Hsla`. Source of truth is the HSL in `src/theme.rs`;
hex below is computed from that HSL (sRGB round). Authored with
`gpui_component::hsl(h: 0..360, s: 0..100, l: 0..100)`.

## Core palette

| token            | HSL              | hex       | role |
|------------------|------------------|-----------|------|
| `bg`             | 345°  6%  7%     | `#131111` | the room — window + terminal pane ground |
| `bg_elevated`    | 345°  8% 10%     | `#1C1718` | lifted layer — panels, popovers, sidebar |
| `surface`        | 345°  8% 12%     | `#211C1D` | interactive fills — buttons, inputs, tabs |
| `border`         | 345° 10% 19%     | `#352C2E` | warm hairline separators |
| `text`           |  30° 27% 89%     | `#EBE3DB` | primary text — warm off-white |
| `text_dim`       |  25° 11% 61%     | `#A69A91` | secondary text — muted parchment |
| `text_faint`     |  15°  6% 39%     | `#69605D` | tertiary / disabled |
| `flame`          |  35° 80% 57%     | `#E9A03A` | **primary accent** — amber candle-glow |
| `flame_dim`      |  35° 62% 41%     | `#A97328` | dimmed ember — amber hover/active |
| `violet`         | 260° 45% 70%     | `#A790D5` | **secondary accent** — muted violet |
| `violet_dim`     | 258° 24% 47%     | `#6C5B95` | dimmed violet |
| `success`        | 110° 28% 60%     | `#86B67C` | soft sage — calm affirmation |
| `danger`         |   5° 55% 59%     | `#D0675D` | warm muted red — doesn't scream |

### Session status dots

| token             | maps to     | meaning |
|-------------------|-------------|---------|
| `status_exited`   | `text_faint`| process left the circle |

> `status_running` / `status_idle` were removed 2026-07-22 (dead code); live
> "working" state is drawn from observed TUI title spinners, not theme tokens.

## ANSI terminal palette (16-color)

**Not this module.** Agent CLIs and shells render inside the embedded terminal
against the **ghostty** 16-color palette, resolved daemon-side: `ANSI16` in
`src/runtime/pty_session.rs` (`#181818` bg / `#d8d8d8` fg, base16-ish
`#ab4642 #a1b56c #f7ca88 #7cafc2 #ba8baf #86c1b9` …), overridable by the
program via OSC 4/10/11. The seance-tinted ANSI table that used to live here
described `theme.rs::ansi_palette()`, which was removed 2026-07-22 with the
dead local-PTY path — nothing read it. Change terminal colors in
`pty_session.rs`; change chrome colors here.

## Public API (`src/theme.rs`)

```rust
pub struct SeancePalette;                 // color accessors, each -> gpui::Hsla
impl SeancePalette {
    pub fn bg() -> Hsla;            pub fn bg_elevated() -> Hsla;
    pub fn surface() -> Hsla;       pub fn border() -> Hsla;
    pub fn text() -> Hsla;          pub fn text_dim() -> Hsla;
    pub fn text_faint() -> Hsla;
    pub fn flame() -> Hsla;         pub fn flame_dim() -> Hsla;
    pub fn violet() -> Hsla;        pub fn violet_dim() -> Hsla;
    pub fn success() -> Hsla;       pub fn danger() -> Hsla;
    pub fn status_exited() -> Hsla;
}

pub fn init(cx: &mut gpui::App);             // activate dark theme + overrides
```

Colors are associated **functions** (not consts) because `gpui::hsla` is a
non-const constructor and `hsl()` clamps — this keeps a single source of truth
and lets values fall out of the same helper the reference crate uses. Zero
runtime cost (`#[inline]`, trivial arithmetic).

## How `init()` applies the theme

Call order in `main`: `gpui_component::init(cx)` → **`theme::init(cx)`** →
open window.

`init()` does four steps (see `src/theme.rs::init`); step 4 is font-family
hygiene on non-macOS — pick the first installed sans/mono rather than letting
gpui probe, which is load-bearing for typing latency:

1. **`Theme::change(ThemeMode::Dark, None, cx)`** — activates dark mode.
   `Theme::change` lazily creates the `Theme` global if absent and calls
   `apply_config(dark_theme)`, seeding every `ThemeColor` field from the
   built-in *Default Dark* before we override.
   — `crates/ui/src/theme/mod.rs:163-183`
2. **Mutate `Theme::global_mut(cx).colors`** — `Theme` derefs to `ThemeColor`
   and `colors` is a public field, so we overwrite ~70 relevant fields with the
   candlelit palette (`apply_palette`).
   — `crates/ui/src/theme/mod.rs:97-109` (Deref/DerefMut), field list at
   `crates/ui/src/theme/theme_color.rs:57-345`
3. **`theme.tokens = ThemeTokens::from(&theme.colors)`** — widgets paint from
   the resolved `ThemeTokens` (color + paint background), which does **not**
   auto-recompute when we poke individual `colors` fields. This regeneration is
   the load-bearing step — skip it and the overrides are invisible to
   components.
   — `ThemeTokens` def `theme_color.rs:347-368`; `From<&ThemeColor>`
   `theme_color.rs:364-368`; same pattern used internally at `mod.rs:234` and
   `schema.rs:939`.

Color transforms (`.opacity/.lighten/.darken`) come from `gpui_component`'s
`Colorize` trait (`crates/ui/src/theme/color.rs:19-61`).

### Field-mapping decisions

- **`primary` = flame**; `warning` also = flame (keeps the app to two accents).
- **`info` / `link` = violet**; violet is the only cool accent.
- `primary_foreground` / `success_foreground` / `warning_foreground` = `bg`
  (dark text on the bright amber/sage for contrast).
- `ring` / `caret` = flame — the candle picks out what has focus.
- `selection` = `violet_dim` @ 45% alpha.
- Inputs are border-driven on dark: gpui-component's `Theme::input_background()`
  mixes `input` with transparent, so we set `input = border` and let focus
  recolor via `ring` (`mod.rs:189-196`).

## Uncertainty flags for the integrator

1. **hex is computed, HSL is canonical.** The table hex is a Python
   `colorsys` sRGB round-trip of the HSL in `theme.rs`; gpui's own HSL→sRGB
   conversion may differ by ±1 per channel. Trust the on-screen render / the
   HSL, not the hex.
2. **`apply_palette` covers the fields seance uses.** The `chart_*` swatches and
   the base `red/green/blue/yellow/magenta/cyan(+_light)` `ThemeColor` fields are
   left at the Default-Dark seed — they only feed data-viz widgets seance
   doesn't ship, and terminal color is resolved daemon-side
   (ghostty palette in `runtime/pty_session.rs`), not by the theme module.
   If a component you add reads those, extend `apply_palette`.
   (`ansi_palette()` was removed 2026-07-22 with the dead local-PTY path.)
3. **No `window_border` on non-Linux**, per gpui-component docs
   (`theme_color.rs:314-319`). seance is Linux-only, so this is fine; we set it
   to `border`.
4. **`Theme::change` re-seeds from Default Dark, then we override.** If a future
   gpui-component rev renames/removes a `ThemeColor` field this file sets, that
   line won't compile — the field list is pinned to rev
   `b5eef62336f88bb6c1ee45bf32f73c9895d49f8d`. Verified against the local
   checkout, not training data.
5. **The web client re-declares this palette by hand.** `crates/seance-web`
   has no access to `theme.rs`: the same values appear as GL constants
   (`renderer.rs`) and as hex literals in the chrome CSS (`ui.rs`, `help.rs`,
   `activity.rs`, `menus.rs`, `probe.rs`). Values currently match; there is no
   compile-time link, so a change here must be mirrored there by hand.
```
