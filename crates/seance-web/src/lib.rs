//! seance web client — wasm32 entry + app core.
//!
//! Thin-client architecture: the daemon owns all session state; this client is
//! a projection. `Conn` speaks the GUI wire protocol over a websocket (via the
//! `seance web` bridge), `ClientState` folds daemon events, `TermRenderer`
//! paints grids on WebGL2 canvases, `Chrome` renders the non-terminal DOM, and
//! `Probe` measures what the whole thing actually costs. This module wires
//! them together: boot/auth, the rAF loop, focus, keyboard/mouse routing,
//! selection, and resize.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use seance_core::control::ControlRequest;
use seance_core::input::{key_to_bytes, TermModes};
use seance_core::protocol::{GuiEvent, GuiRequest};

pub mod activity;
pub mod app_api;
pub mod conn;
pub mod help;
pub mod input;
pub mod keymap;
pub mod menus;
pub mod pr_board;
pub mod probe;
pub mod renderer;
pub mod replay;
pub mod replay_edit;
pub mod state;
pub mod subs;
pub mod ui;

use app_api::Actions;
use conn::{Conn, ConnStatus};
use renderer::{RenderOpts, TermRenderer};
use state::{Applied, ClientState};

const FONT_FAMILY: &str =
    "ui-monospace, 'Cascadia Mono', 'CaskaydiaMono Nerd Font Mono', 'JetBrains Mono', monospace";
const FONT_PX: f32 = 14.0;
/// Cursor blink half-period (ms). Typing resets the phase (solid while typing).
const BLINK_MS: f64 = 530.0;

fn window() -> web_sys::Window {
    web_sys::window().expect("no window")
}

fn document() -> web_sys::Document {
    window().document().expect("no document")
}

fn now_ms() -> f64 {
    window().performance().map(|p| p.now()).unwrap_or(0.0)
}

/// `now` in the DAEMON's unix-ms domain — the clock every PR stamp on the wire
/// uses. Local clocks live in `performance.now()`; `clock_offset_ms` is the
/// boot-time difference (0 in tests, so this is identity there).
fn now_unix_ms(state: &state::ClientState) -> f64 {
    now_ms() + state.clock_offset_ms
}

/// One pane's client-side render context.
struct PaneView {
    renderer: TermRenderer,
    /// Last (cols, rows) sent to the daemon for this pane.
    sent_grid: Option<(u16, u16)>,
    /// Last css size we sized the canvas backing store to.
    css_size: (f64, f64),
}

pub struct App {
    state: RefCell<ClientState>,
    conn: RefCell<Option<Rc<Conn>>>,
    chrome: RefCell<Option<ui::Chrome>>,
    probe: RefCell<probe::Probe>,
    views: RefCell<HashMap<String, PaneView>>,
    dirty_grids: RefCell<HashSet<String>>,
    need_rebuild: Cell<bool>,
    badges_dirty: Cell<bool>,
    /// Highest grid frame seq acked back to the daemon. Flow control: the
    /// daemon holds frames (and merges them) past a small in-flight window, so
    /// a slow link can never queue a backlog of stale frames in buffers it
    /// cannot see. See `runtime::outqueue` on the daemon side.
    acked_grid_seq: Cell<u64>,
    structure_rev_bound: Cell<u64>,
    /// Selection: (pane, anchor cell idx, point cell idx, dragging).
    selection: RefCell<Option<(String, usize, usize, bool)>>,
    /// Wheel fractional-row accumulator per pane.
    wheel_accum: RefCell<HashMap<String, f64>>,
    /// Last input send per pane (ms) — echo frames for a hot pane paint
    /// immediately on apply instead of waiting for the next rAF.
    input_hot_ms: RefCell<HashMap<String, f64>>,
    /// Blink phase origin — reset on input so the cursor is solid while typing.
    blink_t0: Cell<f64>,
    /// Last blink on/off we painted the focused pane with.
    blink_last: Cell<bool>,
    /// Last sidebar time-label refresh (ms) — ticked every ~30s.
    labels_refreshed: Cell<f64>,
    /// Summon: the next PaneSpawned opens an inline rename on its tile header
    /// (native `rename_next_spawn`).
    rename_next_spawn: Cell<bool>,
    session_counter: Cell<u64>,
    /// The daemon's own arrangement blob, as last seen. A push rewrites only
    /// the keys this client owns (`seen` / `pinned`) on top of it — native
    /// puts its per-window folds and the flipped pane face in the same file,
    /// and re-serializing our narrower shape over the top would drop them.
    rail_base: RefCell<Option<serde_json::Value>>,
    /// What we last handed the daemon, and how many of our writes are still in
    /// flight. Together they tell our own broadcast echo from another window's
    /// change (`seance_core::util::rail_prefs_is_foreign`).
    rail_last_sent: RefCell<Option<String>>,
    rail_pending: Rc<Cell<usize>>,
    /// Rows each pane is scrolled back by, as far as this client can tell.
    ///
    /// The daemon owns the real scroll position (alacritty's display offset)
    /// and never puts it on the wire — a snapshot is just the visible grid —
    /// so this counts what we asked for. Both ends clamp at the bottom, which
    /// is the end this drives, so they agree there. It can over-count — the
    /// daemon has already hit the oldest line it kept, or something else
    /// (`ctl send`, another window) scrolled the pane to the tail for us — and
    /// then the button lingers until one tap clears it. The inverse, a pane
    /// behind the tail with no button, cannot happen, which is the direction
    /// that would actually mislead.
    scroll_back: RefCell<HashMap<String, i32>>,
    /// Last `m-scrolled` body class we painted (the phone's jump button).
    scroll_chip_on: Cell<bool>,
}

/// localStorage-backed [`subs::SubStore`].
struct LocalStore;

impl subs::SubStore for LocalStore {
    fn get(&self) -> Option<String> {
        storage_get(subs::STORAGE_KEY)
    }
    fn set(&self, val: &str) {
        let _ = storage_set(subs::STORAGE_KEY, val);
    }
}

impl App {
    fn new() -> Rc<App> {
        // Local activity/touch clocks live in the performance.now() domain;
        // the daemon's are unix ms. Capture the offset once at boot so daemon
        // stamps can be converted on ingest (see ClientState::to_perf).
        let prefs = subs::load(&LocalStore);
        let state = ClientState {
            clock_offset_ms: js_sys::Date::now() - now_ms(),
            subs: prefs,
            ..Default::default()
        };
        Rc::new(App {
            state: RefCell::new(state),
            conn: RefCell::new(None),
            chrome: RefCell::new(None),
            probe: RefCell::new(probe::Probe::new()),
            views: RefCell::new(HashMap::new()),
            dirty_grids: RefCell::new(HashSet::new()),
            need_rebuild: Cell::new(false),
            badges_dirty: Cell::new(false),
            acked_grid_seq: Cell::new(0),
            structure_rev_bound: Cell::new(0),
            selection: RefCell::new(None),
            wheel_accum: RefCell::new(HashMap::new()),
            input_hot_ms: RefCell::new(HashMap::new()),
            blink_t0: Cell::new(0.0),
            blink_last: Cell::new(true),
            labels_refreshed: Cell::new(0.0),
            rename_next_spawn: Cell::new(false),
            session_counter: Cell::new(0),
            rail_base: RefCell::new(None),
            rail_last_sent: RefCell::new(None),
            rail_pending: Rc::new(Cell::new(0)),
            scroll_back: RefCell::new(HashMap::new()),
            scroll_chip_on: Cell::new(false),
        })
    }

    fn send(&self, req: &GuiRequest) {
        if let Some(c) = self.conn.borrow().as_ref() {
            c.send(req);
        }
    }

