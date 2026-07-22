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
    KeyDownEvent, Pixels, Point, ScrollWheelEvent, ShapedLine, SharedString, TextRun, Window,
};
use gpui_component::{notification::Notification, WindowExt as _};

use crate::remote_term::RemoteTerminal;
use crate::runtime::snapshot::{CellSnap, GridSnapshot};
use crate::term_font::{self, term_font, term_font_bold, FONT_SIZE, LINE_HEIGHT_FACTOR};
use crate::term_shared::keystroke_bytes;
use crate::theme::SeancePalette;
use alacritty_terminal::term::TermMode;

/// Visible-grid cell coordinate (0-based).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellPos {
    row: u16,
    col: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectKind {
    Simple,
    Word,
    Lines,
}

/// Linear selection over the visible snapshot grid (ghostty-style).
#[derive(Clone, Debug)]
struct TermSelection {
    /// Click anchor (unexpanded).
    anchor: CellPos,
    /// Drag end (unexpanded).
    cursor: CellPos,
    kind: SelectKind,
}

impl TermSelection {
    fn range(&self, cols: u16, rows: u16) -> (CellPos, CellPos) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut a = self.anchor;
        let mut b = self.cursor;
        a.row = a.row.min(rows - 1);
        b.row = b.row.min(rows - 1);
        a.col = a.col.min(cols - 1);
        b.col = b.col.min(cols - 1);
        match self.kind {
            SelectKind::Simple => {
                let ia = a.row as u32 * cols as u32 + a.col as u32;
                let ib = b.row as u32 * cols as u32 + b.col as u32;
                if ia <= ib {
                    (a, b)
                } else {
                    (b, a)
                }
            }
            SelectKind::Lines => {
                let (r0, r1) = if a.row <= b.row {
                    (a.row, b.row)
                } else {
                    (b.row, a.row)
                };
                (
                    CellPos { row: r0, col: 0 },
                    CellPos {
                        row: r1,
                        col: cols - 1,
                    },
                )
            }
            SelectKind::Word => {
                // Word expansion happens at start/update against the grid.
                let ia = a.row as u32 * cols as u32 + a.col as u32;
                let ib = b.row as u32 * cols as u32 + b.col as u32;
                if ia <= ib {
                    (a, b)
                } else {
                    (b, a)
                }
            }
        }
    }

    fn contains(&self, row: usize, col: usize, cols: u16, rows: u16) -> bool {
        if cols == 0 || rows == 0 {
            return false;
        }
        let (lo, hi) = self.range(cols, rows);
        let i = row as u32 * cols as u32 + col as u32;
        let a = lo.row as u32 * cols as u32 + lo.col as u32;
        let b = hi.row as u32 * cols as u32 + hi.col as u32;
        i >= a && i <= b
    }
}

/// Layout metrics from last canvas prepaint — mouse → cell mapping.
#[derive(Clone, Copy, Default)]
struct ViewMetrics {
    origin: Point<Pixels>,
    cell_w: f32,
    line_h: f32,
    cols: u16,
    rows: u16,
}

pub struct RemoteTerminalView {
    pub terminal: gpui::Entity<RemoteTerminal>,
    focus_handle: FocusHandle,
    scroll_accum: f32,
    /// Drag-select in progress.
    selecting: bool,
    selection: Option<TermSelection>,
}

