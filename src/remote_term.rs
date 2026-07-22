//! Remote terminal: GUI-side mirror of a daemon-owned PTY session.
//!
//! Holds the latest [`GridSnapshot`] and forwards input/resize/scroll to the
//! daemon via [`GuiClient`]. [`TerminalView`]-compatible surface for rendering.

use std::sync::{Arc, Mutex};

use gpui::{Context, EventEmitter};

use crate::gui_client::GuiClient;
use crate::runtime::snapshot::{GhostSnap, GridSnapshot};
use crate::term_shared::{Ghost, TerminalEvent};

/// Debounce PTY resizes so 1px layout jitter can't thrash the daemon.
struct ResizeGate {
    /// Last size we actually sent to the daemon.
    sent: (u16, u16),
    /// Last size measured in layout.
    seen: (u16, u16),
    /// Consecutive frames the measurement has been stable.
    stable: u8,
}

pub struct RemoteTerminal {
    pub slug: String,
    /// Arc so the paint path can hold the grid without cloning thousands of cells.
    pub snapshot: Arc<GridSnapshot>,
    pub ghost: Option<Ghost>,
    /// Who last wrote stdin — drives the causal left-gutter tint.
    pub last_input_origin: Option<String>,
    client: Arc<GuiClient>,
    rev: u64,
    resize: Mutex<ResizeGate>,
}

impl RemoteTerminal {
    pub fn new(slug: String, client: Arc<GuiClient>) -> Self {
        Self {
            snapshot: Arc::new(GridSnapshot::empty(&slug)),
            slug,
            ghost: None,
            last_input_origin: None,
            client,
            rev: 0,
            resize: Mutex::new(ResizeGate {
                sent: (0, 0),
                seen: (0, 0),
                stable: 0,
            }),
        }
    }

    pub fn set_input_origin(&mut self, origin: String, cx: &mut Context<Self>) {
        self.last_input_origin = Some(origin);
        cx.notify();
    }

    /// Accept the next frame at any rev without blanking the last paint.
    /// Workspace switch: daemon will FULL-flush; without this, a same-rev
    /// frame is dropped and the pane can stick on a stale/empty grid.
    pub fn open_rev_gate(&mut self) {
        self.rev = 0;
    }

    /// Clear the painted grid and accept the next frame at any rev.
    /// Used when a damage decode fails — without resetting `rev`, a full
    /// reattach frame at the same rev is dropped as "stale" and the pane
    /// stays blank until something bumps rev (e.g. window resize).
    pub fn clear_for_resync(&mut self, cx: &mut Context<Self>) {
        let pane = self.slug.clone();
        self.rev = 0;
        self.snapshot = Arc::new(GridSnapshot::empty(&pane));
        self.ghost = None;
        {
            let mut g = self.resize.lock().unwrap();
            // Force the next layout pass to re-send size (hysteresis was
            // "stable" on the old geometry).
            g.sent = (0, 0);
            g.seen = (0, 0);
            g.stable = 0;
        }
        cx.notify();
    }

    pub fn apply_snapshot(&mut self, snap: GridSnapshot, cx: &mut Context<Self>) {
        // Drop stale/duplicate frames (throttle + out-of-order socket).
        if snap.rev != 0 && snap.rev <= self.rev {
            return;
        }
        self.rev = snap.rev;
        // Prefer explicit origin on the snap; never wipe a known origin with None.
        if let Some(o) = &snap.last_input_origin {
            self.last_input_origin = Some(o.clone());
        }
        if let Some(g) = &snap.ghost {
            self.ghost = Some(Ghost {
                id: g.id.clone(),
                text: g.text.clone(),
                from: g.from.clone(),
                reason: g.reason.clone(),
            });
        } else {
            self.ghost = None;
        }
        // Keep resize gate aligned with the live PTY size so we don't
        // re-request a size we already have.
        {
            let mut g = self.resize.lock().unwrap();
            g.sent = (snap.cols, snap.rows);
            g.seen = (snap.cols, snap.rows);
            g.stable = 2;
        }
        self.snapshot = Arc::new(snap);
        cx.emit(TerminalEvent::Wakeup);
        // All visible panes paint live. Visibility is enforced upstream
        // (daemon skips other workspaces; GUI drops their grids). The old
        // unfocused ~2–5fps cap was a crisis throttle for pre-batch paint.
        cx.notify();
    }

