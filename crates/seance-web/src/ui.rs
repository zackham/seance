// NEEDS web-sys feature: console  (only for the error-path `console::warn_2`;
// every other API used here is covered by the features already in Cargo.toml —
// Node/EventTarget come in transitively via Element/Document.)

//! Chrome: everything that is not the terminal grid itself.
//!
//! [`Chrome`] owns the DOM outside the canvases — topbar, sidebar, tile frames,
//! asks banner, toasts, login gate — and translates every click into a call on
//! [`Actions`]. It never reads transport and never touches canvas *pixels*: it
//! creates the `<canvas>` elements and hands their identity to the renderer via
//! `id="canvas-{slug}"` / `data-slug="{slug}"`, then leaves them alone on every
//! subsequent in-place update.
//!
//! # Two update paths
//!
//! * [`Chrome::rebuild`] — structural: sidebar, tile set, grid template. This
//!   *destroys and recreates* canvases, so the app must re-acquire its contexts
//!   afterwards. Call it on [`crate::state::Applied::Structure`] only.
//! * [`Chrome::update_badges`] — status dots, titles, origin badges, ghost
//!   chips, focus ring, asks. Touches nothing else; canvases survive. Call it
//!   on [`crate::state::Applied::Badges`] (and freely — it is cheap).
//!
//! # Closure lifetime policy
//!
//! Rust closures handed to the DOM must outlive the listener. This module
//! *stores* them (no `forget()`, so nothing leaks across rebuilds) in three
//! buckets, each cleared exactly when the nodes it belongs to are dropped:
//!
//! * `structural` — topbar / sidebar / tile listeners, cleared at each
//!   `rebuild`.
//! * `ask_clicks` + `ask_keys` — asks banner listeners, cleared at each
//!   `render_asks` (which runs inside both update paths).
//! * `login` — login-card listeners, cleared by `hide_login`.
//!
//! Because ghost chips and status dots can appear *between* rebuilds, their
//! elements (and listeners) are created up-front in `rebuild` and merely shown
//! or hidden by `update_badges` — that keeps every listener rebuild-scoped.
//!
//! Fire-and-forget timers (toast fade, kill-confirm disarm) use
//! `Closure::once_into_js`, which hands ownership to the JS side and cannot
//! dangle.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wasm_bindgen::prelude::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Document, Element, HtmlInputElement, KeyboardEvent, MouseEvent, Window};

use crate::app_api::Actions;
use crate::state::ClientState;

type ClickClosure = Closure<dyn FnMut(MouseEvent)>;
type KeyClosure = Closure<dyn FnMut(KeyboardEvent)>;

/// How long a kill × stays armed after the first click.
const KILL_CONFIRM_MS: f64 = 2000.0;
/// Toast lifetime before the fade-out starts.
const TOAST_MS: i32 = 5000;

/// Handles to the mutable bits of one tile, so `update_badges` can patch in
/// place without selector escaping (slugs are not guaranteed CSS-safe).
struct TileRefs {
    tile: Element,
    dot: Element,
    title: Element,
    badge: Element,
    ghost: Element,
}

/// Handles to the mutable bits of one sidebar pane row.
struct RowRefs {
    row: Element,
    dot: Element,
    badge: Element,
}

pub struct Chrome {
    win: Window,
    doc: Document,
    topbar: Element,
    sidebar: Element,
    tiles: Element,
    toasts: Element,
    asks: Element,

    actions: Rc<dyn Actions>,

    tile_refs: HashMap<String, TileRefs>,
    row_refs: HashMap<String, RowRefs>,

    /// Rebuild-scoped listeners (topbar, sidebar, tiles).
    structural: Vec<ClickClosure>,
    /// Asks-banner listeners, cleared on every asks re-render.
    ask_clicks: Vec<ClickClosure>,
    ask_keys: Vec<KeyClosure>,
    /// Login-card listeners, cleared by `hide_login`.
    login: Vec<ClickClosure>,
    login_keys: Vec<KeyClosure>,

