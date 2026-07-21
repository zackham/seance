//! Terminal session core: alacritty_terminal wiring behind a gpui Entity.
//!
//! Architecture mirrors zed's crates/terminal (Apache-2.0 alacritty fork API,
//! original implementation): a `Term` grid behind a FairMutex, an alacritty
//! EventLoop pumping the PTY on its own thread, and a gpui task moving
//! events from the listener channel into this entity.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use alacritty_terminal::{
    event::{Event as AlacEvent, EventListener, Notify, WindowSize},
    event_loop::{EventLoop, Msg, Notifier},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{cell::Flags, Config, Term, TermMode},
    tty,
    vte::ansi::{Color as AnsiColor, NamedColor, Rgb as AlacRgb},
};
use anyhow::{Context as _, Result};
use futures::{channel::mpsc::UnboundedSender, StreamExt};
use gpui::{px, Bounds, Context, EventEmitter, Pixels, Task};

use crate::theme;

/// Grid geometry in both pixels and cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TermBounds {
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub bounds: Bounds<Pixels>,
}

impl Default for TermBounds {
    fn default() -> Self {
        TermBounds {
            cell_width: px(8.),
            line_height: px(18.),
            bounds: Bounds {
                origin: gpui::point(px(0.), px(0.)),
                size: gpui::size(px(640.), px(400.)),
            },
        }
    }
}

impl TermBounds {
    pub fn num_lines(&self) -> usize {
        ((self.bounds.size.height / self.line_height) as usize).max(2)
    }

    pub fn num_columns(&self) -> usize {
        ((self.bounds.size.width / self.cell_width) as usize).max(2)
    }

    fn window_size(&self) -> WindowSize {
        WindowSize {
            num_lines: self.num_lines() as u16,
            num_cols: self.num_columns() as u16,
            cell_width: f32::from(self.cell_width) as u16,
            cell_height: f32::from(self.line_height) as u16,
        }
    }
}

impl Dimensions for TermBounds {
    fn total_lines(&self) -> usize {
        self.num_lines()
    }

    fn screen_lines(&self) -> usize {
        self.num_lines()
    }

    fn columns(&self) -> usize {
        self.num_columns()
    }
}

#[derive(Clone)]
pub struct Listener(UnboundedSender<AlacEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: AlacEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

/// Events this entity emits to the session manager / views.
#[derive(Clone, Debug)]
pub enum TerminalEvent {
    Wakeup,
    TitleChanged(String),
    Bell,
    Exited(Option<i32>),
    /// The human accepted the pending ghost proposal (Enter/Tab).
    GhostAccepted,
    /// The human rejected it (Esc, or typed over it).
    GhostRejected,
}

/// An agent-proposed command rendered as ghost text at the prompt.
#[derive(Clone, Debug)]
pub struct Ghost {
    pub id: String,
    pub text: String,
    pub from: String,
    pub reason: Option<String>,
}

pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    pub bounds: TermBounds,
    pub title: Option<String>,
    pub exited: Option<Option<i32>>,
    /// Pending agent proposal, rendered as ghost text; resolved by the human.
    pub ghost: Option<Ghost>,
    /// Rev bumped on every wakeup; lets views skip re-shaping unchanged frames.
    pub rev: u64,
    _io_task: Task<()>,
}

pub struct SpawnConfig {
    pub command: String,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
}

const SCROLL_HISTORY_LINES: usize = 10_000;

/// A successfully opened PTY + term, not yet wired to a gpui entity.
/// Splitting open from entity construction keeps spawn failures fallible
/// (a bad cwd from the control plane must not panic the app).
pub struct OpenTerminal {
    term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    events_rx: futures::channel::mpsc::UnboundedReceiver<AlacEvent>,
}

