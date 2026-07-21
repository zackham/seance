# seance — shell integration & command log

Seance knows **command boundaries** in its shell panes: what command ran, in
which directory, its exit code, and how long it took. Agents driving a pane read
these as *structured facts* instead of screen-scraping a rendered terminal, and
the human gets affordances like "jump to the last failed command."

The mechanism is **shell-side hooks, not terminal escape parsing.** Seance's
default shell panes source an rc file (`assets/seance.bash`) whose hooks report
each command's start and end over the existing control plane. No OSC/DEC escape
sequences, no PROMPT reserved-byte tricks, no alacritty-grid heuristics — the
shell tells us directly, in-band with the rest of the control protocol.

```
┌────────────────────┐   seance ctl cmd-begin/-end   ┌──────────────────────┐
│  bash pane          │ ────────────────────────────▶ │  control socket       │
│  (sources           │        (backgrounded,          │  (src/control.rs)     │
│   seance.bash)      │         silent-on-fail)        │        │              │
│  DEBUG trap +       │                                │        ▼              │
│  PROMPT_COMMAND     │                                │  CommandLog           │
└────────────────────┘                                │  (src/cmdlog.rs) +    │
       ▲                                                │  events.jsonl         │
       │ Commands / LastCommand query ops               └──────────────────────┘
   agent / human ◀───────────────────────────────────────────────┘
```

Two files own the pieces that already exist:

- **`assets/seance.bash`** — the rc file the shell sources. It installs a `DEBUG`
  trap (captures the command line before it runs) and a `PROMPT_COMMAND` hook
  (captures `$?` after), and reports via `seance ctl cmd-begin` / `cmd-end`.
- **`src/cmdlog.rs`** — the app-side store: a per-pane ring buffer of
  `CommandRecord`s (cap 500/pane), gpui-free and unit-tested. `begin` opens a
  record and returns its `seq`; `end` closes the most-recent-open record.

**Status (0.9.1):** the protocol ops, CLI, engine wiring, and event-bus
emission (`cmd_start` / `cmd_end` with span ids) **are built**. The sections
below remain useful as the contract. Remaining future work: OSC-133
shell-agnostic markers (optional replacement for bash DEBUG traps), durable
cmdlog across daemon handoff, jump-to-failed-command UI.

---

## 1. Protocol ops to add (`src/control.rs`)

Four new `ControlRequest` variants, tagged `snake_case` by `op` like the rest.
Every op carries the standard `from` (calling pane, auto-filled from
`$SEANCE_SESSION`) and `scope` (workspace, from `$SEANCE_WORKSPACE`) fields, so
they thread through `with_identity` in `ctl.rs` exactly like the existing ops.

### Reporting ops (the shell → app direction)

```rust
/// A command started in the calling pane. `from` IS the pane whose shell
/// fired the hook — the command is attributed to it. `cwd` is the shell's
/// $PWD at command start. No response payload needed (fire-and-forget).
CmdBegin {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    from: Option<String>,
},

/// The most recent open command in the calling pane finished with `exit`.
/// Attributed to `from`, like CmdBegin.
CmdEnd {
    exit: i32,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    from: Option<String>,
},
```

**Attribution: `from`, not `pane`.** Unlike `send`/`read`/`kill` (which target
*another* pane by an explicit `pane` field), the command hooks report on the
*calling* pane's own shell. So the pane id travels via `from` — the CLI fills it
from `$SEANCE_SESSION` automatically (that's already how `ctl.rs` stamps every
request in `with_identity`). This means the shell hooks never need to pass a
pane id explicitly, which is why `assets/seance.bash` calls plain
`seance ctl cmd-begin "$CMD" --cwd "$PWD"` with no pane argument.

A `CmdBegin`/`CmdEnd` whose `from` is `None` (a ctl run outside any seance pane)
has no pane to attribute to — the app should drop it (no-op, `ok_empty()`).

### Query ops (the read-back direction)

