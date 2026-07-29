# seance daemon architecture

Sessions (PTYs + terminal grids) live in a long-lived **daemon**. The GPUI
window is a disposable client. That is the use→develop→use loop.

## Processes

| process | owns | dies when |
|---------|------|-----------|
| `seance daemon` | PTYs, alacritty `Term` grids, control plane, pane metadata | rare; graceful upgrade keeps sessions |
| `seance` (GUI) | window, chrome, notes flip, rendering | any rebuild — reconnects |
| `seance ctl` | nothing (client) | n/a |

## Sockets

- **Control / GUI:** `$XDG_RUNTIME_DIR/seance.sock` (same path as before).
  First line of each connection is a hello:
  - `{"role":"ctl"}` — classic JSON-lines request/response (`seance ctl`)
  - `{"role":"gui"}` — bidirectional GUI protocol (snapshots + input)
  - `{"role":"handoff"}` — daemon upgrade only
- Override: `SEANCE_SOCKET` or `SEANCE_STATE_DIR` (state dir also moves data).

## Phases

### A — daemon split
GUI and ctl talk to the daemon over the socket. Daemon owns every terminal
pane. GUI may exit; sessions keep running.

### B — reconnect
GUI on launch attaches (`gui` role), receives full pane list + grid
snapshots, streams damage thereafter. Crash/restart GUI mid-session is fine.

The GUI client **auto-reconnects** if the socket drops (daemon upgrade,
brief blip). On each reconnect it re-sends `Attach` and re-registers for
broadcasts — required so `seance ctl new` from an agent *outside* seance
shows up in the open window without a full restart. Without this, the
daemon had the pane and `state.json` was correct, but the live GUI was
still subscribed to a dead connection.

### C — graceful daemon upgrade
`seance daemon upgrade` (or auto when GUI starts a newer binary):

1. Spawn new daemon with `--takeover <handoff-sock>`.
2. Old daemon shuts down I/O threads without SIGHUP: each PTY I/O thread
   transfers the master FD via `into_raw_fd` (no close), signals release;
   `prepare_handoff` takes that FD once (no concurrent close/dup race).
3. Pass per-pane: metadata, grid snapshot, master FD via `SCM_RIGHTS`, child pid.
4. New daemon **adopts** FDs only — it does **not** respawn a shell if handoff
   fails (respawn used to hide dead children and look like “only claude survived”).
5. Old process exits. Children never saw SIGHUP when step 2 succeeded.

If you see a shell die across upgrade while an agent pane lives, check
`~/.local/share/seance/daemon-upgrade.log` for `handoff prepare failed` /
`not respawning` lines — that is a failed FD transfer, not intentional.

## Wire (GUI, summary)

Client → daemon:
- `attach` — full state dump
- `input { pane, bytes_b64 }`
- `resize { pane, cols, rows, cell_w, cell_h }`
- `scroll { pane, delta }` / `scroll_bottom`
- `inject { pane, text, submit }`
- `spawn` / `kill` / layout ops (also available via ctl)
- `ghost_accept` / `ghost_reject`

Daemon → client (push):
- `state` — full pane list + workspace chrome
- `grid { pane, rev, cols, rows, cells…, cursor, title, ghost? }`
- `pane_spawned` / `pane_killed` (process exit auto-kills the pane)
- `ask` / `status` / `touch` events

## Layout on disk

Unchanged paths under `~/.local/share/seance/` (or `SEANCE_STATE_DIR`):
state.json, scratch/, events.jsonl, plus `daemon.pid` for the live daemon,
and **`gui.stderr.log`** — durable GUI stderr (panics/backtraces). Rotates to
`gui.stderr.log.1` above ~5 MiB. After a mysterious window death, `tail` that
file first.

## Dev loop — DO NOT hard-kill the daemon

**PTYs live in the daemon.** Killing the daemon process kills every agent
session. GUI death is free; daemon death is not.

```bash
cargo build --release

# GUI chrome only (flip/help/render) — sessions LIVE:
#   close the window, or kill the non-daemon process only, then:
seance                         # reconnects to existing daemon

# Runtime / PTY / protocol / colors changes — sessions LIVE:
seance upgrade                 # graceful binary handoff (SCM_RIGHTS)
# alias: seance reload

# NEVER for routine restarts:
#   pkill -x seance            # kills daemon AND gui → all sessions die
#   kill <daemon-pid>          # same
```

How to tell them apart:

```
pgrep -ax seance
# .../seance daemon            ← owns sessions, leave alone
# .../seance                   ← GUI only, safe to kill
```

Default: starting `seance` ensures a daemon is running (spawns one if missing).
