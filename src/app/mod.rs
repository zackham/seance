//! SeanceApp: root view. Left sidebar (panes grouped by workspace),
//! auto-tiling terminal region, per-pane notes flip, control-plane pump.
//!
//! # Notes = flip the pane
//! Notes are the *back* of a pane, not a side drawer. Click ✎ (or
//! ctrl+shift+s) to flip the pane over onto its shared scratchpad; click
//! again (or the ↻ chip) to flip back. The agent sees the same file via
//! `$SEANCE_SCRATCHPAD`.

use std::time::Duration;

use gpui::{div, prelude::*, px, Context, Entity, FocusHandle, Focusable as _, Window};
use gpui_component::{
    input::{InputEvent, InputState},
    ActiveTheme as _, WindowExt as _,
};

use crate::{
    control::{ControlRequest, ControlResponse},
    gui_client::GuiClient,
    pane::{Pane, PaneBody, SpawnRequest},
    remote_term::RemoteTerminal,
    remote_term_view::RemoteTerminalView,
    runtime::protocol::{GuiEvent, PaneInfo, WindowInfo},
    runtime::snapshot::GridSnapshot,
    scratchpad::ScratchpadDrawer,
    theme::SeancePalette,
};
use std::sync::Arc;

pub(crate) mod actions;
mod chrome;
mod layout;
mod menus;
mod overview;
mod pads;
mod palette;
mod prboard;
mod prlinks;
mod quicklaunch;
mod sidebar;
mod tiles;
mod util;
mod workspaces;

use self::actions::*;
use self::chrome::*;
use self::layout::*;
use self::quicklaunch::QuickLaunchEntry;
use self::util::*;
use self::workspaces::WorkspaceAttention;

/// What's being renamed inline in the sidebar.
#[derive(Clone)]
enum RenameTarget {
    Pane(String),
    Workspace(String),
}

/// What the right drawer shows. Notes live on the *back of a pane* now
/// (see `flipped`); drawer is activity feed + stage pad inspector.
enum Drawer {
    Closed,
    Activity,
    /// Live pad + task envelope for a pane (stage chip / pad chip).
    Pad {
        slug: String,
    },
}

/// Overlay palette (precanned prompts or fuzzy jump).
enum PaletteMode {
    Closed,
    Prompts { query: String, selected: usize },
    Jump { query: String, selected: usize },
}

/// A question an agent asked the human, awaiting an answer.
pub struct PendingAsk {
    pub id: String,
    pub from: String,
    pub workspace: Option<String>,
    pub question: String,
    pub choices: Vec<String>,
    pub answer: Option<String>,
}

/// Agent-reported pane status (planning|working|blocked|needs-human|done|idle).
#[derive(Clone)]
pub struct PaneStatus {
    pub state: String,
    pub note: Option<String>,
}

/// Co-presence chrome for a pane (mirrors daemon agency).
#[derive(Clone, Debug)]
struct OwnerChrome {
    owner: String,
    /// Plumbed from daemon Agency events; not yet rendered (pair/agent badge TODO).
    #[allow(dead_code)]
    drive_mode: String,
    exited: bool,
    exit_code: Option<i32>,
}

pub struct SeanceApp {
    panes: Vec<Pane>,
    asks: Vec<PendingAsk>,
    statuses: std::collections::HashMap<String, PaneStatus>,
    /// Co-presence ownership from daemon Agency events / State.
    owners: std::collections::HashMap<String, OwnerChrome>,
    /// (pane slug -> (verb, actor, when)) — transient "driven by X" flashes.
    touches: std::collections::HashMap<String, (String, String, std::time::Instant)>,
    /// Active whisper compose bar: (pane slug, input state).
    whisper: Option<(String, Entity<InputState>)>,
    /// Pane currently flipped to its notes face: (slug, scratchpad entity).
    flipped: Option<(String, Entity<ScratchpadDrawer>)>,
    active_slug: Option<String>,
    selected_workspace: Option<String>,
    /// Last focused pane slug per workspace — restored on workspace switch.
    workspace_focus: std::collections::HashMap<String, String>,
    /// Where this window has *been*, for the mouse's back/forward buttons.
    /// Kept by watching `selected_workspace` once per render — see
    /// [`Self::sync_nav_history`].
    nav: workspaces::NavHistory,
    extra_workspaces: Vec<String>,
    workspace_order: Vec<String>,
    renaming: Option<(RenameTarget, Entity<InputState>)>,
    drawer: Drawer,
    focus_handle: FocusHandle,
    session_counter: usize,
    /// Connection to the session daemon (owns PTYs).
    client: Arc<GuiClient>,
    /// After a summon, focus this pane once its remote view exists.
    pending_focus: Option<String>,
    /// Sidebar workspace-list scroll — cycling must reveal the selection.
    sidebar_scroll: gpui::ScrollHandle,
    /// UI-initiated spawn/create: open the inline rename field as soon as the
    /// target exists (workspace is immediate; pane waits for PaneSpawned).
    pending_rename: Option<RenameTarget>,
    /// Next `PaneSpawned` from our summon should open rename (not external ctl).
    rename_next_spawn: bool,
    /// Set by [`Self::apply_grid_snap`] when a grid landed on a VISIBLE pane;
    /// the event-batch loop reads+clears it to decide whether to kick a frame.
    grid_batch_visible: bool,
    /// Focus-zoom: only this pane fills the tile region (None = normal grid).
    zoomed_slug: Option<String>,
    /// Overlay palette (ctrl+shift+k prompts / ctrl+shift+j jump).
    palette: PaletteMode,
    /// Horizontal split ratio for 2-pane layout (0.2–0.8). Used when n==2.
    split_ratio: f32,
    /// Per-pane flex weights for n>2 tile resize (slug → weight).
    pane_weights: std::collections::HashMap<String, f32>,
    /// Per-row flex weights for multi-row grids (row key → weight).
    row_weights: std::collections::HashMap<String, f32>,
    /// Dragging sash: (left_slug, right_slug) for multi-pane, or 2-pane marker.
    sash_drag: Option<SashDrag>,
    /// Pad drawer live-refresh generation (bumped on timer / events).
    pad_refresh_tick: u64,
    /// Optional host-bridge widgets (claude accounts, …) — fail closed.
    host: crate::host::HostState,
    /// Host widget ids currently expanded to show every account (collapsed =
    /// current account only).
    host_expanded: std::collections::HashSet<String>,
    /// This GUI connection's window id (from daemon State).
    window_id: Option<String>,
    /// Live windows (multiplayer roster).
    windows: Vec<WindowInfo>,
    /// This window's subscription set, daemon order. State arrives GLOBAL
    /// (every workspace, every pane) and is kept whole — the sidebar renders
    /// the active/parked split locally.
    subscriptions: Vec<String>,
    /// Persisted per-GUI presentation state: which workspaces sit in the
    /// active band, and which this window has ever looked at (`seen`).
    subs_pref: crate::subscriptions_pref::SubscriptionsPref,
    /// A cached list was found (or the first State already seeded one).
    /// Until then, the first State's subscription set becomes the active list.
    subs_seeded: bool,
    /// We adopted the daemon's arrangement and this connection is still
    /// attached on a different set. Inverts the next `State`: bring the
    /// connection to the arrangement instead of folding the connection's
    /// subscriptions into it. Cleared once that reconcile has run.
    rail_from_daemon: bool,
    /// Workspaces parked locally whose `Unsubscribe` may still be in flight —
    /// a State composed before it landed must not re-activate them.
    park_pending: std::collections::BTreeSet<String>,
    /// Last activity timestamp (ms) per workspace — input/inject/status, not click.
    workspace_touch: std::collections::HashMap<String, u64>,
    /// Last observed pane output per workspace (ms) — sidebar shows "time
    /// since last update" instead of a pane count.
    workspace_activity: std::collections::HashMap<String, u64>,
    /// Per-pane deadline (ms) until which grid content changes are treated as
    /// resize reflow, not output. Armed whenever a frame arrives at new dims.
    resize_settle: std::collections::HashMap<String, u64>,
    /// Panes the DAEMON says are streaming right now. Authoritative: grid
    /// frames (and the titles inside them) only arrive for the selected
    /// workspace, so a locally-derived spinner freezes on every other circle.
    /// Seeded from `PaneInfo::busy`, kept live by `GuiEvent::PaneBusy`.
    busy_panes: std::collections::HashSet<String>,
    /// Workspaces that currently have a live-working agent (for falling-edge
    /// touch when work finishes → top of the non-working band).
    workspace_was_working: std::collections::HashSet<String>,
    /// slug → display label, mirrored from `WorkspaceMeta.name`. A circle
    /// absent here reads as its slug — which is what every circle reads as
    /// until someone renames it. Nothing else in this struct is keyed by the
    /// label, so a rename disturbs none of it.
    workspace_names: std::collections::HashMap<String, String>,
    /// Sticky attention on inactive circles until selected (done/needs).
    workspace_unread: std::collections::HashMap<String, WorkspaceAttention>,
    /// Full-window live overview (ctrl+shift+space).
    overview: bool,
    /// This window attached with an empty subscription set (second process).
    empty_window: bool,
    /// Quicklaunch strip entries (~/.config/seance/quicklaunch.json).
    quicklaunch: Vec<QuickLaunchEntry>,
    /// Daemon-side mtime_ms of the config at last load — reload only on
    /// change (None = file absent / never fetched).
    quicklaunch_mtime: Option<u64>,
    /// Last stat check — throttles the mtime probe to every ~2s.
    quicklaunch_checked: Option<std::time::Instant>,
    /// Open quicklaunch create/edit modal (None = closed).
    quicklaunch_editor: Option<quicklaunch::QuickLaunchEditor>,
    /// Host-provided menus (`menus[]` of ~/.config/seance/host.json) — chips
    /// that run a list command on click instead of being polled.
    host_menus: Vec<crate::host::HostMenuConfig>,
    /// Daemon-side mtime_ms of host.json at last menu load (see quicklaunch).
    host_menus_mtime: Option<u64>,
    host_menus_checked: Option<std::time::Instant>,
    /// The one open menu dropdown, if any.
    host_menu: Option<menus::HostMenuOpen>,
    /// Monotonic open counter — stale list results check it and drop out.
    host_menu_token: u64,
    /// `✦` popover: census of the GUI windows attached to this daemon.
    gui_menu_open: bool,
    /// Daemon-scraped PR links per workspace (most-recently-seen LAST) —
    /// a pure mirror of `WorkspaceMeta.pr_links`, rebuilt on every State.
    pr_links: std::collections::HashMap<String, Vec<seance_core::protocol::PrLink>>,
    /// URL of the PR chip whose details popover is showing (hover-driven,
    /// zero delay). `None` = no popover.
    pr_tip: Option<String>,
    /// The popover is pinned open because that chip's context menu is up —
    /// hover-out must not close it while the menu is being used.
    pr_tip_pinned: bool,
    /// Full-content PR board overlay (sidebar `PRs (N)` button).
    pr_board: bool,
    /// Render-safe cache of daemon-side files (pad sidecars, phone binds,
    /// prompt library) — refreshed by a ~2s background loop.
    remote_cache: Arc<crate::remote_cache::RemoteCache>,
    render_probe: RenderProbe,
}