```rust
/// Recent command records for a pane, oldest-first.
Commands {
    #[serde(alias = "session")]
    pane: String,
    #[serde(default)]
    limit: Option<usize>,   // default e.g. 20; cap at CAP_PER_PANE app-side
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    from: Option<String>,
},

/// The most recent command in a pane; with `failed_only`, the most recent
/// one that finished non-zero.
LastCommand {
    #[serde(alias = "session")]
    pane: String,
    #[serde(default)]
    failed_only: bool,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    from: Option<String>,
},
```

`Commands`/`LastCommand` are pane-**targeting** ops (they name a `pane`), so they
obey workspace scoping exactly like `read`/`status`: error if `pane` is outside
`scope`. `pane` resolves a name or slug like the other targeting ops.

Add each to `ctl.rs`'s `with_identity` match arm (stamp `scope`/`from`), and to
the dispatch `match` in `main`.

---

## 2. App-side handling (`src/app.rs`)

The app owns one `CommandLog` (from `src/cmdlog.rs`) alongside its other
per-session state. Wire it on the gpui loop where control requests are applied:

| op | app action |
|----|-----------|
| `CmdBegin` | resolve `from` → pane slug; if none, no-op. `log.begin(slug, command, cwd.unwrap_or_default())`. Emit **one** `events.jsonl` line: `kind: "cmd_start"`, `pane: slug`, `actor: "agent:<slug>"` (or `human`/`cli` per the actor rules), `detail: "$ <command>"`. Respond `ok_empty()`. |
| `CmdEnd` | resolve `from`; if none, no-op. `log.end(slug, exit)`. Look up the just-closed record's `duration_ms()` and emit **one** `events.jsonl` line: `kind: "cmd_end"`, `pane: slug`, `detail` including exit + duration, e.g. `"exit 1 · 2.4s: cargo test"`. Respond `ok_empty()`. |
| `Commands` | scope-check `pane`, resolve slug, `log.list(slug, limit.unwrap_or(20))`, respond `ok(json!({ "commands": [...] }))`. |
| `LastCommand` | scope-check `pane`, resolve slug, `log.last(slug, failed_only)`, respond `ok(json!({ "command": <record-or-null> }))`. |

**Two events per command, matching the existing flight-recorder convention.**
`events.rs` already documents a `kind` vocabulary (`ctl_send`, `pane_spawned`,
…); add `cmd_start` and `cmd_end` to that list's doc comment. The `cmd_end`
detail is where exit + duration live, so a plain `timeline` read shows command
outcomes without touching the `CommandLog` at all — the log is the *structured*
view, the events are the *chronological* view. Both are fed from the same op.

**Lifecycle: drop a pane's log when it dies.** Where the app already handles a
pane being killed/closed (the `pane_killed` path), also call
`log.remove_pane(slug)` so records don't leak for dead panes. `CommandLog`
already makes this idempotent.

### CommandRecord shape (the query response element)

Serialized straight from `src/cmdlog.rs`:

```json
{
  "seq": 12,
  "command": "cargo test",
  "cwd": "/home/z/proj",
  "started_ms": 1700000000000,
  "ended_ms": 1700000002500,   // null while running / on a lost end
  "exit": 0                    // null while running / on a lost end
}
```

`Commands` → `{ "commands": [ <record>, ... ] }` (oldest-first).
`LastCommand` → `{ "command": <record> | null }`.

---

## 3. CLI verbs to add (`src/ctl.rs`)

Two new subcommands, hand-parsed in the same style as `timeline`/`read`:

| verb | usage | maps to |
|------|-------|---------|
| `commands` | `seance ctl commands PANE [--limit N]` | `Commands { pane, limit }` |
| `last-command` | `seance ctl last-command PANE [--failed]` | `LastCommand { pane, failed_only }` |

The shell hooks also call two verbs — but these are the *reporting* side and are
not meant for humans (still, wire them so `assets/seance.bash` works):

| verb | usage | maps to |
|------|-------|---------|
| `cmd-begin` | `seance ctl cmd-begin COMMAND... [--cwd DIR]` | `CmdBegin { command, cwd }` |
| `cmd-end` | `seance ctl cmd-end EXIT` | `CmdEnd { exit }` |