pub fn open_terminal(config: SpawnConfig) -> Result<OpenTerminal> {
    let (events_tx, events_rx) = futures::channel::mpsc::unbounded();

    let term_config = Config {
        scrolling_history: SCROLL_HISTORY_LINES,
        ..Config::default()
    };

    let bounds = TermBounds::default();
    let term = Term::new(term_config, &bounds, Listener(events_tx.clone()));
    let term = Arc::new(FairMutex::new(term));

    let mut env = config.env;
    env.entry("TERM".into()).or_insert("xterm-256color".into());
    env.entry("COLORTERM".into()).or_insert("truecolor".into());

    // Login shell so PATH picks up ~/.local/bin, fnm, etc. — agent CLIs
    // live there. `exec` replaces the shell with the target command.
    let shell = tty::Shell::new(
        "/bin/bash".to_string(),
        vec!["-lc".to_string(), format!("exec {}", config.command)],
    );

    let cwd = if config.cwd.is_dir() {
        config.cwd
    } else {
        std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
    };

    let tty_options = tty::Options {
        shell: Some(shell),
        working_directory: Some(cwd),
        drain_on_exit: true,
        env,
        child_signal_mask: None,
    };

    let pty = tty::new(&tty_options, bounds.window_size(), 0).context("failed to open pty")?;

    let event_loop = EventLoop::new(
        Arc::clone(&term),
        Listener(events_tx),
        pty,
        true,  // drain_on_exit
        false, // ref_test
    )
    .context("failed to start terminal event loop")?;

    let pty_tx = event_loop.channel();
    let _io_thread = event_loop.spawn();

    Ok(OpenTerminal {
        term,
        notifier: Notifier(pty_tx),
        events_rx,
    })
}

impl Terminal {
    pub fn new(open: OpenTerminal, cx: &mut Context<Self>) -> Self {
        let OpenTerminal {
            term,
            notifier,
            mut events_rx,
        } = open;

        // Pump alacritty events into this entity on the foreground executor.
        let io_task = cx.spawn(async move |this, cx| {
            while let Some(event) = events_rx.next().await {
                let Some(this) = this.upgrade() else { break };
                this.update(cx, |terminal: &mut Terminal, cx| {
                    terminal.process_event(event, cx);
                });
            }
        });

        Terminal {
            term,
            notifier,
            bounds: TermBounds::default(),
            title: None,
            exited: None,
            ghost: None,
            rev: 0,
            _io_task: io_task,
        }
    }

