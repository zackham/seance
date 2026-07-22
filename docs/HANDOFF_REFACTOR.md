# Handoff — seance testing + modular refactor (2026-07-21/22)

For the next agent (any model). Read this before touching `app.rs` or re-splitting.

## TL;DR

| item | status |
|------|--------|
| tests | **123 passed** (`cargo test`) |
| release build | green (`cargo build --release`) |
| live smoke | daemon **upgrade** + **restart-gui**; **9 panes survived** |
| engine modularization | **done** |
| ctl modularization | **done** |
| app modularization | **not done** — still monolithic ~5.2k LOC |
| dep upgrades | **none** (do not bump gpui/zed/alacritty) |
| git | uncommitted WIP on `master`; **do not push**; commit only if bio-zack asks |

**Do not** big-bang-split `app.rs` again without a checkpoint commit first.

---

## What “went wrong” earlier (context)

A prior agent session:

1. Expanded unit/integration tests and split `engine` + `ctl` successfully.
2. Attempted to explode `app.rs` into `src/app/{mod,sidebar,tiles,…}`.
3. Mechanical extraction corrupted module boundaries (orphan docs, truncated constants, broken braces).
4. Recovered by restoring `src/app.rs` from **git HEAD** and applying **minimal protocol compatibility** only:
   - `GuiEvent::State { …, window_id: _, windows: _, foreign_workspaces: _ }`
   - `GuiRequest::Attach { empty: false, … }`
5. Engine/ctl splits and tests were **kept**.

So: not a random production meltdown — a **failed app file split**, rolled back. Backend multi-window protocol still exists; **app UI does not wire most of it**.

---

## Smoke results (this session)

Ran against live machine (real panes, not a clean lab):

```text
cargo test                         → 123 ok
cargo build --release              → ok, seance 0.9.13
seance ctl doctor                  → claude/grok/codex/shell ok
seance ctl list --all / roster     → 9 panes live
seance ctl human / brief / skill   → ok
seance ctl read term-1 --lines 5 → ok (real grid)
seance ctl whoami / caps           → ok
seance upgrade                     → ok, sessions preserved
seance restart-gui                 → ok, panes still listed
```

**Session survival rule still absolute:** never `pkill -x seance` / hard-kill daemon. Prefer:

```bash
cargo build --release && seance upgrade   # runtime
seance restart-gui                          # UI only
```

---

## Module map (current)

### Split already (keep)

```
src/runtime/engine/
  mod.rs          ~2317  Engine core: spawn, gui, grids, persist, upgrade
  control.rs      ~1239  handle_control + pad/task bookkeeping
  helpers.rs       ~292  pure: validate_status, atomic pad, task_json, …
  tests.rs         ~302  hermetic control-plane integration tests

src/ctl/
  mod.rs          ~1160  run_ctl entry, skill text, socket I/O, tests
  parse.rs         ~849  all parse_* + with_identity + base64_encode
  wait.rs          ~709  run_wait, run_watch, boot_clear
  print.rs         ~361  human printers + help
  phone.rs         ~292  phone + prompts
```

`main.rs` still has `mod ctl;` / `mod runtime;` — paths resolve via directories.

### Still fat (next work)

| file | ~LOC | notes |
|------|------|--------|
| `src/app.rs` | **5237** | main ball of mud; **do not big-bang** |
| `src/fileview.rs` | 1520 | already has unit tests |
| `src/remote_term_view.rs` | 1342 | GPUI terminal view |
| `src/control.rs` | 1126 | protocol + socket server; has serde tests |
| `src/runtime/engine/mod.rs` | 2317 | could still peel grid/spawn later |

---

## Multi-window asymmetry (important product gap)

Documented in `CHANGELOG.md` **[Unreleased]** as product intent:

- Overview (`ctrl+shift+space`)
- Multi-window (workspace exclusive to one window; transfer / collect / empty second process)

**Implemented on backend / client API:**

| layer | multi-window support |
|-------|----------------------|
| `runtime/protocol.rs` | `Attach.empty`, `SetOverview`, `TransferWorkspace`, `CollectAll`, `WindowInfo`, `ForeignWorkspace`, State fields |
| `runtime/engine` | handlers for those GuiRequests |
| `gui_client.rs` | `connect_empty`, `set_overview`, `transfer_workspace`, `collect_all`, `refresh_grid` |
| **`app.rs`** | **only ignores** `foreign_workspaces` / windows on State; **no** overview map, **no** transfer menus, **no** `connect_empty` path for second process |

So multi-window is **protocol-ready, UI incomplete**. Re-implementing app chrome is a **product feature task**, not a pure refactor. Prefer finishing UI against existing `GuiClient` methods rather than inventing a new protocol.

---

## Testing — what we have

**123 tests** covering:

- ctl parsers (send/finish/note/watch/caps/timeline/…)
- control wire serde (session alias, finish/note/seize/task)
- engine pure helpers (status vocab, pad I/O, self-only)
- engine control plane (list scope, status-set, note rev, finish evidence, task lifecycle, seize/release/drive) via `Engine::bare_for_test`
- agency, caps, cmdlog, snapshot, state, fileview, prompts, scratchpad

