// NEEDS web-sys feature: console  (only for the error-path `console::warn_2`;
// every other API used here is covered by the features already in Cargo.toml —
// Node/EventTarget/UiEvent arrive transitively via Element/Document/MouseEvent.)

//! Chrome: everything that is not the terminal grid itself.
//!
//! [`Chrome`] owns the DOM outside the canvases and translates every click into
//! a call on [`Actions`]. It never reads transport and never touches canvas
//! *pixels*: it creates the `<canvas>` elements and hands their identity to the
//! renderer via `id="canvas-{slug}"` / `data-slug="{slug}"`, then leaves them
//! alone on every subsequent in-place update.
//!
//! # Parity with the native GPUI chrome
//!
//! A deliberate replica of `src/app/sidebar.rs`, `src/app/quicklaunch.rs` and
//! the pane header in `src/app/chrome.rs`:
//!
//! * **sidebar** — 232px, `bg_elevated`, 44px brand header (`✦ seance` + `◈+`;
//!   `✦` toggles the GUI-census popover — window list, kill-other-window,
//!   version + grimoire),
//!   workspace rows only (glyph `◆` selected / `◈` idle / pulsing `◆` while
//!   working, `needs`/`done` text badges in their colors, hover `×` banish,
//!   pane count, full-bleed selected fill, double-click inline rename, per-row
//!   context menu: touch / rename / fork ⑂ / send to «window» / collect all /
//!   banish), empty-area context menu (collect all + pull «ws» from label),
//!   quicklaunch chip strip, host-accounts strip, footer (`+ summon` flex-1
//!   flame · activity `≋` · help `?` violet).
//! * **quicklaunch** — daemon-side `~/.config/seance/quicklaunch.json` over the
//!   fs bridge, 2s mtime-throttled hot reload, write-through on edit, parse
//!   error keeps the previous entries; a chip click opens a FRESH uniquified
//!   workspace named after the entry.
//! * **tiles** — pane header (status dot, name, TUI title, origin badge, ghost
//!   accept/reject chip, zoom, kill) and zoom mode behind the flame
//!   "⛶ zoomed" bar.
//!
//! # Deliberate web divergences (each flagged inline too)
//!
//! 1. **Braille title-spinner → CSS pulse.** The native sidebar cycles braille
//!    frames every 240ms. The DOM can't animate glyph *content* without a timer
//!    per frame, so a working circle keeps `◆` and gets `.working` (a CSS
//!    opacity pulse). Same signal, no rAF churn.
//! 2. **No drag-and-drop.** Canvas tiles aren't draggable and HTML5 DnD would
//!    need the `DragEvent` web-sys feature. Panes move via the pane menu
//!    ("move to → ws", the submenu flattened); quicklaunch chips reorder via
//!    right-click "move up"/"move down" (the native insert-before drop reduces
//!    to the same list op).
//! 3. **No window-targeted workspace moves.** Workspaces are global
//!    subscriptions (0.12) — nothing is "sent" anywhere; the ✦ census popover
//!    is the roster. The parked-group affordance lands in phase 2.
//! 4. **Topbar.** Native has no topbar; the web client needs a permanent
//!    connection indicator (native is always local) and the latency probe
//!    toggle, so a slim `#topbar` carries those plus the selected circle's name.
//! 5. **Two-click destructive buttons.** The tile `×` and the workspace `×` arm
//!    on the first click and fire on the second within 2s. Native fires
//!    immediately; a browser has no window-manager safety net and a stray click
//!    costs a live agent.
//! 6. **No shelve / pop-out / notes-flip / pad / phone / arm chips** on the pane
//!    header, and no per-pane sidebar rows — those native affordances need a
//!    second OS window, the scratchpad drawer or the telegram seam. Pane
//!    identity lives on the tile header, as it does natively.
//!
//! # Two update paths
//!
//! * [`Chrome::rebuild`] — structural: sidebar, tile set, grid template. This
//!   *destroys and recreates* canvases, so the app must re-acquire its contexts
//!   afterwards. Call it on [`crate::state::Applied::Structure`] only.
//! * [`Chrome::update_badges`] — status dots, titles, origin badges, ghost
//!   chips, focus ring, workspace attention, host strip, asks. Touches nothing
//!   else; canvases survive.
//!
//! # Closure lifetime policy
//!
//! Rust closures handed to the DOM must outlive the listener, so this module
//! *stores* them (no `forget()` — nothing leaks across rebuilds):
//!
//! * `structural` — topbar / sidebar / tile listeners, cleared at each
//!   `rebuild`.
//! * `rename.keys` — the inline rename input, cleared when a rename begins.
//! * `ask_clicks` + `ask_keys` — asks banner, cleared at each `render_asks`.
//! * `login` / `login_keys` — login card, cleared by `hide_login`.
//! * the quicklaunch + host strips re-render themselves from *inside* their own
//!   listeners (editing a chip must not force a full rebuild), so their
//!   closures live in `Rc<RefCell<Vec<…>>>` sinks owned by the strips.
//!
//! A sink that is replaced from inside one of its own closures must not drop
//! that closure while it is on the stack — [`drop_later`] hands the old vector
//! to a 0ms timeout so the free happens in a later task.
//!
//! Fire-and-forget timers (toast fade, kill-confirm disarm) use
//! `Closure::once_into_js`, which hands ownership to the JS side.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Document, Element, HtmlInputElement, KeyboardEvent, MouseEvent, Window};

use seance_core::protocol::{FsOp, GuiRequest, PaneInfo};

use crate::app_api::Actions;
use crate::menus::{close_menu, open_menu, MenuEntry};
use crate::state::{Attention, ClientState, HostWidget};

type ClickClosure = Closure<dyn FnMut(MouseEvent)>;
type KeyClosure = Closure<dyn FnMut(KeyboardEvent)>;
type ClickSink = Rc<RefCell<Vec<ClickClosure>>>;
type KeySink = Rc<RefCell<Vec<KeyClosure>>>;

/// How long a destructive × stays armed after the first click.
const KILL_CONFIRM_MS: f64 = 2000.0;
/// Toast lifetime before the fade-out starts.
const TOAST_MS: i32 = 5000;
/// Quicklaunch config on the DAEMON machine (tilde expands daemon-side, so a
/// thin client sees the same strip as the local GUI).
const QUICKLAUNCH_PATH: &str = "~/.config/seance/quicklaunch.json";
/// mtime poll throttle for the quicklaunch hot reload (native: 2s).
const QL_POLL_MS: f64 = 2000.0;

// ── quicklaunch model (mirrors src/app/quicklaunch.rs) ──────────────────────

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
struct QlEntry {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
}

/// True if `name` collides with an entry other than `original` (the entry being
/// edited; `None` for a fresh add).
fn ql_name_collides(entries: &[QlEntry], name: &str, original: Option<&str>) -> bool {
    entries
        .iter()
        .any(|e| e.name == name && Some(e.name.as_str()) != original)
}

/// Replace `original` in place (position preserved, name may change) or append.
fn ql_upsert(entries: &mut Vec<QlEntry>, original: Option<&str>, entry: QlEntry) {
    if let Some(orig) = original {
        if let Some(slot) = entries.iter_mut().find(|e| e.name == orig) {
            *slot = entry;
            return;
        }
    }
    entries.push(entry);
}

/// Shift one entry by `delta` positions, clamped. WEB DIVERGENCE #2: the web
/// stand-in for the native drag-reorder (insert-before) drop.
fn ql_shift(entries: &mut [QlEntry], name: &str, delta: isize) {
    let Some(from) = entries.iter().position(|e| e.name == name) else {
        return;
    };
    let to = from as isize + delta;
    if to < 0 || to as usize >= entries.len() {
        return;
    }
    entries.swap(from, to as usize);
}

/// Shared quicklaunch state — a cloneable handle so listeners can re-render the
/// strip without a full chrome rebuild.
#[derive(Clone)]
struct QuickLaunch {
    entries: Rc<RefCell<Vec<QlEntry>>>,
    mtime: Rc<Cell<Option<u64>>>,
    last_check: Rc<Cell<f64>>,
    inflight: Rc<Cell<bool>>,
    /// The chips container (re-rendered in place).
    chips: Rc<RefCell<Option<Element>>>,
    clicks: ClickSink,
    editor_clicks: ClickSink,
    editor_keys: KeySink,
}

