//! Custom PTY + alacritty Term session — gpui-free, handoff-friendly.
//!
//! We deliberately do **not** use alacritty's `Pty` type for ownership: its
//! `Drop` always SIGHUPs the child. For graceful daemon upgrade we need to
//! release the master FD without killing the process. Our owner dups the FD
//! and forgets the handle (or adopts an FD from SCM_RIGHTS).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use alacritty_terminal::{
    event::{Event as AlacEvent, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Line},
    sync::FairMutex,
    term::{cell::Flags, Config, Term, TermMode},
    vte::ansi::{Color as AnsiColor, NamedColor, Processor, Rgb as AlacRgb},
};
use anyhow::{bail, Context as _, Result};

use super::snapshot::{CellSnap, GhostSnap, GridSnapshot};
use super::{upgrade_in_progress, UPGRADE_IN_PROGRESS};

const SCROLL_HISTORY: usize = 10_000;

/// Events surfaced to the engine / GUI broadcaster.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    Wakeup { slug: String },
    /// Delayed re-attempt after a throttled grid push (so the final frame
    /// after a spinner burst still lands).
    FlushGrid { slug: String },
    Title { slug: String, title: Option<String> },
    Exited { slug: String, code: Option<i32> },
}

struct Listener {
    slug: String,
    tx: Sender<SessionEvent>,
    /// Write-back path for OSC replies / PtyWrite (must reach the PTY).
    write_tx: Sender<IoMsg>,
    title: Arc<Mutex<Option<String>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: AlacEvent) {
        match event {
            AlacEvent::Wakeup => {
                let _ = self.tx.send(SessionEvent::Wakeup {
                    slug: self.slug.clone(),
                });
            }
            AlacEvent::Title(t) => {
                *self.title.lock().unwrap() = Some(t.clone());
                let _ = self.tx.send(SessionEvent::Title {
                    slug: self.slug.clone(),
                    title: Some(t),
                });
            }
            AlacEvent::ResetTitle => {
                *self.title.lock().unwrap() = None;
                let _ = self.tx.send(SessionEvent::Title {
                    slug: self.slug.clone(),
                    title: None,
                });
            }
            AlacEvent::ChildExit(status) => {
                let _ = self.tx.send(SessionEvent::Exited {
                    slug: self.slug.clone(),
                    code: status.code(),
                });
            }
            AlacEvent::Exit => {
                let _ = self.tx.send(SessionEvent::Exited {
                    slug: self.slug.clone(),
                    code: None,
                });
            }
            // Claude (and others) probe palette via OSC to pick a dark theme.
            // Ignoring these makes colors look wrong vs ghostty/alacritty.
            AlacEvent::ColorRequest(index, formatter) => {
                let rgb = color_for_index(index);
                let _ = self
                    .write_tx
                    .send(IoMsg::Write(formatter(rgb).into_bytes()));
            }
            AlacEvent::PtyWrite(s) => {
                let _ = self.write_tx.send(IoMsg::Write(s.into_bytes()));
            }
            AlacEvent::ClipboardLoad(_, formatter) => {
                // Best-effort empty clipboard (GUI owns the real clipboard).
                let _ = self
                    .write_tx
                    .send(IoMsg::Write(formatter("").into_bytes()));
            }
            _ => {}
        }
    }
}

