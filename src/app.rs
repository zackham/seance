//! SeanceApp: root view. Left sidebar (panes grouped by workspace),
//! auto-tiling terminal region, per-pane notes flip, control-plane pump.
//!
//! # Notes = flip the pane
//! Notes are the *back* of a pane, not a side drawer. Click ✎ (or
//! ctrl+shift+s) to flip the pane over onto its shared scratchpad; click
//! again (or the ↻ chip) to flip back. The agent sees the same file via
//! `$SEANCE_SCRATCHPAD`.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    div, prelude::*, px, relative, Action, Animation, AnimationExt as _, Context, Entity,
    FocusHandle, Focusable as _, SharedString, Window, ease_in_out,
};
use gpui_component::{
    input::{Input, InputEvent, InputState},
    menu::ContextMenuExt as _,
    ActiveTheme as _, Colorize as _, GlobalState, StyledExt as _, WindowExt as _,
};
use serde::Deserialize;

use crate::{
    control::{ControlRequest, ControlResponse},
    events,
    gui_client::GuiClient,
    pane::{spawn_pane, Pane, PaneBody, PaneKind, SpawnRequest},
    remote_term::RemoteTerminal,
    remote_term_view::{OverviewThumb, RemoteTerminalView},
    runtime::protocol::{ForeignWorkspace, GuiEvent, PaneInfo, WindowInfo},
    runtime::snapshot::GridSnapshot,
    scratchpad::{ScratchpadDrawer, ScratchpadStore},
    state::AppState,
    terminal::TerminalEvent,
    theme::SeancePalette,
};
use std::sync::Arc;

fn decode_grid_b64(
    data_b64: &str,
    base: Option<&GridSnapshot>,
) -> Result<GridSnapshot, String> {
    use base64::Engine as _;
    use crate::runtime::snapshot::decode_grid_bin_onto;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| e.to_string())?;
    decode_grid_bin_onto(&bytes, base)
}

// Sidebar context-menu actions (menu items dispatch gpui actions).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActToggleTiled(pub String);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActOpenNotes(pub String);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActKillSession(pub String);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActMoveToWorkspace {
    pub slug: String,
    pub workspace: String,
}

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActMoveToNewWorkspace(pub String);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActTogglePopout(pub String);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActForkWorkspace(pub String);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActKillWorkspace(pub String);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActRenamePane(pub String);

/// Prompt injected by the one-click "arm" action — orients an agent in a
/// seance pane so it uses the control plane instead of flying blind.
const SEANCE_ARM_PROMPT: &str = "\
You are inside **seance** — a shared live workspace where humans and agents \
work in the open. Every pane is on my screen; visibility is the point.

Your environment already has:
- `$SEANCE_SESSION` — this pane's id
- `$SEANCE_WORKSPACE` — circle name (`seance ctl` is scoped to it)
- `$SEANCE_SCRATCHPAD` — notes we share (I flip this pane to read them)
- `$SEANCE_SOCKET` — control socket

Please:
1. Run `seance ctl skill` and internalize the engagement protocol
2. Use `seance ctl` to discover/spawn/drive sibling panes in this workspace
3. Prefer `propose` (ghost text I approve) and `ask` (blocking choices) over silent risk
4. Report status (`status-set working|blocked|needs-human|done`) so I can triage
5. Write durable notes to `$SEANCE_SCRATCHPAD` — screens scroll away

**File / markdown panes (critical):**
To put a document on my screen as a live viewer, spawn a **file pane**, not a \
shell with bat/less/watch:

  seance ctl new --name notes --file /absolute/or/relative/path.md

- `.md` renders as markdown and auto-refreshes on mtime (history ◀/▶ built-in).
- Do **NOT** use `new --command 'bat …'` or `watch` loops for docs — those are \
  terminal panes; I want the native file viewer.
- Then **edit the file on disk** (Write/Edit tools). Do not `ctl send` into a \
  file pane (no PTY). Re-`read` the path yourself; the human sees the pane update.
- Wrong: `new --name x --command \"bash -c 'while true; do clear; bat f; sleep 1; done'\"`
- Right:  `new --name x --file \"$PWD/path/to/f.md\"`

Confirm you're oriented and ready, then wait for the next instruction."
;

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActRenameWorkspace(pub String);

/// Bump workspace recency without selecting it (sidebar context menu).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActTouchWorkspace(pub String);

/// Move a workspace to another GUI window (multi-window).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActTransferWorkspace {
    pub workspace: String,
    pub to_window: String,
}

/// Open a new empty OS window and transfer this workspace there.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActTransferWorkspaceNewWindow(pub String);

/// Pull every workspace into this window.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActCollectAllWindows;

/// Pull a foreign workspace into this window.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActPullWorkspace(pub String);

/// Payload for dragging a sidebar pane row onto a workspace header.
#[derive(Clone)]
pub struct DraggedPane {
    pub slug: String,
    pub name: String,
}

/// Payload for dragging a workspace header (reorder workspaces).
#[derive(Clone)]
pub struct DraggedWorkspace {
    pub name: String,
}

/// Tooltip helper: `.tooltip(tip("..."))` on any interactive element.
fn tip(text: &'static str) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
    move |window, cx| gpui_component::tooltip::Tooltip::new(text).build(window, cx)
}

/// Owned-string tooltip (host chip labels, errors, …).
fn tip_s(text: impl Into<String>) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
    let text = text.into();
    move |window, cx| gpui_component::tooltip::Tooltip::new(text.clone()).build(window, cx)
}

/// Standard selected-row fill for sidebar lists (workspaces, host chips, panes).
/// High-contrast on `bg_elevated` — not `surface` (too close to the panel).
#[inline]
fn selected_row_fill() -> gpui::Hsla {
    SeancePalette::border()
}

fn layout_file_path() -> PathBuf {
    PathBuf::from(shellexpand::tilde("~/.local/share/seance/layout.json").into_owned())
}

fn load_layout_file() -> (
    f32,
    std::collections::HashMap<String, f32>,
    std::collections::HashMap<String, f32>,
) {
    let empty = || {
        (
            0.5,
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        )
    };
    let Ok(bytes) = std::fs::read_to_string(layout_file_path()) else {
        return empty();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&bytes) else {
        return empty();
    };
    let split = v
        .get("split_ratio")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.5) as f32;
    let mut weights = std::collections::HashMap::new();
    if let Some(obj) = v.get("weights").and_then(|w| w.as_object()) {
        for (k, val) in obj {
            if let Some(f) = val.as_f64() {
                weights.insert(k.clone(), f as f32);
            }
        }
    }
    let mut row_weights = std::collections::HashMap::new();
    if let Some(obj) = v.get("row_weights").and_then(|w| w.as_object()) {
        for (k, val) in obj {
            if let Some(f) = val.as_f64() {
                row_weights.insert(k.clone(), f as f32);
            }
        }
    }
    (split.clamp(0.2, 0.8), weights, row_weights)
}

fn save_layout_file(
    split_ratio: f32,
    weights: &std::collections::HashMap<String, f32>,
    row_weights: &std::collections::HashMap<String, f32>,
) {
    let mut wmap = serde_json::Map::new();
    for (k, v) in weights {
        wmap.insert(k.clone(), serde_json::json!(*v));
    }
    let mut rmap = serde_json::Map::new();
    for (k, v) in row_weights {
        rmap.insert(k.clone(), serde_json::json!(*v));
    }
    let v = serde_json::json!({
        "split_ratio": split_ratio,
        "weights": wmap,
        "row_weights": rmap,
    });
    if let Ok(s) = serde_json::to_string_pretty(&v) {
        let path = layout_file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, s);
    }
}

fn ui_debug(msg: &str) {
    if std::env::var("SEANCE_DEBUG_UI").is_ok() {
        eprintln!("[seance:ui] {msg}");
    }
}

/// Kill in-progress platform text selection (markdown file panes are
/// `.selectable(true)`). Same fix as the face chip: sidebar drag-and-drop
/// keeps the mouse button down while the cursor crosses the tile region, and
/// without this the markdown body treats that as a text drag-select.
///
/// Cheap when idle: `has_text_selection` short-circuits. Never call this from
/// `on_drag_move` — GPUI refreshes the whole window every drag move already,
/// and clear/end walks every selectable TextView. Continuous kill was the
/// sidebar DnD frame limiter.
fn kill_text_selection(window: &mut Window, cx: &mut gpui::App) {
    if !window.has_text_selection(cx) {
        return;
    }
    window.end_text_selection(cx);
    window.clear_text_selection(cx);
}

/// Sidebar rows own their press/drag. Suppress window text selection for this
/// mouse-down (Button/Input pattern) so a reorder never starts a markdown
/// highlight — even before the drag threshold, and without per-move clears.
fn sidebar_press_no_select(window: &mut Window, cx: &mut gpui::App) {
    GlobalState::suppress_text_selection(cx);
    kill_text_selection(window, cx);
}

/// The little pill that follows the cursor during a drag.
pub struct DragPill {
    label: String,
}

impl Render for DragPill {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(SeancePalette::surface())
            .border_1()
            .border_color(SeancePalette::flame_dim())
            .text_sm()
            .text_color(SeancePalette::text())
            .child(self.label.clone())
    }
}

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
    Pad { slug: String },
}

/// Overlay palette (precanned prompts or fuzzy jump).
enum PaletteMode {
    Closed,
    Prompts { query: String, selected: usize },
    Jump { query: String, selected: usize },
}

/// The grimoire in its own window.
pub struct HelpWindow;

impl Render for HelpWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("grimoire-window")
            .size_full()
            .overflow_y_scroll()
            .bg(SeancePalette::bg())
            .child(render_help())
    }
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

fn status_color(state: &str) -> gpui::Hsla {
    match state {
        "blocked" | "risky" => SeancePalette::danger(),
        "needs-human" => SeancePalette::violet(),
        "done" => SeancePalette::success(),
        "idle" => SeancePalette::text_faint(),
        _ => SeancePalette::flame(), // planning/working
    }
}

/// Badge on an *inactive* workspace header in the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceAttention {
    /// Observed live-busy (TUI title spinner / agent actively driving).
    Working,
    /// Blocked or needs-human.
    NeedsHuman,
    /// Finished work while the human was elsewhere — sticky until select.
    Done,
}

impl WorkspaceAttention {
    fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsHuman => "needs",
            Self::Done => "done",
        }
    }
    fn color(self) -> gpui::Hsla {
        match self {
            Self::Working => SeancePalette::flame(),
            Self::NeedsHuman => SeancePalette::violet(),
            Self::Done => SeancePalette::success(),
        }
    }
    fn priority(self) -> u8 {
        match self {
            Self::NeedsHuman => 3,
            Self::Working => 2,
            Self::Done => 1,
        }
    }
}