    fn handle_event(&self, ev: GuiEvent) {
        // Ack on receipt, before decode/paint: this is transport flow control,
        // and what it needs to report is that the bytes crossed the link.
        // `seq == 0` marks a one-off refresh outside the windowed stream.
        if let GuiEvent::GridBin { seq, .. } = &ev {
            let seq = *seq;
            if seq > self.acked_grid_seq.get() {
                self.acked_grid_seq.set(seq);
                self.send(&GuiRequest::GridAck { seq });
            }
        }
        let (applied, subs_dirty) = {
            let mut st = self.state.borrow_mut();
            let applied = st.apply_event(ev, now_ms());
            (applied, std::mem::take(&mut st.subs_dirty))
        };
        if subs_dirty {
            self.persist_subs();
        }
        match applied {
            Applied::Nothing => {}
            Applied::Grid { pane } => {
                self.probe.borrow_mut().record_grid(&pane);
                self.dirty_grids.borrow_mut().insert(pane.clone());
                // Typing hot: this is plausibly the echo frame for a keystroke
                // we just sent — paint NOW instead of waiting up to a full rAF
                // (native does the same; see term_shared::typing_hot).
                let hot = self
                    .input_hot_ms
                    .borrow()
                    .get(&pane)
                    .is_some_and(|t| now_ms() - t < 250.0);
                if hot {
                    self.paint();
                }
            }
            Applied::NeedRefresh { pane } => {
                self.send(&GuiRequest::RefreshGrid { pane });
            }
            Applied::Structure => self.need_rebuild.set(true),
            Applied::Badges => self.badges_dirty.set(true),
            Applied::RailPrefs { json } => self.adopt_rail_prefs(&json),
            Applied::Error { message } => {
                if let Some(ch) = self.chrome.borrow_mut().as_mut() {
                    ch.toast(&message);
                }
            }
            Applied::Kicked { by } => {
                // Stop the reconnect loop and say why — a kicked window that
                // silently reconnects defeats the ✦ census kill.
                if let Some(c) = self.conn.borrow().as_ref() {
                    c.shutdown();
                }
                if let Some(ch) = self.chrome.borrow_mut().as_mut() {
                    ch.set_conn_status("closed remotely", false);
                    ch.toast(&format!("this window was closed from {by}"));
                }
            }
        }
    }

    /// Selected workspace, defaulting to the first known one.
    fn selected_workspace(&self) -> Option<String> {
        let st = self.state.borrow();
        st.selected_workspace
            .clone()
            .or_else(|| st.active_workspaces().first().cloned())
    }

    fn focused_pane(&self) -> Option<String> {
        self.state.borrow().focused_pane.clone()
    }

    /// Public mirror for keymap execution.
    pub fn focused_pane_pub(&self) -> Option<String> {
        self.focused_pane()
    }

    /// Kill a workspace; when it is the SELECTED one, pre-select the neighbor
    /// below (above when last) in sidebar order so the human lands somewhere
    /// predictable instead of the daemon's first-pane fallback.
    pub fn kill_workspace_selecting_neighbor(self: &Rc<Self>, ws: &str) {
        let neighbor = {
            let st = self.state.borrow();
            if st.selected_workspace.as_deref() == Some(ws) {
                let order = st.active_workspaces();
                order.iter().position(|w| w == ws).and_then(|idx| {
                    order
                        .get(idx + 1)
                        .or_else(|| idx.checked_sub(1).and_then(|j| order.get(j)))
                        .cloned()
                })
            } else {
                None
            }
        };
        self.send(&GuiRequest::KillWorkspace {
            workspace: ws.to_string(),
        });
        if let Some(n) = neighbor {
            self.select_workspace(&n);
        }
    }

    /// Write the rail arrangement to localStorage.
    ///
    /// Incidental bookkeeping only — `seen`, a prune, a fold. These fire on
    /// selection and on every `State`, and pushing them would race deliberate
    /// changes from other windows (native `save_arrangement_local`).
    fn persist_subs(&self) {
        let st = self.state.borrow();
        subs::save(&LocalStore, &st.subs);
    }

    /// A deliberate change (pin / unpin): localStorage AND the daemon.
    ///
    /// Without the second half a pin lived in one browser and died at the next
    /// reload, because `load_rail_prefs` adopts the daemon's copy on connect —
    /// so the phone could pin a circle and the desk would never hear about it,
    /// and neither would the phone after a refresh.
    fn save_arrangement(&self) {
        self.persist_subs();
        self.push_rail_to_daemon();
    }

    /// Hand the arrangement to the daemon, which persists it and broadcasts it
    /// to every other window.
    ///
    /// Only `seen` and `pinned` are ours to write: the same blob carries
    /// native's folds and flipped-pane face, and this client models neither.
    /// Patching the daemon's own json is what keeps them intact.
    fn push_rail_to_daemon(&self) {
        let mut root = self
            .rail_base
            .borrow()
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        if !root.is_object() {
            root = serde_json::json!({});
        }
        {
            let st = self.state.borrow();
            root["seen"] = serde_json::json!(st.subs.seen);
            root["pinned"] = serde_json::json!(st.subs.pinned);
        }
        let Ok(json) = serde_json::to_string(&root) else {
            return;
        };
        *self.rail_base.borrow_mut() = Some(root);
        *self.rail_last_sent.borrow_mut() = Some(json.clone());
        self.rail_pending.set(self.rail_pending.get() + 1);
        let Some(conn) = self.conn.borrow().as_ref().cloned() else {
            // Offline: localStorage still holds it, and the next connect
            // adopts the daemon's copy — the pin is lost, not half-written.
            self.rail_pending
                .set(self.rail_pending.get().saturating_sub(1));
            return;
        };
        let app_pending = Rc::clone(&self.rail_pending);
        let cb: Box<dyn FnOnce(Result<serde_json::Value, String>)> = Box::new(move |_| {
            app_pending.set(app_pending.get().saturating_sub(1));
        });
        conn.fs_call(seance_core::protocol::FsOp::SubsSave { json }, cb);
    }

    /// Pull the daemon-owned rail arrangement on (re)connect.
    ///
    /// The daemon only *broadcasts* `RailPrefs` when someone changes it, so an
    /// event handler alone leaves a freshly-opened browser showing whatever its
    /// localStorage last held — which is how a desk full of pins rendered here
    /// with no pinned band at all. Native does the same pull (`gui_client.rs`).
    fn load_rail_prefs(self: &Rc<Self>) {
        let app = Rc::clone(self);
        let cb: Box<dyn FnOnce(Result<serde_json::Value, String>)> = Box::new(move |result| {
            if let Ok(v) = result {
                // `{"json": null}` = daemon has no arrangement saved yet.
                if let Some(blob) = v.get("json").and_then(|j| j.as_str()) {
                    app.adopt_rail_prefs(blob);
                }
            }
        });
        if let Some(c) = self.conn.borrow().as_ref() {
            c.fs_call(seance_core::protocol::FsOp::SubsLoad, cb);
        }
    }

