# Host bridge (optional sidebar widgets + launch menus)

Seance can show **host-owned** surfaces without linking vita (or any other app)
as a library. The host is a shell command that prints JSON; seance only paints
what it printed and runs a select command.

Two shapes, one config file and one JSON schema:

| | `sidebar[]` widgets | `menus[]` |
|---|---|---|
| when it runs | polled on a clock (`poll_secs`) | on click, never polled |
| how it draws | every item is a permanent chip in the rail | one chip; items drop into a dropdown |
| for | small ambient state that must always be true | a question with a long or slow answer |
| who runs it | the daemon (broadcast to every window) | the clicking window, over the fs bridge |

Rule of thumb: if the list has more than a handful of rows, or costs more than
a moment to produce, or you only want it occasionally — it's a menu.

## Fail closed

- No `~/.config/seance/host.json` and no default adapter → no strip, no menus.
- Poll command missing / non-zero / bad JSON → strip omitted (or last good kept).
- `list_cmd` failing → the dropdown says why, in red, and stays open.
- Core seance never depends on host success for panes / daemon / ctl.

## Config

`~/.config/seance/host.json` (auto-seeded on first GUI launch if
`~/work/vita/scripts/seance_host_accounts.py` exists):

```json
{
  "sidebar": [
    {
      "id": "claude-accounts",
      "title": "claude",
      "poll_secs": 20,
      "poll_cmd": "python3 ~/work/vita/scripts/seance_host_accounts.py list",
      "select_cmd": "python3 ~/work/vita/scripts/seance_host_accounts.py select {id}"
    }
  ],
  "menus": [
    {
      "id": "meetings",
      "title": "meeting",
      "list_cmd": "python3 ~/work/vita/scripts/seance_host_meetings.py list",
      "select_cmd": "python3 ~/work/vita/scripts/seance_host_meetings.py select {id}",
      "empty": "no meetings in the next 7 days"
    }
  ]
}
```

Both commands run through `sh -lc` **on the daemon machine** (tilde expands
there, not on a thin client). The file is mtime-watched: an edit shows up in
open windows within a couple of seconds, no restart.

`{id}` in `select_cmd` is replaced with the clicked item's `id`, **unquoted**.
Item ids must therefore be shell-safe — `[A-Za-z0-9._:/@+-=]`, ≤256 chars. An
item whose id isn't is dropped before it can be drawn. Put the human text in
`label`; the id is a handle (`mtg:2026-08-06/web-fe-sync`, `account-3`).

## JSON schema (v1) — both shapes

Stdout should be one JSON object. Seance parses the whole of stdout first and
only falls back to scanning for the last `{…}` line if that fails, so pretty
printing is fine and leaked log lines are survivable — but a command that
prints nothing but JSON is the one that can't be misread.

```json
{
  "schema": 1,
  "id": "claude-accounts",
  "title": "claude",
  "kind": "accounts",
  "items": [
    {
      "id": "account-3",
      "label": "zack@ridewithgps.com",
      "state": "ok",
      "detail": "4% 5h · ↻3:00pm",
      "detail2": "87% wk · ↻thu 2pm",
      "selected": true
    }
  ],
  "active": "account-3"
}
```

| field | meaning |
|-------|---------|
| `state` | `ok` · `warm` · `busy` · `auth` (color only) |
| `selected` | current host selection (●) — sidebar widgets |
| `detail` | first meta line |
| `detail2` | optional second meta line |
| `group` | **menus only**: heading this item sits under |

Menu items are drawn in the order the host printed them, and `group` is a
**run**, not a bucket: a heading is drawn wherever the group changes. Seance
never reorders a host's list, so a host that wants clean groups emits its rows
already grouped.

## Select result

`select_cmd` should exit 0. Anything it prints is optional detail:

```json
{"ok": true, "message": "mtg-web-fe — arming claude-7", "workspace": "mtg-web-fe"}
```

| field | effect |
|-------|--------|
| `ok: false` | treated as failure even on exit 0; `error` is the toast |
| `error` | failure text (falls back to the command's stderr) |
| `message` | success toast (defaults to the item's label) |
| `workspace` | **menus only**: seance pins and selects this circle — how a host that just spawned one gets the rail to follow |
| `pin` | **menus only**: `false` to land the circle in the normal band instead of pinned (default is pinned) |

A non-zero exit, or `ok:false`, keeps the dropdown open with the reason under
it, so the human can read it and pick again.

Sidebar widgets re-poll immediately after a select. Menus don't: the dropdown
closes on success, and reopening asks again.

## Widget: Claude accounts

Adapter: `vita/scripts/seance_host_accounts.py` → `claude_accounts.list_accounts` /
`switch_account` (same store as telegram `claude`).

Switching updates `~/.claude/.credentials.json` so **new** Claude processes use
the account. Running panes keep their existing process identity until restarted.

## Menu: upcoming meetings

Adapter: `vita/scripts/seance_host_meetings.py` → `vita.meetings.week`.

`list` prints the next 7 days of real meetings grouped by day. `select` opens a
circle for the chosen one — a claude pane in the vita repo — and hands back the
circle's slug so the rail jumps to it. The arming (waiting for the agent's TUI,
then injecting the meeting's agenda prompt) happens in a detached child, so the
click returns instantly and the human watches the pane come up and get its
instructions.

Note what seance does **not** know here: what a meeting is, what an agenda is,
that vita exists. It ran two commands. The entire workflow — the circle's name,
the prompt, the file pane the agent is told to open beside itself — is the
host's, expressed through `seance ctl`. That is the seam: a host can add a
workflow to seance without seance learning the workflow.

## Adding another widget or menu

1. Write a command that emits schema v1 JSON on stdout.
2. Add an entry under `sidebar` (polled) or `menus` (on click) in `host.json`.
3. No seance rebuild, no restart — the config is mtime-watched.