/// Active sash drag state.
#[derive(Clone)]
enum SashDrag {
    /// Classic 2-pane ratio drag.
    TwoPane,
    /// Adjacent panes in a multi-pane row (horizontal sash).
    Pair {
        left: String,
        right: String,
        start_x: f32,
        left_w: f32,
        right_w: f32,
    },
    /// Adjacent grid rows (vertical sash).
    RowPair {
        above_key: String,
        below_key: String,
        start_y: f32,
        above_w: f32,
        below_w: f32,
    },
}

/// Env-gated (`SEANCE_DEBUG_RENDER=1`) frame-rate probe: counts render()
/// entries and reports every ~5s so a notify storm is visible in the gui log.
#[derive(Default)]
struct RenderProbe {
    count: u64,
    window_start: Option<std::time::Instant>,
    /// name → (total, max, samples) since last report.
    sections:
        std::collections::HashMap<&'static str, (std::time::Duration, std::time::Duration, u64)>,
    enabled: Option<bool>,
}

impl RenderProbe {
    fn enabled(&mut self) -> bool {
        *self
            .enabled
            .get_or_insert_with(|| std::env::var_os("SEANCE_DEBUG_RENDER").is_some())
    }

    fn add(&mut self, name: &'static str, dt: std::time::Duration) {
        if !self.enabled() {
            return;
        }
        let e = self.sections.entry(name).or_insert((
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            0,
        ));
        e.0 += dt;
        e.1 = e.1.max(dt);
        e.2 += 1;
    }

    fn tick(&mut self) {
        if !self.enabled() {
            return;
        }
        let now = std::time::Instant::now();
        let start = *self.window_start.get_or_insert(now);
        self.count += 1;
        let elapsed = now.duration_since(start);
        if elapsed >= std::time::Duration::from_secs(5) {
            let mut parts: Vec<String> = self
                .sections
                .iter()
                .map(|(k, (tot, max, n))| {
                    format!(
                        "{k} avg {:.1}ms max {:.1}ms",
                        tot.as_secs_f64() * 1000.0 / (*n).max(1) as f64,
                        max.as_secs_f64() * 1000.0
                    )
                })
                .collect();
            parts.sort();
            eprintln!(
                "[seance render-probe] {:.1} renders/s over {:.1}s · {}",
                self.count as f64 / elapsed.as_secs_f64(),
                elapsed.as_secs_f64(),
                parts.join(" · ")
            );
            self.count = 0;
            self.sections.clear();
            self.window_start = Some(now);
        }
    }
}

