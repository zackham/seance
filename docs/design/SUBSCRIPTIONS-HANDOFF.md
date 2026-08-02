# handoff: build the subscription model

*for a fresh claude session. written by silicon-zack 2026-08-01 at the end of
the 0.10.9→0.11.2 run (web client, replay, census). context: zack approved
[SUBSCRIPTIONS.md](SUBSCRIPTIONS.md) in full — design questions are CLOSED,
this is now an execution job.*

## scope (in order; stop after 2 if quality would suffer)

1. **Engine: subscriptions replace ownership.** Delete `workspace_window`.
   Each `GuiConn` gets a `subscriptions: HashSet<String>` + existing
   selected/focused. Grid fan-out per subscriber: selected ws = 16ms,
   subscribed-visible = 66ms, unsubscribed = none. New GuiRequests:
   `Subscribe { workspace }` / `Unsubscribe { workspace }` (replace
   TransferWorkspace/CollectAll/pull — DELETE those variants and their
   handlers/menus outright; grep both GUIs). `State` loses
   `foreign_workspaces`; every workspace rides `workspace_meta` (already
   carries clocks for all). Orphan rule: pane spawned via a GUI → auto-added
   to that conn's subscriptions; ctl spawns → nobody's, badge `needs` via the
   existing attention machinery.
2. **Both sidebars: active/parked split.** Active = subscribed (pinned) set,
   rendered exactly like today. Parked = every other workspace, one collapsed
   group underneath, same sort (working band + last-output), select-to-
   subscribe. Persistence: native → state dir file; web → localStorage.
   Ctrl+page cycles ACTIVE only. "add to active"/"park" in row context menus.
3. *(separate later session — do NOT start)*: PR links + watcher
   (SUBSCRIPTIONS.md §PR links).

## non-negotiables (learned this run, the hard way)

- **Version lockstep**: this breaks the GUI wire → bump minor (0.12.0),
  CHANGELOG + README status per CLAUDE.md rule, then deploy in one motion:
  `cargo build --release && ./scripts/build-web.sh release && seance upgrade`,
  restart bridges + GUI. Hello is strict — a half-deploy bricks clients.
- **Verify in the browser, not just the compiler.** agent-browser drives the
  web GUI end-to-end (pull/subscribe flows are fully scriptable; see
  `docs/WEB.md`). Two web sessions can simulate multiplayer.
- **The daemon hosts live sessions** (zack's ~30 workspaces + the very pane
  you may be running in). `seance upgrade` is safe (handoff, proven ~10×
  this run); killing the daemon is not. Never `pkill -f` — the pattern
  matches your own wrapper shell (bit us twice); kill exact pids.
- **Recorder + replay must keep working** — the recorder taps
  (`engine/gui.rs`, `record_grid_tap`/`record_event`) and activity-clock
  stamps ride the paths you're editing. `cargo test --workspace` (325+) plus
  a record→export→play smoke after engine surgery.
- **Sub-agent pattern that works** (seance CLAUDE.md + PLAYBOOK): opus
  agents on disjoint files with exact API contracts, told to grep
  `~/.cargo/registry` / `deps/zed` instead of trusting memory, no cargo runs
  in agents — integrate and compile centrally.
- Known traps: browser caches wasm in-session (close the agent-browser
  session to bust); `&expr?` Element→Node coercion; editor↔player RefCell
  re-entrancy (try_borrow discipline); the jump-focus class (always arm
  `pending_focus` on navigation — pattern is in `set_active`).

## file map (where the work lives)

- engine: `src/runtime/engine/{mod,gui}.rs` (ownership sites: grep
  `workspace_window`, `foreign_workspaces`, `TransferWorkspace`,
  `CollectAll`, `grid_interval_for`)
- wire: `crates/seance-core/src/protocol.rs`
- native sidebar: `src/app/{sidebar,workspaces,mod}.rs`
- web: `crates/seance-web/src/{state,ui,lib}.rs`
- persistence: `src/state.rs` (AppState) / web localStorage
- docs to update when done: `docs/{WEB,DAEMON,CONTROL}.md` + CHANGELOG +
  README (the ✦ census popover already lists windows — it becomes the
  multiplayer roster, extend not replace)