/// Claude Code / ink TUIs put a braille spinner in the OSC title while streaming.
/// Idle Claude uses `✳` (U+2733) — that is *not* busy.
fn title_looks_busy(title: &str) -> bool {
    let t = title.trim_start();
    let Some(c) = t.chars().next() else {
        return false;
    };
    matches!(c, '\u{2800}'..='\u{28FF}')
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// If `~/.local/share/seance/scratch/<slug>.telegram.json` exists, post status
/// to that topic via vita (best-effort, never blocks the GUI).
fn telegram_status_bridge(slug: &str, state: &str, note: Option<&str>) {
    let path = PathBuf::from(shellexpand::tilde(&format!(
        "~/.local/share/seance/scratch/{slug}.telegram.json"
    )).into_owned());
    let Ok(bytes) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&bytes) else {
        return;
    };
    let Some(topic_id) = v.get("topic_id").and_then(|t| t.as_str()).map(|s| s.to_string()) else {
        return;
    };
    let text = match note {
        Some(n) if !n.is_empty() => format!("seance `{slug}` → *{state}*: {n}"),
        _ => format!("seance `{slug}` → *{state}*"),
    };
    std::thread::spawn(move || {
        let input = serde_json::json!({"topic_id": topic_id, "text": text});
        let input_s = input.to_string();
        let vita = PathBuf::from(shellexpand::tilde("~/work/vita").into_owned());
        let run = vita.join("run");
        let mut cmd = if run.exists() {
            let mut c = std::process::Command::new(&run);
            c.current_dir(&vita);
            c.args(["capabilities", "call", "vita.telegram.send", "--input", &input_s]);
            c
        } else {
            let mut c = std::process::Command::new("vita");
            c.args(["capabilities", "call", "vita.telegram.send", "--input", &input_s]);
            c
        };
        let _ = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

/// Co-presence chrome for a pane (mirrors daemon agency).
#[derive(Clone, Debug)]
struct OwnerChrome {
    owner: String,
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
    ask_counter: u64,
    /// proposal id -> (pane slug, outcome once resolved)
    proposals: std::collections::HashMap<String, (String, Option<String>)>,
    proposal_counter: u64,
    /// Active whisper compose bar: (pane slug, input state).
    whisper: Option<(String, Entity<InputState>)>,
    /// Pane currently flipped to its notes face: (slug, scratchpad entity).
    flipped: Option<(String, Entity<ScratchpadDrawer>)>,
    cmd_log: crate::cmdlog::CommandLog,
    active_slug: Option<String>,
    selected_workspace: Option<String>,
    /// Last focused pane slug per workspace — restored on workspace switch.
    workspace_focus: std::collections::HashMap<String, String>,
    extra_workspaces: Vec<String>,
    workspace_order: Vec<String>,
    renaming: Option<(RenameTarget, Entity<InputState>)>,
    drawer: Drawer,
    store: ScratchpadStore,
    focus_handle: FocusHandle,
    session_counter: usize,
    /// Connection to the session daemon (owns PTYs).
    client: Arc<GuiClient>,
    /// After a summon, focus this pane once its remote view exists.
    pending_focus: Option<String>,
    /// UI-initiated spawn/create: open the inline rename field as soon as the
    /// target exists (workspace is immediate; pane waits for PaneSpawned).
    pending_rename: Option<RenameTarget>,
    /// Next `PaneSpawned` from our summon should open rename (not external ctl).
    rename_next_spawn: bool,
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
    /// This GUI connection's window id (from daemon State).
    window_id: Option<String>,
    /// Live windows for transfer menus.
    windows: Vec<WindowInfo>,
    /// Workspaces owned by other windows (empty-sidebar pull menu).
    foreign_workspaces: Vec<ForeignWorkspace>,
    /// Last activity timestamp (ms) per workspace — input/inject/status, not click.
    workspace_touch: std::collections::HashMap<String, u64>,
    /// Sticky attention on inactive circles until selected (done/needs).
    workspace_unread: std::collections::HashMap<String, WorkspaceAttention>,
    /// Full-window live overview (ctrl+shift+space).
    overview: bool,
    /// Workspace waiting to move into a newly-opened empty window.
    pending_transfer: Option<String>,
    /// This window attached as empty (second process / new-window transfer target).
    empty_window: bool,
}

/// Active sash drag state.
#[derive(Clone)]
enum SashDrag {
    /// Classic 2-pane ratio drag.
    TwoPane { start_x: f32 },
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

impl SeanceApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_inner(window, cx, false)
    }

    /// Empty window: claims no workspaces until pull/transfer (multi-window).
    pub fn new_empty_window(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_inner(window, cx, true)
    }

    fn new_inner(window: &mut Window, cx: &mut Context<Self>, empty: bool) -> Self {
        let store = ScratchpadStore::new().expect("scratchpad dir");

        // Connect to the session daemon (PTYs live there).
        let (client, event_rx) = if empty {
            GuiClient::connect_empty().expect("gui client connect empty")
        } else {
            GuiClient::connect().expect("gui client connect to daemon")
        };

        let mut app = SeanceApp {
            panes: Vec::new(),
            asks: Vec::new(),
            statuses: std::collections::HashMap::new(),
            owners: std::collections::HashMap::new(),
            touches: std::collections::HashMap::new(),
            ask_counter: 0,
            proposals: std::collections::HashMap::new(),
            proposal_counter: 0,
            whisper: None,
            flipped: None,
            cmd_log: crate::cmdlog::CommandLog::new(),
            active_slug: None,
            selected_workspace: None,
            workspace_focus: std::collections::HashMap::new(),
            extra_workspaces: Vec::new(),
            workspace_order: Vec::new(),
            renaming: None,
            drawer: Drawer::Closed,
            store,
            focus_handle: cx.focus_handle(),
            session_counter: 0,
            client,
            pending_focus: None,
            pending_rename: None,
            rename_next_spawn: false,
            zoomed_slug: None,
            palette: PaletteMode::Closed,
            split_ratio: 0.5,
            pane_weights: std::collections::HashMap::new(),
            row_weights: std::collections::HashMap::new(),
            sash_drag: None,
            pad_refresh_tick: 0,
            host: crate::host::HostState::load(),
            window_id: None,
            windows: Vec::new(),
            foreign_workspaces: Vec::new(),
            workspace_touch: std::collections::HashMap::new(),
            workspace_unread: std::collections::HashMap::new(),
            overview: false,
            pending_transfer: None,
            empty_window: empty,
        };
        let _ = crate::prompts::ensure_user_file();
        let (split, weights, row_weights) = load_layout_file();
        app.split_ratio = split;
        app.pane_weights = weights;
        app.row_weights = row_weights;

        // Host bridge: poll optional sidebar widgets (claude accounts, …).
        if app.host.enabled() {
            let (host_tx, mut host_rx) =
                futures::channel::mpsc::unbounded::<crate::host::HostState>();
            let poll_secs = app.host.min_poll_secs();
            std::thread::Builder::new()
                .name("seance-host-poll".into())
                .spawn(move || {
                    let mut state = crate::host::HostState::load();
                    loop {
                        state.poll_all();
                        if host_tx.unbounded_send(state.clone()).is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_secs(poll_secs));
                    }
                })
                .ok();
            cx.spawn(async move |this, cx| {
                use futures::StreamExt as _;
                while let Some(next) = host_rx.next().await {
                    let Some(this) = this.upgrade() else { break };
                    this.update(cx, |app: &mut SeanceApp, cx| {
                        app.host.widgets = next.widgets;
                        app.host.ever_ok = next.ever_ok || app.host.ever_ok;
                        cx.notify();
                    });
                }
            })
            .detach();
        }

        // Bridge: std thread blocks on daemon events → unbounded mpsc → gpui task.
        let (async_tx, mut async_rx) = futures::channel::mpsc::unbounded::<GuiEvent>();
        std::thread::Builder::new()
            .name("seance-gui-events".into())
            .spawn(move || {
                while let Ok(ev) = event_rx.recv() {
                    if async_tx.unbounded_send(ev).is_err() {
                        break;
                    }
                }
            })
            .ok();

        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(ev) = async_rx.next().await {
                let Some(this) = this.upgrade() else { break };
                this.update(cx, |app: &mut SeanceApp, cx| {
                    app.apply_gui_event_no_window(ev, cx);
                });
            }
        })
        .detach();

        // Live-refresh pad drawer every 2s while open (disk mtime/content).
        cx.spawn(async move |this, cx| {
            loop {
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
                foreign_workspaces,
            } => {
                // Multi-window identity + peer list.
                let prev_windows: std::collections::HashSet<String> =
                    self.windows.iter().map(|w| w.id.clone()).collect();
                if let Some(id) = window_id {
                    self.window_id = Some(id);
                }
                self.windows = windows;
                self.foreign_workspaces = foreign_workspaces;

                // Complete "send to new window": transfer once the empty peer appears.
                if let Some(ws) = self.pending_transfer.clone() {
                    let self_id = self.window_id.clone();
                    let peer = self.windows.iter().find(|w| {
                        Some(w.id.as_str()) != self_id.as_deref() && !prev_windows.contains(&w.id)
                    });
                    if let Some(peer) = peer {
                        let to = peer.id.clone();
                        let _ = self.client.transfer_workspace(&ws, &to);
                        self.pending_transfer = None;
                    }
                }

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
                self.statuses = statuses
                    .into_iter()
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
                self.panes.sort_by_key(|p| {
                    order.get(p.slug.as_str()).copied().unwrap_or(usize::MAX)
                });
                // active_slug from daemon; repair if missing / not in selected
                // workspace. Keyboard recovery is render-side (ensure_keyboard_focus)
                // so we don't steal focus from whisper / rename / palette here.
                self.ensure_active_pane_in_workspace();
                cx.notify();
            }
            GuiEvent::Grid(snap) => {
                self.apply_grid_snap(snap, cx);
            }
            GuiEvent::GridBin { pane, data_b64 } => {
                // Damage frames need the previous snapshot as base.
                let base = self
                    .panes
                    .iter()
                    .find(|p| p.slug == pane)
                    .and_then(|p| p.remote_terminal())
                    .map(|rt| rt.read(cx).snapshot.clone());
                let base_ref = base.as_ref().map(|a| a.as_ref());
                match decode_grid_b64(&data_b64, base_ref) {
                    Ok(snap) => self.apply_grid_snap(snap, cx),
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
                                "[seance gui] grid_bin resync for {pane}: {e} (cleared base; full reattach)"
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
                            let _ = self.client.send(crate::runtime::protocol::GuiRequest::Attach {
                                empty: false,
                                selected_workspace: self.selected_workspace.clone(),
                                focused_pane: self.active_slug.clone(),
                            });
                        }
                    }
                }
            }
            GuiEvent::PaneSpawned { pane } => {
                let slug = pane.slug.clone();
                let ws = pane.workspace.clone();
                self.ensure_remote_pane_cx(&pane, cx);
                // Summon → select workspace, make active, focus the new pane.
                self.selected_workspace = Some(ws.clone());
                self.active_slug = Some(slug.clone());
                self.pending_focus = Some(slug.clone());
                let _ = self.client.set_focus(Some(slug.clone()), Some(ws));
                self.focus_pane_if_possible(&slug, cx);
                // UI summon: open the sidebar title in edit mode so the human
                // can name it immediately. External `ctl new` leaves the flag
                // false and does not steal rename focus.
                if self.rename_next_spawn {
                    self.rename_next_spawn = false;
                    self.pending_rename = Some(RenameTarget::Pane(slug));
                }
                cx.notify();
            }
            GuiEvent::PaneKilled { slug } => {
                self.panes.retain(|p| p.slug != slug);
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
                    telegram_status_bridge(&slug, &state, note.as_deref());
                }
                self.note_workspace_status_event(&slug, &state);
                self.statuses.insert(slug, PaneStatus { state, note });
                if matches!(self.drawer, Drawer::Pad { .. }) {
                    self.pad_refresh_tick = self.pad_refresh_tick.wrapping_add(1);
                }
                cx.notify();
            }
            GuiEvent::Touch { slug, verb, actor } => {
                self.touch(&slug, &verb, &actor, cx);
            }
            GuiEvent::InputOrigin { pane, origin } => {
                // Real input (keystroke / inject / propose) bumps recency.
                // Focus/select alone never emits InputOrigin.
                if let Some(ws) = self
                    .panes
                    .iter()
                    .find(|p| p.slug == pane)
                    .map(|p| p.workspace.clone())
                {
                    self.touch_workspace(&ws);
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
            GuiEvent::Error { message } => {
                eprintln!("[seance gui] daemon error: {message}");
            }
            GuiEvent::Ack { .. } | GuiEvent::Pong => {}
        }
    }

    /// Apply a decoded grid to the matching remote pane. Shared by JSON
    /// `grid` and binary `grid_bin` events. Only paints panes on the selected
    /// workspace (other workspaces never get live pushes from the daemon).
    fn apply_grid_snap(&mut self, snap: GridSnapshot, cx: &mut Context<Self>) {
        let slug = snap.pane.clone();
        // Skip paint work for panes not on screen. Hidden workspaces with
        // spinning TUIs used to keep the GUI at 90%+ CPU.
        let ws = self.selected_workspace.as_deref();
        let visible = self.panes.iter().any(|p| {
            p.slug == slug && p.popped.is_none() && ws.is_none_or(|w| p.workspace == w)
        });
        if !visible {
            return;
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
                PaneBody::Terminal { view, .. } => Some(view.read(cx).focus_handle()),
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

    /// During render we have a Window — apply pending_focus (summon / palette
    /// close), or recover when nothing in the window is focused (cold launch).
    fn ensure_keyboard_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(slug) = self.pending_focus.clone() {
            if let Some(pane) = self.panes.iter().find(|p| p.slug == slug) {
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
        // Cold launch / dead handle: GPUI focus is None → key path is only the
        // absolute root node, so seance chords and terminals never see keys.
        if window.focused(cx).is_none() {
            if let Some(slug) = self.active_slug.clone() {
                if let Some(pane) = self.panes.iter().find(|p| p.slug == slug) {
                    pane.focus_content(window, cx);
                }
            }
        }
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = PaletteMode::Closed;
        // Return keys to the active terminal after overlay.
        if let Some(slug) = self.active_slug.clone() {
            if let Some(pane) = self.panes.iter().find(|p| p.slug == slug) {
                pane.focus_content(window, cx);
            }
        }
        cx.notify();
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
            if self.overview {
                self.set_overview(false, cx);
                cx.stop_propagation();
                return;
            }
            if self.renaming.is_some() {
                self.renaming = None;
                self.pending_rename = None;
                if let Some(slug) = self.active_slug.clone() {
                    if let Some(pane) = self.panes.iter().find(|p| p.slug == slug) {
                        pane.focus_content(window, cx);
                    }
                }
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
                    // Empty then banish workspace (or active pane if empty circle).
                    if let Some(ws) = self.selected_workspace.clone() {
                        let has = self.panes.iter().any(|p| p.workspace == ws);
                        if has {
                            self.kill_workspace(&ws, cx);
                        } else {
                            self.kill_active_pane(cx);
                        }
                    } else {
                        self.kill_active_pane(cx);
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
            }
            return;
        }
        if info.kind == "file" {
            let path = std::path::PathBuf::from(
                info.file
                    .clone()
                    .unwrap_or_else(|| info.command.clone()),
            );
            let view = cx.new(|cx| crate::fileview::FileView::new(path.clone(), cx));
            self.panes.push(Pane {
                kind: PaneKind::File,
                name: info.name.clone(),
                slug: info.slug.clone(),
                workspace: info.workspace.clone(),
                cwd: info.cwd.clone(),
                command: info.command.clone(),
                tiled: info.tiled,
                resume_on_restore: false,
                scratch_path: std::path::PathBuf::from(&info.scratchpad),
                body: PaneBody::File { view },
                popped: None,
            });
            return;
        }
        let terminal =
            cx.new(|_cx| RemoteTerminal::new(info.slug.clone(), Arc::clone(&self.client)));
        let view = cx.new(|cx| RemoteTerminalView::new(terminal.clone(), cx));
        self.panes.push(Pane {
            kind: PaneKind::Terminal,
            name: info.name.clone(),
            slug: info.slug.clone(),
            workspace: info.workspace.clone(),
            cwd: info.cwd.clone(),
            command: info.command.clone(),
            tiled: info.tiled,
            resume_on_restore: false,
            scratch_path: std::path::PathBuf::from(&info.scratchpad),
            body: PaneBody::Remote { terminal, view },
            popped: None,
        });
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
                tiled: true,
                resume: false,
                file: None,
            },
            cx,
        );
    }

    /// All workspaces in sidebar display order.
    /// Bands: working → needs → done-unread → rest; each by activity recency
    /// (input/inject/status — *not* click-to-select).
    fn workspaces(&self) -> Vec<String> {
        let mut known: std::collections::HashSet<String> = self
            .panes
            .iter()
            .map(|s| s.workspace.clone())
            .chain(self.extra_workspaces.iter().cloned())
            .chain(self.selected_workspace.iter().cloned())
            .collect();
        let mut out: Vec<String> = known.iter().cloned().collect();
        out.sort_by_key(|ws| self.workspace_sort_key(ws));
        // Keep explicit order as a weak tie-break for equal keys via stable sort of residual.
        let _ = self.workspace_order;
        out
    }

    fn workspace_sort_key(&self, ws: &str) -> (u8, std::cmp::Reverse<u64>, String) {
        let band = self.workspace_band(ws);
        let touch = self.workspace_touch.get(ws).copied().unwrap_or(0);
        (band, std::cmp::Reverse(touch), ws.to_string())
    }

    /// 0=working, 1=needs, 2=done-unread, 3=rest.
    fn workspace_band(&self, ws: &str) -> u8 {
        if let Some(a) = self.workspace_attention(ws) {
            match a {
                WorkspaceAttention::Working => 0,
                WorkspaceAttention::NeedsHuman => 1,
                WorkspaceAttention::Done => 2,
            }
        } else {
            3
        }
    }

    /// Observed live-busy: braille OSC title spinner, or agent-owned status.
    fn pane_is_live_working(&self, slug: &str, cx: &gpui::App) -> bool {
        if let Some(title) = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| p.title(cx))
        {
            if title_looks_busy(&title) {
                return true;
            }
        }
        let owner = self.owners.get(slug);
        let st = self.statuses.get(slug).map(|s| s.state.as_str());
        match (owner, st) {
            // Human-owned sticky "working" is often stale inject chrome — ignore.
            (Some(o), Some("working") | Some("planning")) if o.owner == "human" => false,
            (_, Some("working") | Some("planning")) => true,
            (Some(o), _) if o.owner.starts_with("agent:") && !o.exited => {
                // Agent holds keys without status-set — still "live" if title busy already handled.
                false
            }
            _ => false,
        }
    }

    fn touch_workspace(&mut self, ws: &str) {
        if ws.is_empty() {
            return;
        }
        self.workspace_touch.insert(ws.to_string(), now_ms());
    }

    /// Ensure `ws` is listed in sidebar order, appended at the bottom when new.
    fn ensure_workspace_at_bottom(&mut self, ws: &str) {
        if self.workspace_order.iter().any(|w| w == ws) {
            return;
        }
        self.workspace_order.push(ws.to_string());
        self.touch_workspace(ws);
    }

    fn note_workspace_status_event(&mut self, slug: &str, state: &str) {
        let Some(ws) = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| p.workspace.clone())
        else {
            return;
        };
        self.touch_workspace(&ws);
        // Sticky unread only when the human is *not* looking at this circle.
        if self.selected_workspace.as_deref() == Some(ws.as_str()) {
            self.workspace_unread.remove(&ws);
            return;
        }
        let att = match state {
            "needs-human" | "blocked" | "risky" => Some(WorkspaceAttention::NeedsHuman),
            "done" => Some(WorkspaceAttention::Done),
            "working" | "planning" => Some(WorkspaceAttention::Working),
            _ => None,
        };
        if let Some(a) = att {
            let cur = self.workspace_unread.get(&ws).copied();
            if cur.map(|c| a.priority() > c.priority()).unwrap_or(true) {
                self.workspace_unread.insert(ws, a);
            }
        }
    }

    fn workspace_attention(&self, workspace: &str) -> Option<WorkspaceAttention> {
        // Live busy wins over sticky status.
        let live = self.panes.iter().any(|p| {
            p.workspace == workspace && {
                // Need App for title — approximate via statuses + owners without cx:
                // title check done in render path with cx. Here status/owner only.
                match self.statuses.get(&p.slug).map(|s| s.state.as_str()) {
                    Some("working") | Some("planning") => {
                        let human = self
                            .owners
                            .get(&p.slug)
                            .map(|o| o.owner == "human")
                            .unwrap_or(false);
                        !human
                    }
                    Some("needs-human") | Some("blocked") | Some("risky") => false, // handled below
                    _ => false,
                }
            }
        });
        // needs-human live
        let needs = self.panes.iter().any(|p| {
            p.workspace == workspace
                && matches!(
                    self.statuses.get(&p.slug).map(|s| s.state.as_str()),
                    Some("needs-human") | Some("blocked") | Some("risky")
                )
        });
        if needs {
            return Some(WorkspaceAttention::NeedsHuman);
        }
        if live {
            return Some(WorkspaceAttention::Working);
        }
        // Also check unread sticky
        self.workspace_unread.get(workspace).copied()
    }

    /// Live attention with title spinners (needs `&App`).
    fn workspace_attention_cx(&self, workspace: &str, cx: &gpui::App) -> Option<WorkspaceAttention> {
        let needs = self.panes.iter().any(|p| {
            p.workspace == workspace
                && matches!(
                    self.statuses.get(&p.slug).map(|s| s.state.as_str()),
                    Some("needs-human") | Some("blocked") | Some("risky")
                )
        });
        if needs {
            return Some(WorkspaceAttention::NeedsHuman);
        }
        let working = self
            .panes
            .iter()
            .any(|p| p.workspace == workspace && self.pane_is_live_working(&p.slug, cx));
        if working {
            return Some(WorkspaceAttention::Working);
        }
        self.workspace_unread.get(workspace).copied()
    }

    /// Open an empty OS window (same process) for multi-window transfers.
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

    fn send_workspace_to_new_window(&mut self, workspace: &str, cx: &mut Context<Self>) {
        self.pending_transfer = Some(workspace.to_string());
        self.open_empty_os_window(cx);
    }

    fn set_overview(&mut self, on: bool, cx: &mut Context<Self>) {
        self.overview = on;
        let _ = self.client.set_overview(on);
        cx.notify();
    }

    fn overview_thumb_for(
        &self,
        terminal: &gpui::Entity<RemoteTerminal>,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<OverviewThumb> {
        cx.new(|cx| OverviewThumb::new(terminal.clone(), cx))
    }

    fn render_overview(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspaces = self.workspaces();
        let selected = self.selected_workspace.clone();
        let n = workspaces.len().max(1);
        let cols = (n as f32).sqrt().ceil() as usize;
        let cards = self.pack_overview_cards(workspaces, selected, cols.max(1), cx);
        div()
            .id("overview")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(SeancePalette::bg())
            .child(
                div()
                    .flex_none()
                    .h(px(40.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(SeancePalette::border())
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(SeancePalette::text())
                            .child("overview · all workspaces"),
                    )
                    .child(
                        div()
                            .id("overview-close")
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_overview(false, cx);
                            }))
                            .tooltip(tip("exit overview (esc · ctrl+shift+space)"))
                            .child("esc · ctrl+shift+space"),
                    ),
            )
            .child(
                div()
                    .id("overview-scroll")
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(cards),
            )
    }

    fn pack_overview_cards(
        &mut self,
        workspaces: Vec<String>,
        selected: Option<String>,
        cols: usize,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut cards: Vec<gpui::AnyElement> = Vec::with_capacity(workspaces.len());
        for ws in &workspaces {
            let is_sel = selected.as_deref() == Some(ws.as_str());
            let attention = self.workspace_attention_cx(ws, cx);
            let thumbs: Vec<gpui::AnyElement> = self
                .panes
                .iter()
                .filter(|p| p.workspace == *ws && p.tiled && p.popped.is_none())
                .filter_map(|pane| {
                    let rt = pane.remote_terminal()?.clone();
                    let thumb = self.overview_thumb_for(&rt, cx);
                    let sc = self
                        .statuses
                        .get(&pane.slug)
                        .map(|s| status_color(&s.state))
                        .unwrap_or(SeancePalette::border());
                    Some(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h(px(80.))
                            .border_1()
                            .border_color(sc)
                            .rounded_md()
                            .overflow_hidden()
                            .child(thumb)
                            .into_any_element(),
                    )
                })
                .collect();
            let ws_click = ws.clone();
            let badge = attention.map(|a| {
                div()
                    .px_1p5()
                    .rounded_md()
                    .text_xs()
                    .text_color(a.color())
                    .child(a.label())
                    .into_any_element()
            });
            cards.push(
                div()
                    .id(SharedString::from(format!("ov-card-{ws}")))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(if is_sel {
                        SeancePalette::flame()
                    } else {
                        SeancePalette::border()
                    })
                    .bg(SeancePalette::bg_elevated())
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_overview(false, cx);
                        this.select_workspace(&ws_click, window, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .font_semibold()
                                    .text_color(SeancePalette::text())
                                    .child(ws.clone()),
                            )
                            .children(badge),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .min_h(px(100.))
                            .children(if thumbs.is_empty() {
                                vec![div()
                                    .text_xs()
                                    .text_color(SeancePalette::text_faint())
                                    .child("(empty)")
                                    .into_any_element()]
                            } else {
                                thumbs
                            }),
                    )
                    .into_any_element(),
            );
        }
        // Pack into rows of `cols`.
        let mut rows = Vec::new();
        let mut it = cards.into_iter();
        loop {
            let mut row_kids = Vec::new();
            for _ in 0..cols {
                if let Some(c) = it.next() {
                    row_kids.push(c);
                }
            }
            if row_kids.is_empty() {
                break;
            }
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .children(row_kids)
                    .into_any_element(),
            );
        }
        rows
    }

    /// Move workspace `moved` to appear before `before` in the sidebar.
    /// Optimistic local update; daemon is the source of truth and persists.
    fn reorder_workspace(&mut self, moved: &str, before: &str, cx: &mut Context<Self>) {
        if moved == before {
            return;
        }
        let mut order = self.workspaces();
        order.retain(|w| w != moved);
        let idx = order.iter().position(|w| w == before).unwrap_or(order.len());
        order.insert(idx, moved.to_string());
        self.workspace_order = order;
        // Daemon owns persistence — GUI-only save would race and be overwritten
        // by the next daemon persist with the old order.
        let _ = self.client.reorder_workspace(moved, before);
        cx.notify();
    }

    /// Move `slug` into `workspace`, positioned before pane `before_slug`
    /// (or appended when `before_slug` is None). Optimistic local reorder;
    /// daemon reorders + persists and pushes State back.
    fn reorder_pane(
        &mut self,
        slug: &str,
        workspace: &str,
        before_slug: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if Some(slug) == before_slug {
            return;
        }
        let Some(from_idx) = self.panes.iter().position(|p| p.slug == slug) else {
            return;
        };
        let mut pane = self.panes.remove(from_idx);
        pane.workspace = workspace.to_string();
        let insert_at = before_slug
            .and_then(|b| self.panes.iter().position(|p| p.slug == b))
            .unwrap_or(self.panes.len());
        events::log(
            "human",
            Some(workspace),
            Some(slug),
            "pane_moved",
            format!("moved '{}' into {} (reorder)", pane.name, workspace),
        );
        self.panes.insert(insert_at, pane);
        self.selected_workspace = Some(workspace.to_string());
        let _ = self.client.move_pane(slug, workspace, before_slug);
        cx.notify();
    }

    fn create_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.workspaces();
        let mut n = existing.len() + 1;
        let name = loop {
            let candidate = format!("circle-{n}");
            if !existing.contains(&candidate) {
                break candidate;
            }
            n += 1;
        };
        let _ = self.client.create_workspace(&name);
        if !self.extra_workspaces.contains(&name) {
            self.extra_workspaces.push(name.clone());
        }
        self.ensure_workspace_at_bottom(&name);
        self.selected_workspace = Some(name.clone());
        // Immediate inline rename — name is known up front.
        self.start_rename(RenameTarget::Workspace(name.clone()), &name, window, cx);
    }

    // ---- inline rename ----

    fn start_rename(
        &mut self,
        target: RenameTarget,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx).default_value(current.to_string())
        });
        cx.subscribe_in(
            &input,
            window,
            |this: &mut SeanceApp, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let value = input.read(cx).value().to_string();
                    this.commit_rename(value.trim(), cx);
                    let _ = window;
                }
                InputEvent::Blur => {
                    this.renaming = None;
                    cx.notify();
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
            RenameTarget::Workspace(old) => {
                let new_ws = crate::state::slugify(new_name);
                for pane in &mut self.panes {
                    if pane.workspace == old {
                        pane.workspace = new_ws.clone();
                    }
                }
                for ws in &mut self.extra_workspaces {
                    if *ws == old {
                        *ws = new_ws.clone();
                    }
                }
                for w in &mut self.workspace_order {
                    if *w == old {
                        *w = new_ws.clone();
                    }
                }
                if self.selected_workspace.as_deref() == Some(old.as_str()) {
                    self.selected_workspace = Some(new_ws.clone());
                }
                if let Some(slug) = self.workspace_focus.remove(&old) {
                    self.workspace_focus.insert(new_ws.clone(), slug);
                }
                let _ = self.client.rename_workspace(&old, &new_ws);
            }
        }
        cx.notify();
    }

    fn select_workspace(&mut self, workspace: &str, window: &mut Window, cx: &mut Context<Self>) {
        let changed = self.selected_workspace.as_deref() != Some(workspace);
        // Remember which pane was active in the circle we're leaving.
        if changed {
            if let (Some(old_ws), Some(slug)) =
                (self.selected_workspace.clone(), self.active_slug.clone())
            {
                if self
                    .panes
                    .iter()
                    .any(|p| p.slug == slug && p.workspace == old_ws)
                {
                    self.workspace_focus.insert(old_ws, slug);
                }
            }
        }
        self.selected_workspace = Some(workspace.to_string());
        // Selecting a circle clears sticky "done/needs" unread — does NOT bump touch.
        self.workspace_unread.remove(workspace);
        // When entering a circle that was off-screen, zero local revs for its
        // panes so the daemon's full flush can't be dropped as "stale". The
        // daemon also sends FULL frames on workspace change.
        if changed {
            let slugs: Vec<String> = self
                .panes
                .iter()
                .filter(|p| p.workspace == workspace)
                .map(|p| p.slug.clone())
                .collect();
            for slug in slugs {
                if let Some(rt) = self
                    .panes
                    .iter()
                    .find(|p| p.slug == slug)
                    .and_then(|p| p.remote_terminal())
                    .cloned()
                {
                    // Keep last pixels until the full frame lands — only reset
                    // the rev gate, not the cells (avoids a blank flash).
                    rt.update(cx, |t, _| t.open_rev_gate());
                }
            }
        }
        // Invariant: workspace with panes always has an active pane.
        // Keep current active if it's already in this workspace; else restore
        // remembered / first tiled / any.
        let restore = self
            .active_slug
            .clone()
            .filter(|s| {
                self.panes
                    .iter()
                    .any(|p| p.slug == *s && p.workspace == workspace)
            })
            .or_else(|| self.preferred_pane_in_workspace(workspace));
        if let Some(slug) = restore {
            if self.active_slug.as_deref() != Some(slug.as_str()) {
                self.set_active(&slug, window, cx);
                return;
            }
            let _ = self
                .client
                .set_focus(Some(slug), Some(workspace.to_string()));
        } else {
            // Empty workspace — no pane to activate.
            self.active_slug = None;
            let _ = self.client.set_focus(None, Some(workspace.to_string()));
        }
        self.persist(cx);
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

    /// Cycle the selected workspace in sidebar order. `delta` is +1 (next /
    /// PageDown) or -1 (prev / PageUp). Wraps. Focuses a pane in the target
    /// workspace when one exists so keyboard goes there.
    fn cycle_workspace(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        let list = self.workspaces();
        if list.is_empty() {
            return;
        }
        let cur = self
            .selected_workspace
            .as_deref()
            .and_then(|w| list.iter().position(|x| x == w))
            .unwrap_or(0);
        let n = list.len() as i32;
        let next = (cur as i32 + delta).rem_euclid(n) as usize;
        let ws = list[next].clone();
        if self.selected_workspace.as_deref() == Some(ws.as_str()) {
            return;
        }
        events::log(
            "human",
            Some(&ws),
            None,
            "workspace_selected",
            format!("cycled to workspace '{ws}'"),
        );
        // Restores last active pane for `ws` (or first tiled/any).
        self.select_workspace(&ws, window, cx);
    }

    fn move_to_workspace(&mut self, slug: &str, workspace: &str, cx: &mut Context<Self>) {
        // Append into target workspace (no before-slug) — same path as drag
        // onto a workspace header, so order persists via the daemon.
        self.reorder_pane(slug, workspace, None, cx);
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
        let ok = self.active_slug.as_ref().is_some_and(|s| {
            self.panes
                .iter()
                .any(|p| &p.slug == s && p.workspace == ws)
        });
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

    fn find_session(&self, key: &str) -> Option<usize> {
        self.panes
            .iter()
            .position(|s| s.slug == key || s.name == key)
    }

    fn set_active(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_slug.as_deref() != Some(slug) {
            let ws = self
                .panes
                .iter()
                .find(|p| p.slug == slug)
                .map(|p| p.workspace.clone());
            events::log("human", ws.as_deref(), Some(slug), "focus", format!("focused '{slug}'"));
        }
        self.active_slug = Some(slug.to_string());
        if let Some(pane) = self.panes.iter().find(|s| s.slug == slug) {
            let ws = pane.workspace.clone();
            self.selected_workspace = Some(ws.clone());
            self.workspace_focus.insert(ws.clone(), slug.to_string());
            let _ = self.client.set_focus(Some(slug.to_string()), Some(ws));
            pane.focus_content(window, cx);
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

    /// Kill every pane in a workspace, then drop the workspace itself.
    fn kill_workspace(&mut self, workspace: &str, cx: &mut Context<Self>) {
        let _ = self.client.kill_workspace(workspace);
        cx.notify();
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
            events::log(
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
        let drawer = cx.new(|cx| ScratchpadDrawer::new(&self.store, slug.to_string(), title, window, cx));
        // Focus the notes editor.
        let focus = drawer.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.flipped = Some((slug.to_string(), drawer));
        events::log(
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

    #[allow(dead_code)]
    /// Retired: control plane lives in the daemon (`Engine::handle_control`).
    #[allow(dead_code)]
    fn handle_control(&mut self, _request: ControlRequest, _cx: &mut Context<Self>) -> ControlResponse {
        ControlResponse::err(
            "control plane is daemon-only — this GUI path is retired (foundation 0.9.1)",
        )
    }


    /// Fork a workspace via the daemon (sole owner of PTYs + scratch copy).
    /// GUI never spawns local PTYs post-daemon-split.
    fn fork_workspace(
        &mut self,
        src: &str,
        name: Option<String>,
        actor: &str,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if !self.panes.iter().any(|p| p.workspace == src) {
            return None;
        }
        if let Err(e) = self.client.fork_workspace(src, name.clone()) {
            eprintln!("[seance] fork_workspace via daemon failed: {e:#}");
            return None;
        }
        let new_ws = name
            .as_ref()
            .map(|n| crate::state::slugify(n))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("{src}-fork"));
        events::log(
            actor,
            Some(&new_ws),
            None,
            "workspace_forked",
            format!("fork requested '{src}' -> '{new_ws}' (daemon)"),
        );
        cx.notify();
        Some(new_ws)
    }

    fn start_whisper(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Toggle off if already whispering into this pane.
        if self.whisper.as_ref().is_some_and(|(s, _)| s == slug) {
            self.whisper = None;
            cx.notify();
            return;
        }
        // Notes face and whisper both claim the body — unflip first.
        if self.flipped.as_ref().is_some_and(|(s, _)| s == slug) {
            self.flipped = None;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("type a steer… Enter sends into the agent · Esc cancels")
        });
        cx.subscribe_in(
            &input,
            window,
            |this: &mut SeanceApp, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let text = input.read(cx).value().to_string();
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        this.whisper = None;
                        cx.notify();
                        return;
                    }
                    if let Some((slug, _)) = this.whisper.take() {
                        this.inject_into_pane(
                            &slug,
                            &format!("[whisper from zack] {text}"),
                            "whisper",
                            format!("whispered: {text}"),
                            cx,
                        );
                    }
                    let _ = window;
                }
                InputEvent::Blur => {
                    // Keep the bar open on blur (user may click "arm"); only Esc /
                    // empty Enter / cancel button dismisses. Blur alone is noisy.
                }
                _ => {}
            },
        )
        .detach();
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.whisper = Some((slug.to_string(), input));
        self.active_slug = Some(slug.to_string());
        cx.notify();
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
            events::log(
                "human",
                Some(&pane.workspace),
                Some(slug),
                kind,
                detail,
            );
            if let Some(rt) = pane.remote_terminal() {
                rt.read(cx).inject(text.to_string(), true);
                self.touch(slug, "whispered", "you", cx);
            } else if let Some(terminal) = pane.terminal() {
                terminal.update(cx, |term, cx| {
                    term.scroll_to_bottom();
                    term.inject(text.to_string(), true, cx);
                });
                self.touch(slug, "whispered", "you", cx);
            }
        }
        cx.notify();
    }

    /// Type the agent profile command line into the *active* pane (shell) and
    /// submit — for when you're already in a shell and want the right flags
    /// without spawning a new pane.
    fn launch_agent_into_active(
        &mut self,
        profile: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slug) = self.active_slug.clone() else {
            window.push_notification(
                gpui_component::notification::Notification::warning(
                    "select a pane first",
                ),
                cx,
            );
            return;
        };
        let has_term = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .is_some_and(|p| p.remote_terminal().is_some() || p.terminal().is_some());
        if !has_term {
            window.push_notification(
                gpui_component::notification::Notification::warning(
                    "active pane isn't a terminal",
                ),
                cx,
            );
            return;
        }
        match crate::agents::resolve(profile) {
            Ok(p) => {
                let cmd = crate::agents::command_line(&p);
                self.inject_into_pane(
                    &slug,
                    &cmd,
                    "agent_launch",
                    format!("launched profile '{profile}': {cmd}"),
                    cx,
                );
                window.push_notification(
                    gpui_component::notification::Notification::success(format!(
                        "→ {profile}"
                    )),
                    cx,
                );
            }
            Err(e) => {
                window.push_notification(
                    gpui_component::notification::Notification::error(e),
                    cx,
                );
            }
        }
    }

    /// Quiet bar: inject claude / codex / grok profile commands into active shell.
    fn render_agent_launch_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let profiles = ["claude", "codex", "grok"];
        let has_active = self.active_slug.is_some();
        div()
            .id("agent-launch-bar")
            .flex_none()
            .w_full()
            .px_2()
            .pt_1()
            .flex()
            .items_center()
            .gap_1p5()
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(SeancePalette::text_faint())
                    .child("run in pane"),
            )
            .children(profiles.into_iter().map(|name| {
                let n = name.to_string();
                let n2 = n.clone();
                let enabled = has_active;
                div()
                    .id(SharedString::from(format!("launch-{n}")))
                    .flex_none()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .text_xs()
                    .border_1()
                    .border_color(if enabled {
                        SeancePalette::border()
                    } else {
                        SeancePalette::border().opacity(0.5)
                    })
                    .bg(SeancePalette::surface())
                    .text_color(if enabled {
                        match name {
                            "claude" => SeancePalette::flame(),
                            "codex" => SeancePalette::violet(),
                            "grok" => SeancePalette::success(),
                            _ => SeancePalette::text_dim(),
                        }
                    } else {
                        SeancePalette::text_faint()
                    })
                    .when(enabled, |d| {
                        d.hover(|s| s.bg(SeancePalette::border())).cursor_pointer()
                    })
                    .tooltip(tip_s(format!(
                        "type `{}` profile into active shell (with skip/always-approve flags)",
                        n
                    )))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.launch_agent_into_active(&n2, window, cx);
                    }))
                    .child(n)
            }))
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(SeancePalette::text_faint())
                    .child(if has_active {
                        "pastes + enter into focused pane"
                    } else {
                        "select a shell pane first"
                    }),
            )
    }

    fn cancel_whisper(&mut self, cx: &mut Context<Self>) {
        self.whisper = None;
        cx.notify();
    }

    /// Record a transient cross-pane touch ("⚡ driven by X") and schedule its
    /// fade — the visible-agency overlay the council converged on.
    fn touch(&mut self, slug: &str, verb: &str, actor: &str, cx: &mut Context<Self>) {
        if let Some(ws) = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| p.workspace.clone())
        {
            self.touch_workspace(&ws);
        }
        self.touches.insert(
            slug.to_string(),
            (verb.to_string(), actor.to_string(), std::time::Instant::now()),
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

    /// Minimize shelf: chips for shelved panes in the selected circle only.
    /// Hidden entirely when nothing is minimized.
    fn render_minimize_shelf(&self, window_active: bool, cx: &Context<Self>) -> impl IntoElement {
        let ws = self.selected_workspace.clone();
        let shelved: Vec<&Pane> = self
            .panes
            .iter()
            .filter(|p| {
                !p.tiled
                    && p.popped.is_none()
                    && ws.as_ref().is_none_or(|w| p.workspace == *w)
            })
            .collect();
        if shelved.is_empty() {
            return div().into_any_element();
        }
        let active = if window_active {
            self.active_slug.clone()
        } else {
            None
        };
        div()
            .id("minimize-shelf")
            .flex_none()
            .px_2()
            .py_1()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_1()
            .border_b_1()
            .border_color(SeancePalette::border())
            .bg(SeancePalette::bg_elevated())
            .children(shelved.into_iter().map(|pane| {
                let slug = pane.slug.clone();
                let name = pane.name.clone();
                let is_active = active.as_deref() == Some(slug.as_str());
                div()
                    .id(SharedString::from(format!("shelf-{slug}")))
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .text_xs()
                    .cursor_pointer()
                    .bg(if is_active {
                        selected_row_fill()
                    } else {
                        SeancePalette::surface()
                    })
                    .text_color(if is_active {
                        SeancePalette::flame()
                    } else {
                        SeancePalette::text_dim()
                    })
                    .hover(|s| s.bg(SeancePalette::surface().lighten(0.05)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // Click-to-show: re-tile shelved pane.
                        this.toggle_tiled(&slug, cx);
                        this.set_active(&slug, window, cx);
                    }))
                    .child(name)
                    .into_any_element()
            }))
            .into_any_element()
    }

    /// Unanswered agent questions for the selected workspace, as a toast strip.
    fn render_asks(&self, cx: &Context<Self>) -> Vec<gpui::AnyElement> {
        self.asks
            .iter()
            .filter(|a| a.answer.is_none())
            .filter(|a| {
                a.workspace.is_none()
                    || self.selected_workspace.is_none()
                    || a.workspace == self.selected_workspace
            })
            .map(|ask| {
                let id = ask.id.clone();
                let mut row = div()
                    .flex_none()
                    .mx_1()
                    .mt_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(SeancePalette::violet_dim())
                    .bg(SeancePalette::bg_elevated())
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .text_color(SeancePalette::violet())
                            .child("❓"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(SeancePalette::text())
                            .child(format!("{} asks: {}", ask.from, ask.question)),
                    );
                let choices: Vec<String> = if ask.choices.is_empty() {
                    vec!["ok".to_string(), "no".to_string()]
                } else {
                    ask.choices.clone()
                };
                for choice in choices {
                    let id2 = id.clone();
                    let label = choice.clone();
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("ask-{id2}-{label}")))
                            .flex_none()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_sm()
                            .text_color(SeancePalette::flame())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.answer_ask(&id2, label.clone(), cx);
                            }))
                            .child(choice),
                    );
                }
                row.into_any_element()
            })
            .collect()
    }

    fn render_activity(&self) -> gpui::AnyElement {
        let entries = events::read(0, self.selected_workspace.as_deref(), None, None, 60);
        div()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .children(entries.into_iter().rev().map(|e| {
                let actor_color = if e.actor == "human" {
                    SeancePalette::flame()
                } else if e.actor.starts_with("agent:") {
                    SeancePalette::violet()
                } else {
                    SeancePalette::text_faint()
                };
                div()
                    .flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .flex_none()
                            .text_color(SeancePalette::text_faint())
                            .child(events::fmt_time(e.ts)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(90.))
                            .overflow_hidden()
                            .text_color(actor_color)
                            .child(e.actor.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(SeancePalette::text_dim())
                            .child(e.detail.clone()),
                    )
            }))
            .into_any_element()
    }

    /// Host-bridge strip(s) above the summon footer. Empty when no host or poll failed.
    fn render_host_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        if self.host.widgets.is_empty() {
            return div().flex_none().into_any_element();
        }
        div()
            .flex_none()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(SeancePalette::border())
            .children(self.host.widgets.iter().map(|w| {
                let title = if w.title.is_empty() {
                    w.id.clone()
                } else {
                    w.title.clone()
                };
                let widget_id = w.id.clone();
                div()
                    .flex()
                    .flex_col()
                    .py_1p5()
                    .gap_0p5()
                    .child(
                        div()
                            .px_2()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(SeancePalette::text_faint())
                                    .child(format!("── {title} ──")),
                            )
                            .when_some(w.error.as_ref(), |d, err| {
                                d.child(
                                    div()
                                        .id(SharedString::from(format!("host-err-{}", widget_id)))
                                        .text_xs()
                                        .text_color(SeancePalette::danger())
                                        .tooltip(tip_s(err.clone()))
                                        .child("!"),
                                )
                            }),
                    )
                    .children(w.items.iter().map(|item| {
                        let wid = widget_id.clone();
                        let iid = item.id.clone();
                        let selected = item.selected;
                        let state = item.state.as_str();
                        let color = match state {
                            "busy" => SeancePalette::danger(),
                            "warm" => SeancePalette::flame(),
                            "auth" => SeancePalette::violet(),
                            _ if selected => SeancePalette::success(),
                            _ => SeancePalette::text_faint(),
                        };
                        let mark = if selected { "●" } else { "○" };
                        let label = item.label.clone();
                        let detail = item.detail.clone();
                        let detail2 = item.detail2.clone();
                        let tip_text = if selected {
                            format!("{label} · active · click to refresh")
                        } else {
                            format!("switch to {label}")
                        };
                        // Full-bleed selected row (same fill as workspaces).
                        div()
                            .id(SharedString::from(format!("host-{wid}-{iid}")))
                            .flex()
                            .items_start()
                            .gap_1p5()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(selected_row_fill()))
                            .hover(|s| {
                                if selected {
                                    s.bg(selected_row_fill().lighten(0.04))
                                } else {
                                    s.bg(SeancePalette::surface())
                                }
                            })
                            .tooltip(tip_s(tip_text))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.host_select(&wid, &iid, window, cx);
                            }))
                            .child(
                                div()
                                    .flex_none()
                                    .pt(px(1.))
                                    .text_xs()
                                    .text_color(color)
                                    .child(mark),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_xs()
                                            .font_weight(if selected {
                                                gpui::FontWeight::SEMIBOLD
                                            } else {
                                                gpui::FontWeight::NORMAL
                                            })
                                            .text_color(if selected {
                                                SeancePalette::text()
                                            } else {
                                                SeancePalette::text_dim()
                                            })
                                            .child(label),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_xs()
                                            .text_color(SeancePalette::text_faint())
                                            .child(detail),
                                    )
                                    .when(!detail2.is_empty(), |d| {
                                        d.child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_xs()
                                                .text_color(SeancePalette::text_faint())
                                                .child(detail2),
                                        )
                                    }),
                            )
                    }))
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn host_select(
        &mut self,
        widget_id: &str,
        item_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Already selected — soft re-poll only.
        if self
            .host
            .widgets
            .iter()
            .find(|w| w.id == widget_id)
            .and_then(|w| w.items.iter().find(|i| i.id == item_id))
            .is_some_and(|i| i.selected)
        {
            self.host.poll_all();
            cx.notify();
            return;
        }
        match self.host.select(widget_id, item_id) {
            Ok(raw) => {
                // Prefer host JSON message when present.
                let msg = serde_json::from_str::<serde_json::Value>(&raw)
                    .ok()
                    .and_then(|v| {
                        let email = v.get("email").and_then(|e| e.as_str());
                        let id = v.get("id").and_then(|e| e.as_str()).unwrap_or(item_id);
                        Some(match email {
                            Some(e) if !e.is_empty() && e != "unknown" => {
                                format!("claude → {id} ({e})")
                            }
                            _ => format!("claude → {id}"),
                        })
                    })
                    .unwrap_or_else(|| format!("claude → {item_id}"));
                window.push_notification(
                    gpui_component::notification::Notification::success(msg),
                    cx,
                );
            }
            Err(e) => {
                window.push_notification(
                    gpui_component::notification::Notification::error(format!("switch failed: {e}")),
                    cx,
                );
            }
        }
        cx.notify();
    }

    fn render_sidebar(&self, window_active: bool, cx: &Context<Self>) -> impl IntoElement {
        // Ordered groups, INCLUDING empty workspaces (they render with 0 panes).
        let ordered = self.workspaces();
        let by_workspace: Vec<(String, Vec<&Pane>)> = ordered
            .into_iter()
            .map(|ws| {
                let panes: Vec<&Pane> =
                    self.panes.iter().filter(|p| p.workspace == ws).collect();
                (ws, panes)
            })
            .collect();

        let _ = window_active; // focus chrome reserved for future empty-window dimming

        div()
            .id("sidebar")
            .flex_none()
            .w(px(232.))
            .h_full()
            .flex()
            .flex_col()
            .bg(SeancePalette::bg_elevated())
            .border_r_1()
            .border_color(SeancePalette::border())
            .child(
                // Brand header.
                div()
                    .flex_none()
                    .h(px(44.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(SeancePalette::border())
                    .child(
                        div()
                            .text_color(SeancePalette::flame())
                            .text_lg()
                            .child("✦"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(SeancePalette::text())
                            .text_sm()
                            .font_semibold()
                            .child("seance"),
                    )
                    .child(
                        div()
                            .id("new-workspace")
                            .flex_none()
                            .px_1p5()
                            .rounded_md()
                            .text_xs()
                            .text_color(SeancePalette::violet_dim())
                            .hover(|s| s.text_color(SeancePalette::violet()).bg(SeancePalette::surface()))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.create_workspace(window, cx);
                            }))
                            .tooltip(tip("new empty workspace (name it immediately)"))
                            .child("◈+"),
                    ),
            )
            .child({
                // Workspace list only — context menus live on *rows*, not the scroller.
                // Empty-area multi-window menu is a separate flex filler (avoids double menus).
                div()
                    .id("pane-list")
                    .flex_1()
                    .overflow_y_scroll()
                    // No horizontal pad — selected workspace fill is full-bleed.
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(by_workspace.into_iter().map(|(workspace, panes)| {
                        let selected = self.selected_workspace.as_deref() == Some(workspace.as_str());
                        let pane_n = panes.len();
                        let ws_for_click = workspace.clone();
                        let ws_for_group_drop = workspace.clone();
                        let ws_for_pane_drop = workspace.clone();
                        let ws_for_ws_drop = workspace.clone();
                        let ws_for_menu = workspace.clone();
                        let renaming_this_ws = matches!(
                            &self.renaming,
                            Some((RenameTarget::Workspace(w), _)) if *w == workspace
                        );
                        let rename_input = self.renaming.as_ref().map(|(_, i)| i.clone());
                        // Collapsed workspaces: header only (no pane rows).
                        let header: gpui::AnyElement = if renaming_this_ws {
                            div()
                                .px_2()
                                .py_1p5()
                                .children(rename_input.map(|i| Input::new(&i)))
                                .into_any_element()
                        } else {
                            div()
                                .id(SharedString::from(format!("ws-{workspace}")))
                                .px_2()
                                .py_1p5()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .cursor_pointer()
                                .when(selected, |d| d.bg(selected_row_fill()))
                                .hover(|s| {
                                    if selected {
                                        s.bg(selected_row_fill().lighten(0.04))
                                    } else {
                                        s.bg(SeancePalette::surface())
                                    }
                                })
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(|_this, _, window, cx| {
                                        sidebar_press_no_select(window, cx);
                                    }),
                                )
                                .on_drag(
                                    DraggedWorkspace {
                                        name: workspace.clone(),
                                    },
                                    |drag, _, window, cx| {
                                        kill_text_selection(window, cx);
                                        ui_debug(&format!("drag started: workspace '{}'", drag.name));
                                        let label = format!("◈ {}", drag.name);
                                        cx.new(|_| DragPill { label })
                                    },
                                )
                                .drag_over::<DraggedPane>(|style, _, _, _| {
                                    style.bg(SeancePalette::violet_dim())
                                })
                                .on_drop(cx.listener(move |this, drag: &DraggedPane, _, cx| {
                                    ui_debug(&format!(
                                        "drop pane '{}' on workspace header '{}'",
                                        drag.slug, ws_for_pane_drop
                                    ));
                                    this.reorder_pane(&drag.slug, &ws_for_pane_drop, None, cx);
                                }))
                                .drag_over::<DraggedWorkspace>(|style, _, _, _| {
                                    style.bg(SeancePalette::flame_dim())
                                })
                                .on_drop(cx.listener(move |this, drag: &DraggedWorkspace, _, cx| {
                                    ui_debug(&format!(
                                        "drop workspace '{}' before '{}'",
                                        drag.name, ws_for_ws_drop
                                    ));
                                    this.reorder_workspace(&drag.name, &ws_for_ws_drop, cx);
                                }))
                                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                    if event.click_count() == 2 {
                                        this.start_rename(
                                            RenameTarget::Workspace(ws_for_click.clone()),
                                            &ws_for_click.clone(),
                                            window,
                                            cx,
                                        );
                                    } else {
                                        this.select_workspace(&ws_for_click, window, cx);
                                    }
                                }))
                                .context_menu({
                                    let ws_m = ws_for_menu.clone();
                                    let peers: Vec<(String, String)> = self
                                        .windows
                                        .iter()
                                        .filter(|w| Some(w.id.as_str()) != self.window_id.as_deref())
                                        .map(|w| (w.id.clone(), w.label.clone()))
                                        .collect();
                                    move |menu, _, _| {
                                        let mut m = menu
                                            .menu(
                                                "touch (bump recency)",
                                                Box::new(ActTouchWorkspace(ws_m.clone())),
                                            )
                                            .menu(
                                                "rename workspace",
                                                Box::new(ActRenameWorkspace(ws_m.clone())),
                                            )
                                            .menu(
                                                "fork workspace ⑂",
                                                Box::new(ActForkWorkspace(ws_m.clone())),
                                            )
                                            .separator()
                                            .menu(
                                                "send to new window",
                                                Box::new(ActTransferWorkspaceNewWindow(
                                                    ws_m.clone(),
                                                )),
                                            );
                                        for (id, label) in &peers {
                                            m = m.menu(
                                                format!("send to {label}"),
                                                Box::new(ActTransferWorkspace {
                                                    workspace: ws_m.clone(),
                                                    to_window: id.clone(),
                                                }),
                                            );
                                        }
                                        m = m
                                            .menu(
                                                "collect all windows here",
                                                Box::new(ActCollectAllWindows),
                                            )
                                            .separator()
                                            .menu(
                                                "banish workspace (kill all panes)",
                                                Box::new(ActKillWorkspace(ws_m.clone())),
                                            );
                                        m
                                    }
                                })
                                .child(
                                    div()
                                        .flex_none()
                                        .text_sm()
                                        .text_color(if selected {
                                            SeancePalette::flame()
                                        } else {
                                            SeancePalette::text_faint()
                                        })
                                        .child(if selected { "◆" } else { "◈" }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_sm()
                                        .font_weight(if selected {
                                            gpui::FontWeight::SEMIBOLD
                                        } else {
                                            gpui::FontWeight::NORMAL
                                        })
                                        .text_color(if selected {
                                            SeancePalette::text()
                                        } else {
                                            SeancePalette::text_dim()
                                        })
                                        .child(workspace.clone()),
                                )
                                .children({
                                    // Live badge (working/needs/done) for inactive circles.
                                    let att = if selected {
                                        None
                                    } else {
                                        self.workspace_attention_cx(&workspace, cx)
                                    };
                                    att.map(|a| {
                                        div()
                                            .flex_none()
                                            .px_1()
                                            .rounded_sm()
                                            .text_xs()
                                            .text_color(a.color())
                                            .child(a.label())
                                    })
                                })
                                .child(
                                    // Hover × to banish (only when selected header shows count otherwise).
                                    div()
                                        .id(SharedString::from(format!("ws-banish-{workspace}")))
                                        .flex_none()
                                        .px_1()
                                        .rounded_sm()
                                        .text_xs()
                                        .text_color(SeancePalette::text_faint())
                                        .hover(|s| {
                                            s.text_color(SeancePalette::danger())
                                                .bg(SeancePalette::surface())
                                        })
                                        .cursor_pointer()
                                        .on_click({
                                            let ws = workspace.clone();
                                            cx.listener(move |this, _, _, cx| {
                                                this.kill_workspace(&ws, cx);
                                            })
                                        })
                                        .tooltip(tip("banish workspace (kill all panes)"))
                                        .child("×"),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(if selected {
                                            SeancePalette::text_dim()
                                        } else {
                                            SeancePalette::text_faint()
                                        })
                                        .child(format!("{pane_n}")),
                                )
                                .into_any_element()
                        };
                        div()
                            .id(SharedString::from(format!("wsgroup-{workspace}")))
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .mb_0p5()
                            .drag_over::<DraggedPane>(|style, _, _, _| {
                                style.bg(SeancePalette::surface())
                            })
                            .on_drop(cx.listener(move |this, drag: &DraggedPane, _, cx| {
                                ui_debug(&format!(
                                    "drop pane '{}' on workspace group '{}'",
                                    drag.slug, ws_for_group_drop
                                ));
                                this.reorder_pane(&drag.slug, &ws_for_group_drop, None, cx);
                            }))
                            .child(header)
                    }))
                    // Flex filler: only *blank* sidebar area gets pull/collect menu
                    // (workspace rows have their own menus — don't nest on the scroller).
                    .child({
                        let foreign = self.foreign_workspaces.clone();
                        div()
                            .id("sidebar-empty-hit")
                            .flex_1()
                            .min_h(px(48.))
                            .w_full()
                            .context_menu(move |menu, _, _| {
                                let mut m = menu.menu(
                                    "collect all windows here",
                                    Box::new(ActCollectAllWindows),
                                );
                                if !foreign.is_empty() {
                                    m = m.separator();
                                    for f in &foreign {
                                        m = m.menu(
                                            format!(
                                                "pull «{}» from {}",
                                                f.workspace, f.window_label
                                            ),
                                            Box::new(ActPullWorkspace(f.workspace.clone())),
                                        );
                                    }
                                }
                                m
                            })
                    })
            })
            .child(self.render_host_sidebar(cx))
            .child(
                // Footer: summon + help.
                div()
                    .flex_none()
                    .p_2()
                    .border_t_1()
                    .border_color(SeancePalette::border())
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("summon")
                            .flex_1()
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .text_sm()
                            .text_color(SeancePalette::flame())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.new_default_session(cx);
                            }))
                            .tooltip(tip(
                                "new shell pane in this workspace (ctrl+shift+n) — name it in the sidebar",
                            ))
                            .child("+ summon"),
                    )
                    .child(
                        div()
                            .id("activity")
                            .flex_none()
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .flex()
                            .items_center()
                            .text_sm()
                            .text_color(SeancePalette::text_dim())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.drawer = if matches!(this.drawer, Drawer::Activity) {
                                    Drawer::Closed
                                } else {
                                    Drawer::Activity
                                };
                                cx.notify();
                            }))
                            .tooltip(tip("activity feed — who did what, live"))
                            .child("≋"),
                    )
                    .child(
                        div()
                            .id("help")
                            .flex_none()
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .flex()
                            .items_center()
                            .text_sm()
                            .text_color(SeancePalette::violet())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_help_window(cx);
                            }))
                            .tooltip(tip("open the grimoire — full guide to seance"))
                            .child("?"),
                    ),
            )
    }

    /// Stage strip — only when something needs the human.
    /// Human-only shells stay clean (no second roster). Shows chips for
    /// needs-human / blocked / risky in the selected workspace.
    fn render_stage_strip(&self, window_active: bool, cx: &Context<Self>) -> impl IntoElement {
        let ws = self.selected_workspace.clone();
        let mut rows: Vec<(&Pane, Option<&PaneStatus>)> = self
            .panes
            .iter()
            .filter(|p| ws.as_ref().is_none_or(|w| p.workspace == *w))
            .map(|p| (p, self.statuses.get(&p.slug)))
            .filter(|(_, st)| {
                matches!(
                    st.map(|s| s.state.as_str()),
                    Some("needs-human" | "blocked" | "risky")
                )
            })
            .collect();
        rows.sort_by_key(|(p, st)| {
            let urgency = match st.map(|s| s.state.as_str()) {
                Some("needs-human") => 0,
                Some("blocked") | Some("risky") => 1,
                _ => 2,
            };
            (urgency, p.name.clone())
        });
        if rows.is_empty() {
            return div().flex_none().into_any_element();
        }
        let active = if window_active {
            self.active_slug.clone()
        } else {
            None
        };
        div()
            .id("stage-strip")
            .flex_none()
            .w_full()
            .px_1()
            .pt_1()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_1()
            .children(rows.into_iter().map(|(pane, st)| {
                let slug = pane.slug.clone();
                let is_active = active.as_deref() == Some(slug.as_str());
                let state = st.map(|s| s.state.as_str()).unwrap_or("-");
                let note = st.and_then(|s| s.note.as_deref()).unwrap_or("");
                let color = status_color(state);
                let label = if note.is_empty() {
                    format!("{} · {state}", pane.name)
                } else {
                    let n = if note.len() > 28 {
                        let mut end = 28;
                        while end > 0 && !note.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}…", &note[..end])
                    } else {
                        note.to_string()
                    };
                    format!("{} · {state} · {n}", pane.name)
                };
                div()
                    .id(SharedString::from(format!("stage-{slug}")))
                    .flex_none()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_active {
                        SeancePalette::flame()
                    } else {
                        SeancePalette::border()
                    })
                    .bg(SeancePalette::surface())
                    .text_xs()
                    .text_color(color)
                    .cursor_pointer()
                    .hover(|s| s.bg(SeancePalette::border()))
                    .tooltip(tip("click focus + pad drawer · double-click zoom"))
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if event.click_count() >= 2 {
                            this.toggle_zoom(&slug, cx);
                        } else {
                            this.focus_pane_slug(&slug, window, cx);
                            this.open_pad_drawer(&slug, cx);
                        }
                    }))
                    .child(label)
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn open_pad_drawer(&mut self, slug: &str, cx: &mut Context<Self>) {
        self.drawer = Drawer::Pad {
            slug: slug.to_string(),
        };
        cx.notify();
    }

    /// Read pad body + task sidecar from disk (daemon-owned paths).
    fn load_pad_bundle(slug: &str) -> (String, Option<String>, Option<serde_json::Value>) {
        let base = PathBuf::from(
            shellexpand::tilde("~/.local/share/seance/scratch").into_owned(),
        );
        let pad_path = base.join(format!("{slug}.md"));
        let pad = std::fs::read_to_string(&pad_path).unwrap_or_else(|_| String::new());
        let task_id = std::fs::read_to_string(base.join(format!("{slug}.taskid")))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let task_json = std::fs::read_to_string(base.join(format!("{slug}.task.json")))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        (pad, task_id, task_json)
    }

    fn phone_bind_path(slug: &str) -> PathBuf {
        PathBuf::from(shellexpand::tilde(&format!(
            "~/.local/share/seance/scratch/{slug}.telegram.json"
        )).into_owned())
    }

    fn phone_bind_json(slug: &str) -> Option<serde_json::Value> {
        let p = Self::phone_bind_path(slug);
        let bytes = std::fs::read_to_string(&p).ok()?;
        serde_json::from_str(&bytes).ok()
    }

    fn phone_linked(slug: &str) -> Option<String> {
        Self::phone_bind_json(slug).and_then(|v| {
            v.get("topic_id")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
    }

    fn phone_link(slug: &str) -> Option<String> {
        Self::phone_bind_json(slug).and_then(|v| {
            v.get("link")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
    }

    /// One-button telegram topic for a pane (shells `seance ctl phone`).
    fn phone_pane(&mut self, slug: &str, cx: &mut Context<Self>) {
        let slug = slug.to_string();
        // If already linked, open telegram if we have a link + pad drawer.
        if let Some(tid) = Self::phone_linked(&slug) {
            if let Some(link) = Self::phone_link(&slug) {
                let _ = std::process::Command::new("xdg-open")
                    .arg(&link)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
            crate::desktop_notify::notify(
                "seance · already phoned",
                &format!("{slug} → topic {tid}"),
            );
            self.open_pad_drawer(&slug, cx);
            return;
        }
        // Off UI thread — vita open_topic can take seconds.
        let slug_bg = slug.clone();
        cx.spawn(async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move {
                    std::process::Command::new("seance")
                        .args(["ctl", "phone", &slug_bg])
                        .output()
                })
                .await;
            let Some(this) = this.upgrade() else { return };
            this.update(cx, |app, cx| {
                match out {
                    Ok(o) if o.status.success() => {
                        let topic = Self::phone_linked(&slug)
                            .unwrap_or_else(|| String::from_utf8_lossy(&o.stdout).trim().to_string());
                        if let Some(link) = Self::phone_link(&slug) {
                            let _ = std::process::Command::new("xdg-open")
                                .arg(&link)
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn();
                        }
                        crate::desktop_notify::notify(
                            "seance · phone linked",
                            &format!("{slug} → {topic}"),
                        );
                        app.open_pad_drawer(&slug, cx);
                    }
                    Ok(o) => {
                        let err = String::from_utf8_lossy(&o.stderr);
                        crate::desktop_notify::notify(
                            "seance · phone failed",
                            if err.trim().is_empty() {
                                "ctl phone failed (is vita up?)"
                            } else {
                                err.trim()
                            },
                        );
                    }
                    Err(e) => {
                        crate::desktop_notify::notify("seance · phone failed", &format!("{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_pad_drawer(&self, slug: &str, cx: &Context<Self>) -> impl IntoElement {
        // Include tick so GPUI re-renders when pad_refresh_tick advances.
        let _tick = self.pad_refresh_tick;
        let _ = cx;
        let name = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| slug.to_string());
        let st = self.statuses.get(slug);
        let (pad, task_id, task_json) = Self::load_pad_bundle(slug);
        let phone = Self::phone_linked(slug);
        let status_line = match st {
            Some(s) => match &s.note {
                Some(n) if !n.is_empty() => format!("{} · {n}", s.state),
                _ => s.state.clone(),
            },
            None => "—".into(),
        };
        let task_body = task_json
            .as_ref()
            .and_then(|v| v.get("body").and_then(|b| b.as_str()))
            .unwrap_or("")
            .to_string();
        let task_status = task_json
            .as_ref()
            .and_then(|v| v.get("status").and_then(|b| b.as_str()))
            .unwrap_or("-");
        let task_id_s = task_id.unwrap_or_else(|| "—".into());

        let pad_display = if pad.trim().is_empty() {
            "(empty pad)".to_string()
        } else {
            // Show tail so latest finish is visible without scroll thrash.
            let lines: Vec<&str> = pad.lines().collect();
            if lines.len() > 80 {
                let mut s = String::from("…\n");
                s.push_str(&lines[lines.len() - 80..].join("\n"));
                s
            } else {
                pad
            }
        };
        let task_display = if task_body.trim().is_empty() {
            "(no active/recent inject body)".to_string()
        } else {
            if task_body.len() > 2500 {
                let mut end = 2500;
                while end > 0 && !task_body.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &task_body[..end])
            } else {
                task_body
            }
        };

        let slug_phone = slug.to_string();
        let slug_flip = slug.to_string();

        div()
            .id(SharedString::from(format!("pad-drawer-{slug}")))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(SeancePalette::flame())
                            .child(format!("{name}")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .child(format!("`{slug}`")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(status_color(
                                st.map(|s| s.state.as_str()).unwrap_or("-"),
                            ))
                            .child(status_line),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(SeancePalette::text_faint())
                    .child(match &phone {
                        Some(t) => format!("☎ topic {t}"),
                        None => "☎ not phoned".into(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("pad-phone-{slug}")))
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_xs()
                            .text_color(SeancePalette::violet())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.phone_pane(&slug_phone, cx);
                            }))
                            .child(if phone.is_some() {
                                "☎ re-show"
                            } else {
                                "☎ phone"
                            }),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("pad-flip-{slug}")))
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_xs()
                            .text_color(SeancePalette::flame())
                            .bg(SeancePalette::surface())
                            .hover(|s| s.bg(SeancePalette::border()))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.flip_notes_for(&slug_flip, window, cx);
                            }))
                            .child("✎ edit notes"),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(SeancePalette::violet())
                    .child(format!("task {task_id_s} · {task_status}")),
            )
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(SeancePalette::bg())
                    .border_1()
                    .border_color(SeancePalette::border())
                    .text_xs()
                    .text_color(SeancePalette::text_dim())
                    .child(task_display),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(SeancePalette::violet())
                    .child("pad (tail)"),
            )
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(SeancePalette::bg())
                    .border_1()
                    .border_color(SeancePalette::border())
                    .text_xs()
                    .text_color(SeancePalette::text())
                    .child(pad_display),
            )
            .into_any_element()
    }

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
        let _ = window;
        cx.notify();
    }

    fn toggle_zoom(&mut self, slug: &str, cx: &mut Context<Self>) {
        if self.zoomed_slug.as_deref() == Some(slug) {
            self.zoomed_slug = None;
        } else {
            self.zoomed_slug = Some(slug.to_string());
            self.active_slug = Some(slug.to_string());
        }
        cx.notify();
    }

    /// Full-bleed single pane with a persistent zoom bar so the mode is obvious.
    fn render_zoomed_pane(
        &self,
        pane: &Pane,
        window_active: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let name = pane.name.clone();
        let slug = pane.slug.clone();
        let slug_unzoom = slug.clone();
        let whisper = self
            .whisper
            .as_ref()
            .filter(|(ws, _)| *ws == pane.slug)
            .map(|(_, i)| i);
        let flipped = self
            .flipped
            .as_ref()
            .filter(|(ws, _)| *ws == pane.slug)
            .map(|(_, d)| d);
        // Zoom chrome stays (mode is sticky); focus ring only when window is active.
        let active = if window_active {
            Some(pane.slug.as_str())
        } else {
            None
        };
        div()
            .flex_1()
            .h_full()
            .w_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(SeancePalette::bg())
            .child(
                // Zoom mode strip — flame bar so you never forget you're zoomed.
                div()
                    .flex_none()
                    .h(px(28.))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(SeancePalette::flame().opacity(if window_active {
                        0.18
                    } else {
                        0.10
                    }))
                    .border_b_2()
                    .border_color(if window_active {
                        SeancePalette::flame()
                    } else {
                        SeancePalette::flame_dim()
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(if window_active {
                                        SeancePalette::flame()
                                    } else {
                                        SeancePalette::text_faint()
                                    })
                                    .child("⛶ zoomed"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(SeancePalette::text())
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(SeancePalette::text_faint())
                                    .child(format!("`{slug}`")),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(SeancePalette::text_faint())
                                    .child("esc · ctrl+shift+z"),
                            )
                            .child(
                                div()
                                    .id("zoom-unzoom-btn")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(SeancePalette::flame())
                                    .bg(SeancePalette::surface())
                                    .border_1()
                                    .border_color(SeancePalette::flame_dim())
                                    .hover(|s| s.bg(SeancePalette::flame().opacity(0.25)))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_zoom(&slug_unzoom, cx);
                                    }))
                                    .tooltip(tip("unzoom (esc)"))
                                    .child("unzoom"),
                            ),
                    ),
            )
            .child(
                // Must be a flex container — render_pane roots with flex_1 and
                // only expands when the parent is flex (pre-chrome zoom path
                // used .flex() on the tile wrapper; without it the terminal
                // body collapses to 0 height and looks blank).
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .p_1()
                    .flex()
                    .child(render_pane(
                        pane,
                        active,
                        self.statuses.get(&pane.slug),
                        self.owners.get(&pane.slug),
                        self.touches.get(&pane.slug),
                        None,
                        flipped,
                        true, // is_zoomed
                        cx,
                    )),
            )
            .into_any_element()
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

    fn palette_move(&mut self, delta: i32) {
        let n = match &self.palette {
            PaletteMode::Prompts { query, .. } => {
                crate::prompts::filter(&crate::prompts::load_all(), query).len()
            }
            PaletteMode::Jump { query, .. } => {
                let q = query.trim().to_ascii_lowercase();
                let pane_n = self
                    .panes
                    .iter()
                    .filter(|p| {
                        if q.is_empty() {
                            return true;
                        }
                        let hay = format!("{} {} {} {}", p.name, p.slug, p.command, p.workspace)
                            .to_ascii_lowercase();
                        q.split_whitespace().all(|t| hay.contains(t))
                    })
                    .count();
                pane_n + self.workspaces().len()
            }
            PaletteMode::Closed => 0,
        };
        match &mut self.palette {
            PaletteMode::Prompts { selected, .. } | PaletteMode::Jump { selected, .. } => {
                if n == 0 {
                    *selected = 0;
                    return;
                }
                let cur = *selected as i32;
                *selected = ((cur + delta).rem_euclid(n as i32)) as usize;
            }
            PaletteMode::Closed => {}
        }
    }

    /// Query daemon command log for last failed command; flash as a status note
    /// and open activity drawer so the human can see context.
    fn show_last_failed(&mut self, slug: &str, cx: &mut Context<Self>) {
        let slug = slug.to_string();
        let out = std::process::Command::new("seance")
            .args(["ctl", "last-command", &slug, "--failed", "--json"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout);
                let cmd = serde_json::from_str::<serde_json::Value>(&s)
                    .ok()
                    .and_then(|v| {
                        v.pointer("/data/command")
                            .or_else(|| v.get("command"))
                            .and_then(|c| c.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| s.trim().to_string());
                let exit = serde_json::from_str::<serde_json::Value>(&s)
                    .ok()
                    .and_then(|v| {
                        v.pointer("/data/exit")
                            .or_else(|| v.get("exit"))
                            .and_then(|e| e.as_i64())
                    });
                let note = match exit {
                    Some(e) => format!("last failed (exit {e}): {cmd}"),
                    None => format!("last failed: {cmd}"),
                };
                self.statuses.insert(
                    slug.clone(),
                    PaneStatus {
                        state: "needs-human".into(),
                        note: Some(note.clone()),
                    },
                );
                crate::desktop_notify::notify("seance · last failed", &note);
                self.drawer = Drawer::Activity;
                cx.notify();
            }
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                crate::desktop_notify::notify(
                    "seance · last failed",
                    if err.trim().is_empty() {
                        "no failed command on this pane"
                    } else {
                        err.trim()
                    },
                );
            }
            Err(e) => {
                crate::desktop_notify::notify("seance · last failed", &format!("ctl error: {e}"));
            }
        }
    }

    fn activate_palette_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.palette {
            PaletteMode::Closed => {}
            PaletteMode::Prompts { query, selected } => {
                let hits = crate::prompts::filter(&crate::prompts::load_all(), query);
                if let Some(p) = hits.get(*selected) {
                    let body = p.body.clone();
                    self.inject_prompt_into_active(&body, cx);
                }
            }
            PaletteMode::Jump { query, selected } => {
                let q = query.trim().to_ascii_lowercase();
                let mut items: Vec<String> = self
                    .panes
                    .iter()
                    .filter(|p| {
                        if q.is_empty() {
                            return true;
                        }
                        let hay = format!("{} {} {} {}", p.name, p.slug, p.command, p.workspace)
                            .to_ascii_lowercase();
                        q.split_whitespace().all(|t| hay.contains(t))
                    })
                    .map(|p| p.slug.clone())
                    .collect();
                for ws in self.workspaces() {
                    if q.is_empty() || ws.to_ascii_lowercase().contains(&q) {
                        items.push(format!("ws:{ws}"));
                    }
                }
                if let Some(id) = items.get(*selected).cloned() {
                    if let Some(ws) = id.strip_prefix("ws:") {
                        self.select_workspace(ws, window, cx);
                        self.close_palette(window, cx);
                    } else {
                        self.focus_pane_slug(&id, window, cx);
                        self.palette = PaletteMode::Closed;
                        cx.notify();
                    }
                }
            }
        }
    }

    fn render_palette(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let (title, query, selected, items): (String, String, usize, Vec<(String, String)>) =
            match &self.palette {
                PaletteMode::Closed => return None,
                PaletteMode::Prompts { query, selected } => {
                    let all = crate::prompts::load_all();
                    let hits = crate::prompts::filter(&all, query);
                    let items: Vec<_> = hits
                        .into_iter()
                        .map(|p| (p.id, format!("{} — {}", p.title, p.body.chars().take(60).collect::<String>())))
                        .collect();
                    ("precanned prompts · ctrl+shift+k".into(), query.clone(), *selected, items)
                }
                PaletteMode::Jump { query, selected } => {
                    let q = query.trim().to_ascii_lowercase();
                    let mut items: Vec<(String, String)> = self
                        .panes
                        .iter()
                        .filter(|p| {
                            if q.is_empty() {
                                return true;
                            }
                            let hay = format!("{} {} {} {}", p.name, p.slug, p.command, p.workspace)
                                .to_ascii_lowercase();
                            q.split_whitespace().all(|t| hay.contains(t))
                        })
                        .map(|p| {
                            let st = self
                                .statuses
                                .get(&p.slug)
                                .map(|s| s.state.as_str())
                                .unwrap_or("-");
                            (
                                p.slug.clone(),
                                format!("{} · {st} · {}", p.name, p.workspace),
                            )
                        })
                        .collect();
                    // Also offer workspaces as jump targets with ws: prefix
                    for ws in self.workspaces() {
                        if q.is_empty()
                            || ws.to_ascii_lowercase().contains(&q)
                        {
                            items.push((format!("ws:{ws}"), format!("workspace · {ws}")));
                        }
                    }
                    ("jump · ctrl+shift+j".into(), query.clone(), *selected, items)
                }
            };
        let n = items.len();
        let sel = if n == 0 { 0 } else { selected.min(n - 1) };
        Some(
            div()
                .id("palette-overlay")
                .absolute()
                .top(px(48.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .w(px(520.))
                        .max_h(px(360.))
                        .rounded_lg()
                        .border_1()
                        .border_color(SeancePalette::flame_dim())
                        .bg(SeancePalette::bg_elevated())
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(SeancePalette::border())
                                .text_xs()
                                .text_color(SeancePalette::text_faint())
                                .child(title),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(SeancePalette::border())
                                .text_sm()
                                .text_color(SeancePalette::flame())
                                .child(format!("› {query}█")),
                        )
                        .child(
                            div()
                                .id("palette-list")
                                .flex_1()
                                .overflow_y_scroll()
                                .py_1()
                                .children(items.into_iter().enumerate().map(|(i, (id, label))| {
                                    let active = i == sel;
                                    div()
                                        .id(SharedString::from(format!("pal-{i}-{id}")))
                                        .px_3()
                                        .py_1()
                                        .text_sm()
                                        .bg(if active {
                                            SeancePalette::surface()
                                        } else {
                                            gpui::transparent_black()
                                        })
                                        .text_color(if active {
                                            SeancePalette::flame()
                                        } else {
                                            SeancePalette::text()
                                        })
                                        .child(label)
                                        .into_any_element()
                                })),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(SeancePalette::text_faint())
                                .child("enter select · esc close · type to filter"),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_tiles(&self, window_active: bool, cx: &Context<Self>) -> impl IntoElement {
        // The tiling region shows only the SELECTED workspace's tiled panes.
        let mut tiled: Vec<&Pane> = self
            .panes
            .iter()
            .filter(|s| {
                s.tiled
                    && s.popped.is_none()
                    && self
                        .selected_workspace
                        .as_deref()
                        .is_none_or(|ws| s.workspace == ws)
            })
            .collect();
        // Focus-zoom: single pane fills the region with unmistakable chrome.
        if let Some(z) = self.zoomed_slug.as_deref() {
            if let Some(p) = tiled
                .iter()
                .find(|p| p.slug == z)
                .copied()
                .or_else(|| self.panes.iter().find(|p| p.slug == z))
            {
                return self.render_zoomed_pane(p, window_active, cx);
            }
        }
        let n = tiled.len();
        // Focus ring only while the OS window is active.
        let active = if window_active {
            self.active_slug.clone()
        } else {
            None
        };

        if n == 0 {
            let ws = self
                .selected_workspace
                .clone()
                .unwrap_or_else(|| "this workspace".into());
            return div()
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_color(SeancePalette::flame_dim())
                                .text_2xl()
                                .child("✦"),
                        )
                        .child(
                            div()
                                .text_color(SeancePalette::text_faint())
                                .text_sm()
                                .child(format!("{ws} is empty — summon a spirit (ctrl+shift+n)")),
                        ),
                )
                .into_any_element();
        }

        // 2-pane resizable split (horizontal sash).
        if n == 2 && self.zoomed_slug.is_none() {
            let left = tiled[0];
            let right = tiled[1];
            let ratio = self.split_ratio.clamp(0.2, 0.8);
            let left_pct = (ratio * 100.0) as u32;
            let right_pct = 100 - left_pct;
            let flipped_l = self
                .flipped
                .as_ref()
                .filter(|(ws, _)| *ws == left.slug)
                .map(|(_, d)| d);
            let flipped_r = self
                .flipped
                .as_ref()
                .filter(|(ws, _)| *ws == right.slug)
                .map(|(_, d)| d);
            return div()
                .flex_1()
                .h_full()
                .w_full()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_row()
                .p_1()
                .gap_0()
                .child(
                    div()
                        .h_full()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .flex()
                        .w(relative(left_pct as f32 / 100.0))
                        .child(render_pane(
                            left,
                            active.as_deref(),
                            self.statuses.get(&left.slug),
                            self.owners.get(&left.slug),
                            self.touches.get(&left.slug),
                            None,
                            flipped_l,
                            false,
                            cx,
                        )),
                )
                .child(
                    div()
                        .id("sash")
                        .flex_none()
                        .w(px(5.))
                        .h_full()
                        .cursor_col_resize()
                        .bg(SeancePalette::border())
                        .hover(|s| s.bg(SeancePalette::flame_dim()))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, ev: &gpui::MouseDownEvent, _, cx| {
                                this.sash_drag = Some(SashDrag::TwoPane {
                                    start_x: ev.position.x.into(),
                                });
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .h_full()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .flex()
                        .w(relative(right_pct as f32 / 100.0))
                        .child(render_pane(
                            right,
                            active.as_deref(),
                            self.statuses.get(&right.slug),
                            self.owners.get(&right.slug),
                            self.touches.get(&right.slug),
                            None,
                            flipped_r,
                            false,
                            cx,
                        )),
                )
                .into_any_element();
        }

        // Weighted auto-grid with inter-pane + inter-row sashes (n≠2, or zoomed).
        let cols = (n as f32).sqrt().ceil() as usize;
        let rows = n.div_ceil(cols);

        // Pre-slice into rows so we can hang vertical sashes between them.
        let mut row_lists: Vec<Vec<&Pane>> = Vec::new();
        {
            let mut it = tiled.into_iter();
            for _ in 0..rows {
                let mut row_panes: Vec<&Pane> = Vec::new();
                for _ in 0..cols {
                    if let Some(pane) = it.next() {
                        row_panes.push(pane);
                    }
                }
                if !row_panes.is_empty() {
                    row_lists.push(row_panes);
                }
            }
        }

        let mut grid = div()
            .flex_1()
            .h_full()
            .w_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap_0()
            .p_1();
        for (ri, row_panes) in row_lists.iter().enumerate() {
            let row_key = format!(
                "row-{}",
                row_panes.first().map(|p| p.slug.as_str()).unwrap_or("x")
            );
            let row_w = self
                .row_weights
                .get(&row_key)
                .copied()
                .unwrap_or(1.0)
                .max(0.15);
            let mut row = div()
                .min_h_0()
                .min_w_0()
                .w_full()
                .overflow_hidden()
                .flex()
                .flex_row()
                .gap_0()
                .flex_grow(row_w);
            for (i, pane) in row_panes.iter().enumerate() {
                let w = self
                    .pane_weights
                    .get(&pane.slug)
                    .copied()
                    .unwrap_or(1.0)
                    .max(0.15);
                let flipped = self
                    .flipped
                    .as_ref()
                    .filter(|(ws, _)| *ws == pane.slug)
                    .map(|(_, d)| d);
                row = row.child(
                    div()
                        .h_full()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .flex()
                        .flex_grow(w)
                        .child(render_pane(
                            pane,
                            active.as_deref(),
                            self.statuses.get(&pane.slug),
                            self.owners.get(&pane.slug),
                            self.touches.get(&pane.slug),
                            None, // whisper retired
                            flipped,
                            false,
                            cx,
                        )),
                );
                if i + 1 < row_panes.len() {
                    let left = pane.slug.clone();
                    let right = row_panes[i + 1].slug.clone();
                    let left_w = self.pane_weights.get(&left).copied().unwrap_or(1.0);
                    let right_w = self.pane_weights.get(&right).copied().unwrap_or(1.0);
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("sash-{left}-{right}")))
                            .flex_none()
                            .w(px(5.))
                            .h_full()
                            .cursor_col_resize()
                            .bg(SeancePalette::border())
                            .hover(|s| s.bg(SeancePalette::flame_dim()))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                    this.sash_drag = Some(SashDrag::Pair {
                                        left: left.clone(),
                                        right: right.clone(),
                                        start_x: ev.position.x.into(),
                                        left_w,
                                        right_w,
                                    });
                                    cx.notify();
                                }),
                            ),
                    );
                }
            }
            grid = grid.child(row);
            // Vertical sash between rows.
            if ri + 1 < row_lists.len() {
                let above_key = row_key.clone();
                let below_key = format!(
                    "row-{}",
                    row_lists[ri + 1]
                        .first()
                        .map(|p| p.slug.as_str())
                        .unwrap_or("x")
                );
                let above_w = self.row_weights.get(&above_key).copied().unwrap_or(1.0);
                let below_w = self.row_weights.get(&below_key).copied().unwrap_or(1.0);
                grid = grid.child(
                    div()
                        .id(SharedString::from(format!("rsash-{above_key}-{below_key}")))
                        .flex_none()
                        .h(px(5.))
                        .w_full()
                        .cursor_row_resize()
                        .bg(SeancePalette::border())
                        .hover(|s| s.bg(SeancePalette::flame_dim()))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                this.sash_drag = Some(SashDrag::RowPair {
                                    above_key: above_key.clone(),
                                    below_key: below_key.clone(),
                                    start_y: ev.position.y.into(),
                                    above_w,
                                    below_w,
                                });
                                cx.notify();
                            }),
                        ),
                );
            }
        }
        grid.into_any_element()
    }

    fn weight_of(&self, slug: &str) -> f32 {
        self.pane_weights.get(slug).copied().unwrap_or(1.0).max(0.15)
    }
}

