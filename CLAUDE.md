# seance — notes for coding agents (working on this repo)

Seance is a **human + agent co-working** app: multi-pane live terminals on
Linux (GPUI), shared scratchpads, file panes, and a Unix-socket control plane
so anyone in the circle can engage everyone else in the open. Product intent
is in `README.md` and `seance ctl skill` — not "Claude wrapper."

## Hard rules (read before anything else)

1. **Never hard-kill the daemon** (`pkill -x seance` murders live sessions).
   Deploy: `cargo build --release && seance upgrade` (runtime) or
   `seance restart-gui` (UI only). Sessions survive both.
2. **`./scripts/check.sh` must pass before every commit** — fmt --check,
   deny-warnings `cargo check --all-targets`, full test suite. The repo is at
   **zero warnings** and stays there; a warning is a build failure.
3. **Never bump gpui / zed / alacritty / gpui-component revs** casually.
   Pinned pair (bump only together, see `docs/PLAYBOOK.md`):
   GPUI patched to `deps/zed` @ `1a246efd7e1b83ab568ec5e3e6c1a43a42e1abba`
   (`./scripts/bootstrap-deps.sh`), `gpui-component` @
   `b5eef62336f88bb6c1ee45bf32f73c9895d49f8d`. Grep `deps/zed` for real APIs —
   GPUI training data is stale; **never write GPUI calls from memory**.
4. **Version lockstep is one rule, stated once.** Every workspace crate
   inherits `workspace.package.version`, and the hello line carries it: the
   daemon refuses any ctl/GUI/web client whose version isn't an exact match.
   So a version bump means, in the same breath: `./scripts/build-web.sh
   release` (rebuild the committed `crates/seance-web/dist`), `cargo build
   --release && seance upgrade` (daemon), then restart the GUI and any running
   `seance web` bridge. Skip one and that surface fails with version skew —
   by design, not a bug.
5. **Stay on `master`; never push** unless explicitly asked.
6. **Checkpoint-commit before any multi-hundred-line mechanical change.**
   A big-bang split of app.rs failed once and had to be rolled back from git.
   The discipline that then succeeded: one slice → `cargo test` → commit →
   next slice. Nothing bigger than one green step at a time.

## Architecture map (post-0.9.14 modular split; 0.11 workspace carve)

