//! Sidebar context-menu actions (menu items dispatch gpui actions) and the
//! one-click "arm" prompt injected into a fresh agent pane.

use gpui::Action;
use serde::Deserialize;

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
pub(crate) const SEANCE_ARM_PROMPT: &str = "\
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

Confirm you're oriented and ready, then wait for the next instruction.";

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