    /// Slug of the last tile the human clicked (chrome's own notion; the
    /// authoritative focus is app-owned and arrives back as `focused_pane`).
    focused: Option<String>,
    /// Armed kill buttons: slug -> arm timestamp (ms).
    kill_armed: Rc<RefCell<HashMap<String, f64>>>,
    /// Is the spawn form open?
    spawn_open: Rc<RefCell<bool>>,
    /// Last connection status, re-applied on every rebuild.
    conn: (String, bool),
}

impl Chrome {
    pub fn new(actions: Rc<dyn Actions>) -> Result<Chrome, JsValue> {
        let win = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
        let doc = win
            .document()
            .ok_or_else(|| JsValue::from_str("no document"))?;

        let topbar = need(&doc, "topbar")?;
        let sidebar = need(&doc, "sidebar")?;
        let tiles = need(&doc, "tiles")?;
        let toasts = need(&doc, "toasts")?;
        // The asks banner is optional in the shell; synthesise it if absent so
        // an older index.html still works.
        let asks = match doc.get_element_by_id("asks") {
            Some(el) => el,
            None => {
                let el = doc.create_element("div")?;
                el.set_id("asks");
                let parent = doc
                    .get_element_by_id("app")
                    .ok_or_else(|| JsValue::from_str("missing #app"))?;
                parent.append_child(&el)?;
                el
            }
        };

        Ok(Chrome {
            win,
            doc,
            topbar,
            sidebar,
            tiles,
            toasts,
            asks,
            actions,
            tile_refs: HashMap::new(),
            row_refs: HashMap::new(),
            structural: Vec::new(),
            ask_clicks: Vec::new(),
            ask_keys: Vec::new(),
            login: Vec::new(),
            login_keys: Vec::new(),
            focused: None,
            kill_armed: Rc::new(RefCell::new(HashMap::new())),
            spawn_open: Rc::new(RefCell::new(false)),
            conn: ("connecting".to_string(), false),
        })
    }

    // ── structural rebuild ──────────────────────────────────────────────

    /// Full DOM rebuild of topbar, sidebar and tiles. Destroys canvases.
    pub fn rebuild(&mut self, state: &ClientState) {
        if let Err(e) = self.rebuild_inner(state) {
            log("chrome: rebuild failed", &e);
        }
    }

    fn rebuild_inner(&mut self, state: &ClientState) -> Result<(), JsValue> {
        // Dropping the old listeners before dropping their nodes.
        self.structural.clear();
        self.tile_refs.clear();
        self.row_refs.clear();
        self.focused = state.focused_pane.clone();

        let workspaces = state.workspaces();
        let selected = state
            .selected_workspace
            .clone()
            .or_else(|| workspaces.first().cloned());

        self.build_topbar(selected.as_deref())?;
        self.build_sidebar(state, &workspaces, selected.as_deref())?;
        self.build_tiles(state, selected.as_deref())?;
        self.render_asks(state)?;
        self.apply_badges(state);
        Ok(())
    }

    fn build_topbar(&mut self, selected: Option<&str>) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        self.topbar.set_inner_html("");

        let title = text_el(&doc, "div", "tb-title", selected.unwrap_or("no workspace"))?;
        self.topbar.append_child(&title)?;
        self.topbar
            .append_child(text_el(&doc, "div", "tb-sub", "seance")?.unchecked_ref())?;
        self.topbar
            .append_child(mk(&doc, "div", "tb-spacer")?.unchecked_ref())?;

        // connection indicator
        let conn = mk(&doc, "div", "tb-conn")?;
        let conn_dot = mk(&doc, "span", "dot")?;
        conn_dot.set_id("conn-dot");
        let conn_label = mk(&doc, "span", "")?;
        conn_label.set_id("conn-label");
        conn.append_child(&conn_dot)?;
        conn.append_child(&conn_label)?;
        self.topbar.append_child(&conn)?;

        // + pane
        let spawn_btn = text_el(&doc, "button", "tb-btn", "+ pane")?;
        self.topbar.append_child(&spawn_btn)?;

        // probe
        let probe_btn = text_el(&doc, "button", "tb-btn", "probe")?;
        self.topbar.append_child(&probe_btn)?;
        {
            let actions = self.actions.clone();
            bind_click(&probe_btn, &mut self.structural, move |_| {
                actions.toggle_probe()
            })?;
        }