```
crates/seance-core/    sans-io shared crate — MUST compile native AND wasm32:
                       protocol.rs (wire types), snapshot.rs (SCG3 codec),
                       input.rs (key encoding), control.rs, auth.rs,
                       replay.rs (SRR1 format), util.rs (slugify)
crates/seance-web/     the wasm browser client: lib.rs (app core, rAF loop),
                       renderer.rs (WebGL2 atlas), conn.rs / state.rs / input.rs,
                       ui.rs + menus.rs + keymap.rs + help.rs (chrome),
                       activity.rs, probe.rs, pr_board.rs (the PR sweep
                       overlay — pure model + DOM, native prboard.rs twin),
                       replay.rs (player),
                       replay_edit.rs (editor); dist/ is committed
src/webbridge.rs       `seance web`: ws↔unix pump, token auth, static files,
                       /ws + /healthz + /replay/{list,manifest,pane,publish}
src/replayexport.rs    `seance replay` CLI + bundle exporter + publisher seam
src/runtime/recorder.rs  daemon-side replay ring recorder (48h DVR)
src/main.rs            entry: version/ctl/daemon/web/replay dispatch, SIGPIPE, window setup
src/app/               the GPUI app, split by surface:
  mod.rs      (~2.3k)  SeanceApp struct, boot, GuiEvent loop, key capture,
                       focus/rename/pane lifecycle, render() entry
  actions.rs           all Act* gpui actions + SEANCE_ARM_PROMPT
  layout.rs            layout.json load/save (pure parse/serialize split)
  util.rs              pure helpers (tips, status colors, drag types)
  chrome.rs            render_pane, help overlay, asks/activity/stage strips
  pads.rs              scratchpad drawer + phone spine
  overview.rs          ctrl+shift+space live map
  sidebar.rs           left rail: active/parked workspace rows, context menus, host list
  tiles.rs             tile grid + sashes + zoom
  palette.rs           command palette
  quicklaunch.rs       launch strip: quicklaunch chips + create/edit modal
                       (daemon-side json); also hosts the menu chips
  menus.rs             host-provided MENUS (`menus[]` of host.json): a chip
                       that runs `list_cmd` on click, drops a picker, runs
                       `select_cmd` on choice. On-demand twin of the polled
                       host widgets — see docs/HOST.md
  workspaces.rs        workspace state ops + WorkspaceAttention + active/parked
                       partition + NavHistory (mouse back/forward visit path,
                       kept by watching the selection each render)
  prlinks.rs           PR header chip + all-links popover + pr_attention helper
  prboard.rs           `PRs (N)` sweep overlay: pure board model (grouping,
                       ordering, staleness, dup annotation) + render half
src/runtime/engine/workspaces.rs  circle identity: stable slug + mutable
                       label, resolve-by-either, and the one generic
                       scope/workspace normalizer both request planes run
src/runtime/engine/    the daemon: mod.rs (~0.6k: Engine, persist, upgrade
                       handoff) + gui.rs (conn registry, state/grid push,
                       handle_gui) + spawn.rs (PTY lifecycle) + control.rs
                       (handle_control) + helpers.rs + tests.rs + gui_tests.rs
                       + pr_links.rs (per-workspace PR URL list, cap 8)
src/runtime/           protocol.rs (re-exports seance-core wire types + the
                       native-only handoff types), snapshot.rs (re-export),
                       pty_session.rs (daemon PTY via alacritty_terminal),
                       recorder.rs, pr_scrape.rs (PR URLs out of raw PTY
                       output: ANSI-strip + chunk-split carry)
src/ctl/               the CLI client: mod.rs, parse.rs, wait.rs, print.rs, phone.rs
src/control.rs         control-plane wire types + serde
src/gui_client.rs      GUI→daemon request client + fs-bridge fs_call plumbing
src/tunnel.rs          thin-client ssh -N -L forward supervisor (docs/REMOTE.md)
src/fileview.rs        live file pane: markdown/monospace body, history stepping
src/mdfold.rs          markdown section folding — a PURE model over the source
                       (path-keyed folds, `seance-h` fences the pane renders
                       itself). Deliberately not a fork of gpui-component's
                       markdown stack; see docs/FILE-PANES.md
src/subscriptions_pref.rs  per-GUI active-set persistence (~/.config/seance/subscriptions.json)
src/launch.rs          launch preference (local vs remote host, persisted)
src/picker.rs          startup picker window (choose daemon location)
src/sysopen.rs         portability helpers + THE link-open seam (open_detached
                       / open_blocking; every click that opens a url)
src/scrylink.rs        route our own hosts (localhost, ham.xyz) to scry's
                       control socket, workspace `general`; fails to the
                       default browser on anything unexpected
src/daemon/fsbridge.rs daemon side of the fs bridge + host widget poller
src/daemon/prwatch.rs  external PR-poller ingest (`pr_watch.json`, mtime-polled)
src/remote_term*.rs    daemon-backed terminal model + GPUI view
src/term_shared.rs     TerminalEvent/Ghost/keystroke_bytes shared by remote path
```

There is **no local-PTY path**: the old in-GUI `terminal.rs`/`terminal_view.rs`
were deleted 2026-07-22 as unreachable (git history has them). All PTYs live
in the daemon; the GUI renders `PaneBody::Remote`/`File` only. Do not
reintroduce a local terminal without a product decision.

