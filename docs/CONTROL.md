# seance — control plane

The control plane is how **anyone in the circle** (agent, shell script, or
human at a keyboard outside the GUI) engages the shared space.

A pane — any agent CLI or shell — can spawn siblings, type into them, read
their screens, ask the human a blocking question, or propose a command as
ghost text. The human watches every terminal live. Visibility is the point;
the protocol exists so agents don’t work offstage.

The technique for injecting prompts is borrowed from the `decap` project:
**bracketed paste** (`\x1b[200~` … `\x1b[201~`) followed by a carriage return to
submit. TUI agents treat a bracketed-paste block as one atomic paste, so a
multi-line prompt lands intact instead of triggering per-line submits.

Two surfaces expose the same protocol:

- a **Unix-socket server** the app runs (`src/control.rs`),
- a **CLI client**, `seance ctl …` (`src/ctl.rs`), which a master session
  shells out to.

---

## Transport

- **Socket:** `$XDG_RUNTIME_DIR/seance.sock`, falling back to
  `/tmp/seance-$UID.sock` when `XDG_RUNTIME_DIR` is unset.
- **Wire format:** **JSON lines.** One request JSON object per `\n`-terminated
  line in; one response JSON object per line out. A connection may stay open and
  carry many request/response pairs in order — a master session can keep one
  socket and pipeline `send`/`read` without reconnecting.
- **Concurrency:** the server accepts on one thread and handles each connection
  on its own thread (blocking IO, no async runtime). Each request is forwarded
  onto the gpui main loop and answered from there, with a **10-second timeout**
  per request — a wedged main loop yields an `ok: false` error, never a hang.
- **Single instance:** on startup the server try-connects to an existing socket
  file. If a live server answers, it refuses to start (two seances would fight
  over the socket); a stale file left by a crash is removed and rebound.

### Request

Every request is a JSON object tagged by an `op` field (snake_case):

| op | fields | meaning |
|----|--------|---------|
| `list` | — | list every session with status |
| `new` | `name` (req), `cwd?`, `command?`, `workspace?` | spawn a session |
| `send` | `session`, `text`, `submit?`=`true` | paste `text`, optionally submit |
| `send_raw` | `session`, `bytes_b64` | inject raw bytes (base64), no wrap, no submit |
| `read` | `session`, `lines?` | rendered visible screen (tail N lines if given) |
| `status` | `session` | one session's metadata |
| `kill` | `session` | terminate the session |
| `scratchpad` | `session` | path to the session's shared scratchpad file |

`session` is a session name/slug (the app resolves either).

### Response

```json
{ "ok": true,  "data": <any> }
{ "ok": false, "error": "human-readable message" }
```

`data` is omitted on bare successes (e.g. `kill`); `error` is omitted on
success. Response payloads by op (best-effort shapes the app fills in):

| op | `data` on success |
|----|-------------------|
| `list` | array of session objects: `{name, workspace?, command?, running, title?}` |
| `new` | `{name, slug}` (or the slug string) of the created session |
| `send` | omitted / `null` |
| `send_raw` | omitted / `null` |
| `read` | the screen text — a string, or `{screen: "..."}` |
| `status` | `{name, workspace?, command, running, title?}` |
| `kill` | omitted / `null` |
| `scratchpad` | the file path — a string, or `{path: "..."}` |

---

## Workspace scoping (v0.3)

Every op carries an optional `scope` field naming a workspace. When set, the
op only sees/affects panes in that workspace:

- `list` filters to the scoped workspace; the response echoes `"scope"`.
- `new` spawns into the scoped workspace by default and refuses `--workspace`
  values outside it.
- All pane-targeting ops (`send`/`send-raw`/`read`/`status`/`kill`/
  `scratchpad`) error with a clear message if the pane is outside scope.

**The CLI fills `scope` automatically from `$SEANCE_WORKSPACE`** — so a ctl
run *inside* a seance pane is confined to its own workspace. A master agent
in workspace `lab` can only drive `lab` panes. Overrides: `--all` lifts the
scope for one call; `--scope WS` targets another workspace explicitly.
Callers outside seance (no env var) are unscoped and see everything.

Naming note: panes were called "sessions" in v0.1; the wire protocol accepts
both `pane` and `session` keys for pane ids. Terminals are the first pane
kind (`"kind": "terminal"` in `list`/`status`) — the protocol is
kind-agnostic so markdown/graph/etc panes can land without breaking clients.

