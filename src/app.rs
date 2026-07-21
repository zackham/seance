//! SeanceApp: root view. Left sidebar (panes grouped by workspace),
//! auto-tiling terminal region, per-pane notes flip, control-plane pump.
//!
//! # Notes = flip the pane
//! Notes are the *back* of a pane, not a side drawer. Click ✎ (or
//! ctrl+shift+s) to flip the pane over onto its shared scratchpad; click
//! again (or the ↻ chip) to flip back. The agent sees the same file via
//! `$SEANCE_SCRATCHPAD`.

use std::time::Duration;

use futures::StreamExt;
use gpui::{
    div, prelude::*, px, Action, Animation, AnimationExt as _, Context, Entity, FocusHandle,
    Focusable as _, SharedString, Window, ease_in_out,
};
use gpui_component::{
    input::{Input, InputEvent, InputState},
    menu::ContextMenuExt as _,
    ActiveTheme as _, GlobalState, StyledExt as _, WindowExt as _,
};
use serde::Deserialize;

use crate::{
    control::{ControlRequest, ControlResponse},
    events,
    gui_client::GuiClient,
    pane::{spawn_pane, Pane, PaneBody, PaneKind, SpawnRequest},
    remote_term::RemoteTerminal,
    remote_term_view::RemoteTerminalView,
    runtime::protocol::{GuiEvent, PaneInfo},
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

Confirm you're oriented and ready, then wait for the next instruction."
;

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActRenameWorkspace(pub String);

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
/// (see `flipped`); the drawer is only the activity feed.
enum Drawer {
    Closed,
    Activity,
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

pub struct SeanceApp {
    panes: Vec<Pane>,
    asks: Vec<PendingAsk>,
    statuses: std::collections::HashMap<String, PaneStatus>,
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
}

impl SeanceApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = ScratchpadStore::new().expect("scratchpad dir");

        // Connect to the session daemon (PTYs live there).
        let (client, event_rx) = GuiClient::connect().expect("gui client connect to daemon");

        let mut app = SeanceApp {
            panes: Vec::new(),
            asks: Vec::new(),
            statuses: std::collections::HashMap::new(),
            touches: std::collections::HashMap::new(),
            ask_counter: 0,
            proposals: std::collections::HashMap::new(),
            proposal_counter: 0,
            whisper: None,
            flipped: None,
            cmd_log: crate::cmdlog::CommandLog::new(),
            active_slug: None,
            selected_workspace: None,
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
        };

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
            } => {
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
                }
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
                        eprintln!("[seance gui] bad grid_bin for {pane}: {e}");
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
                if self.active_slug.as_deref() == Some(slug.as_str()) {
                    self.active_slug = self.panes.first().map(|p| p.slug.clone());
                }
                cx.notify();
            }
            GuiEvent::PaneExited { slug, .. } => {
                // Daemon also broadcasts PaneKilled and removes the pane; this
                // is a belt-and-suspenders remove if Killed is delayed/lost.
                self.panes.retain(|p| p.slug != slug);
                if self.active_slug.as_deref() == Some(slug.as_str()) {
                    self.active_slug = self.panes.first().map(|p| p.slug.clone());
                }
                if self.flipped.as_ref().is_some_and(|(s, _)| s == &slug) {
                    self.flipped = None;
                }
                if self.whisper.as_ref().is_some_and(|(s, _)| s == &slug) {
                    self.whisper = None;
                }
                cx.notify();
            }
            GuiEvent::Ask { ask } => {
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
                self.statuses.insert(slug, PaneStatus { state, note });
                cx.notify();
            }
            GuiEvent::Touch { slug, verb, actor } => {
                self.touch(&slug, &verb, &actor, cx);
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

    /// All workspaces in sidebar display order: explicit order first, then
    /// any not-yet-ordered ones alphabetically.
    fn workspaces(&self) -> Vec<String> {
        let mut known: Vec<String> = self
            .panes
            .iter()
            .map(|s| s.workspace.clone())
            .chain(self.extra_workspaces.iter().cloned())
            .chain(self.selected_workspace.iter().cloned())
            .collect();
        known.sort();
        known.dedup();
        let mut out: Vec<String> = self
            .workspace_order
            .iter()
            .filter(|w| known.contains(w))
            .cloned()
            .collect();
        for w in known {
            if !out.contains(&w) {
                out.push(w);
            }
        }
        out
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
        self.extra_workspaces.push(name.clone());
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
                let _ = self.client.rename_workspace(&old, &new_ws);
            }
        }
        cx.notify();
    }

    fn select_workspace(&mut self, workspace: &str, cx: &mut Context<Self>) {
        self.selected_workspace = Some(workspace.to_string());
        let _ = self.client.set_focus(None, Some(workspace.to_string()));
        self.persist(cx);
        cx.notify();
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
        self.select_workspace(&ws, cx);
        // Prefer a tiled pane in this workspace, else any pane there.
        let slug = self
            .panes
            .iter()
            .find(|p| p.workspace == ws && p.tiled && p.popped.is_none())
            .or_else(|| self.panes.iter().find(|p| p.workspace == ws))
            .map(|p| p.slug.clone());
        if let Some(slug) = slug {
            self.set_active(&slug, window, cx);
        }
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
            self.selected_workspace = Some(pane.workspace.clone());
            let _ = self.client.set_focus(
                Some(slug.to_string()),
                Some(pane.workspace.clone()),
            );
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
        if self.active_slug.as_deref() == Some(slug) {
            self.active_slug = self.panes.first().map(|s| s.slug.clone());
        }
        if self.flipped.as_ref().is_some_and(|(s, _)| s == slug) {
            self.flipped = None;
        }
        if self.whisper.as_ref().is_some_and(|(s, _)| s == slug) {
            self.whisper = None;
        }
        cx.notify();
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

    fn persist(&self, cx: &mut Context<Self>) {
        let state = AppState {
            panes: self.panes.iter().map(|s| s.persisted()).collect(),
            sidebar_width: None,
            drawer_width: None,
            drawer_open: matches!(self.drawer, Drawer::Activity),
            active_slug: self.active_slug.clone(),
            selected_workspace: self.selected_workspace.clone(),
            extra_workspaces: self.extra_workspaces.clone(),
            workspace_order: self.workspace_order.clone(),
            window_size: None,
        };
        if let Err(err) = state.save() {
            eprintln!("[seance] state save failed: {err:#}");
        }
        let _ = cx;
    }

    // ---- control plane ----

    fn handle_control(&mut self, request: ControlRequest, cx: &mut Context<Self>) -> ControlResponse {
        use serde_json::json;
        let ok = |data: serde_json::Value| ControlResponse {
            ok: true,
            data: Some(data),
            error: None,
        };
        let err = |msg: String| ControlResponse {
            ok: false,
            data: None,
            error: Some(msg),
        };

        // Scoped lookup: a workspace-scoped caller only sees its own panes.
        let find_scoped = |app: &Self, key: &str, scope: &Option<String>| -> Result<usize, String> {
            match app.find_session(key) {
                Some(idx) => match scope {
                    Some(ws) if app.panes[idx].workspace != *ws => Err(format!(
                        "pane '{key}' is outside your workspace '{ws}' (use --all to cross)"
                    )),
                    _ => Ok(idx),
                },
                None => Err(match scope {
                    Some(ws) => format!("no pane '{key}' in workspace '{ws}'"),
                    None => format!("no pane '{key}'"),
                }),
            }
        };

        // Attribution: agents self-identify via `from` (their own pane slug).
        let actor_of = |from: &Option<String>| -> String {
            match from {
                Some(f) => format!("agent:{f}"),
                None => "cli".to_string(),
            }
        };

        match request {
            ControlRequest::List { scope, from } => {
                let _ = actor_of(&from);
                let panes: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|s| scope.as_deref().is_none_or(|ws| s.workspace == ws))
                    .map(|s| {
                        json!({
                            "kind": s.kind_str(),
                            "name": s.name,
                            "slug": s.slug,
                            "workspace": s.workspace,
                            "command": s.command,
                            "cwd": s.cwd,
                            "tiled": s.tiled,
                            "popped": s.popped.is_some(),
                            "running": s.is_running(cx),
                            "title": s.title(cx),
                        })
                    })
                    .collect();
                ok(json!({ "panes": panes, "scope": scope }))
            }
            ControlRequest::New {
                name,
                cwd,
                command,
                workspace,
                file,
                scope,
                from,
            } => {
                let actor = actor_of(&from);
                // Scoped callers spawn into their own workspace by default,
                // and may not spawn into a different one.
                let workspace = workspace.or_else(|| scope.clone());
                if let (Some(ws), Some(sc)) = (workspace.as_deref(), scope.as_deref()) {
                    if ws != sc {
                        return err(format!(
                            "scoped to workspace '{sc}' — cannot spawn into '{ws}' (use --all)"
                        ));
                    }
                }
                let slug = self.spawn_internal(
                    SpawnRequest {
                        name,
                        cwd,
                        command,
                        workspace,
                        tiled: true,
                        resume: false,
                        file,
                    },
                    cx,
                );
                match slug {
                    Some(slug) => {
                        let pane = self.panes.iter().find(|s| s.slug == slug).unwrap();
                        events::log(
                            &actor,
                            Some(&pane.workspace),
                            Some(&slug),
                            "ctl_new",
                            format!("spawned '{}'", pane.name),
                        );
                        ok(json!({
                            "slug": slug,
                            "workspace": pane.workspace,
                            "scratchpad": pane.scratch_path.to_string_lossy(),
                        }))
                    }
                    None => err("spawn failed (see seance stderr)".into()),
                }
            }
            ControlRequest::Send {
                pane,
                text,
                submit,
                scope,
                from,
            } => match find_scoped(self, &pane, &scope) {
                Ok(idx) => {
                    let Some(terminal) = self.panes[idx].terminal().cloned() else {
                        return err("not a terminal pane".into());
                    };
                    let actor = actor_of(&from);
                    let slug = self.panes[idx].slug.clone();
                    events::log(
                        &actor,
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "ctl_send",
                        format!("sent {} chars{}", text.len(), if submit { " + submit" } else { "" }),
                    );
                    self.touch(&slug, "⚡ driven", &actor, cx);
                    terminal.update(cx, |term, cx| {
                        term.scroll_to_bottom();
                        term.inject(text, submit, cx);
                    });
                    ok(serde_json::Value::Null)
                }
                Err(e) => err(e),
            },
            ControlRequest::SendRaw {
                pane,
                bytes_b64,
                scope,
                from,
            } => match find_scoped(self, &pane, &scope) {
                Ok(idx) => match base64_decode(&bytes_b64) {
                    Ok(bytes) => {
                        let Some(terminal) = self.panes[idx].terminal().cloned() else {
                            return err("not a terminal pane".into());
                        };
                        let actor = actor_of(&from);
                        let slug = self.panes[idx].slug.clone();
                        events::log(
                            &actor,
                            Some(&self.panes[idx].workspace),
                            Some(&slug),
                            "ctl_send_raw",
                            format!("sent {} raw bytes", bytes.len()),
                        );
                        self.touch(&slug, "⚡ driven", &actor, cx);
                        terminal.update(cx, |term, _| {
                            term.write_bytes(bytes);
                        });
                        ok(serde_json::Value::Null)
                    }
                    Err(e) => err(format!("bad base64: {e}")),
                },
                Err(e) => err(e),
            },
            ControlRequest::Read { pane, lines, scope, from } => {
                match find_scoped(self, &pane, &scope) {
                    Ok(idx) => {
                        let actor = actor_of(&from);
                        let slug = self.panes[idx].slug.clone();
                        // Self-reads are routine; only surface cross-pane observation.
                        if from.as_deref() != Some(slug.as_str()) {
                            events::log(
                                &actor,
                                Some(&self.panes[idx].workspace),
                                Some(&slug),
                                "ctl_read",
                                "read screen".to_string(),
                            );
                            self.touch(&slug, "👁 observed", &actor, cx);
                        }
                        let text = match self.panes[idx].terminal() {
                            Some(t) => t.read(cx).screen_text(lines),
                            None => {
                                // File panes: reading = the document's content.
                                self.panes[idx]
                                    .file_view()
                                    .map(|v| {
                                        std::fs::read_to_string(v.read(cx).path())
                                            .unwrap_or_default()
                                    })
                                    .unwrap_or_default()
                            }
                        };
                        ok(json!({ "screen": text }))
                    }
                    Err(e) => err(e),
                }
            }
            ControlRequest::Status { pane, scope, from } => match find_scoped(self, &pane, &scope) {
                #[allow(unused_variables)]
                Ok(idx) => {
                    let s = &self.panes[idx];
                    let exit_code = s
                        .terminal()
                        .and_then(|t| t.read(cx).exited.flatten());
                    ok(json!({
                        "kind": s.kind_str(),
                        "name": s.name,
                        "slug": s.slug,
                        "workspace": s.workspace,
                        "command": s.command,
                        "running": s.is_running(cx),
                        "exit_code": exit_code,
                        "title": s.title(cx),
                        "tiled": s.tiled,
                        "popped": s.popped.is_some(),
                    }))
                }
                Err(e) => err(e),
            },
            ControlRequest::Kill { pane, scope, from } => match find_scoped(self, &pane, &scope) {
                Ok(idx) => {
                    let actor = actor_of(&from);
                    let slug = self.panes[idx].slug.clone();
                    events::log(
                        &actor,
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "ctl_kill",
                        format!("killed '{}'", self.panes[idx].name),
                    );
                    self.kill_session(&slug, cx);
                    ok(serde_json::Value::Null)
                }
                Err(e) => err(e),
            },
            ControlRequest::Scratchpad { pane, scope, from } => match find_scoped(self, &pane, &scope) {
                #[allow(unused_variables)]
                Ok(idx) => ok(json!({
                    "path": self.panes[idx].scratch_path.to_string_lossy(),
                })),
                Err(e) => err(e),
            },
            ControlRequest::Timeline {
                since_secs,
                pane,
                actor,
                limit,
                scope,
                from: _,
            } => {
                let since_ms = since_secs
                    .map(|s| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0)
                            .saturating_sub(s * 1000)
                    })
                    .unwrap_or(0);
                let entries = events::read(
                    since_ms,
                    scope.as_deref(),
                    pane.as_deref(),
                    actor.as_deref(),
                    limit.unwrap_or(100),
                );
                let rows: Vec<_> = entries
                    .iter()
                    .map(|e| {
                        json!({
                            "time": events::fmt_time(e.ts),
                            "actor": e.actor,
                            "pane": e.pane,
                            "workspace": e.workspace,
                            "kind": e.kind,
                            "detail": e.detail,
                        })
                    })
                    .collect();
                ok(json!({ "events": rows }))
            }
            ControlRequest::StatusSet {
                state,
                note,
                pane,
                scope,
                from,
            } => {
                let target = pane.or_else(|| from.clone());
                let Some(target) = target else {
                    return err("status-set: no pane (run inside a pane or pass --pane)".into());
                };
                match find_scoped(self, &target, &scope) {
                    Ok(idx) => {
                        let slug = self.panes[idx].slug.clone();
                        let actor = actor_of(&from);
                        events::log(
                            &actor,
                            Some(&self.panes[idx].workspace),
                            Some(&slug),
                            "status_set",
                            format!("{state}{}", note.as_deref().map(|n| format!(": {n}")).unwrap_or_default()),
                        );
                        self.statuses.insert(
                            slug,
                            PaneStatus {
                                state,
                                note,
                            },
                        );
                        cx.notify();
                        ok(serde_json::Value::Null)
                    }
                    Err(e) => err(e),
                }
            }
            ControlRequest::Ask {
                question,
                choices,
                scope,
                from,
            } => {
                self.ask_counter += 1;
                let id = format!("ask-{}", self.ask_counter);
                let from_label = from.clone().unwrap_or_else(|| "cli".into());
                events::log(
                    &actor_of(&from),
                    scope.as_deref(),
                    from.as_deref(),
                    "ask",
                    format!("asked: {question}"),
                );
                self.asks.push(PendingAsk {
                    id: id.clone(),
                    from: from_label,
                    workspace: scope.clone(),
                    question,
                    choices: choices.unwrap_or_default(),
                    answer: None,
                });
                cx.notify();
                ok(json!({ "id": id }))
            }
            ControlRequest::Propose {
                pane,
                text,
                reason,
                scope,
                from,
            } => match find_scoped(self, &pane, &scope) {
                Ok(idx) => {
                    if self.panes[idx].terminal().is_none() {
                        return err("not a terminal pane".into());
                    }
                    self.proposal_counter += 1;
                    let id = format!("prop-{}", self.proposal_counter);
                    let actor = actor_of(&from);
                    let slug = self.panes[idx].slug.clone();
                    let from_label = from.clone().unwrap_or_else(|| "cli".into());
                    // Supersede any pending proposal on this pane.
                    let old = self.panes[idx]
                        .terminal()
                        .and_then(|t| t.read(cx).ghost.as_ref().map(|g| g.id.clone()));
                    if let Some(old_id) = old {
                        if let Some(entry) = self.proposals.get_mut(&old_id) {
                            entry.1 = Some("superseded".to_string());
                        }
                    }
                    events::log(
                        &actor,
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "propose",
                        format!("proposed: {text}"),
                    );
                    self.touch(&slug, "💭 proposal", &actor, cx);
                    self.proposals.insert(id.clone(), (slug, None));
                    self.panes[idx].terminal().unwrap().clone().update(cx, |term, _| {
                        term.ghost = Some(crate::terminal::Ghost {
                            id: id.clone(),
                            text,
                            from: from_label,
                            reason,
                        });
                    });
                    cx.notify();
                    ok(json!({ "id": id }))
                }
                Err(e) => err(e),
            },
            ControlRequest::ProposeResult { id, .. } => match self.proposals.get(&id) {
                Some((_, Some(outcome))) => {
                    let outcome = outcome.clone();
                    self.proposals.remove(&id);
                    ok(json!({ "resolved": true, "outcome": outcome }))
                }
                Some((_, None)) => ok(json!({ "resolved": false })),
                None => err(format!("no proposal '{id}'")),
            },
            ControlRequest::Human { .. } => ok(json!({
                "focused_pane": self.active_slug,
                "selected_workspace": self.selected_workspace,
                "pending_asks": self.asks.iter().filter(|a| a.answer.is_none()).count(),
            })),
            ControlRequest::WorkspaceFork {
                workspace,
                name,
                scope,
                from,
            } => {
                let src = workspace
                    .or_else(|| scope.clone())
                    .or_else(|| self.selected_workspace.clone());
                let Some(src) = src else {
                    return err("fork: no source workspace".into());
                };
                let new_name = self.fork_workspace(&src, name, &actor_of(&from), cx);
                match new_name {
                    Some(n) => ok(json!({ "workspace": n })),
                    None => err(format!("workspace '{src}' has no panes")),
                }
            }
            ControlRequest::CmdBegin { command, cwd, from, .. } => {
                let Some(pane) = from else {
                    return err("cmd-begin: must be called from inside a pane".into());
                };
                let cwd = cwd.unwrap_or_default();
                events::log(
                    &format!("agent:{pane}"),
                    None,
                    Some(&pane),
                    "cmd_start",
                    format!("$ {command}"),
                );
                self.cmd_log.begin(&pane, command, cwd);
                ok(serde_json::Value::Null)
            }
            ControlRequest::CmdEnd { exit, from, .. } => {
                let Some(pane) = from else {
                    return err("cmd-end: must be called from inside a pane".into());
                };
                self.cmd_log.end(&pane, exit);
                if let Some(rec) = self.cmd_log.last(&pane, false) {
                    events::log(
                        &format!("agent:{pane}"),
                        None,
                        Some(&pane),
                        "cmd_end",
                        format!(
                            "exit {} after {}ms: {}",
                            exit,
                            rec.duration_ms().unwrap_or(0),
                            rec.command
                        ),
                    );
                }
                cx.notify();
                ok(serde_json::Value::Null)
            }
            ControlRequest::Commands { pane, limit, scope, .. } => {
                match find_scoped(self, &pane, &scope) {
                    Ok(idx) => {
                        let slug = self.panes[idx].slug.clone();
                        let records = self.cmd_log.list(&slug, limit.unwrap_or(50));
                        ok(serde_json::to_value(records).unwrap_or(serde_json::Value::Null))
                    }
                    Err(e) => err(e),
                }
            }
            ControlRequest::LastCommand { pane, failed_only, scope, .. } => {
                match find_scoped(self, &pane, &scope) {
                    Ok(idx) => {
                        let slug = self.panes[idx].slug.clone();
                        match self.cmd_log.last(&slug, failed_only) {
                            Some(rec) => {
                                ok(serde_json::to_value(rec).unwrap_or(serde_json::Value::Null))
                            }
                            None => err("no matching command".into()),
                        }
                    }
                    Err(e) => err(e),
                }
            }
            ControlRequest::AskResult { id, .. } => {
                match self.asks.iter().position(|a| a.id == id) {
                    Some(idx) => {
                        if let Some(answer) = self.asks[idx].answer.clone() {
                            self.asks.remove(idx);
                            cx.notify();
                            ok(json!({ "answered": true, "answer": answer }))
                        } else {
                            ok(json!({ "answered": false }))
                        }
                    }
                    None => err(format!("no ask '{id}'")),
                }
            }
        }
    }

    /// Fork a workspace: respawn each pane (name/cwd/command preserved) into a
    /// fresh workspace and copy scratchpad contents. PTYs restart; layout,
    /// commands, and notes carry over.
    fn fork_workspace(
        &mut self,
        src: &str,
        name: Option<String>,
        actor: &str,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let members: Vec<(String, String, String, bool, String)> = self
            .panes
            .iter()
            .filter(|p| p.workspace == src)
            .map(|p| {
                (
                    p.name.clone(),
                    p.cwd.clone(),
                    p.command.clone(),
                    p.tiled,
                    p.scratch_path.to_string_lossy().to_string(),
                )
            })
            .collect();
        if members.is_empty() {
            return None;
        }
        let existing = self.workspaces();
        let new_ws = name
            .map(|n| crate::state::slugify(&n))
            .filter(|n| !n.is_empty() && !existing.contains(n))
            .unwrap_or_else(|| {
                let mut n = 2;
                loop {
                    let candidate = format!("{src}-fork-{n}");
                    if !existing.contains(&candidate) {
                        break candidate;
                    }
                    n += 1;
                }
            });
        events::log(
            actor,
            Some(&new_ws),
            None,
            "workspace_forked",
            format!("forked '{src}' -> '{new_ws}' ({} panes)", members.len()),
        );
        for (pname, cwd, command, tiled, old_scratch) in members {
            let slug = self.spawn_internal(
                SpawnRequest {
                    name: pname,
                    cwd: Some(cwd),
                    command: Some(command),
                    workspace: Some(new_ws.clone()),
                    tiled,
                    resume: false,
                    file: None,
                },
                cx,
            );
            // Carry the notes across.
            if let Some(slug) = slug {
                if let Some(new_pane) = self.panes.iter().find(|p| p.slug == slug) {
                    if let Ok(content) = std::fs::read_to_string(&old_scratch) {
                        let _ = std::fs::write(&new_pane.scratch_path, content);
                    }
                }
            }
        }
        self.selected_workspace = Some(new_ws.clone());
        self.persist(cx);
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

    fn cancel_whisper(&mut self, cx: &mut Context<Self>) {
        self.whisper = None;
        cx.notify();
    }

    /// Record a transient cross-pane touch ("⚡ driven by X") and schedule its
    /// fade — the visible-agency overlay the council converged on.
    fn touch(&mut self, slug: &str, verb: &str, actor: &str, cx: &mut Context<Self>) {
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

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
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

        let active = self.active_slug.clone();

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
            .child(
                div()
                    .id("pane-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(by_workspace.into_iter().map(|(workspace, panes)| {
                        let selected = self.selected_workspace.as_deref() == Some(workspace.as_str());
                        let all_workspaces = self.workspaces();
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
                        let header: gpui::AnyElement = if renaming_this_ws {
                            div()
                                .px_2()
                                .pt_2()
                                .pb_1()
                                .children(rename_input.map(|i| Input::new(&i)))
                                .into_any_element()
                        } else {
                            div()
                                .id(SharedString::from(format!("ws-{workspace}")))
                                .px_2()
                                .pt_2()
                                .pb_1()
                                .rounded_md()
                                .text_xs()
                                .cursor_pointer()
                                .text_color(if selected {
                                    SeancePalette::violet()
                                } else {
                                    SeancePalette::violet_dim()
                                })
                                .when(selected, |d| d.bg(SeancePalette::surface()))
                                .hover(|s| s.bg(SeancePalette::surface()))
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
                                        // Once at drag start only. Mid-drag,
                                        // `has_active_drag` already freezes
                                        // window text selection updates.
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
                                        this.select_workspace(&ws_for_click, cx);
                                    }
                                }))
                                .context_menu(move |menu, _, _| {
                                    menu.menu(
                                        "rename workspace",
                                        Box::new(ActRenameWorkspace(ws_for_menu.clone())),
                                    )
                                    .menu(
                                        "fork workspace ⑂",
                                        Box::new(ActForkWorkspace(ws_for_menu.clone())),
                                    )
                                    .separator()
                                    .menu(
                                        "banish workspace (kill all panes)",
                                        Box::new(ActKillWorkspace(ws_for_menu.clone())),
                                    )
                                })
                                .child(format!(
                                    "{} {workspace} ({})",
                                    if selected { "◆" } else { "◈" },
                                    panes.len()
                                ))
                                .into_any_element()
                        };
                        div()
                            .id(SharedString::from(format!("wsgroup-{workspace}")))
                            .flex()
                            .flex_col()
                            .gap_0p5()
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
                            .children(panes.into_iter().map(|pane| {
                                render_session_row(
                                    pane,
                                    active.as_deref(),
                                    &all_workspaces,
                                    self.renaming.as_ref(),
                                    self.statuses.get(&pane.slug),
                                    cx,
                                )
                            }))
                    })),
            )
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

    fn render_tiles(&self, cx: &Context<Self>) -> impl IntoElement {
        // The tiling region shows only the SELECTED workspace's tiled panes.
        let tiled: Vec<&Pane> = self
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
        let n = tiled.len();
        let active = self.active_slug.clone();

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

        // Balanced auto-grid: cols = ceil(sqrt(n)).
        let cols = (n as f32).sqrt().ceil() as usize;
        let rows = n.div_ceil(cols);

        // min_w_0 / overflow_hidden on every flex level so panes can shrink
        // with the window. Without that, flex items default to min-content
        // (terminal grid / markdown line length) and the tile region refuses
        // to narrow — content spills off-screen instead of reflowing.
        let mut grid = div()
            .flex_1()
            .h_full()
            .w_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap_1()
            .p_1();
        let mut it = tiled.into_iter();
        for _ in 0..rows {
            let mut row = div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .w_full()
                .overflow_hidden()
                .flex()
                .gap_1();
            for _ in 0..cols {
                if let Some(pane) = it.next() {
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
                    row = row.child(render_pane(
                        pane,
                        active.as_deref(),
                        self.statuses.get(&pane.slug),
                        self.touches.get(&pane.slug),
                        whisper,
                        flipped,
                        cx,
                    ));
                }
            }
            grid = grid.child(row);
        }
        grid.into_any_element()
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
    let slug_for_kill = slug.clone();

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
        .rounded_md()
        .border_1()
        .border_color(gpui::transparent_black())
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .when(is_active, |d| d.bg(SeancePalette::surface()))
        .hover(|s| s.bg(SeancePalette::surface()))
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
                .text_sm()
                .text_color(if is_active {
                    SeancePalette::text()
                } else {
                    SeancePalette::text_dim()
                })
                .overflow_hidden()
                .child(if is_popped {
                    format!("{} ⇱", pane.name)
                } else {
                    pane.name.clone()
                }),
        )
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
        .child(
            div()
                .id(SharedString::from(format!("kill-{slug}")))
                .flex_none()
                .text_xs()
                .text_color(SeancePalette::text_faint())
                .cursor_pointer()
                .invisible()
                .group_hover("row", |s| s.visible())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.kill_session(&slug_for_kill, cx);
                    cx.stop_propagation();
                }))
                .tooltip(tip("banish (kill) this pane"))
                .child("✕"),
        )
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
    touch: Option<&(String, String, std::time::Instant)>,
    whisper: Option<&Entity<InputState>>,
    flipped: Option<&Entity<ScratchpadDrawer>>,
    cx: &Context<SeanceApp>,
) -> impl IntoElement {
    let is_active = active == Some(pane.slug.as_str());
    let is_flipped = flipped.is_some();
    let is_whispering = whisper.is_some();
    let slug = pane.slug.clone();
    let running = pane.is_running(cx);
    let title = pane.title(cx).unwrap_or_else(|| pane.command.clone());
    // Local or daemon-backed terminal panes both get arm/whisper chrome.
    let has_terminal = pane.terminal().is_some() || pane.remote_terminal().is_some();

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

    // Whisper compose bar (rendered; previously state was set but never shown).
    let whisper_bar: Option<gpui::AnyElement> = whisper.map(|input| {
        let slug_arm = slug.clone();
        let slug_cancel = slug.clone();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1p5()
            .border_t_1()
            .border_color(SeancePalette::violet_dim())
            .bg(SeancePalette::surface())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .text_color(SeancePalette::violet())
                            .child("💬 whisper — injects into the agent's prompt (Enter sends)"),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("whisper-cancel-{slug}")))
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .hover(|s| s.text_color(SeancePalette::flame()))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cancel_whisper(cx);
                                cx.stop_propagation();
                            }))
                            .tooltip(tip("cancel whisper"))
                            .child("✕"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(input)),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("arm-{slug_arm}")))
                            .flex_none()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_xs()
                            .text_color(SeancePalette::flame())
                            .bg(SeancePalette::bg())
                            .border_1()
                            .border_color(SeancePalette::flame_dim())
                            .hover(|s| s.bg(SeancePalette::bg_elevated()))
                            .cursor_pointer()
                            .on_click(cx.listener({
                                let slug = slug_arm.clone();
                                move |this, _, _, cx| {
                                    this.arm_pane(&slug, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .tooltip(tip(
                                "arm this agent with seance orientation (one-click inject)",
                            ))
                            .child("⚡ arm"),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("whisper-x-{slug_cancel}")))
                            .flex_none()
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .hover(|s| s.text_color(SeancePalette::flame()))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cancel_whisper(cx);
                                cx.stop_propagation();
                            }))
                            .child("esc"),
                    ),
            )
            .into_any_element()
    });

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
        .border_1()
        .border_color(if is_flipped {
            SeancePalette::violet()
        } else if is_active {
            SeancePalette::flame_dim()
        } else {
            SeancePalette::border()
        })
        .bg(SeancePalette::bg())
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
                .bg(if is_flipped {
                    SeancePalette::surface()
                } else {
                    SeancePalette::bg_elevated()
                })
                .child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(if running {
                            if is_active {
                                SeancePalette::flame()
                            } else {
                                SeancePalette::flame_dim()
                            }
                        } else {
                            SeancePalette::status_exited()
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .text_xs()
                        .text_color(if is_flipped {
                            SeancePalette::violet()
                        } else {
                            SeancePalette::text_dim()
                        })
                        .overflow_hidden()
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
                // Whisper (terminals only).
                .when(has_terminal, |d| {
                    d.child(
                        div()
                            .id(SharedString::from(format!("whisper-{slug}")))
                            .flex_none()
                            .text_xs()
                            .text_color(if is_whispering {
                                SeancePalette::violet()
                            } else {
                                SeancePalette::text_faint()
                            })
                            .hover(|s| s.text_color(SeancePalette::violet()))
                            .cursor_pointer()
                            .on_click(cx.listener({
                                let slug = slug.clone();
                                move |this, _, window, cx| {
                                    this.start_whisper(&slug, window, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .tooltip(tip(
                                "whisper — open a bar to inject a message into this agent",
                            ))
                            .child("💬"),
                    )
                })
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
        .children(whisper_bar)
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
        .child(row("💬", "whisper — open a compose bar; Enter injects into the agent"))
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
        .child(row("ctrl+shift+s", "flip notes on the active pane / flip back"))
        .child(row("ctrl+shift+p", "pop active pane out / return to the circle"))
        .child(row("ctrl+pgup / pgdn", "previous / next workspace (sidebar order, wraps)"))
        .child(row("escape", "dismiss an open whisper compose bar"))
        .child(section("terminal focus"))
        .child(row("ctrl+shift+c / v", "copy selection / paste"))
        .child(row("shift+pgup/pgdn", "scrollback"))
        .child(row("mouse drag", "select text (copies on release)"))
        .child(row("wheel", "scroll scrollback"))
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

        div()
            .id("seance-root")
            .size_full()
            .flex()
            .bg(SeancePalette::bg())
            .track_focus(&self.focus_handle)
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
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let ks = &event.keystroke;
                // Escape dismisses whisper compose or cancels inline rename.
                if ks.key.as_str() == "escape" {
                    if this.renaming.is_some() {
                        this.renaming = None;
                        this.pending_rename = None;
                        cx.notify();
                        cx.stop_propagation();
                        return;
                    }
                    if this.whisper.is_some() {
                        this.cancel_whisper(cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                // Ctrl+PageUp / Ctrl+PageDown — cycle workspaces (terminal
                // emulators' classic prev/next-tab chord; PTY views bubble it).
                if ks.modifiers.control
                    && !ks.modifiers.shift
                    && !ks.modifiers.alt
                    && (ks.key.as_str() == "pageup" || ks.key.as_str() == "pagedown")
                {
                    let delta = if ks.key.as_str() == "pageup" { -1 } else { 1 };
                    this.cycle_workspace(delta, window, cx);
                    cx.stop_propagation();
                    return;
                }
                if ks.modifiers.control && ks.modifiers.shift {
                    match ks.key.as_str() {
                        "n" => {
                            this.new_default_session(cx);
                            cx.stop_propagation();
                        }
                        "s" => {
                            this.toggle_notes_flip(window, cx);
                            cx.stop_propagation();
                        }
                        "p" => {
                            if let Some(slug) = this.active_slug.clone() {
                                this.toggle_popout(&slug, cx);
                                cx.stop_propagation();
                            }
                        }
                        _ => {}
                    }
                }
            }))
            .child(self.render_sidebar(cx))
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
                    .child(self.render_tiles(cx)),
            )
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
