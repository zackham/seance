//! Canvas view that renders a [`RemoteTerminal`]'s grid snapshot.
//!
//! Glyphs are **snapped to the cell grid** via `shape_line(..., force_width)` so
//! block-drawing art (Claude's logo) doesn't wave — same idea as ghostty.
//! Text is **batched into style runs** (not one shape_line per cell) so typing
//! stays cheap; the old per-cell path pegged a core at ~90% idle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use gpui::{
    canvas, div, fill, point, prelude::*, px, App, Bounds, Context, FocusHandle, Focusable, Hsla,
    KeyDownEvent, Pixels, ScrollWheelEvent, ShapedLine, SharedString, TextRun, Window,
};

use crate::remote_term::RemoteTerminal;
use crate::runtime::snapshot::CellSnap;
use crate::term_font::{self, term_font, term_font_bold, FONT_SIZE, LINE_HEIGHT_FACTOR};
use crate::terminal::keystroke_bytes;
use crate::theme::SeancePalette;
use alacritty_terminal::term::TermMode;

pub use crate::term_font::FONT_FAMILY;

pub struct RemoteTerminalView {
    pub terminal: gpui::Entity<RemoteTerminal>,
    focus_handle: FocusHandle,
    scroll_accum: f32,
}

impl RemoteTerminalView {
    pub fn new(terminal: gpui::Entity<RemoteTerminal>, cx: &mut Context<Self>) -> Self {
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        Self {
            terminal,
            focus_handle: cx.focus_handle(),
            scroll_accum: 0.,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let term = self.terminal.read(cx);

        if term.ghost.is_some() {
            match ks.key.as_str() {
                "enter" | "tab" => {
                    term.ghost_accept();
                    cx.stop_propagation();
                    return;
                }
                "escape" => {
                    term.ghost_reject();
                    cx.stop_propagation();
                    return;
                }
                _ => {
                    if ks.key_char.is_some() {
                        term.ghost_reject();
                    }
                }
            }
        }

        if ks.modifiers.shift && (ks.key == "pageup" || ks.key == "pagedown") {
            let page = term.snapshot.rows as i32 - 2;
            let delta = if ks.key == "pageup" { page } else { -page };
            if term.snapshot.alt_screen {
                let bytes = if ks.key == "pageup" {
                    b"\x1b[5~".to_vec()
                } else {
                    b"\x1b[6~".to_vec()
                };
                term.write_bytes(bytes);
            } else {
                term.scroll_lines(delta);
            }
            cx.stop_propagation();
            return;
        }

        // Ctrl+PageUp/Down: workspace cycle (seance chrome) — bubble, no PTY.
        if ks.modifiers.control
            && !ks.modifiers.shift
            && (ks.key == "pageup" || ks.key == "pagedown")
        {
            return;
        }

        // Ctrl+Shift chords belong to the app (summon, notes, popout, …).
        if ks.modifiers.control && ks.modifiers.shift {
            return;
        }

        let mut mode = TermMode::empty();
        if term.snapshot.app_cursor {
            mode.insert(TermMode::APP_CURSOR);
        }
        if let Some(bytes) = keystroke_bytes(&event.keystroke, mode) {
            // Local echo printable singles before the round-trip grid returns.
            let echo = ks
                .key_char
                .as_ref()
                .and_then(|s| {
                    if !ks.modifiers.control && !ks.modifiers.alt && s.chars().count() == 1 {
                        s.chars().next()
                    } else {
                        None
                    }
                })
                .filter(|c| !c.is_control());
            self.terminal.update(cx, |t, cx| {
                t.scroll_to_bottom();
                t.write_bytes(bytes);
                if let Some(ch) = echo {
                    t.local_echo_char(ch, cx);
                }
            });
            cx.stop_propagation();
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let line_height = px(FONT_SIZE * LINE_HEIGHT_FACTOR);
        self.scroll_accum +=
            f32::from(event.delta.pixel_delta(line_height).y) / f32::from(line_height);
        let lines = self.scroll_accum.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_accum -= lines as f32;

        let snap = self.terminal.read(cx).snapshot.clone();
        let n = lines.unsigned_abs() as usize;
        let wheel_up = lines > 0;

        if snap.mouse_mode {
            let col = (snap.cursor_col as u16).saturating_add(1).max(1);
            let row = (snap.cursor_row as u16).saturating_add(1).max(1);
            let button: u8 = if wheel_up { 64 } else { 65 };
            let mut bytes = Vec::new();
            for _ in 0..n {
                if snap.sgr_mouse {
                    bytes.extend_from_slice(format!("\x1b[<{button};{col};{row}M").as_bytes());
                } else {
                    bytes.extend_from_slice(&[
                        0x1b,
                        b'[',
                        b'M',
                        32 + button,
                        32 + (col.min(223) as u8),
                        32 + (row.min(223) as u8),
                    ]);
                }
            }
            self.terminal.read(cx).write_bytes(bytes);
        } else if snap.alt_screen && snap.alternate_scroll {
            let key: &[u8] = if snap.app_cursor {
                if wheel_up {
                    b"\x1bOA"
                } else {
                    b"\x1bOB"
                }
            } else if wheel_up {
                b"\x1b[A"
            } else {
                b"\x1b[B"
            };
            let mut bytes = Vec::new();
            for _ in 0..n {
                bytes.extend_from_slice(key);
            }
            self.terminal.read(cx).write_bytes(bytes);
        } else {
            self.terminal.read(cx).scroll_lines(lines);
        }
        cx.notify();
    }
}

impl Focusable for RemoteTerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RemoteTerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let term = self.terminal.clone();
        let focus = self.focus_handle.clone();

        div()
            .id("remote-term-view")
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .track_focus(&focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        // Cache cell metrics — shaping █ every frame was pure waste.
                        let (cell_w, line_h) = cell_metrics(window);

                        let cols =
                            ((f32::from(bounds.size.width) / f32::from(cell_w)) as u16).max(2);
                        let rows =
                            ((f32::from(bounds.size.height) / f32::from(line_h)) as u16).max(2);
                        // Debounced — single-frame col jitter must not thrash.
                        term.update(cx, |t, _| t.resize_cells(cols, rows));

                        // Arc clone — not a deep copy of every cell.
                        let snap = Arc::clone(&term.read(cx).snapshot);
                        let ghost = term.read(cx).ghost.clone();
                        let slug = term.read(cx).slug.clone();
                        Layout {
                            slug,
                            bounds,
                            cell_w,
                            line_h,
                            snap,
                            ghost_text: ghost.map(|g| g.text),
                        }
                    },
                    |_bounds, layout: Layout, window, cx| {
                        paint_grid(&layout, window, cx);
                    },
                )
                .size_full(),
            )
    }
}