Notes for the parsers:

- `cmd-begin` joins trailing words into `command` (like `send` joins its text),
  and takes an optional `--cwd DIR`. `assets/seance.bash` passes the command line
  as one quoted arg plus `--cwd "$PWD"`, so a single word usually suffices, but
  join-trailing keeps it robust if the shell splits.
- `cmd-end` takes exactly one integer arg, the exit code (`$?`).
- `commands` renders human-readably (a compact table: `seq`, `✓/✗ exit`,
  duration, command) unless `--json`. `last-command` prints the one record.
- Add both human-facing verbs to `seance ctl help` and the `SKILL_TEXT` block so
  a master agent discovers them. Suggested skill-text lines:
  - `seance ctl commands PANE [--limit N]` — recent commands a shell pane ran,
    with exit codes and durations (structured; no screen-scraping).
  - `seance ctl last-command PANE [--failed]` — the pane's last command (or last
    *failed* command). Use to check "did that just work?" without a `read`.

---

## 4. Spawn-side change (`src/pane.rs` + `src/app.rs`)

Default shell panes must launch bash pointed at the rc file; explicit
`--command` panes are untouched.

### Install the rc file on startup

On app startup, copy `assets/seance.bash` to
`~/.local/share/seance/seance.bash` (the seance state dir, same root as
`events.jsonl` and the scratchpads). Overwrite on every startup so an upgraded
binary ships an upgraded rc file. This is a plain "write the embedded asset to
disk" step — embed the file with `include_str!("../assets/seance.bash")` so the
binary is self-contained, then write it (best-effort; a failure just means panes
fall back to a plain shell, which is the graceful-degradation contract anyway).

### Point the default command at it

`pane.rs` has `DEFAULT_COMMAND = "bash -l"`. The default-shell spawn should
instead launch:

```
bash --init-file ~/.local/share/seance/seance.bash
```

`--init-file` replaces bash's normal interactive startup, which is why
`seance.bash` **sources the user's `~/.bashrc` itself** before installing hooks —
the user's shell is otherwise unchanged. (`--init-file` already implies an
interactive shell, so the `-l` login flag is dropped; if login-shell semantics
matter, source `~/.profile`/`~/.bash_profile` inside the rc file too — currently
it sources only `~/.bashrc`, matching a non-login interactive bash.)

Recommended shape: keep `DEFAULT_COMMAND` as the human-readable label but, in the
spawn path, detect "this is the default shell" (command unset / equals the
default) and substitute the `--init-file` invocation with the resolved absolute
path to the installed rc file. **Explicit `--command` panes** (`claude`, a
one-off script, `--command "bash -l"` typed deliberately) get exactly what was
asked for — no rc injection. The discriminator is "did the caller specify a
command?", which `SpawnRequest.command: Option<String>` already carries (`None`
⇒ default shell ⇒ inject; `Some(_)` ⇒ verbatim).

The pane already gets `SEANCE_SESSION`/`SEANCE_SOCKET`/`SEANCE_WORKSPACE` in its
env (see `spawn_pane`), which is exactly what the hooks and `seance ctl` need —
no new env vars.

---

## 5. How the hooks work (`assets/seance.bash`)

Sourced as `bash --init-file`, the rc file:

1. **Sources `~/.bashrc`** if present, so the pane is a normal shell first.
2. **Gates on `$SEANCE_SESSION` being set AND `seance` on `PATH`.** If either is
   missing it installs nothing and returns — a plain interactive bash, zero
   overhead. (This is why a shell outside seance, or one where the binary moved,
   just behaves normally.)
3. **`DEBUG` trap** — fires before each command; captures `$BASH_COMMAND` and
   reports `seance ctl cmd-begin "$CMD" --cwd "$PWD" >/dev/null 2>&1 &`
   (backgrounded, disowned, fully silenced — never blocks the shell).