**Env isolation:** `state::test_env_lock()` is shared with engine tests so `SEANCE_STATE_DIR` mutations cannot race.

**Still thin / missing (good next tests):**

- engine `handle_gui` (Attach empty window, transfer/collect) with fake GuiConn
- grid damage / bin path (snapshot tests exist; engine throttle not unit-tested)
- app pure helpers once extracted (`title_looks_busy` was WIP; not in HEAD app)
- e2e script `./scripts/e2e-thorough.sh` / collab test after orchestration changes

---

## Safe plan for the rest of the refactor

### Hard rules

1. **Checkpoint commit** (or stash with message) before each slice of `app.rs`.
2. One slice → `cargo test` → only then next slice.
3. Prefer `impl SuperType` in child modules (child can see private fields of parent types in Rust).
4. Never bump gpui / zed / alacritty / gpui-component revs casually (`docs/PLAYBOOK.md`).
5. Stay on `master`; never push unless asked.
6. Live sessions: upgrade/restart-gui only.

### Recommended order (app)

| phase | extract | target |
|-------|---------|--------|
| A | Action structs + `SEANCE_ARM_PROMPT` | `app/actions.rs` |
| B | Pure helpers (tips, layout.json load/save, status colors) | `app/util.rs` / `app/layout.rs` |
| C | Free render helpers (`render_pane`, help) | `app/chrome.rs` |
| D | Overview methods only | `app/overview.rs` + `impl SeanceApp` in child |
| E | Sidebar / host render | `app/sidebar.rs` |
| F | Tiles + sash | `app/tiles.rs` |
| G | Leftover core in `app/mod.rs` | aim &lt; ~1500 LOC |

**Do not** start with D–F until A–B are green. The failed attempt died on D–F style cuts.

### Optional engine follow-ups (lower risk than app)

- Peel `handle_gui` / grid push into `engine/gui.rs`
- Peel spawn/kill into `engine/spawn.rs`
- Keep `handle_control` where it is (already separate)

### Optional ctl follow-ups

- Move giant `SKILL_TEXT` to `ctl/skill.md` or `include_str!`
- Split wait harvest helpers if wait.rs grows again

---

## Working tree snapshot (at handoff)

Uncommitted (typical):

- **New:** `src/ctl/`, `src/runtime/engine/`, this doc, design pass notes
- **Deleted as files:** `src/ctl.rs`, `src/runtime/engine.rs` (replaced by dirs)
- **Modified:** tests/helpers across agency/caps/control/scratchpad/state; protocol + gui_client multi-window API; small `app.rs` wire fixes; docs/changelog

**Not committed** by design unless bio-zack says so.

---

## Commands cheat sheet

```bash
cd ~/work/seance
./scripts/bootstrap-deps.sh          # if deps/zed missing
cargo test
cargo build --release
./target/release/seance upgrade      # daemon, preserve sessions
./target/release/seance restart-gui  # UI only
./target/release/seance ctl list --all
./target/release/seance ctl roster
./target/release/seance ctl doctor
./target/release/seance ctl skill
```

Agent collab / e2e (heavier):

```bash
./scripts/agent-collab-test.sh       # see docs/AGENT_COLLAB_TEST.md
./scripts/e2e-thorough.sh
```

---

## Suggested first messages for a new agent

**If finishing modularization:**

> Checkpoint commit first. Split `app.rs` only: actions + pure helpers in phase A/B. Run `cargo test` after each slice. Do not big-bang. Engine and ctl are already modular. No dep bumps. Stay on master; don’t push.

**If finishing multi-window UI:**

> Backend is ready (`GuiClient::set_overview/transfer_workspace/collect_all/connect_empty`, engine handlers, protocol). Wire `app.rs` UI to those APIs per CHANGELOG Unreleased. Do not invent new ops. Preserve sessions via upgrade/restart-gui only.

**If verifying only:**

> `cargo test && cargo build --release && seance upgrade && seance ctl roster` — all panes should remain.

---

## Success criteria for “refactor complete”

- [ ] No single source file &gt; ~1500–2000 LOC without a documented reason
- [ ] `cargo test` ≥ current 123, no flaky `SEANCE_STATE_DIR` races
- [ ] `seance upgrade` preserves sessions on a live box
- [ ] Multi-window either fully wired in app **or** CHANGELOG Unreleased demoted to “backend only / not shipped”
- [ ] Handoff / CHANGELOG updated; version bump only if shipping

---

## Contacts / canon docs

| doc | why |
|-----|-----|
| `CLAUDE.md` | agent rules, session survival, product contracts |
| `docs/PLAYBOOK.md` | GPUI pins |
| `docs/CONTROL.md` | ctl protocol |
| `docs/DAEMON.md` | daemon / upgrade |
| `docs/HOST.md` | host bridge |
| `CHANGELOG.md` | user-facing deltas |
| this file | refactor handoff |

---

*Written after live smoke (upgrade + restart-gui, 9 panes intact) and 123 green tests. Prefer evidence over vibes.*
