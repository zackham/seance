//! TerminalView: focusable gpui view rendering a Terminal entity's grid via
//! a canvas element — background rects, style-batched shaped text runs, and
//! a candle-amber cursor.

use alacritty_terminal::term::cell::Flags;
use gpui::{
    canvas, div, fill, font, point, prelude::*, px, App, Bounds, Context, FocusHandle, Focusable,
    FontStyle, FontWeight, Hsla, KeyDownEvent, Pixels, Point, ScrollWheelEvent, ShapedLine,
    SharedString, TextRun, UnderlineStyle, Window,
};

use crate::terminal::{self, convert_color, Terminal};
use crate::theme::SeancePalette;
use gpui_component::{notification::Notification, WindowExt as _};

// Match ghostty (monospace → JetBrainsMono Nerd Font). See term_font.rs.
pub use crate::term_font::{FONT_FAMILY, FONT_SIZE, LINE_HEIGHT_FACTOR};

pub struct TerminalView {
    pub terminal: gpui::Entity<Terminal>,
    focus_handle: FocusHandle,
    /// Fractional wheel-scroll accumulator — sub-line deltas must accumulate
    /// or trackpad/wheel events truncate to zero lines (v0.1 bug).
    scroll_accum: f32,
    selecting: bool,
}

struct FrameLayout {
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    bg: Hsla,
    rects: Vec<BgRect>,
    lines: Vec<PositionedLine>,
    cursor: Option<CursorLayout>,
    ghost: Option<GhostLayout>,
}

struct BgRect {
    line: i32,
    start_col: i32,
    width_cells: i32,
    color: Hsla,
}

struct PositionedLine {
    line: i32,
    start_col: i32,
    shaped: ShapedLine,
}

struct GhostLayout {
    line: i32,
    col: i32,
    shaped: ShapedLine,
    banner: Option<ShapedLine>,
}

struct CursorLayout {
    line: i32,
    col: i32,
    color: Hsla,
    /// Character under the cursor, repainted in bg color for contrast.
    ch: Option<(String, Hsla)>,
    block: bool,
}

