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

use seance_core::input::{key_to_bytes, TermModes};
use seance_core::protocol::{GuiEvent, GuiRequest};

pub mod app_api;
pub mod conn;
pub mod input;
pub mod probe;
pub mod renderer;
pub mod state;
pub mod ui;

use app_api::Actions;
use conn::{Conn, ConnStatus};
use renderer::{RenderOpts, TermRenderer};
use state::{Applied, ClientState};

const FONT_FAMILY: &str = "ui-monospace, 'Cascadia Mono', 'CaskaydiaMono Nerd Font Mono', 'JetBrains Mono', monospace";
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
    window()
        .performance()
        .map(|p| p.now())
        .unwrap_or(0.0)
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
}

impl App {
    fn new() -> Rc<App> {
        Rc::new(App {
            state: RefCell::new(ClientState::default()),
            conn: RefCell::new(None),
            chrome: RefCell::new(None),
            probe: RefCell::new(probe::Probe::new()),
            views: RefCell::new(HashMap::new()),
            dirty_grids: RefCell::new(HashSet::new()),
            need_rebuild: Cell::new(false),
            badges_dirty: Cell::new(false),
            structure_rev_bound: Cell::new(0),
            selection: RefCell::new(None),
            wheel_accum: RefCell::new(HashMap::new()),
            input_hot_ms: RefCell::new(HashMap::new()),
            blink_t0: Cell::new(0.0),
            blink_last: Cell::new(true),
        })
    }

    fn send(&self, req: &GuiRequest) {
        if let Some(c) = self.conn.borrow().as_ref() {
            c.send(req);
        }
    }

    fn handle_event(&self, ev: GuiEvent) {
        let applied = self.state.borrow_mut().apply_event(ev);
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
            Applied::Error { message } => {
                if let Some(ch) = self.chrome.borrow_mut().as_mut() {
                    ch.toast(&message);
                }
            }
        }
    }

    /// Selected workspace, defaulting to the first known one.
    fn selected_workspace(&self) -> Option<String> {
        let st = self.state.borrow();
        st.selected_workspace
            .clone()
            .or_else(|| st.workspaces().first().cloned())
    }

    fn focused_pane(&self) -> Option<String> {
        self.state.borrow().focused_pane.clone()
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
        if self.need_rebuild.get() {
            self.need_rebuild.set(false);
            self.rebuild();
            self.badges_dirty.set(false);
        } else if self.badges_dirty.get() {
            self.badges_dirty.set(false);
            let st = self.state.borrow();
            if let Some(ch) = self.chrome.borrow_mut().as_mut() {
                ch.update_badges(&st);
            }
        }
        self.sync_sizes();
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
        if let Some(c) = self.conn.borrow().as_ref() {
            c.input(pane, bytes);
        }
    }

    fn on_keydown(self: &Rc<Self>, ev: web_sys::KeyboardEvent) {
        // Chrome shortcuts first.
        let (ctrl, shift, meta) = (ev.ctrl_key(), ev.shift_key(), ev.meta_key());
        let key = ev.key();
        if ctrl && shift && (key == "P" || key == "p") {
            self.toggle_probe();
            ev.prevent_default();
            return;
        }
        if ctrl && (key == "PageUp" || key == "PageDown") {
            self.cycle_workspace(if key == "PageUp" { -1 } else { 1 });
            ev.prevent_default();
            return;
        }
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

    fn cycle_workspace(self: &Rc<Self>, dir: i32) {
        let st = self.state.borrow();
        let wss = st.workspaces();
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
    fn spawn_pane(&self, name: &str, cwd: Option<String>, command: Option<String>, workspace: Option<String>) {
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
        self.0.send(&GuiRequest::RenamePane { pane: slug.into(), name: name.into() });
    }
    fn create_workspace(&self, name: &str) {
        self.0.send(&GuiRequest::CreateWorkspace { name: name.into() });
    }
    fn rename_workspace(&self, old: &str, new: &str) {
        self.0.send(&GuiRequest::RenameWorkspace { old: old.into(), new: new.into() });
    }
    fn kill_workspace(&self, ws: &str) {
        self.0.send(&GuiRequest::KillWorkspace { workspace: ws.into() });
    }
    fn answer_ask(&self, id: &str, answer: &str) {
        self.0.send(&GuiRequest::AnswerAsk { id: id.into(), answer: answer.into() });
    }
    fn inject(&self, pane: &str, text: &str, submit: bool) {
        self.0.send(&GuiRequest::Inject { pane: pane.into(), text: text.into(), submit });
    }
    fn input_bytes(&self, pane: &str, bytes: &[u8]) {
        self.0.pty_input(pane, bytes);
    }
    fn scroll(&self, pane: &str, delta: i32) {
        self.0.send(&GuiRequest::Scroll { pane: pane.into(), delta });
    }
    fn scroll_bottom(&self, pane: &str) {
        self.0.send(&GuiRequest::ScrollBottom { pane: pane.into() });
    }
    fn resize(&self, pane: &str, cols: u16, rows: u16) {
        self.0.send(&GuiRequest::Resize { pane: pane.into(), cols, rows });
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
            let tok: String = tail
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
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

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&format!("seance-web panic: {info}").into());
    }));

    let app = App::new();
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
    window().request_animation_frame(
        raf.borrow().as_ref().unwrap().as_ref().unchecked_ref(),
    )?;
    // The rAF closure cycle keeps itself alive for the page lifetime.
    std::mem::forget(raf);

    Ok(())
}