fn render_session_row(
    pane: &Pane,
    active: Option<&str>,
    all_workspaces: &[String],
    renaming: Option<&(RenameTarget, Entity<InputState>)>,
    status: Option<&PaneStatus>,
    cx: &Context<SeanceApp>,
) -> gpui::AnyElement {
    // Inline rename swap: this row becomes an input while being renamed.
    if let Some((RenameTarget::Pane(rslug), input)) = renaming {
        if *rslug == pane.slug {
            return div()
                .px_2()
                .py_1()
                .child(Input::new(input))
                .into_any_element();
        }
    }
    let slug = pane.slug.clone();
    let is_active = active == Some(pane.slug.as_str());
    let running = pane.is_running(cx);
    let dot_color = if !running {
        SeancePalette::status_exited()
    } else if let Some(st) = status {
        status_color(&st.state)
    } else {
        SeancePalette::status_running()
    };

    let slug_for_click = slug.clone();
    let slug_for_tile = slug.clone();

    let menu_slug = slug.clone();
    let menu_tiled = pane.tiled;
    let is_popped = pane.popped.is_some();
    let menu_workspaces: Vec<String> = all_workspaces
        .iter()
        .filter(|w| **w != pane.workspace)
        .cloned()
        .collect();

    div()
        .id(SharedString::from(format!("row-{slug}")))
        .group("row")
        .px_2()
        .py_1()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .when(is_active, |d| d.bg(selected_row_fill()))
        .hover(|s| {
            if is_active {
                s.bg(selected_row_fill().lighten(0.04))
            } else {
                s.bg(SeancePalette::surface())
            }
        })
        // Own this press so markdown never begins a window text selection
        // while the button is down (threshold / reorder / cross-tile drag).
        // Suppress once on down + clear once on drag start — never per-move.
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|_this, _, window, cx| {
                sidebar_press_no_select(window, cx);
            }),
        )
        .on_drag(
            DraggedPane {
                slug: slug.clone(),
                name: pane.name.clone(),
            },
            |drag, _offset, window, cx| {
                kill_text_selection(window, cx);
                ui_debug(&format!("drag started: pane '{}'", drag.slug));
                cx.new(|_| DragPill {
                    label: format!("{} ⇢", drag.name),
                })
            },
        )
        .drag_over::<DraggedPane>(|style, _, _, _| {
            style.border_color(SeancePalette::flame_dim())
        })
        .on_drop(cx.listener({
            let target_slug = slug.clone();
            let target_ws = pane.workspace.clone();
            move |this, drag: &DraggedPane, _, cx| {
                ui_debug(&format!(
                    "drop pane '{}' on row '{}'",
                    drag.slug, target_slug
                ));
                this.reorder_pane(&drag.slug, &target_ws, Some(&target_slug), cx);
                cx.stop_propagation();
            }
        }))
        .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
            if event.click_count() == 2 {
                let current = this
                    .panes
                    .iter()
                    .find(|p| p.slug == slug_for_click)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                this.start_rename(
                    RenameTarget::Pane(slug_for_click.clone()),
                    &current,
                    window,
                    cx,
                );
                return;
            }
            let popped = this
                .panes
                .iter()
                .find(|p| p.slug == slug_for_click)
                .and_then(|p| p.popped);
            if let Some(handle) = popped {
                this.active_slug = Some(slug_for_click.clone());
                let _ = handle.update(cx, |_, window, _| window.activate_window());
                cx.notify();
            } else {
                // Click-to-show: a shelved pane tiles itself on click.
                if let Some(p) = this.panes.iter_mut().find(|p| p.slug == slug_for_click) {
                    if !p.tiled {
                        p.tiled = true;
                    }
                }
                this.set_active(&slug_for_click, window, cx);
            }
        }))
        .child(
            div()
                .flex_none()
                .size(px(7.))
                .rounded_full()
                .bg(dot_color),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(if is_active {
                    SeancePalette::text()
                } else {
                    SeancePalette::text_dim()
                })
                // Single line — long names ellipsize, never wrap under the badge.
                .truncate()
                .child(if is_popped {
                    format!("{} ⇱", pane.name)
                } else {
                    pane.name.clone()
                }),
        )
        // Stage/roster lite: show badge text so humans see status without
        // flipping every pane (0.9.5).
        .when_some(status.map(|s| s.state.clone()), |d, st| {
            d.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(dot_color)
                    .child(st),
            )
        })
        .child(
            div()
                .id(SharedString::from(format!("tile-{slug}")))
                .flex_none()
                .text_xs()
                .text_color(if pane.tiled {
                    SeancePalette::flame()
                } else {
                    SeancePalette::text_faint()
                })
                .cursor_pointer()
                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                    let _ = event;
                    this.toggle_tiled(&slug_for_tile, cx);
                    cx.stop_propagation();
                }))
                .tooltip(tip("tile / shelve this pane"))
                .child(if pane.tiled { "▣" } else { "□" }),
        )
        // Banish is right-click menu or ctrl+shift+w only — no hover ✕.
        .context_menu(move |menu, _window, _cx| {
            let mut menu = menu
                .menu(
                    if menu_tiled { "shelve pane" } else { "tile pane" },
                    Box::new(ActToggleTiled(menu_slug.clone())),
                )
                .menu("flip notes ✎", Box::new(ActOpenNotes(menu_slug.clone())))
                .menu("rename", Box::new(ActRenamePane(menu_slug.clone())))
                .menu(
                    if is_popped { "return to circle ⇲" } else { "pop out ⇱" },
                    Box::new(ActTogglePopout(menu_slug.clone())),
                )
                .separator();
            for ws in &menu_workspaces {
                menu = menu.menu(
                    format!("move → {ws}"),
                    Box::new(ActMoveToWorkspace {
                        slug: menu_slug.clone(),
                        workspace: ws.clone(),
                    }),
                );
            }
            menu = menu.menu(
                "move → new workspace",
                Box::new(ActMoveToNewWorkspace(menu_slug.clone())),
            );
            menu.separator()
                .menu("banish (kill)", Box::new(ActKillSession(menu_slug.clone())))
        })
        .into_any_element()
}