    /// Adopt a shared arrangement blob (seen/pinned).
    ///
    /// Adopt WITHOUT echoing: `persist_subs` only writes localStorage, so this
    /// never calls `SubsSave` and cannot start the broadcast loop the daemon
    /// warns about. Local fold state (`collapsed`) is deliberately preserved —
    /// which clusters you keep rolled up is per-window, unlike membership.
    ///
    /// Our own write comes back here too (the daemon broadcasts to every
    /// window including the sender). Adopting that echo is how a pin undoes
    /// itself: fresh local state replaced by an older copy of it.
    fn adopt_rail_prefs(&self, json: &str) {
        if !seance_core::util::rail_prefs_is_foreign(
            self.rail_pending.get(),
            self.rail_last_sent.borrow().as_deref(),
            json,
        ) {
            return;
        }
        let Some(pref) = subs::SubPrefs::parse(json) else {
            return;
        };
        // Keep the daemon's own shape around: a later push patches `seen` and
        // `pinned` into THIS, so keys we don't model survive us.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
            *self.rail_base.borrow_mut() = Some(v);
        }
        {
            let mut st = self.state.borrow_mut();
            if st.subs.pinned == pref.pinned && st.subs.seen == pref.seen {
                return;
            }
            st.subs.seen = pref.seen.clone();
            st.subs.pinned = pref.pinned.clone();
            st.subs.seeded = true;
        }
        self.persist_subs();
        self.need_rebuild.set(true);
    }

    /// Row menu / circle menu "pin": into the top section.
    pub fn pin_workspace(self: &Rc<Self>, ws: &str) {
        if !self.state.borrow_mut().subs.pin(ws) {
            return;
        }
        self.save_arrangement();
        self.need_rebuild.set(true);
    }

    /// Row menu / circle menu "unpin": back into the normal band.
    pub fn unpin_workspace(self: &Rc<Self>, ws: &str) {
        if !self.state.borrow_mut().subs.unpin(ws) {
            return;
        }
        self.save_arrangement();
        self.need_rebuild.set(true);
    }

    /// ctrl+shift+r: inline-rename the selected workspace in the sidebar.
    pub fn begin_selected_workspace_rename(&self) -> bool {
        let ws = match self.state.borrow().selected_workspace.clone() {
            Some(w) => w,
            None => return false,
        };
        if let Some(ch) = self.chrome.borrow_mut().as_mut() {
            ch.begin_rename_workspace(&ws);
            return true;
        }
        false
    }

    /// Escape closes the topmost chrome layer; false = nothing open, the key
    /// belongs to the PTY. Order mirrors native: menu > help > activity >
    /// zoom > selection.
    pub fn escape_topmost(&self) -> bool {
        if menus::close_menu() || help::close() || activity::close() || pr_board::close() {
            return true;
        }
        {
            let mut st = self.state.borrow_mut();
            if st.zoomed.is_some() {
                st.zoomed = None;
                drop(st);
                self.need_rebuild.set(true);
                return true;
            }
        }
        let had_sel = self.selection.borrow().is_some();
        if had_sel {
            let pane = self.selection.borrow_mut().take().map(|(p, _, _, _)| p);
            if let Some(p) = pane {
                self.dirty_grids.borrow_mut().insert(p);
            }
            return true;
        }
        false
    }

    /// Rebuild chrome DOM and (re)bind canvases → renderers.
    fn rebuild(self: &Rc<Self>) {
        {
            let st = self.state.borrow();
            if let Some(ch) = self.chrome.borrow_mut().as_mut() {
                ch.rebuild(&st);
            }
        }
        // Chrome recreated the canvas elements — rebind renderers.
        let mut views = self.views.borrow_mut();
        views.clear();
        let doc = document();
        let slugs: Vec<String> = {
            let st = self.state.borrow();
            match self.selected_workspace() {
                Some(ws) => st
                    .panes_in(&ws)
                    .iter()
                    .filter(|p| p.tiled)
                    .map(|p| p.slug.clone())
                    .collect(),
                None => Vec::new(),
            }
        };
        let dpr = window().device_pixel_ratio();
        for slug in slugs {
            let Some(el) = doc.get_element_by_id(&format!("canvas-{slug}")) else {
                continue;
            };
            let canvas: web_sys::HtmlCanvasElement = match el.dyn_into() {
                Ok(c) => c,
                Err(_) => continue,
            };
            match TermRenderer::new(canvas) {
                Ok(mut r) => {
                    r.set_font(FONT_FAMILY, FONT_PX, dpr);
                    views.insert(
                        slug.clone(),
                        PaneView {
                            renderer: r,
                            sent_grid: None,
                            css_size: (0.0, 0.0),
                        },
                    );
                    self.dirty_grids.borrow_mut().insert(slug);
                }
                Err(e) => web_sys::console::error_2(&"renderer init failed".into(), &e),
            }
        }
    }

    /// Drop the size latch so the next `sync_sizes` re-states our geometry.
    ///
    /// PTY dims are last-writer-wins in the engine, and every client only
    /// sends `Resize` when its *own* measured grid changes. So once another
    /// client reshapes a pane (the phone attaching at 40 cols), this client
    /// never mentions its size again and the PTY stays wrong for it until
    /// the human happens to resize a window.
    ///
    /// Re-asserting on activation makes "most recently active client owns the
    /// dims" fall out of the protocol we already have — and doing it *only*
    /// on activation is what stops two attached clients from fighting: an
    /// unfocused client stays quiet instead of reflowing the pane back.
    fn reassert_grid(&self) {
        {
            let mut views = self.views.borrow_mut();
            for view in views.values_mut() {
                view.sent_grid = None;
            }
        }
        self.sync_sizes();
    }

    /// Size canvases to their tiles; send Resize when the fitting grid changed.
    fn sync_sizes(&self) {
        let doc = document();
        let mut views = self.views.borrow_mut();
        for (slug, view) in views.iter_mut() {
            let Some(el) = doc.get_element_by_id(&format!("canvas-{slug}")) else {
                continue;
            };
            let Some(parent) = el.parent_element() else {
                continue;
            };
            let rect = parent.get_bounding_client_rect();
            let (w, h) = (rect.width(), rect.height());
            if w < 8.0 || h < 8.0 {
                continue;
            }
            if (w - view.css_size.0).abs() > 0.5 || (h - view.css_size.1).abs() > 0.5 {
                view.renderer.resize_to(w, h);
                view.css_size = (w, h);
                self.dirty_grids.borrow_mut().insert(slug.clone());
            }
            let grid = view.renderer.grid_for(w, h);
            if grid.0 >= 2 && grid.1 >= 2 && view.sent_grid != Some(grid) {
                view.sent_grid = Some(grid);
                self.send(&GuiRequest::Resize {
                    pane: slug.clone(),
                    cols: grid.0,
                    rows: grid.1,
                });
            }
        }
    }

    fn paint(&self) {
        let focused = self.focused_pane();
        let t = now_ms();
        let blink_on = ((t - self.blink_t0.get()) / BLINK_MS) as u64 % 2 == 0;
        let mut dirty = std::mem::take(&mut *self.dirty_grids.borrow_mut());
        // Blink transition repaints the focused pane only.
        if blink_on != self.blink_last.get() {
            self.blink_last.set(blink_on);
            if let Some(f) = &focused {
                dirty.insert(f.clone());
            }
        }
        if dirty.is_empty() {
            return;
        }
        let st = self.state.borrow();
        let sel = self.selection.borrow();
        let mut views = self.views.borrow_mut();
        let mut probe = self.probe.borrow_mut();
        for slug in dirty {
            let (Some(view), Some(snap)) = (views.get_mut(&slug), st.grids.get(&slug)) else {
                continue;
            };
            let selection = sel
                .as_ref()
                .filter(|(p, _, _, _)| *p == slug)
                .map(|(_, a, b, _)| (*a.min(b), *a.max(b)));
            let is_focused = focused.as_deref() == Some(slug.as_str());
            let ms = view.renderer.render(
                snap,
                &RenderOpts {
                    focused: is_focused,
                    cursor_visible: !is_focused || blink_on,
                    selection,
                },
            );
            probe.record_frame(ms);
        }
    }

    fn frame(self: &Rc<Self>) {
        {
            // Finish detection re-sorts the sidebar; cheap, once per frame.
            let mut st = self.state.borrow_mut();
            st.sync_working_touches(now_ms());
        }
        // Idle circles' "3m ago" labels tick without any daemon event.
        if now_ms() - self.labels_refreshed.get() > 30_000.0 {
            self.labels_refreshed.set(now_ms());
            self.badges_dirty.set(true);
        }
        if self.need_rebuild.get() {
            self.need_rebuild.set(false);
            self.rebuild();
            self.badges_dirty.set(false);
            // Summon: focus the freshly arrived pane so typing lands in the
            // TERMINAL immediately (rename is a double-click away — an
            // auto-opened rename input ate the first keystrokes).
            if self.rename_next_spawn.get() {
                let spawned = self.state.borrow_mut().last_spawned.take();
                if let Some(slug) = spawned {
                    self.rename_next_spawn.set(false);
                    self.focus_pane(&slug);
                }
            }
        } else if self.badges_dirty.get() {
            self.badges_dirty.set(false);
            let st = self.state.borrow();
            if let Some(ch) = self.chrome.borrow_mut().as_mut() {
                ch.update_badges(&st);
            }
            activity::refresh(&st);
            pr_board::refresh(&st, now_unix_ms(&st));
        }
        self.sync_sizes();
        self.sync_scroll_chip();
        self.paint();
        if let Some(c) = self.conn.borrow().as_ref() {
            self.probe.borrow_mut().set_rtt(c.rtt_ms());
        }
        self.probe.borrow_mut().tick();
    }

    // ── input routing ───────────────────────────────────────────────────────

    fn modes_for(&self, pane: &str) -> TermModes {
        self.state
            .borrow()
            .grids
            .get(pane)
            .map(TermModes::from_snapshot)
            .unwrap_or_default()
    }

    fn pty_input(&self, pane: &str, bytes: &[u8]) {
        self.probe.borrow_mut().record_input(pane);
        self.input_hot_ms
            .borrow_mut()
            .insert(pane.to_string(), now_ms());
        self.blink_t0.set(now_ms());
        // The daemon scrolls a pane to the bottom on every keystroke
        // (`GuiRequest::Input` → `scroll_to_bottom`), so typing is also how
        // you leave the scrollback.
        self.at_bottom(pane);
        if let Some(c) = self.conn.borrow().as_ref() {
            c.input(pane, bytes);
        }
    }

    /// Positive `delta` is back in history (the `GuiRequest::Scroll`
    /// convention), so this is simply how far from the live tail we believe
    /// the pane is. Never below zero: the daemon won't go past the bottom.
    fn note_scroll(&self, pane: &str, delta: i32) {
        let mut back = self.scroll_back.borrow_mut();
        let v = back.entry(pane.to_string()).or_insert(0);
        *v = (*v + delta).max(0);
    }

    fn at_bottom(&self, pane: &str) {
        self.scroll_back.borrow_mut().remove(pane);
    }

    fn scrolled_back(&self, pane: &str) -> bool {
        self.scroll_back.borrow().get(pane).is_some_and(|v| *v > 0)
    }

    /// The pane the jump-to-bottom button would act on: the focused one when
    /// it is behind the tail, else any pane on screen that is.
    ///
    /// Focus alone is not the right question on a phone. A finger drag scrolls
    /// whatever is under it (`seance_mobile_scroll` takes the pane, not the
    /// focus), so a pane you have never tapped can be sitting in history — and
    /// on a fresh load nothing is focused at all.
    fn scrolled_back_pane(&self) -> Option<String> {
        if let Some(pane) = self.focused_pane() {
            if self.scrolled_back(&pane) {
                return Some(pane);
            }
        }
        let ws = self.selected_workspace()?;
        let panes: Vec<String> = {
            let st = self.state.borrow();
            st.panes
                .iter()
                .filter(|p| p.workspace == ws)
                .map(|p| p.slug.clone())
                .collect()
        };
        panes.into_iter().find(|slug| self.scrolled_back(slug))
    }

    /// Raise (or drop) the phone's jump-to-bottom button, via a `<body>`
    /// attribute its CSS keys off. Once per frame rather than at every
    /// mutation point: there are six of those and missing one leaves a button
    /// that lies about where you are.
    ///
    /// An attribute and not a class: the phone chrome owns `body.className`
    /// (drawer, keyboard, sheets) and writing it from here would wipe whatever
    /// it had up.
    fn sync_scroll_chip(&self) {
        let on = self.scrolled_back_pane().is_some();
        if self.scroll_chip_on.get() == on {
            return;
        }
        self.scroll_chip_on.set(on);
        let Some(body) = document().body() else {
            return;
        };
        let el: &web_sys::Element = body.unchecked_ref();
        if on {
            let _ = el.set_attribute("data-scrolled", "1");
        } else {
            let _ = el.remove_attribute("data-scrolled");
        }
    }

    fn on_keydown(self: &Rc<Self>, ev: web_sys::KeyboardEvent) {
        // A focused text input (inline rename, quicklaunch editor, ask reply,
        // login) owns the keyboard — its own handlers deal with Enter/Escape.
        if let Some(active) = document().active_element() {
            let tag = active.tag_name();
            if tag == "INPUT" || tag == "TEXTAREA" {
                return;
            }
        }
        // Chrome commands first (native keymap + alt-fallbacks; keymap.rs).
        if let Some(cmd) = keymap::command_for(&ev) {
            let actions = AppActions(Rc::clone(self));
            if keymap::execute(cmd, &actions, self) {
                ev.prevent_default();
                ev.stop_propagation();
                return;
            }
        }
        let (ctrl, meta) = (ev.ctrl_key(), ev.meta_key());
        let key = ev.key();
        let Some(pane) = self.focused_pane() else {
            return;
        };
        // Copy selection on ctrl/cmd+c when a selection exists (else fall
        // through: plain ctrl+c is SIGINT).
        if (ctrl || meta) && (key == "c" || key == "C") {
            if let Some(text) = self.selection_text() {
                let _ = window().navigator().clipboard().write_text(&text);
                *self.selection.borrow_mut() = None;
                self.dirty_grids.borrow_mut().insert(pane);
                ev.prevent_default();
                return;
            }
        }
        // cmd/meta combos belong to the browser.
        if meta {
            return;
        }
        // Paste arrives via the paste event; don't double-handle ctrl+v.
        if ctrl && (key == "v" || key == "V") {
            return;
        }
        let Some(ki) = input::keyboard_to_keyinput(&ev) else {
            return;
        };
        if let Some(bytes) = key_to_bytes(&ki, self.modes_for(&pane)) {
            // Any PTY-bound key clears the selection.
            if self.selection.borrow().is_some() {
                *self.selection.borrow_mut() = None;
                self.dirty_grids.borrow_mut().insert(pane.clone());
            }
            self.pty_input(&pane, &bytes);
            ev.prevent_default();
            ev.stop_propagation();
        }
    }

    fn on_paste(self: &Rc<Self>, ev: web_sys::ClipboardEvent) {
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let Some(data) = ev.clipboard_data() else {
            return;
        };
        if let Ok(text) = data.get_data("text/plain") {
            if !text.is_empty() {
                self.pty_input(&pane, &input::paste_bytes(&text));
                ev.prevent_default();
            }
        }
    }

    /// Ctrl+PageUp/Down. Parked circles are deliberately out of the rotation —
    /// that is the point of folding them away.
    fn cycle_workspace(self: &Rc<Self>, dir: i32) {
        // Cycle EXACTLY the list the sidebar shows, read live at each press
        // (owner decision 2026-08-02: pageup/down must always correspond to
        // the left sidebar — no snapshots, no alternate orders).
        let st = self.state.borrow();
        let wss = st.displayed_active_ring();
        drop(st);
        if wss.is_empty() {
            return;
        }
        let cur = self.selected_workspace();
        let idx = cur
            .as_ref()
            .and_then(|c| wss.iter().position(|w| w == c))
            .unwrap_or(0);
        let next = ((idx as i32 + dir).rem_euclid(wss.len() as i32)) as usize;
        self.select_workspace(&wss[next]);
    }

    /// Ctrl+shift+home / ctrl+shift+1..9: jump to rail row `idx` (0-based).
    /// Row 0 is the first pinned circle if anything is pinned, else the top of
    /// active (a working agent, else most recent), and so on down the bands —
    /// the same live list ctrl+page walks, so the row you count with your eye
    /// is the row you get. Always reports the key as handled: alt+home is the
    /// browser's "go to homepage" and an empty rail must not let it through.
    pub(crate) fn select_nth_workspace(self: &Rc<Self>, idx: usize) -> bool {
        let target = {
            let st = self.state.borrow();
            st.displayed_active_ring().into_iter().nth(idx)
        };
        let Some(ws) = target else {
            return true;
        };
        if self.selected_workspace().as_deref() == Some(ws.as_str()) {
            return true;
        }
        self.select_workspace(&ws);
        true
    }

    /// Cell index under a mouse event on a pane canvas, clamped to the grid.
    fn cell_at(&self, pane: &str, ev: &web_sys::MouseEvent) -> Option<(u16, u16, usize)> {
        let views = self.views.borrow();
        let view = views.get(pane)?;
        let (cw, ch) = view.renderer.cell_size_css();
        let st = self.state.borrow();
        let snap = st.grids.get(pane)?;
        let col = ((ev.offset_x() as f32 / cw) as i32).clamp(0, snap.cols as i32 - 1) as u16;
        let row = ((ev.offset_y() as f32 / ch) as i32).clamp(0, snap.rows as i32 - 1) as u16;
        Some((col, row, row as usize * snap.cols as usize + col as usize))
    }

    fn selection_text(&self) -> Option<String> {
        let sel = self.selection.borrow();
        let (pane, a, b, _) = sel.as_ref()?;
        let st = self.state.borrow();
        let snap = st.grids.get(pane)?;
        let (start, end) = (*a.min(b), *a.max(b));
        if snap.cells.is_empty() {
            return None;
        }
        let cols = snap.cols as usize;
        let mut out = String::new();
        let mut row_start = start;
        while row_start <= end {
            let row = row_start / cols;
            let row_end = ((row + 1) * cols - 1).min(end);
            let line: String = snap.cells[row_start..=row_end.min(snap.cells.len() - 1)]
                .iter()
                .map(|c| c.c)
                .collect();
            out.push_str(line.trim_end());
            row_start = (row + 1) * cols;
            if row_start <= end {
                out.push('\n');
            }
        }
        Some(out)
    }

    fn pane_for_event_target(ev: &web_sys::Event) -> Option<String> {
        let target = ev.target()?;
        let el: web_sys::Element = target.dyn_into().ok()?;
        let hit = el.closest("[data-slug]").ok()??;
        hit.get_attribute("data-slug")
    }

    fn on_mousedown(self: &Rc<Self>, ev: web_sys::MouseEvent) {
        let Some(pane) = Self::pane_for_event_target(ev.as_ref()) else {
            return;
        };
        if self.focused_pane().as_deref() != Some(pane.as_str()) {
            self.focus_pane(&pane);
        }
        // Only canvas hits drive the PTY/selection; header clicks are chrome's.
        let is_canvas = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .map(|e| e.tag_name() == "CANVAS")
            .unwrap_or(false);
        if !is_canvas || ev.button() != 0 {
            return;
        }
        let Some((col, row, idx)) = self.cell_at(&pane, &ev) else {
            return;
        };
        // ctrl+click opens the link under the cursor, same as the native pane.
        if ev.ctrl_key() {
            let st = self.state.borrow();
            let hit = st
                .grids
                .get(&pane)
                .and_then(|s| seance_core::links::url_at_cell(s, row, col));
            drop(st);
            if let Some(url) = hit {
                ui::open_url(&url);
                ev.prevent_default();
                return;
            }
        }
        let st = self.state.borrow();
        let mouse_mode = st
            .grids
            .get(&pane)
            .map(|s| s.mouse_mode && s.sgr_mouse)
            .unwrap_or(false);
        drop(st);
        if mouse_mode && !ev.shift_key() {
            if let Some(bytes) = input::mouse_report(&ev, true, col, row) {
                self.pty_input(&pane, &bytes);
            }
            return;
        }
        *self.selection.borrow_mut() = Some((pane.clone(), idx, idx, true));
        self.dirty_grids.borrow_mut().insert(pane);
        ev.prevent_default();
    }

    fn on_mousemove(self: &Rc<Self>, ev: web_sys::MouseEvent) {
        let pane = {
            let sel = self.selection.borrow();
            match sel.as_ref() {
                Some((pane, _, _, true)) => pane.clone(),
                _ => return,
            }
        };
        if let Some((_, _, idx)) = self.cell_at(&pane, &ev) {
            let mut sel = self.selection.borrow_mut();
            if let Some((_, _, point_ref, _)) = sel.as_mut() {
                if *point_ref != idx {
                    *point_ref = idx;
                    self.dirty_grids.borrow_mut().insert(pane);
                }
            }
        }
    }

    fn on_mouseup(self: &Rc<Self>, ev: web_sys::MouseEvent) {
        let mut sel = self.selection.borrow_mut();
        if let Some((pane, a, b, dragging)) = sel.as_mut() {
            if *dragging {
                *dragging = false;
                // Click without drag = no selection.
                if a == b {
                    let pane = pane.clone();
                    *sel = None;
                    drop(sel);
                    self.dirty_grids.borrow_mut().insert(pane);
                    return;
                }
            }
            return;
        }
        drop(sel);
        // SGR release for mouse-mode apps.
        if let Some(pane) = Self::pane_for_event_target(ev.as_ref()) {
            let st = self.state.borrow();
            let mouse_mode = st
                .grids
                .get(&pane)
                .map(|s| s.mouse_mode && s.sgr_mouse)
                .unwrap_or(false);
            drop(st);
            if mouse_mode {
                if let Some((col, row, _)) = self.cell_at(&pane, &ev) {
                    if let Some(bytes) = input::mouse_report(&ev, false, col, row) {
                        self.pty_input(&pane, &bytes);
                    }
                }
            }
        }
    }

    fn on_wheel(self: &Rc<Self>, ev: web_sys::WheelEvent) {
        let Some(pane) = Self::pane_for_event_target(ev.as_ref()) else {
            return;
        };
        let st = self.state.borrow();
        let Some(snap) = st.grids.get(&pane) else {
            return;
        };
        let cell_h = self
            .views
            .borrow()
            .get(&pane)
            .map(|v| v.renderer.cell_size_css().1)
            .unwrap_or(17.0);
        let (col, row) = self
            .cell_at(&pane, ev.as_ref())
            .map(|(c, r, _)| (c, r))
            .unwrap_or((0, 0));
        let snap_clone = snap.clone();
        drop(st);
        let mut accum = self.wheel_accum.borrow_mut();
        let acc = accum.entry(pane.clone()).or_insert(0.0);
        match input::wheel_to_action(&ev, &snap_clone, cell_h, col, row, acc) {
            input::WheelAction::Scroll(rows) => {
                drop(accum);
                self.note_scroll(&pane, rows);
                self.send(&GuiRequest::Scroll { pane, delta: rows });
            }
            input::WheelAction::Bytes(bytes) => {
                drop(accum);
                self.pty_input(&pane, &bytes);
            }
            input::WheelAction::None => {}
        }
        ev.prevent_default();
    }
}