        // spawn form (created hidden; toggled by the + pane button)
        let form = mk(&doc, "div", "hidden")?;
        form.set_id("spawn-form");
        let name_in = input_el(&doc, "text", "name (required)")?;
        let cmd_in = input_el(&doc, "text", "command (optional)")?;
        let cwd_in = input_el(&doc, "text", "cwd (optional)")?;
        form.append_child(text_el(&doc, "div", "field-label", "summon a pane")?.unchecked_ref())?;
        form.append_child(&name_in)?;
        form.append_child(&cmd_in)?;
        form.append_child(&cwd_in)?;
        let row = mk(&doc, "div", "spawn-row")?;
        let cancel = text_el(&doc, "button", "", "cancel")?;
        let go = text_el(&doc, "button", "btn-primary", "spawn")?;
        row.append_child(&cancel)?;
        row.append_child(&go)?;
        form.append_child(&row)?;
        self.topbar.append_child(&form)?;

        {
            let form_el = form.clone();
            let open = self.spawn_open.clone();
            let name = name_in.clone();
            bind_click(&spawn_btn, &mut self.structural, move |_| {
                let next = !*open.borrow();
                *open.borrow_mut() = next;
                form_el.set_class_name(if next { "" } else { "hidden" });
                if next {
                    let _ = name.focus();
                }
            })?;
        }
        {
            let form_el = form.clone();
            let open = self.spawn_open.clone();
            bind_click(&cancel, &mut self.structural, move |_| {
                *open.borrow_mut() = false;
                form_el.set_class_name("hidden");
            })?;
        }
        {
            let actions = self.actions.clone();
            let form_el = form.clone();
            let open = self.spawn_open.clone();
            let ws = selected.map(|s| s.to_string());
            let (n, c, d) = (name_in.clone(), cmd_in.clone(), cwd_in.clone());
            bind_click(&go, &mut self.structural, move |_| {
                let name = n.value().trim().to_string();
                if name.is_empty() {
                    let _ = n.focus();
                    return;
                }
                actions.spawn_pane(&name, opt(d.value()), opt(c.value()), ws.clone());
                n.set_value("");
                c.set_value("");
                d.set_value("");
                *open.borrow_mut() = false;
                form_el.set_class_name("hidden");
            })?;
        }