fn render_pane(
    pane: &Pane,
    active: Option<&str>,
    status: Option<&PaneStatus>,
    owner: Option<&OwnerChrome>,
    touch: Option<&(String, String, std::time::Instant)>,
    whisper: Option<&Entity<InputState>>,
    flipped: Option<&Entity<ScratchpadDrawer>>,
    is_zoomed: bool,
    cx: &Context<SeanceApp>,
) -> impl IntoElement {
    let is_active = active == Some(pane.slug.as_str());
    let is_flipped = flipped.is_some();
    let _ = whisper; // whisper UI retired — keep arg for call-site stability
    let slug = pane.slug.clone();
    let running = pane.is_running(cx);
    let title = pane.title(cx).unwrap_or_else(|| pane.command.clone());
    // Local or daemon-backed terminal panes both get arm/phone chrome.
    let has_terminal = pane.terminal().is_some() || pane.remote_terminal().is_some();
    let exited = owner.map(|o| o.exited).unwrap_or(false);
    let owner_label = owner.map(|o| {
        if o.exited {
            match o.exit_code {
                Some(c) => format!("☠ exit {c}"),
                None => "☠ exited".into(),
            }
        } else if o.owner == "human" {
            "⌨ you".into()
        } else if o.owner == "none" {
            "· free".into()
        } else if o.owner.starts_with("agent:") || o.owner == "cli" {
            format!("⚡ {}", o.owner.trim_start_matches("agent:"))
        } else {
            o.owner.clone()
        }
    });
    // Frame border prioritizes focus, not ownership — ownership stays in the
    // header chip (⌨/⚡). Inactive panes share one quiet border so active is
    // obvious at a glance. Zoom mode uses a loud flame ring only while the
    // OS window is focused (`is_active` is already gated on window_active).
    let frame_border = if exited {
        SeancePalette::danger()
    } else if is_active {
        if is_flipped {
            SeancePalette::violet()
        } else {
            SeancePalette::flame()
        }
    } else if is_zoomed {
        // Zoomed but window unfocused — keep a quiet ring, not "has focus".
        SeancePalette::border()
    } else {
        SeancePalette::border()
    };

    // Body: notes face if flipped, otherwise the terminal/file content.
    // Soft fade when the notes face appears (cheap stand-in for a card flip).
    let body: gpui::AnyElement = if let Some(notes) = flipped {
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(SeancePalette::bg_elevated())
            .child(notes.clone())
            .with_animation(
                SharedString::from(format!("flip-in-{slug}")),
                Animation::new(Duration::from_millis(220)).with_easing(ease_in_out),
                |this, delta| this.opacity(0.35 + 0.65 * delta),
            )
            .into_any_element()
    } else {
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(pane.content_element())
            .into_any_element()
    };

    div()
        .id(SharedString::from(format!("pane-{slug}")))
        .flex_1()
        .min_h_0()
        // Allow shrinking below content min-size (default flex min is
        // min-content — terminal cols / long markdown lines). Soft floor
        // keeps a sliver of chrome visible without pinning panes wide.
        .min_w(px(48.))
        .w_full()
        .overflow_hidden()
        .flex()
        .flex_col()
        .rounded_md()
        // Always 2px border so focus only recolors — never reflows terminal
        // cols (border_1 → border_2 used to steal a cell of width).
        .border_2()
        .border_color(frame_border)
        .bg(SeancePalette::bg())
        .opacity(if exited {
            0.72
        } else if is_active {
            1.0
        } else {
            0.88
        })
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener({
                let slug = slug.clone();
                move |this, _, window, cx| {
                    this.set_active(&slug, window, cx);
                }
            }),
        )
        .child(
            // Pane title strip.
            div()
                .flex_none()
                .h(px(26.))
                .px_2()
                .flex()
                .items_center()
                .gap_1p5()
                .min_w_0()
                .overflow_hidden()
                .bg(if is_active {
                    SeancePalette::surface()
                } else if is_zoomed {
                    SeancePalette::bg_elevated()
                } else {
                    SeancePalette::bg_elevated()
                })
                .when(is_zoomed, |d| {
                    d.child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(if is_active {
                                SeancePalette::flame()
                            } else {
                                SeancePalette::text_faint()
                            })
                            .child("⛶"),
                    )
                })
                .children(owner_label.map(|lab| {
                    div()
                        .flex_none()
                        .text_xs()
                        .whitespace_nowrap()
                        .text_color(if exited {
                            SeancePalette::danger()
                        } else if !is_active {
                            SeancePalette::text_faint()
                        } else if lab.starts_with('⌨') {
                            SeancePalette::flame()
                        } else if lab.starts_with('⚡') {
                            SeancePalette::violet()
                        } else {
                            SeancePalette::text_faint()
                        })
                        .child(lab)
                }))
                .child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(if running {
                            if is_active {
                                SeancePalette::flame()
                            } else {
                                SeancePalette::text_faint()
                            }
                        } else {
                            SeancePalette::status_exited()
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(if is_active && is_flipped {
                            SeancePalette::violet()
                        } else if is_active {
                            SeancePalette::text()
                        } else {
                            SeancePalette::text_faint()
                        })
                        .truncate()
                        .child(if is_flipped {
                            format!("{} — notes (back)", pane.name)
                        } else {
                            format!("{} — {}", pane.name, title)
                        }),
                )
                .children(status.map(|st| {
                    div()
                        .flex_none()
                        .px_1p5()
                        .rounded_md()
                        .text_xs()
                        .text_color(status_color(&st.state))
                        .bg(SeancePalette::bg())
                        .child(st.state.clone())
                }))
                .children(touch.map(|(verb, actor, _)| {
                    div()
                        .flex_none()
                        .px_1p5()
                        .rounded_md()
                        .text_xs()
                        .text_color(SeancePalette::violet())
                        .bg(SeancePalette::bg())
                        .child(format!("{verb} by {actor}"))
                }))
                // Arm: one-click seance orientation (terminals only).
                .when(has_terminal && !is_flipped, |d| {
                    d.child(
                        div()
                            .id(SharedString::from(format!("arm-strip-{slug}")))
                            .flex_none()
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .hover(|s| s.text_color(SeancePalette::flame()))
                            .cursor_pointer()
                            .on_click(cx.listener({
                                let slug = slug.clone();
                                move |this, _, _, cx| {
                                    this.arm_pane(&slug, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .tooltip(tip(
                                "arm — inject seance orientation so the agent uses ctl / workspace",
                            ))
                            .child("⚡"),
                    )
                })
                // Phone: one-button telegram topic (vita seam).
                .when(has_terminal, |d| {
                    let linked = SeanceApp::phone_linked(&slug).is_some();
                    d.child(
                        div()
                            .id(SharedString::from(format!("phone-{slug}")))
                            .flex_none()
                            .text_xs()
                            .text_color(if linked {
                                SeancePalette::violet()
                            } else {
                                SeancePalette::text_faint()
                            })
                            .hover(|s| s.text_color(SeancePalette::violet()))
                            .cursor_pointer()
                            .on_click(cx.listener({
                                let slug = slug.clone();
                                move |this, _, _, cx| {
                                    this.phone_pane(&slug, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .tooltip(tip(
                                "phone — open a telegram topic seeded with workspace roster + seance ctl how-to",
                            ))
                            .child("☎"),
                    )
                })
                // Pad drawer (quick inspect without flip).
                .child(
                    div()
                        .id(SharedString::from(format!("pad-chip-{slug}")))
                        .flex_none()
                        .text_xs()
                        .text_color(SeancePalette::text_faint())
                        .hover(|s| s.text_color(SeancePalette::flame()))
                        .cursor_pointer()
                        .on_click(cx.listener({
                            let slug = slug.clone();
                            move |this, _, _, cx| {
                                this.open_pad_drawer(&slug, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .tooltip(tip("pad drawer — task + scratchpad tail"))
                        .child("▤"),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("popout-{slug}")))
                        .flex_none()
                        .text_xs()
                        .text_color(SeancePalette::text_faint())
                        .hover(|s| s.text_color(SeancePalette::flame()))
                        .cursor_pointer()
                        .on_click(cx.listener({
                            let slug = slug.clone();
                            move |this, _, _, cx| {
                                this.pop_out(&slug, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .tooltip(tip("pop out to its own window (ctrl+shift+p)"))
                        .child("⇱"),
                )
                // Notes flip — prominent when flipped (violet "back" affordance).
                .child(
                    div()
                        .id(SharedString::from(format!("notes-{slug}")))
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .text_xs()
                        .text_color(if is_flipped {
                            SeancePalette::bg()
                        } else {
                            SeancePalette::text_faint()
                        })
                        .when(is_flipped, |d| d.bg(SeancePalette::violet()))
                        .hover(|s| {
                            if is_flipped {
                                s.bg(SeancePalette::violet_dim())
                            } else {
                                s.text_color(SeancePalette::flame())
                            }
                        })
                        .cursor_pointer()
                        // Stop mouse-down so a drag that starts on the chip
                        // doesn't become a text selection on the face content
                        // when the flip reveals markdown underneath.
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|_this, _, window, cx| {
                                window.end_text_selection(cx);
                                window.clear_text_selection(cx);
                                cx.stop_propagation();
                            }),
                        )
                        .on_click(cx.listener({
                            let slug = slug.clone();
                            move |this, _, window, cx| {
                                this.flip_notes_for(&slug, window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .tooltip(tip(if is_flipped {
                            "flip back to the terminal (ctrl+shift+s)"
                        } else {
                            "flip pane over — notes on the back (ctrl+shift+s)"
                        }))
                        .child(if is_flipped { "↻ face" } else { "✎ notes" }),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("shelve-{slug}")))
                        .flex_none()
                        .text_xs()
                        .text_color(SeancePalette::text_faint())
                        .hover(|s| s.text_color(SeancePalette::flame()))
                        .cursor_pointer()
                        .on_click(cx.listener({
                            let slug = slug.clone();
                            move |this, _, _, cx| {
                                this.toggle_tiled(&slug, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .tooltip(tip("shelve this pane (back via sidebar click)"))
                        .child("▣"),
                ),
        )
        .child(body)

}

fn drawer_close_bar(title: &'static str, cx: &Context<SeanceApp>) -> impl IntoElement {
    div()
        .flex_none()
        .h(px(30.))
        .px_3()
        .flex()
        .items_center()
        .justify_between()
        .border_b_1()
        .border_color(SeancePalette::border())
        .child(
            div()
                .text_xs()
                .text_color(SeancePalette::text_faint())
                .child(title),
        )
        .child(
            div()
                .id(SharedString::from(format!("close-drawer-{title}")))
                .px_1()
                .text_sm()
                .text_color(SeancePalette::text_faint())
                .hover(|s| s.text_color(SeancePalette::flame()))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.drawer = Drawer::Closed;
                    this.persist(cx);
                    cx.notify();
                }))
                .child("✕"),
        )
}

fn render_help() -> gpui::AnyElement {
    fn h1(title: &'static str) -> gpui::Div {
        div()
            .pt_4()
            .pb_1()
            .text_sm()
            .font_semibold()
            .text_color(SeancePalette::text())
            .child(title)
    }
    fn section(title: &'static str) -> gpui::Div {
        div()
            .pt_3()
            .pb_1()
            .text_xs()
            .font_semibold()
            .text_color(SeancePalette::violet())
            .child(title)
    }
    fn row(key: &'static str, desc: &'static str) -> gpui::Div {
        div()
            .flex()
            .gap_2()
            .py_0p5()
            .text_sm()
            .child(
                div()
                    .flex_none()
                    .w(px(168.))
                    .text_color(SeancePalette::flame())
                    .child(key),
            )
            .child(div().text_color(SeancePalette::text_dim()).child(desc))
    }
    fn p(text: &'static str) -> gpui::Div {
        div()
            .text_sm()
            .text_color(SeancePalette::text_dim())
            .pb_1()
            .child(text)
    }
    fn bullet(text: &'static str) -> gpui::Div {
        div()
            .flex()
            .gap_2()
            .text_sm()
            .text_color(SeancePalette::text_dim())
            .child(div().flex_none().text_color(SeancePalette::flame_dim()).child("·"))
            .child(div().child(text))
    }

    div()
        .p_4()
        .flex()
        .flex_col()
        .gap_0p5()
        // ── title ──────────────────────────────────────────────────────────
        .child(
            div()
                .text_lg()
                .font_semibold()
                .text_color(SeancePalette::text())
                .child("✦ grimoire — seance"),
        )
        .child(p(
            "a native human+AI co-working playground. every pane is live on your \
             screen; every agent action and every human click can flow through one \
             control plane we fully own.",
        ))
        // ── what is this ───────────────────────────────────────────────────
        .child(h1("what seance is"))
        .child(bullet(
            "panes — terminal sessions (claude / codex / grok / shell) or live file viewers",
        ))
        .child(bullet(
            "workspaces — named circles; the tiling grid shows only the selected one",
        ))
        .child(bullet(
            "control plane — seance ctl over a unix socket so agents drive sibling panes",
        ))
        .child(bullet(
            "notes flip — each pane has a shared markdown scratchpad on its back",
        ))
        .child(bullet(
            "attribution — human + agent actions land in one event log (activity drawer ≋)",
        ))
        // ── pane chrome ────────────────────────────────────────────────────
        .child(h1("pane chrome (title strip)"))
        .child(row("⚡", "arm — one-click inject seance orientation into this agent"))
        .child(row("☎", "phone — telegram topic + stage card (roster/ctl how-to; no participant claim)"))
        .child(row("▤", "pad drawer — task inject body + scratchpad tail"))
        .child(row("💬", "whisper — open a compose bar; Enter injects into the agent"))
        .child(row("stage chip click", "focus pane + open pad drawer (double-click zooms)"))
        .child(row("✎ notes", "flip the pane over onto its notes face"))
        .child(row("↻ face", "flip back from notes to the terminal"))
        .child(row("⇱", "pop the pane into its own OS window (ctrl+shift+p)"))
        .child(row("▣", "shelve / tile (sidebar click re-shows a shelved pane)"))
        .child(row("status badge", "agent-reported state via ctl status-set"))
        .child(row("⚡ driven / 👁", "transient flash when another pane touches this one"))
        // ── notes flip ─────────────────────────────────────────────────────
        .child(h1("notes — the back of a pane"))
        .child(p(
            "notes are not a side drawer. click ✎ notes (or ctrl+shift+s on the \
             active pane) to flip the pane over. the face is a live markdown \
             scratchpad at ~/.local/share/seance/scratch/<slug>.md. the agent \
             in that pane sees the same file via $SEANCE_SCRATCHPAD — writes \
             appear live on both sides (1s poll, last-writer-wins).",
        ))
        .child(bullet("click ↻ face or press ctrl+shift+s again to flip back"))
        .child(bullet("violet border = notes face is up"))
        .child(bullet("right-click a sidebar row → flip notes ✎"))
        // ── whisper + arm ──────────────────────────────────────────────────
        .child(h1("whisper + arm — talking to an agent"))
        .child(p(
            "whisper is for mid-flight steers that should land in the agent's \
             prompt without you fighting its TUI. click 💬 on a terminal pane: \
             a compose bar appears at the bottom of that pane. type, press Enter \
             — seance bracketed-pastes `[whisper from zack] …` and submits. \
             empty Enter / Esc / ✕ cancels.",
        ))
        .child(p(
            "arm (⚡) is the one-click version of “you are in seance — use it.” \
             it injects a short orientation prompt that tells the agent about \
             $SEANCE_* env vars, to run `seance ctl skill`, prefer propose for \
             risky commands, and write notes to $SEANCE_SCRATCHPAD. use it the \
             moment you drop a fresh claude into a pane and want it oriented.",
        ))
        .child(bullet("arm is also available as a chip on the open whisper bar"))
        .child(bullet(
            "for durable notes the agent should keep, prefer the notes flip — not whisper",
        ))
        .child(bullet(
            "ghost propose (ctl propose) is the agent→human safe path: dimmed text, Enter/Esc",
        ))
        // ── workspaces ─────────────────────────────────────────────────────
        .child(h1("workspaces"))
        .child(row("click header", "select workspace (tiling region filters to it)"))
        .child(row("double-click", "rename workspace inline"))
        .child(row("drag header", "reorder workspaces in the sidebar"))
        .child(row("drag pane row", "move pane into another workspace / reorder"))
        .child(row("right-click header", "rename · fork ⑂ · banish (kill all panes)"))
        .child(row("+ (footer)", "new empty workspace"))
        .child(p(
            "banish workspace kills every pane under it (PTYs shut down), removes \
             the workspace from the sidebar, and selects another. irreversible \
             for the processes — scratchpad files on disk are kept.",
        ))
        // ── keys ───────────────────────────────────────────────────────────
        .child(h1("keys"))
        .child(section("global"))
        .child(row("ctrl+shift+n", "summon a new shell pane in the current workspace"))
        .child(row("ctrl+shift+w", "banish (kill) the active pane"))
        .child(row("ctrl+shift+s", "flip notes on the active pane / flip back"))
        .child(row("ctrl+shift+p", "pop active pane out / return to the circle"))
        .child(row("ctrl+shift+k", "precanned prompt palette"))
        .child(row("ctrl+shift+j", "fuzzy jump to pane / workspace"))
        .child(row("ctrl+shift+z", "focus-zoom active pane (esc unzoom)"))
        .child(row("ctrl+shift+f", "jump to last failed shell command"))
        .child(row("ctrl+pgup / pgdn", "previous / next workspace (sidebar order, wraps)"))
        .child(row("ctrl+shift+pgup / pgdn", "previous / next pane in this workspace"))
        .child(row("escape", "dismiss whisper / palette / unzoom"))
        .child(section("terminal focus"))
        .child(row("ctrl+shift+c / v", "copy selection / paste"))
        .child(row("shift+pgup/pgdn", "scrollback"))
        .child(row("ctrl+click / middle-click", "open OSC-8 / URL under cursor"))
        .child(row("mouse drag", "select text (copies on release)"))
        .child(row("wheel", "scroll scrollback"))
        .child(row("2-pane sash", "drag the vertical divider to resize"))
        .child(section("ghost command (agent proposed)"))
        .child(row("enter / tab", "accept + run the dimmed ghost command"))
        .child(row("escape", "dismiss the proposal"))
        .child(row("type", "override — typing clears the ghost"))
        // ── control plane ──────────────────────────────────────────────────
        .child(h1("control plane — seance ctl"))
        .child(p(
            "any process inside a pane (or outside, unscoped) can drive the circle \
             via `seance ctl …` over $XDG_RUNTIME_DIR/seance.sock. inside a pane, \
             calls are auto-scoped to $SEANCE_WORKSPACE; pass --all to cross.",
        ))
        .child(section("discovery + lifecycle"))
        .child(row("ctl list", "panes in scope (+ state, kind, workspace)"))
        .child(row("ctl new --name N", "spawn (--command, --cwd, --workspace, --file PATH)"))
        .child(row("ctl status P", "running/exited, title, popped"))
        .child(row("ctl kill P", "terminate a pane"))
        .child(row("ctl human", "where is the human? focus + workspace + pending asks"))
        .child(section("drive + observe"))
        .child(row("ctl send P TEXT", "bracketed-paste + submit (—no-submit stages)"))
        .child(row("ctl send-raw P $'\\x03'", "raw keys: Ctrl-C, Enter, Esc, arrows"))
        .child(row("ctl read P [--lines N]", "rendered visible screen (truth for agents)"))
        .child(row("ctl propose P CMD", "ghost text; blocks until human accepts/rejects"))
        .child(section("human↔agent surfaces"))
        .child(row("ctl ask \"Q\" --choices a,b", "toast with buttons; CLI blocks for answer"))
        .child(row("ctl status-set STATE", "planning|working|blocked|needs-human|done|idle"))
        .child(row("ctl scratchpad P", "path of that pane's shared notes file"))
        .child(row("ctl timeline --since 10m", "attributed event log (human + agent)"))
        .child(row("ctl fork [--name N]", "fork a workspace: panes respawn, notes copy"))
        .child(row("ctl skill", "print the agent-facing driving guide (paste target)"))
        .child(row("ctl commands P", "structured shell history from shell integration"))
        .child(row("ctl last-command P", "most recent {command,cwd,exit,duration_ms}"))
        .child(section("the loop that works (for agents)"))
        .child(bullet("spawn:  seance ctl new --name worker-1 --cwd /path --command claude"))
        .child(bullet("task:   seance ctl send worker-1 \"…\""))
        .child(bullet("poll:   seance ctl read worker-1 --lines 40  until idle / prompt"))
        .child(bullet("collect: echo result >> $SEANCE_SCRATCHPAD"))
        .child(bullet("clean:  seance ctl kill worker-1"))
        // ── env ────────────────────────────────────────────────────────────
        .child(h1("environment every pane gets"))
        .child(row("$SEANCE_SESSION", "this pane's slug"))
        .child(row("$SEANCE_WORKSPACE", "workspace name (auto-scopes ctl)"))
        .child(row("$SEANCE_SCRATCHPAD", "absolute path to shared notes file"))
        .child(row("$SEANCE_SOCKET", "control socket path"))
        // ── files ──────────────────────────────────────────────────────────
        .child(h1("where things live on disk"))
        .child(row("state", "~/.local/share/seance/state.json"))
        .child(row("notes", "~/.local/share/seance/scratch/<slug>.md"))
        .child(row("events", "~/.local/share/seance/events.jsonl"))
        .child(row("file history", "~/.local/share/seance/filehist/"))
        .child(row("socket", "$XDG_RUNTIME_DIR/seance.sock"))
        // ── file panes ─────────────────────────────────────────────────────
        .child(h1("file panes"))
        .child(p(
            "seance ctl new --name doc --file PATH opens a live viewer (markdown \
             rendered) with mtime poll + history snapshots (◀/▶). no PTY. use \
             when an agent is editing a file you want to watch.",
        ))
        // ── activity ───────────────────────────────────────────────────────
        .child(h1("activity + asks"))
        .child(bullet("≋ in the footer opens the activity drawer (event feed)"))
        .child(bullet(
            "agents call ctl ask → a toast with choice buttons appears above the tiles",
        ))
        .child(bullet("you click; the blocking ctl call returns the answer"))
        // ── tips ───────────────────────────────────────────────────────────
        .child(h1("tips"))
        .child(bullet(
            "fresh claude pane → hit ⚡ arm first, then give the real task via whisper or typing",
        ))
        .child(bullet(
            "prefer ghost propose (from agents) over silent send for anything destructive",
        ))
        .child(bullet(
            "two seance instances fight over the socket — only one can own the control plane",
        ))
        .child(bullet(
            "after rebuilds: cargo build --release && restart so you aren't testing stale code",
        ))
        .child(bullet(
            "deep protocol: docs/CONTROL.md · build/pinning: docs/PLAYBOOK.md · theme: docs/THEME.md",
        ))
        .child(
            div()
                .pt_4()
                .text_xs()
                .text_color(SeancePalette::text_faint())
                .child("grimoire grows with the app — if a surface isn't here, that's a bug."),
        )
        .into_any_element()
}

impl Render for SeanceApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme_bg = cx.theme().background;
        let _ = theme_bg;

        // Summon arrives without a Window on the event path; open rename here.
        if self.pending_rename.is_some() {
            self.flush_pending_rename(window, cx);
        }
        // Launch / spawn: put keyboard on the active terminal once the view exists.
        // Skip while palette / rename / whisper / notes drawer owns input.
        if matches!(self.palette, PaletteMode::Closed)
            && self.renaming.is_none()
            && self.whisper.is_none()
            && self.flipped.is_none()
        {
            self.ensure_keyboard_focus(window, cx);
        }

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
            .on_action(cx.listener(|this, act: &ActKillWorkspace, _, cx| {
                this.kill_workspace(&act.0.clone(), cx);
            }))
            .on_action(cx.listener(|this, act: &ActMoveToWorkspace, _, cx| {
                this.move_to_workspace(&act.slug.clone(), &act.workspace.clone(), cx);
            }))
            .on_action(cx.listener(|this, act: &ActMoveToNewWorkspace, _, cx| {
                let n = this.workspaces().len() + 1;
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
                this.start_rename(
                    RenameTarget::Workspace(act.0.clone()),
                    &act.0.clone(),
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, act: &ActTouchWorkspace, _, cx| {
                this.touch_workspace(&act.0);
                cx.notify();
            }))
            .on_action(cx.listener(|this, act: &ActTransferWorkspace, _, cx| {
                let _ = this
                    .client
                    .transfer_workspace(&act.workspace, &act.to_window);
            }))
            .on_action(cx.listener(|this, act: &ActTransferWorkspaceNewWindow, _, cx| {
                this.send_workspace_to_new_window(&act.0, cx);
            }))
            .on_action(cx.listener(|this, _: &ActCollectAllWindows, _, cx| {
                let _ = this.client.collect_all();
            }))
            .on_action(cx.listener(|this, act: &ActPullWorkspace, _, cx| {
                if let Some(wid) = this.window_id.clone() {
                    let _ = this.client.transfer_workspace(&act.0, &wid);
                }
            }))
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
                    SashDrag::TwoPane { .. } => {
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
                        save_layout_file(
                            this.split_ratio,
                            &this.pane_weights,
                            &this.row_weights,
                        );
                        cx.notify();
                    }
                }),
            )
            .child(self.render_sidebar(window.is_window_active(), cx))
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
                    .children(self.render_asks(cx))
                    .child(self.render_minimize_shelf(window.is_window_active(), cx))
                    .child(self.render_stage_strip(window.is_window_active(), cx))
                    .child(self.render_tiles(window.is_window_active(), cx)),
            )
            .children(self.overview.then(|| self.render_overview(cx).into_any_element()))
            .children(self.render_palette(cx))
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

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    // Minimal std-only base64 (standard alphabet, padded).
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    let input: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for chunk in input.chunks(4) {
        let vals: Vec<u8> = chunk
            .iter()
            .take_while(|&&b| b != b'=')
            .map(|&b| lookup[b as usize])
            .collect();
        if vals.iter().any(|&v| v == 255) {
            return Err("invalid character".into());
        }
        match vals.len() {
            4 => {
                out.push((vals[0] << 2) | (vals[1] >> 4));
                out.push((vals[1] << 4) | (vals[2] >> 2));
                out.push((vals[2] << 6) | vals[3]);
            }
            3 => {
                out.push((vals[0] << 2) | (vals[1] >> 4));
                out.push((vals[1] << 4) | (vals[2] >> 2));
            }
            2 => {
                out.push((vals[0] << 2) | (vals[1] >> 4));
            }
            _ => return Err("truncated input".into()),
        }
    }
    Ok(out)
}
