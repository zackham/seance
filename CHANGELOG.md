# Changelog

All notable changes to seance are documented here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)-ish.
Versioning: `0.9.x` while the product is still finding its shape — bump
**patch** for daily-driver / orchestration shippable batches, **minor** only
for deliberate breaks. Date is UTC commit day of the version tag/commit.

When shipping a versioned commit (`seance 0.9.N — …`):

1. Bump `Cargo.toml` / `Cargo.lock` / README status line
2. **Add a section at the top of this file** (same commit)
3. Update any version-pinned contracts in `CLAUDE.md` if behavior changed

Unreleased work can sit under `## [Unreleased]` until the version bump.

---

## [Unreleased]

## [0.9.22] — 2026-07-22

### Changed

- **Workspace sidebar auto-sorts; drag-reorder removed.** Circles with an
  actively working agent (title spinner / agent status) float to the top;
  otherwise order is last human touch — typing into any terminal in that
  circle, or right-click header → **touch**. Selecting a workspace alone
  does not reorder. Pane drag between circles is unchanged.
- **Claude account strip is collapsed by default** — only the current account
  shows. Click it (or the section title) to expand the full list; click
  another account to switch and collapse. Clicking the already-selected
  account is a no-op.
- **Finishing work bumps sidebar recency** — when a circle stops having any
  live-working agent (status or title spinner), it is touched so it sits at
  the top of the non-working band.
- **GUI relaunch restores last selected workspace** — after `restart-gui` /
  last-window close, the sole reattaching window selects the prior circle
  (and focused pane) instead of jumping to the first in order.

## [0.9.21] — 2026-07-22

### Fixed

- **Quicklaunch selects its new workspace.** GUI-requested spawns now update
  the requesting window's selection daemon-side before the State push — the
  push used to carry the old selection and revert the GUI's switch (invisible
  for same-workspace summons, visible for quicklaunch's fresh workspaces).

## [0.9.20] — 2026-07-22

### Fixed

- **Quicklaunch modal inputs are clickable** — mouse events were falling
  through the overlay to the terminal underneath, which stole focus on
  mouse-down (`.occlude()` on the overlay). Same fix applied to the overview
  root, where dead-space clicks silently focused hidden panes.

### Changed

- **Quicklaunch always opens a fresh workspace** named after the entry
  (uniquified: vita, vita-2, …) with a single pane and no rename prompt.
  The `workspace` config field and modal input are gone; a legacy
  `"workspace"` key in the JSON still parses and is ignored.

## [0.9.19] — 2026-07-22

### Added

- **Quicklaunch management UI** — the strip is now editable in place:
  right-click a chip for **edit… / remove**, **drag-drop** chips to reorder
  (insert-before, same as sidebar rows), and a **`+`** button on the title
  row opens a modal editor (name / cwd / command / workspace; Enter saves,
  Esc cancels; empty or colliding names block the save with a hint). All
  changes persist atomically to `~/.config/seance/quicklaunch.json` — the
  file stays the source of truth and hand-edits still hot-reload. Caveat:
  unknown JSON fields don't survive a UI edit (serde round-trip).

## [0.9.18] — 2026-07-22

### Added

- **Quicklaunch strip** in the sidebar (above the claude-accounts host
  strip): configurable one-click buttons that spawn a terminal in a chosen
  working dir running a chosen command. Config at
  `~/.config/seance/quicklaunch.json`:
  `[{"name": "vita", "cwd": "~/work/vita", "command": "claude"}]` —
  `command` omitted = plain shell; optional `"workspace"` targets/creates a
  workspace (default: selected). Hot-reloads on file edit (~2s mtime watch);
  a bad edit keeps the previous entries. Hidden when the file is
  missing/empty.

## [0.9.17] — 2026-07-22

### Changed

- Overview cards get a **hover effect** (lifted bg + warm border) so it's
  obvious they're click-to-select.

## [0.9.16] — 2026-07-22

### Fixed

- **Overview no longer shows blank cards** for workspaces you haven't visited
  since GUI start. The GUI's CPU guard (drop grid frames for non-selected
  workspaces) was also eating the daemon's overview open-flush; the guard now
  stands down while overview is open. This regression was fixed once before
  in an app.rs working tree that got rolled back — re-fixed at the guard
  itself with a comment explaining the interplay. Workaround was
  select-workspace + resize; no longer needed.

## [0.9.15] — 2026-07-22

Multi-window completion + overview that actually fills the screen.

### Changed

- **Overview (`ctrl+shift+space`) fills the viewport**: workspace cards split
  the window into an equal grid (spacer-padded rows, equal card widths); pane
  thumbnails letterbox up to but never above **1× native resolution**. No more
  postage stamps huddled in the corner.
- Grid damage-decode failure now repairs with a **targeted per-pane FULL
  frame** (`refresh_grid`) instead of re-attaching the whole window.

### Added

- New remote panes request their first FULL frame on mount — workspaces
  arriving via **transfer / pull / collect** paint immediately instead of
  waiting for the daemon's delayed flush.