struct Dims {
    cols: u16,
    rows: u16,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// Spawn configuration for a new session.
pub struct SpawnConfig {
    pub command: String,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

/// Adopted PTY from a graceful upgrade (master FD + child pid).
pub struct AdoptedPty {
    pub master_fd: OwnedFd,
    pub child_pid: u32,
    pub cols: u16,
    pub rows: u16,
    pub title: Option<String>,
    pub ghost: Option<GhostSnap>,
}

enum IoMsg {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Shutdown { kill_child: bool },
}

/// One live terminal session.
pub struct PtySession {
    pub slug: String,
    term: Arc<FairMutex<Term<Listener>>>,
    io_tx: Sender<IoMsg>,
    _io_thread: Option<JoinHandle<()>>,
    child: Arc<Mutex<Option<Child>>>,
    child_pid: Arc<Mutex<Option<u32>>>,
    master_fd: Arc<Mutex<Option<RawFd>>>,
    title: Arc<Mutex<Option<String>>>,
    pub ghost: Arc<Mutex<Option<GhostSnap>>>,
    /// Last actor/origin that wrote stdin to this PTY (for causal tint).
    /// Values: `human` | `agent:<slug>` | `cli` | `propose` | …
    pub last_input_origin: Arc<Mutex<Option<String>>>,
    rev: AtomicU64,
    exited: Arc<AtomicBool>,
    cols: Arc<Mutex<u16>>,
    rows: Arc<Mutex<u16>>,
    /// When true, Drop will not SIGHUP (upgrade handoff took the FD).
    handoff_release: AtomicBool,
}

impl PtySession {
    pub fn spawn(
        slug: String,
        config: SpawnConfig,
        event_tx: Sender<SessionEvent>,
    ) -> Result<Self> {
        let cols = config.cols.max(2);
        let rows = config.rows.max(2);

        let (master, slave) = open_pty(cols, rows)?;
        let master_raw = master.as_raw_fd();

        let mut cmd = Command::new("/bin/bash");
        cmd.arg("-lc")
            .arg(format!("exec {}", config.command))
            .current_dir(&config.cwd)
            .stdin(unsafe { Stdio::from_raw_fd(slave.try_clone()?.into_raw_fd()) })
            .stdout(unsafe { Stdio::from_raw_fd(slave.try_clone()?.into_raw_fd()) })
            .stderr(unsafe { Stdio::from_raw_fd(slave.into_raw_fd()) })
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            // Force 24-bit color in chalk/supports-color/ink (claude uses these).
            .env("FORCE_COLOR", "3");
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        // New session, controlling tty = slave (already set as stdio).
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // Set controlling terminal.
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                    // Non-fatal on some systems.
                }
                Ok(())
            });
        }

        let child = cmd.spawn().context("spawn shell in pty")?;
        let pid = child.id();
        // Close our copy of slave — child has it.
        // (slave was moved into Stdio)

        set_nonblocking(master_raw)?;

        Self::from_parts(
            slug,
            master,
            Some(child),
            Some(pid),
            cols,
            rows,
            None,
            None,
            event_tx,
        )
    }

    pub fn adopt(
        slug: String,
        adopted: AdoptedPty,
        event_tx: Sender<SessionEvent>,
    ) -> Result<Self> {
        let master_raw = adopted.master_fd.as_raw_fd();
        set_nonblocking(master_raw)?;
        Self::from_parts(
            slug,
            adopted.master_fd,
            None, // child handle lost across upgrade; we track pid only
            Some(adopted.child_pid),
            adopted.cols.max(2),
            adopted.rows.max(2),
            adopted.title,
            adopted.ghost,
            event_tx,
        )
    }

    fn from_parts(
        slug: String,
        master: OwnedFd,
        child: Option<Child>,
        child_pid: Option<u32>,
        cols: u16,
        rows: u16,
        title_init: Option<String>,
        ghost: Option<GhostSnap>,
        event_tx: Sender<SessionEvent>,
    ) -> Result<Self> {
        let title = Arc::new(Mutex::new(title_init));
        // I/O channel first so the Term listener can write OSC replies back.
        let (io_tx, io_rx) = mpsc::channel::<IoMsg>();
        let listener = Listener {
            slug: slug.clone(),
            tx: event_tx.clone(),
            write_tx: io_tx.clone(),
            title: title.clone(),
        };

        let dims = Dims { cols, rows };
        let term_config = Config {
            scrolling_history: SCROLL_HISTORY,
            ..Config::default()
        };
        let mut term = Term::new(term_config, &dims, listener);
        // Seed the palette through the public parser so Named/Indexed colors
        // resolve before the client issues OSC. Without this, cells painted as
        // "default fg" stay monochrome and Claude's logo never gets orange.
        seed_term_palette(&mut term);
        let term = Arc::new(FairMutex::new(term));

        let master_file = File::from(master);
        let master_fd_slot = Arc::new(Mutex::new(Some(master_file.as_raw_fd())));
        // Keep File in the thread.
        let child = Arc::new(Mutex::new(child));
        let child_pid = Arc::new(Mutex::new(child_pid));
        let exited = Arc::new(AtomicBool::new(false));
        let cols_a = Arc::new(Mutex::new(cols));
        let rows_a = Arc::new(Mutex::new(rows));

        let term_io = Arc::clone(&term);
        let child_io = Arc::clone(&child);
        let child_pid_io = Arc::clone(&child_pid);
        let exited_io = Arc::clone(&exited);
        let slug_io = slug.clone();
        let event_tx_io = event_tx;
        let master_fd_slot_io = Arc::clone(&master_fd_slot);
        let cols_io = Arc::clone(&cols_a);
        let rows_io = Arc::clone(&rows_a);

        let io_thread = thread::Builder::new()
            .name(format!("pty-{slug}"))
            .spawn(move || {
                io_loop(
                    master_file,
                    master_fd_slot_io,
                    term_io,
                    io_rx,
                    child_io,
                    child_pid_io,
                    exited_io,
                    slug_io,
                    event_tx_io,
                    cols_io,
                    rows_io,
                );
            })
            .context("spawn pty io thread")?;

        Ok(Self {
            slug,
            term,
            io_tx,
            _io_thread: Some(io_thread),
            child,
            child_pid,
            master_fd: master_fd_slot,
            title,
            ghost: Arc::new(Mutex::new(ghost)),
            last_input_origin: Arc::new(Mutex::new(None)),
            rev: AtomicU64::new(1),
            exited,
            cols: cols_a,
            rows: rows_a,
            handoff_release: AtomicBool::new(false),
        })
    }

    pub fn is_running(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }

    pub fn title(&self) -> Option<String> {
        self.title.lock().unwrap().clone()
    }

    pub fn child_pid(&self) -> Option<u32> {
        *self.child_pid.lock().unwrap()
    }

    pub fn size(&self) -> (u16, u16) {
        (*self.cols.lock().unwrap(), *self.rows.lock().unwrap())
    }

    pub fn write_bytes(&self, bytes: Vec<u8>) {
        let _ = self.io_tx.send(IoMsg::Write(bytes));
    }

    /// Record who last wrote stdin (for causal tint + event origin).
    pub fn set_input_origin(&self, origin: impl Into<String>) {
        *self.last_input_origin.lock().unwrap() = Some(origin.into());
    }

    pub fn input_origin(&self) -> Option<String> {
        self.last_input_origin.lock().unwrap().clone()
    }

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
            self.write_bytes(
                text.replace("\r\n", "\r")
                    .replace('\n', "\r")
                    .into_bytes(),
            );
        }
    }

    pub fn inject(&self, text: String, submit: bool) {
        self.paste(&text);
        if submit {
            let tx = self.io_tx.clone();
            // Multi-line paste (esp. grok) sometimes needs a second Enter;
            // agents tolerate an extra CR at an empty prompt.
            let extra_enter = text.contains('\n');
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(180));
                let _ = tx.send(IoMsg::Write(b"\r".to_vec()));
                if extra_enter {
                    thread::sleep(Duration::from_millis(80));
                    let _ = tx.send(IoMsg::Write(b"\r".to_vec()));
                }
            });
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let cols = cols.max(2);
        let rows = rows.max(2);
        *self.cols.lock().unwrap() = cols;
        *self.rows.lock().unwrap() = rows;
        let _ = self.io_tx.send(IoMsg::Resize { cols, rows });
    }

    pub fn scroll_lines(&self, delta: i32) {
        self.term.lock().scroll_display(Scroll::Delta(delta));
        self.rev.fetch_add(1, Ordering::SeqCst);
    }

    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
        self.rev.fetch_add(1, Ordering::SeqCst);
    }

    pub fn bump_rev(&self) {
        self.rev.fetch_add(1, Ordering::SeqCst);
    }

    pub fn rev(&self) -> u64 {
        self.rev.load(Ordering::SeqCst)
    }

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
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        if let Some(n) = lines {
            let skip = out.len().saturating_sub(n);
            out.drain(..skip);
        }
        out.join("\n")
    }

    pub fn snapshot(&self) -> GridSnapshot {
        let term = self.term.lock();
        let mode = *term.mode();
        let alt_screen = mode.contains(TermMode::ALT_SCREEN);
        let alternate_scroll = mode.contains(TermMode::ALTERNATE_SCROLL);
        let app_cursor = mode.contains(TermMode::APP_CURSOR);
        let mouse_mode = mode.intersects(TermMode::MOUSE_MODE);
        let sgr_mouse = mode.contains(TermMode::SGR_MOUSE);
        let grid = term.grid();
        let cols = grid.columns() as u16;
        let rows = grid.screen_lines() as u16;
        let cursor = term.grid().cursor.point;
        let display_offset = grid.display_offset() as i32;
        let colors = term.colors();

        let mut cells = Vec::with_capacity((cols as usize) * (rows as usize));
        let mut hyperlinks: Vec<super::snapshot::HyperlinkSpan> = Vec::new();
        let mut open_link: Option<(u16, u16, String)> = None; // row, col_start, uri
        for line_idx in 0..rows as i32 {
            let line = Line(line_idx - display_offset);
            let row_u = line_idx as u16;
            // close any open link at end of previous row
            if let Some((r, cs, uri)) = open_link.take() {
                hyperlinks.push(super::snapshot::HyperlinkSpan {
                    row: r,
                    col_start: cs,
                    col_end: cols,
                    uri,
                });
            }
            for col in 0..cols as usize {
                let cell = &grid[line][Column(col)];
                let fg = resolve_color(colors, &cell.fg, cell.flags, false);
                let bg = resolve_color(colors, &cell.bg, Flags::empty(), true);
                let has_link = cell.hyperlink().map(|h| h.uri().to_string());
                match (&mut open_link, has_link) {
                    (Some((r, cs, uri)), Some(u)) if *r == row_u && *uri == u => {
                        // continue open span
                    }
                    (Some((r, cs, uri)), other) => {
                        hyperlinks.push(super::snapshot::HyperlinkSpan {
                            row: *r,
                            col_start: *cs,
                            col_end: col as u16,
                            uri: uri.clone(),
                        });
                        open_link = other.map(|u| (row_u, col as u16, u));
                    }
                    (None, Some(u)) => {
                        open_link = Some((row_u, col as u16, u));
                    }
                    (None, None) => {}
                }
                let mut underline = cell.flags.contains(Flags::UNDERLINE);
                if cell.hyperlink().is_some() {
                    underline = true;
                }
                cells.push(CellSnap {
                    c: if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        ' '
                    } else {
                        cell.c
                    },
                    fg,
                    bg,
                    bold: cell.flags.contains(Flags::BOLD),
                    dim: cell.flags.contains(Flags::DIM),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline,
                    inverse: cell.flags.contains(Flags::INVERSE),
                });
            }
        }
        if let Some((r, cs, uri)) = open_link.take() {
            hyperlinks.push(super::snapshot::HyperlinkSpan {
                row: r,
                col_start: cs,
                col_end: cols,
                uri,
            });
        }

        // cursor relative to visible screen
        let cursor_row = (cursor.line.0 + display_offset).clamp(0, rows as i32 - 1) as u16;
        let cursor_col = (cursor.column.0 as u16).min(cols.saturating_sub(1));

        // Omit `text` on the wire — paint uses `cells`, and serializing the
        // full screen string doubled payload size for no GUI benefit.
        drop(term);

        GridSnapshot {
            pane: self.slug.clone(),
            rev: self.rev(),
            cols,
            rows,
            cursor_col,
            cursor_row,
            cursor_shape_block: true,
            title: self.title(),
            running: self.is_running(),
            cells,
            ghost: self.ghost.lock().unwrap().clone(),
            text: String::new(),
            alt_screen,
            alternate_scroll,
            app_cursor,
            mouse_mode,
            sgr_mouse,
            last_input_origin: self.input_origin(),
            hyperlinks,
        }
    }

    /// Kill the child and stop the I/O thread.
    pub fn shutdown(&self) {
        let _ = self.io_tx.send(IoMsg::Shutdown { kill_child: true });
    }

    /// Prepare for handoff: stop I/O without killing the child; return the
    /// master FD (dup) for SCM_RIGHTS. After this, the session is inert.
    pub fn prepare_handoff(&self) -> Result<(OwnedFd, u32)> {
        self.handoff_release.store(true, Ordering::SeqCst);
        let _ = self.io_tx.send(IoMsg::Shutdown { kill_child: false });
        // Wait briefly for the I/O thread to release the FD.
        for _ in 0..50 {
            if self.master_fd.lock().unwrap().is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let raw = self
            .master_fd
            .lock()
            .unwrap()
            .take()
            .context("master fd already taken")?;
        // The I/O thread should have forgotten the File; we re-own via dup of
        // whatever is left. Actually the thread transfers ownership back by
        // writing into master_fd... see io_loop. If take() got None, the
        // thread still holds it and put it back as None after close.
        // Simpler path: thread on kill_child=false dups FD into a global channel.
        // For reliability, dup from the raw we stored.
        let owned = unsafe { OwnedFd::from_raw_fd(libc::dup(raw)) };
        if owned.as_raw_fd() < 0 {
            bail!("dup master fd failed");
        }
        // Close the original raw if still open (thread may have closed).
        let _ = unsafe { libc::close(raw) };

        let pid = self.child_pid().context("no child pid for handoff")?;
        Ok((owned, pid))
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.handoff_release.load(Ordering::SeqCst) || upgrade_in_progress() {
            // Don't kill the child — upgrade owns it now.
            let _ = self.io_tx.send(IoMsg::Shutdown { kill_child: false });
            return;
        }
        self.shutdown();
        if let Some(handle) = self._io_thread.take() {
            let _ = handle.join();
        }
    }
}