impl RemoteTerminalView {
    pub fn new(terminal: gpui::Entity<RemoteTerminal>, cx: &mut Context<Self>) -> Self {
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        Self {
            terminal,
            focus_handle: cx.focus_handle(),
            scroll_accum: 0.,
            selecting: false,
            selection: None,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn metrics(&self, cx: &App) -> Option<ViewMetrics> {
        let slug = &self.terminal.read(cx).slug;
        load_metrics(slug)
    }

    fn cell_at(&self, pos: Point<Pixels>, cx: &App) -> Option<CellPos> {
        let m = self.metrics(cx)?;
        if m.cell_w <= 0. || m.line_h <= 0. || m.cols == 0 || m.rows == 0 {
            return None;
        }
        let rel_x = f32::from(pos.x - m.origin.x);
        let rel_y = f32::from(pos.y - m.origin.y);
        let col = (rel_x / m.cell_w).floor() as i32;
        let row = (rel_y / m.line_h).floor() as i32;
        let col = col.clamp(0, m.cols as i32 - 1) as u16;
        let row = row.clamp(0, m.rows as i32 - 1) as u16;
        Some(CellPos { row, col })
    }

    fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            // caller notifies
        }
        self.selecting = false;
    }

    fn selection_text(&self, cx: &App) -> Option<String> {
        let sel = self.selection.as_ref()?;
        let snap = &self.terminal.read(cx).snapshot;
        let cols = snap.cols;
        let rows = snap.rows;
        if cols == 0 || rows == 0 || snap.cells.is_empty() {
            return None;
        }
        let (lo, hi) = sel.range(cols, rows);
        let mut out = String::new();
        for row in lo.row..=hi.row {
            let start_col = if row == lo.row { lo.col } else { 0 };
            let end_col = if row == hi.row {
                hi.col
            } else {
                cols.saturating_sub(1)
            };
            let mut line = String::new();
            for col in start_col..=end_col {
                let idx = row as usize * cols as usize + col as usize;
                if idx < snap.cells.len() {
                    let c = snap.cells[idx].c;
                    line.push(if c == '\0' { ' ' } else { c });
                }
            }
            // Trim trailing spaces per line (ghostty / xterm convention).
            let trimmed = line.trim_end();
            if row > lo.row {
                out.push('\n');
            }
            out.push_str(trimmed);
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn copy_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_text(cx) {
            if !text.is_empty() {
                let n = text.chars().count();
                let lines = text.lines().count().max(1);
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                let msg = if lines > 1 {
                    format!("copied · {n} chars · {lines} lines")
                } else {
                    format!("copied · {n} chars")
                };
                window.push_notification(Notification::success(msg), cx);
            }
        }
    }

    fn expand_word(snap: &GridSnapshot, pos: CellPos) -> (CellPos, CellPos) {
        let cols = snap.cols as usize;
        let rows = snap.rows as usize;
        if cols == 0 || rows == 0 || snap.cells.is_empty() {
            return (pos, pos);
        }
        let row = pos.row as usize;
        if row >= rows {
            return (pos, pos);
        }
        let cell = |c: usize| -> char {
            let idx = row * cols + c;
            if idx < snap.cells.len() {
                let ch = snap.cells[idx].c;
                if ch == '\0' {
                    ' '
                } else {
                    ch
                }
            } else {
                ' '
            }
        };
        let is_word =
            |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':');
        let col = (pos.col as usize).min(cols.saturating_sub(1));
        let ch0 = cell(col);
        if !is_word(ch0) {
            return (pos, pos);
        }
        let mut left = col;
        while left > 0 && is_word(cell(left - 1)) {
            left -= 1;
        }
        let mut right = col;
        while right + 1 < cols && is_word(cell(right + 1)) {
            right += 1;
        }
        (
            CellPos {
                row: pos.row,
                col: left as u16,
            },
            CellPos {
                row: pos.row,
                col: right as u16,
            },
        )
    }

    fn start_selection_at(&mut self, pos: CellPos, kind: SelectKind, cx: &App) {
        let snap = &self.terminal.read(cx).snapshot;
        let (anchor, cursor, kind) = match kind {
            SelectKind::Simple => (pos, pos, SelectKind::Simple),
            // Expand to word immediately and store as Simple so range() is correct.
            SelectKind::Word => {
                let (a, b) = Self::expand_word(snap, pos);
                (a, b, SelectKind::Simple)
            }
            SelectKind::Lines => (
                CellPos {
                    row: pos.row,
                    col: 0,
                },
                CellPos {
                    row: pos.row,
                    col: snap.cols.saturating_sub(1),
                },
                SelectKind::Lines,
            ),
        };
        self.selection = Some(TermSelection {
            anchor,
            cursor,
            kind,
        });
        self.selecting = true;
    }

