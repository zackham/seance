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
| `new` | `seance ctl new --name NAME [--cwd DIR] [--command CMD] [--workspace WS]` |
| `send` | `seance ctl send SESSION TEXT...` `[--no-submit]` |
| `send-raw` | `seance ctl send-raw SESSION BYTES` |
| `read` | `seance ctl read SESSION [--lines N]` |
| `status` | `seance ctl status SESSION` |
| `kill` | `seance ctl kill SESSION` |
| `scratchpad` | `seance ctl scratchpad SESSION` |
| `help` | `seance ctl help` |

Notes:

- **`send`** joins all trailing words into the prompt, so you don't have to
  quote it: `seance ctl send build run the full test suite`. Quote if you need
  to preserve exact spacing or punctuation the shell would eat.
- **`--no-submit`** stages text without pressing Enter.
- **`send-raw`** interprets `BYTES` as a UTF-8 string and base64-encodes it.
  Use shell `$'…'` escapes for control chars: `send-raw build $'\x03'` (Ctrl-C),
  `send-raw build $'\r'` (bare Enter).
- **`read`** prints the screen verbatim; **`scratchpad`** prints just the path,
  so `cat "$(seance ctl scratchpad build)"` works.

### Examples

```bash
seance ctl new --name build --cwd ~/proj --command claude
seance ctl send build "run the test suite and summarize any failures"
seance ctl read build --lines 40
seance ctl send-raw build $'\x03'          # interrupt whatever's running
cat "$(seance ctl scratchpad build)"       # read the shared notes
seance ctl status build --json             # machine-readable status
seance ctl kill build
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