**Thin-client invariant (0.10+, docs/REMOTE.md): the GUI never touches the
local filesystem for workspace content or behavior-affecting config.** Files,
pads, layout, host widgets, event-log writes all go through the daemon fs
bridge (`GuiRequest::Fs` / `daemon/fsbridge.rs`); bridge calls are blocking
and must never run on the render/UI thread (background executor + update —
see fileview.rs / scratchpad.rs for the idiom). The GUI may run on macOS
against a remote daemon; keep new code portable (use `sysopen.rs`, never
`/proc` or `xdg-open` directly). Version lockstep across all clients is hard
rule 4. The web client (`crates/seance-web`) is a third client on the same
protocol and the same invariant: it has no filesystem at all.

## Module conventions (the split's contract — follow it)

- Child modules of `app/` and `engine/` hold `impl SeanceApp` / `impl Engine`
  blocks. Descendant modules see parent private fields — that's the design;
  don't add getters to route around it.
- A method called across module boundaries is `pub(super)`, no wider. Only
  `SeanceApp` itself (main.rs/popout.rs) needs `pub`. `Act*` structs are
  `pub` (gpui requirement).
- Every file owns its `use` header; parent imports don't flow through.
- Multi-window protocol APIs not yet wired to UI are kept behind documented
  `#[allow(dead_code)]` (`GuiClient::refresh_grid`,
  `Engine::full_state_event`, `empty_window` read-side). **Wire or retire
  consciously — never delete blind, never strip the allow to "fix" a warning.**
- Dead-code deletions must be compiler-verified (rustc "never used" + zero
  grep hits incl. `scripts/*.sh` and docs) — see the 0.9.14 CHANGELOG entries
  for the precedent.
- `fileview.rs` (~1.7k) and `remote_term_view.rs` (~1.5k) are the largest
  files left in `src/` outside `app/mod.rs` — cohesive and tested; split only
  with cause. Same rule holds for `seance-web`'s `ui.rs` / `replay*.rs`.

## Build / test / run

```bash
./scripts/bootstrap-deps.sh   # once, if deps/zed missing
./scripts/check.sh            # THE gate: fmt + deny-warnings + workspace tests
./scripts/build-web.sh release             # rebuild crates/seance-web/dist
./scripts/bundle-macos.sh     # macOS: build + install /Applications/Seance.app
cargo build --release && seance upgrade    # deploy runtime, sessions live
seance restart-gui                         # deploy UI only
seance ctl skill              # agent-facing engagement protocol
seance ctl list --all · roster · doctor
```

First cold build ~10 min (gpui at opt-level 3 even in dev — do not remove the
`[profile.dev.package]` opt-level overrides).

### Tests

300+ tests across the workspace (`src` ~200, `seance-core` ~35,
`seance-web` ~80), all hermetic. Engine tests use `Engine::bare_for_test` +
`push_stub_pane`; multi-window `handle_gui` coverage lives in
`engine/gui_tests.rs` (captured GuiEvent payloads on fake conns). Any test
touching `SEANCE_STATE_DIR` must go through `state::test_env_lock()` — env
races are the historical flake source. No timing-sensitive tests: the grid
throttle timer path is deliberately untested (real clocks flake); its pure
core (`grid_interval_for`) is pinned instead. Never write a test that sleeps
to synchronize.

## Verifying (evidence over vibes)

- `seance ctl read <pane>` — true rendered grid
- `seance ctl human` / `roster` / `brief` — focus, stage, dense state
- `seance ctl pad PANE --cat` — one-hop pad body
- `SEANCE_DEBUG_IO=1` — PTY I/O on stderr
- After daemon-touching changes: `seance upgrade` then `ctl list --all` —
  the pane count must not drop. That's the session-survival proof.

## Product rules (don't regress these)