    fn update_selection_at(&mut self, pos: CellPos, cx: &App) {
        let Some(sel) = self.selection.as_mut() else {
            return;
        };
        match sel.kind {
            SelectKind::Simple | SelectKind::Word => {
                sel.cursor = pos;
            }
            SelectKind::Lines => {
                let cols = self.terminal.read(cx).snapshot.cols.saturating_sub(1);
                sel.cursor = CellPos {
                    row: pos.row,
                    col: cols,
                };
            }
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
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

        // Terminal paste/copy before other ctrl+shift app chords.
        if ks.modifiers.control && ks.modifiers.shift {
            match ks.key.as_str() {
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                        self.clear_selection();
                        self.terminal.update(cx, |t, _| {
                            t.scroll_to_bottom();
                            t.paste(&text);
                        });
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                "c" => {
                    self.copy_selection(window, cx);
                    cx.stop_propagation();
                    return;
                }
                // Other chords bubble to seance chrome.
                _ => return,
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

        let mut mode = TermMode::empty();
        if term.snapshot.app_cursor {
            mode.insert(TermMode::APP_CURSOR);
        }
        if let Some(bytes) = keystroke_bytes(&event.keystroke, mode) {
            // Typing clears selection (ghostty convention).
            if self.selection.is_some() {
                self.clear_selection();
            }
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

    fn open_link_at(
        &mut self,
        pos: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (cell_w, line_h) = cell_metrics(window);
        let term = self.terminal.read(cx);
        // Approximate: canvas fills the view; origin is view origin ≈ 0 relative
        // to this element. Use window bounds of focus isn't available — fall
        // back to row/col from local coords when possible via last snapshot size.
        let snap = term.snapshot.clone();
        // Without element-local bounds here, use a coarse approach: search all
        // OSC-8 spans first; if only one on screen open it; else scan URLs on
        // the line nearest the click using pixel→cell from top-left of view.
        // Parent places us at tile origin; MouseDown position is window-global.
        // Best-effort: convert via cell metrics assuming the view's last resize.
        let _ = pos;
        let _ = cell_w;
        let _ = line_h;
        // Prefer single visible hyperlink when only one exists (common for
        // "open this PR" agent output). Otherwise try URL regex on all lines.
        if snap.hyperlinks.len() == 1 {
            open_uri(&snap.hyperlinks[0].uri);
            cx.stop_propagation();
            return;
        }
        if let Some(h) = snap.hyperlinks.first() {
            // Multiple: open the first that looks like http(s).
            if let Some(h) = snap
                .hyperlinks
                .iter()
                .find(|h| h.uri.starts_with("http://") || h.uri.starts_with("https://"))
            {
                open_uri(&h.uri);
                cx.stop_propagation();
                return;
            }
            open_uri(&h.uri);
            cx.stop_propagation();
            return;
        }
        // Fallback: bare URL on screen text.
        if let Some(url) = first_http_url_in_cells(&snap) {
            open_uri(&url);
            cx.stop_propagation();
        }
    }

    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let selection = self.selection.clone();

        div()
            .id("remote-term-view")
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .track_focus(&focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, ev: &gpui::MouseDownEvent, window, cx| {
                    let handle = this.focus_handle.clone();
                    window.focus(&handle, cx);

                    // Ctrl+click: open hyperlink (not selection).
                    if ev.modifiers.control {
                        this.open_link_at(ev.position, window, cx);
                        return;
                    }

                    // Always allow drag-select. We don't forward mouse button
                    // events to the PTY yet (only wheel when mouse_mode), so
                    // blocking selection for mouse-mode apps (Claude, vim,
                    // etc.) just made drag a no-op. When click reporting lands,
                    // restore: mouse_mode && !shift → forward, else select.
                    let Some(cell) = this.cell_at(ev.position, cx) else {
                        return;
                    };
                    let kind = match ev.click_count {
                        2 => SelectKind::Word,
                        n if n >= 3 => SelectKind::Lines,
                        _ => SelectKind::Simple,
                    };
                    if kind == SelectKind::Simple {
                        this.clear_selection();
                    }
                    this.start_selection_at(cell, kind, cx);
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, _window, cx| {
                if !this.selecting || !ev.dragging() {
                    return;
                }
                if let Some(cell) = this.cell_at(ev.position, cx) {
                    this.update_selection_at(cell, cx);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, window, cx| {
                    if this.selecting {
                        this.selecting = false;
                        // Ghostty / primary-selection: copy on release.
                        this.copy_selection(window, cx);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener(|this, ev: &gpui::MouseDownEvent, window, cx| {
                    this.open_link_at(ev.position, window, cx);
                }),
            )
            .child(
                canvas(
                    {
                        let term = term.clone();
                        let selection = selection.clone();
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
                            let input_origin = term.read(cx).last_input_origin.clone();
                            Layout {
                                slug,
                                bounds,
                                cell_w,
                                line_h,
                                font_size: px(FONT_SIZE),
                                snap,
                                ghost_text: ghost.map(|g| g.text),
                                input_origin,
                                selection,
                                origin_gutter: true,
                            }
                        }
                    },
                    |_bounds, layout: Layout, window, cx| {
                        // Persist metrics so mouse handlers can map coords.
                        // Stored via a side channel keyed by slug (view state
                        // isn't reachable from paint closure without Entity).
                        store_metrics(
                            &layout.slug,
                            ViewMetrics {
                                origin: layout.bounds.origin,
                                cell_w: f32::from(layout.cell_w),
                                line_h: f32::from(layout.line_h),
                                cols: layout.snap.cols,
                                rows: layout.snap.rows,
                            },
                        );
                        paint_grid(&layout, window, cx);
                    },
                )
                .size_full(),
            )
    }
}

/// Live terminal thumbnail for the workspace overview — paints the current
/// grid scaled to fit, **never** resizes the PTY.
pub struct OverviewThumb {
    terminal: gpui::Entity<RemoteTerminal>,
}

impl OverviewThumb {
    pub fn new(terminal: gpui::Entity<RemoteTerminal>, cx: &mut Context<Self>) -> Self {
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        Self { terminal }
    }
}

impl Render for OverviewThumb {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let term = self.terminal.clone();
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(term_default_bg())
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let (native_cw, native_lh) = cell_metrics(window);
                        let snap = Arc::clone(&term.read(cx).snapshot);
                        let ghost = term.read(cx).ghost.clone();
                        let slug = term.read(cx).slug.clone();
                        let input_origin = term.read(cx).last_input_origin.clone();

                        let cols = snap.cols.max(1) as f32;
                        let rows = snap.rows.max(1) as f32;
                        let bw = f32::from(bounds.size.width).max(1.);
                        let bh = f32::from(bounds.size.height).max(1.);
                        let ncw = f32::from(native_cw).max(1.);
                        let nlh = f32::from(native_lh).max(1.);
                        let fit = (bw / (cols * ncw)).min(bh / (rows * nlh));

                        // Never shrink past ~readable. If the whole grid won't
                        // fit at that scale, crop (viewport) instead of micro-
                        // text. That's why most cards looked pure black: fit
                        // scale was ~0.1 and glyphs vanished into the bg.
                        // Target ~8px font when crop is required.
                        const MIN_READABLE: f32 = 0.42;
                        let (scale, scroll_x, scroll_y) = if fit >= MIN_READABLE {
                            // Whole grid fits — letterbox centered.
                            let scale = fit.min(1.0);
                            let grid_w = cols * ncw * scale;
                            let grid_h = rows * nlh * scale;
                            (scale, -((bw - grid_w) * 0.5), -((bh - grid_h) * 0.5))
                        } else {
                            let scale = MIN_READABLE;
                            let cell_w = ncw * scale;
                            let line_h = nlh * scale;
                            let grid_w = cols * cell_w;
                            let grid_h = rows * line_h;
                            let max_sx = (grid_w - bw).max(0.0);
                            let max_sy = (grid_h - bh).max(0.0);
                            // Bias viewport toward the cursor so active TUIs
                            // show the interesting region, not blank top.
                            let cx_px = (snap.cursor_col as f32 + 0.5) * cell_w;
                            let cy_px = (snap.cursor_row as f32 + 0.5) * line_h;
                            let sx = (cx_px - bw * 0.45).clamp(0.0, max_sx);
                            let sy = (cy_px - bh * 0.55).clamp(0.0, max_sy);
                            (scale, sx, sy)
                        };

                        let cell_w = px(ncw * scale);
                        let line_h = px(nlh * scale);
                        let grid_w = f32::from(cell_w) * cols;
                        let grid_h = f32::from(line_h) * rows;
                        let paint_bounds = Bounds {
                            origin: point(
                                bounds.origin.x - px(scroll_x),
                                bounds.origin.y - px(scroll_y),
                            ),
                            size: gpui::size(px(grid_w), px(grid_h)),
                        };
                        // Font tracks scale so glyphs stay inside cells.
                        let font_size = px((FONT_SIZE * scale).clamp(6.0, FONT_SIZE));
                        Layout {
                            // Separate cache key from the live full-size view.
                            // Include scale+scroll so crop pans invalidate cache.
                            slug: format!("ov:{slug}:{:.2}:{:.0}:{:.0}", scale, scroll_x, scroll_y),
                            bounds: paint_bounds,
                            cell_w,
                            line_h,
                            font_size,
                            snap,
                            ghost_text: ghost.map(|g| g.text),
                            input_origin,
                            selection: None,
                            origin_gutter: false,
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
    font_size: Pixels,
    snap: Arc<crate::runtime::snapshot::GridSnapshot>,
    ghost_text: Option<String>,
    /// Causal tint: who last wrote stdin.
    input_origin: Option<String>,
    selection: Option<TermSelection>,
    /// Left-edge origin rail (full terminal only — thumbs skip it).
    origin_gutter: bool,
}

/// Metrics side-channel: paint runs outside Entity update, so mouse handlers
/// read last geometry from here. Keyed by pane slug.
fn view_metrics_map() -> &'static Mutex<HashMap<String, ViewMetrics>> {
    static M: OnceLock<Mutex<HashMap<String, ViewMetrics>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn store_metrics(slug: &str, m: ViewMetrics) {
    if let Ok(mut g) = view_metrics_map().lock() {
        g.insert(slug.to_string(), m);
    }
}

fn load_metrics(slug: &str) -> Option<ViewMetrics> {
    view_metrics_map()
        .lock()
        .ok()
        .and_then(|g| g.get(slug).copied())
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
    font_size: f32,
    ghost: Option<String>,
    input_origin: Option<String>,
    /// (start_row, start_col, end_row, end_col) or None.
    selection_key: Option<(u16, u16, u16, u16)>,
    rects: Vec<(f32, f32, f32, f32, Hsla)>,
    texts: Vec<(f32, f32, ShapedLine)>,
    cursor: (f32, f32, f32, f32),
    ghost_shaped: Option<(f32, f32, ShapedLine)>,
}

fn selection_key(
    sel: &Option<TermSelection>,
    cols: u16,
    rows: u16,
) -> Option<(u16, u16, u16, u16)> {
    let sel = sel.as_ref()?;
    let (lo, hi) = sel.range(cols, rows);
    Some((lo.row, lo.col, hi.row, hi.col))
}

fn open_uri(uri: &str) {
    let uri = uri.to_string();
    std::thread::spawn(move || {
        let _ = std::process::Command::new("xdg-open")
            .arg(&uri)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

fn first_http_url_in_cells(snap: &crate::runtime::snapshot::GridSnapshot) -> Option<String> {
    let cols = snap.cols as usize;
    if cols == 0 || snap.cells.is_empty() {
        return None;
    }
    let rows = snap.cells.len() / cols;
    for r in 0..rows {
        let mut line = String::with_capacity(cols);
        for c in 0..cols {
            line.push(snap.cells[r * cols + c].c);
        }
        if let Some(url) = extract_http_url(&line) {
            return Some(url);
        }
    }
    None
}

fn extract_http_url(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 7 < bytes.len() {
        if bytes[i..].starts_with(b"https://") || bytes[i..].starts_with(b"http://") {
            let start = i;
            i += if bytes[i + 4] == b's' { 8 } else { 7 };
            while i < bytes.len() {
                let c = bytes[i] as char;
                if c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c) {
                    i += 1;
                } else {
                    break;
                }
            }
            let url = line[start..i].trim_end_matches(['.', ',', ')', ']', ';', ':']);
            if url.len() > 10 {
                return Some(url.to_string());
            }
        } else {
            i += 1;
        }
    }
    None
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
        && c.font_size == f32::from(layout.font_size)
        && c.ghost == layout.ghost_text
        && c.input_origin == layout.input_origin
        && c.selection_key == selection_key(&layout.selection, layout.snap.cols, layout.snap.rows)
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
                if layout.origin_gutter {
                    paint_origin_gutter(layout, window);
                }
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
    let sel = layout.selection.as_ref();
    let sel_cols = layout.snap.cols;
    let sel_rows = layout.snap.rows;
    let in_sel =
        |row: usize, col: usize| sel.is_some_and(|s| s.contains(row, col, sel_cols, sel_rows));
    let sel_bg = SeancePalette::violet_dim().opacity(0.55);

    for row in 0..rows {
        flush(&mut open, &mut batches);
        // Skip fully blank rows (common empty space below prompt) unless the
        // cursor lives there or selection covers the row.
        let row_selected = sel.is_some_and(|s| {
            let (lo, hi) = s.range(sel_cols, sel_rows);
            row as u16 >= lo.row && row as u16 <= hi.row
        });
        if row != cursor_row && !row_selected {
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
            let mut style = cell_style(cell);
            let selected = in_sel(row, col);
            if selected {
                style.bg = Some(sel_bg);
                // Keep text readable over selection tint.
                style.fg = term_default_fg();
            }

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
                b.row == row && b.style == style && b.start_col + b.text.chars().count() == col
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
    let font_size = layout.font_size;
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

    if layout.origin_gutter {
        paint_origin_gutter(layout, window);
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
                font_size: f32::from(layout.font_size),
                ghost: layout.ghost_text.clone(),
                input_origin: layout.input_origin.clone(),
                selection_key: selection_key(&layout.selection, layout.snap.cols, layout.snap.rows),
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
    paint_origin_gutter(layout, window);
}

/// 2px left gutter tinted by who last wrote stdin — causal attribution made
/// visceral without cell-level byte tracking (which needs a parallel grid).
fn paint_origin_gutter(layout: &Layout, window: &mut Window) {
    let Some(ref origin) = layout.input_origin else {
        return;
    };
    let color = if origin == "human" {
        SeancePalette::text_faint().opacity(0.35)
    } else if origin.starts_with("agent:") || origin == "cli" {
        SeancePalette::violet().opacity(0.75)
    } else if origin == "propose" || origin.contains("propose") {
        SeancePalette::flame().opacity(0.7)
    } else {
        SeancePalette::success().opacity(0.55)
    };
    let gutter = Bounds {
        origin: layout.bounds.origin,
        size: gpui::size(px(2.0), layout.bounds.size.height),
    };
    window.paint_quad(fill(gutter, color));
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
