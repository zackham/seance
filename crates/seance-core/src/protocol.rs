//! Wire types for daemon ↔ GUI and connection hello.

use serde::{Deserialize, Serialize};

use crate::control::ControlRequest;
use crate::snapshot::{GhostSnap, GridSnapshot};

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
    ///
    /// `subscriptions` seeds this connection's subscription set (workspaces it
    /// wants grid streams for — there is no ownership):
    /// * `Some(list)` → subscribe to exactly `list ∩ known workspaces`
    ///   (`Some(vec![])` = a deliberately blank window).
    /// * `None` → subscribe to every current workspace (fresh client).
    Attach {
        #[serde(default)]
        selected_workspace: Option<String>,
        #[serde(default)]
        focused_pane: Option<String>,
        #[serde(default)]
        subscriptions: Option<Vec<String>>,
    },
    /// Add `workspace` to this connection's subscription set.
    Subscribe {
        workspace: String,
    },
    /// Drop `workspace` from this connection's subscription set.
    Unsubscribe {
        workspace: String,
    },
    /// Put a whole circle to sleep: every pane's process exits, the last frame
    /// is frozen, identity and claude conversation are kept. Refused unless
    /// every pane in it is restorable.
    SleepWorkspace {
        workspace: String,
    },
    /// Relaunch every sleeping pane in the circle (`claude --resume <id>`).
    WakeWorkspace {
        workspace: String,
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
    /// Kick another GUI window off the daemon (✦ popover "kill"). It drops off
    /// the roster exactly as if it had sent Bye.
    CloseWindow {
        window: String,
    },
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
    /// Window is closing — drop this connection (workspaces are global; nothing
    /// is reassigned).
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
    /// e.g. `cadence +2` or `(empty)` — first subscribed workspace + "+N".
    pub label: String,
    /// Size of that window's subscription set.
    pub workspace_count: usize,
}

/// Daemon → GUI push messages.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum GuiEvent {
    /// Global daemon state. `panes` / `statuses` / `asks` / `extra_workspaces`
    /// / `workspace_order` / `workspace_meta` cover EVERY workspace — the
    /// per-connection view is `subscriptions`, and clients filter locally.
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
        /// All live windows (multiplayer roster). Label = first ws + "+N".
        #[serde(default)]
        windows: Vec<WindowInfo>,
        /// This connection's subscription set, ordered by `workspace_order`
        /// then alpha for the leftovers.
        #[serde(default)]
        subscriptions: Vec<String>,
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
    /// The pane's TUI spinner started or stopped. **Broadcast to every
    /// connection, subscription-blind** — grid frames only reach the window
    /// that has the workspace selected, so a client watching the sidebar has
    /// no other way to learn a circle stopped working. Edge-triggered: only
    /// flips are sent.
    PaneBusy {
        pane: String,
        busy: bool,
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
    /// This window was closed from another GUI (✦ popover). The client must
    /// stop reconnecting and show why — reconnect would just re-register.
    Kicked {
        by: String,
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
    /// Daemon's verdict on "an agent is streaming in this pane right now"
    /// (`util::title_looks_busy` over the live OSC title). Seeds the client's
    /// working badges; kept fresh by [`GuiEvent::PaneBusy`].
    #[serde(default)]
    pub busy: bool,
    /// Asleep: no process, no RAM. The pane keeps its identity and its last
    /// rendered frame (served frozen), and wakes back onto the same claude
    /// conversation. Clients render it greyed with an awaken affordance.
    #[serde(default)]
    pub asleep: bool,
    /// The daemon can put this pane back exactly as it is (a claude pane whose
    /// conversation exists on disk, or a file pane). Clients offer "sleep"
    /// only when every pane in the circle says yes — the check needs the
    /// filesystem, so it can't be made client-side.
    #[serde(default)]
    pub restorable: bool,
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
    /// The circle's stable **slug** — its identity. Minted once at creation
    /// and never rewritten, so anything holding it (a pane's environment, a
    /// client's pin/park prefs, a path on disk) survives a rename.
    pub workspace: String,
    /// Human-facing label. Free text, mutable, and the only thing a rename
    /// changes. Absent means "same as the slug".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Last real pane output (recorder-observed content change).
    #[serde(default)]
    pub last_output_ms: u64,
    /// Last human input (keystroke / inject) into any pane here.
    #[serde(default)]
    pub last_touch_ms: u64,
    /// PR links scraped from this workspace's pane output, most-recently-seen
    /// LAST. Statuses are filled in by an external poller through
    /// `<state_dir>/pr_watch.json` — the daemon only owns the URL list.
    #[serde(default)]
    pub pr_links: Vec<PrLink>,
}

/// One PR URL observed in a workspace's pane output.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PrLink {
    /// Canonical `https://github.com/OWNER/REPO/pull/N` URL.
    pub url: String,
    /// Latest poller verdict, when the watcher has seen this URL.
    #[serde(default)]
    pub status: Option<PrStatus>,
    /// When the URL was last seen in pane output (unix ms).
    #[serde(default)]
    pub seen_ms: u64,
}