impl SeanceApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_inner(window, cx, false)
    }

    /// Empty window: subscribes to no workspaces until one is selected.
    /// A second OS window that subscribes to nothing. Its only caller
    /// ("send to new window") went with the ownership model; phase 2's
    /// active/parked sidebar re-wires it as "open a window here".
    #[allow(dead_code)]
    pub fn new_empty_window(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_inner(window, cx, true)
    }

    fn new_inner(window: &mut Window, cx: &mut Context<Self>, empty: bool) -> Self {
        // Connect to the session daemon (PTYs live there). The persisted
        // active list must be read BEFORE connecting — it seeds `Attach`.
        let pref = if empty {
            None
        } else {
            crate::subscriptions_pref::load()
        };
        let seed: Option<Vec<String>> = pref.as_ref().map(|p| p.active.iter().cloned().collect());
        let (client, event_rx) = if empty {
            GuiClient::connect_empty().expect("gui client connect empty")
        } else {
            GuiClient::connect(seed).expect("gui client connect to daemon")
        };
        // `connect()` decides blank-window on its own (second process /
        // SEANCE_EMPTY_WINDOW); such a window must never persist a list.
        let empty = empty || client.is_empty_window();
        let subs_seeded = pref.is_some() && !empty;
        let remote_cache = Arc::new(crate::remote_cache::RemoteCache::new(Arc::clone(&client)));

        let mut app = SeanceApp {
            panes: Vec::new(),
            asks: Vec::new(),
            statuses: std::collections::HashMap::new(),
            owners: std::collections::HashMap::new(),
            touches: std::collections::HashMap::new(),
            whisper: None,
            flipped: None,
            active_slug: None,
            selected_workspace: None,
            workspace_focus: std::collections::HashMap::new(),
            extra_workspaces: Vec::new(),
            workspace_order: Vec::new(),
            renaming: None,
            drawer: Drawer::Closed,
            focus_handle: cx.focus_handle(),
            session_counter: 0,
            client,
            pending_focus: None,
            sidebar_scroll: gpui::ScrollHandle::new(),
            pending_rename: None,
            rename_next_spawn: false,
            grid_batch_visible: false,
            zoomed_slug: None,
            palette: PaletteMode::Closed,
            split_ratio: 0.5,
            pane_weights: std::collections::HashMap::new(),
            row_weights: std::collections::HashMap::new(),
            sash_drag: None,
            pad_refresh_tick: 0,
            host: crate::host::HostState::load(),
            host_expanded: std::collections::HashSet::new(),
            window_id: None,
            windows: Vec::new(),
            subscriptions: Vec::new(),
            subs_pref: pref.unwrap_or_default(),
            subs_seeded,
            rail_from_daemon: false,
            park_pending: std::collections::BTreeSet::new(),
            workspace_touch: std::collections::HashMap::new(),
            workspace_activity: std::collections::HashMap::new(),
            resize_settle: std::collections::HashMap::new(),
            busy_panes: std::collections::HashSet::new(),
            workspace_was_working: std::collections::HashSet::new(),
            workspace_names: std::collections::HashMap::new(),
            workspace_unread: std::collections::HashMap::new(),
            overview: false,
            empty_window: empty,
            quicklaunch: Vec::new(),
            quicklaunch_mtime: None,
            quicklaunch_checked: None,
            quicklaunch_editor: None,
            host_menus: Vec::new(),
            host_menus_mtime: None,
            host_menus_checked: None,
            host_menu: None,
            host_menu_token: 0,
            gui_menu_open: false,
            pr_links: std::collections::HashMap::new(),
            pr_tip: None,
            pr_tip_pinned: false,
            pr_board: false,
            remote_cache,
            render_probe: RenderProbe::default(),
            nav: workspaces::NavHistory::default(),
        };
        // Seed the prompt-library user file on the DAEMON machine (write-if-
        // missing) and pre-warm the cache so the palette has user prompts by
        // the first refresh tick. Plain thread: bridge calls block.
        {
            let client = Arc::clone(&app.client);
            let cache = Arc::clone(&app.remote_cache);
            std::thread::Builder::new()
                .name("seance-prompts-seed".into())
                .spawn(move || {
                    let path = crate::prompts::remote_config_path();
                    if let Ok(None) = client.fs_stat(&path) {
                        let _ = client
                            .fs_write(&path, crate::prompts::default_user_file_json().as_bytes());
                    }
                    let _ = cache.fetch_now(&path);
                })
                .ok();
        }
        // Shared layout lives daemon-side (thin clients see the same tiling).
        // One blocking bridge call at boot; defaults on any failure.
        let (split, weights, row_weights) = load_layout_json(
            app.client
                .layout_load()
                .ok()
                .flatten()
                .as_deref()
                .unwrap_or(""),
        );
        app.split_ratio = split;
        app.pane_weights = weights;
        app.row_weights = row_weights;

        // The rail arrangement is daemon-owned too (0.23). The local file read
        // before connecting was only the `Attach` seed; whatever the daemon
        // holds is the arrangement, and a window that disagrees is the one
        // that's wrong. Same blocking-call-at-boot shape as the layout above.
        if !app.empty_window {
            match app.client.subs_load() {
                Ok(Some(json)) => {
                    if let Some(pref) = crate::subscriptions_pref::parse(&json) {
                        app.subs_pref = pref;
                        // Don't let the first `State` seed or fold anything in:
                        // this connection attached on a different set, so the
                        // connection follows the arrangement, not vice versa.
                        app.subs_seeded = true;
                        app.rail_from_daemon = true;
                        crate::subscriptions_pref::save(&app.subs_pref);
                        app.client
                            .set_subscription_seed(app.subs_pref.active.iter().cloned().collect());
                    }
                }
                // A daemon with no copy yet — first window up after the
                // upgrade donates its arrangement instead of everyone
                // starting from a blank rail.
                Ok(None) => app.push_rail_to_daemon(),
                Err(e) => eprintln!("[seance gui] rail prefs load failed: {e}"),
            }
        }

        // Host widgets are polled daemon-side and arrive as HostWidgets
        // pushes (thin clients see the daemon machine's chips) — no GUI poll.

        // Bridge: std thread blocks on daemon events → unbounded mpsc → gpui task.
        // Events are timestamped at the bridge so queue age is measurable at
        // apply time ([seance lat] "gui bridge age").
        let (async_tx, mut async_rx) =
            futures::channel::mpsc::unbounded::<(std::time::Instant, GuiEvent)>();
        std::thread::Builder::new()
            .name("seance-gui-events".into())
            .spawn(move || {
                while let Ok(ev) = event_rx.recv() {
                    if async_tx
                        .unbounded_send((std::time::Instant::now(), ev))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .ok();

        // Batch-drain: one `update` (→ one render cycle) applies EVERYTHING
        // queued, instead of one update per event. Under grid streams the old
        // per-event loop interleaved a render cycle between events, so a
        // keystroke's echo waited behind backlog × frame-cost (measured p95
        // 198ms bridge→apply). Cap keeps one pathological burst from wedging
        // the main thread for unbounded time.
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(first) = async_rx.next().await {
                let mut batch = vec![first];
                while batch.len() < 512 {
                    match async_rx.try_recv() {
                        Ok(ev) => batch.push(ev),
                        Err(_) => break, // empty or closed — apply what we have
                    }
                }
                let Some(this) = this.upgrade() else { break };
                this.update(cx, |app: &mut SeanceApp, cx| {
                    app.grid_batch_visible = false;
                    for (t, ev) in batch.drain(..) {
                        crate::latency_probe::record(
                            "gui bridge age",
                            t.elapsed().as_micros() as u64,
                        );
                        app.apply_gui_event_no_window(ev, cx);
                    }
                    let grids = app.grid_batch_visible;
                    // Pane-entity notify does NOT schedule a window frame in
                    // this gpui build — measured: grids applied between spinner
                    // ticks sat unpainted until the next 240ms tick
                    // (apply→paint p50 ~68ms, render gap pinned at 233–250ms).
                    // A root notify is what actually produces a frame, so kick
                    // one per applied grid batch: immediately while a human
                    // keystroke is in flight (echo latency), at most ~30fps
                    // otherwise (stream smoothness without per-event churn).
                    if grids {
                        static LAST_KICK: std::sync::Mutex<Option<std::time::Instant>> =
                            std::sync::Mutex::new(None);
                        let now = std::time::Instant::now();
                        let mut g = LAST_KICK.lock().unwrap();
                        let due = crate::term_shared::typing_hot()
                            || g.is_none_or(|t| now.duration_since(t).as_millis() >= 33);
                        if due {
                            *g = Some(now);
                            // Frame-scheduling gauge: root notify → render()
                            // ("gui kick→render") — must be ~1 vsync for the
                            // typing-hot path to meet the latency target.
                            crate::latency_probe::mark("g_kick", "app");
                            cx.notify();
                        } else {
                            // Throttled — arm ONE deferred kick so the final
                            // frame of a burst paints within ~33ms instead of
                            // waiting for the next 240ms spinner tick.
                            static KICK_ARMED: std::sync::atomic::AtomicBool =
                                std::sync::atomic::AtomicBool::new(false);
                            use std::sync::atomic::Ordering;
                            if !KICK_ARMED.swap(true, Ordering::Relaxed) {
                                cx.spawn(async move |this, cx| {
                                    cx.background_executor()
                                        .timer(Duration::from_millis(33))
                                        .await;
                                    KICK_ARMED.store(false, Ordering::Relaxed);
                                    if let Some(this) = this.upgrade() {
                                        this.update(cx, |_, cx| cx.notify());
                                    }
                                })
                                .detach();
                            }
                        }
                    }
                });
            }
        })
        .detach();

        // Remote-cache refresh loop: every 2s, pull wanted daemon files on
        // the background executor; repaint only when something changed.
        let cache = Arc::clone(&app.remote_cache);
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(2000))
                .await;
            let c = Arc::clone(&cache);
            let changed = cx
                .background_executor()
                .spawn(async move { c.refresh() })
                .await;
            let Some(this) = this.upgrade() else { break };
            if changed {
                this.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();

        // Live-refresh pad drawer every 2s while open (disk mtime/content).
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(2000))
                .await;
            let Some(this) = this.upgrade() else { break };
            this.update(cx, |app: &mut SeanceApp, cx| {
                if matches!(app.drawer, Drawer::Pad { .. }) {
                    app.pad_refresh_tick = app.pad_refresh_tick.wrapping_add(1);
                    cx.notify();
                }
            });
        })
        .detach();

        // Sidebar working-spinner animation while any circle is live-busy.
        // NOT cheap: each notify re-renders the whole window (~55ms at 24
        // circles) — 240ms keeps the glyph alive without eating the thread.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(240))
                .await;
            let Some(this) = this.upgrade() else { break };
            this.update(cx, |app: &mut SeanceApp, cx| {
                if !app.workspace_was_working.is_empty() {
                    cx.notify();
                }
            });
        })
        .detach();

        // gpui frame tracing → [seance lat] "gpui draw": true Window::draw
        // cost (layout+prepaint+paint), the number element construction
        // probes can't see. Cheap ring buffer; drained every 5s.
        gpui::profiler::set_frame_trace_enabled(true);
        let mut frame_collector = gpui::profiler::FrameTimingCollector::new();
        cx.spawn(async move |_this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(5000))
                .await;
            for t in frame_collector.collect_unseen() {
                crate::latency_probe::record("gpui draw", t.draw_duration().as_micros() as u64);
            }
        })
        .detach();

        let _ = window;
        app
    }

    fn apply_gui_event_no_window(&mut self, ev: GuiEvent, cx: &mut Context<Self>) {
        // Most handlers don't need a real Window; ensure_remote_pane only
        // needs cx for entity creation.
        match ev {
            GuiEvent::State {
                panes,
                selected_workspace,
                focused_pane,
                extra_workspaces,
                workspace_order,
                asks,
                statuses,
                window_id,
                windows,
                subscriptions,
                workspace_meta,
            } => {
                // Multi-window identity + peer roster.
                if let Some(id) = window_id {
                    self.window_id = Some(id);
                }
                self.windows = windows;
                self.subscriptions = subscriptions;

                // State is global from 0.12 — every workspace, every pane. The
                // active/parked split is presentation state this window owns,
                // so nothing is dropped here; the sidebar renders the split.
                let known: std::collections::BTreeSet<String> = panes
                    .iter()
                    .map(|p| p.workspace.clone())
                    .chain(extra_workspaces.iter().cloned())
                    .chain(workspace_order.iter().cloned())
                    .chain(workspace_meta.iter().map(|m| m.workspace.clone()))
                    .chain(selected_workspace.iter().cloned())
                    .collect();
                self.reconcile_subscriptions(&known);

                // Re-seed busy from the daemon's verdict — a full state push is
                // the resync point for panes whose flips we may have missed
                // (reconnect, upgrade handoff).
                self.busy_panes = panes
                    .iter()
                    .filter(|p| p.busy)
                    .map(|p| p.slug.clone())
                    .collect();

                self.selected_workspace = selected_workspace;
                self.active_slug = focused_pane;
                self.extra_workspaces = extra_workspaces;
                self.workspace_order = workspace_order;
                self.asks = asks
                    .into_iter()
                    .map(|a| PendingAsk {
                        id: a.id,
                        from: a.from,
                        workspace: a.workspace,
                        question: a.question,
                        choices: a.choices,
                        answer: a.answer,
                    })
                    .collect();
                let pane_slugs: std::collections::HashSet<String> =
                    panes.iter().map(|p| p.slug.clone()).collect();
                self.statuses = statuses
                    .into_iter()
                    .filter(|s| pane_slugs.contains(&s.slug))
                    .map(|s| {
                        (
                            s.slug,
                            PaneStatus {
                                state: s.state,
                                note: s.note,
                            },
                        )
                    })
                    .collect();
                let known: std::collections::HashSet<_> =
                    panes.iter().map(|p| p.slug.clone()).collect();
                for info in &panes {
                    self.ensure_remote_pane_cx(info, cx);
                    if let Some(owner) = &info.owner {
                        self.owners.insert(
                            info.slug.clone(),
                            OwnerChrome {
                                owner: owner.clone(),
                                drive_mode: info
                                    .drive_mode
                                    .clone()
                                    .unwrap_or_else(|| "pair".into()),
                                exited: info.exited,
                                exit_code: info.exit_code,
                            },
                        );
                    }
                }
                self.owners.retain(|k, _| known.contains(k));
                self.panes.retain(|p| known.contains(&p.slug));
                // Daemon pane-list order is the persistence key for sidebar +
                // tile layout. Reconcile local order so a State push (after
                // reorder, attach, or upgrade) doesn't leave the GUI stuck on
                // a pre-reorder sequence while the daemon has the real one.
                let order: std::collections::HashMap<&str, usize> = panes
                    .iter()
                    .enumerate()
                    .map(|(i, p)| (p.slug.as_str(), i))
                    .collect();
                self.panes
                    .sort_by_key(|p| order.get(p.slug.as_str()).copied().unwrap_or(usize::MAX));
                // Daemon-owned activity clocks are the durable copy: seed the
                // local mirrors with a MAX merge (both are unix ms, so they
                // compare directly). Local stamping stays for instant feedback.
                // pr_links arrive for every known workspace: rebuild the
                // mirror wholesale so cleared/renamed circles drop out.
                let mut links = std::collections::HashMap::new();
                // Labels arrive for every known circle, so rebuild wholesale:
                // a circle renamed back to its slug must lose its entry.
                let mut names = std::collections::HashMap::new();
                for m in workspace_meta {
                    if let Some(n) = m.name.clone() {
                        names.insert(m.workspace.clone(), n);
                    }
                    if !m.pr_links.is_empty() {
                        links.insert(m.workspace.clone(), m.pr_links.clone());
                    }
                    if m.last_output_ms > 0 {
                        let cur = self
                            .workspace_activity
                            .get(&m.workspace)
                            .copied()
                            .unwrap_or(0);
                        if m.last_output_ms > cur {
                            self.workspace_activity
                                .insert(m.workspace.clone(), m.last_output_ms);
                        }
                    }
                    if m.last_touch_ms > 0 {
                        let cur = self.workspace_touch.get(&m.workspace).copied().unwrap_or(0);
                        if m.last_touch_ms > cur {
                            self.workspace_touch.insert(m.workspace, m.last_touch_ms);
                        }
                    }
                }
                self.pr_links = links;
                self.workspace_names = names;
                // active_slug from daemon; repair if missing / not in selected
                // workspace. Keyboard recovery is render-side (ensure_keyboard_focus)
                // so we don't steal focus from whisper / rename / palette here.
                self.ensure_active_pane_in_workspace();
                self.sync_workspace_working_touches();
                cx.notify();
            }
            GuiEvent::Grid(snap) => {
                self.apply_grid_snap(snap, cx);
            }
            GuiEvent::GridBin { pane, data_b64 } => {
                let apply_t0 = std::time::Instant::now();
                // Damage frames need the previous snapshot as base.
                let base = self
                    .panes
                    .iter()
                    .find(|p| p.slug == pane)
                    .and_then(|p| p.remote_terminal())
                    .map(|rt| rt.read(cx).snapshot.clone());
                let base_ref = base.as_ref().map(|a| a.as_ref());
                match decode_grid_b64(&data_b64, base_ref) {
                    Ok(snap) => {
                        self.apply_grid_snap(snap, cx);
                        crate::latency_probe::record(
                            "gui grid apply",
                            apply_t0.elapsed().as_micros() as u64,
                        );
                    }
                    Err(e) => {
                        // Size mismatch / missing base after upgrade or resize:
                        // drop local base so the next FULL frame applies cleanly.
                        // Rate-limit log + re-attach — reconnect used to spam.
                        static LAST_RESYNC: std::sync::Mutex<Option<std::time::Instant>> =
                            std::sync::Mutex::new(None);
                        let now = std::time::Instant::now();
                        let mut do_resync = true;
                        if let Ok(mut g) = LAST_RESYNC.lock() {
                            if let Some(t) = *g {
                                if now.duration_since(t).as_millis() < 2000 {
                                    do_resync = false;
                                }
                            }
                            if do_resync {
                                *g = Some(now);
                            }
                        }
                        // Only touch the pane when we can guarantee a repair
                        // frame. Blanking the base without a re-Attach would
                        // leave an idle pane stuck empty until its next push;
                        // when rate-limited we simply drop the bad frame and
                        // keep the last-good grid until the in-flight FULL lands.
                        if do_resync {
                            eprintln!(
                                "[seance gui] grid_bin resync for {pane}: {e} (cleared base; pane refresh)"
                            );
                            if let Some(rt) = self
                                .panes
                                .iter()
                                .find(|p| p.slug == pane)
                                .and_then(|p| p.remote_terminal())
                                .cloned()
                            {
                                // Must zero rev — empty snap alone leaves a high
                                // rev and every full frame at that rev is dropped.
                                rt.update(cx, |t, cx| t.clear_for_resync(cx));
                            }
                            // Targeted repair: one FULL frame for this pane.
                            // (Used to re-Attach the whole window — heavier and
                            // racy with every other pane's in-flight damage.)
                            let _ = self.client.refresh_grid(&pane);
                        }
                    }
                }
            }
            GuiEvent::PaneSpawned { pane } => {
                let slug = pane.slug.clone();
                let ws = pane.workspace.clone();
                if pane.busy {
                    self.busy_panes.insert(slug.clone());
                }
                self.ensure_remote_pane_cx(&pane, cx);
                // Summon → select workspace, make active, focus the new pane.
                self.selected_workspace = Some(ws.clone());
                self.active_slug = Some(slug.clone());
                self.pending_focus = Some(slug.clone());
                let _ = self.client.set_focus(Some(slug.clone()), Some(ws));
                self.focus_pane_if_possible(&slug, cx);
                // Summon lands keys on the TERMINAL immediately (pending_focus
                // above) — no auto-rename steal; naming is a double-click away.
                // (Was: open pane rename, which ate the first keystrokes.)
                self.rename_next_spawn = false;
                cx.notify();
            }
            GuiEvent::PaneBusy { pane, busy } => {
                let changed = if busy {
                    self.busy_panes.insert(pane)
                } else {
                    self.busy_panes.remove(&pane)
                };
                if changed {
                    // A circle leaving the working band takes a touch here, not
                    // on selection — that's the whole point of the broadcast.
                    self.sync_workspace_working_touches();
                    cx.notify();
                }
            }
            GuiEvent::PaneKilled { slug } => {
                self.panes.retain(|p| p.slug != slug);
                self.busy_panes.remove(&slug);
                self.workspace_focus.retain(|_, s| s != &slug);
                // Never leave a workspace with panes but no active pane.
                let prev = self.active_slug.clone();
                self.ensure_active_pane_in_workspace();
                if self.active_slug != prev {
                    if let Some(next) = self.active_slug.clone() {
                        self.pending_focus = Some(next);
                    }
                }
                cx.notify();
            }
            GuiEvent::PaneExited { slug, exit_code } => {
                // Tombstone: keep the pane; mark ownership chrome. Explicit
                // kill still removes via PaneKilled.
                let entry = self.owners.entry(slug.clone()).or_insert(OwnerChrome {
                    owner: "none".into(),
                    drive_mode: "pair".into(),
                    exited: true,
                    exit_code,
                });
                entry.exited = true;
                entry.exit_code = exit_code;
                entry.owner = "none".into();
                cx.notify();
            }
            GuiEvent::Ask { ask } => {
                crate::desktop_notify::ask(&ask.from, &ask.question);
                self.asks.push(PendingAsk {
                    id: ask.id,
                    from: ask.from,
                    workspace: ask.workspace,
                    question: ask.question,
                    choices: ask.choices,
                    answer: ask.answer,
                });
                cx.notify();
            }
            GuiEvent::AskResolved { id } => {
                self.asks.retain(|a| a.id != id);
                cx.notify();
            }
            GuiEvent::Status { slug, state, note } => {
                if state == "needs-human" || state == "blocked" {
                    crate::desktop_notify::needs_human(&slug, note.as_deref());
                    // If this pane is phoned to telegram, post a one-liner.
                    telegram_status_bridge(
                        Arc::clone(&self.client),
                        &slug,
                        &state,
                        note.as_deref(),
                    );
                }
                self.note_workspace_status_event(&slug, &state);
                self.statuses.insert(slug, PaneStatus { state, note });
                self.sync_workspace_working_touches();
                if matches!(self.drawer, Drawer::Pad { .. }) {
                    self.pad_refresh_tick = self.pad_refresh_tick.wrapping_add(1);
                }
                cx.notify();
            }
            GuiEvent::Touch { slug, verb, actor } => {
                self.touch(&slug, &verb, &actor, cx);
            }
            GuiEvent::InputOrigin { pane, origin } => {
                // Real input (keystroke / inject / propose) bumps workspace
                // recency for sidebar auto-sort. Focus/select alone never
                // emits InputOrigin.
                if let Some(ws) = self
                    .panes
                    .iter()
                    .find(|p| p.slug == pane)
                    .map(|p| p.workspace.clone())
                {
                    self.touch_workspace(&ws);
                    cx.notify(); // re-sort sidebar by last human touch
                }
                if let Some(rt) = self
                    .panes
                    .iter()
                    .find(|p| p.slug == pane)
                    .and_then(|p| p.remote_terminal())
                    .cloned()
                {
                    rt.update(cx, |t, cx| t.set_input_origin(origin, cx));
                }
            }
            GuiEvent::Agency {
                pane,
                owner,
                drive_mode,
                human_idle: _,
                exited,
                exit_code,
            } => {
                self.owners.insert(
                    pane.clone(),
                    OwnerChrome {
                        owner,
                        drive_mode,
                        exited,
                        exit_code,
                    },
                );
                self.sync_workspace_working_touches();
                cx.notify();
            }
            GuiEvent::Ghost { pane, ghost } => {
                if let Some(rt) = self
                    .panes
                    .iter()
                    .find(|p| p.slug == pane)
                    .and_then(|p| p.remote_terminal())
                    .cloned()
                {
                    rt.update(cx, |t, cx| t.set_ghost(ghost, cx));
                }
            }
            GuiEvent::Activity {
                workspace,
                last_output_ms,
            } => {
                // Daemon says this circle produced real output. Max-merge:
                // a local stamp from a frame we just painted may be newer.
                let cur = self
                    .workspace_activity
                    .get(&workspace)
                    .copied()
                    .unwrap_or(0);
                if last_output_ms > cur {
                    self.workspace_activity.insert(workspace, last_output_ms);
                    cx.notify();
                }
            }
            GuiEvent::Kicked { by } => {
                // Closed from another GUI's ✦ popover — leave cleanly, no
                // reconnect (the supervisor would just re-register us).
                eprintln!("[seance gui] closed remotely by {by}");
                self.client.disconnect();
                cx.quit();
            }
            GuiEvent::Error { message } => {
                eprintln!("[seance gui] daemon error: {message}");
            }
            GuiEvent::RailPrefs { json } => {
                // Another window (or this one) changed the shared arrangement.
                // Adopt wholesale and DO NOT save: the daemon broadcasts to
                // every window including the sender, so saving here would put
                // one pin into an endless round trip.
                if let Some(pref) = crate::subscriptions_pref::parse(&json) {
                    if pref != self.subs_pref {
                        self.subs_pref = pref;
                        crate::subscriptions_pref::save(&self.subs_pref);
                        self.client
                            .set_subscription_seed(self.subs_pref.active.iter().cloned().collect());
                        // Bring this connection to the new arrangement on the
                        // next State, the same way boot adoption does.
                        self.rail_from_daemon = true;
                        cx.notify();
                    }
                }
            }
            GuiEvent::HostWidgets { widgets } => {
                // Daemon-side poller push: replace chip state wholesale.
                if let Ok(snaps) =
                    serde_json::from_value::<Vec<crate::host::HostWidgetSnap>>(widgets)
                {
                    self.host.ever_ok = self.host.ever_ok || !snaps.is_empty();
                    self.host.widgets = snaps;
                    cx.notify();
                }
            }
            GuiEvent::Ack { ok, error, .. } => {
                // Acks are informational, but a failed op must not be a
                // silent dead click — land it in the gui log at least.
                if !ok {
                    eprintln!(
                        "[seance gui] daemon rejected request: {}",
                        error.unwrap_or_else(|| "unknown error".into())
                    );
                }
            }
            // FsResult is routed to fs_call waiters inside gui_client and
            // never reaches the app stream; ignore defensively.
            GuiEvent::FsResult { .. } | GuiEvent::Pong => {}
        }
    }

    /// Apply a decoded grid to the matching remote pane. Shared by JSON
    /// `grid` and binary `grid_bin` events. Outside overview, only panes on
    /// the selected workspace fully paint — hidden panes only absorb frames
    /// when busy-ness flips (spinner ↔ idle) so working badges + finish-touch
    /// stay correct without the old 90%+ CPU tax from spinning TUIs.
    fn apply_grid_snap(&mut self, snap: GridSnapshot, cx: &mut Context<Self>) {
        let slug = snap.pane.clone();
        // Time-since-activity stamps ONLY on real content change. Attach /
        // pull / relaunch / workspace-switch all re-push FULL frames with
        // identical (or first-seen) content — those must not reset the clock.
        // Nor may the SIGWINCH redraw that follows the PTY resize we send the
        // first time a circle's tiles are laid out (see RESIZE_SETTLE_MS):
        // selecting a circle must never bump its last-active time.
        let now = crate::app::util::now_ms();
        let shape = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| p.remote_terminal())
            .map(|rt| {
                let prev = &rt.read(cx).snapshot;
                (
                    prev.cells.is_empty(),
                    prev.cols == snap.cols && prev.rows == snap.rows,
                    prev.cells != snap.cells,
                )
            });
        let mut content_changed = false;
        if let Some((prev_empty, dims_match, cells_changed)) = shape {
            if !dims_match {
                // Reflow moment — arm the settle window for the redraw burst.
                self.resize_settle
                    .insert(slug.clone(), now + crate::app::util::RESIZE_SETTLE_MS);
            }
            content_changed = crate::app::util::grid_frame_is_output(
                prev_empty,
                dims_match,
                cells_changed,
                now,
                self.resize_settle.get(&slug).copied(),
            );
            if content_changed {
                self.resize_settle.remove(&slug);
            }
        }
        if content_changed {
            if let Some(ws) = self
                .panes
                .iter()
                .find(|p| p.slug == slug)
                .map(|p| p.workspace.clone())
            {
                self.workspace_activity.insert(ws, now);
            }
        }
        if !self.overview {
            let ws = self.selected_workspace.as_deref();
            let visible = self.panes.iter().any(|p| {
                p.slug == slug && p.popped.is_none() && ws.is_none_or(|w| p.workspace == w)
            });
            if !visible {
                if let Some(rt) = self
                    .panes
                    .iter()
                    .find(|p| p.slug == slug)
                    .and_then(|p| p.remote_terminal())
                    .cloned()
                {
                    let old_busy = rt
                        .read(cx)
                        .title()
                        .as_deref()
                        .map(title_looks_busy)
                        .unwrap_or(false);
                    let new_busy = snap.title.as_deref().map(title_looks_busy).unwrap_or(false);
                    if old_busy == new_busy {
                        return;
                    }
                    rt.update(cx, |t, cx| {
                        t.apply_snapshot(snap, cx);
                    });
                    self.sync_workspace_working_touches();
                    cx.notify();
                }
                return;
            }
        }
        if let Some(rt) = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| p.remote_terminal())
            .cloned()
        {
            rt.update(cx, |t, cx| {
                t.apply_snapshot(snap, cx);
            });
            self.sync_workspace_working_touches();
            self.grid_batch_visible = true;
        }
    }

    /// Focus a pane's terminal view if we can reach a window.
    fn focus_pane_if_possible(&mut self, slug: &str, cx: &mut Context<Self>) {
        let handle = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| match &p.body {
                PaneBody::Remote { view, .. } => Some(view.read(cx).focus_handle()),
                PaneBody::File { .. } => None,
            });
        let Some(handle) = handle else {
            return;
        };
        // Context may not own a Window; try every open window.
        for wh in cx.windows() {
            let focused = wh
                .update(cx, |_root, window, cx| {
                    window.focus(&handle, cx);
                    true
                })
                .unwrap_or(false);
            if focused {
                self.pending_focus = None;
                return;
            }
        }
    }

    /// Put keyboard focus somewhere useful after rename / overlay dismiss /
    /// empty-circle create. Prefer the active pane in the selected workspace;
    /// if the circle is empty, land on the app root so capture chords
    /// (`ctrl+shift+n`, …) still fire (focus=None swallows keys entirely).
    fn restore_keyboard_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_active_pane_in_workspace();
        let ws = self.selected_workspace.clone();
        if let Some(slug) = self.active_slug.clone() {
            if let Some(pane) = self
                .panes
                .iter()
                .find(|p| p.slug == slug && ws.as_ref().is_none_or(|w| p.workspace == *w))
            {
                pane.focus_content(window, cx);
                return;
            }
        }
        // Empty selected circle (or active slug dead/wrong) — try any pane
        // in the circle before falling back to the root handle.
        if let Some(ws) = ws.as_deref() {
            if let Some(slug) = self.preferred_pane_in_workspace(ws) {
                self.set_active(&slug, window, cx);
                return;
            }
        }
        let fh = self.focus_handle.clone();
        window.focus(&fh, cx);
    }

    /// During render we have a Window — apply pending_focus (summon / palette
    /// close), or recover when nothing in the window is focused (cold launch).
    fn ensure_keyboard_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(slug) = self.pending_focus.clone() {
            if let Some(pane) = self.panes.iter().find(|p| p.slug == slug) {
                // Don't steal from an open rename of this pane — Enter will
                // restore focus when the human finishes naming.
                if matches!(&self.renaming, Some((RenameTarget::Pane(s), _)) if s == &slug) {
                    return;
                }
                pane.focus_content(window, cx);
                self.pending_focus = None;
                return;
            }
            // View not ready yet — keep pending for a later frame.
            return;
        }
        // Keep active_slug coherent with the selected workspace (invariant:
        // never no active pane when the workspace has panes).
        self.ensure_active_pane_in_workspace();
        // Cold launch / dead handle / post-rename: GPUI focus is None →
        // key events never reach capture or the terminal. Park on the
        // active pane, or the root handle for empty circles.
        if window.focused(cx).is_none() {
            self.restore_keyboard_focus(window, cx);
        }
    }

    /// Global key chords + palette capture. Runs in the *capture* phase so
    /// app hotkeys win even when a terminal child is focused (bubble-only
    /// never reached the root when focus was None or a non-descendant).
    fn on_global_key_capture(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &event.keystroke;
        let key = ks.key.as_str();

        // ---- palette is open: own all keys until dismissed ----
        if !matches!(self.palette, PaletteMode::Closed) {
            if key == "escape" {
                self.close_palette(window, cx);
                cx.stop_propagation();
                return;
            }
            if key == "enter" {
                self.activate_palette_selection(window, cx);
                cx.stop_propagation();
                return;
            }
            if key == "up" || key == "arrowup" {
                self.palette_move(-1);
                cx.notify();
                cx.stop_propagation();
                return;
            }
            if key == "down" || key == "arrowdown" {
                self.palette_move(1);
                cx.notify();
                cx.stop_propagation();
                return;
            }
            if key == "backspace" {
                match &mut self.palette {
                    PaletteMode::Prompts { query, selected }
                    | PaletteMode::Jump { query, selected } => {
                        query.pop();
                        *selected = 0;
                    }
                    PaletteMode::Closed => {}
                }
                cx.notify();
                cx.stop_propagation();
                return;
            }
            // Prefer key_char (layout-aware) for filter text.
            let add = if let Some(ref ch) = ks.key_char {
                if !ks.modifiers.control && !ks.modifiers.alt && !ch.is_empty() {
                    Some(ch.clone())
                } else {
                    None
                }
            } else if key == "space" && !ks.modifiers.control && !ks.modifiers.alt {
                Some(" ".to_string())
            } else if key.len() == 1 && !ks.modifiers.control && !ks.modifiers.alt {
                Some(key.to_string())
            } else {
                None
            };
            if let Some(add) = add {
                match &mut self.palette {
                    PaletteMode::Prompts { query, selected }
                    | PaletteMode::Jump { query, selected } => {
                        query.push_str(&add);
                        *selected = 0;
                    }
                    PaletteMode::Closed => {}
                }
                cx.notify();
                cx.stop_propagation();
                return;
            }
            // Swallow other keys while palette is open so PTY doesn't see them.
            cx.stop_propagation();
            return;
        }

        // ---- escape for chrome overlays only; else let terminal get it ----
        if key == "escape" {
            if self.gui_menu_open {
                self.gui_menu_open = false;
                cx.notify();
                cx.stop_propagation();
                return;
            }
            if self.close_host_menu(cx) {
                cx.stop_propagation();
                return;
            }
            if self.quicklaunch_editor.is_some() {
                self.cancel_quicklaunch_editor(cx);
                if let Some(slug) = self.active_slug.clone() {
                    if let Some(pane) = self.panes.iter().find(|p| p.slug == slug) {
                        pane.focus_content(window, cx);
                    }
                }
                cx.stop_propagation();
                return;
            }
            if self.overview {
                self.set_overview(false, cx);
                cx.stop_propagation();
                return;
            }
            if self.pr_board {
                self.set_pr_board(false, cx);
                cx.stop_propagation();
                return;
            }
            if self.renaming.is_some() {
                self.renaming = None;
                self.pending_rename = None;
                self.restore_keyboard_focus(window, cx);
                cx.notify();
                cx.stop_propagation();
                return;
            }
            if self.whisper.is_some() {
                self.cancel_whisper(cx);
                cx.stop_propagation();
                return;
            }
            if self.zoomed_slug.is_some() {
                self.zoomed_slug = None;
                cx.notify();
                cx.stop_propagation();
                return;
            }
            // Not ours — fall through to focused terminal.
            return;
        }

        // Ctrl+PageUp/Down — cycle workspaces; Ctrl+Shift+Page — cycle panes.
        // Accept pageup/pagedown (GPUI) and common aliases.
        let is_page_up = matches!(key, "pageup" | "page_up" | "prior");
        let is_page_down = matches!(key, "pagedown" | "page_down" | "next");
        if ks.modifiers.control && !ks.modifiers.alt && (is_page_up || is_page_down) {
            let delta = if is_page_up { -1 } else { 1 };
            if ks.modifiers.shift {
                self.cycle_pane(delta, window, cx);
            } else {
                self.cycle_workspace(delta, window, cx);
            }
            cx.stop_propagation();
            return;
        }

        if ks.modifiers.control && ks.modifiers.shift && !ks.modifiers.alt {
            match key {
                "n" => {
                    self.new_default_session(cx);
                    cx.stop_propagation();
                }
                "w" => {
                    // Kill the active pane only. Last pane in a circle also
                    // banishes the workspace (two presses for a 2-pane circle).
                    // Empty selected circle (no panes) → banish the shell.
                    if let Some(slug) = self.active_slug.clone() {
                        let ws = self
                            .panes
                            .iter()
                            .find(|p| p.slug == slug)
                            .map(|p| p.workspace.clone());
                        let last_in_ws = ws.as_ref().is_some_and(|w| {
                            self.panes.iter().filter(|p| p.workspace == *w).count() == 1
                        });
                        if last_in_ws {
                            if let Some(w) = ws {
                                self.kill_workspace(&w, window, cx);
                            }
                        } else {
                            self.kill_active_pane(cx);
                        }
                    } else if let Some(ws) = self.selected_workspace.clone() {
                        if !self.panes.iter().any(|p| p.workspace == ws) {
                            self.kill_workspace(&ws, window, cx);
                        }
                    }
                    cx.stop_propagation();
                }
                "s" => {
                    self.toggle_notes_flip(window, cx);
                    cx.stop_propagation();
                }
                "p" => {
                    if let Some(slug) = self.active_slug.clone() {
                        self.toggle_popout(&slug, cx);
                        cx.stop_propagation();
                    }
                }
                " " | "space" => {
                    self.set_overview(!self.overview, cx);
                    cx.stop_propagation();
                }
                "k" => {
                    self.palette = PaletteMode::Prompts {
                        query: String::new(),
                        selected: 0,
                    };
                    // Keep focus on root handle so typing is unambiguous even
                    // if a child steals bubble; capture still owns keys.
                    let fh = self.focus_handle.clone();
                    window.focus(&fh, cx);
                    cx.notify();
                    cx.stop_propagation();
                }
                "j" => {
                    self.palette = PaletteMode::Jump {
                        query: String::new(),
                        selected: 0,
                    };
                    let fh = self.focus_handle.clone();
                    window.focus(&fh, cx);
                    cx.notify();
                    cx.stop_propagation();
                }
                "z" | "m" => {
                    if let Some(slug) = self.active_slug.clone() {
                        self.toggle_zoom(&slug, cx);
                        cx.stop_propagation();
                    }
                }
                "r" => {
                    // Inline-rename the selected workspace; Enter commits and
                    // returns focus to the pane that was active.
                    if let Some(ws) = self.selected_workspace.clone() {
                        let label = self.workspace_label(&ws);
                        self.start_rename(RenameTarget::Workspace(ws.clone()), &label, window, cx);
                        cx.stop_propagation();
                    }
                }
                "f" => {
                    if let Some(slug) = self.active_slug.clone() {
                        self.show_last_failed(&slug, cx);
                        cx.stop_propagation();
                    }
                }
                _ => {}
            }
        }
    }

    fn ensure_remote_pane_cx(&mut self, info: &PaneInfo, cx: &mut Context<Self>) {
        if self.panes.iter().any(|p| p.slug == info.slug) {
            if let Some(p) = self.panes.iter_mut().find(|p| p.slug == info.slug) {
                p.name = info.name.clone();
                p.workspace = info.workspace.clone();
                p.tiled = info.tiled;
                p.command = info.command.clone();
                p.cwd = info.cwd.clone();
                p.scratchpad = info.scratchpad.clone();
                p.asleep = info.asleep;
                p.restorable = info.restorable;
            }
            return;
        }
        if info.kind == "file" {
            let path =
                std::path::PathBuf::from(info.file.clone().unwrap_or_else(|| info.command.clone()));
            let view =
                cx.new(|cx| crate::fileview::FileView::new(path.clone(), self.client.clone(), cx));
            self.panes.push(Pane {
                name: info.name.clone(),
                slug: info.slug.clone(),
                workspace: info.workspace.clone(),
                cwd: info.cwd.clone(),
                command: info.command.clone(),
                tiled: info.tiled,
                scratchpad: info.scratchpad.clone(),
                asleep: info.asleep,
                restorable: info.restorable,
                body: PaneBody::File { view },
                popped: None,
            });
            return;
        }
        let terminal =
            cx.new(|_cx| RemoteTerminal::new(info.slug.clone(), Arc::clone(&self.client)));
        let view = cx.new(|cx| RemoteTerminalView::new(terminal.clone(), cx));
        self.panes.push(Pane {
            name: info.name.clone(),
            slug: info.slug.clone(),
            workspace: info.workspace.clone(),
            cwd: info.cwd.clone(),
            command: info.command.clone(),
            tiled: info.tiled,
            scratchpad: info.scratchpad.clone(),
            asleep: info.asleep,
            restorable: info.restorable,
            body: PaneBody::Remote { terminal, view },
            popped: None,
        });
        // Fresh mount = empty snapshot. Ask the daemon for a FULL frame so
        // panes arriving via transfer/pull/collect paint immediately instead
        // of waiting for the engine's delayed belt-and-suspenders flush.
        let _ = self.client.refresh_grid(&info.slug);
        // If we were waiting to focus this slug, try now that the view exists.
        if self.pending_focus.as_deref() == Some(info.slug.as_str()) {
            self.focus_pane_if_possible(&info.slug, cx);
        }
    }

    // ---- pane management ----

    fn spawn_internal(&mut self, req: SpawnRequest, cx: &mut Context<Self>) -> Option<String> {
        // All spawns go through the daemon — PTYs never live in the GUI process.
        let _ = self.client.spawn_pane(
            &req.name,
            req.cwd,
            req.command,
            req.workspace.or_else(|| self.selected_workspace.clone()),
            req.file,
        );
        self.session_counter += 1;
        cx.notify();
        // Real slug arrives via GuiEvent::PaneSpawned / State.
        None
    }

    fn new_default_session(&mut self, cx: &mut Context<Self>) {
        let n = self.session_counter + 1;
        // Rename opens when PaneSpawned arrives (slug is assigned by daemon).
        self.rename_next_spawn = true;
        self.spawn_internal(
            SpawnRequest {
                name: format!("term-{n}"),
                cwd: None,
                command: None,
                workspace: self.selected_workspace.clone(),
                file: None,
            },
            cx,
        );
    }

    /// Open an empty OS window (same process) for multi-window transfers.
    /// Paired with [`Self::new_empty_window`] — unwired until phase 2.
    #[allow(dead_code)]
    fn open_empty_os_window(&mut self, cx: &mut Context<Self>) {
        let bounds = gpui::Bounds::centered(None, gpui::size(px(1280.), px(800.)), cx);
        let _ = cx.open_window(
            gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("seance".into()),
                    ..Default::default()
                }),
                app_id: Some("seance".into()),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| SeanceApp::new_empty_window(window, cx));
                // On close, tell daemon (Bye) via disconnect.
                let client = view.read(cx).client.clone();
                window.on_window_should_close(cx, move |_, _| {
                    client.disconnect();
                    true
                });
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            },
        );
    }

    // ---- inline rename ----

    /// The inline-rename editor for `slug`, if that pane is the current
    /// rename target — the pane title strip swaps it in for the title text.
    pub(super) fn pane_rename_input(&self, slug: &str) -> Option<&Entity<InputState>> {
        match &self.renaming {
            Some((RenameTarget::Pane(s), input)) if s == slug => Some(input),
            _ => None,
        }
    }

    fn start_rename(
        &mut self,
        target: RenameTarget,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current.to_string()));
        cx.subscribe_in(
            &input,
            window,
            |this: &mut SeanceApp, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let value = input.read(cx).value().to_string();
                    this.commit_rename(value.trim(), cx);
                    // Pane if the circle has one; else app root so chords work
                    // on a brand-new empty workspace (focus=None eats keys).
                    this.restore_keyboard_focus(window, cx);
                    // Next frame: re-assert in case blur of the disposed input
                    // races and clears focus again.
                    cx.defer_in(window, |this, window, cx| {
                        if this.renaming.is_none()
                            && this.whisper.is_none()
                            && matches!(this.palette, PaletteMode::Closed)
                        {
                            this.restore_keyboard_focus(window, cx);
                        }
                    });
                }
                InputEvent::Blur => {
                    // Only cancel if still renaming — Enter already cleared it
                    // and restored pane focus; a follow-up blur must not steal.
                    if this.renaming.is_some() {
                        this.renaming = None;
                        this.restore_keyboard_focus(window, cx);
                        cx.notify();
                    }
                }
                _ => {}
            },
        )
        .detach();
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.renaming = Some((target, input.clone()));
        self.pending_rename = None;
        // Select-all AFTER the current event (esp. double-click mouse-up) so the
        // click that opened rename doesn't land on the new input and collapse
        // the caret. Typing then replaces the whole name.
        cx.defer_in(window, move |_, window, cx| {
            input.update(cx, |state, cx| {
                let len = state.text().len();
                state.set_selected_range(0..len, cx);
            });
            let focus = input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.notify();
    }

    /// If a create/summon requested rename, schedule it once we have a Window
    /// (PaneSpawned arrives on a no-window path).
    fn flush_pending_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.pending_rename.take() else {
            return;
        };
        let current = match &target {
            RenameTarget::Pane(slug) => self
                .panes
                .iter()
                .find(|p| p.slug == *slug)
                .map(|p| p.name.clone()),
            RenameTarget::Workspace(w) => Some(w.clone()),
        };
        let Some(current) = current else {
            // Target not ready yet — retry next frame.
            self.pending_rename = Some(target);
            return;
        };
        // Defer so we don't start_rename (notify/subscribe) mid-render.
        cx.defer_in(window, move |this, window, cx| {
            // Still free? User may have started another rename.
            if this.renaming.is_some() {
                return;
            }
            this.start_rename(target, &current, window, cx);
        });
    }

    fn commit_rename(&mut self, new_name: &str, cx: &mut Context<Self>) {
        let Some((target, _)) = self.renaming.take() else {
            return;
        };
        if new_name.is_empty() {
            cx.notify();
            return;
        }
        match target {
            RenameTarget::Pane(slug) => {
                if let Some(pane) = self.panes.iter_mut().find(|p| p.slug == slug) {
                    pane.name = new_name.to_string();
                }
                // Daemon is source of truth — don't only dual-write state.json.
                let _ = self.client.rename_pane(&slug, new_name);
            }
            RenameTarget::Workspace(slug) => {
                // Nothing local to migrate: the slug is the identity and it
                // does not move. Every map here — touch, unread, focus,
                // selection, pin/park prefs — is keyed by it and stays
                // correct. Optimistically show the new label; the daemon's
                // next State push confirms it.
                self.workspace_names
                    .insert(slug.clone(), new_name.to_string());
                let _ = self.client.rename_workspace(&slug, new_name);
            }
        }
        cx.notify();
    }

    /// Cycle focus among panes in the selected workspace (sidebar/list order).
    /// `delta` is +1 (next / PageDown) or -1 (prev / PageUp). Wraps.
    /// Prefer tiled non-popped panes; if none, any pane in the workspace.
    fn cycle_pane(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        let ws = self
            .selected_workspace
            .clone()
            .or_else(|| self.active_session().map(|p| p.workspace.clone()));
        let Some(ws) = ws else {
            return;
        };
        let tiled: Vec<String> = self
            .panes
            .iter()
            .filter(|p| p.workspace == ws && p.tiled && p.popped.is_none())
            .map(|p| p.slug.clone())
            .collect();
        let list: Vec<String> = if tiled.len() >= 2 {
            tiled
        } else {
            self.panes
                .iter()
                .filter(|p| p.workspace == ws && p.popped.is_none())
                .map(|p| p.slug.clone())
                .collect()
        };
        if list.len() < 2 {
            return;
        }
        let cur = self
            .active_slug
            .as_deref()
            .and_then(|s| list.iter().position(|x| x == s))
            .unwrap_or(0);
        let n = list.len() as i32;
        let next = (cur as i32 + delta).rem_euclid(n) as usize;
        let slug = list[next].clone();
        self.set_active(&slug, window, cx);
    }

    fn active_session(&self) -> Option<&Pane> {
        self.active_slug
            .as_ref()
            .and_then(|slug| self.panes.iter().find(|s| &s.slug == slug))
    }

    /// Preferred pane for a workspace: last focused (if still present and not
    /// popped), else first tiled non-popped, else any non-popped, else any.
    fn preferred_pane_in_workspace(&self, workspace: &str) -> Option<String> {
        self.workspace_focus
            .get(workspace)
            .cloned()
            .filter(|s| {
                self.panes
                    .iter()
                    .any(|p| p.slug == *s && p.workspace == workspace && p.popped.is_none())
            })
            .or_else(|| {
                self.panes
                    .iter()
                    .find(|p| p.workspace == workspace && p.tiled && p.popped.is_none())
                    .or_else(|| {
                        self.panes
                            .iter()
                            .find(|p| p.workspace == workspace && p.popped.is_none())
                    })
                    .or_else(|| self.panes.iter().find(|p| p.workspace == workspace))
                    .map(|p| p.slug.clone())
            })
    }

    /// Invariant: a selected workspace that has panes always has an active
    /// pane. Repairs `active_slug` when it is None, dead, or in another
    /// workspace. Syncs daemon focus only when the active pane changes.
    fn ensure_active_pane_in_workspace(&mut self) {
        let Some(ws) = self.selected_workspace.clone() else {
            // No selected workspace — keep active only if the slug still exists.
            let ok = self
                .active_slug
                .as_ref()
                .is_some_and(|s| self.panes.iter().any(|p| &p.slug == s));
            if ok {
                return;
            }
            let next = self.panes.first().map(|p| p.slug.clone());
            if self.active_slug != next {
                self.active_slug = next.clone();
                let _ = self.client.set_focus(next, None);
            }
            return;
        };
        let ok = self
            .active_slug
            .as_ref()
            .is_some_and(|s| self.panes.iter().any(|p| &p.slug == s && p.workspace == ws));
        if ok {
            if let Some(slug) = self.active_slug.clone() {
                self.workspace_focus.insert(ws, slug);
            }
            return;
        }
        let next = self.preferred_pane_in_workspace(&ws);
        if self.active_slug != next {
            if let Some(ref slug) = next {
                self.workspace_focus.insert(ws.clone(), slug.clone());
            }
            self.active_slug = next.clone();
            let _ = self.client.set_focus(next, Some(ws));
        }
    }

    fn set_active(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_slug.as_deref() != Some(slug) {
            let ws = self
                .panes
                .iter()
                .find(|p| p.slug == slug)
                .map(|p| p.workspace.clone());
            self.client.log_event(
                "human",
                ws.as_deref(),
                Some(slug),
                "focus",
                format!("focused '{slug}'"),
            );
        }
        self.active_slug = Some(slug.to_string());
        if let Some(pane) = self.panes.iter().find(|s| s.slug == slug) {
            let ws = pane.workspace.clone();
            self.selected_workspace = Some(ws.clone());
            self.workspace_focus.insert(ws.clone(), slug.to_string());
            let _ = self.client.set_focus(Some(slug.to_string()), Some(ws));
            // Flipped to notes: keep focus IN the editor — focusing the
            // terminal here stole the caret on every click inside notes
            // (the input's own mousedown places the cursor; we must not
            // yank focus back to the face).
            if self.flipped.as_ref().is_some_and(|(s, _)| s == slug) {
                if let Some((_, drawer)) = self.flipped.as_ref() {
                    let fh = drawer.read(cx).focus_handle(cx);
                    window.focus(&fh, cx);
                }
            } else {
                // Eager focus for the common case + a render-time backstop:
                // when this navigation just switched workspaces, the target
                // tile may not exist yet — pending_focus re-applies once it
                // renders (ensure_keyboard_focus), killing the recurring
                // "jumped but can't type until I click" class.
                pane.focus_content(window, cx);
                self.pending_focus = Some(slug.to_string());
            }
        }
        cx.notify();
    }

    fn toggle_tiled(&mut self, slug: &str, cx: &mut Context<Self>) {
        let tiled = self
            .panes
            .iter()
            .find(|s| s.slug == slug)
            .map(|p| !p.tiled)
            .unwrap_or(true);
        if let Some(pane) = self.panes.iter_mut().find(|s| s.slug == slug) {
            pane.tiled = tiled;
        }
        let _ = self.client.set_tiled(slug, tiled);
        cx.notify();
    }

    fn kill_session(&mut self, slug: &str, cx: &mut Context<Self>) {
        let _ = self.client.kill(slug);
        // Optimistic local remove; daemon confirms via PaneKilled.
        self.panes.retain(|p| p.slug != slug);
        self.workspace_focus.retain(|_, s| s != slug);
        // Never leave a workspace with panes but no active pane.
        let prev = self.active_slug.clone();
        self.ensure_active_pane_in_workspace();
        if self.active_slug != prev {
            if let Some(next) = self.active_slug.clone() {
                self.pending_focus = Some(next);
            }
        }
        if self.flipped.as_ref().is_some_and(|(s, _)| s == slug) {
            self.flipped = None;
        }
        if self.whisper.as_ref().is_some_and(|(s, _)| s == slug) {
            self.whisper = None;
        }
        if self.zoomed_slug.as_deref() == Some(slug) {
            self.zoomed_slug = None;
        }
        if matches!(&self.drawer, Drawer::Pad { slug: s } if s == slug) {
            self.drawer = Drawer::Closed;
        }
        cx.notify();
    }

    /// Banish the focused pane (hotkey).
    fn kill_active_pane(&mut self, cx: &mut Context<Self>) {
        if let Some(slug) = self.active_slug.clone() {
            self.kill_session(&slug, cx);
        }
    }

    // ---- pop-out ----

    fn toggle_popout(&mut self, slug: &str, cx: &mut Context<Self>) {
        let popped = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .is_some_and(|p| p.popped.is_some());
        if popped {
            self.pop_in(slug, cx);
        } else {
            self.pop_out(slug, cx);
        }
    }

    fn pop_out(&mut self, slug: &str, cx: &mut Context<Self>) {
        let Some(idx) = self.panes.iter().position(|p| p.slug == slug) else {
            return;
        };
        if let Some(handle) = &self.panes[idx].popped {
            // Already out — just raise its window.
            let _ = handle.update(cx, |_, window, _| window.activate_window());
            return;
        }

        let pane = &self.panes[idx];
        let view = pane.content_any_view();
        let name = format!("{} ✦ seance", pane.name);
        let pane_name = pane.name.clone();
        let slug_owned = pane.slug.clone();
        let app = cx.entity().downgrade();

        let bounds = gpui::Bounds::centered(None, gpui::size(px(960.), px(640.)), cx);
        let result = cx.open_window(
            gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(name.into()),
                    ..Default::default()
                }),
                app_id: Some("seance".into()),
                ..Default::default()
            },
            |window, cx| {
                // WM close (title-bar ✕) returns the pane to the circle.
                let weak = app.clone();
                let slug_close = slug_owned.clone();
                window.on_window_should_close(cx, move |_, cx| {
                    if let Some(app) = weak.upgrade() {
                        app.update(cx, |app, cx| app.note_popout_closed(&slug_close, cx));
                    }
                    true
                });
                let popout = cx.new(|_| crate::popout::PopoutView {
                    slug: slug_owned.clone(),
                    name: pane_name.clone(),
                    view: view.clone(),
                    app: app.clone(),
                });
                cx.new(|cx| gpui_component::Root::new(popout, window, cx))
            },
        );

        match result {
            Ok(handle) => {
                self.panes[idx].popped = Some(handle);
                self.active_slug = Some(slug.to_string());
                cx.notify();
            }
            Err(err) => eprintln!("[seance] pop-out failed: {err:#}"),
        }
    }

    /// Return a popped pane to the main window (closes its OS window).
    pub fn pop_in(&mut self, slug: &str, cx: &mut Context<Self>) {
        let Some(idx) = self.panes.iter().position(|p| p.slug == slug) else {
            return;
        };
        if let Some(handle) = self.panes[idx].popped.take() {
            self.panes[idx].tiled = true;
            // Defer removal: pop_in may be invoked from inside that window's
            // own update cycle (the "return to circle" button).
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            });
            cx.notify();
        }
    }

    /// The popped window is closing via the WM — reclaim the pane.
    fn note_popout_closed(&mut self, slug: &str, cx: &mut Context<Self>) {
        if let Some(pane) = self.panes.iter_mut().find(|p| p.slug == slug) {
            if pane.popped.take().is_some() {
                pane.tiled = true;
                cx.notify();
            }
        }
    }

    /// Toggle the notes face of the active pane (ctrl+shift+s).
    fn toggle_notes_flip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slug) = self.active_slug.clone() else {
            return;
        };
        self.flip_notes_for(&slug, window, cx);
    }

    /// Flip a pane onto its notes face, or flip it back if already notes-up.
    fn flip_notes_for(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Kill any in-progress markdown text selection. Without this, clicking
        // "face" while the mouse is still down (or a drag that started on the
        // button) resumes selection on the re-shown file pane body.
        window.end_text_selection(cx);
        window.clear_text_selection(cx);

        if self.flipped.as_ref().is_some_and(|(s, _)| s == slug) {
            self.flipped = None;
            // Return focus to the terminal (or leave file pane unfocused).
            if let Some(pane) = self.panes.iter().find(|p| p.slug == slug) {
                pane.focus_content(window, cx);
            }
            self.client.log_event(
                "human",
                self.panes
                    .iter()
                    .find(|p| p.slug == slug)
                    .map(|p| p.workspace.as_str()),
                Some(slug),
                "notes_flip_back",
                format!("flipped '{slug}' back to face"),
            );
            cx.notify();
            return;
        }

        let Some(pane) = self.panes.iter().find(|s| s.slug == slug) else {
            return;
        };
        let title = pane.name.clone();
        let ws = pane.workspace.clone();
        self.active_slug = Some(slug.to_string());
        self.selected_workspace = Some(ws.clone());
        // Close whisper if open on this pane — notes take the body.
        if self.whisper.as_ref().is_some_and(|(s, _)| s == slug) {
            self.whisper = None;
        }
        let pad_path = pane.scratchpad.clone();
        let drawer =
            cx.new(|cx| ScratchpadDrawer::new(self.client.clone(), pad_path, title, window, cx));
        // Focus the notes editor.
        let focus = drawer.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.flipped = Some((slug.to_string(), drawer));
        self.client.log_event(
            "human",
            Some(&ws),
            Some(slug),
            "notes_flip",
            format!("flipped '{slug}' to notes"),
        );
        cx.notify();
    }

    fn open_help_window(&mut self, cx: &mut Context<Self>) {
        let bounds = gpui::Bounds::centered(None, gpui::size(px(880.), px(780.)), cx);
        let _ = cx.open_window(
            gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("grimoire ✦ seance".into()),
                    ..Default::default()
                }),
                app_id: Some("seance".into()),
                ..Default::default()
            },
            |_, cx| cx.new(|_| HelpWindow),
        );
    }

    /// No-op: the daemon (`Engine::persist`) is the sole writer of
    /// `state.json`. Dual writers caused races after the daemon split.
    fn persist(&self, _cx: &mut Context<Self>) {}

    // ---- control plane (DEAD after daemon split) ----
    //
    // All ctl ops are handled by `Engine::handle_control` in the daemon.
    // This method is retained only so old call sites don't break the
    // compile if any residual reference remains; it must never be the
    // live path.

    /// Retired: control plane lives in the daemon (`Engine::handle_control`).
    #[allow(dead_code)] // retired GUI control-plane stub — kept so residual refs still compile
    fn handle_control(
        &mut self,
        _request: ControlRequest,
        _cx: &mut Context<Self>,
    ) -> ControlResponse {
        ControlResponse::err(
            "control plane is daemon-only — this GUI path is retired (foundation 0.9.1)",
        )
    }

    /// One-click: inject seance orientation into an agent pane.
    fn arm_pane(&mut self, slug: &str, cx: &mut Context<Self>) {
        self.whisper = None;
        self.inject_into_pane(
            slug,
            SEANCE_ARM_PROMPT,
            "arm",
            "armed pane with seance orientation".into(),
            cx,
        );
    }

    /// Inject text into a terminal pane (bracketed-paste + submit) and log it.
    fn inject_into_pane(
        &mut self,
        slug: &str,
        text: &str,
        kind: &str,
        detail: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self.panes.iter().find(|p| p.slug == slug) {
            self.client
                .log_event("human", Some(&pane.workspace), Some(slug), kind, detail);
            if let Some(rt) = pane.remote_terminal() {
                rt.read(cx).inject(text.to_string(), true);
                self.touch(slug, "whispered", "you", cx);
            }
        }
        cx.notify();
    }

    fn cancel_whisper(&mut self, cx: &mut Context<Self>) {
        self.whisper = None;
        cx.notify();
    }

    /// Record a transient cross-pane touch ("⚡ driven by X") and schedule its
    /// fade — the visible-agency overlay the council converged on.
    /// Does *not* bump workspace sidebar recency (only human typing / explicit
    /// "touch" menu does that).
    fn touch(&mut self, slug: &str, verb: &str, actor: &str, cx: &mut Context<Self>) {
        self.touches.insert(
            slug.to_string(),
            (
                verb.to_string(),
                actor.to_string(),
                std::time::Instant::now(),
            ),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(5200))
                .await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |app: &mut SeanceApp, cx| {
                    app.touches
                        .retain(|_, (_, _, at)| at.elapsed().as_millis() < 5000);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn answer_ask(&mut self, id: &str, answer: String, cx: &mut Context<Self>) {
        let _ = self.client.answer_ask(id, &answer);
        if let Some(ask) = self.asks.iter_mut().find(|a| a.id == id) {
            ask.answer = Some(answer);
            cx.notify();
        }
    }

    // ---- rendering ----

    fn focus_pane_slug(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.active_slug = Some(slug.to_string());
        if let Some(p) = self.panes.iter().find(|p| p.slug == slug) {
            let ws = p.workspace.clone();
            if self.selected_workspace.as_deref() != Some(ws.as_str()) {
                self.selected_workspace = Some(ws.clone());
            }
            let _ = self.client.set_focus(Some(slug.to_string()), Some(ws));
        }
        self.focus_pane_if_possible(slug, cx);
        self.pending_focus = Some(slug.to_string());
        let _ = window;
        cx.notify();
    }

    fn inject_prompt_into_active(&mut self, body: &str, cx: &mut Context<Self>) {
        let Some(slug) = self.active_slug.clone() else {
            return;
        };
        let (cwd, _cmd) = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| (p.cwd.clone(), p.command.clone()))
            .unwrap_or_else(|| (".".into(), String::new()));
        let text = crate::prompts::expand(body, &slug, &cwd, "");
        let _ = self.client.inject(&slug, &text, true);
        // Caller may not have a window; mark for focus restore on next render.
        self.palette = PaletteMode::Closed;
        self.pending_focus = Some(slug);
        cx.notify();
    }
}

