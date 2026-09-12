---
name: seance-control
description: Manage seance circles and agent sessions through the local CLI. Use to find ongoing work, read results, send follow-ups, or delegate tasks in seance. Does not apply merely because the user is editing the seance source code.
---

# seance control

Use `seance ctl` from the local shell. On macOS it follows the app's saved
connection even when the GUI is closed. In ChatGPT, this requires a task running
with **work locally**. The CLI handles the connection; no host-specific commands
belong in the workflow.

Read `seance ctl skill` once for the live engagement contract; use `seance ctl help`
for command syntax. That contract covers workers inside panes too: you are an
outside orchestrator unless `SEANCE_SESSION` is set. An explicit `SEANCE_SOCKET`
or in-pane `SEANCE_SESSION` takes precedence over the saved connection.
`seance ctl --local ...` deliberately bypasses the saved remote; it is not a
connection-repair fallback.

If `seance` is absent from PATH, check `~/.local/bin/seance` or the installed
`/Applications/Seance.app/Contents/MacOS/seance` before reporting it unavailable.

## find the work

Start with `seance ctl roster --json`. Use `--all` for a requested overview or
search across circles; once identified, use `--scope CIRCLE_SLUG`.
The JSON response contains `data.panes`: match the topic using `workspace_name`,
`name`, `title`, and `cwd`, then retain `workspace` and `slug` as stable ids.
Labels can change, and several panes can share a display name.

Read the relevant context before steering:

```bash
seance ctl task PANE_SLUG --json
seance ctl pad PANE_SLUG --cat
seance ctl read PANE_SLUG --lines 80
```

Use the task and pad for durable assignments and answers; the rendered terminal
helps explain present activity or a blocking prompt. A badge, title, sleeping
state, or old pad alone does not establish that the current request is finished.

## send and retrieve

Check `kind` and `command` before sending: agent panes accept instructions,
shell panes execute submitted text, and file panes have no input terminal.
Prefer the existing relevant agent. For a requested new worker, use
`seance ctl doctor` to inspect available profiles, then
`seance ctl new --name NAME --workspace CIRCLE --cwd DAEMON_DIR --agent PROFILE --wait-ready`.
Use the slug printed after `created`; a name collision can add a suffix.
Do not combine `--wait-ready` with `--json`, which currently returns before waiting.

Send the actual task, its scope, and what completion should contain. Preserve
literal text through stdin or a local file:

```bash
seance ctl send PANE_SLUG --stdin --json <<'SEANCE_TASK'
Summarize the current result and remaining blocker for the assignment we discussed.
Read seance ctl task for this assignment's task id. Complete using seance ctl finish
with that task id, --status done, and your answer supplied through --stdin.
SEANCE_TASK
```

Retain the returned `task_id`. Retrieve that dispatch's answer with:

```bash
seance ctl wait PANE_SLUG --task TASK_ID --status done --timeout 30 --cat
```

This requires completion evidence after injection. Keep bounded waits in a
background task when the client supports it so voice conversation can continue.
On timeout, report what is still running and keep the task id for a later check.
Do not poll terminal screens in a sleep loop, use `--badge-only` to manufacture
success, or claim spoken updates were delivered without observing that delivery.
If a send's outcome is uncertain, inspect `task` before retrying it.

## files and shared control

`send`, `note`, and `finish --file PATH` read a **local** file and transfer its
contents. `pad --cat` retrieves remote notes directly. Conversely, `new --cwd`,
`new --file`, and `wait --artifact` refer to paths on the **daemon's machine**;
the laptop's `$PWD` and returned scratchpad paths are not interchangeable.
Use `new --file` for a requested live document viewer.

`select`/`focus` changes the selected pane in **every attached GUI window**;
use it only when asked to show or switch to that pane. Respect active human input;
do not add `--force`, seize control, interrupt, or kill a pane just to get past a
blocked operation. Manage only the sessions and work the user authorized.
