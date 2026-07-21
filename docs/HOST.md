# Host bridge (optional sidebar widgets)

Seance can show **host-owned** strips in the left sidebar without linking vita
(or any other app) as a library. The host is a shell command that prints JSON;
seance only paints chips and runs a select command.

## Fail closed

- No `~/.config/seance/host.json` and no default adapter → no strip.
- Poll command missing / non-zero / bad JSON → strip omitted (or last good kept).
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
  ]
}
```

`{id}` in `select_cmd` is replaced with the clicked item’s `id`.

## Poll JSON schema (v1)

Stdout must be a single JSON object (last `{…}` line is accepted if logs leak):

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
| `selected` | current host selection (●) |
| `detail` | first meta line (5h usage + reset) |
| `detail2` | optional second meta line (weekly usage + reset) |

## Select result

`select_cmd` should exit 0 and ideally print:

```json
{"ok":true,"id":"account-2","email":"…","active":"account-2"}
```

Seance toasts success/failure; re-polls immediately.

## First widget: Claude accounts

Adapter: `vita/scripts/seance_host_accounts.py` → `claude_accounts.list_accounts` /
`switch_account` (same store as telegram `claude`).

Switching updates `~/.claude/.credentials.json` so **new** Claude processes use
the account. Running panes keep their existing process identity until restarted.

## Adding another widget

1. Write a poll command that emits schema v1 JSON.
2. Add a second entry under `sidebar` in `host.json`.
3. No seance rebuild required if the UI already handles `items[]`.