/// `Actions` façade handed to chrome (and used internally).
struct AppActions(Rc<App>);

impl Actions for AppActions {
    fn send(&self, req: GuiRequest) {
        self.0.send(&req);
    }
    fn focus_pane(&self, slug: &str) {
        self.0.focus_pane(slug);
    }
    fn select_workspace(&self, ws: &str) {
        self.0.select_workspace(ws);
    }
    fn spawn_pane(
        &self,
        name: &str,
        cwd: Option<String>,
        command: Option<String>,
        workspace: Option<String>,
    ) {
        self.0.send(&GuiRequest::Spawn {
            name: name.to_string(),
            cwd,
            command,
            workspace: workspace.or_else(|| self.0.selected_workspace()),
            file: None,
            tiled: true,
        });
    }
    fn kill_pane(&self, slug: &str) {
        self.0.send(&GuiRequest::Kill { pane: slug.into() });
    }
    fn rename_pane(&self, slug: &str, name: &str) {
        self.0.send(&GuiRequest::RenamePane {
            pane: slug.into(),
            name: name.into(),
        });
    }
    fn create_workspace(&self, name: &str) {
        self.0
            .send(&GuiRequest::CreateWorkspace { name: name.into() });
    }
    fn rename_workspace(&self, old: &str, new: &str) {
        // A rename sets the label; the slug this circle is pinned under
        // does not move, so there is nothing to carry.
        self.0.send(&GuiRequest::RenameWorkspace {
            old: old.into(),
            new: new.into(),
        });
    }
    fn kill_workspace(&self, ws: &str) {
        self.0.kill_workspace_selecting_neighbor(ws);
    }
    fn answer_ask(&self, id: &str, answer: &str) {
        self.0.send(&GuiRequest::AnswerAsk {
            id: id.into(),
            answer: answer.into(),
        });
    }
    fn inject(&self, pane: &str, text: &str, submit: bool) {
        // Inject scrolls the pane to the bottom daemon-side, same as a
        // keystroke.
        self.0.at_bottom(pane);
        self.0.send(&GuiRequest::Inject {
            pane: pane.into(),
            text: text.into(),
            submit,
        });
    }
    fn input_bytes(&self, pane: &str, bytes: &[u8]) {
        self.0.pty_input(pane, bytes);
    }
    fn scroll(&self, pane: &str, delta: i32) {
        self.0.note_scroll(pane, delta);
        self.0.send(&GuiRequest::Scroll {
            pane: pane.into(),
            delta,
        });
    }
    fn scroll_bottom(&self, pane: &str) {
        self.0.at_bottom(pane);
        self.0.send(&GuiRequest::ScrollBottom { pane: pane.into() });
    }
    fn resize(&self, pane: &str, cols: u16, rows: u16) {
        self.0.send(&GuiRequest::Resize {
            pane: pane.into(),
            cols,
            rows,
        });
    }
    fn refresh_grid(&self, pane: &str) {
        self.0.send(&GuiRequest::RefreshGrid { pane: pane.into() });
    }
    fn ghost_accept(&self, pane: &str) {
        self.0.send(&GuiRequest::GhostAccept { pane: pane.into() });
    }
    fn ghost_reject(&self, pane: &str) {
        self.0.send(&GuiRequest::GhostReject { pane: pane.into() });
    }
    fn toggle_probe(&self) {
        self.0.toggle_probe();
    }