struct Layout {
    slug: String,
    bounds: Bounds<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
    snap: Arc<crate::runtime::snapshot::GridSnapshot>,
    ghost_text: Option<String>,
}

/// Shaped paint replay for a terminal whose grid hasn't changed.
///
/// GPUI does a full `window.refresh()` on every mouse move while a drag is
/// active (sidebar DnD pill follows the cursor). Without this cache we re-scan
/// the grid and re-`shape_line` every visible terminal every move — feels like
/// frame limiting even when nothing in the PTY changed.
#[derive(Clone)]
struct ShapedPaintCache {
    rev: u64,
    origin_x: f32,
    origin_y: f32,
    width: f32,
    height: f32,
    cell_w: f32,
    line_h: f32,
    ghost: Option<String>,
    rects: Vec<(f32, f32, f32, f32, Hsla)>,
    texts: Vec<(f32, f32, ShapedLine)>,
    cursor: (f32, f32, f32, f32),
    ghost_shaped: Option<(f32, f32, ShapedLine)>,
}

fn shaped_paint_caches() -> &'static Mutex<HashMap<String, ShapedPaintCache>> {
    static C: OnceLock<Mutex<HashMap<String, ShapedPaintCache>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_matches(c: &ShapedPaintCache, layout: &Layout) -> bool {
    c.rev == layout.snap.rev
        && c.origin_x == f32::from(layout.bounds.origin.x)
        && c.origin_y == f32::from(layout.bounds.origin.y)
        && c.width == f32::from(layout.bounds.size.width)
        && c.height == f32::from(layout.bounds.size.height)
        && c.cell_w == f32::from(layout.cell_w)
        && c.line_h == f32::from(layout.line_h)
        && c.ghost == layout.ghost_text
}