impl Render for SeanceApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme_bg = cx.theme().background;
        let _ = theme_bg;

        // SEANCE_DEBUG_RENDER=1: 5s render-rate + construction-cost report to
        // stderr (gui.stderr.log). Measures element construction only — gpui
        // layout/paint happen outside this fn — but the RATE is exact.
        self.render_probe.tick();
        crate::latency_probe::complete("g_kick", "app", "gui kick→render");
        // Stamp render entry; paint_grid records "gui render→paint" against it
        // — locates frame cost between element construction and canvas paint
        // (i.e. gpui layout) without patching gpui.
        crate::remote_term_view::stamp_render_start();
        // Always-on inter-render gap ([seance lat] "gui render gap"): the
        // effective app frame cadence — the ceiling on notify→paint latency.
        {
            static LAST_RENDER: std::sync::Mutex<Option<std::time::Instant>> =
                std::sync::Mutex::new(None);
            let mut g = LAST_RENDER.lock().unwrap();
            if let Some(prev) = *g {
                crate::latency_probe::record("gui render gap", prev.elapsed().as_micros() as u64);
            }
            *g = Some(std::time::Instant::now());
        }

        // Launch-strip config hot-reload (background bridge stat, ~2s throttle).
        self.reload_quicklaunch_if_stale(cx);
        self.reload_host_menus_if_stale(cx);

        // Record where we are for the mouse's back/forward buttons. Watching
        // the selection is the only way to catch every mover (see the method).
        self.sync_nav_history();

        // Summon arrives without a Window on the event path; open rename here.
        if self.pending_rename.is_some() {
            self.flush_pending_rename(window, cx);
        }
        // Launch / spawn: put keyboard on the active terminal once the view exists.
        // Skip while palette / rename / whisper / notes drawer / quicklaunch
        // editor owns input.
        if matches!(self.palette, PaletteMode::Closed)
            && self.renaming.is_none()
            && self.whisper.is_none()
            && self.flipped.is_none()
            && self.quicklaunch_editor.is_none()
        {
            self.ensure_keyboard_focus(window, cx);
        }

        // Timed section construction (SEANCE_DEBUG_RENDER=1 reports per-section
        // avg/max ms — pinpoints which surface makes frames expensive).
        let active = window.is_window_active();
        let t0 = std::time::Instant::now();
        let sidebar_el = self.render_sidebar(active, cx).into_any_element();
        let t1 = std::time::Instant::now();
        let asks_el: Vec<gpui::AnyElement> = self
            .render_asks(cx)
            .into_iter()
            .map(|e| e.into_any_element())
            .collect();
        let shelf_el = self.render_minimize_shelf(active, cx).into_any_element();
        let stage_el = self.render_stage_strip(active, cx).into_any_element();
        let awaken_el = self.render_awaken_bar(cx);
        let pr_el = self.render_pr_chip(cx);
        let t2 = std::time::Instant::now();
        let tiles_el = self.render_tiles(active, cx).into_any_element();
        let t3 = std::time::Instant::now();
        self.render_probe.add("sidebar", t1 - t0);
        self.render_probe.add("strips", t2 - t1);
        self.render_probe.add("tiles", t3 - t2);

        div()
            .id("seance-root")
            .size_full()
            .flex()
            .bg(SeancePalette::bg())
            .track_focus(&self.focus_handle)
            // Capture phase: app chords + palette win before focused terminal.
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                this.on_global_key_capture(event, window, cx);
            }))
            .on_action(cx.listener(|this, act: &ActToggleTiled, _, cx| {
                this.toggle_tiled(&act.0, cx);
            }))
            .on_action(cx.listener(|this, act: &ActOpenNotes, window, cx| {
                this.flip_notes_for(&act.0.clone(), window, cx);
            }))
            .on_action(cx.listener(|this, act: &ActKillSession, _, cx| {
                this.kill_session(&act.0.clone(), cx);
            }))
            .on_action(cx.listener(|this, act: &ActKillWorkspace, window, cx| {
                this.kill_workspace(&act.0.clone(), window, cx);
            }))
            .on_action(cx.listener(|this, act: &ActMoveToWorkspace, _, cx| {
                this.move_to_workspace(&act.slug.clone(), &act.workspace.clone(), cx);
            }))
            .on_action(cx.listener(|this, act: &ActMoveToNewWorkspace, _, cx| {
                let n = this.known_workspace_names().len() + 1;
                this.move_to_workspace(&act.0.clone(), &format!("circle-{n}"), cx);
            }))
            .on_action(cx.listener(|this, act: &ActTogglePopout, _, cx| {
                this.toggle_popout(&act.0.clone(), cx);
            }))
            .on_action(cx.listener(|this, act: &ActRenamePane, window, cx| {
                let current = this
                    .panes
                    .iter()
                    .find(|p| p.slug == act.0)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                this.start_rename(RenameTarget::Pane(act.0.clone()), &current, window, cx);
            }))
            .on_action(cx.listener(|this, act: &ActForkWorkspace, _, cx| {
                this.fork_workspace(&act.0.clone(), None, "human", cx);
            }))
            .on_action(cx.listener(|this, act: &ActRenameWorkspace, window, cx| {
                let label = this.workspace_label(&act.0);
                this.start_rename(RenameTarget::Workspace(act.0.clone()), &label, window, cx);
            }))
            .on_action(cx.listener(|_this, act: &ActShareReplay, _, _cx| {
                share_replay_open(&act.0);
            }))
            .on_action(cx.listener(|_this, act: &ActOpenPrLink, _, _cx| {
                crate::sysopen::open_detached(&act.0);
            }))
            .on_action(cx.listener(|this, act: &ActRemovePrLink, _, cx| {
                this.remove_pr_link(&act.workspace.clone(), &act.url.clone(), cx);
            }))
            .on_action(cx.listener(|this, act: &ActClearPrLinks, _, cx| {
                this.clear_pr_links(&act.0.clone(), cx);
            }))
            .on_action(cx.listener(|this, act: &ActSleepWorkspace, _, cx| {
                let _ = this.client.sleep_workspace(&act.0.clone());
                cx.notify();
            }))
            .on_action(cx.listener(|this, act: &ActWakeWorkspace, window, cx| {
                this.wake_workspace_focused(&act.0.clone(), window, cx);
            }))
            .on_action(cx.listener(|this, act: &ActParkWorkspace, window, cx| {
                this.park_workspace(&act.0.clone(), window, cx);
            }))
            .on_action(cx.listener(|this, act: &ActActivateWorkspace, _, cx| {
                this.activate_workspace(&act.0.clone());
                cx.notify();
            }))
            .on_action(cx.listener(|this, act: &ActPinWorkspace, _, cx| {
                this.pin_workspace(&act.0.clone());
                cx.notify();
            }))
            .on_action(cx.listener(|this, act: &ActUnpinWorkspace, _, cx| {
                this.unpin_workspace(&act.0.clone());
                cx.notify();
            }))
            .on_action(cx.listener(|this, act: &ActTouchWorkspace, _, cx| {
                this.touch_workspace(&act.0);
                cx.notify();
            }))
            .on_action(cx.listener(|this, act: &ActQuickLaunchEdit, window, cx| {
                this.open_quicklaunch_editor(Some(&act.0.clone()), window, cx);
            }))
            .on_action(cx.listener(|this, act: &ActQuickLaunchRemove, _, cx| {
                this.quicklaunch_remove(&act.0.clone(), cx);
            }))
            // Mouse side buttons walk the circles you've been in. Bound on the
            // root so they work over a terminal, the rail, anywhere — the
            // terminal forwards no button events to the PTY, so nothing
            // downstream wants them. A modal overlay that `occlude()`s
            // (menus, the PR board, the overview) swallows them, which is the
            // right answer: navigate the thing in front of you first.
            .on_mouse_down(
                gpui::MouseButton::Navigate(gpui::NavigationDirection::Back),
                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                    this.nav_back(window, cx);
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Navigate(gpui::NavigationDirection::Forward),
                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                    this.nav_forward(window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, window, cx| {
                let Some(drag) = this.sash_drag.clone() else {
                    return;
                };
                let bounds = window.bounds();
                let x: f32 = ev.position.x.into();
                let w: f32 = bounds.size.width.into();
                let main_left = 232.0;
                let main_w = (w - main_left).max(100.0);
                match drag {
                    SashDrag::TwoPane => {
                        let ratio = ((x - main_left) / main_w).clamp(0.2, 0.8);
                        this.split_ratio = ratio;
                    }
                    SashDrag::Pair {
                        left,
                        right,
                        start_x,
                        left_w,
                        right_w,
                    } => {
                        // Delta as fraction of main width → rebalance pair weights.
                        let dx = (x - start_x) / main_w;
                        let sum = (left_w + right_w).max(0.3);
                        let mut nl = (left_w + dx * sum).clamp(0.15, sum - 0.15);
                        let mut nr = sum - nl;
                        if nr < 0.15 {
                            nr = 0.15;
                            nl = sum - nr;
                        }
                        this.pane_weights.insert(left, nl);
                        this.pane_weights.insert(right, nr);
                    }
                    SashDrag::RowPair {
                        above_key,
                        below_key,
                        start_y,
                        above_w,
                        below_w,
                    } => {
                        let h: f32 = bounds.size.height.into();
                        let main_h = (h - 40.0).max(80.0); // rough chrome
                        let y: f32 = ev.position.y.into();
                        let dy = (y - start_y) / main_h;
                        let sum = (above_w + below_w).max(0.3);
                        let mut na = (above_w + dy * sum).clamp(0.15, sum - 0.15);
                        let mut nb = sum - na;
                        if nb < 0.15 {
                            nb = 0.15;
                            na = sum - nb;
                        }
                        this.row_weights.insert(above_key, na);
                        this.row_weights.insert(below_key, nb);
                    }
                }
                cx.notify();
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.sash_drag.is_some() {
                        this.sash_drag = None;
                        save_layout_daemon(
                            &this.client,
                            this.split_ratio,
                            &this.pane_weights,
                            &this.row_weights,
                        );
                        cx.notify();
                    }
                }),
            )
            .child(sidebar_el)
            .child(
                // min_w_0 is load-bearing: without it the main column's
                // min-content width (sum of tile mins) blocks window shrink
                // and the right edge of the last pane goes off-screen.
                div()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .children(asks_el)
                    .child(shelf_el)
                    .child(pr_el)
                    .child(stage_el)
                    .child(awaken_el)
                    .child(tiles_el),
            )
            .children(
                self.overview
                    .then(|| self.render_overview(cx).into_any_element()),
            )
            .children(
                (self.pr_board && !self.overview)
                    .then(|| self.render_pr_board(cx).into_any_element()),
            )
            // Below the (deferred) menu dropdown, above everything else — the
            // click-away catcher for an open host menu.
            .children(self.render_host_menu_scrim(window, cx))
            .children(self.render_palette(cx))
            .children(self.render_quicklaunch_editor(cx))
            .children(self.render_gui_menu(cx))
            .children(match &self.drawer {
                Drawer::Closed => None,
                Drawer::Activity => Some(
                    div()
                        .flex_none()
                        .w(px(400.))
                        .h_full()
                        .flex()
                        .flex_col()
                        .border_l_1()
                        .border_color(SeancePalette::border())
                        .bg(SeancePalette::bg_elevated())
                        .child(drawer_close_bar("activity", cx))
                        .child(
                            div()
                                .id("activity-drawer")
                                .flex_1()
                                .overflow_y_scroll()
                                .child(self.render_activity()),
                        )
                        .into_any_element(),
                ),
                Drawer::Pad { slug } => {
                    let slug = slug.clone();
                    Some(
                        div()
                            .flex_none()
                            .w(px(420.))
                            .h_full()
                            .flex()
                            .flex_col()
                            .border_l_1()
                            .border_color(SeancePalette::border())
                            .bg(SeancePalette::bg_elevated())
                            .child(drawer_close_bar("pad", cx))
                            .child(self.render_pad_drawer(&slug, cx))
                            .into_any_element(),
                    )
                }
            })
    }
}

/// "share replay…": make sure the web bridge is up, then open the editor in
/// the default browser. Best-effort — every failure is a notify, never a crash.
fn share_replay_open(workspace: &str) {
    let token_path = crate::runtime::state_data_dir().join("web-token");
    let token = std::fs::read_to_string(&token_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    // Ensure a bridge: if nothing answers on 9666, spawn one detached.
    let up = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], 9666)),
        std::time::Duration::from_millis(300),
    )
    .is_ok();
    if !up {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .arg("web")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            std::thread::sleep(std::time::Duration::from_millis(600));
        }
    }
    let url = format!("http://127.0.0.1:9666/?token={token}#replay-edit?workspace={workspace}");
    crate::sysopen::open_detached(&url);
}