## v0.7 additions (the transparency release)

- Every op now carries `from` (the calling pane's slug, auto-filled from
  `$SEANCE_SESSION`) — actions are **attributed**: `human` / `agent:<pane>` /
  `cli` in one event log at `~/.local/share/seance/events.jsonl`.
- `timeline` — query the log (`since_secs`, `pane`, `actor`, `limit`).
- `status_set` — agent self-reported status; badge on the pane + colored
  sidebar dot (planning|working|blocked|needs-human|done|idle).
- `ask` / `ask_result` — agent asks the human; a toast with choice buttons
  appears above the tiling region; the CLI blocks and prints the answer.
- Cross-pane `send`/`read` flash "⚡ driven by X" / "👁 observed by X" on the
  target pane strip for ~5s. The activity drawer (`≋` in the footer) shows
  the live event feed.

## Semantics

### `send` — the driving primitive

`send` with `submit: true` (the default):

1. the app **bracketed-pastes** `text` into the session's terminal
   (`\x1b[200~` + text + `\x1b[201~`) — the paste wrapping happens app-side, you
   just supply the raw text,
2. waits a **~150 ms settle delay**, then
3. sends a **carriage return** (`\r`) to submit.

The settle delay matters: TUI agents (claude / codex / grok CLIs) need a beat to
finish processing the paste before the Enter keystroke lands, or the submit
races the paste and gets eaten. With `submit: false` the text is left sitting in
the input unsent — useful for staging a prompt you'll submit later, or for
composing across several `send` calls.

### `send_raw` — the escape hatch

`send_raw` writes raw bytes straight to the PTY: no paste wrapping, no settle
delay, no submit. It's for control characters and key sequences the paste path
can't express — Ctrl-C to interrupt a running command (`0x03`), a bare Enter
(`0x0d`), arrow keys, Escape. Bytes are base64-encoded on the wire; the CLI
encodes for you (pass a shell-escaped string).

### `read` — the observing primitive

`read` returns the **rendered visible screen** — the exact text a human sees in
the pane, not the raw PTY byte stream. With `lines: N` it returns the tail N
lines (reaching into scrollback as needed); omitted, it returns the full visible
screen. This is how a master session watches a worker: poll `read` until the
screen shows the worker is idle / awaiting input, then act.

Because `read` is a screen snapshot, expect prompt boxes, spinners, and partial
frames. Poll on an interval and look for a stable idle marker (an empty prompt,
a `>` cursor, "esc to interrupt" gone) rather than parsing mid-render frames.

### `scratchpad` — the durable side-channel

Each session has a markdown scratchpad at
`~/.local/share/seance/scratch/<slug>.md`, surfaced live in the app's drawer.
The agent *inside* a session sees its own path via `$SEANCE_SCRATCHPAD`. A
master reads/writes a worker's scratchpad by path (from `scratchpad`) to hand
off durable instructions and collect durable results — notes that outlive any
single screen and that the human can watch update in real time.

### Session environment

Spawned sessions get two env vars so the agent inside knows it's under seance:

- `SEANCE_SESSION` — the session's slug/id,
- `SEANCE_SCRATCHPAD` — absolute path to its own scratchpad file.

---

## CLI reference — `seance ctl`

```
seance ctl <command> [args] [--json]
```

`--json` (accepted anywhere) prints the raw JSON response instead of the
human-readable rendering. Exit codes: **0** ok · **1** request failed · **2**
cannot connect (with an "is seance running?" hint).

| command | usage |
|---------|-------|
| `list` | `seance ctl list` |
| `new` | `seance ctl new --name NAME [--cwd DIR] [--agent NAME\|--command CMD] [--workspace WS] [--wait-ready]` |
| `send` | `seance ctl send PANE TEXT...` `[--file PATH\|--stdin] [--no-submit] [--force]` → `task_id` |
| `send-raw` | `seance ctl send-raw PANE BYTES` |
| `read` | `seance ctl read PANE [--lines N]` (debug) |
| `status` / `kill` | `seance ctl status\|kill PANE` |
| `scratchpad` / `pad` | `seance ctl pad [PANE] [--cat]` (default: `$SEANCE_SESSION`) |
| `note` | `seance ctl note [PANE] TEXT...` `[--file PATH] [--replace]` |
| `finish` | `seance ctl finish [PANE] [--file\|--stdin] [--status done] [--note N] [--task ID]` |
| `task` / `inbox` | durable inject body (`--id` or self) |
| `roster` / `stage` / `brief` | dense stage projection |
| `wait` | `wait PANE… --status done [--cat\|--harvest] [--badge-only] …` |
| `harvest` | alias: `wait … --status done --cat` |
| `whoami` | principal + session + active `task_id` |
| `doctor` / `skill` / `help` | profiles + agent contract |