        // Re-apply the last known connection status to the fresh nodes.
        let (label, ok) = self.conn.clone();
        self.paint_conn(&label, ok);
        Ok(())
    }

    fn build_sidebar(
        &mut self,
        state: &ClientState,
        workspaces: &[String],
        selected: Option<&str>,
    ) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        self.sidebar.set_inner_html("");

        if workspaces.is_empty() {
            let empty = mk(&doc, "div", "empty")?;
            empty.append_child(text_el(&doc, "div", "", "no workspaces here")?.unchecked_ref())?;
            if state.foreign_workspaces.is_empty() {
                let hint = mk(&doc, "div", "empty-hint")?;
                hint.append_child(text_el(&doc, "span", "", "run ")?.unchecked_ref())?;
                hint.append_child(text_el(&doc, "code", "", "seance ctl new")?.unchecked_ref())?;
                empty.append_child(&hint)?;
            }
            self.sidebar.append_child(&empty)?;
            self.build_foreign(state)?;
            return Ok(());
        }

        self.sidebar
            .append_child(text_el(&doc, "div", "side-head", "workspaces")?.unchecked_ref())?;

        for ws in workspaces {
            let count = state.panes_in(ws).len();
            let class = if Some(ws.as_str()) == selected {
                "ws-row selected"
            } else {
                "ws-row"
            };
            let row = mk(&doc, "div", class)?;
            row.append_child(text_el(&doc, "span", "row-name", ws)?.unchecked_ref())?;
            row.append_child(text_el(&doc, "span", "row-count", &count.to_string())?.unchecked_ref())?;
            self.sidebar.append_child(&row)?;

            let actions = self.actions.clone();
            let name = ws.clone();
            bind_click(&row, &mut self.structural, move |_| {
                actions.select_workspace(&name)
            })?;
        }

        self.build_foreign(state)?;

        let Some(ws) = selected else { return Ok(()) };
        self.sidebar.append_child(mk(&doc, "div", "side-sep")?.unchecked_ref())?;
        self.sidebar
            .append_child(text_el(&doc, "div", "side-head", "panes")?.unchecked_ref())?;

        let panes = state.panes_in(ws);
        if panes.is_empty() {
            let empty = mk(&doc, "div", "empty")?;
            empty.append_child(text_el(&doc, "div", "", "no panes")?.unchecked_ref())?;
            self.sidebar.append_child(&empty)?;
            return Ok(());
        }

        let list = mk(&doc, "div", "side-panes")?;
        for pane in panes {
            let row = mk(&doc, "div", "pane-row")?;
            let dot = mk(&doc, "span", "dot")?;
            let badge = mk(&doc, "span", "badge")?;
            row.append_child(&dot)?;
            row.append_child(text_el(&doc, "span", "row-name", &pane.name)?.unchecked_ref())?;
            row.append_child(&badge)?;
            list.append_child(&row)?;

            {
                let actions = self.actions.clone();
                let slug = pane.slug.clone();
                bind_click(&row, &mut self.structural, move |_| actions.focus_pane(&slug))?;
            }
            self.row_refs
                .insert(pane.slug.clone(), RowRefs { row, dot, badge });
        }
        self.sidebar.append_child(&list)?;
        Ok(())
    }

    /// Workspaces living on other windows (the native GUI, usually): list them
    /// with a pull affordance — TransferWorkspace moves ownership here. This
    /// is THE affordance for "seance from anywhere": attach from a browser,
    /// pull the workspace you need, work, and the daemon keeps it durable.
    fn build_foreign(&mut self, state: &ClientState) -> Result<(), JsValue> {
        if state.foreign_workspaces.is_empty() {
            return Ok(());
        }
        let doc = self.doc.clone();
        self.sidebar.append_child(mk(&doc, "div", "side-sep")?.unchecked_ref())?;
        self.sidebar
            .append_child(text_el(&doc, "div", "side-head", "elsewhere")?.unchecked_ref())?;
        let Some(my_window) = state.window_id.clone() else {
            return Ok(());
        };
        for fw in &state.foreign_workspaces {
            let row = mk(&doc, "div", "ws-row foreign")?;
            row.append_child(text_el(&doc, "span", "row-name", &fw.workspace)?.unchecked_ref())?;
            row.append_child(
                text_el(&doc, "span", "row-count", &fw.window_label)?.unchecked_ref(),
            )?;
            let pull = text_el(&doc, "button", "pull-btn", "pull")?;
            row.append_child(pull.unchecked_ref())?;
            self.sidebar.append_child(&row)?;

            let actions = self.actions.clone();
            let ws = fw.workspace.clone();
            let to_window = my_window.clone();
            bind_click(&pull, &mut self.structural, move |ev| {
                ev.stop_propagation();
                actions.send(seance_core::protocol::GuiRequest::TransferWorkspace {
                    workspace: ws.clone(),
                    to_window: to_window.clone(),
                });
            })?;
        }
        Ok(())
    }

    fn build_tiles(&mut self, state: &ClientState, selected: Option<&str>) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        self.tiles.set_inner_html("");

        let panes: Vec<_> = match selected {
            Some(ws) => state
                .panes_in(ws)
                .into_iter()
                .filter(|p| p.tiled)
                .collect(),
            None => Vec::new(),
        };

        if panes.is_empty() {
            self.tiles
                .set_attribute("style", "grid-template-columns:1fr;grid-template-rows:1fr")?;
            let empty = mk(&doc, "div", "empty")?;
            let msg = if selected.is_some() {
                "no panes — spawn one"
            } else {
                "no workspaces"
            };
            empty.append_child(text_el(&doc, "div", "", msg)?.unchecked_ref())?;
            if selected.is_none() {
                let hint = mk(&doc, "div", "empty-hint")?;
                hint.append_child(text_el(&doc, "span", "", "run ")?.unchecked_ref())?;
                hint.append_child(text_el(&doc, "code", "", "seance ctl new")?.unchecked_ref())?;
                empty.append_child(&hint)?;
            }
            self.tiles.append_child(&empty)?;
            return Ok(());
        }

        // Near-square grid: cols = ceil(sqrt(n)), rows = ceil(n / cols).
        let n = panes.len();
        let cols = (n as f64).sqrt().ceil().max(1.0) as usize;
        let rows = n.div_ceil(cols);
        self.tiles.set_attribute(
            "style",
            &format!(
                "grid-template-columns:repeat({cols},minmax(0,1fr));\
                 grid-template-rows:repeat({rows},minmax(0,1fr))"
            ),
        )?;

        for pane in panes {
            let slug = pane.slug.clone();
            let tile = mk(&doc, "div", "tile")?;
            tile.set_attribute("data-slug", &slug)?;

            let header = mk(&doc, "div", "tile-header")?;
            let dot = mk(&doc, "span", "dot")?;
            let name = text_el(&doc, "span", "tile-name", &pane.name)?;
            let title = text_el(&doc, "span", "tile-title", "")?;
            let badge = mk(&doc, "span", "badge")?;

            // Ghost chip: built once here, shown/hidden by update_badges so its
            // listeners stay rebuild-scoped.
            let ghost = mk(&doc, "span", "ghost-chip hidden")?;
            ghost.append_child(text_el(&doc, "span", "", "ghost")?.unchecked_ref())?;
            let accept = text_el(&doc, "button", "", "✓")?;
            let reject = text_el(&doc, "button", "", "✗")?;
            ghost.append_child(&accept)?;
            ghost.append_child(&reject)?;

            let kill = text_el(&doc, "button", "kill", "×")?;

            header.append_child(&dot)?;
            header.append_child(&name)?;
            header.append_child(&title)?;
            header.append_child(&badge)?;
            header.append_child(&ghost)?;
            header.append_child(&kill)?;

            let body = mk(&doc, "div", "tile-body")?;
            let canvas = doc.create_element("canvas")?;
            canvas.set_id(&format!("canvas-{slug}"));
            canvas.set_attribute("data-slug", &slug)?;
            body.append_child(&canvas)?;

            tile.append_child(&header)?;
            tile.append_child(&body)?;
            self.tiles.append_child(&tile)?;

            // focus: header or canvas
            for target in [&header, &canvas] {
                let actions = self.actions.clone();
                let s = slug.clone();
                bind_click(target, &mut self.structural, move |_| actions.focus_pane(&s))?;
            }

            {
                let actions = self.actions.clone();
                let s = slug.clone();
                bind_click(&accept, &mut self.structural, move |e| {
                    e.stop_propagation();
                    actions.ghost_accept(&s);
                })?;
            }
            {
                let actions = self.actions.clone();
                let s = slug.clone();
                bind_click(&reject, &mut self.structural, move |e| {
                    e.stop_propagation();
                    actions.ghost_reject(&s);
                })?;
            }

            // kill × — first click arms (danger), second within 2s kills.
            {
                let actions = self.actions.clone();
                let armed = self.kill_armed.clone();
                let win = self.win.clone();
                let btn = kill.clone();
                let s = slug.clone();
                bind_click(&kill, &mut self.structural, move |e| {
                    e.stop_propagation();
                    let now = js_sys::Date::now();
                    let previously = armed.borrow().get(&s).copied();
                    match previously {
                        Some(t) if now - t <= KILL_CONFIRM_MS => {
                            armed.borrow_mut().remove(&s);
                            btn.set_class_name("kill");
                            actions.kill_pane(&s);
                        }
                        _ => {
                            armed.borrow_mut().insert(s.clone(), now);
                            btn.set_class_name("kill armed");
                            // Disarm visually when the window closes.
                            let armed2 = armed.clone();
                            let btn2 = btn.clone();
                            let s2 = s.clone();
                            let cb = Closure::once_into_js(move || {
                                let stale = armed2
                                    .borrow()
                                    .get(&s2)
                                    .map(|t| js_sys::Date::now() - *t >= KILL_CONFIRM_MS)
                                    .unwrap_or(false);
                                if stale {
                                    armed2.borrow_mut().remove(&s2);
                                    btn2.set_class_name("kill");
                                }
                            });
                            let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(
                                cb.unchecked_ref(),
                                KILL_CONFIRM_MS as i32 + 50,
                            );
                        }
                    }
                })?;
            }

            self.tile_refs.insert(
                slug,
                TileRefs {
                    tile,
                    dot,
                    title,
                    badge,
                    ghost,
                },
            );
        }
        Ok(())
    }

    // ── in-place updates ────────────────────────────────────────────────

    /// Patch status dots, titles, origin badges, ghost chips, focus ring and
    /// the asks banner. Never touches canvases.
    /// Inline-rename hooks (parity build replaces these with real inputs).
    pub fn begin_rename_workspace(&mut self, _ws: &str) {}
    pub fn begin_rename_pane(&mut self, _slug: &str) {}

    pub fn update_badges(&mut self, state: &ClientState) {
        self.apply_badges(state);
        if let Err(e) = self.render_asks(state) {
            log("chrome: asks render failed", &e);
        }
    }

    fn apply_badges(&mut self, state: &ClientState) {
        let focused = state.focused_pane.clone().or_else(|| self.focused.clone());

        for (slug, refs) in &self.tile_refs {
            let Some(pane) = state.pane(slug) else { continue };
            let status = state.statuses.get(slug).map(|s| s.state.as_str());
            let exited = pane.exited
                || state.agency.get(slug).map(|a| a.exited).unwrap_or(false)
                || !pane.running;
            refs.dot.set_class_name(&format!(
                "dot {}",
                dot_class(status, exited)
            ));

            let note = state.statuses.get(slug).and_then(|s| s.note.clone());
            let title = pane.title.clone().or(note).unwrap_or_default();
            refs.title.set_text_content(Some(&title));

            let (badge, badge_class) = origin_badge(state, pane);
            refs.badge.set_text_content(Some(&badge));
            refs.badge.set_class_name(badge_class);

            let has_ghost = state
                .grids
                .get(slug)
                .map(|g| g.ghost.is_some())
                .unwrap_or(false);
            refs.ghost.set_class_name(if has_ghost {
                "ghost-chip"
            } else {
                "ghost-chip hidden"
            });

            let mut class = String::from("tile");
            if focused.as_deref() == Some(slug.as_str()) {
                class.push_str(" focused");
            }
            if exited {
                class.push_str(" exited");
            }
            refs.tile.set_class_name(&class);
        }

        for (slug, refs) in &self.row_refs {
            let Some(pane) = state.pane(slug) else { continue };
            let status = state.statuses.get(slug).map(|s| s.state.as_str());
            let exited = pane.exited
                || state.agency.get(slug).map(|a| a.exited).unwrap_or(false)
                || !pane.running;
            refs.dot
                .set_class_name(&format!("dot {}", dot_class(status, exited)));
            let (badge, badge_class) = origin_badge(state, pane);
            refs.badge.set_text_content(Some(&badge));
            refs.badge.set_class_name(badge_class);
            refs.row.set_class_name(if focused.as_deref() == Some(slug.as_str()) {
                "pane-row focused"
            } else {
                "pane-row"
            });
        }
    }

    fn render_asks(&mut self, state: &ClientState) -> Result<(), JsValue> {
        self.ask_clicks.clear();
        self.ask_keys.clear();
        self.asks.set_inner_html("");
        let doc = self.doc.clone();

        let ws = state.selected_workspace.clone();
        for ask in state.asks.iter().filter(|a| a.answer.is_none()) {
            // Workspace-scoped asks only show in their workspace; global asks
            // (workspace = None) always show.
            if let (Some(a_ws), Some(sel)) = (ask.workspace.as_ref(), ws.as_ref()) {
                if a_ws != sel {
                    continue;
                }
            }
            let row = mk(&doc, "div", "ask")?;
            row.append_child(text_el(&doc, "span", "ask-from", &ask.from)?.unchecked_ref())?;
            row.append_child(text_el(&doc, "span", "ask-q", &ask.question)?.unchecked_ref())?;

            for choice in &ask.choices {
                let btn = text_el(&doc, "button", "ask-choice", choice)?;
                row.append_child(&btn)?;
                let actions = self.actions.clone();
                let (id, answer) = (ask.id.clone(), choice.clone());
                bind_click(&btn, &mut self.ask_clicks, move |_| {
                    actions.answer_ask(&id, &answer)
                })?;
            }

            let free = input_el(&doc, "text", "answer…")?;
            free.set_class_name("ask-free");
            row.append_child(&free)?;
            let send = text_el(&doc, "button", "", "send")?;
            row.append_child(&send)?;
            {
                let actions = self.actions.clone();
                let id = ask.id.clone();
                let input = free.clone();
                bind_click(&send, &mut self.ask_clicks, move |_| {
                    let v = input.value();
                    if !v.trim().is_empty() {
                        actions.answer_ask(&id, v.trim());
                        input.set_value("");
                    }
                })?;
            }
            {
                let actions = self.actions.clone();
                let id = ask.id.clone();
                let input = free.clone();
                bind_key(&free, &mut self.ask_keys, move |e| {
                    if e.key() == "Enter" {
                        let v = input.value();
                        if !v.trim().is_empty() {
                            actions.answer_ask(&id, v.trim());
                            input.set_value("");
                        }
                    }
                })?;
            }

            self.asks.append_child(&row)?;
        }
        Ok(())
    }

    // ── connection status ───────────────────────────────────────────────

    pub fn set_conn_status(&mut self, label: &str, ok: bool) {
        self.conn = (label.to_string(), ok);
        self.paint_conn(label, ok);
    }

    fn paint_conn(&self, label: &str, ok: bool) {
        if let Some(dot) = self.doc.get_element_by_id("conn-dot") {
            dot.set_class_name(if ok { "dot ok" } else { "dot bad" });
        }
        if let Some(el) = self.doc.get_element_by_id("conn-label") {
            el.set_text_content(Some(label));
        }
    }

    // ── toasts ──────────────────────────────────────────────────────────

    /// Transient message, bottom-center. Errors (heuristic on the text) get the
    /// danger tint. Fades and removes itself after 5s.
    pub fn toast(&mut self, message: &str) {
        if let Err(e) = self.toast_inner(message) {
            log("chrome: toast failed", &e);
        }
    }

    fn toast_inner(&mut self, message: &str) -> Result<(), JsValue> {
        let lower = message.to_lowercase();
        let is_err = ["error", "failed", "fail:", "disconnect", "refused", "denied"]
            .iter()
            .any(|k| lower.contains(k));
        let el = text_el(
            &self.doc,
            "div",
            if is_err { "toast error" } else { "toast" },
            message,
        )?;
        self.toasts.append_child(&el)?;

        let fade = el.clone();
        let cb = Closure::once_into_js(move || {
            let cls = fade.class_name();
            fade.set_class_name(&format!("{cls} fading"));
            let gone = fade.clone();
            let cb2 = Closure::once_into_js(move || gone.remove());
            if let Some(w) = web_sys::window() {
                let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(
                    cb2.unchecked_ref(),
                    450,
                );
            }
        });
        self.win
            .set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), TOAST_MS)?;
        Ok(())
    }

    // ── login gate ──────────────────────────────────────────────────────

    /// Show the token gate. `on_submit` receives the typed token on button
    /// click or Enter; the caller decides whether to `hide_login`.
    pub fn show_login(&mut self, on_submit: Box<dyn FnMut(String)>) {
        if let Err(e) = self.show_login_inner(on_submit) {
            log("chrome: login failed", &e);
        }
    }

    fn show_login_inner(&mut self, on_submit: Box<dyn FnMut(String)>) -> Result<(), JsValue> {
        self.login.clear();
        self.login_keys.clear();

        let root = need(&self.doc, "login")?;
        root.remove_attribute("hidden")?;
        let input: HtmlInputElement = need(&self.doc, "login-token")?
            .dyn_into()
            .map_err(|_| JsValue::from_str("#login-token is not an input"))?;
        let button = need(&self.doc, "login-join")?;

        let sink = Rc::new(RefCell::new(on_submit));
        {
            let sink = sink.clone();
            let input = input.clone();
            bind_click(&button, &mut self.login, move |_| {
                let v = input.value();
                if !v.is_empty() {
                    (sink.borrow_mut())(v);
                }
            })?;
        }
        {
            let sink = sink.clone();
            let field = input.clone();
            bind_key(&input, &mut self.login_keys, move |e| {
                if e.key() == "Enter" {
                    let v = field.value();
                    if !v.is_empty() {
                        (sink.borrow_mut())(v);
                    }
                }
            })?;
        }
        let _ = input.focus();
        Ok(())
    }

    pub fn hide_login(&mut self) {
        self.login.clear();
        self.login_keys.clear();
        if let Some(root) = self.doc.get_element_by_id("login") {
            if let Err(e) = root.set_attribute("hidden", "") {
                log("chrome: hide_login failed", &e);
            }
        }
        if let Some(input) = self
            .doc
            .get_element_by_id("login-token")
            .and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
        {
            input.set_value("");
        }
    }

    // ── queries ─────────────────────────────────────────────────────────

    /// Slug of the tile the human last clicked. Advisory: real focus is
    /// app-owned (every click also calls `Actions::focus_pane`, and `rebuild`
    /// re-seeds this from `ClientState::focused_pane`).
    pub fn focused_canvas_slug(&self) -> Option<String> {
        self.focused.clone()
    }
}