    fn process_event(&mut self, event: AlacEvent, cx: &mut Context<Self>) {
        match event {
            AlacEvent::Wakeup => {
                self.rev += 1;
                cx.emit(TerminalEvent::Wakeup);
                cx.notify();
            }
            AlacEvent::PtyWrite(out) => {
                if std::env::var("SEANCE_DEBUG_IO").is_ok() {
                    eprintln!("[term-reply] {:?}", out);
                }
                self.write_bytes(out.into_bytes());
            }
            AlacEvent::Title(title) => {
                self.title = Some(title.clone());
                cx.emit(TerminalEvent::TitleChanged(title));
                cx.notify();
            }
            AlacEvent::ResetTitle => {
                self.title = None;
                cx.notify();
            }
            AlacEvent::ColorRequest(index, formatter) => {
                let color = color_for_index(index);
                self.write_bytes(formatter(color).into_bytes());
            }
            AlacEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            }
            AlacEvent::ClipboardLoad(_, formatter) => {
                let text = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap_or_default();
                self.write_bytes(formatter(&text).into_bytes());
            }
            AlacEvent::Bell => {
                cx.emit(TerminalEvent::Bell);
            }
            AlacEvent::ChildExit(status) => {
                let code = status.code();
                self.exited = Some(code);
                cx.emit(TerminalEvent::Exited(code));
                cx.notify();
            }
            AlacEvent::Exit => {
                if self.exited.is_none() {
                    self.exited = Some(None);
                    cx.emit(TerminalEvent::Exited(None));
                    cx.notify();
                }
            }
            AlacEvent::MouseCursorDirty
            | AlacEvent::CursorBlinkingChange
            | AlacEvent::TextAreaSizeRequest(_) => {}
        }
    }

    pub fn is_running(&self) -> bool {
        self.exited.is_none()
    }

    pub fn write_bytes(&self, bytes: Vec<u8>) {
        if std::env::var("SEANCE_DEBUG_IO").is_ok() {
            eprintln!("[io->pty] {:?}", String::from_utf8_lossy(&bytes));
        }
        self.notifier.notify(bytes);
    }

    pub fn write_str(&self, s: &str) {
        self.write_bytes(s.as_bytes().to_vec());
    }

    /// Paste text; wraps in bracketed-paste markers when the app requested it.
    pub fn paste(&self, text: &str) {
        let bracketed = {
            let term = self.term.lock();
            term.mode().contains(TermMode::BRACKETED_PASTE)
        };
        if bracketed {
            let mut bytes = Vec::with_capacity(text.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.replace('\x1b', "").as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            self.write_bytes(bytes);
        } else {
            self.write_str(&text.replace("\r\n", "\r").replace('\n', "\r"));
        }
    }

    /// Inject text like a paste, optionally submitting with CR after a settle
    /// delay (decap-style prompt injection for TUI agents).
    pub fn inject(&self, text: String, submit: bool, cx: &mut Context<Self>) {
        self.paste(&text);
        if submit {
            let notifier = Notifier(self.notifier.0.clone());
            cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                notifier.notify(b"\r".to_vec());
            })
            .detach();
        }
    }

    pub fn resize(&mut self, new_bounds: TermBounds) {
        if new_bounds == self.bounds {
            return;
        }
        self.bounds = new_bounds;
        if let Err(err) = self.notifier.0.send(Msg::Resize(new_bounds.window_size())) {
            log_err(&format!("pty resize failed: {err}"));
        }
        self.term.lock().resize(new_bounds);
    }

    pub fn scroll_lines(&mut self, delta: i32) {
        self.term.lock().scroll_display(Scroll::Delta(delta));
        self.rev += 1;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.lock().scroll_display(Scroll::Bottom);
        self.rev += 1;
    }

    #[allow(dead_code)]
    pub fn display_offset(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    /// Run `f` against the locked term's renderable state.
    pub fn with_term<R>(&self, f: impl FnOnce(&Term<Listener>) -> R) -> R {
        let term = self.term.lock();
        f(&term)
    }

    /// Rendered screen as plain text (for the control plane's `read`).
    /// Includes the visible screen; `lines` tails the output.
    pub fn screen_text(&self, lines: Option<usize>) -> String {
        let term = self.term.lock();
        let grid = term.grid();
        let mut out: Vec<String> = Vec::with_capacity(grid.screen_lines());
        for line_idx in 0..grid.screen_lines() as i32 {
            let line = Line(line_idx - grid.display_offset() as i32);
            let mut text = String::with_capacity(grid.columns());
            for col in 0..grid.columns() {
                let cell = &grid[line][Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                text.push(cell.c);
            }
            out.push(text.trim_end().to_string());
        }
        // Trim trailing blank lines.
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        if let Some(n) = lines {
            let skip = out.len().saturating_sub(n);
            out.drain(..skip);
        }
        out.join("\n")
    }

    pub fn shutdown(&self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }

    // ---- selection ----

    /// Window-coordinate position -> buffer grid point + cell side.
    pub fn grid_point(
        &self,
        position: gpui::Point<Pixels>,
    ) -> (alacritty_terminal::index::Point, Side) {
        let b = self.bounds;
        let rel_x = f32::from(position.x - b.bounds.origin.x).max(0.);
        let rel_y = f32::from(position.y - b.bounds.origin.y).max(0.);
        let col = ((rel_x / f32::from(b.cell_width)) as usize).min(b.num_columns() - 1);
        let line = ((rel_y / f32::from(b.line_height)) as i32).min(b.num_lines() as i32 - 1);
        let display_offset = self.term.lock().grid().display_offset() as i32;
        let point = alacritty_terminal::index::Point::new(Line(line - display_offset), Column(col));
        let in_cell_x = rel_x - col as f32 * f32::from(b.cell_width);
        let side = if in_cell_x > f32::from(b.cell_width) / 2. {
            Side::Right
        } else {
            Side::Left
        };
        (point, side)
    }

    pub fn start_selection(&mut self, position: gpui::Point<Pixels>, kind: SelectionType) {
        let (point, side) = self.grid_point(position);
        let mut term = self.term.lock();
        term.selection = Some(Selection::new(kind, point, side));
        drop(term);
        self.rev += 1;
    }

    pub fn update_selection(&mut self, position: gpui::Point<Pixels>) {
        let (point, side) = self.grid_point(position);
        let mut term = self.term.lock();
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, side);
        }
        drop(term);
        self.rev += 1;
    }

    pub fn clear_selection(&mut self) {
        self.term.lock().selection = None;
        self.rev += 1;
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl EventEmitter<TerminalEvent> for Terminal {}

fn log_err(msg: &str) {
    eprintln!("[seance:terminal] {msg}");
}

/// Convert a gpui Hsla to an alacritty Rgb.
fn hsla_to_rgb(color: gpui::Hsla) -> AlacRgb {
    let rgba: gpui::Rgba = color.into();
    AlacRgb {
        r: (rgba.r * 255.) as u8,
        g: (rgba.g * 255.) as u8,
        b: (rgba.b * 255.) as u8,
    }
}

/// Answer OSC color queries (claude-code probes bg to pick its dark theme).
fn color_for_index(index: usize) -> AlacRgb {
    match index {
        0..=15 => hsla_to_rgb(theme::ansi_palette()[index]),
        16..=231 => {
            let i = index - 16;
            let steps = [0u8, 95, 135, 175, 215, 255];
            AlacRgb {
                r: steps[i / 36],
                g: steps[(i / 6) % 6],
                b: steps[i % 6],
            }
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            AlacRgb { r: v, g: v, b: v }
        }
        256 => hsla_to_rgb(theme::SeancePalette::text()),      // foreground
        257 => hsla_to_rgb(theme::SeancePalette::bg()),        // background
        258 => hsla_to_rgb(theme::SeancePalette::flame()),     // cursor
        _ => hsla_to_rgb(theme::SeancePalette::text()),
    }
}

/// Map an ANSI cell color to Hsla using the candlelit palette.
pub fn convert_color(color: &AnsiColor, dim: bool) -> Option<gpui::Hsla> {
    let mut out = match color {
        AnsiColor::Named(named) => match named {
            NamedColor::Foreground => Some(theme::SeancePalette::text()),
            NamedColor::Background => None, // default bg -> transparent
            NamedColor::Cursor => Some(theme::SeancePalette::flame()),
            NamedColor::Black => Some(theme::ansi_palette()[0]),
            NamedColor::Red => Some(theme::ansi_palette()[1]),
            NamedColor::Green => Some(theme::ansi_palette()[2]),
            NamedColor::Yellow => Some(theme::ansi_palette()[3]),
            NamedColor::Blue => Some(theme::ansi_palette()[4]),
            NamedColor::Magenta => Some(theme::ansi_palette()[5]),
            NamedColor::Cyan => Some(theme::ansi_palette()[6]),
            NamedColor::White => Some(theme::ansi_palette()[7]),
            NamedColor::BrightBlack => Some(theme::ansi_palette()[8]),
            NamedColor::BrightRed => Some(theme::ansi_palette()[9]),
            NamedColor::BrightGreen => Some(theme::ansi_palette()[10]),
            NamedColor::BrightYellow => Some(theme::ansi_palette()[11]),
            NamedColor::BrightBlue => Some(theme::ansi_palette()[12]),
            NamedColor::BrightMagenta => Some(theme::ansi_palette()[13]),
            NamedColor::BrightCyan => Some(theme::ansi_palette()[14]),
            NamedColor::BrightWhite => Some(theme::ansi_palette()[15]),
            NamedColor::BrightForeground => Some(theme::SeancePalette::text()),
            NamedColor::DimBlack => Some(dimmed(theme::ansi_palette()[0])),
            NamedColor::DimRed => Some(dimmed(theme::ansi_palette()[1])),
            NamedColor::DimGreen => Some(dimmed(theme::ansi_palette()[2])),
            NamedColor::DimYellow => Some(dimmed(theme::ansi_palette()[3])),
            NamedColor::DimBlue => Some(dimmed(theme::ansi_palette()[4])),
            NamedColor::DimMagenta => Some(dimmed(theme::ansi_palette()[5])),
            NamedColor::DimCyan => Some(dimmed(theme::ansi_palette()[6])),
            NamedColor::DimWhite => Some(dimmed(theme::ansi_palette()[7])),
            NamedColor::DimForeground => Some(dimmed(theme::SeancePalette::text())),
        },
        AnsiColor::Spec(rgb) => Some(gpui::Rgba {
            r: rgb.r as f32 / 255.,
            g: rgb.g as f32 / 255.,
            b: rgb.b as f32 / 255.,
            a: 1.,
        }
        .into()),
        AnsiColor::Indexed(i) => {
            let i = *i as usize;
            match i {
                0..=15 => Some(theme::ansi_palette()[i]),
                16..=231 => {
                    let j = i - 16;
                    let steps = [0f32, 95., 135., 175., 215., 255.];
                    Some(
                        gpui::Rgba {
                            r: steps[j / 36] / 255.,
                            g: steps[(j / 6) % 6] / 255.,
                            b: steps[j % 6] / 255.,
                            a: 1.,
                        }
                        .into(),
                    )
                }
                _ => {
                    let v = (8 + (i.saturating_sub(232)) * 10) as f32 / 255.;
                    Some(gpui::Rgba { r: v, g: v, b: v, a: 1. }.into())
                }
            }
        }
    };
    if dim {
        out = out.map(dimmed);
    }
    out
}

fn dimmed(mut color: gpui::Hsla) -> gpui::Hsla {
    color.a *= 0.65;
    color
}

/// Key event -> PTY bytes. Compact xterm mapping covering what agent TUIs use.
pub fn keystroke_bytes(keystroke: &gpui::Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let mods = keystroke.modifiers;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    // Named/control keys first.
    let named: Option<&[u8]> = match keystroke.key.as_str() {
        "enter" => {
            if mods.shift {
                // Newline-without-submit for agent TUIs (ink treats \n as meta-enter).
                Some(b"\n".as_slice())
            } else {
                Some(b"\r".as_slice())
            }
        }
        "backspace" => Some(if mods.control { b"\x08" } else { b"\x7f" }),
        "tab" => Some(if mods.shift { b"\x1b[Z" } else { b"\t" }),
        "escape" => Some(b"\x1b"),
        "up" => Some(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }),
        "down" => Some(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }),
        "right" => Some(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }),
        "left" => Some(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }),
        "home" => Some(if app_cursor { b"\x1bOH" } else { b"\x1b[H" }),
        "end" => Some(if app_cursor { b"\x1bOF" } else { b"\x1b[F" }),
        // Bare / shift page keys only — ctrl+page* is seance workspace cycle.
        "pageup" if !mods.control => Some(b"\x1b[5~"),
        "pagedown" if !mods.control => Some(b"\x1b[6~"),
        "delete" => Some(b"\x1b[3~"),
        "insert" => Some(b"\x1b[2~"),
        "f1" => Some(b"\x1bOP"),
        "f2" => Some(b"\x1bOQ"),
        "f3" => Some(b"\x1bOR"),
        "f4" => Some(b"\x1bOS"),
        "f5" => Some(b"\x1b[15~"),
        "f6" => Some(b"\x1b[17~"),
        "f7" => Some(b"\x1b[18~"),
        "f8" => Some(b"\x1b[19~"),
        "f9" => Some(b"\x1b[20~"),
        "f10" => Some(b"\x1b[21~"),
        "f11" => Some(b"\x1b[23~"),
        "f12" => Some(b"\x1b[24~"),
        _ => None,
    };
    if let Some(bytes) = named {
        return Some(bytes.to_vec());
    }

    // Ctrl+letter -> C0 control codes.
    if mods.control {
        let key = keystroke.key.as_str();
        if key.len() == 1 {
            let ch = key.chars().next().unwrap().to_ascii_lowercase();
            let byte = match ch {
                'a'..='z' => Some(ch as u8 - b'a' + 1),
                '@' | ' ' => Some(0),
                '[' => Some(27),
                '\\' => Some(28),
                ']' => Some(29),
                '^' => Some(30),
                '_' | '/' => Some(31),
                _ => None,
            };
            if let Some(b) = byte {
                return Some(vec![b]);
            }
        }
    }

    // Plain characters (IME-composed or direct); alt prefixes ESC.
    if let Some(key_char) = &keystroke.key_char {
        let mut bytes = Vec::with_capacity(key_char.len() + 1);
        if mods.alt {
            bytes.push(0x1b);
        }
        bytes.extend_from_slice(key_char.as_bytes());
        return Some(bytes);
    }

    None
}

/// Size helper for views: pixel bounds -> TermBounds with given cell metrics.
pub fn term_bounds(
    bounds: Bounds<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
) -> TermBounds {
    TermBounds {
        cell_width,
        line_height,
        bounds,
    }
}


