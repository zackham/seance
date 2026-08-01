//! Wire types for daemon ↔ GUI and connection hello.

use serde::{Deserialize, Serialize};

use crate::snapshot::{GhostSnap, GridSnapshot};
use crate::control::ControlRequest;

/// First line on every socket connection.
#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub role: String,
    /// Optional protocol version.
    #[serde(default)]
    pub v: Option<u32>,
    /// Client build version (`CARGO_PKG_VERSION`). The daemon enforces an
    /// exact match for `ctl` / `gui` roles — with thin clients on other
    /// machines, silent version skew is a protocol-corruption hazard, so
    /// mismatch (or absence) fails loudly. `handoff` / `upgrade` roles are
    /// exempt: upgrades cross versions by design.
    #[serde(default)]
    pub build: Option<String>,
}

/// The hello line a same-version client sends (role = "ctl" or "gui").
/// `build` must be the seance workspace version — all crates inherit
/// `workspace.package.version`, so pass your own `env!("CARGO_PKG_VERSION")`.
pub fn hello_line_with(role: &str, build: &str) -> String {
    format!("{{\"role\":\"{role}\",\"build\":\"{build}\"}}\n")
}

#[cfg(test)]
mod hello_tests {
    use super::*;

    #[test]
    fn hello_line_roundtrips_with_build() {
        let line = hello_line_with("ctl", env!("CARGO_PKG_VERSION"));
        let hello: Hello = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(hello.role, "ctl");
        assert_eq!(hello.build.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn legacy_hello_without_build_parses_as_absent() {
        let hello: Hello = serde_json::from_str(r#"{"role":"gui"}"#).unwrap();
        assert_eq!(hello.role, "gui");
        assert!(hello.build.is_none());
    }
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
    /// Daemon-side filesystem/config bridge (thin client: file panes, pads,
    /// layout, host widgets all live on the daemon's machine). Correlated by
    /// `id`; the reply is a [`GuiEvent::FsResult`] with the same id. Executed
    /// off the request loop so slow ops never stall input.
    Fs {
        id: u64,
        #[serde(flatten)]
        fs: FsOp,
    },
    /// Fire-and-forget event-log write (human UI actions). Thin clients must
    /// land these in the DAEMON's flight recorder, not a local file — agents
    /// watch that timeline.
    Event {
        actor: String,
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default)]
        pane: Option<String>,
        kind: String,
        detail: String,
    },
    Ping,
    /// Window is closing — reassign workspaces and drop this connection.
    Bye,
}

/// Operations served by the daemon fs bridge. Paths are daemon-machine paths
/// (same trust domain as the daemon itself — the control plane can already
/// spawn arbitrary commands).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "fs_op", rename_all = "snake_case")]
pub enum FsOp {
    /// → `{contents_b64, mtime_ms}` or error when unreadable.
    Read {
        path: String,
    },
    /// Atomic write. → `{mtime_ms}`.
    Write {
        path: String,
        contents_b64: String,
    },
    /// → `{exists, mtime_ms, size}` (`exists:false` is ok, not an error).
    Stat {
        path: String,
    },
    /// → `{entries: [{name, is_dir}]}`.
    List {
        path: String,
    },
    Remove {
        path: String,
    },
    /// Shared GUI layout (split/weights), persisted in the daemon state dir so
    /// every attached window — local or thin client — sees the same tiling.
    /// → `{json: string|null}`.
    LayoutLoad,
    LayoutSave {
        json: String,
    },
    /// Run a host widget's select command daemon-side. → `{output}` and a
    /// refreshed [`GuiEvent::HostWidgets`] broadcast.
    HostSelect {
        widget: String,
        item: String,
    },
    /// Run a shell command daemon-side (`sh -lc`). Same trust domain as the
    /// control plane (which already spawns arbitrary commands). Output is
    /// truncated to 64KiB per stream. → `{status, stdout, stderr}`.
    Shell {
        cmd: String,
    },
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
        /// Daemon-owned per-workspace activity clocks (0.11+). Absent on
        /// older daemons — clients then keep their purely local stamps.
        #[serde(default)]
        workspace_meta: Vec<WorkspaceMeta>,
    },
    /// Incremental push of the daemon-owned output clock for one workspace.
    /// Emitted by the recorder tap (real content change), throttled per pane.
    Activity {
        workspace: String,
        last_output_ms: u64,
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
    /// Reply to [`GuiRequest::Fs`], correlated by id. Routed to the waiting
    /// `fs_call` in the gui client, never into the app event stream.
    FsResult {
        id: u64,
        ok: bool,
        #[serde(default)]
        data: Option<serde_json::Value>,
        #[serde(default)]
        error: Option<String>,
    },
    /// Host sidebar widgets, polled daemon-side and pushed on attach + every
    /// poll tick (`Vec<HostWidgetSnap>` as JSON).
    HostWidgets {
        widgets: serde_json::Value,
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

/// Daemon-owned activity clocks for one workspace (unix ms, 0 = never).
///
/// These live in the daemon so they survive GUI relaunch, workspace pulls
/// between windows, and `seance upgrade` — the clients are mirrors.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub workspace: String,
    /// Last real pane output (recorder-observed content change).
    #[serde(default)]
    pub last_output_ms: u64,
    /// Last human input (keystroke / inject) into any pane here.
    #[serde(default)]
    pub last_touch_ms: u64,
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

#[cfg(test)]
mod workspace_meta_tests {
    use super::*;

    /// Old daemons/recordings have no `workspace_meta` — must still parse.
    #[test]
    fn legacy_state_without_workspace_meta_parses() {
        let ev: GuiEvent = serde_json::from_str(
            r#"{"event":"state","panes":[],"selected_workspace":"lab",
                "focused_pane":null,"extra_workspaces":[],
                "workspace_order":["lab"],"asks":[],"statuses":[]}"#,
        )
        .unwrap();
        match ev {
            GuiEvent::State { workspace_meta, .. } => assert!(workspace_meta.is_empty()),
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn workspace_meta_roundtrips_and_defaults_missing_clocks() {
        let ev: GuiEvent = serde_json::from_str(
            r#"{"event":"state","panes":[],"selected_workspace":null,
                "focused_pane":null,"extra_workspaces":[],
                "workspace_order":[],"asks":[],"statuses":[],
                "workspace_meta":[{"workspace":"lab","last_output_ms":7},
                                  {"workspace":"main"}]}"#,
        )
        .unwrap();
        let GuiEvent::State { workspace_meta, .. } = ev else {
            panic!("expected State");
        };
        assert_eq!(workspace_meta.len(), 2);
        assert_eq!(workspace_meta[0].last_output_ms, 7);
        assert_eq!(workspace_meta[0].last_touch_ms, 0);
        assert_eq!(workspace_meta[1].workspace, "main");
        assert_eq!(workspace_meta[1].last_output_ms, 0);
    }

    #[test]
    fn activity_event_roundtrips() {
        let json = serde_json::to_string(&GuiEvent::Activity {
            workspace: "lab".into(),
            last_output_ms: 42,
        })
        .unwrap();
        let back: GuiEvent = serde_json::from_str(&json).unwrap();
        match back {
            GuiEvent::Activity {
                workspace,
                last_output_ms,
            } => {
                assert_eq!(workspace, "lab");
                assert_eq!(last_output_ms, 42);
            }
            _ => panic!("expected Activity"),
        }
    }
}