// ── helpers ─────────────────────────────────────────────────────────────

fn need(doc: &Document, id: &str) -> Result<Element, JsValue> {
    doc.get_element_by_id(id)
        .ok_or_else(|| JsValue::from_str(&format!("missing #{id} in index.html")))
}

fn mk(doc: &Document, tag: &str, class: &str) -> Result<Element, JsValue> {
    let el = doc.create_element(tag)?;
    if !class.is_empty() {
        el.set_class_name(class);
    }
    Ok(el)
}

fn text_el(doc: &Document, tag: &str, class: &str, text: &str) -> Result<Element, JsValue> {
    let el = mk(doc, tag, class)?;
    el.set_text_content(Some(text));
    Ok(el)
}

fn input_el(doc: &Document, kind: &str, placeholder: &str) -> Result<HtmlInputElement, JsValue> {
    let el: HtmlInputElement = doc
        .create_element("input")?
        .dyn_into()
        .map_err(|_| JsValue::from_str("input element cast failed"))?;
    el.set_type(kind);
    el.set_placeholder(placeholder);
    Ok(el)
}

fn bind_click(
    target: &Element,
    sink: &mut Vec<ClickClosure>,
    mut f: impl FnMut(MouseEvent) + 'static,
) -> Result<(), JsValue> {
    let cb = Closure::wrap(Box::new(move |e: MouseEvent| f(e)) as Box<dyn FnMut(MouseEvent)>);
    target.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
    sink.push(cb);
    Ok(())
}

