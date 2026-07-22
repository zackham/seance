//! Wire types for daemon ↔ GUI and connection hello.

use serde::{Deserialize, Serialize};

use super::snapshot::{GhostSnap, GridSnapshot};
use crate::control::ControlRequest;

/// First line on every socket connection.
#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub role: String,
    /// Optional protocol version.
    #[serde(default)]
    pub v: Option<u32>,
}

/// Client → daemon on a GUI connection (JSON lines after hello).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GuiRequest {
    /// Full attach: daemon replies with State then streams grids.
    /// `empty: true` = new/second window that claims no existing workspaces
    /// (unless this is the only GUI, in which case orphans are always claimed).
    Attach {
        #[serde(default)]
        selected_workspace: Option<String>,
        #[serde(default)]
        focused_pane: Option<String>,
        /// Prefer an empty window (second process / "new window").
        #[serde(default)]
        empty: bool,
    },
    Input {
        pane: String,
        bytes_b64: String,
    },
    Resize {
        pane: String,
        cols: u16,
        rows: u16,
    },
    Scroll {
        pane: String,
        delta: i32,
    },
    ScrollBottom {
        pane: String,
    },
    Inject {
        pane: String,
        text: String,
        #[serde(default = "default_true")]
        submit: bool,
    },
    GhostAccept {
        pane: String,
    },
    GhostReject {
        pane: String,
    },
    /// Layout / spawn ops also usable from GUI.
    Spawn {
        name: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default)]
        file: Option<String>,
        #[serde(default = "default_true")]
        tiled: bool,
    },
    Kill {
        pane: String,
    },
    SetTiled {
        pane: String,
        tiled: bool,
    },
    /// Move `pane` into `workspace`, optionally inserting it immediately
    /// before `before` (another pane slug). When `before` is absent the pane
    /// is appended after other panes in that workspace (i.e. at end of the
    /// global pane list among peers that share the workspace — full list
    /// order is still the persistence key).
    MovePane {
        pane: String,
        workspace: String,
        #[serde(default)]
        before: Option<String>,
    },
    /// Sidebar workspace drag: place `moved` immediately before `before`.
    ReorderWorkspace {
        moved: String,
        before: String,
    },
    RenamePane {
        pane: String,
        name: String,
    },
    RenameWorkspace {
        old: String,
        new: String,
    },
    CreateWorkspace {
        name: String,
    },
    KillWorkspace {
        workspace: String,
    },
    ForkWorkspace {
        workspace: String,
        #[serde(default)]
        name: Option<String>,
    },
    SetFocus {
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        workspace: Option<String>,
    },
    /// Live multi-workspace grid streaming for the overview (ctrl+shift+space).
    /// When enabled, non-selected workspaces push at a reduced rate so thumbs
    /// stay live without thrashing the GUI.
    SetOverview {
        enabled: bool,
    },
    /// Force a FULL grid frame for one pane (GUI resync after damage desync).
    RefreshGrid {
        pane: String,
    },
    /// Move a workspace to another GUI window (exclusive ownership).
    TransferWorkspace {
        workspace: String,
        to_window: String,
    },
    /// Pull every workspace into this window (other windows go empty).
    CollectAll,
    AnswerAsk {
        id: String,
        answer: String,
    },
    /// Classic control plane ops from the GUI (status-set, etc.).
    Ctl(ControlRequest),
    Ping,
    /// Window is closing — reassign workspaces and drop this connection.
    Bye,
}

fn default_true() -> bool {
    true
}

/// One GUI window connected to the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: String,
    /// e.g. `cadence +2` or `(empty)`.
    pub label: String,
    pub workspace_count: usize,
}

/// Workspace living on another window (for pull-to-here menus).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForeignWorkspace {
    pub workspace: String,
    pub window_id: String,
    pub window_label: String,
}