- **Visibility is the point** — agents work on the human's screen, not offstage
- Default new pane is a **shell** (human can always take over); agents via `--agent` / `--command`
- Prefer `propose` / `ask` / status badges / scratchpads over silent side effects
- Workspace scoping is default inside a pane; `--all` is explicit cross-circle
- Durable text → scratchpad or file pane; screens are ephemeral
- **Completion is evidence-bound** — `finish` with body; `pad_rev` / since-inject wait
- **Self-only** note/finish/status-set when `$SEANCE_SESSION` is set (orchestrators outside a pane may cross)
- Sidebar **working** badge derives from *observed* TUI title spinners, not
  sticky `status-set` — don't "fix" idle circles by re-sticking status

### Circle identity (0.16+) — slug is the id, label is the text

A circle has a **slug** (minted once at creation, never rewritten) and a
**label** (free text, what a rename changes). Same split panes have. Everything
is keyed by the slug: panes, activity clocks, PR links + dismissals,
selections, subscriptions, client pin/park prefs, and `$SEANCE_WORKSPACE` in
every running pane's environment — which is the case that forced the change,
since nothing can write into the environment of a process already running.
**Do not reintroduce rename migrations**; if you find yourself carrying state
from an old workspace name to a new one, the identity moved and it shouldn't
have. `scope`/`workspace` accept either form (slug wins, then an unambiguous
label) via `normalize_workspace_keys` at the daemon door — don't add per-handler
resolution. `ctl whoami` answers from the pane and is the authority.

### ctl contract (current)

`send` → `task_id` + sidecar; `wait --status done` is evidence-bound
(`--badge-only` to skip) with **event-driven wake**; `wait … --cat` /
`harvest` fan-in harvests pads; `ctl task` / `whoami` re-read inject; exit →
idle; roster shows **slug**; `--wait-ready` runs profile **boot-clear**;
`phone` / `prompts` are human-spine ctl surfaces. `pr-link add|clear` seeds /
drops scraped PR links (statuses come from the `pr_watch.json` watcher, not ctl). Cmdlog **survives upgrade**.
`sleep`/`wake` put a circle down and bring it back (`runtime/engine/sleep.rs`):
**only restorable circles** — every pane a claude session with a transcript on
disk, or a file pane; a shell vetoes its circle. Slept panes keep identity + a
frozen last frame (`<state-dir>/frozen/<slug>.scg`) and stay slept across
restart/upgrade. `send` (and GUI keystrokes) auto-wake; scrolling doesn't.
Auto-sleep at 12h idle (`daemon/sleepsweep.rs`).
`ctl phone` opens a vita telegram topic and **seeds a stage card** — **no**
`register_participant` claim. Full protocol: `docs/CONTROL.md`.

### Multi-agent collab test

**In-seance orchestrator**, not an external worker driver: run
`./scripts/agent-collab-test.sh` (docs: `docs/AGENT_COLLAB_TEST.md`, outputs:
`data/agent-collab-runs/<workspace>/`). After orchestration-behavior changes,
re-run it and read orch + worker pads before claiming done. Pure refactors
(behavior-preserving, check.sh green, upgrade smoke passed) don't require it.

## Docs to keep current

- **`CHANGELOG.md`** — canonical user-facing history. Every versioned ship
  (`Cargo.toml` bump + commit subject `seance 0.9.N — …`) must in the same
  commit: bump Cargo.toml/Cargo.lock/README status line, add the `## [0.9.N]`
  section at top, clear `[Unreleased]`. Product deltas, not commit dumps.
- `docs/PLAYBOOK.md` (pins) · `docs/CONTROL.md` (protocol) ·
  `docs/DAEMON.md` (upgrade) · `docs/THEME.md` (palette; `SeancePalette`) ·
  `docs/REMOTE.md` (thin client) · `docs/PERF-TERMINAL.md` (render path,
  both native and web numbers) · `docs/WEB.md` (web client + bridge;
  `crates/seance-core` is the sans-io wire crate shared native/wasm — it must
  always compile for both targets) · `docs/REPLAY.md` (recorder, SRR1, player,
  editor, publisher seam).

## Conventions

- Domain modules carry rustdoc headers
- Atomic writes for state/scratch
- Control plane: JSON lines; `from` / `scope` stamped by ctl