    fn summon(&self) {
        let n = self.0.session_counter.get() + 1;
        self.0.session_counter.set(n);
        self.0.rename_next_spawn.set(true);
        self.0.send(&GuiRequest::Spawn {
            name: format!("term-{n}"),
            cwd: None,
            command: None,
            workspace: self.0.selected_workspace(),
            file: None,
            tiled: true,
        });
    }

    fn quicklaunch(&self, name: &str, cwd: Option<String>, command: Option<String>) {
        let ws = {
            let st = self.0.state.borrow();
            // Uniquify against EVERY known circle, not just subscribed ones —
            // the daemon-owned clock census carries them all.
            let mut taken: Vec<String> = st.workspaces();
            taken.extend(st.workspace_activity.keys().cloned());
            taken.extend(st.workspace_touch.keys().cloned());
            let refs: Vec<&str> = taken.iter().map(|s| s.as_str()).collect();
            seance_core::util::unique_slug(name, &refs)
        };
        self.0.send(&GuiRequest::Spawn {
            name: name.to_string(),
            cwd,
            command: command.filter(|c| !c.trim().is_empty()),
            workspace: Some(ws.clone()),
            file: None,
            tiled: true,
        });
        // Land in the fresh circle when its pane arrives, PINNED: you pressed
        // a button to start this thing, so it belongs at the top of the rail
        // until you say otherwise (pin implies active — native twin does the
        // same in app/quicklaunch.rs).
        {
            let mut st = self.0.state.borrow_mut();
            st.subs.pin(&ws);
            st.selected_workspace = Some(ws);
        }
        // Through the daemon, like any other pin: a circle launched from the
        // phone should be pinned at the desk too. The pin references a circle
        // the daemon hasn't minted yet — `settle_absent` is what stops the
        // next `State` from pruning it straight back off.
        self.0.save_arrangement();
        self.0.need_rebuild.set(true);
    }