- **Empty second window** shows pull instructions on the stage (right-click
  sidebar to pull / send from another window) instead of the summon hint.
- **Drive-mode chip** in the pane header when a pane isn't in default pair
  mode: `⛔ locked` (agents can't inject) / `⚡ led` (agent drives).

### Removed

- `Engine::full_state_event` (superseded by per-window state) — the last of
  the "protocol-ready, awaiting UI wiring" allows is gone; every multi-window
  API is now wired (`refresh_grid`, `flush_all_grids` via CollectAll,
  `empty_window` read-side).

## [0.9.14] — 2026-07-22

Codebase-health release: the full modular refactor the 0.9.13 handoff called
for, plus dead-subsystem removal, zero warnings, and a hard test/format gate.
No protocol break; live smoke = daemon upgrade + GUI restart with 9 panes
surviving. Adversarially reviewed as behavior-preserving (all moved bodies
diffed against baseline; state files load identically).

### Changed

- Split oversized modules for maintainability (no protocol break):
  - `app.rs` (5.9k LOC) → `app/{mod,actions,layout,util,chrome,pads,overview,sidebar,tiles,palette,workspaces}.rs` — core `app/mod.rs` now ~1.9k
  - `runtime/engine` → `engine/{mod,gui,spawn,control,helpers,tests,gui_tests}.rs` — `mod.rs` now ~0.6k
  - `ctl` → `ctl/{mod,parse,wait,print,phone}.rs`
- Expanded unit/integration tests (~76 → **143**): hermetic engine
  control-plane tests, **multi-window `handle_gui` tests** (attach/empty/
  transfer/collect/overview/bye/prune against captured GuiEvent payloads),
  layout.json parse round-trips, app pure-helper pins
- Zero build warnings (was 89), enforced by `scripts/check.sh`
  (fmt --check + deny-warnings check + tests)

### Removed

- Dead pre-daemon **local-PTY subsystem** (~1.4k LOC): `terminal.rs`,
  `terminal_view.rs`, `PaneBody::Terminal` and pane vestige. Compiler-proven
  unreachable — the live path is daemon PTYs (`pty_session` + `engine/spawn`)
  rendered by `remote_term_view`. Shared items live on in `term_shared.rs`.
- Dead in-GUI control-server cluster in `control.rs` (superseded by the
  daemon's own ctl serving), retired whisper compose + run-in-pane launch bar
  code, misc never-read fields/methods across the tree
- Multi-window protocol APIs not yet wired to UI are kept and marked
  (`refresh_grid`, `flush_all_grids`, `full_state_event`, `empty_window`)

### Fixed

- Restored multi-window **app UI** after accidental loss in refactor restore:
  workspace context menu (send to new window / peer windows / collect all),
  empty-sidebar right-click pull, same-process empty window, overview
  (`ctrl+shift+space`), minimize shelf, touch, hover banish ×, activity-band
  sidebar sort, title-spinner working badges
- Double context menu on workspace rows (empty-area pull/collect menu no longer
  nests on the scroller under circle menus)
- Tile **row sashes** (vertical multi-row resize) + `row_weights` in layout.json
- Whisper compose UI + run-in-pane launch bar removed from chrome (steer via
  agent TUI / `ctl send` / notes flip)
- Sidebar **working** badge uses *observed* TUI title spinners (Claude braille),
  not sticky `status-set working` — stale inject/open-task no longer marks
  idle circles; live agents without status-set now light up. Daemon also
  forwards title-only OSC changes (was skipped when cells unchanged).

### Added

- Sidebar: inactive workspaces show **working** / **needs** / **done** when panes
  are active or finished since last visit (collapsed circles stay scannable)
- Tile **row sashes** (vertical split resize) + layout.json `row_weights`
- **Minimize shelf** — only when the selected circle has shelved panes; chips
  only (no label). Hidden entirely when nothing is minimized
- Pane **right-click menu** — minimize, notes, rename, popout, move, banish
- **Overview** (`ctrl+shift+space`) — full-window live map of every workspace
  with scaled terminal grids (daemon streams non-selected circles while open)
- **Multi-window** — a workspace lives in exactly one window. Right-click a
  circle: send to new window / send to `name +N` / collect all here. Second
  `seance` process opens an empty window (right-click empty sidebar to pull).

### Removed

- **Whisper** UI (💬 compose bar / mid-flight inject chrome) — steer via the
  agent TUI, `ctl send`, or notes flip; ⚡ arm remains
- **Run in pane** agent launch bar (claude/codex/grok chips) — reclaim chrome;
  run profiles manually
- Sidebar **pane rows** and workspace **manual drag-reorder**

### Changed

- Pane chrome design pass: owner accent rail, shorter titles, quieter action
  cluster, higher inactive opacity (see `docs/DESIGN_PASS_2026-07-21.md`)
- Workspace list **auto-sorts**: working → needs → done-unread → rest, each band
  by activity recency (input / inject / status), not click-to-select or PTY paint

### Fixed

- Daemon upgrade handoff: stop closing/dup-racing the PTY master FD (idle shells
  were SIGHUP'd while busy Claude panes often survived); wait on I/O release
  flag; never respawn a fresh shell when SCM_RIGHTS adopt fails
- Pane sash resize: use GPUI `on_drag` / `on_drag_move` so resize works over
  markdown/file panes and across multi-row grids (was broken once the pointer
  left the 5px divider onto a selectable viewer)
- Notes flip: focus the notes editor after mount; re-steal if the terminal
  face FocusHandle still holds keyboard (could not type in notes)
- Host claude switcher: collapsed to active account only; click expands list
  (height slide down/up), pick collapses again

---

## [0.9.13] — 2026-07-21

### Added

- Host sidebar bridge (`src/host.rs`, `docs/HOST.md`) — optional JSON-polled chips (e.g. claude accounts); fail-closed
- Agent launch bar (claude / codex / grok → paste + enter into focused pane)
- Capture-phase global hotkeys; workspace cycle (`ctrl+pageup/down`) and pane cycle (`ctrl+shift+pageup/down`)
- Remember last focused pane per workspace; restore on circle switch
- Invariant: selected workspace with panes always has an active pane
- Terminal drag-select + copy toast; paste via inject path
- `ctrl+shift+w` banish (kill) active pane
- File-pane guidance in `ctl skill` / help (`new --file` vs bat/watch loops)

### Changed

- Process exit **auto-closes** the pane (no tombstone chrome); handoff/restore drop legacy exited panes
- Workspace switch forces FULL grid flush + local rev-gate open (fewer blank panes)
- Damage decode failures rate-limit reattach and clear rev when resyncing
- Sidebar selected-row fill shared for workspaces / panes / host chips

### Docs

- `docs/HOST.md`; orchestration/daemon process-exit semantics; README hotkeys + screenshot

---

## [0.9.12] — 2026-07-20

### Changed

- `ctl phone`: open telegram topic only (**no** `register_participant` claim); seed a **stage card** (workspace, roster, ctl how-to)
- Bound topic still receives optional needs-human one-liners

### Removed

- `export-session` HTML scrubber (half-measure); full continuous grid replay remains a filed epic, not this

---

## [0.9.11] — 2026-07-20

### Added

- Weighted multi-pane sashes (`n≥2`) with `layout.json` persist
- Pad drawer live-refresh; phone off UI thread + open telegram link
- Cmdlog serde + handoff/cold persist; gated shell cmd-end → idle
- Export v1 decision-timeline HTML (later removed in 0.9.12)
- `e2e-thorough.sh`, `upgrade-load-test.sh`

---

## [0.9.10] — 2026-07-20

### Added

- Pad drawer (stage chip / ▤): task inject body + scratchpad tail + phone status
- Pane chrome ☎ (`ctl phone`) and ▤ (pad drawer)

---

## [0.9.9] — 2026-07-20

### Fixed

- Flaky `seance upgrade` EAGAIN: blocking socket, longer timeout, flush+half-close, concurrent-upgrade gate
- FULL grids on GUI Attach; damage-decode resync without blanking when rate-limited
- FULL frames clear hyperlinks authoritatively

---

## [0.9.8] — 2026-07-19

### Added

- Stage strip (urgency-sorted roster chips); desktop notify on needs-human / ask
- Precanned prompts (`ctrl+shift+k`), fuzzy jump (`j`), focus-zoom (`z`), 2-pane sash
- OSC-8 / URL open; last-failed command (`f`)
- Event-driven `wait` wake; profile boot-clear after `--wait-ready`
- `ctl phone` (vita telegram topic); prompts library
- `seance --version` no longer launches the GUI

---

## [0.9.7] — 2026-07-19

### Added

- `wait --cat` / `--harvest` and `ctl harvest` (fan-in done + pad bodies)
- Task sidecars (`.taskid` / `.task.json`) next to scratchpad on inject
- Skill rewrite for worker/orch hot path; roster prefers slug; `task=` on roster

---

## [0.9.6] — 2026-07-19

### Added

- Task envelopes: `task_id` on send, durable inject inbox (`ctl task`), `finish --task`
- Evidence-bound `wait --status done` (pad growth since inject; `--badge-only` escape)
- Inject baselines persist on cold restart / handoff
- In-seance orchestrator collab test (`scripts/agent-collab-test.sh`)

### Changed

- `status-set done` gated on evidence; process exit → idle; pad defaults to self

---

## [0.9.5] — 2026-07-18

### Added

- Orchestrator A+: co-presence, dense brief/wait/roster, `send --file`
- `finish` / `note` with attributed atomic pads; `pad_rev` + since-inject wait
- Lifecycle persist across handoff/disk; codex full-access profile
- Multi-agent collab test harness + docs

---

## [0.9.0] — 2026-07-17

### Added

- Initial public release: multi-pane live terminals on a long-lived daemon
- Flip-notes scratchpads, file panes, control plane (`ask` / `propose` / status / `ctl skill`)
- Any agent CLI or shell as a first-class pane — not a single-vendor wrapper