fn replay_shaped_paint(c: &ShapedPaintCache, window: &mut Window, cx: &mut App) {
    let bounds = Bounds {
        origin: point(px(c.origin_x), px(c.origin_y)),
        size: gpui::size(px(c.width), px(c.height)),
    };
    window.paint_quad(fill(bounds, term_default_bg()));
    for &(x, y, w, h, color) in &c.rects {
        window.paint_quad(fill(
            Bounds {
                origin: point(px(x), px(y)),
                size: gpui::size(px(w), px(h)),
            },
            color,
        ));
    }
    let line_h = px(c.line_h);
    for (x, y, shaped) in &c.texts {
        let _ = shaped.paint(
            point(px(*x), px(*y)),
            line_h,
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
    }
    let (cx0, cy0, cw, ch) = c.cursor;
    window.paint_quad(fill(
        Bounds {
            origin: point(px(cx0), px(cy0)),
            size: gpui::size(px(cw), px(ch)),
        },
        SeancePalette::flame().opacity(0.85),
    ));
    if let Some((x, y, shaped)) = &c.ghost_shaped {
        let _ = shaped.paint(
            point(px(*x), px(*y)),
            line_h,
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

struct BgRect {
    row: usize,
    start_col: usize,
    width_cells: usize,
    color: Hsla,
}

struct TextBatch {
    row: usize,
    start_col: usize,
    text: String,
    style: Style,
}

/// Measure mono cell size once per process (font is fixed for the app).
fn cell_metrics(window: &mut Window) -> (Pixels, Pixels) {
    use std::sync::OnceLock;
    static CACHED: OnceLock<(f32, f32)> = OnceLock::new();
    if let Some(&(w, h)) = CACHED.get() {
        return (px(w), px(h));
    }
    let probe = window.text_system().shape_line(
        SharedString::from("█"),
        px(FONT_SIZE),
        &[TextRun {
            len: '█'.len_utf8(),
            font: term_font(),
            color: term_default_fg(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    let w = f32::from(probe.width);
    let h = FONT_SIZE * LINE_HEIGHT_FACTOR;
    let _ = CACHED.set((w, h));
    (px(w), px(h))
}

fn term_default_fg() -> Hsla {
    // ghostty foreground = #d8d8d8
    gpui::Rgba {
        r: 0xd8 as f32 / 255.,
        g: 0xd8 as f32 / 255.,
        b: 0xd8 as f32 / 255.,
        a: 1.,
    }
    .into()
}
fn term_default_bg() -> Hsla {
    // ghostty background = #181818
    gpui::Rgba {
        r: 0x18 as f32 / 255.,
        g: 0x18 as f32 / 255.,
        b: 0x18 as f32 / 255.,
        a: 1.,
    }
    .into()
}

fn paint_grid(layout: &Layout, window: &mut Window, cx: &mut App) {
    // Replay path: same grid + bounds as last paint → skip reshape (sidebar DnD).
    if let Ok(guard) = shaped_paint_caches().lock() {
        if let Some(c) = guard.get(&layout.slug) {
            if cache_matches(c, layout) {
                let c = c.clone();
                drop(guard);
                replay_shaped_paint(&c, window, cx);
                return;
            }
        }
    }

    let origin = layout.bounds.origin;
    let bg = term_default_bg();
    window.paint_quad(fill(layout.bounds, bg));

    let cols = layout.snap.cols as usize;
    let rows = layout.snap.rows as usize;
    if cols == 0 || rows == 0 || layout.snap.cells.is_empty() {
        return;
    }

    // Build batched bg rects + text runs (same-style contiguous cells).
    // Was: shape_line per cell → ~cols*rows shapes/frame and 80–90% CPU idle.
    let mut rects: Vec<BgRect> = Vec::new();
    let mut batches: Vec<TextBatch> = Vec::new();
    let mut open: Option<TextBatch> = None;

    let flush = |open: &mut Option<TextBatch>, batches: &mut Vec<TextBatch>| {
        if let Some(b) = open.take() {
            if !b.text.is_empty() {
                batches.push(b);
            }
        }
    };

    let cursor_row = layout.snap.cursor_row as usize;
    for row in 0..rows {
        flush(&mut open, &mut batches);
        // Skip fully blank rows (common empty space below prompt) unless the
        // cursor lives there — still need the caret paint path.
        if row != cursor_row {
            let base = row * cols;
            let end = (base + cols).min(layout.snap.cells.len());
            if base < end
                && layout.snap.cells[base..end]
                    .iter()
                    .all(|c| (c.c == ' ' || c.c == '\0') && c.bg == 0xFFFF_FFFF)
            {
                continue;
            }
        }
        for col in 0..cols {
            let idx = row * cols + col;
            if idx >= layout.snap.cells.len() {
                break;
            }
            let cell = &layout.snap.cells[idx];
            let style = cell_style(cell);

            if let Some(bgc) = style.bg {
                match rects.last_mut() {
                    Some(last)
                        if last.row == row
                            && last.color == bgc
                            && last.start_col + last.width_cells == col =>
                    {
                        last.width_cells += 1;
                    }
                    _ => rects.push(BgRect {
                        row,
                        start_col: col,
                        width_cells: 1,
                        color: bgc,
                    }),
                }
            }

            let ch = cell.c;
            let blank = ch == ' ' || ch == '\0';
            if blank {
                // Spaces break text runs (cheaper; bg already handled).
                flush(&mut open, &mut batches);
                continue;
            }

            let continues = open.as_ref().is_some_and(|b| {
                b.row == row
                    && b.style == style
                    && b.start_col + b.text.chars().count() == col
            });
            if continues {
                open.as_mut().unwrap().text.push(ch);
            } else {
                flush(&mut open, &mut batches);
                open = Some(TextBatch {
                    row,
                    start_col: col,
                    text: ch.to_string(),
                    style,
                });
            }
        }
    }
    flush(&mut open, &mut batches);

    // Paint background runs + collect absolute geometry for the cache.
    let mut cache_rects: Vec<(f32, f32, f32, f32, Hsla)> = Vec::with_capacity(rects.len());
    for r in &rects {
        let rect = Bounds {
            origin: point(
                origin.x + layout.cell_w * r.start_col as f32,
                origin.y + layout.line_h * r.row as f32,
            ),
            size: gpui::size(layout.cell_w * r.width_cells as f32, layout.line_h),
        };
        cache_rects.push((
            f32::from(rect.origin.x),
            f32::from(rect.origin.y),
            f32::from(rect.size.width),
            f32::from(rect.size.height),
            r.color,
        ));
        window.paint_quad(fill(rect, r.color));
    }

    // Shape + paint text runs. force_width = cell width snaps glyphs to the
    // grid (box-drawing / logo stay column-aligned without per-cell shaping).
    let font_size = px(FONT_SIZE);
    let mut cache_texts: Vec<(f32, f32, ShapedLine)> = Vec::with_capacity(batches.len());
    for b in &batches {
        let font = if b.style.bold {
            term_font_bold()
        } else {
            term_font()
        };
        let shaped: ShapedLine = window.text_system().shape_line(
            SharedString::from(b.text.clone()),
            font_size,
            &[TextRun {
                len: b.text.len(),
                font,
                color: b.style.fg,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            Some(layout.cell_w),
        );
        let pos = point(
            origin.x + layout.cell_w * b.start_col as f32,
            origin.y + layout.line_h * b.row as f32,
        );
        cache_texts.push((f32::from(pos.x), f32::from(pos.y), shaped.clone()));
        let _ = shaped.paint(pos, layout.line_h, gpui::TextAlign::Left, None, window, cx);
    }

    // Cursor.
    let cc = layout.snap.cursor_col as f32;
    let cr = layout.snap.cursor_row as f32;
    let cursor_bounds = Bounds {
        origin: point(origin.x + layout.cell_w * cc, origin.y + layout.line_h * cr),
        size: gpui::size(layout.cell_w, layout.line_h),
    };
    window.paint_quad(fill(cursor_bounds, SeancePalette::flame().opacity(0.85)));

    let mut ghost_shaped = None;
    if let Some(ref g) = layout.ghost_text {
        let shaped = window.text_system().shape_line(
            SharedString::from(g.clone()),
            font_size,
            &[TextRun {
                len: g.len(),
                font: term_font(),
                color: SeancePalette::violet().opacity(0.7),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let pos = point(
            origin.x + layout.cell_w * (cc + 1.0),
            origin.y + layout.line_h * cr,
        );
        ghost_shaped = Some((f32::from(pos.x), f32::from(pos.y), shaped.clone()));
        let _ = shaped.paint(pos, layout.line_h, gpui::TextAlign::Left, None, window, cx);
    }

    if let Ok(mut guard) = shaped_paint_caches().lock() {
        guard.insert(
            layout.slug.clone(),
            ShapedPaintCache {
                rev: layout.snap.rev,
                origin_x: f32::from(origin.x),
                origin_y: f32::from(origin.y),
                width: f32::from(layout.bounds.size.width),
                height: f32::from(layout.bounds.size.height),
                cell_w: f32::from(layout.cell_w),
                line_h: f32::from(layout.line_h),
                ghost: layout.ghost_text.clone(),
                rects: cache_rects,
                texts: cache_texts,
                cursor: (
                    f32::from(cursor_bounds.origin.x),
                    f32::from(cursor_bounds.origin.y),
                    f32::from(cursor_bounds.size.width),
                    f32::from(cursor_bounds.size.height),
                ),
                ghost_shaped,
            },
        );
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Style {
    fg: Hsla,
    bg: Option<Hsla>,
    bold: bool,
}

fn cell_style(c: &CellSnap) -> Style {
    let mut fg = u32_to_hsla(c.fg).unwrap_or_else(term_default_fg);
    let mut bg = u32_to_hsla(c.bg);
    if c.inverse {
        std::mem::swap(&mut fg, &mut bg.get_or_insert_with(term_default_bg));
    }
    if c.dim {
        fg = fg.opacity(0.65);
    }
    Style {
        fg,
        bg,
        bold: c.bold,
    }
}

fn u32_to_hsla(v: u32) -> Option<Hsla> {
    if v == 0xFFFF_FFFF {
        return None;
    }
    let r = ((v >> 16) & 0xff) as f32 / 255.;
    let g = ((v >> 8) & 0xff) as f32 / 255.;
    let b = (v & 0xff) as f32 / 255.;
    Some(gpui::Rgba { r, g, b, a: 1. }.into())
}

// silence unused import if FONT_FAMILY only re-exported
#[allow(unused_imports)]
use term_font::FONT_FAMILY as _;