impl TerminalView {
    pub fn new(terminal: gpui::Entity<Terminal>, cx: &mut Context<Self>) -> Self {
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        Self {
            terminal,
            focus_handle: cx.focus_handle(),
            scroll_accum: 0.,
            selecting: false,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;

        // Ghost proposal interception: Enter/Tab accepts, Esc rejects, any
        // printable key rejects-and-types.
        if self.terminal.read(cx).ghost.is_some() {
            match ks.key.as_str() {
                "enter" | "tab" => {
                    self.terminal.update(cx, |_, cx| {
                        cx.emit(crate::terminal::TerminalEvent::GhostAccepted);
                    });
                    cx.stop_propagation();
                    return;
                }
                "escape" => {
                    self.terminal.update(cx, |_, cx| {
                        cx.emit(crate::terminal::TerminalEvent::GhostRejected);
                    });
                    cx.stop_propagation();
                    return;
                }
                _ => {
                    if ks.key_char.is_some() {
                        self.terminal.update(cx, |_, cx| {
                            cx.emit(crate::terminal::TerminalEvent::GhostRejected);
                        });
                        // fall through: the key also types normally
                    }
                }
            }
        }

        let mode = self.terminal.read(cx).mode();

        // Shift+PageUp/PageDown: scrollback (terminal convention).
        if ks.modifiers.shift && (ks.key == "pageup" || ks.key == "pagedown") {
            let page = self.terminal.read(cx).bounds.num_lines() as i32 - 2;
            let delta = if ks.key == "pageup" { page } else { -page };
            self.terminal.update(cx, |term, _| term.scroll_lines(delta));
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

        // Any other ctrl+shift chord belongs to the app (summon, drawer, ...)
        // — bubble it instead of eating it as a control byte (v0.1 bug).
        if ks.modifiers.control && ks.modifiers.shift {
            return;
        }

        if let Some(bytes) = terminal::keystroke_bytes(&event.keystroke, mode) {
            self.terminal.update(cx, |term, _| {
                term.scroll_to_bottom();
                term.clear_selection();
                term.write_bytes(bytes);
            });
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

        let mode = self.terminal.read(cx).mode();
        use alacritty_terminal::term::TermMode;
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            // Full-screen apps (vim, htop, less): wheel becomes arrow keys.
            let key = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
            let key = if mode.contains(TermMode::APP_CURSOR) {
                key.to_vec()
            } else if lines > 0 {
                b"\x1b[A".to_vec()
            } else {
                b"\x1b[B".to_vec()
            };
            let mut bytes = Vec::new();
            for _ in 0..lines.unsigned_abs() {
                bytes.extend_from_slice(&key);
            }
            self.terminal.update(cx, |term, _| term.write_bytes(bytes));
        } else {
            self.terminal.update(cx, |term, _| term.scroll_lines(lines));
        }
    }

    fn copy_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.terminal.read(cx).selection_text() {
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

    fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = window;
        if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
            self.terminal.update(cx, |term, _| {
                term.scroll_to_bottom();
                term.paste(&text);
            });
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal = self.terminal.clone();
        let focused = self.focus_handle.clone();

        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Terminal-convention chords; all other ctrl+shift bubble up.
                let ks = &event.keystroke;
                if ks.modifiers.control && ks.modifiers.shift {
                    match ks.key.as_str() {
                        "v" => {
                            this.paste(window, cx);
                            cx.stop_propagation();
                            return;
                        }
                        "c" => {
                            this.copy_selection(window, cx);
                            cx.stop_propagation();
                            return;
                        }
                        _ => {}
                    }
                }
                this.on_key_down(event, window, cx);
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                this.on_scroll(event, window, cx);
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    let handle = this.focus_handle.clone();
                    window.focus(&handle, cx);
                    let kind = match event.click_count {
                        2 => alacritty_terminal::selection::SelectionType::Semantic,
                        n if n >= 3 => alacritty_terminal::selection::SelectionType::Lines,
                        _ => alacritty_terminal::selection::SelectionType::Simple,
                    };
                    let position = event.position;
                    this.selecting = true;
                    this.terminal.update(cx, |term, _| {
                        if event.click_count == 1 {
                            term.clear_selection();
                        }
                        term.start_selection(position, kind);
                    });
                    cx.notify();
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                    if this.selecting && event.dragging() {
                        let position = event.position;
                        this.terminal
                            .update(cx, |term, _| term.update_selection(position));
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _event, window, cx| {
                    this.selecting = false;
                    // Primary-selection convention: copy on select-end.
                    this.copy_selection(window, cx);
                    cx.notify();
                }),
            )
            .child(
                canvas(
                    {
                        let terminal = terminal.clone();
                        let focused = focused.clone();
                        move |bounds, window, cx| {
                            prepaint_terminal(terminal, focused, bounds, window, cx)
                        }
                    },
                    move |_bounds, layout: FrameLayout, window, cx| {
                        paint_terminal(layout, window, cx);
                    },
                )
                .size_full(),
            )
    }
}

fn prepaint_terminal(
    terminal: gpui::Entity<Terminal>,
    focus: FocusHandle,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> FrameLayout {
    let font_size = px(FONT_SIZE);
    let line_height = (font_size * LINE_HEIGHT_FACTOR).round();
    let base_font = crate::term_font::term_font();

    // Measure the advance width of one cell (full block = box-drawing metric).
    let probe_run = TextRun {
        len: '█'.len_utf8(),
        font: base_font.clone(),
        color: SeancePalette::text(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let probe =
        window
            .text_system()
            .shape_line(SharedString::from("█"), font_size, &[probe_run], None);
    let cell_width = probe.width;

    // Resize the PTY grid if the pane geometry changed.
    terminal.update(cx, |term, _| {
        term.resize(terminal::term_bounds(bounds, cell_width, line_height));
    });

    let is_focused = focus.is_focused(window);

    let mut rects: Vec<BgRect> = Vec::new();
    let mut lines: Vec<PositionedLine> = Vec::new();
    let mut cursor = None;

    terminal.read(cx).with_term(|term| {
        let content = term.renderable_content();
        let display_offset = content.display_offset as i32;
        let show_cursor = content.cursor.shape
            != alacritty_terminal::vte::ansi::CursorShape::Hidden
            && display_offset == 0;

        // Batch cells by (line, contiguous run, style).
        struct Batch {
            line: i32,
            start_col: i32,
            text: String,
            runs: Vec<TextRun>,
        }
        let mut batch: Option<Batch> = None;
        let flush = |b: Option<Batch>, lines: &mut Vec<PositionedLine>, window: &mut Window| {
            if let Some(b) = b {
                if !b.text.is_empty() {
                    let shaped = window.text_system().shape_line(
                        SharedString::from(b.text),
                        font_size,
                        &b.runs,
                        None,
                    );
                    lines.push(PositionedLine {
                        line: b.line,
                        start_col: b.start_col,
                        shaped,
                    });
                }
            }
        };

        for indexed in content.display_iter {
            let cell = &indexed.cell;
            let grid_point = indexed.point;
            // display_iter yields grid coords; convert to viewport lines.
            let line = grid_point.line.0 + display_offset;
            let col = grid_point.column.0 as i32;

            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }

            let inverse = cell.flags.contains(Flags::INVERSE);
            let mut fg = convert_color(&cell.fg, cell.flags.contains(Flags::DIM))
                .unwrap_or(SeancePalette::text());
            let mut bg = convert_color(&cell.bg, false);
            if inverse {
                let old_fg = fg;
                fg = bg.unwrap_or(SeancePalette::bg());
                bg = Some(old_fg);
            }
            if content.selection.is_some_and(|s| s.contains(grid_point)) {
                bg = Some(SeancePalette::violet_dim());
                fg = SeancePalette::text();
            }

            if let Some(bg_color) = bg {
                match rects.last_mut() {
                    Some(last)
                        if last.line == line
                            && last.color == bg_color
                            && last.start_col + last.width_cells == col =>
                    {
                        last.width_cells += 1;
                    }
                    _ => rects.push(BgRect {
                        line,
                        start_col: col,
                        width_cells: 1,
                        color: bg_color,
                    }),
                }
            }

            let ch = cell.c;
            let is_blank = ch == ' ' && cell.zerowidth().is_none();

            let weight = if cell.flags.contains(Flags::BOLD) {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            };
            let style = if cell.flags.contains(Flags::ITALIC) {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            };
            let underline = cell
                .flags
                .intersects(Flags::UNDERLINE | Flags::DOUBLE_UNDERLINE | Flags::UNDERCURL)
                .then(|| UnderlineStyle {
                    thickness: px(1.),
                    color: Some(fg),
                    wavy: cell.flags.contains(Flags::UNDERCURL),
                });
            let strikethrough =
                cell.flags
                    .contains(Flags::STRIKEOUT)
                    .then(|| gpui::StrikethroughStyle {
                        thickness: px(1.),
                        color: Some(fg),
                    });

            let mut cell_font = base_font.clone();
            cell_font.weight = weight;
            cell_font.style = style;

            let same_style = |run: &TextRun| {
                run.color == fg
                    && run.font.weight == weight
                    && run.font.style == style
                    && run.underline.as_ref().map(|u| u.wavy) == underline.as_ref().map(|u| u.wavy)
                    && run.underline.is_some() == underline.is_some()
                    && run.strikethrough.is_some() == strikethrough.is_some()
            };

            let continues = batch.as_ref().is_some_and(|b| {
                b.line == line && b.start_col + b.text.chars().count() as i32 == col
            });

            if continues && !is_blank {
                let b = batch.as_mut().unwrap();
                if b.runs.last().is_some_and(&same_style) {
                    b.runs.last_mut().unwrap().len += ch.len_utf8();
                } else {
                    b.runs.push(TextRun {
                        len: ch.len_utf8(),
                        font: cell_font,
                        color: fg,
                        background_color: None,
                        underline,
                        strikethrough,
                    });
                }
                b.text.push(ch);
                if let Some(zw) = cell.zerowidth() {
                    for z in zw {
                        b.text.push(*z);
                        b.runs.last_mut().unwrap().len += z.len_utf8();
                    }
                }
            } else if !is_blank {
                flush(batch.take(), &mut lines, window);
                let mut text = String::new();
                text.push(ch);
                let mut run_len = ch.len_utf8();
                if let Some(zw) = cell.zerowidth() {
                    for z in zw {
                        text.push(*z);
                        run_len += z.len_utf8();
                    }
                }
                batch = Some(Batch {
                    line,
                    start_col: col,
                    text,
                    runs: vec![TextRun {
                        len: run_len,
                        font: cell_font,
                        color: fg,
                        background_color: None,
                        underline,
                        strikethrough,
                    }],
                });
            } else if batch.as_ref().is_some_and(|b| b.line != line) {
                flush(batch.take(), &mut lines, window);
            } else if continues {
                // Blank cell inside a batch: keep run continuity cheaply by
                // flushing — batches restart after whitespace gaps.
                flush(batch.take(), &mut lines, window);
            }

            // Cursor?
            if show_cursor && grid_point == content.cursor.point {
                let block = matches!(
                    content.cursor.shape,
                    alacritty_terminal::vte::ansi::CursorShape::Block
                ) && is_focused;
                cursor = Some(CursorLayout {
                    line,
                    col,
                    color: SeancePalette::flame(),
                    ch: (!is_blank && block).then(|| (ch.to_string(), SeancePalette::bg())),
                    block,
                });
            }
        }
        flush(batch.take(), &mut lines, window);
    });

    // Ghost proposal: dimmed italic text at the cursor + a small banner above.
    let ghost = terminal.read(cx).ghost.clone().and_then(|g| {
        let cur = cursor.as_ref()?;
        let mut base = font(FONT_FAMILY);
        base.style = FontStyle::Italic;
        let first_line = g.text.lines().next().unwrap_or("").to_string();
        let display = if g.text.lines().count() > 1 {
            format!("{first_line} ⏎…")
        } else {
            first_line
        };
        let mut color = SeancePalette::violet();
        color.a = 0.75;
        let run = TextRun {
            len: display.len(),
            font: base.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped =
            window
                .text_system()
                .shape_line(SharedString::from(display), font_size, &[run], None);
        let banner_text = format!(
            "💭 {} proposes{} — enter/tab run · esc dismiss · type to override",
            g.from,
            g.reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        );
        let mut banner_color = SeancePalette::violet_dim();
        banner_color.a = 0.9;
        let banner_run = TextRun {
            len: banner_text.len(),
            font: font(FONT_FAMILY),
            color: banner_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let banner = window.text_system().shape_line(
            SharedString::from(banner_text),
            px(FONT_SIZE - 2.),
            &[banner_run],
            None,
        );
        Some(GhostLayout {
            line: cur.line,
            col: cur.col,
            shaped,
            banner: Some(banner),
        })
    });

    FrameLayout {
        origin: bounds.origin,
        cell_width,
        line_height,
        bg: SeancePalette::bg(),
        rects,
        lines,
        cursor,
        ghost,
    }
}

fn paint_terminal(layout: FrameLayout, window: &mut Window, cx: &mut App) {
    let origin = layout.origin;
    let cw = layout.cell_width;
    let lh = layout.line_height;

    for rect in &layout.rects {
        let pos = point(
            origin.x + cw * rect.start_col as f32,
            origin.y + lh * rect.line as f32,
        );
        window.paint_quad(fill(
            Bounds::new(pos, gpui::size(cw * rect.width_cells as f32, lh)),
            rect.color,
        ));
    }

    // Cursor block behind text.
    if let Some(cursor) = &layout.cursor {
        let pos = point(
            origin.x + cw * cursor.col as f32,
            origin.y + lh * cursor.line as f32,
        );
        if cursor.block {
            window.paint_quad(fill(Bounds::new(pos, gpui::size(cw, lh)), cursor.color));
        } else {
            // Unfocused or beam: hollow-ish underline bar.
            window.paint_quad(fill(
                Bounds::new(point(pos.x, pos.y + lh - px(2.)), gpui::size(cw, px(2.))),
                cursor.color,
            ));
        }
    }

    for line in &layout.lines {
        let pos = point(
            origin.x + cw * line.start_col as f32,
            origin.y + lh * line.line as f32,
        );
        let _ = line
            .shaped
            .paint(pos, lh, gpui::TextAlign::Left, None, window, cx);
    }

    // Repaint the character under a block cursor in bg color for contrast.
    if let Some(cursor) = &layout.cursor {
        if let Some((ch, color)) = &cursor.ch {
            let run = TextRun {
                len: ch.len(),
                font: font(FONT_FAMILY),
                color: *color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(ch.clone()),
                px(FONT_SIZE),
                &[run],
                None,
            );
            let pos = point(
                origin.x + cw * cursor.col as f32,
                origin.y + lh * cursor.line as f32,
            );
            let _ = shaped.paint(pos, lh, gpui::TextAlign::Left, None, window, cx);
        }
    }

    if let Some(ghost) = &layout.ghost {
        let pos = point(
            origin.x + cw * (ghost.col as f32 + 1.0),
            origin.y + lh * ghost.line as f32,
        );
        let _ = ghost
            .shaped
            .paint(pos, lh, gpui::TextAlign::Left, None, window, cx);
        if let Some(banner) = &ghost.banner {
            let bpos = point(
                origin.x + cw,
                origin.y + lh * (ghost.line as f32 - 1.0).max(0.),
            );
            let _ = banner.paint(bpos, lh, gpui::TextAlign::Left, None, window, cx);
        }
    }

    let _ = layout.bg;
}