fn io_loop(
    mut master: File,
    master_fd_slot: Arc<Mutex<Option<RawFd>>>,
    term: Arc<FairMutex<Term<Listener>>>,
    io_rx: Receiver<IoMsg>,
    child: Arc<Mutex<Option<Child>>>,
    child_pid: Arc<Mutex<Option<u32>>>,
    exited: Arc<AtomicBool>,
    slug: String,
    event_tx: Sender<SessionEvent>,
    cols: Arc<Mutex<u16>>,
    rows: Arc<Mutex<u16>>,
) {
    let mut parser: Processor = Processor::new();
    let mut buf = [0u8; 65536];
    *master_fd_slot.lock().unwrap() = Some(master.as_raw_fd());

    loop {
        if UPGRADE_IN_PROGRESS.load(Ordering::SeqCst) {
            // Drain pending writes then wait for Shutdown.
        }

        // Non-blocking read from PTY.
        match master.read(&mut buf) {
            Ok(0) => {
                // EOF
                if !exited.swap(true, Ordering::SeqCst) {
                    let _ = event_tx.send(SessionEvent::Exited {
                        slug: slug.clone(),
                        code: None,
                    });
                }
            }
            Ok(n) => {
                {
                    let mut t = term.lock();
                    parser.advance(&mut *t, &buf[..n]);
                }
                let _ = event_tx.send(SessionEvent::Wakeup {
                    slug: slug.clone(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                if !exited.swap(true, Ordering::SeqCst) {
                    let _ = event_tx.send(SessionEvent::Exited {
                        slug: slug.clone(),
                        code: None,
                    });
                }
            }
        }

        // Process control messages (non-blocking).
        loop {
            match io_rx.try_recv() {
                Ok(IoMsg::Write(bytes)) => {
                    let _ = master.write_all(&bytes);
                }
                Ok(IoMsg::Resize { cols: c, rows: r }) => {
                    *cols.lock().unwrap() = c;
                    *rows.lock().unwrap() = r;
                    let ws = libc::winsize {
                        ws_row: r,
                        ws_col: c,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    unsafe {
                        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
                    }
                    let dims = Dims { cols: c, rows: r };
                    term.lock().resize(dims);
                    let _ = event_tx.send(SessionEvent::Wakeup {
                        slug: slug.clone(),
                    });
                }
                Ok(IoMsg::Shutdown { kill_child }) => {
                    if kill_child {
                        if let Some(mut ch) = child.lock().unwrap().take() {
                            let _ = ch.kill();
                            let _ = ch.wait();
                        } else if let Some(pid) = child_pid.lock().unwrap().take() {
                            unsafe {
                                libc::kill(pid as i32, libc::SIGHUP);
                            }
                        }
                    } else {
                        // Handoff: leave child alive; release FD ownership.
                        // Dup is done by prepare_handoff; just drop File without kill.
                        *master_fd_slot.lock().unwrap() = Some(master.as_raw_fd());
                        // Leak the file by forgetting after taking raw ownership
                        // so Drop doesn't close before handoff dups.
                        let raw = master.as_raw_fd();
                        std::mem::forget(master);
                        *master_fd_slot.lock().unwrap() = Some(raw);
                        return;
                    }
                    *master_fd_slot.lock().unwrap() = None;
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(mut ch) = child.lock().unwrap().take() {
                        let _ = ch.kill();
                        let _ = ch.wait();
                    }
                    return;
                }
            }
        }

        // Check child exit.
        if let Some(ch) = child.lock().unwrap().as_mut() {
            match ch.try_wait() {
                Ok(Some(status)) => {
                    if !exited.swap(true, Ordering::SeqCst) {
                        let _ = event_tx.send(SessionEvent::Exited {
                            slug: slug.clone(),
                            code: status.code(),
                        });
                    }
                }
                _ => {}
            }
        } else if let Some(pid) = *child_pid.lock().unwrap() {
            // No Child handle (post-upgrade): poll with waitpid WNOHANG.
            let mut status = 0;
            let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
            if r == pid as i32 {
                if !exited.swap(true, Ordering::SeqCst) {
                    let code = if libc::WIFEXITED(status) {
                        Some(libc::WEXITSTATUS(status))
                    } else {
                        None
                    };
                    let _ = event_tx.send(SessionEvent::Exited {
                        slug: slug.clone(),
                        code,
                    });
                }
                *child_pid.lock().unwrap() = None;
            }
        }

        thread::sleep(Duration::from_millis(8));
    }
}

fn open_pty(cols: u16, rows: u16) -> Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    let mut ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) };
    if rc != 0 {
        bail!("openpty failed: {}", std::io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    Ok((master, slave))
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        bail!("fcntl GETFL: {}", std::io::Error::last_os_error());
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        bail!("fcntl SETFL: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Ghostty palette from ~/.config/ghostty/config (exact).
/// Terminal *content* matches ghostty — chrome stays candlelit separately.
const ANSI16: [u32; 16] = [
    0x00_18_18_18, //  0 black
    0x00_ab_46_42, //  1 red
    0x00_a1_b5_6c, //  2 green
    0x00_f7_ca_88, //  3 yellow
    0x00_7c_af_c2, //  4 blue
    0x00_ba_8b_af, //  5 magenta
    0x00_86_c1_b9, //  6 cyan
    0x00_d8_d8_d8, //  7 white
    0x00_58_58_58, //  8 bright black
    0x00_ab_46_42, //  9 bright red
    0x00_a1_b5_6c, // 10 bright green
    0x00_f7_ca_88, // 11 bright yellow
    0x00_7c_af_c2, // 12 bright blue
    0x00_ba_8b_af, // 13 bright magenta
    0x00_86_c1_b9, // 14 bright cyan
    0x00_f8_f8_f8, // 15 bright white
];

/// Default fg/bg — ghostty's foreground/background.
const DEFAULT_FG: u32 = 0x00_d8_d8_d8;
const DEFAULT_BG: u32 = 0x00_18_18_18;

/// Answer OSC color queries (claude probes these to pick its dark theme).
fn color_for_index(index: usize) -> AlacRgb {
    let pack = match index {
        0..=15 => ANSI16[index],
        16..=231 => {
            let i = index - 16;
            let steps = [0u32, 95, 135, 175, 215, 255];
            (steps[i / 36] << 16) | (steps[(i / 6) % 6] << 8) | steps[i % 6]
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u32;
            (v << 16) | (v << 8) | v
        }
        256 => DEFAULT_FG, // foreground
        257 => DEFAULT_BG, // background
        258 => 0x00_e5_c0_7b, // cursor
        _ => DEFAULT_FG,
    };
    AlacRgb {
        r: ((pack >> 16) & 0xff) as u8,
        g: ((pack >> 8) & 0xff) as u8,
        b: (pack & 0xff) as u8,
    }
}

fn pack_rgb(rgb: AlacRgb) -> u32 {
    ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | (rgb.b as u32)
}

fn unpack_rgb(pack: u32) -> AlacRgb {
    AlacRgb {
        r: ((pack >> 16) & 0xff) as u8,
        g: ((pack >> 8) & 0xff) as u8,
        b: (pack & 0xff) as u8,
    }
}

/// Initialize term.colors via OSC so the palette is non-empty from the start.
fn seed_term_palette(term: &mut Term<Listener>) {
    let mut parser: Processor = Processor::new();
    let mut seq = String::new();
    for (i, &pack) in ANSI16.iter().enumerate() {
        let c = unpack_rgb(pack);
        // OSC 4 ; idx ; rgb:RR/GG/BB ST
        seq.push_str(&format!(
            "\x1b]4;{i};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\",
            r = c.r,
            g = c.g,
            b = c.b,
        ));
    }
    let fg = unpack_rgb(DEFAULT_FG);
    let bg = unpack_rgb(DEFAULT_BG);
    // OSC 10 default fg, OSC 11 default bg
    seq.push_str(&format!(
        "\x1b]10;rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\",
        r = fg.r,
        g = fg.g,
        b = fg.b,
    ));
    seq.push_str(&format!(
        "\x1b]11;rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\",
        r = bg.r,
        g = bg.g,
        b = bg.b,
    ));
    parser.advance(term, seq.as_bytes());
}

fn dim_u32(c: u32) -> u32 {
    let r = ((c >> 16) & 0xff) * 65 / 100;
    let g = ((c >> 8) & 0xff) * 65 / 100;
    let b = (c & 0xff) * 65 / 100;
    (r << 16) | (g << 8) | b
}

/// Resolve a cell color the way alacritty's display does: prefer the term's
/// live palette (OSC-set), then static ANSI16, with bold→bright for 0..=7.
fn resolve_color(
    colors: &alacritty_terminal::term::color::Colors,
    color: &AnsiColor,
    flags: Flags,
    is_bg: bool,
) -> u32 {
    const DEFAULT: u32 = 0xFFFF_FFFF;
    match color {
        AnsiColor::Spec(rgb) => {
            let packed = pack_rgb(*rgb);
            if flags.contains(Flags::DIM) {
                dim_u32(packed)
            } else {
                packed
            }
        }
        AnsiColor::Named(n) => {
            // Bold/dim named variants first (alacritty display does this).
            let named = if flags.contains(Flags::BOLD) && !flags.contains(Flags::DIM) {
                n.to_bright()
            } else if flags.contains(Flags::DIM) {
                n.to_dim()
            } else {
                *n
            };
            // Prefer the live palette (OSC 4/10/11). Claude sets these and then
            // paints with Named Foreground/Background — if we skip the lookup
            // and return a sentinel, the logo/text all go monochrome white.
            if let Some(rgb) = colors[named] {
                return pack_rgb(rgb);
            }
            // Also try the raw named color before bright/dim transform.
            if let Some(rgb) = colors[*n] {
                return pack_rgb(rgb);
            }
            // Default fg/bg with no OSC override → sentinel so the GUI can
            // paint its own default (cool white / dark).
            if matches!(n, NamedColor::Background) && is_bg {
                return DEFAULT;
            }
            if matches!(n, NamedColor::Foreground) && !is_bg {
                return DEFAULT;
            }
            named_fallback(named, is_bg)
        }
        AnsiColor::Indexed(idx) => {
            let mut idx = *idx as usize;
            // Bold on 0..=7 → bright 8..=15.
            if flags.contains(Flags::BOLD) && (0..=7).contains(&idx) {
                idx += 8;
            }
            if let Some(rgb) = colors[idx] {
                let packed = pack_rgb(rgb);
                return if flags.contains(Flags::DIM) {
                    dim_u32(packed)
                } else {
                    packed
                };
            }
            indexed_fallback(idx, flags.contains(Flags::DIM))
        }
    }
}

fn named_fallback(n: NamedColor, is_bg: bool) -> u32 {
    const DEFAULT: u32 = 0xFFFF_FFFF;
    match n {
        NamedColor::Background if is_bg => DEFAULT,
        NamedColor::Foreground if !is_bg => DEFAULT,
        NamedColor::Black => ANSI16[0],
        NamedColor::Red => ANSI16[1],
        NamedColor::Green => ANSI16[2],
        NamedColor::Yellow => ANSI16[3],
        NamedColor::Blue => ANSI16[4],
        NamedColor::Magenta => ANSI16[5],
        NamedColor::Cyan => ANSI16[6],
        NamedColor::White => ANSI16[7],
        NamedColor::BrightBlack => ANSI16[8],
        NamedColor::BrightRed => ANSI16[9],
        NamedColor::BrightGreen => ANSI16[10],
        NamedColor::BrightYellow => ANSI16[11],
        NamedColor::BrightBlue => ANSI16[12],
        NamedColor::BrightMagenta => ANSI16[13],
        NamedColor::BrightCyan => ANSI16[14],
        NamedColor::BrightWhite | NamedColor::BrightForeground => ANSI16[15],
        NamedColor::Foreground => DEFAULT_FG,
        NamedColor::Background => DEFAULT_BG,
        NamedColor::Cursor => 0x00_e5_c0_7b,
        NamedColor::DimBlack => dim_u32(ANSI16[0]),
        NamedColor::DimRed => dim_u32(ANSI16[1]),
        NamedColor::DimGreen => dim_u32(ANSI16[2]),
        NamedColor::DimYellow => dim_u32(ANSI16[3]),
        NamedColor::DimBlue => dim_u32(ANSI16[4]),
        NamedColor::DimMagenta => dim_u32(ANSI16[5]),
        NamedColor::DimCyan => dim_u32(ANSI16[6]),
        NamedColor::DimWhite => dim_u32(ANSI16[7]),
        NamedColor::DimForeground => dim_u32(DEFAULT_FG),
        _ => DEFAULT,
    }
}

fn indexed_fallback(idx: usize, dim: bool) -> u32 {
    let packed = match idx {
        0..=15 => ANSI16[idx],
        16..=231 => {
            let j = idx - 16;
            let steps = [0u32, 95, 135, 175, 215, 255];
            (steps[j / 36] << 16) | (steps[(j / 6) % 6] << 8) | steps[j % 6]
        }
        232..=255 => {
            let v = (8 + (idx - 232) * 10) as u32;
            (v << 16) | (v << 8) | v
        }
        _ => DEFAULT_FG,
    };
    if dim {
        dim_u32(packed)
    } else {
        packed
    }
}
