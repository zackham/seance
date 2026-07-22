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

### Changed

- Split oversized modules for maintainability (no protocol break):
  - `runtime/engine` → `engine/{mod,control,helpers,tests}.rs`
  - `ctl` → `ctl/{mod,parse,wait,print,phone}.rs`
- Expanded unit/integration tests (~76 → 123), including hermetic engine control-plane tests
- Refactor handoff for remaining work: `docs/HANDOFF_REFACTOR.md`

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