impl QuickLaunch {
    fn new() -> Self {
        Self {
            entries: Rc::new(RefCell::new(Vec::new())),
            mtime: Rc::new(Cell::new(None)),
            last_check: Rc::new(Cell::new(f64::NEG_INFINITY)),
            inflight: Rc::new(Cell::new(false)),
            chips: Rc::new(RefCell::new(None)),
            clicks: Rc::new(RefCell::new(Vec::new())),
            editor_clicks: Rc::new(RefCell::new(Vec::new())),
            editor_keys: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

/// Shared host-accounts strip state (same reason: it re-renders itself).
#[derive(Clone)]
struct HostStrip {
    root: Rc<RefCell<Option<Element>>>,
    widgets: Rc<RefCell<Vec<HostWidget>>>,
    expanded: Rc<RefCell<HashSet<String>>>,
    clicks: ClickSink,
    /// Signature of the last adopted widget set (skip no-op renders).
    sig: Rc<RefCell<String>>,
}

impl HostStrip {
    fn new() -> Self {
        Self {
            root: Rc::new(RefCell::new(None)),
            widgets: Rc::new(RefCell::new(Vec::new())),
            expanded: Rc::new(RefCell::new(HashSet::new())),
            clicks: Rc::new(RefCell::new(Vec::new())),
            sig: Rc::new(RefCell::new(String::new())),
        }
    }
}

/// Everything the inline rename needs, cloneable into a listener.
#[derive(Clone)]
struct Rename {
    doc: Document,
    actions: Rc<dyn Actions>,
    keys: KeySink,
    /// The open input and the row/header content it replaced.
    open: Rc<RefCell<Option<(HtmlInputElement, Element)>>>,
}

enum RenameKind {
    Workspace(String),
    Pane(String),
}

/// Handles to the mutable bits of one tile.
struct TileRefs {
    tile: Element,
    dot: Element,
    name: Element,
    title: Element,
    badge: Element,
    ghost: Element,
    /// Header content wrapper — hidden while the rename input is up.
    main: Element,
    header: Element,
}

/// Handles to the mutable bits of one sidebar workspace row.
struct WsRefs {
    row: Element,
    /// Row content wrapper — hidden while the rename input is up.
    main: Element,
    glyph: Element,
    att: Element,
    count: Element,
    selected: bool,
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
    ws_refs: HashMap<String, WsRefs>,

    /// Rebuild-scoped listeners (topbar, sidebar, tiles).
    structural: Vec<ClickClosure>,
    /// Asks-banner listeners, cleared on every asks re-render.
    ask_clicks: Vec<ClickClosure>,
    ask_keys: Vec<KeyClosure>,
    /// Login-card listeners, cleared by `hide_login`.
    login: Vec<ClickClosure>,
    login_keys: Vec<KeyClosure>,

    rename: Rename,
    quicklaunch: QuickLaunch,
    host: HostStrip,

    /// `◈+` created this workspace — inline-rename it as soon as its row exists.
    /// Native renames immediately; the web has to wait one round trip for the
    /// daemon's State push.
    pending_ws_rename: Rc<RefCell<Option<String>>>,

    /// Slug of the last tile the human clicked (chrome's own notion; the
    /// authoritative focus is app-owned and arrives back as `focused_pane`).
    focused: Option<String>,
    /// Armed destructive buttons: key -> arm timestamp (ms).
    kill_armed: Rc<RefCell<HashMap<String, f64>>>,
    /// Last connection status, re-applied on every rebuild.
    conn: (String, bool),
    /// Last selected workspace we scrolled into view (avoid scroll-jacking on
    /// unrelated rebuilds).
    last_scrolled_ws: Option<String>,

    /// `✦` popover (GUI census) is open. Shared with its own listeners (they
    /// close it) and survives a rebuild — the card is re-rendered under the
    /// fresh brand header.
    gui_menu_open: Rc<Cell<bool>>,
    /// Popover-scoped listeners, replaced on every popover render.
    gui_menu_clicks: ClickSink,
    /// Document-level click-away dismisser, live only while the popover is up.
    gui_menu_dismiss: Rc<RefCell<Option<ClickClosure>>>,
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

        let rename = Rename {
            doc: doc.clone(),
            actions: actions.clone(),
            keys: Rc::new(RefCell::new(Vec::new())),
            open: Rc::new(RefCell::new(None)),
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
            ws_refs: HashMap::new(),
            structural: Vec::new(),
            ask_clicks: Vec::new(),
            ask_keys: Vec::new(),
            login: Vec::new(),
            login_keys: Vec::new(),
            rename,
            quicklaunch: QuickLaunch::new(),
            host: HostStrip::new(),
            pending_ws_rename: Rc::new(RefCell::new(None)),
            focused: None,
            kill_armed: Rc::new(RefCell::new(HashMap::new())),
            conn: ("connecting".to_string(), false),
            last_scrolled_ws: None,
            gui_menu_open: Rc::new(Cell::new(false)),
            gui_menu_clicks: Rc::new(RefCell::new(Vec::new())),
            gui_menu_dismiss: Rc::new(RefCell::new(None)),
        })
    }

    fn now_ms(&self) -> f64 {
        self.win.performance().map(|p| p.now()).unwrap_or(0.0)
    }

    // ── structural rebuild ──────────────────────────────────────────────

    /// Full DOM rebuild of topbar, sidebar and tiles. Destroys canvases.
    pub fn rebuild(&mut self, state: &ClientState) {
        if let Err(e) = self.rebuild_inner(state) {
            log("chrome: rebuild failed", &e);
        }
        // `◈+` names its circle immediately (native parity); the row only
        // exists once the daemon's State push lands, so it happens here.
        let pending = self.pending_ws_rename.borrow_mut().take();
        if let Some(ws) = pending {
            if self.ws_refs.contains_key(&ws) {
                self.begin_rename_workspace(&ws);
            } else {
                *self.pending_ws_rename.borrow_mut() = Some(ws);
            }
        }
    }

    fn rebuild_inner(&mut self, state: &ClientState) -> Result<(), JsValue> {
        // Drop the old listeners before dropping their nodes. `rebuild` runs
        // from the frame loop, never from inside a listener, so a direct clear
        // is safe here.
        let _ = close_menu();
        self.structural.clear();
        self.rename.keys.borrow_mut().clear();
        *self.rename.open.borrow_mut() = None;
        self.tile_refs.clear();
        self.ws_refs.clear();
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
        self.sync_host(state);
        self.maybe_reload_quicklaunch();
        Ok(())
    }

    /// Slim status strip. WEB DIVERGENCE #4: native has no topbar.
    fn build_topbar(&mut self, selected: Option<&str>) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        self.topbar.set_inner_html("");

        let title = text_el(&doc, "div", "tb-title", selected.unwrap_or("no workspace"))?;
        self.topbar.append_child(&title)?;
        self.topbar
            .append_child(mk(&doc, "div", "tb-spacer")?.unchecked_ref())?;

        let conn = mk(&doc, "div", "tb-conn")?;
        let conn_dot = mk(&doc, "span", "dot")?;
        conn_dot.set_id("conn-dot");
        let conn_label = mk(&doc, "span", "")?;
        conn_label.set_id("conn-label");
        conn.append_child(&conn_dot)?;
        conn.append_child(&conn_label)?;
        self.topbar.append_child(&conn)?;

        let probe_btn = text_el(&doc, "button", "tb-btn", "probe")?;
        probe_btn.set_attribute("title", "latency probe overlay (ctrl+shift+p)")?;
        self.topbar.append_child(&probe_btn)?;
        {
            let actions = self.actions.clone();
            bind_click(&probe_btn, &mut self.structural, move |_| {
                actions.toggle_probe()
            })?;
        }

        // Re-apply the last known connection status to the fresh nodes.
        let (label, ok) = self.conn.clone();
        self.paint_conn(&label, ok);
        Ok(())
    }

    // ── sidebar ─────────────────────────────────────────────────────────

    fn build_sidebar(
        &mut self,
        state: &ClientState,
        workspaces: &[String],
        selected: Option<&str>,
    ) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        self.sidebar.set_inner_html("");

        self.build_brand(state, workspaces)?;

        let list = mk(&doc, "div", "ws-list")?;
        self.sidebar.append_child(&list)?;

        // Peer windows for "send to …" (native lists every window but this one).
        if workspaces.is_empty() {
            let empty = mk(&doc, "div", "empty")?;
            empty.append_child(text_el(&doc, "div", "", "no workspaces here")?.unchecked_ref())?;
            {
                let hint = mk(&doc, "div", "empty-hint")?;
                hint.append_child(text_el(&doc, "span", "", "run ")?.unchecked_ref())?;
                hint.append_child(text_el(&doc, "code", "", "seance ctl new")?.unchecked_ref())?;
                empty.append_child(&hint)?;
            }
            list.append_child(&empty)?;
        }

        for ws in workspaces {
            self.build_ws_row(&list, state, ws, selected)?;
        }

        // Flex filler below the rows. The "elsewhere" rail and its
        // pull/collect menu went with the ownership model; phase 2 puts the
        // parked (unsubscribed) group here.
        let hit = mk(&doc, "div", "side-empty-hit")?;
        list.append_child(&hit)?;