Notes:

- **`send`**: shell expands `$VARS` — use `--file`/`--stdin`. Inject creates a
  **task envelope** (`task_id`), sets `status=working`, records pad baseline.
  Sidecars: `<scratch>.taskid` / `<scratch>.task.json`.
- **`wait --status done`**: evidence-bound (pad must grow since inject) unless
  `--badge-only`. Prints `done …` (not `ready`) when waiting on done.
- **`--cat` / `harvest`**: after success, print each pane's pad body (fan-in).
- **`finish`**: pad body + status + task close; `done` requires body.
- **Roster** prefers **slug** when name≠slug (ctl needs slug).

### Examples

```bash
seance ctl new --name build --cwd ~/proj --agent claude --wait-ready
seance ctl send build --file /tmp/task.md          # task=task-N
seance ctl wait build --status done --timeout 600 --cat
seance ctl harvest w1 w2 w3 --timeout 900
seance ctl task                                    # inside a worker
seance ctl finish --stdin --status done <<'EOF'
answer
EOF
seance ctl roster
```

---

## Agent skill (engagement protocol)

**Canonical source: `seance ctl skill`** — ships in the binary so it cannot
drift from the implementation. That text is the product contract for how
agents engage the human and the circle (status, ask, propose, scratchpad,
read-before-assume). Prefer the command over any doc snapshot.

Three ways to arm an agent (any CLI with shell access — not vendor-specific):

1. Tell it: *run `seance ctl skill` and follow those instructions*
2. Inject: `seance ctl send PANE "$(seance ctl skill)"` (or the ⚡ arm control)
3. Paste the output into whatever system/context file that agent reads

Default `new` pane command is a **shell** (human can always take the keyboard).
Pass `--command claude` / `codex` / `grok` / … for an agent worker.

---

## Foundation 0.9.1 — event bus, watch, capabilities

### Event bus

Every meaningful action publishes an `Event` with:

| field | meaning |
|-------|---------|
| `id` / `seq` | stable id + monotonic sequence |
| `actor` | `human` / `agent:<slug>` / `cli` / `daemon` / `system` |
| `kind` | machine-readable (`ctl_send`, `cmd_start`, `focus`, …) |
| `origin` | optional provenance (`ctl_send`, `human_keystroke`, `propose_accepted`, …) |
| `caused_by` / `span` | causal chain + span id (e.g. command runs) |

Durable: `~/.local/share/seance/events.jsonl`. Live: in-process subscribers.

### `seance ctl watch`

```bash
seance ctl watch --kinds status_set,ask,cmd_end
seance ctl watch --pane worker-1 --since-seq 42
seance ctl watch --no-catch-up   # live only
```

After the ack line, the connection streams `ControlResponse` lines whose
`data` is a full event object until the client disconnects. This is the
epoll of the collab OS — prefer it over polling `read`.

### Capabilities / policy

Default policy is **`open`** (legacy behaviour). Persist at
`~/.local/share/seance/caps.json`.

```bash
seance ctl policy                          # get
seance ctl policy set propose_required     # send/send_raw need grant or propose
seance ctl policy set locked               # mutating ops denied without grant
seance ctl policy set open
seance ctl grant agent:worker-1 send --ttl 3600
seance ctl revoke agent:worker-1 send
seance ctl whoami
seance ctl caps
```

Human UI and daemon are always unrestricted. Always-free ops under any policy:
list/read/status/timeline/watch/human/ask/propose/status_set/scratchpad/commands/…

### Causal attribution tint

PTY stdin is tagged with origin. The GUI paints a 2px left gutter:

- faint — last input was human
- violet — agent / cli inject
- flame — accepted ghost proposal

### Docs vs skill

**Canonical agent contract is `seance ctl skill`.** This file and `ctl help`
track the surface; when they disagree, skill + this foundation section win.