    fn toggle_collapsed(&self, key: &str) {
        if self.0.state.borrow_mut().subs.toggle_collapsed(key) {
            self.0.persist_subs();
        }
        self.0.need_rebuild.set(true);
    }
    fn request_rebuild(&self) {
        self.0.need_rebuild.set(true);
    }

    fn pin_workspace(&self, ws: &str) {
        self.0.pin_workspace(ws);
    }

    fn unpin_workspace(&self, ws: &str) {
        self.0.unpin_workspace(ws);
    }

    fn cycle_workspace(&self, delta: i32) {
        self.0.cycle_workspace(delta);
    }

    fn cycle_pane(&self, delta: i32) {
        let (slugs, cur) = {
            let st = self.0.state.borrow();
            let ws = match &st.selected_workspace {
                Some(w) => w.clone(),
                None => return,
            };
            let slugs: Vec<String> = st
                .panes_in(&ws)
                .iter()
                .filter(|p| p.tiled)
                .map(|p| p.slug.clone())
                .collect();
            (slugs, st.focused_pane.clone())
        };
        if slugs.is_empty() {
            return;
        }
        let idx = cur
            .and_then(|c| slugs.iter().position(|s| *s == c))
            .unwrap_or(0);
        let next = ((idx as i32 + delta).rem_euclid(slugs.len() as i32)) as usize;
        self.0.focus_pane(&slugs[next]);
    }

    fn kill_active(&self) {
        let st = self.0.state.borrow();
        let focused = st.focused_pane.clone();
        if let Some(slug) = focused {
            let ws = st.pane(&slug).map(|p| p.workspace.clone());
            let last_in_ws = ws
                .as_ref()
                .is_some_and(|w| st.panes.iter().filter(|p| p.workspace == *w).count() == 1);
            drop(st);
            if last_in_ws {
                if let Some(w) = ws {
                    self.0.kill_workspace_selecting_neighbor(&w);
                }
            } else {
                self.0.send(&GuiRequest::Kill { pane: slug });
            }
        } else if let Some(ws) = st.selected_workspace.clone() {
            let empty = !st.panes.iter().any(|p| p.workspace == ws);
            drop(st);
            if empty {
                self.0.kill_workspace_selecting_neighbor(&ws);
            }
        }
    }

    fn toggle_zoom(&self, slug: &str) {
        {
            let mut st = self.0.state.borrow_mut();
            st.zoomed = if st.zoomed.as_deref() == Some(slug) {
                None
            } else {
                Some(slug.to_string())
            };
        }
        self.0.need_rebuild.set(true);
    }

    fn toggle_help(&self) {
        help::toggle();
    }

    fn toggle_activity(&self) {
        let st = self.0.state.borrow();
        activity::toggle(&st);
    }

    fn toggle_pr_board(&self) {
        let actions: Rc<dyn Actions> = Rc::new(AppActions(Rc::clone(&self.0)));
        let st = self.0.state.borrow();
        pr_board::toggle(&st, now_unix_ms(&st), actions);
    }

    fn remove_pr_link(&self, ws: &str, url: &str) {
        self.0.send(&GuiRequest::Ctl(ControlRequest::PrLinkClear {
            url: Some(url.to_string()),
            workspace: Some(ws.to_string()),
            scope: None,
            from: Some("web".into()),
        }));
        let changed = self.0.state.borrow_mut().remove_pr_link(ws, url);
        if changed {
            self.0.need_rebuild.set(true);
            let st = self.0.state.borrow();
            pr_board::refresh(&st, now_unix_ms(&st));
        }
    }

    fn fs_call(
        &self,
        op: seance_core::protocol::FsOp,
        cb: Box<dyn FnOnce(Result<serde_json::Value, String>)>,
    ) {
        if let Some(c) = self.0.conn.borrow().as_ref() {
            c.fs_call(op, cb);
        }
    }

    fn host_select(&self, widget: &str, item: &str) {
        let app = Rc::clone(&self.0);
        let item_label = item.to_string();
        let op = seance_core::protocol::FsOp::HostSelect {
            widget: widget.to_string(),
            item: item.to_string(),
        };
        self.fs_call(
            op,
            Box::new(move |result| {
                let msg = match result {
                    Ok(_) => format!("claude → {item_label}"),
                    Err(e) => format!("switch failed: {e}"),
                };
                if let Some(ch) = app.chrome.borrow_mut().as_mut() {
                    ch.toast(&msg);
                }
            }),
        );
    }
}

impl App {
    fn focus_pane(self: &Rc<Self>, slug: &str) {
        {
            let mut st = self.state.borrow_mut();
            st.focused_pane = Some(slug.to_string());
        }
        self.send(&GuiRequest::SetFocus {
            pane: Some(slug.to_string()),
            workspace: None,
        });
        self.need_rebuild.set(true);
    }

    fn select_workspace(self: &Rc<Self>, ws: &str) {
        // Selecting is looking — clears the `needs` badge.
        let looked = {
            let mut st = self.state.borrow_mut();
            st.subs.mark_seen(ws)
        };
        if looked {
            self.persist_subs();
        }
        {
            let mut st = self.state.borrow_mut();
            st.selected_workspace = Some(ws.to_string());
            // Focus follows: first pane of the workspace.
            st.focused_pane = st.panes_in(ws).first().map(|p| p.slug.clone());
        }
        self.send(&GuiRequest::SetFocus {
            pane: self.focused_pane(),
            workspace: Some(ws.to_string()),
        });
        self.need_rebuild.set(true);
    }

    fn toggle_probe(self: &Rc<Self>) {
        self.probe.borrow_mut().toggle();
    }

    fn connect(self: &Rc<Self>, token: &str) {
        let loc = window().location();
        let proto = if loc.protocol().ok().as_deref() == Some("https:") {
            "wss"
        } else {
            "ws"
        };
        let host = loc.host().unwrap_or_else(|_| "localhost:9666".into());
        let url = format!("{proto}://{host}/ws?token={token}");
        let app_ev = Rc::clone(self);
        let app_st = Rc::clone(self);
        let conn = conn::connect(
            url,
            Box::new(move |ev| app_ev.handle_event(ev)),
            Box::new(move |status| {
                let mut chrome = app_st.chrome.borrow_mut();
                let Some(ch) = chrome.as_mut() else { return };
                match status {
                    ConnStatus::Connecting => ch.set_conn_status("connecting", false),
                    ConnStatus::Connected => {
                        ch.set_conn_status("live", true);
                        ch.hide_login();
                        // Drop the chrome borrow before the fs_call: adopting
                        // rebuilds the sidebar, which borrows chrome again.
                        drop(chrome);
                        app_st.load_rail_prefs();
                        return;
                    }
                    ConnStatus::Disconnected => ch.set_conn_status("reconnecting", false),
                    ConnStatus::AuthFailed => {
                        ch.set_conn_status("auth failed", false);
                        let _ = storage_remove("seance_token");
                        let app = Rc::clone(&app_st);
                        ch.show_login(Box::new(move |tok| {
                            let _ = storage_set("seance_token", &tok);
                            app.connect(&tok);
                        }));
                    }
                }
            }),
        );
        *self.conn.borrow_mut() = Some(conn);
    }
}