    pub fn set_ghost(&mut self, ghost: Option<GhostSnap>, cx: &mut Context<Self>) {
        self.ghost = ghost.map(|g| Ghost {
            id: g.id,
            text: g.text,
            from: g.from,
            reason: g.reason,
        });
        // Rebuild snapshot with updated ghost (rare path).
        let mut snap = (*self.snapshot).clone();
        snap.ghost = self.ghost.as_ref().map(|g| GhostSnap {
            id: g.id.clone(),
            text: g.text.clone(),
            from: g.from.clone(),
            reason: g.reason.clone(),
        });
        self.snapshot = Arc::new(snap);
        cx.notify();
    }

    pub fn is_running(&self) -> bool {
        self.snapshot.running
    }

    pub fn title(&self) -> Option<String> {
        self.snapshot.title.clone()
    }

    pub fn write_bytes(&self, bytes: Vec<u8>) {
        let _ = self.client.input(&self.slug, &bytes);
    }

    /// Paste clipboard text into the PTY. Daemon-side inject uses bracketed
    /// paste when the app requested it (same path as `ctl send --no-submit`).
    pub fn paste(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let _ = self.client.inject(&self.slug, text, false);
    }

    /// Optimistic local echo for a printable ASCII key so typing feels instant
    /// even when a neighbor TUI is burning paint budget. Authoritative grid
    /// from the daemon (higher `rev`) overwrites this on the next push.
    pub fn local_echo_char(&mut self, ch: char, cx: &mut Context<Self>) {
        if ch.is_control() || ch == '\u{7f}' {
            return;
        }
        let mut s = (*self.snapshot).clone();
        if s.cols == 0 || s.rows == 0 || s.cells.is_empty() {
            return;
        }
        let cols = s.cols as usize;
        let row = s.cursor_row as usize;
        let col = s.cursor_col as usize;
        if row >= s.rows as usize {
            return;
        }
        let idx = row * cols + col;
        if idx >= s.cells.len() {
            return;
        }
        // Don't invent colors — keep whatever was under the cursor.
        s.cells[idx].c = ch;
        s.cells[idx].bold = false;
        s.cells[idx].dim = false;
        s.cells[idx].italic = false;
        s.cells[idx].underline = false;
        s.cells[idx].inverse = false;
        if col + 1 < cols {
            s.cursor_col = (col + 1) as u16;
        }
        // Leave `rev` unchanged so the next real frame always wins.
        self.snapshot = Arc::new(s);
        cx.notify();
    }

    pub fn inject(&self, text: String, submit: bool) {
        let _ = self.client.inject(&self.slug, &text, submit);
    }

    /// Request a PTY resize.
    ///
    /// - **Large reflows** (pane kill auto-close, sash, window resize — any
    ///   change of more than 1 col/row): send immediately on first measure.
    ///   Waiting for a second stable frame used to leave siblings stuck for
    ///   seconds when nothing else was painting (idle shells after a kill).
    /// - **±1 cell jitter**: still needs 2 consecutive matching frames so
    ///   float cell-width noise can't thrash 120↔121 forever.
    pub fn resize_cells(&self, cols: u16, rows: u16) {
        let cols = cols.max(2);
        let rows = rows.max(2);
        let should_send = {
            let mut g = self.resize.lock().unwrap();
            if g.sent == (cols, rows) {
                g.seen = (cols, rows);
                g.stable = 2;
                false
            } else if g.seen != (cols, rows) {
                g.seen = (cols, rows);
                let big = cols.abs_diff(g.sent.0) > 1 || rows.abs_diff(g.sent.1) > 1;
                if big || g.sent == (0, 0) {
                    // Real reflow or first layout — don't wait for another paint.
                    g.sent = (cols, rows);
                    g.stable = 2;
                    true
                } else {
                    g.stable = 1;
                    false
                }
            } else {
                g.stable = g.stable.saturating_add(1);
                if g.stable >= 2 {
                    g.sent = (cols, rows);
                    true
                } else {
                    false
                }
            }
        };
        if should_send {
            let _ = self.client.resize(&self.slug, cols, rows);
        }
    }

    pub fn scroll_lines(&self, delta: i32) {
        let _ = self.client.scroll(&self.slug, delta);
    }

    pub fn scroll_to_bottom(&self) {
        let _ = self.client.scroll_bottom(&self.slug);
    }

    pub fn ghost_accept(&self) {
        let _ = self.client.ghost_accept(&self.slug);
    }

    pub fn ghost_reject(&self) {
        let _ = self.client.ghost_reject(&self.slug);
    }
}

impl EventEmitter<TerminalEvent> for RemoteTerminal {}