fn bind_key(
    target: &HtmlInputElement,
    sink: &mut Vec<KeyClosure>,
    mut f: impl FnMut(KeyboardEvent) + 'static,
) -> Result<(), JsValue> {
    let cb =
        Closure::wrap(Box::new(move |e: KeyboardEvent| f(e)) as Box<dyn FnMut(KeyboardEvent)>);
    target.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref())?;
    sink.push(cb);
    Ok(())
}

fn opt(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Status → dot class. Mirrors the native `status_color` mapping
/// (`src/app/util.rs`): blocked/risky → danger, needs-human → violet,
/// done → success, idle → dim success, anything else → working (flame pulse).
fn dot_class(status: Option<&str>, exited: bool) -> &'static str {
    if exited {
        return "exited";
    }
    match status {
        Some("blocked") | Some("risky") => "blocked",
        Some("needs-human") => "needs-human",
        Some("done") => "done",
        Some("idle") | None => "idle",
        _ => "working",
    }
}

/// Origin/owner badge text + class. Prefers who last wrote stdin
/// (`input_origin`), falling back to the agency owner then the pane owner.
fn origin_badge(
    state: &ClientState,
    pane: &seance_core::protocol::PaneInfo,
) -> (String, &'static str) {
    let text = state
        .input_origin
        .get(&pane.slug)
        .cloned()
        .or_else(|| {
            state
                .agency
                .get(&pane.slug)
                .map(|a| a.owner.clone())
                .filter(|o| !o.is_empty())
        })
        .or_else(|| pane.owner.clone())
        .filter(|o| !o.is_empty() && o != "none")
        .unwrap_or_default();
    let class = if text.starts_with("agent") || text == "propose" {
        "badge agent"
    } else if text == "human" {
        "badge human"
    } else {
        "badge"
    };
    (text, class)
}

fn log(what: &str, err: &JsValue) {
    web_sys::console::warn_2(&JsValue::from_str(what), err);
}