        self.build_quicklaunch()?;
        self.build_host()?;
        self.build_footer()?;
        Ok(())
    }

    /// `✦ seance` + `◈+` (new empty workspace, named immediately). The `✦`
    /// glyph toggles the GUI-census popover.
    fn build_brand(&mut self, state: &ClientState, workspaces: &[String]) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        let head = mk(&doc, "div", "side-brand")?;
        let mark = text_el(&doc, "span", "brand-mark", "✦")?;
        mark.set_attribute("title", "connected guis")?;
        head.append_child(&mark)?;
        head.append_child(text_el(&doc, "span", "brand-name", "seance")?.unchecked_ref())?;
        let new_ws = text_el(&doc, "button", "brand-new", "◈+")?;
        new_ws.set_attribute("title", "new empty workspace (name it immediately)")?;
        head.append_child(&new_ws)?;
        self.sidebar.append_child(&head)?;

        let actions = self.actions.clone();
        let taken: Vec<String> = workspaces.to_vec();
        let pending = self.pending_ws_rename.clone();
        bind_click(&new_ws, &mut self.structural, move |ev| {
            ev.stop_propagation();
            // Native `create_workspace`: first free `circle-N`.
            let mut n = taken.len() + 1;
            let name = loop {
                let candidate = format!("circle-{n}");
                if !taken.iter().any(|w| *w == candidate) {
                    break candidate;
                }
                n += 1;
            };
            actions.create_workspace(&name);
            actions.select_workspace(&name);
            *pending.borrow_mut() = Some(name);
        })?;

        // ── ✦ census popover ────────────────────────────────────────────
        let rows: Vec<WindowRow> = state
            .windows
            .iter()
            .map(|w| (w.id.clone(), w.label.clone(), w.workspace_count))
            .collect();
        let self_id = state.window_id.clone();
        {
            let doc2 = doc.clone();
            let head2 = head.clone();
            let actions = self.actions.clone();
            let open = self.gui_menu_open.clone();
            let clicks = self.gui_menu_clicks.clone();
            let dismiss = self.gui_menu_dismiss.clone();
            let rows = rows.clone();
            let self_id = self_id.clone();
            bind_click(&mark, &mut self.structural, move |ev| {
                ev.stop_propagation();
                if open.get() {
                    gui_menu_close(&open, &clicks, &dismiss);
                    return;
                }
                open.set(true);
                if let Err(e) = gui_menu_render(
                    &doc2,
                    &head2,
                    &rows,
                    self_id.as_deref(),
                    &actions,
                    &open,
                    &clicks,
                    &dismiss,
                ) {
                    log("chrome: gui menu render failed", &e);
                }
            })?;
        }
        // Survive the rebuild: re-render under the fresh header when open.
        if self.gui_menu_open.get() {
            gui_menu_render(
                &doc,
                &head,
                &rows,
                self_id.as_deref(),
                &self.actions,
                &self.gui_menu_open,
                &self.gui_menu_clicks,
                &self.gui_menu_dismiss,
            )?;
        }
        Ok(())
    }

    fn build_ws_row(
        &mut self,
        list: &Element,
        state: &ClientState,
        ws: &str,
        selected: Option<&str>,
    ) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        let is_selected = Some(ws) == selected;
        let activity = state.activity_label(ws, self.now_ms());
        let att = state.workspace_attention(ws);
        let working = matches!(att, Some(Attention::Working));

        let row = mk(
            &doc,
            "div",
            if is_selected {
                "ws-row selected"
            } else {
                "ws-row"
            },
        )?;
        let main = mk(&doc, "div", "ws-main")?;

        // WEB DIVERGENCE #1: braille spinner → `◆` with a CSS pulse.
        let glyph = text_el(
            &doc,
            "span",
            ws_glyph_class(working, is_selected),
            ws_glyph_char(working, is_selected),
        )?;
        let name = text_el(&doc, "span", "ws-name", ws)?;
        // Text badges only for needs/done — working is the left-hand glyph.
        let (att_text, att_class) = ws_att(att, is_selected);
        let att_el = text_el(&doc, "span", att_class, att_text)?;
        let banish = text_el(&doc, "button", "ws-banish", "×")?;
        banish.set_attribute("title", "banish workspace (kill all panes) — click twice")?;
        let count_el = text_el(&doc, "span", "ws-count", &activity)?;

        main.append_child(&glyph)?;
        main.append_child(&name)?;
        main.append_child(&att_el)?;
        main.append_child(&banish)?;
        main.append_child(&count_el)?;
        row.append_child(&main)?;
        list.append_child(&row)?;

        // click = select
        {
            let actions = self.actions.clone();
            let ws = ws.to_string();
            bind_click(&row, &mut self.structural, move |_| {
                actions.select_workspace(&ws)
            })?;
        }
        // double-click = inline rename (native row semantics)
        {
            let rn = self.rename.clone();
            let (row2, main2) = (row.clone(), main.clone());
            let ws = ws.to_string();
            bind(&row, "dblclick", &mut self.structural, move |ev| {
                ev.prevent_default();
                open_rename(&rn, &row2, &main2, &ws, RenameKind::Workspace(ws.clone()));
            })?;
        }
        // WEB DIVERGENCE #5: two-click arm on the banish ×.
        {
            let actions = self.actions.clone();
            let armed = self.kill_armed.clone();
            let win = self.win.clone();
            let btn = banish.clone();
            let ws = ws.to_string();
            bind_click(&banish, &mut self.structural, move |ev| {
                ev.stop_propagation();
                let a = actions.clone();
                let w = ws.clone();
                arm_or_fire(
                    &win,
                    &armed,
                    &format!("ws:{ws}"),
                    &btn,
                    "ws-banish",
                    move || a.kill_workspace(&w),
                );
            })?;
        }
        // Per-row context menu (native order).
        {
            let actions = self.actions.clone();
            let rn = self.rename.clone();
            let (row2, main2) = (row.clone(), main.clone());
            let ws = ws.to_string();
            bind_ctx(&row, &mut self.structural, move |ev| {
                ev.prevent_default();
                ev.stop_propagation();
                let mut entries = Vec::new();
                {
                    let a = actions.clone();
                    let w = ws.clone();
                    entries.push(MenuEntry::item("touch (bump recency)", move || {
                        a.touch_workspace(&w)
                    }));
                }
                {
                    let rn = rn.clone();
                    let (r, m) = (row2.clone(), main2.clone());
                    let w = ws.clone();
                    entries.push(MenuEntry::item("rename workspace", move || {
                        open_rename(&rn, &r, &m, &w, RenameKind::Workspace(w.clone()))
                    }));
                }
                {
                    let a = actions.clone();
                    let w = ws.clone();
                    entries.push(MenuEntry::item("fork workspace ⑂", move || {
                        a.send(GuiRequest::ForkWorkspace {
                            workspace: w,
                            name: None,
                        })
                    }));
                }
                {
                    let w = ws.clone();
                    entries.push(MenuEntry::item("share replay…", move || {
                        // The editor is a page-level takeover — route via hash
                        // + reload so start() re-dispatches into replay_edit.
                        if let Some(win) = web_sys::window() {
                            let _ = win
                                .location()
                                .set_hash(&format!("replay-edit?workspace={w}"));
                            let _ = win.location().reload();
                        }
                    }));
                }
                entries.push(MenuEntry::Separator);
                {
                    let a = actions.clone();
                    let w = ws.clone();
                    entries.push(MenuEntry::danger(
                        "banish workspace (kill all panes)",
                        move || a.kill_workspace(&w),
                    ));
                }
                open_menu(ev.client_x() as f64, ev.client_y() as f64, entries);
            })?;
        }

        self.ws_refs.insert(
            ws.to_string(),
            WsRefs {
                row,
                main,
                glyph,
                att: att_el,
                count: count_el,
                selected: is_selected,
            },
        );
        // Cycling (ctrl+pageup/down) can select a row that's scrolled out of
        // the rail — bring it into view once per selection change.
        if is_selected && self.last_scrolled_ws.as_deref() != Some(ws) {
            self.last_scrolled_ws = Some(ws.to_string());
            if let Some(refs) = self.ws_refs.get(ws) {
                refs.row.scroll_into_view_with_bool(false);
            }
        }
        Ok(())
    }

    // ── quicklaunch strip ───────────────────────────────────────────────

    fn build_quicklaunch(&mut self) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        let strip = mk(&doc, "div", "ql-strip")?;
        let head = mk(&doc, "div", "ql-head")?;
        head.append_child(
            text_el(&doc, "span", "ql-title", "── vita quicklaunch ──")?.unchecked_ref(),
        )?;
        let add = text_el(&doc, "button", "ql-add", "+")?;
        add.set_attribute("title", "add quicklaunch entry")?;
        head.append_child(&add)?;
        strip.append_child(&head)?;

        let chips = mk(&doc, "div", "ql-chips")?;
        strip.append_child(&chips)?;
        self.sidebar.append_child(&strip)?;
        *self.quicklaunch.chips.borrow_mut() = Some(chips);

        {
            let ql = self.quicklaunch.clone();
            let actions = self.actions.clone();
            let doc2 = doc.clone();
            bind_click(&add, &mut self.structural, move |ev| {
                ev.stop_propagation();
                ql_open_editor(&doc2, &ql, &actions, None);
            })?;
        }
        ql_render(&doc, &self.quicklaunch, &self.actions);
        Ok(())
    }

    /// Throttled mtime check of the daemon-side config (native: render-time
    /// stat, 2s throttle, parse error keeps the previous entries, bridge
    /// failure keeps state and retries next tick).
    fn maybe_reload_quicklaunch(&self) {
        if self.quicklaunch.inflight.get() {
            return;
        }
        let now = self.now_ms();
        if now - self.quicklaunch.last_check.get() < QL_POLL_MS {
            return;
        }
        self.quicklaunch.last_check.set(now);
        self.quicklaunch.inflight.set(true);

        let ql = self.quicklaunch.clone();
        let actions = self.actions.clone();
        let doc = self.doc.clone();
        self.actions.fs_call(
            FsOp::Stat {
                path: QUICKLAUNCH_PATH.to_string(),
            },
            Box::new(move |res| {
                let value = match res {
                    Ok(v) => v,
                    // Bridge down — keep state, retry on the next tick.
                    Err(_) => {
                        ql.inflight.set(false);
                        return;
                    }
                };
                let exists = value.get("exists").and_then(|e| e.as_bool()) == Some(true);
                // File missing = None; exists-with-unreadable-mtime = Some(0).
                let cur: Option<u64> = if exists {
                    Some(value.get("mtime_ms").and_then(|m| m.as_u64()).unwrap_or(0))
                } else {
                    None
                };
                if cur == ql.mtime.get() {
                    ql.inflight.set(false);
                    return;
                }
                if cur.is_none() {
                    ql.mtime.set(None);
                    ql.entries.borrow_mut().clear();
                    ql_render(&doc, &ql, &actions);
                    ql.inflight.set(false);
                    return;
                }
                let ql2 = ql.clone();
                let doc2 = doc.clone();
                let actions2 = actions.clone();
                actions.fs_call(
                    FsOp::Read {
                        path: QUICKLAUNCH_PATH.to_string(),
                    },
                    Box::new(move |res| {
                        ql2.inflight.set(false);
                        let Ok(v) = res else { return };
                        let Some(b64) = v.get("contents_b64").and_then(|c| c.as_str()) else {
                            return;
                        };
                        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
                        else {
                            return;
                        };
                        let Ok(text) = String::from_utf8(bytes) else {
                            return;
                        };
                        // Adopt the mtime even on a parse error, so a broken
                        // file isn't re-read every 2s.
                        ql2.mtime.set(cur);
                        match serde_json::from_str::<Vec<QlEntry>>(&text) {
                            Ok(parsed) => {
                                let changed = {
                                    let cur = ql2.entries.borrow();
                                    *cur != parsed
                                };
                                if changed {
                                    *ql2.entries.borrow_mut() = parsed;
                                    ql_render(&doc2, &ql2, &actions2);
                                }
                            }
                            // Parse error keeps the previous entries (native).
                            Err(e) => log(
                                "quicklaunch.json parse error (keeping previous)",
                                &JsValue::from_str(&e.to_string()),
                            ),
                        }
                    }),
                );
            }),
        );
    }

    // ── host accounts strip ─────────────────────────────────────────────

    fn build_host(&mut self) -> Result<(), JsValue> {
        let root = mk(&self.doc, "div", "host-strip hidden")?;
        self.sidebar.append_child(&root)?;
        *self.host.root.borrow_mut() = Some(root);
        // Force a render against the cached widget set on the next sync.
        self.host.sig.borrow_mut().clear();
        Ok(())
    }

    /// Adopt fresh host widgets and re-render the strip when they changed.
    fn sync_host(&mut self, state: &ClientState) {
        let sig = format!("{:?}", state.host_widgets);
        if *self.host.sig.borrow() == sig {
            return;
        }
        *self.host.sig.borrow_mut() = sig;
        *self.host.widgets.borrow_mut() = state.host_widgets.clone();
        host_render(&self.doc, &self.host, &self.actions);
    }

    // ── footer ──────────────────────────────────────────────────────────

    fn build_footer(&mut self) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        let footer = mk(&doc, "div", "side-footer")?;

        let summon = text_el(&doc, "button", "foot-summon", "+ summon")?;
        summon.set_attribute(
            "title",
            "new shell pane in this workspace (ctrl+shift+n) — name it right away",
        )?;
        let activity = text_el(&doc, "button", "foot-btn", "≋")?;
        activity.set_attribute("title", "activity feed — who did what, live")?;
        let help = text_el(&doc, "button", "foot-btn foot-help", "?")?;
        help.set_attribute("title", "open the grimoire — full guide to seance")?;

        footer.append_child(&summon)?;
        footer.append_child(&activity)?;
        footer.append_child(&help)?;
        self.sidebar.append_child(&footer)?;

        {
            let actions = self.actions.clone();
            bind_click(&summon, &mut self.structural, move |_| actions.summon())?;
        }
        {
            let actions = self.actions.clone();
            bind_click(&activity, &mut self.structural, move |_| {
                actions.toggle_activity()
            })?;
        }
        {
            let actions = self.actions.clone();
            bind_click(&help, &mut self.structural, move |_| actions.toggle_help())?;
        }
        Ok(())
    }

    // ── tiles ───────────────────────────────────────────────────────────

    fn build_tiles(&mut self, state: &ClientState, selected: Option<&str>) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        self.tiles.set_inner_html("");
        self.tiles.set_class_name("");

        let all: Vec<&PaneInfo> = match selected {
            Some(ws) => state.panes_in(ws).into_iter().filter(|p| p.tiled).collect(),
            None => Vec::new(),
        };

        if all.is_empty() {
            self.tiles
                .set_attribute("style", "grid-template-columns:1fr;grid-template-rows:1fr")?;
            let empty = mk(&doc, "div", "empty")?;
            empty.append_child(text_el(&doc, "div", "empty-mark", "✦")?.unchecked_ref())?;
            let msg = match selected {
                Some(ws) => format!("{ws} is empty — summon a spirit (ctrl+shift+n)"),
                None => "empty window — right-click the sidebar to pull a workspace here".into(),
            };
            empty.append_child(text_el(&doc, "div", "", &msg)?.unchecked_ref())?;
            self.tiles.append_child(&empty)?;
            return Ok(());
        }

        // Focus-zoom: one pane fills the region behind a flame mode bar.
        let zoomed: Option<String> = state
            .zoomed
            .clone()
            .filter(|z| all.iter().any(|p| p.slug == *z));

        let panes: Vec<&PaneInfo> = match &zoomed {
            Some(z) => all.into_iter().filter(|p| p.slug == *z).collect(),
            None => all,
        };

        // Every workspace (for the pane menu's "move to →" items).
        let workspaces = state.workspaces();

        if let Some(z) = zoomed.clone() {
            self.tiles.set_class_name("zoomed");
            self.tiles.remove_attribute("style")?;
            let bar = mk(&doc, "div", "zoom-bar")?;
            let left = mk(&doc, "div", "zoom-left")?;
            left.append_child(text_el(&doc, "span", "zoom-mark", "⛶ zoomed")?.unchecked_ref())?;
            let zname = state.pane(&z).map(|p| p.name.clone()).unwrap_or_default();
            left.append_child(text_el(&doc, "span", "zoom-name", &zname)?.unchecked_ref())?;
            left.append_child(
                text_el(&doc, "span", "zoom-slug", &format!("`{z}`"))?.unchecked_ref(),
            )?;
            bar.append_child(&left)?;
            let right = mk(&doc, "div", "zoom-right")?;
            right.append_child(
                text_el(&doc, "span", "zoom-hint", "esc · ctrl+shift+z")?.unchecked_ref(),
            )?;
            let unzoom = text_el(&doc, "button", "zoom-btn", "unzoom")?;
            unzoom.set_attribute("title", "unzoom (esc)")?;
            right.append_child(&unzoom)?;
            bar.append_child(&right)?;
            self.tiles.append_child(&bar)?;
            let actions = self.actions.clone();
            bind_click(&unzoom, &mut self.structural, move |ev| {
                ev.stop_propagation();
                actions.toggle_zoom(&z);
            })?;
        } else {
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
        }

        let is_zoomed = zoomed.is_some();
        for pane in panes {
            self.build_tile(pane, &workspaces, is_zoomed)?;
        }
        Ok(())
    }

    fn build_tile(
        &mut self,
        pane: &PaneInfo,
        workspaces: &[String],
        is_zoomed: bool,
    ) -> Result<(), JsValue> {
        let doc = self.doc.clone();
        let slug = pane.slug.clone();
        let tile = mk(&doc, "div", "tile")?;
        tile.set_attribute("data-slug", &slug)?;

        let header = mk(&doc, "div", "tile-header")?;
        let main = mk(&doc, "div", "th-main")?;
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

        let zoom = text_el(&doc, "button", "tile-zoom", "⛶")?;
        zoom.set_attribute(
            "title",
            if is_zoomed {
                "unzoom (esc · ctrl+shift+z)"
            } else {
                "zoom this pane (ctrl+shift+z)"
            },
        )?;
        let kill = text_el(&doc, "button", "kill", "×")?;
        kill.set_attribute("title", "kill pane — click twice")?;

        main.append_child(&dot)?;
        main.append_child(&name)?;
        main.append_child(&title)?;
        main.append_child(&badge)?;
        main.append_child(&ghost)?;
        main.append_child(&zoom)?;
        main.append_child(&kill)?;
        header.append_child(&main)?;

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
            bind_click(target, &mut self.structural, move |_| {
                actions.focus_pane(&s)
            })?;
        }
        // Double-click the header = inline rename (mirrors the workspace row).
        {
            let rn = self.rename.clone();
            let (header2, main2) = (header.clone(), main.clone());
            let (s, n) = (slug.clone(), pane.name.clone());
            bind(&header, "dblclick", &mut self.structural, move |ev| {
                ev.prevent_default();
                open_rename(&rn, &header2, &main2, &n, RenameKind::Pane(s.clone()));
            })?;
        }
        // Pane context menu on the whole tile (so a right-click on the canvas
        // gets it too, and the browser menu never appears inside app surfaces).
        {
            let actions = self.actions.clone();
            let rn = self.rename.clone();
            let (header2, main2) = (header.clone(), main.clone());
            let (s, pname) = (slug.clone(), pane.name.clone());
            let ws_list: Vec<String> = workspaces
                .iter()
                .filter(|w| **w != pane.workspace)
                .cloned()
                .collect();
            bind_ctx(&tile, &mut self.structural, move |ev| {
                ev.prevent_default();
                ev.stop_propagation();
                let mut entries = Vec::new();
                {
                    let rn = rn.clone();
                    let (h, m, n) = (header2.clone(), main2.clone(), pname.clone());
                    let s2 = s.clone();
                    entries.push(MenuEntry::item("rename pane", move || {
                        open_rename(&rn, &h, &m, &n, RenameKind::Pane(s2))
                    }));
                }
                {
                    let a = actions.clone();
                    let s2 = s.clone();
                    entries.push(MenuEntry::item(
                        if is_zoomed { "unzoom" } else { "zoom" },
                        move || a.toggle_zoom(&s2),
                    ));
                }
                // WEB DIVERGENCE #2: no pane drag — "move to workspace…" is a
                // flat list of destinations.
                if !ws_list.is_empty() {
                    entries.push(MenuEntry::Separator);
                    for w in &ws_list {
                        let a = actions.clone();
                        let s2 = s.clone();
                        let w2 = w.clone();
                        entries.push(MenuEntry::item(format!("move to → {w}"), move || {
                            a.send(GuiRequest::MovePane {
                                pane: s2,
                                workspace: w2,
                                before: None,
                            })
                        }));
                    }
                }
                entries.push(MenuEntry::Separator);
                {
                    let a = actions.clone();
                    let s2 = s.clone();
                    entries.push(MenuEntry::danger("kill pane", move || a.kill_pane(&s2)));
                }
                open_menu(ev.client_x() as f64, ev.client_y() as f64, entries);
            })?;
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
        {
            let actions = self.actions.clone();
            let s = slug.clone();
            bind_click(&zoom, &mut self.structural, move |e| {
                e.stop_propagation();
                actions.toggle_zoom(&s);
            })?;
        }
        // WEB DIVERGENCE #5: kill × arms on the first click, fires on the second.
        {
            let actions = self.actions.clone();
            let armed = self.kill_armed.clone();
            let win = self.win.clone();
            let btn = kill.clone();
            let s = slug.clone();
            bind_click(&kill, &mut self.structural, move |e| {
                e.stop_propagation();
                let a = actions.clone();
                let s2 = s.clone();
                arm_or_fire(
                    &win,
                    &armed,
                    &format!("pane:{s}"),
                    &btn,
                    "kill",
                    move || a.kill_pane(&s2),
                );
            })?;
        }

        self.tile_refs.insert(
            slug,
            TileRefs {
                tile,
                dot,
                name,
                title,
                badge,
                ghost,
                main,
                header,
            },
        );
        Ok(())
    }

    // ── inline rename ───────────────────────────────────────────────────

    /// Open the inline rename input on a workspace row (Enter commits via
    /// `Actions::rename_workspace`, Esc cancels). Native
    /// `start_rename(RenameTarget::Workspace(..))`.
    pub fn begin_rename_workspace(&mut self, ws: &str) {
        let Some(refs) = self.ws_refs.get(ws) else {
            return;
        };
        let (row, main) = (refs.row.clone(), refs.main.clone());
        open_rename(
            &self.rename,
            &row,
            &main,
            ws,
            RenameKind::Workspace(ws.to_string()),
        );
    }

    /// Open the inline rename input on a tile header (Enter commits via
    /// `Actions::rename_pane`). Native `start_rename(RenameTarget::Pane(..))`,
    /// used by `+ summon` on the freshly arrived pane.
    pub fn begin_rename_pane(&mut self, slug: &str) {
        let Some(refs) = self.tile_refs.get(slug) else {
            return;
        };
        let (header, main) = (refs.header.clone(), refs.main.clone());
        let current = refs.name.text_content().unwrap_or_default();
        open_rename(
            &self.rename,
            &header,
            &main,
            &current,
            RenameKind::Pane(slug.to_string()),
        );
    }

    // ── in-place updates ────────────────────────────────────────────────

    /// Patch status dots, titles, origin badges, ghost chips, focus ring,
    /// workspace attention, the host strip and the asks banner. Never touches
    /// canvases.
    pub fn update_badges(&mut self, state: &ClientState) {
        self.apply_badges(state);
        self.sync_host(state);
        self.maybe_reload_quicklaunch();
        if let Err(e) = self.render_asks(state) {
            log("chrome: asks render failed", &e);
        }
    }

    fn apply_badges(&mut self, state: &ClientState) {
        let focused = state.focused_pane.clone().or_else(|| self.focused.clone());

        for (slug, refs) in &self.tile_refs {
            let Some(pane) = state.pane(slug) else {
                continue;
            };
            let status = state.statuses.get(slug).map(|s| s.state.as_str());
            let exited = pane.exited
                || state.agency.get(slug).map(|a| a.exited).unwrap_or(false)
                || !pane.running;
            refs.dot
                .set_class_name(&format!("dot {}", dot_class(status, exited)));
            refs.name.set_text_content(Some(&pane.name));

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

        for (ws, refs) in &self.ws_refs {
            let att = state.workspace_attention(ws);
            let working = matches!(att, Some(Attention::Working));
            refs.glyph
                .set_class_name(ws_glyph_class(working, refs.selected));
            refs.glyph
                .set_text_content(Some(ws_glyph_char(working, refs.selected)));
            let (text, class) = ws_att(att, refs.selected);
            refs.att.set_text_content(Some(text));
            refs.att.set_class_name(class);
            refs.count
                .set_text_content(Some(&state.activity_label(ws, self.now_ms())));
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
                bind_key(free.unchecked_ref(), &mut self.ask_keys, move |e| {
                    // Keep typing local — the document keymap must not see it.
                    e.stop_propagation();
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
        let is_err = [
            "error",
            "failed",
            "fail:",
            "disconnect",
            "refused",
            "denied",
        ]
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
            bind_key(input.unchecked_ref(), &mut self.login_keys, move |e| {
                e.stop_propagation();
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

// ── inline rename (free fns: listeners have no `&mut Chrome`) ───────────────

/// Swap `main` for a focused text input inside `host`. Enter commits, Esc
/// cancels; every other key is stopped so the document keymap and the PTY
/// never see the typing.
fn open_rename(rn: &Rename, host: &Element, main: &Element, initial: &str, kind: RenameKind) {
    // Close any previous rename first.
    if let Some((old_input, old_main)) = rn.open.borrow_mut().take() {
        end_rename(&old_input, &old_main);
    }
    // The old key closures may include the one currently executing.
    let stale: Vec<KeyClosure> = std::mem::take(&mut *rn.keys.borrow_mut());
    drop_later(Vec::new(), stale);

    let Ok(input) = input_el(&rn.doc, "text", "name") else {
        return;
    };
    input.set_class_name("rename-input");
    input.set_value(initial);
    main.set_class_name(&format!("{} rename-hidden", main.class_name()));
    if host.append_child(input.unchecked_ref()).is_err() {
        main.set_class_name(&main.class_name().replace(" rename-hidden", ""));
        return;
    }
    let _ = input.focus();
    input.select();
    *rn.open.borrow_mut() = Some((input.clone(), main.clone()));

    let actions = rn.actions.clone();
    let field = input.clone();
    let main2 = main.clone();
    let open = rn.open.clone();
    let _ = bind_key(
        input.unchecked_ref(),
        &mut rn.keys.borrow_mut(),
        move |ev| {
            ev.stop_propagation();
            match ev.key().as_str() {
                "Enter" => {
                    let value = field.value().trim().to_string();
                    if !value.is_empty() {
                        match &kind {
                            RenameKind::Workspace(old) => {
                                if value != *old {
                                    actions.rename_workspace(old, &value);
                                }
                            }
                            RenameKind::Pane(slug) => actions.rename_pane(slug, &value),
                        }
                    }
                    *open.borrow_mut() = None;
                    end_rename(&field, &main2);
                }
                "Escape" => {
                    *open.borrow_mut() = None;
                    end_rename(&field, &main2);
                }
                _ => {}
            }
        },
    );
}

fn end_rename(input: &HtmlInputElement, main: &Element) {
    let el: &Element = input.unchecked_ref();
    el.remove();
    main.set_class_name(&main.class_name().replace(" rename-hidden", ""));
}

// ── quicklaunch rendering ───────────────────────────────────────────────────

/// Re-render the chip row from `ql.entries`. Callable from inside a chip or
/// menu listener — the superseded closures are freed in a later task.
fn ql_render(doc: &Document, ql: &QuickLaunch, actions: &Rc<dyn Actions>) {
    let chips = match ql.chips.borrow().clone() {
        Some(c) => c,
        None => return,
    };
    let mut fresh: Vec<ClickClosure> = Vec::new();
    chips.set_inner_html("");
    let entries = ql.entries.borrow().clone();
    // Native hides the chip row (not the title row) when the config is empty.
    chips.set_class_name(if entries.is_empty() {
        "ql-chips hidden"
    } else {
        "ql-chips"
    });

    for entry in &entries {
        let Ok(chip) = mk(doc, "div", "ql-chip") else {
            continue;
        };
        chip.set_text_content(Some(&entry.name));
        let cwd_desc = entry.cwd.clone().unwrap_or_else(|| "~".into());
        let cmd_desc = entry
            .command
            .clone()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| "shell".into());
        let _ = chip.set_attribute("title", &format!("{cwd_desc} $ {cmd_desc}"));
        let _ = chips.append_child(&chip);

        // Click = FRESH uniquified workspace named after the entry.
        {
            let actions = actions.clone();
            let e = entry.clone();
            let _ = bind_click(&chip, &mut fresh, move |ev| {
                ev.stop_propagation();
                // cwd travels RAW — `~` expands on the DAEMON's machine.
                actions.quicklaunch(&e.name, e.cwd.clone(), e.command.clone());
            });
        }
        // Right-click: edit… / move up / move down / remove.
        // WEB DIVERGENCE #2: the move items stand in for drag-reorder.
        {
            let actions = actions.clone();
            let ql = ql.clone();
            let doc2 = doc.clone();
            let name = entry.name.clone();
            let _ = bind_ctx(&chip, &mut fresh, move |ev| {
                ev.prevent_default();
                ev.stop_propagation();
                let mut items = Vec::new();
                {
                    let (d, q, a, n) = (doc2.clone(), ql.clone(), actions.clone(), name.clone());
                    items.push(MenuEntry::item("edit…", move || {
                        ql_open_editor(&d, &q, &a, Some(n))
                    }));
                }
                {
                    let (d, q, a, n) = (doc2.clone(), ql.clone(), actions.clone(), name.clone());
                    items.push(MenuEntry::item("move up", move || {
                        ql_shift(&mut q.entries.borrow_mut()[..], &n, -1);
                        ql_save(&a, &q);
                        ql_render(&d, &q, &a);
                    }));
                }
                {
                    let (d, q, a, n) = (doc2.clone(), ql.clone(), actions.clone(), name.clone());
                    items.push(MenuEntry::item("move down", move || {
                        ql_shift(&mut q.entries.borrow_mut()[..], &n, 1);
                        ql_save(&a, &q);
                        ql_render(&d, &q, &a);
                    }));
                }
                items.push(MenuEntry::Separator);
                {
                    let (d, q, a, n) = (doc2.clone(), ql.clone(), actions.clone(), name.clone());
                    items.push(MenuEntry::danger("remove", move || {
                        q.entries.borrow_mut().retain(|e| e.name != n);
                        ql_save(&a, &q);
                        ql_render(&d, &q, &a);
                    }));
                }
                open_menu(ev.client_x() as f64, ev.client_y() as f64, items);
            });
        }
    }
    let stale = std::mem::replace(&mut *ql.clicks.borrow_mut(), fresh);
    drop_later(stale, Vec::new());
}

/// Serialize + write through to the daemon config, adopting the returned mtime
/// so the hot reload doesn't re-read our own write. Best effort: a failure
/// leaves the in-memory strip intact (the edit just isn't durable).
fn ql_save(actions: &Rc<dyn Actions>, ql: &QuickLaunch) {
    let json = match serde_json::to_string_pretty(&*ql.entries.borrow()) {
        Ok(j) => j,
        Err(e) => {
            log(
                "quicklaunch serialize failed",
                &JsValue::from_str(&e.to_string()),
            );
            return;
        }
    };
    let mtime = ql.mtime.clone();
    actions.fs_call(
        FsOp::Write {
            path: QUICKLAUNCH_PATH.to_string(),
            contents_b64: base64::engine::general_purpose::STANDARD.encode(json.as_bytes()),
        },
        Box::new(move |res| {
            if let Ok(v) = res {
                mtime.set(Some(
                    v.get("mtime_ms").and_then(|m| m.as_u64()).unwrap_or(0),
                ));
            }
        }),
    );
}

/// Dimmed-backdrop modal with name / cwd / command (native 420px card with the
/// flame_dim border). Enter saves, Esc cancels, backdrop click cancels.
fn ql_open_editor(
    doc: &Document,
    ql: &QuickLaunch,
    actions: &Rc<dyn Actions>,
    original: Option<String>,
) {
    ql_close_editor(doc, ql);
    let Some(app) = doc.get_element_by_id("app") else {
        return;
    };
    let seed = original
        .as_ref()
        .and_then(|n| ql.entries.borrow().iter().find(|e| e.name == *n).cloned())
        .unwrap_or_default();

    let (Ok(overlay), Ok(card)) = (mk(doc, "div", "ql-overlay"), mk(doc, "div", "ql-card")) else {
        return;
    };
    overlay.set_id("ql-editor");
    let title = if original.is_some() {
        "edit quicklaunch"
    } else {
        "new quicklaunch"
    };
    if let Ok(h) = text_el(doc, "div", "ql-card-title", title) {
        let _ = card.append_child(&h);
    }

    let field = |label: &str, placeholder: &str, value: &str| -> Option<HtmlInputElement> {
        let wrap = mk(doc, "div", "ql-field").ok()?;
        let lab = text_el(doc, "div", "ql-label", label).ok()?;
        let input = input_el(doc, "text", placeholder).ok()?;
        input.set_value(value);
        wrap.append_child(&lab).ok()?;
        wrap.append_child(input.unchecked_ref()).ok()?;
        card.append_child(&wrap).ok()?;
        Some(input)
    };

    // Built in DOM order: name, hint, cwd, command (native layout).
    let Some(name_in) = field("name", "vita", &seed.name) else {
        return;
    };
    let Ok(hint) = text_el(doc, "div", "ql-hint hidden", "") else {
        return;
    };
    let _ = card.append_child(&hint);
    let (Some(cwd_in), Some(cmd_in)) = (
        field("cwd", "~/work/vita", seed.cwd.as_deref().unwrap_or("")),
        field(
            "command",
            "claude (empty = plain shell)",
            seed.command.as_deref().unwrap_or(""),
        ),
    ) else {
        return;
    };

    let (Ok(row), Ok(cancel), Ok(save)) = (
        mk(doc, "div", "ql-actions"),
        text_el(doc, "button", "ql-cancel", "cancel"),
        text_el(doc, "button", "ql-save", "save"),
    ) else {
        return;
    };
    let _ = row.append_child(&cancel);
    let _ = row.append_child(&save);
    let _ = card.append_child(&row);
    let _ = overlay.append_child(&card);
    let _ = app.append_child(&overlay);
    let _ = name_in.focus();
    name_in.select();

    let mut clicks: Vec<ClickClosure> = Vec::new();
    let mut keys: Vec<KeyClosure> = Vec::new();

    // Validate + persist. Blocked (stays open, hint shown) on an empty name or
    // a name that collides with a *different* entry — native semantics.
    let commit = {
        let ql = ql.clone();
        let actions = actions.clone();
        let doc = doc.clone();
        let original = original.clone();
        let (n, c, d, hint) = (
            name_in.clone(),
            cwd_in.clone(),
            cmd_in.clone(),
            hint.clone(),
        );
        Rc::new(move || {
            let name = n.value().trim().to_string();
            let bad = if name.is_empty() {
                Some("name required")
            } else if ql_name_collides(&ql.entries.borrow()[..], &name, original.as_deref()) {
                Some("name in use")
            } else {
                None
            };
            if let Some(msg) = bad {
                hint.set_text_content(Some(msg));
                hint.set_class_name("ql-hint");
                let _ = n.focus();
                return;
            }
            let norm = |v: String| {
                let t = v.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            };
            let entry = QlEntry {
                name,
                cwd: norm(c.value()),
                command: norm(d.value()),
            };
            ql_upsert(&mut ql.entries.borrow_mut(), original.as_deref(), entry);
            ql_save(&actions, &ql);
            ql_close_editor(&doc, &ql);
            ql_render(&doc, &ql, &actions);
        })
    };

    {
        let commit = commit.clone();
        let _ = bind_click(&save, &mut clicks, move |ev| {
            ev.stop_propagation();
            (commit)();
        });
    }
    {
        let (d, q) = (doc.clone(), ql.clone());
        let _ = bind_click(&cancel, &mut clicks, move |ev| {
            ev.stop_propagation();
            ql_close_editor(&d, &q);
        });
    }
    // Backdrop click cancels; clicks inside the card do not bubble to it.
    {
        let (d, q) = (doc.clone(), ql.clone());
        let _ = bind_click(&overlay, &mut clicks, move |_| ql_close_editor(&d, &q));
    }
    {
        let _ = bind_click(&card, &mut clicks, move |ev| ev.stop_propagation());
    }
    for input in [&name_in, &cwd_in, &cmd_in] {
        let commit = commit.clone();
        let (d, q) = (doc.clone(), ql.clone());
        let _ = bind_key(input.unchecked_ref(), &mut keys, move |ev| {
            ev.stop_propagation();
            match ev.key().as_str() {
                "Enter" => (commit)(),
                "Escape" => ql_close_editor(&d, &q),
                _ => {}
            }
        });
    }

    let stale_clicks = std::mem::replace(&mut *ql.editor_clicks.borrow_mut(), clicks);
    let stale_keys = std::mem::replace(&mut *ql.editor_keys.borrow_mut(), keys);
    drop_later(stale_clicks, stale_keys);
}

fn ql_close_editor(doc: &Document, ql: &QuickLaunch) {
    if let Some(el) = doc.get_element_by_id("ql-editor") {
        el.remove();
    }
    let stale_clicks = std::mem::take(&mut *ql.editor_clicks.borrow_mut());
    let stale_keys = std::mem::take(&mut *ql.editor_keys.borrow_mut());
    drop_later(stale_clicks, stale_keys);
}

// ── host accounts strip ─────────────────────────────────────────────────────

/// Collapsed (default): only the current/selected account. Click the title (or
/// the collapsed row) to expand; click an account to select it and collapse;
/// clicking the already-current account collapses without re-running select.
fn host_render(doc: &Document, host: &HostStrip, actions: &Rc<dyn Actions>) {
    let root = match host.root.borrow().clone() {
        Some(r) => r,
        None => return,
    };
    let mut fresh: Vec<ClickClosure> = Vec::new();
    root.set_inner_html("");
    let widgets = host.widgets.borrow().clone();
    root.set_class_name(if widgets.is_empty() {
        "host-strip hidden"
    } else {
        "host-strip"
    });

    for w in &widgets {
        let title = if w.title.is_empty() {
            w.id.clone()
        } else {
            w.title.clone()
        };
        let expanded = host.expanded.borrow().contains(&w.id);
        let caret = if expanded { "▾" } else { "▸" };

        // Prefer explicit selected flag, then host `active`, then first.
        let current_id: Option<String> = w
            .items
            .iter()
            .find(|i| i.selected)
            .map(|i| i.id.clone())
            .or_else(|| w.active.clone())
            .or_else(|| w.items.first().map(|i| i.id.clone()));

        let (Ok(group), Ok(head)) = (mk(doc, "div", "host-group"), mk(doc, "div", "host-head"))
        else {
            continue;
        };
        if let Ok(t) = text_el(doc, "span", "host-title", &format!("{caret} {title}")) {
            let _ = t.set_attribute(
                "title",
                if expanded {
                    "collapse accounts"
                } else {
                    "expand accounts"
                },
            );
            let _ = head.append_child(&t);
        }
        if let Some(err) = w.error.as_ref() {
            if let Ok(e) = text_el(doc, "span", "host-err", "!") {
                let _ = e.set_attribute("title", err);
                let _ = head.append_child(&e);
            }
        }
        let _ = group.append_child(&head);
        {
            let host2 = host.clone();
            let actions2 = actions.clone();
            let doc2 = doc.clone();
            let id = w.id.clone();
            let _ = bind_click(&head, &mut fresh, move |ev| {
                ev.stop_propagation();
                {
                    let mut exp = host2.expanded.borrow_mut();
                    if !exp.remove(&id) {
                        exp.insert(id.clone());
                    }
                }
                host_render(&doc2, &host2, &actions2);
            });
        }

        let visible: Vec<_> = if expanded {
            w.items.iter().collect()
        } else {
            w.items
                .iter()
                .filter(|i| current_id.as_deref() == Some(i.id.as_str()) || i.selected)
                .collect()
        };

        for item in visible {
            let selected = item.selected || current_id.as_deref() == Some(item.id.as_str());
            // busy = danger, warm = flame, auth = violet, selected = success.
            let state_class = match item.state.as_str() {
                "busy" => "busy",
                "warm" => "warm",
                "auth" => "auth",
                _ if selected => "current",
                _ => "",
            };
            let Ok(row) = mk(
                doc,
                "div",
                if selected {
                    "host-item selected"
                } else {
                    "host-item"
                },
            ) else {
                continue;
            };
            let _ = row.set_attribute(
                "title",
                &if !expanded {
                    format!("{} · click to show all accounts", item.label)
                } else if selected {
                    format!("{} · current · click to collapse", item.label)
                } else {
                    format!("switch to {}", item.label)
                },
            );
            if let Ok(mark) = text_el(
                doc,
                "span",
                &format!("host-mark {state_class}"),
                if selected { "●" } else { "○" },
            ) {
                let _ = row.append_child(&mark);
            }
            if let Ok(col) = mk(doc, "div", "host-lines") {
                if let Ok(l) = text_el(doc, "div", "host-label", &item.label) {
                    let _ = col.append_child(&l);
                }
                if !item.detail.is_empty() {
                    if let Ok(l) = text_el(doc, "div", "host-detail", &item.detail) {
                        let _ = col.append_child(&l);
                    }
                }
                if !item.detail2.is_empty() {
                    if let Ok(l) = text_el(doc, "div", "host-detail", &item.detail2) {
                        let _ = col.append_child(&l);
                    }
                }
                let _ = row.append_child(&col);
            }
            let _ = group.append_child(&row);

            let host2 = host.clone();
            let actions2 = actions.clone();
            let doc2 = doc.clone();
            let wid = w.id.clone();
            let iid = item.id.clone();
            let already = selected;
            let was_expanded = expanded;
            let _ = bind_click(&row, &mut fresh, move |ev| {
                ev.stop_propagation();
                if !was_expanded {
                    host2.expanded.borrow_mut().insert(wid.clone());
                    host_render(&doc2, &host2, &actions2);
                    return;
                }
                // Always collapse on the second click.
                host2.expanded.borrow_mut().remove(&wid);
                if !already {
                    // Daemon-side, seconds-slow; result arrives as a toast.
                    actions2.host_select(&wid, &iid);
                }
                host_render(&doc2, &host2, &actions2);
            });
        }
        let _ = root.append_child(&group);
    }
    let stale = std::mem::replace(&mut *host.clicks.borrow_mut(), fresh);
    drop_later(stale, Vec::new());
}

// ── ✦ census popover ────────────────────────────────────────────────────

/// One window in the census: `(id, label, workspace_count)`.
type WindowRow = (String, String, usize);

/// Tear the popover down: drop the card, release the click-away listener and
/// retire the popover-scoped closures. Safe to call from *inside* one of those
/// closures — they're freed in a later task (`drop_later`).
fn gui_menu_close(
    open: &Rc<Cell<bool>>,
    clicks: &ClickSink,
    dismiss: &Rc<RefCell<Option<ClickClosure>>>,
) {
    open.set(false);
    let doc = web_sys::window().and_then(|w| w.document());
    if let Some(doc) = doc.as_ref() {
        if let Ok(Some(card)) = doc.query_selector(".gui-menu") {
            card.remove();
        }
    }
    let mut stale: Vec<ClickClosure> = std::mem::take(&mut *clicks.borrow_mut());
    if let Some(cb) = dismiss.borrow_mut().take() {
        if let Some(doc) = doc.as_ref() {
            let _ =
                doc.remove_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref());
        }
        stale.push(cb);
    }
    drop_later(stale, Vec::new());
}

/// Build the popover under the brand header: one row per connected GUI window
/// (label + circle count), a `kill` affordance for every window but this one
/// (`CloseWindow` — the daemon unregisters it and the client quits on
/// `Kicked`), then the version + grimoire footer.
#[allow(clippy::too_many_arguments)]
fn gui_menu_render(
    doc: &Document,
    host: &Element,
    rows: &[WindowRow],
    self_id: Option<&str>,
    actions: &Rc<dyn Actions>,
    open: &Rc<Cell<bool>>,
    clicks: &ClickSink,
    dismiss: &Rc<RefCell<Option<ClickClosure>>>,
) -> Result<(), JsValue> {
    // Any previous card (and its listeners) goes first.
    if let Ok(Some(old)) = host.query_selector(".gui-menu") {
        old.remove();
    }
    drop_later(std::mem::take(&mut *clicks.borrow_mut()), Vec::new());

    let card = mk(doc, "div", "gui-menu")?;
    card.append_child(text_el(doc, "div", "gui-menu-head", "connected guis")?.unchecked_ref())?;

    for (id, label, count) in rows {
        let row = mk(doc, "div", "gui-menu-row")?;
        row.append_child(text_el(doc, "span", "gui-menu-label", label)?.unchecked_ref())?;
        let circles = if *count == 1 {
            "1 circle".to_string()
        } else {
            format!("{count} circles")
        };
        row.append_child(text_el(doc, "span", "gui-menu-count", &circles)?.unchecked_ref())?;
        if Some(id.as_str()) == self_id {
            row.append_child(
                text_el(doc, "span", "gui-menu-self", "(this window)")?.unchecked_ref(),
            )?;
        } else {
            let kill = text_el(doc, "button", "gui-menu-kill", "kill")?;
            kill.set_attribute("title", &format!("close «{label}»"))?;
            row.append_child(kill.unchecked_ref())?;
            let actions = actions.clone();
            let id = id.clone();
            let open = open.clone();
            let clicks2 = clicks.clone();
            let dismiss2 = dismiss.clone();
            bind_click(&kill, &mut clicks.borrow_mut(), move |ev| {
                ev.stop_propagation();
                actions.send(GuiRequest::CloseWindow { window: id.clone() });
                gui_menu_close(&open, &clicks2, &dismiss2);
            })?;
        }
        card.append_child(&row)?;
    }

    card.append_child(mk(doc, "div", "gui-menu-sep")?.unchecked_ref())?;
    card.append_child(
        text_el(
            doc,
            "div",
            "gui-menu-foot",
            concat!("seance ", env!("CARGO_PKG_VERSION")),
        )?
        .unchecked_ref(),
    )?;

    let help = mk(doc, "div", "gui-menu-row gui-menu-help")?;
    help.append_child(text_el(doc, "span", "gui-menu-label", "grimoire")?.unchecked_ref())?;
    help.append_child(text_el(doc, "span", "gui-menu-count", "?")?.unchecked_ref())?;
    card.append_child(&help)?;
    {
        let actions = actions.clone();
        let open = open.clone();
        let clicks2 = clicks.clone();
        let dismiss2 = dismiss.clone();
        bind_click(&help, &mut clicks.borrow_mut(), move |ev| {
            ev.stop_propagation();
            actions.toggle_help();
            gui_menu_close(&open, &clicks2, &dismiss2);
        })?;
    }

    host.append_child(&card)?;

    // Click-away. `.brand-mark` is excluded so pressing ✦ again reaches its own
    // click handler and toggles cleanly instead of double-closing.
    if dismiss.borrow().is_none() {
        let open2 = open.clone();
        let clicks2 = clicks.clone();
        let dismiss2 = dismiss.clone();
        let cb = Closure::wrap(Box::new(move |ev: MouseEvent| {
            let inside = ev
                .target()
                .and_then(|t| t.dyn_into::<Element>().ok())
                .and_then(|e| e.closest(".gui-menu, .brand-mark").ok().flatten())
                .is_some();
            if !inside {
                gui_menu_close(&open2, &clicks2, &dismiss2);
            }
        }) as Box<dyn FnMut(MouseEvent)>);
        doc.add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref())?;
        *dismiss.borrow_mut() = Some(cb);
    }
    Ok(())
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
    el.set_attribute("spellcheck", "false")?;
    Ok(el)
}

fn bind(
    target: &Element,
    event: &str,
    sink: &mut Vec<ClickClosure>,
    mut f: impl FnMut(MouseEvent) + 'static,
) -> Result<(), JsValue> {
    let cb = Closure::wrap(Box::new(move |e: MouseEvent| f(e)) as Box<dyn FnMut(MouseEvent)>);
    target.add_event_listener_with_callback(event, cb.as_ref().unchecked_ref())?;
    sink.push(cb);
    Ok(())
}

fn bind_click(
    target: &Element,
    sink: &mut Vec<ClickClosure>,
    f: impl FnMut(MouseEvent) + 'static,
) -> Result<(), JsValue> {
    bind(target, "click", sink, f)
}

fn bind_ctx(
    target: &Element,
    sink: &mut Vec<ClickClosure>,
    f: impl FnMut(MouseEvent) + 'static,
) -> Result<(), JsValue> {
    bind(target, "contextmenu", sink, f)
}

fn bind_key(
    target: &Element,
    sink: &mut Vec<KeyClosure>,
    mut f: impl FnMut(KeyboardEvent) + 'static,
) -> Result<(), JsValue> {
    let cb = Closure::wrap(Box::new(move |e: KeyboardEvent| f(e)) as Box<dyn FnMut(KeyboardEvent)>);
    target.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref())?;
    sink.push(cb);
    Ok(())
}

/// Free superseded closures in a later task. A strip that re-renders from
/// inside one of its own listeners would otherwise drop the closure that is
/// currently on the stack.
fn drop_later(clicks: Vec<ClickClosure>, keys: Vec<KeyClosure>) {
    if clicks.is_empty() && keys.is_empty() {
        return;
    }
    let cb = Closure::once_into_js(move || {
        drop(clicks);
        drop(keys);
    });
    if let Some(w) = web_sys::window() {
        let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 0);
    }
}

/// Two-click destructive confirm (WEB DIVERGENCE #5). The first click arms the
/// button (`.armed`), a second within 2s fires; a timer disarms it visually.
fn arm_or_fire(
    win: &Window,
    armed: &Rc<RefCell<HashMap<String, f64>>>,
    key: &str,
    btn: &Element,
    base_class: &str,
    fire: impl FnOnce(),
) {
    let now = js_sys::Date::now();
    let previously = armed.borrow().get(key).copied();
    match previously {
        Some(t) if now - t <= KILL_CONFIRM_MS => {
            armed.borrow_mut().remove(key);
            btn.set_class_name(base_class);
            fire();
        }
        _ => {
            armed.borrow_mut().insert(key.to_string(), now);
            btn.set_class_name(&format!("{base_class} armed"));
            let armed2 = armed.clone();
            let btn2 = btn.clone();
            let key2 = key.to_string();
            let base = base_class.to_string();
            let cb = Closure::once_into_js(move || {
                let stale = armed2
                    .borrow()
                    .get(&key2)
                    .map(|t| js_sys::Date::now() - *t >= KILL_CONFIRM_MS)
                    .unwrap_or(false);
                if stale {
                    armed2.borrow_mut().remove(&key2);
                    btn2.set_class_name(&base);
                }
            });
            let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.unchecked_ref(),
                KILL_CONFIRM_MS as i32 + 50,
            );
        }
    }
}

/// Workspace glyph: `◆` selected / working, `◈` idle (native sidebar).
fn ws_glyph_char(working: bool, selected: bool) -> &'static str {
    if working || selected {
        "◆"
    } else {
        "◈"
    }
}

