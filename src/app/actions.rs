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

/// Move a circle out of this GUI's active band into the parked group
/// (`Unsubscribe`) — nothing happens to any other GUI.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActParkWorkspace(pub String);

/// Sleep a circle: every pane's process exits, the last frame stays readable,
/// and it wakes back onto the same conversations. Daemon-side and global.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActSleepWorkspace(pub String);

/// Wake a sleeping circle (the awaken bar, and the context menu).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActWakeWorkspace(pub String);

/// The inverse: parked → active (`Subscribe`), without selecting it.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActActivateWorkspace(pub String);

/// Pin a circle into the sidebar's top section (implies active/`Subscribe`).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActPinWorkspace(pub String);

/// The inverse: back down into the normal active band, still subscribed.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActUnpinWorkspace(pub String);

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
- `$SEANCE_WORKSPACE` — circle **slug** (`seance ctl` is scoped to it). Stable: \
  renaming a circle changes its label, not this. `ctl whoami` is the authority.
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

/// Open the web replay editor for a workspace (sidebar context menu).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActShareReplay(pub String);

/// Bump workspace recency without selecting it (sidebar context menu).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActTouchWorkspace(pub String);

/// Open the quicklaunch editor pre-filled for the named entry (context menu).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActQuickLaunchEdit(pub String);

/// Remove the named quicklaunch entry and persist (context menu).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActQuickLaunchRemove(pub String);

/// Open one PR link in the browser (header PR chip context menu).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActOpenPrLink(pub String);

/// Drop one PR ref from one circle (sticky dismissal) — PR chip context menu.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActRemovePrLink {
    pub url: String,
    pub workspace: String,
}

/// Drop every PR ref on one circle — PR chip context menu.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = seance, no_json)]
pub struct ActClearPrLinks(pub String);
