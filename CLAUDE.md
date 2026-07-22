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
4. **Stay on `master`; never push** unless explicitly asked.
5. **Checkpoint-commit before any multi-hundred-line mechanical change.**
   A big-bang split of app.rs failed once and had to be rolled back from git.
   The discipline that then succeeded: one slice → `cargo test` → commit →
   next slice. Nothing bigger than one green step at a time.

## Architecture map (post-0.9.14 modular split)

```
src/main.rs            entry: version/ctl/daemon dispatch, SIGPIPE, window setup
src/app/               the GPUI app, split by surface:
  mod.rs      (~1.9k)  SeanceApp struct, boot, GuiEvent loop, key capture,
                       focus/rename/pane lifecycle, render() entry
  actions.rs           all Act* gpui actions + SEANCE_ARM_PROMPT
  layout.rs            layout.json load/save (pure parse/serialize split)
  util.rs              pure helpers (tips, status colors, drag types)
  chrome.rs            render_pane, help overlay, asks/activity/stage strips
  pads.rs              scratchpad drawer + phone spine
  overview.rs          ctrl+shift+space live map
  sidebar.rs           left rail: workspace rows, context menus, host list
  tiles.rs             tile grid + sashes + zoom
  palette.rs           command palette
  workspaces.rs        workspace state ops + WorkspaceAttention
src/runtime/engine/    the daemon: mod.rs (~0.6k: Engine, persist, upgrade
                       handoff) + gui.rs (conn registry, state/grid push,
                       handle_gui) + spawn.rs (PTY lifecycle) + control.rs
                       (handle_control) + helpers.rs + tests.rs + gui_tests.rs
src/runtime/           protocol.rs (wire types), pty_session.rs (daemon PTY
                       via alacritty_terminal), snapshot.rs (grid encoding)
src/ctl/               the CLI client: mod.rs, parse.rs, wait.rs, print.rs, phone.rs
src/control.rs         control-plane wire types + serde
src/gui_client.rs      GUI→daemon request client
src/remote_term*.rs    daemon-backed terminal model + GPUI view
src/term_shared.rs     TerminalEvent/Ghost/keystroke_bytes shared by remote path
```

There is **no local-PTY path**: the old in-GUI `terminal.rs`/`terminal_view.rs`
were deleted 2026-07-22 as unreachable (git history has them). All PTYs live
in the daemon; the GUI renders `PaneBody::Remote`/`File` only. Do not
reintroduce a local terminal without a product decision.

## Module conventions (the split's contract — follow it)

- Child modules of `app/` and `engine/` hold `impl SeanceApp` / `impl Engine`
  blocks. Descendant modules see parent private fields — that's the design;
  don't add getters to route around it.
- A method called across module boundaries is `pub(super)`, no wider. Only
  `SeanceApp` itself (main.rs/popout.rs) needs `pub`. `Act*` structs are
  `pub` (gpui requirement).
- Every file owns its `use` header; parent imports don't flow through.
- Multi-window protocol APIs not yet wired to UI are kept behind documented
  `#[allow(dead_code)]` (`GuiClient::refresh_grid`, `Engine::flush_all_grids`,
  `Engine::full_state_event`, `empty_window` read-side). **Wire or retire
  consciously — never delete blind, never strip the allow to "fix" a warning.**
- Dead-code deletions must be compiler-verified (rustc "never used" + zero
  grep hits incl. `scripts/*.sh` and docs) — see the 0.9.14 CHANGELOG entries
  for the precedent.
- `fileview.rs` (~1.5k) and `remote_term_view.rs` (~1.3k) are the two
  remaining large files — cohesive and tested; split only with cause.

## Build / test / run

```bash
./scripts/bootstrap-deps.sh   # once, if deps/zed missing
./scripts/check.sh            # THE gate: fmt + deny-warnings + tests (143)
cargo build --release && seance upgrade    # deploy runtime, sessions live
seance restart-gui                         # deploy UI only
seance ctl skill              # agent-facing engagement protocol
seance ctl list --all · roster · doctor
```

First cold build ~10 min (gpui at opt-level 3 even in dev — do not remove the
`[profile.dev.package]` opt-level overrides).

### Tests

143 tests, all hermetic. Engine tests use `Engine::bare_for_test` +
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

### ctl contract (current)

`send` → `task_id` + sidecar; `wait --status done` is evidence-bound
(`--badge-only` to skip) with **event-driven wake**; `wait … --cat` /
`harvest` fan-in harvests pads; `ctl task` / `whoami` re-read inject; exit →
idle; roster shows **slug**; `--wait-ready` runs profile **boot-clear**;
`phone` / `prompts` are human-spine ctl surfaces. Cmdlog **survives upgrade**.
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
- **`docs/HANDOFF_REFACTOR.md`** — current module map + refactor conventions.
  Update it if you change the module layout.
- `docs/PLAYBOOK.md` (pins) · `docs/CONTROL.md` (protocol) ·
  `docs/DAEMON.md` (upgrade) · `docs/THEME.md` (palette; `SeancePalette`).

## Conventions

- Domain modules carry rustdoc headers
- Atomic writes for state/scratch
- Control plane: JSON lines; `from` / `scope` stamped by ctl