/// WEB DIVERGENCE #1: `.working` is a CSS pulse standing in for the native
/// braille frame cycle.
fn ws_glyph_class(working: bool, selected: bool) -> &'static str {
    if working {
        "ws-glyph working"
    } else if selected {
        "ws-glyph selected"
    } else {
        "ws-glyph idle"
    }
}

/// Attention text badge — needs/done only; working is the left glyph, and the
/// selected circle never shows one (native: `if selected { None }`).
fn ws_att(att: Option<Attention>, selected: bool) -> (&'static str, &'static str) {
    if selected {
        return ("", "ws-att hidden");
    }
    match att {
        Some(Attention::NeedsHuman) => (Attention::NeedsHuman.label(), "ws-att needs"),
        Some(Attention::Done) => (Attention::Done.label(), "ws-att done"),
        _ => ("", "ws-att hidden"),
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
fn origin_badge(state: &ClientState, pane: &PaneInfo) -> (String, &'static str) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> QlEntry {
        QlEntry {
            name: name.into(),
            cwd: None,
            command: None,
        }
    }

    fn names(v: &[QlEntry]) -> Vec<&str> {
        v.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn upsert_appends_or_replaces_in_place() {
        let mut v = vec![entry("a"), entry("b")];
        ql_upsert(&mut v, None, entry("c"));
        assert_eq!(names(&v), ["a", "b", "c"]);
        ql_upsert(
            &mut v,
            Some("b"),
            QlEntry {
                name: "b2".into(),
                cwd: Some("~/x".into()),
                command: None,
            },
        );
        assert_eq!(names(&v), ["a", "b2", "c"]);
        assert_eq!(v[1].cwd.as_deref(), Some("~/x"));
    }

    #[test]
    fn collision_excludes_the_edited_entry() {
        let v = vec![entry("a"), entry("b")];
        assert!(ql_name_collides(&v, "a", None));
        assert!(!ql_name_collides(&v, "c", None));
        assert!(!ql_name_collides(&v, "a", Some("a")));
        assert!(ql_name_collides(&v, "b", Some("a")));
    }

    #[test]
    fn shift_clamps_at_the_ends() {
        let mut v = vec![entry("a"), entry("b"), entry("c")];
        ql_shift(&mut v, "c", -1);
        assert_eq!(names(&v), ["a", "c", "b"]);
        ql_shift(&mut v, "a", -1);
        assert_eq!(names(&v), ["a", "c", "b"]);
        ql_shift(&mut v, "b", 1);
        assert_eq!(names(&v), ["a", "c", "b"]);
        ql_shift(&mut v, "ghost", 1);
        assert_eq!(names(&v), ["a", "c", "b"]);
    }

    #[test]
    fn parse_matches_the_native_config_shape() {
        // A legacy "workspace" key must still parse (ignored since 0.9.20).
        let v: Vec<QlEntry> = serde_json::from_str(
            r#"[{"name":"vita","cwd":"~/work/vita","command":"claude","workspace":"vita"},
                {"name":"scratch"}]"#,
        )
        .unwrap();
        assert_eq!(v[0].command.as_deref(), Some("claude"));
        assert!(v[1].cwd.is_none() && v[1].command.is_none());
        // Round-trip stays parseable (skip_serializing_if keeps it compact).
        let json = serde_json::to_string(&vec![entry("scratch")]).unwrap();
        assert_eq!(json, r#"[{"name":"scratch"}]"#);
    }

    #[test]
    fn glyph_and_attention_match_the_native_rules() {
        assert_eq!(ws_glyph_char(false, true), "◆");
        assert_eq!(ws_glyph_char(true, false), "◆");
        assert_eq!(ws_glyph_char(false, false), "◈");
        assert_eq!(ws_glyph_class(true, true), "ws-glyph working");
        // Selected rows never show a text badge; working is the glyph.
        assert_eq!(ws_att(Some(Attention::NeedsHuman), true).0, "");
        assert_eq!(ws_att(Some(Attention::Working), false).0, "");
        assert_eq!(ws_att(Some(Attention::NeedsHuman), false).0, "needs");
        assert_eq!(ws_att(Some(Attention::Done), false).0, "done");
    }
}
