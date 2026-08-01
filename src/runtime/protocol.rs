//! Wire types for daemon ↔ GUI and connection hello.
//!
//! The client-facing wire (Hello, GuiRequest/GuiEvent, FsOp, pane/ask/status
//! info) lives in `seance-core` — shared verbatim with the web client. This
//! module re-exports it and keeps the native-only handoff types (they carry
//! CommandLog/AgencySnap, which are daemon-internal).

use serde::{Deserialize, Serialize};

use super::snapshot::GhostSnap;

pub use seance_core::protocol::*;

/// The hello line every same-version client sends (role = "ctl" or "gui").
pub fn hello_line(role: &str) -> String {
    seance_core::protocol::hello_line_with(role, env!("CARGO_PKG_VERSION"))
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
    /// workspace → last real pane output (unix ms). Daemon-owned activity
    /// clock; absent on older daemons → clocks start unknown, not zero.
    #[serde(default)]
    pub workspace_output: Vec<(String, u64)>,
    /// workspace → last human input (unix ms).
    #[serde(default)]
    pub workspace_touch_ms: Vec<(String, u64)>,
}
