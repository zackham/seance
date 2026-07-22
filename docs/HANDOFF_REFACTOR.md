# Handoff — seance modular refactor (COMPLETE, 2026-07-22)

Status doc for the next agent. The refactor this file used to plan is **done
and shipped as 0.9.14**. History of the plan/failed-first-attempt is in git
(`git log --follow docs/HANDOFF_REFACTOR.md`).

## TL;DR

| item | status |
|------|--------|
| tests | **143 passed** (`cargo test`), 3× stability-verified |
| warnings | **0** (was 89), enforced by `scripts/check.sh` (deny-warnings) |
| app modularization | **done** — `app/mod.rs` ~1.9k, 10 child modules |
| engine modularization | **done** — `engine/mod.rs` ~0.6k + gui/spawn/control/helpers |
| dead local-PTY subsystem | **removed** (~1.4k LOC; `term_shared.rs` keeps the 3 live items) |
| live smoke | daemon **upgrade** + **restart-gui**; **9 panes survived** (2026-07-22) |
| adversarial review | **passed** — all moved bodies diffed vs baseline, zero logic drift |
| dep bumps | **none** (gpui/zed/alacritty pins untouched — `docs/PLAYBOOK.md`) |

## Module map (current)

```
src/app/            mod.rs ~1.9k (struct, boot, event loop, key capture,
                    focus/rename/lifecycle, render entry)
                    actions.rs (Act* + SEANCE_ARM_PROMPT) · layout.rs ·
                    util.rs · chrome.rs (pane chrome/help/strips) · pads.rs ·
                    overview.rs · sidebar.rs · tiles.rs · palette.rs ·
                    workspaces.rs
src/runtime/engine/ mod.rs ~0.6k (Engine, new/from_handoff, persist, upgrade)
                    gui.rs (GuiConn registry, state/grid push, handle_gui) ·
                    spawn.rs (PTY spawn/kill/fork) · control.rs · helpers.rs ·
                    tests.rs · gui_tests.rs (hermetic multi-window tests)
src/ctl/            mod.rs · parse.rs · wait.rs · print.rs · phone.rs
src/term_shared.rs  TerminalEvent/Ghost/keystroke_bytes for the remote path
```

Conventions the split established (follow them):

- Child modules hold `impl SeanceApp` / `impl Engine` blocks; they see parent
  private fields (descendant modules). Methods called across module
  boundaries are `pub(super)`; keep surfaces minimal.
- Every file carries its own `use` header; parent private imports don't flow.
- `scripts/check.sh` must stay green (fmt --check, deny-warnings
  check --all-targets, full tests). Run it before every commit.

## Multi-window: FULLY wired as of 0.9.15

Overview (`ctrl+shift+space`, viewport-filling grid with ≤1× thumbs), transfer
menus, collect-all, empty-window pull (with stage instructions), minimize
shelf, row sashes — all live. The former "protocol-ready, awaiting UI" allows
are all resolved: `refresh_grid` wired (pane mount + damage repair),
`flush_all_grids` wired (CollectAll), `empty_window` read (stage hint),
`full_state_event` retired. Drive-mode chip (`⛔ locked` / `⚡ led`) renders in
pane headers. There are zero `#[allow(dead_code)]`-for-wiring markers left.

## Remaining sensible next steps (none blocking)

- `fileview.rs` (1512) and `remote_term_view.rs` (1330) are the last two
  files above ~1.3k; both cohesive, both have tests — split only with cause.
- Grid throttle timer path (`push_grid_throttled`) is consciously untested
  (real clocks → flake risk); damage-vs-full decision needs a live PtySession.
- e2e: `./scripts/e2e-thorough.sh` + `./scripts/agent-collab-test.sh` after
  orchestration-behavior changes (not needed for pure refactors).

## Hard rules (unchanged)

1. **Session survival is absolute**: `cargo build --release && seance upgrade`
   (runtime) / `seance restart-gui` (UI). Never `pkill -x seance`.
2. Never bump gpui / zed / alacritty / gpui-component revs casually.
3. Stay on `master`; never push unless asked.
4. Checkpoint-commit before any multi-hundred-line mechanical change.