fn storage_get(key: &str) -> Option<String> {
    window().local_storage().ok()??.get_item(key).ok()?
}
fn storage_set(key: &str, val: &str) -> Result<(), JsValue> {
    if let Ok(Some(s)) = window().local_storage() {
        s.set_item(key, val)?;
    }
    Ok(())
}
fn storage_remove(key: &str) -> Result<(), JsValue> {
    if let Ok(Some(s)) = window().local_storage() {
        s.remove_item(key)?;
    }
    Ok(())
}

/// Token from ?token=/#token= (stored + stripped from the URL) or storage.
fn initial_token() -> Option<String> {
    let loc = window().location();
    let href = loc.href().ok()?;
    for sep in ["?token=", "#token="] {
        if let Some(i) = href.find(sep) {
            let tail = &href[i + sep.len()..];
            let tok: String = tail.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if !tok.is_empty() {
                let _ = storage_set("seance_token", &tok);
                // Strip the token from the address bar + history.
                if let Ok(hist) = window().history() {
                    let clean = &href[..i];
                    let _ = hist.replace_state_with_url(&JsValue::NULL, "", Some(clean));
                }
                return Some(tok);
            }
        }
    }
    storage_get("seance_token")
}

// ── mobile bridge ───────────────────────────────────────────────────────
// The phone chrome (mic, key pad, swipe, circle menu) is plain JS in
// www/index.html, because none of it needs the renderer — but the websocket
// (and the grid) lives in here, so JS has no way to reach a pane. These
// functions are that seam.
//
// Deliberately thin: text and keys go through the SAME `key_to_bytes` /
// `paste_bytes` encoders and the same `pty_input` path a physical keyboard
// uses, so application-cursor mode and bracketed paste behave identically
// no matter which surface produced the keystroke. Adding a key later is one
// arm in `seance_mobile_key` — the encoder already knows every key name
// `map_key_name` accepts.
thread_local! {
    static MOBILE_APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

fn with_mobile_app<T>(f: impl FnOnce(&Rc<App>) -> T) -> Option<T> {
    MOBILE_APP.with(|m| m.borrow().as_ref().map(f))
}

/// Type text into the focused pane. `submit` appends a carriage return.
/// Returns false when nothing is focused, so JS can keep the draft.
#[wasm_bindgen]
pub fn seance_mobile_text(text: &str, submit: bool) -> bool {
    with_mobile_app(|app| {
        let Some(pane) = app.focused_pane_pub() else {
            return false;
        };
        if !text.is_empty() {
            app.pty_input(&pane, &input::paste_bytes(text));
        }
        if submit {
            app.pty_input(&pane, b"\r");
        }
        true
    })
    .unwrap_or(false)
}

/// Send one key to the focused pane, encoded against that pane's current
/// terminal modes. `key` is either a named key ("ArrowUp", "Escape", "Tab")
/// or a single character ("c") — with `ctrl` set, core emits the C0 code, so
/// ctrl+c is a real SIGINT rather than a stray letter.
#[wasm_bindgen]
pub fn seance_mobile_key(key: &str, ctrl: bool, alt: bool, shift: bool) -> bool {
    with_mobile_app(|app| {
        let Some(pane) = app.focused_pane_pub() else {
            return false;
        };
        let mods = seance_core::input::Modifiers {
            shift,
            control: ctrl,
            alt,
            platform: false,
        };
        let Some(ki) = input::key_input_from_parts(key, mods) else {
            return false;
        };
        let Some(bytes) = seance_core::input::key_to_bytes(&ki, app.modes_for(&pane)) else {
            return false;
        };
        app.pty_input(&pane, &bytes);
        true
    })
    .unwrap_or(false)
}

/// Touch-drag scrollback for `pane`, in CSS pixels of finger travel.
///
/// Routed through `scroll_action` with `touch = true`, which is the wheel's
/// precedence MINUS the alternate-scroll arrow arm: a mouse-reporting app
/// still gets SGR wheel events, anything else gets daemon scrollback. The
/// arrow arm is a wheel convention and sending it from a finger drag walked
/// Claude through prompt history instead of scrolling. Sub-row remainders
/// accumulate in the shared `wheel_accum`, so a slow drag still moves rather
/// than rounding to zero.
#[wasm_bindgen]
pub fn seance_mobile_scroll(pane: &str, dy_px: f64) -> bool {
    with_mobile_app(|app| {
        let st = app.state.borrow();
        let Some(snap) = st.grids.get(pane).cloned() else {
            return false;
        };
        drop(st);
        let cell_h = app
            .views
            .borrow()
            .get(pane)
            .map(|v| v.renderer.cell_size_css().1)
            .unwrap_or(17.0);
        let rows = {
            let mut accum = app.wheel_accum.borrow_mut();
            let acc = accum.entry(pane.to_string()).or_insert(0.0);
            // Finger down should reveal earlier output, which is a wheel-up:
            // negate so the drag carries the content with it.
            input::wheel_rows(-dy_px, web_sys::WheelEvent::DOM_DELTA_PIXEL, cell_h, acc)
        };
        match input::scroll_action(rows, &snap, 0, 0, true) {
            input::WheelAction::Scroll(r) => {
                app.note_scroll(pane, r);
                app.send(&GuiRequest::Scroll {
                    pane: pane.to_string(),
                    delta: r,
                });
                true
            }
            input::WheelAction::Bytes(b) => {
                app.pty_input(pane, &b);
                true
            }
            input::WheelAction::None => false,
        }
    })
    .unwrap_or(false)
}

/// The URL under a viewport point in `pane` — the tap twin of native
/// ctrl+click. `None` means the finger landed on plain text, which is the
/// phone chrome's signal to leave the tap alone.
///
/// Client coordinates, not offsets: a touch carries no `offsetX`, and the
/// canvas rect is the only thing that makes the two comparable.
#[wasm_bindgen]
pub fn seance_mobile_url_at(pane: &str, client_x: f64, client_y: f64) -> Option<String> {
    with_mobile_app(|app| {
        let canvas = document().get_element_by_id(&format!("canvas-{pane}"))?;
        let rect = canvas.get_bounding_client_rect();
        let (x, y) = (client_x - rect.left(), client_y - rect.top());
        if x < 0.0 || y < 0.0 || x >= rect.width() || y >= rect.height() {
            return None;
        }
        let (cw, ch) = app
            .views
            .borrow()
            .get(pane)
            .map(|v| v.renderer.cell_size_css())?;
        if cw <= 0.0 || ch <= 0.0 {
            return None;
        }
        let st = app.state.borrow();
        let snap = st.grids.get(pane)?;
        let col = ((x / cw as f64) as i32).clamp(0, snap.cols as i32 - 1) as u16;
        let row = ((y / ch as f64) as i32).clamp(0, snap.rows as i32 - 1) as u16;
        seance_core::links::url_at_cell(snap, row, col)
    })
    .flatten()
}

/// Jump the focused pane back to the live tail. The phone's button for it is
/// raised by the `m-scrolled` body class and hidden again when this lands.
#[wasm_bindgen]
pub fn seance_mobile_scroll_bottom() -> bool {
    with_mobile_app(|app| {
        let Some(pane) = app.scrolled_back_pane().or_else(|| app.focused_pane_pub()) else {
            return false;
        };
        AppActions(Rc::clone(app)).scroll_bottom(&pane);
        app.sync_scroll_chip();
        true
    })
    .unwrap_or(false)
}

/// Is the selected circle pinned to the top of the rail? `None` = nothing
/// selected, which is the phone chrome's cue to hide the control.
#[wasm_bindgen]
pub fn seance_mobile_is_pinned() -> Option<bool> {
    with_mobile_app(|app| {
        let ws = app.selected_workspace()?;
        Some(app.state.borrow().subs.is_pinned(&ws))
    })
    .flatten()
}

/// Pin or unpin the selected circle; returns the state it ended in.
///
/// The desktop reaches this by right-clicking a rail row — a gesture a finger
/// doesn't have, which is why the circles launched from the phone could never
/// be put back where he wanted them.
#[wasm_bindgen]
pub fn seance_mobile_set_pinned(pinned: bool) -> Option<bool> {
    with_mobile_app(|app| {
        let ws = app.selected_workspace()?;
        if pinned {
            app.pin_workspace(&ws);
        } else {
            app.unpin_workspace(&ws);
        }
        Some(app.state.borrow().subs.is_pinned(&ws))
    })
    .flatten()
}

/// Banish the selected circle — kill every pane in it — and land on a
/// neighbour. Returns the label of what was banished, for the toast.
///
/// The rail row's `×` is the desktop spelling and it is on the phone too, but
/// it is a 14px hover-sized target inside a row whose tap selects the circle;
/// this is the same verb with room to press it. The confirm step lives in the
/// chrome (arm, then fire), mirroring the row's two-click arm.
#[wasm_bindgen]
pub fn seance_mobile_banish_workspace() -> Option<String> {
    with_mobile_app(|app| {
        let ws = app.selected_workspace()?;
        let label = app.state.borrow().workspace_label(&ws);
        app.kill_workspace_selecting_neighbor(&ws);
        Some(label)
    })
    .flatten()
}

/// Rename the selected circle. The slug never moves (circle identity is the
/// slug, `engine/workspaces.rs`), so this only changes the label the rail and
/// topbar show — the phone's reach for the desktop's double-click rename.
#[wasm_bindgen]
pub fn seance_mobile_rename_workspace(name: &str) -> bool {
    with_mobile_app(|app| {
        let name = name.trim();
        let Some(ws) = app.selected_workspace() else {
            return false;
        };
        if name.is_empty() {
            return false;
        }
        app.send(&GuiRequest::RenameWorkspace {
            old: ws,
            new: name.to_string(),
        });
        true
    })
    .unwrap_or(false)
}

/// Swipe target: step to the previous (-1) / next (+1) circle.
#[wasm_bindgen]
pub fn seance_mobile_cycle_workspace(delta: i32) -> bool {
    with_mobile_app(|app| {
        AppActions(Rc::clone(app)).cycle_workspace(delta);
        true
    })
    .unwrap_or(false)
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&format!("seance-web panic: {info}").into());
    }));

    // ── replay modes take over the page entirely ────────────────────────────
    // Published bundle: index.html sets window.__SEANCE_REPLAY__ = manifest url.
    if let Some(manifest_url) = replay_bundle_url() {
        boot_replay_player(manifest_url)?;
        return Ok(());
    }
    // Editor route: #replay-edit?workspace=W[&from_ms=..&to_ms=..]
    if let Some((ws, from, to)) = replay_edit_route() {
        let token = initial_token().unwrap_or_default();
        replay_edit::open(ws, token, from, to);
        return Ok(());
    }

    let app = App::new();
    MOBILE_APP.with(|m| *m.borrow_mut() = Some(Rc::clone(&app)));
    let actions: Rc<dyn Actions> = Rc::new(AppActions(Rc::clone(&app)));
    *app.chrome.borrow_mut() = Some(ui::Chrome::new(actions)?);

    // Global input listeners. #tiles is stable across chrome rebuilds
    // (delegation via data-slug), document catches keys and paste.
    let doc = document();
    {
        let a = Rc::clone(&app);
        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |ev| a.on_keydown(ev));
        doc.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }
    {
        let a = Rc::clone(&app);
        let cb = Closure::<dyn FnMut(web_sys::ClipboardEvent)>::new(move |ev| a.on_paste(ev));
        doc.add_event_listener_with_callback("paste", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }
    if let Some(tiles) = doc.get_element_by_id("tiles") {
        let a = Rc::clone(&app);
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev| a.on_mousedown(ev));
        tiles.add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref())?;
        cb.forget();
        let a = Rc::clone(&app);
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev| a.on_mousemove(ev));
        tiles.add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref())?;
        cb.forget();
        let a = Rc::clone(&app);
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev| a.on_mouseup(ev));
        tiles.add_event_listener_with_callback("mouseup", cb.as_ref().unchecked_ref())?;
        cb.forget();
        let a = Rc::clone(&app);
        let cb = Closure::<dyn FnMut(web_sys::WheelEvent)>::new(move |ev| a.on_wheel(ev));
        let opts = web_sys::AddEventListenerOptions::new();
        opts.set_passive(false);
        tiles.add_event_listener_with_callback_and_add_event_listener_options(
            "wheel",
            cb.as_ref().unchecked_ref(),
            &opts,
        )?;
        cb.forget();
    }

    // Whoever the human is actually looking at owns the PTY geometry.
    // Becoming visible (tab switch, unlocking the phone) or regaining window
    // focus re-states our grid; an unfocused client never does, so the
    // desktop and the phone hand the pane back and forth instead of
    // thrashing it between two sizes.
    {
        let a = Rc::clone(&app);
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            if document().visibility_state() == web_sys::VisibilityState::Visible {
                a.reassert_grid();
            }
        });
        doc.add_event_listener_with_callback("visibilitychange", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }
    {
        let a = Rc::clone(&app);
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| a.reassert_grid());
        window().add_event_listener_with_callback("focus", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }
    // iOS Safari reflows the viewport after the orientation event, not with
    // it; the frame loop catches the new size, this just drops the latch so
    // the daemon hears about it.
    {
        let a = Rc::clone(&app);
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| a.reassert_grid());
        window()
            .add_event_listener_with_callback("orientationchange", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // Boot: token → connect, else login.
    match initial_token() {
        Some(tok) => app.connect(&tok),
        None => {
            let a = Rc::clone(&app);
            if let Some(ch) = app.chrome.borrow_mut().as_mut() {
                ch.show_login(Box::new(move |tok| {
                    let _ = storage_set("seance_token", &tok);
                    a.connect(&tok);
                }));
            }
        }
    }

    // rAF loop.
    let raf: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let raf2 = Rc::clone(&raf);
    let a = Rc::clone(&app);
    *raf.borrow_mut() = Some(Closure::new(move || {
        a.frame();
        if let Some(cb) = raf2.borrow().as_ref() {
            let _ = window().request_animation_frame(cb.as_ref().unchecked_ref());
        }
    }));
    window().request_animation_frame(raf.borrow().as_ref().unwrap().as_ref().unchecked_ref())?;
    // The rAF closure cycle keeps itself alive for the page lifetime.
    std::mem::forget(raf);

    Ok(())
}