4. **`PROMPT_COMMAND` hook** — runs before each prompt (after the previous
   command finished); captures `$?` on its first line and reports
   `seance ctl cmd-end "$EXIT" >/dev/null 2>&1 &`, then re-arms the trap.

### Re-entry guarding (the load-bearing part)

The `DEBUG` trap fires before *every* simple command, including the commands
inside `PROMPT_COMMAND` and every stage of a pipeline. Without guards it would
log its own reporting call and one begin per pipeline stage. Three guards:

- `[ -n "$COMP_LINE" ]` — skip while in programmable completion.
- `[ "$BASH_COMMAND" = "$PROMPT_COMMAND" ]` — skip the trap firing *for* the
  prompt hook itself.
- **A one-shot armed latch** (`_seance_armed`): the trap captures the *first*
  command after a prompt, then disarms until `PROMPT_COMMAND` re-arms it. This
  collapses a pipeline/compound command to a **single** `cmd-begin` (the text of
  its first-fired command). The latch **starts disarmed** so the rc file's own
  setup commands aren't captured; the first prompt arms it, so the first thing
  ever captured is a real user command.

`$?` is snapshotted as the very first statement of the prompt hook (`local
exit=$?`) before anything can clobber it, and we only emit `cmd-end` if a
command was actually captured that cycle — so empty-line Enters and the initial
prompt never produce an unpaired `cmd-end`.

---

## 6. Known limits & caveats

These are the honest edges of shell-hook command tracking. The `CommandLog`
store is built to degrade gracefully at each one (open records stay "unknown,"
never fabricated) — but the main agent should know them:

- **Pipelines** report as **one** record with the text of the pipeline's
  first-fired command (the latch collapses the stages). The `exit` is the
  pipeline's overall `$?` — bash's default (last stage's status), *not*
  `PIPESTATUS`. This is the right granularity for "what did the user run," but
  don't expect per-stage exit codes.
- **Multi-line commands** (a `for`/`while`/`if` block, a `\`-continued line, a
  quoted newline) capture as the first *simple command* the DEBUG trap fires on,
  not the whole source text. `command` may therefore be a fragment of a
  multi-line construct. `cmd-end` still pairs correctly (one prompt = one end).
- **Subshells / `&` background jobs / process substitution** run commands whose
  DEBUG firings are latched out (only the first command per prompt cycle is
  captured). A `foo &` disown'd job that finishes later does **not** produce its
  own `cmd-end`. Command tracking is about foreground, prompt-to-prompt commands.
- **`exit` / the last command before the shell dies** gets a `cmd-begin` but no
  `cmd-end` (no subsequent prompt fires). That record stays open — correctly
  rendered as "ran, outcome unknown." `remove_pane` cleans it up when the pane
  is torn down.
- **Fire-and-forget delivery can reorder at the socket.** The `&`-backgrounded
  reporters race, so two panes' (or even one pane's begin vs. end) messages can
  arrive slightly out of order. This is safe because: (a) a single shell is
  strictly sequential — it cannot start command N+1 until command N's prompt
  hook has fired — so at most **one** record per pane is open at a time in
  practice, and `CommandLog::end` closing "the most recent open record" is
  unambiguous; (b) if a rare interleaving still closed the wrong record, the
  cost is a swapped exit code on adjacent commands, never a crash or a leak.
- **A lost `cmd-begin`** (control plane briefly down) means its `cmd-end` finds
  no open record → dropped as a no-op. **A lost `cmd-end`** leaves the record
  open forever (until eviction/`remove_pane`) → "unknown," never a fake exit.
- **Non-bash shells** (zsh, fish) get no integration — they don't source
  `seance.bash`. They still run fine; they just have no command log. Extending
  to zsh (`preexec`/`precmd`) is a future rc file, not a change here.
- **`$SECONDS`/timing** comes from app-side wall-clock stamps on begin/end (the
  `CommandLog` stamps `now_ms()`), not from the shell — so a slow control-plane
  delivery adds at most a few ms of skew to `duration_ms`, and there's no shell
  arithmetic to get wrong.