/// Daemon → GUI push messages.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum GuiEvent {
    State {
        panes: Vec<PaneInfo>,
        selected_workspace: Option<String>,
        focused_pane: Option<String>,
        extra_workspaces: Vec<String>,
        workspace_order: Vec<String>,
        asks: Vec<AskInfo>,
        statuses: Vec<StatusInfo>,
        /// This GUI connection's window id (multi-window).
        #[serde(default)]
        window_id: Option<String>,
        /// All live windows (for transfer menus). Label = first ws + "+N".
        #[serde(default)]
        windows: Vec<WindowInfo>,
        /// Workspaces owned by *other* windows (empty-sidebar pull menu).
        #[serde(default)]
        foreign_workspaces: Vec<ForeignWorkspace>,
    },
    /// Legacy JSON grid (debug / fallback). Live path prefers [`Self::GridBin`].
    Grid(GridSnapshot),
    /// Compact RLE binary grid (`SCG2` blob, base64). Hot path for paint.
    GridBin {
        pane: String,
        data_b64: String,
    },
    PaneSpawned {
        pane: PaneInfo,
    },
    PaneKilled {
        slug: String,
    },
    PaneExited {
        slug: String,
        exit_code: Option<i32>,
    },
    Ask {
        ask: AskInfo,
    },
    AskResolved {
        id: String,
    },
    Status {
        slug: String,
        state: String,
        note: Option<String>,
    },
    Touch {
        slug: String,
        verb: String,
        actor: String,
    },
    /// Causal attribution: who last wrote stdin to this pane's PTY.
    InputOrigin {
        pane: String,
        /// `human` | `agent:<slug>` | `cli` | `propose` | …
        origin: String,
    },
    /// Co-presence: input ownership / drive mode changed.
    Agency {
        pane: String,
        owner: String,
        drive_mode: String,
        human_idle: bool,
        exited: bool,
        #[serde(default)]
        exit_code: Option<i32>,
    },
    Ghost {
        pane: String,
        ghost: Option<GhostSnap>,
    },
    Error {
        message: String,
    },
    /// Response to a GuiRequest that needs ack (spawn, etc.).
    Ack {
        ok: bool,
        #[serde(default)]
        data: Option<serde_json::Value>,
        #[serde(default)]
        error: Option<String>,
    },
    Pong,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneInfo {
    pub kind: String,
    pub name: String,
    pub slug: String,
    pub workspace: String,
    pub command: String,
    pub cwd: String,
    pub tiled: bool,
    pub running: bool,
    pub title: Option<String>,
    pub scratchpad: String,
    /// For file panes: the path being watched.
    #[serde(default)]
    pub file: Option<String>,
    /// Input owner: `none` | `human` | `agent:<id>` | `cli`.
    #[serde(default)]
    pub owner: Option<String>,
    /// `pair` | `locked_human` | `agent_led`
    #[serde(default)]
    pub drive_mode: Option<String>,
    /// Process exited but pane kept as tombstone.
    #[serde(default)]
    pub exited: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AskInfo {
    pub id: String,
    pub from: String,
    pub workspace: Option<String>,
    pub question: String,
    pub choices: Vec<String>,
    pub answer: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusInfo {
    pub slug: String,
    pub state: String,
    pub note: Option<String>,
    /// Scratchpad revision at last status/note/finish write (0.9.5+).
    #[serde(default)]
    pub pad_rev: u64,
}

/// Pad rev + bytes recorded at last inject — wait uses this for since-inject evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InjectBaseline {
    pub slug: String,
    pub pad_rev: u64,
    pub pad_bytes: u64,
}

/// Dispatch envelope for one inject→finish cycle (0.9.6).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub pane: String,
    pub inject_pad_rev: u64,
    pub inject_pad_bytes: u64,
    /// Full inject text (durable inbox for workers / orchestrators).
    #[serde(default)]
    pub body: String,
    /// open | done | cancelled | orphaned
    #[serde(default = "default_task_open")]
    pub status: String,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
}

fn default_task_open() -> String {
    "open".into()
}

/// Handoff message (old daemon → new) — FDs travel out-of-band via SCM_RIGHTS.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandoffPane {
    pub name: String,
    pub slug: String,
    pub workspace: String,
    pub cwd: String,
    pub command: String,
    pub tiled: bool,
    pub resume_on_restore: bool,
    pub kind: String,
    pub file: Option<String>,
    pub child_pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    /// Master PTY fd index into the SCM_RIGHTS list (terminal panes only).
    pub fd_index: Option<usize>,
    pub title: Option<String>,
    pub text_snapshot: String,
    pub ghost: Option<GhostSnap>,
    /// Co-presence state (0.9.5+). Missing → default agency.
    #[serde(default)]
    pub agency: Option<crate::agency::AgencySnap>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandoffBundle {
    pub panes: Vec<HandoffPane>,
    pub selected_workspace: Option<String>,
    pub focused_pane: Option<String>,
    pub extra_workspaces: Vec<String>,
    pub workspace_order: Vec<String>,
    pub proposal_counter: u64,
    pub ask_counter: u64,
    /// Live badges (0.9.5+) — survive `seance upgrade`.
    #[serde(default)]
    pub statuses: Vec<StatusInfo>,
    /// Unanswered asks (0.9.5+).
    #[serde(default)]
    pub asks: Vec<AskInfo>,
    /// Per-pane pad revision counters.
    #[serde(default)]
    pub pad_revs: Vec<(String, u64)>,
    /// Inject baselines for evidence-bound wait.
    #[serde(default)]
    pub inject_baselines: Vec<InjectBaseline>,
    /// Open/recent tasks (0.9.6).
    #[serde(default)]
    pub tasks: Vec<TaskRecord>,
    #[serde(default)]
    pub task_counter: u64,
    /// pane slug → active task id
    #[serde(default)]
    pub active_tasks: Vec<(String, String)>,
    /// Shell command log (0.9.11 — survive upgrade).
    #[serde(default)]
    pub cmd_log: crate::cmdlog::CommandLog,
}