// ── replay boot helpers ─────────────────────────────────────────────────────

/// Published-bundle marker: `window.__SEANCE_REPLAY__ = "recording/manifest.json"`.
fn replay_bundle_url() -> Option<String> {
    let win = web_sys::window()?;
    js_sys::Reflect::get(&win, &JsValue::from_str("__SEANCE_REPLAY__"))
        .ok()?
        .as_string()
}

fn boot_replay_player(manifest_url: String) -> Result<(), JsValue> {
    let doc = document();
    // The player owns the page: hide the app shell, mount into a fresh root.
    if let Some(app) = doc.get_element_by_id("app") {
        if let Some(el) = app.dyn_ref::<web_sys::HtmlElement>() {
            let _ = el.style().set_property("display", "none");
        }
    }
    let mount = doc.create_element("div")?;
    mount.set_id("replay-root");
    if let Some(body) = doc.body() {
        body.append_child(&mount)?;
    }
    let _player = replay::Player::create(
        mount,
        replay::Source::Bundle { manifest_url },
        Box::new(|res| {
            if let Err(e) = res {
                web_sys::console::error_1(&format!("replay load failed: {e}").into());
            }
        }),
    );
    // Keep the player alive for the page lifetime.
    std::mem::forget(_player);
    Ok(())
}

/// Parse `#replay-edit?workspace=W&from_ms=..&to_ms=..` from the location hash.
fn replay_edit_route() -> Option<(String, Option<u64>, Option<u64>)> {
    let hash = window().location().hash().ok()?;
    let rest = hash.strip_prefix("#replay-edit")?;
    let query = rest.strip_prefix('?').unwrap_or("");
    let mut ws = None;
    let mut from = None;
    let mut to = None;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        match k {
            "workspace" => ws = Some(v.to_string()),
            "from_ms" => from = v.parse().ok(),
            "to_ms" => to = v.parse().ok(),
            _ => {}
        }
    }
    ws.map(|w| (w, from, to))
}