/// External-poller view of one PR (written to `pr_watch.json`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PrStatus {
    /// Freeform: `open` | `draft` | `merged` | `closed` | …
    pub state: String,
    /// `needs` (resurface the workspace) | `done` | None.
    #[serde(default)]
    pub attention: Option<String>,
    /// Short chip text, e.g. `CI ✗`, `approved ✓`, `2 comments`.
    #[serde(default)]
    pub label: String,
    /// Poller's last refresh (unix ms).
    #[serde(default)]
    pub updated_ms: u64,
    /// PR is a draft.
    #[serde(default)]
    pub is_draft: bool,
    /// `pass` | `fail` | `running`, or None when the PR has no checks.
    #[serde(default)]
    pub ci: Option<String>,
    /// `required` | `approved` | `changes`, or None.
    #[serde(default)]
    pub review: Option<String>,
    /// PR open time (unix ms; 0 = unknown).
    #[serde(default)]
    pub opened_ms: u64,
    /// Latest review submission (unix ms; 0 = unknown).
    #[serde(default)]
    pub last_review_ms: u64,
    /// Latest comment (unix ms; 0 = unknown).
    #[serde(default)]
    pub last_comment_ms: u64,
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

    /// Attach's subscription seed is three-valued: absent/null = "everything",
    /// `[]` = a deliberately blank window, a list = exactly those.
    #[test]
    fn attach_subscription_seed_is_three_valued() {
        let missing: GuiRequest = serde_json::from_str(r#"{"op":"attach"}"#).unwrap();
        let empty: GuiRequest =
            serde_json::from_str(r#"{"op":"attach","subscriptions":[]}"#).unwrap();
        let listed: GuiRequest =
            serde_json::from_str(r#"{"op":"attach","subscriptions":["lab"]}"#).unwrap();
        let seed = |r: &GuiRequest| match r {
            GuiRequest::Attach { subscriptions, .. } => subscriptions.clone(),
            _ => panic!("expected Attach"),
        };
        assert_eq!(seed(&missing), None);
        assert_eq!(seed(&empty), Some(vec![]));
        assert_eq!(seed(&listed), Some(vec!["lab".to_string()]));
    }

    #[test]
    fn subscribe_ops_roundtrip() {
        let json = serde_json::to_string(&GuiRequest::Subscribe {
            workspace: "lab".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"op":"subscribe","workspace":"lab"}"#);
        let back: GuiRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, GuiRequest::Subscribe { workspace } if workspace == "lab"));
        let back: GuiRequest =
            serde_json::from_str(r#"{"op":"unsubscribe","workspace":"lab"}"#).unwrap();
        assert!(matches!(back, GuiRequest::Unsubscribe { workspace } if workspace == "lab"));
    }

    /// `subscriptions` defaults to empty on a payload that predates it.
    #[test]
    fn state_without_subscriptions_parses() {
        let ev: GuiEvent = serde_json::from_str(
            r#"{"event":"state","panes":[],"selected_workspace":null,
                "focused_pane":null,"extra_workspaces":[],
                "workspace_order":[],"asks":[],"statuses":[]}"#,
        )
        .unwrap();
        match ev {
            GuiEvent::State { subscriptions, .. } => assert!(subscriptions.is_empty()),
            _ => panic!("expected State"),
        }
    }

    /// `pr_links` is additive: a 0.12 payload (and a 0.12 client) still parse.
    #[test]
    fn workspace_meta_pr_links_are_optional_and_roundtrip() {
        let old: WorkspaceMeta =
            serde_json::from_str(r#"{"workspace":"lab","last_output_ms":1,"last_touch_ms":2}"#)
                .unwrap();
        assert!(old.pr_links.is_empty());

        let meta = WorkspaceMeta {
            workspace: "lab".into(),
            last_output_ms: 1,
            last_touch_ms: 2,
            pr_links: vec![PrLink {
                url: "https://github.com/o/r/pull/3".into(),
                status: Some(PrStatus {
                    state: "open".into(),
                    attention: Some("needs".into()),
                    label: "CI x".into(),
                    updated_ms: 9,
                    is_draft: true,
                    ci: Some("running".into()),
                    review: Some("changes".into()),
                    opened_ms: 100,
                    last_review_ms: 200,
                    last_comment_ms: 300,
                }),
                seen_ms: 5,
            }],
        };
        let back: WorkspaceMeta =
            serde_json::from_str(&serde_json::to_string(&meta).unwrap()).unwrap();
        assert_eq!(back.pr_links, meta.pr_links);
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
