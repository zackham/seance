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

## [Unreleased]

## [0.15.1] — 2026-08-04

### Fixed

- **Awaken lands the keyboard in the circle.** Every awaken affordance is a
  click, and a click leaves focus on the thing clicked — the bar button, the
  menu item — so waking a circle left you unable to type until you clicked a
  pane. Both GUIs now focus the circle's pane as part of waking. Native holds
  it in `pending_focus`, which survives the daemon round-trip and applies on
  the first render after the relaunch; web focuses the pane and blurs the
  button (a focused `<button>` also eats Enter/Space, which would re-fire the
  wake). Same family as the summon and empty-circle focus bugs: the pane view
  is alive the whole time, so nothing re-focuses it unless we say so.

## [0.15.0] — 2026-08-04

Sleep a circle you're done with. It stops costing RAM and keeps its mind.

### Added

- **Sleep / wake.** A sleeping pane has **no process** — the PTY is gone and
  so is everything the agent was holding (a claude pane plus its MCP children
  runs a few hundred MB; forty of them is most of a workstation). What
  survives is everything that makes the pane itself: slug, name, circle, cwd,
  command, its claude conversation id, scratchpad, task, status — plus the
  **last frame it rendered**, frozen to `<state-dir>/frozen/<slug>.scg` and
  served in place of a live grid, so a slept circle still reads with nothing
  behind it. Waking relaunches `claude --resume <id>` in the same cwd.
- **Only where it can be undone.** Sleep is offered for a claude pane with a
  session id *and* a transcript on disk, or a file pane. A shell can't be
  rebuilt — cwd drift, history, running children — so one shell vetoes its
  whole circle, and the refusal names it. Sleeping is a deliberate crash;
  it is only safe because 0.14.2 gave each pane its own conversation id.
- **Reading a sleeping circle.** Its panes still show what they were showing,
  dimmed to 45% with a scrim: readable, and unmistakably not live. The
  **awaken bar sits above the tile area**, never over the content, and says
  what you're looking at rather than letting a frozen frame pass for a live
  one. Right-click a circle for `sleep circle` / `awaken circle`; the sidebar
  marks it `☾`. Both GUIs.
- **`seance ctl sleep [WS]` / `wake [WS]`**, and `ctl list` renders
  `state=asleep`. **`ctl send` to a sleeping pane wakes it** — anything else
  breaks every orchestrator the first time it addresses a circle that dozed
  off. Typing into a sleeping pane wakes it too (the waking keystroke is
  dropped rather than fed to a half-drawn prompt); scrolling doesn't —
  reading the frozen frame is what it's for.
- **Auto-sleep at 12h idle**, swept every 5 minutes. Restorable circles only,
  and only ones with a real activity clock — no observation is not evidence
  of idleness. Idle means the daemon's own clocks (last pane output, floored
  by last human input), the same pair the sidebar row shows.
- **Slept stays slept** across daemon restart and `seance upgrade`. Bringing
  a slept circle back up on restore would defeat the entire point.

## [0.14.2] — 2026-08-04

A circle stops working on its own, and the working band holds still.

### Fixed

- **A finished circle leaves the working band without being clicked.** The
  sidebar's working badge reads a pane's TUI spinner out of its OSC title —
  but titles ride on grid frames, and grid frames only go to the window that
  has that workspace *selected*. Every other circle's title was therefore
  frozen at whatever it wore when you looked away: the spinner it had while
  working. So a circle stayed "working" until you clicked it, at which point a
  fresh frame arrived and it dropped out instantly — the click looked like the
  cause. The daemon now owns the verdict (it is the only party that sees every
  title) and **broadcasts busy flips to every connection, subscription-blind**
  (`GuiEvent::PaneBusy`, edge-triggered; `PaneInfo::busy` seeds a fresh
  attach). Both GUIs read that instead of a local title. One detector,
  `seance_core::util::title_looks_busy`, shared by daemon and both clients.

### Changed

- **The working band sorts A–Z**, in both GUIs. It used to sort by when each
  circle started working, which meant the top of the sidebar reshuffled every
  time an agent picked up or finished — exactly the rows you're trying to read
  are the ones that move. Alphabetical is stable while a dozen agents run.
  Idle circles keep the recency sort (last output, human touch as floor), and
  a circle still gets a touch the moment it finishes so freshly-done work
  lands at the top of the idle band.

- **`ctl` targets the pane you named.** Pane lookup resolved `slug == key ||
  name == key` in one pass, so an *earlier* pane's display name shadowed a
  *later* pane's slug — with two panes named `term-2` (the second being slug
  `term-2-2`), `ctl send term-2` drove the wrong pane and silently skipped the
  one asked for, injecting into its neighbour twice. Slug now wins over another
  pane's name, and `scope` narrows the candidates *before* matching so a scoped
  call disambiguates instead of erroring. "No such pane" is still distinct from
  "outside your workspace". Regression test:
  `slug_beats_another_panes_display_name`.

- **A claude pane owns its conversation across daemon death.** Spawning a
  claude pane now mints a UUID and passes `--session-id <uuid>`; the id is
  persisted on the pane (`PersistedPane::claude_session`, serde-default so old
  state files load) and carried through a graceful upgrade
  (`HandoffPane::claude_session`). Restore relaunches with `--resume <uuid>`.
  Before this, restore ran the bare persisted command, so a daemon crash
  silently started a *fresh* conversation in every pane — on 2026-08-04 a
  system OOM took the daemon down and all 49 panes came back empty (the
  transcripts were fine; nothing pointed at them).
  `--continue` is deliberately **not** the restore path: it resumes the most
  recent conversation *in the cwd*, so the 42 panes sharing `~/work/vita` would
  every one of them have landed on the same conversation. `resume_on_restore`
  stays as the fallback for panes persisted without an id.
  A pane created but never prompted has no transcript, and `--resume` on a
  missing id exits non-zero (which closes the pane) — that case re-asserts
  `--session-id` instead, so the pane keeps its identity either way. A command
  that already carries an explicit `--resume`/`--session-id` is adopted as-is
  rather than re-minted.

## [0.14.1] — 2026-08-02

Remove one PR ref and have it stay removed; cycle in the order you see.

### Added

- **Remove a single PR ref, both GUIs.** Hover a row in the chip popover or
  the PR board and a **✕** drops just that reference — no more clearing the
  whole circle to be rid of one stale link.
- **Removal is sticky.** A TUI repaint re-emits the same bytes, so a plain
  removal used to undo itself within seconds: the daemon now keeps a
  per-workspace **dismissed set** (cap 32, oldest evicted) that blocks the
  re-scrape at its single choke point. It persists across daemon restart and
  `seance upgrade` handoff, carries through rename, and drops with the
  workspace. A new *distinct* URL is still tracked, `seance ctl pr-link add`
  un-dismisses, and both single-URL and clear-all `pr-link clear` dismiss
  what they remove.

### Fixed

- **`ctrl+page` cycles the order you're looking at.** 0.14.0 rotated a ring
  in workspace *creation* order, so "next" landed somewhere visually random.
  Cycling now walks the **displayed sidebar order**, frozen for the duration
  of a 2-second keypress burst so a mid-burst resort can't reshuffle the
  rotation under your fingers. Both GUIs.

## [0.14.0] — 2026-08-02

The PR board: every circle's open PRs in one sweep, needs first.

### Added

- **PR board, both GUIs.** A `PRs (N)` button in the sidebar — visible only
  when something is open — opens a full-content overlay that sweeps every PR
  link the daemon knows, **circle first**: one section per circle, circles
  that need you first, and inside each the same order. A row is `repo#N` plus
  draft / CI / review glyphs, its age, the last review or comment, and a
  **push or close** mark once it's been quiet four days. Merged and closed PRs
  stay in the list but read muted; a header counts open / draft / done. The
  same PR pinned by two circles is a mistake, not a duplicate to hide, so it's
  annotated *also in <circle>* in both places. Click a row to open the PR,
  a section header to select that circle; escape or a backdrop click closes.
- **Structured PR state on the wire.** `PrStatus` gains `is_draft`,
  `ci` (`pass` / `fail` / `running`), `review` (`required` / `approved` /
  `changes`), and `opened_ms` / `last_review_ms` / `last_comment_ms` — so the
  board can sort and age PRs instead of reprinting one label string. All
  fields are `#[serde(default)]`: a 0.13-shaped `pr_watch.json` still parses,
  and the vita `gh` poller now supplies them.

### Changed

- **A PR verdict *change* bumps the circle's recency clock**, so a PR going
  red (or green) floats its circle the way pane activity does. Transitions
  only — identical re-polls and the neutral backfill at boot never reshuffle
  the sidebar under you.
- **Chip and popover lead with the repo name** (`repo#12`), not a bare `#12`.
  The `org/` prefix appears only when the links in view span more than one
  org — the common single-org case stays short.

## [0.13.0] — 2026-08-01

PR links: the pull request you're waiting on becomes a circle that asks for you.

### Added

- **PR links are scraped, not configured.** The daemon reads every byte of
  pane output already, so `gh pr create` (or a paste, or an agent echoing a
  URL) is enough: GitHub PR URLs are pulled out of the raw PTY stream —
  ANSI stripped, safe across chunk boundaries — and attributed to that pane's
  workspace, most recent last, capped at 8 per circle. The list persists
  across daemon restart *and* `seance upgrade` handoff.
- **A watcher seam maps PR state onto attention.** An external poller owns
  the judgment and writes `<state-dir>/pr_watch.json`
  (`{url: {state, attention, label, updated_ms}}`, atomic); the daemon
  mtime-polls it every 2s and merges verdicts onto links it already scraped —
  it never decides anything itself. Vita's `gh` loop (every ~3min) is the
  reference watcher. `needs` (changes requested, CI failing, new comment)
  lights the circle exactly like an agent asking for help, so a **parked**
  circle with a red PR resurfaces via the parked dot; approved + green reads
  as `done`. Pane-needs still outranks it: pane-needs > working > pr-needs >
  sticky / pr-done.
- **Header chip, both GUIs.** The selected circle's most recent PR renders as
  `#N` + the poller's label, colored by verdict; click opens the PR in a
  browser, the caret (or right-click) opens a popover listing every link plus
  *clear PR links*.
- **`seance ctl pr-link add WS URL` / `pr-link clear WS [URL]`** — seed a link
  by hand (backfill, or a PR nobody printed in a pane) or drop one/all.
  Cap-checked as `pr_link_add` / `pr_link_clear`.

### Changed

- **Wire addition (additive, not breaking):** `WorkspaceMeta` gains
  `pr_links: Vec<PrLink>` (`{url, status?, seen_ms}`, `PrStatus` =
  `{state, attention?, label, updated_ms}`). The field is `#[serde(default)]`,
  so a 0.12-shaped payload still parses — version lockstep on hello applies as
  always, but nothing in the old wire changed meaning.
- **Ctrl+page cycles a stable ring.** Cycling now walks `workspace_order`
  instead of the recency sort, so the next circle doesn't move under you while
  panes chatter.
- **Workspaces that lose their last pane are pruned**, unless they were
  created explicitly (`extra_workspaces` are exempt). No more empty circles
  accumulating behind a killed agent.

## [0.12.0] — 2026-08-01

Subscriptions replace ownership: a circle can be live in every window at once.

### Changed

- **Workspace ownership is gone.** A workspace was exclusively owned by one
  GUI window; to see it elsewhere you had to *pull* it away from wherever it
  was. Now each connection carries its own subscription set, so the same
  circle renders on the desktop, the laptop and the browser simultaneously —
  no tug-of-war, no "elsewhere" limbo.
- **Both sidebars split active / parked.** Active = what this window
  subscribes to, rendered exactly as before; everything else collapses into
  one **parked (N)** accordion below with the same sort and attention badges.
  Selecting a parked row subscribes it; row menus gain *park* and *add to
  active*; ctrl+page cycles the active list only. Panes spawned by `ctl` land
  parked and badge **needs** until you first select them.
- The active set persists per client and is replayed on attach — native
  `~/.config/seance/subscriptions.json` (`{active, seen}`, atomic, shared with
  the reconnect supervisor), web `localStorage["seance_active"]`. No stored
  set = subscribe everything, so existing installs come up unchanged.
- Grid fan-out is now per subscriber: 16ms for a subscriber's selected
  workspace, 66ms for subscribed-but-not-selected (and overview), nothing for
  a workspace nobody watches — the minimum across connections wins, and a
  full grid goes out whenever any interested window doesn't have it selected.
- Recording is independent of watching: the recorder taps ahead of fan-out,
  so a zero-subscriber pane still records to the 48h ring.

### Breaking

- **Wire change** (all clients must match, as usual): `GuiRequest::Subscribe`
  / `Unsubscribe` replace `TransferWorkspace` / `CollectAll`, which are
  deleted. `GuiEvent::State` is now global (panes, statuses, asks, order for
  every workspace) plus a `subscriptions` field; `foreign_workspaces` is
  gone. `Attach` carries `subscriptions: Option<Vec<String>>` — `None`
  subscribes to everything (migration path), `Some([])` is an empty window.
  `Engine::flush_all_grids` retired.

## [0.11.2] — 2026-08-01

### Added

- **✦ GUI census** (both GUIs): click the brand ✦ for a popover listing every
  connected GUI window with a remote **kill** (`CloseWindow` → `Kicked`; the
  kicked client stops reconnecting and says who closed it), plus version and
  grimoire shortcuts. The buried-tab problem dies here.
- `docs/design/SUBSCRIPTIONS.md` — the ownership→subscriptions design
  (active/parked, multiplayer, PR links) awaiting a decision pass.

### Changed

- Sidebar sort matches the displayed clock: working band ordered by
  when-work-started (stable, newest first); idle band by last output.
  Web "elsewhere" rows show the activity time instead of the (identical)
  host-window label.

## [0.11.1] — 2026-08-01

### Fixed

- **Workspace activity clocks are daemon-owned** — time-since-output labels
  and auto-sort recency now survive GUI relaunch, workspace pulls, `seance
  upgrade` (handoff) and cold daemon restarts (state.json). Stamps originate
  in the recorder (the one place that knows "real content change at unchanged
  dims"), throttled to one note per pane per 5s; `State` carries
  `workspace_meta`, live updates ride a new `Activity` event. Attach / pull /
  relaunch / resize reflows no longer reset any clock.
- **The jump-focus class**: every chrome navigation (ctrl+shift+j palette,
  tile clicks, workspace switches, palette close) now arms `pending_focus` as
  a render-time backstop — no more "jumped but can't type until I click".
- Banishing the ACTIVE workspace selects the neighbor below (above when last)
  in sidebar order, both GUIs.
- Ctrl+page cycling reveals the newly selected workspace in the sidebar
  (native `scroll_to_item`; web `scrollIntoView`) instead of leaving it
  scrolled out of view.

## [0.11.0] — 2026-07-31

The web era: seance from anywhere, and sessions you can share.

### Added

- **Web client** (`seance web`): a wasm thin client speaking the daemon's GUI
  protocol over a websocket bridge (token-auth'd, tailscale-transport policy).
  WebGL2 glyph-atlas terminal renderer (paint p95 0.4ms), native chrome parity
  (auto-sorted workspace rail, quicklaunch, accounts strip, context menus,
  keymap with `alt+` twins, grimoire, activity drawer), pull-workspace from
  the sidebar. `crates/seance-core` carve: the sans-io wire protocol, SCG3
  codec, and input encoding shared by every client. `docs/WEB.md`.
- **Session replay** (`docs/REPLAY.md`): an always-on 48h ring recorder in the
  daemon (output-driven; keyframes before every human prompt), a player that
  treats the timeline as ACTIVITY (idle collapses to a beat, typing plays at
  real cadence; recorded-resolution letterboxed panes; prompts rail +
  Previous/Next fly-to; `#t=` deep links), a web trim/publish editor shared by
  all GUIs (workspace right-click → *share replay…*), `seance replay`
  export/list/edit CLI, and an arms-length publisher seam
  (`~/.config/seance/publish.json`; bundles are self-contained static sites
  by default).

### Changed

- Echo latency on the web path: p50 51.5ms → 9.1ms (bridge 2ms poll +
  typing-hot immediate paint; the daemon already bypassed its throttle).
- Sidebar shows time-since-last-output instead of pane counts; banish × is
  hover-only; summon focuses the terminal immediately (rename moved to
  double-click); quicklaunch strip renamed "vita quicklaunch"; notes-flip
  keeps editor focus on click (native).

### Fixed

- Bridge no-cache on `.wasm`/`.css` (stale-module ABI breaks).
- Replay export resolves workspaces of exited panes from the ring's own
  `Spawned` events — sharing after the work is done is the normal case.

## [0.10.9] — 2026-07-30

### Fixed

- **Pagers work in panes again** (`git log` paged instead of dumping). The
  daemon inherits the environment of whoever (re)started it — an agent
  session running `seance upgrade` leaked its non-interactive overrides
  (`PAGER=cat`, `GIT_TERMINAL_PROMPT=0`, …) into every spawned pane. Pane
  PTYs now scrub agent-ish env (PAGER/GIT_PAGER/SYSTEMD_PAGER/
  GIT_TERMINAL_PROMPT/CI/DEBIAN_FRONTEND/NO_COLOR) at spawn — panes are
  human terminals by definition.

## [0.10.8] — 2026-07-30

### Fixed

- **Typing lag, part 5 (the big one): every frame cost 66ms because the
  chrome fonts didn't exist.** gpui-component's stock theme requests
  `.SystemUIFont` (a macOS pseudo-family that never resolves on Linux) and a
  Linux mono default (`DejaVu Sans Mono`) that isn't installed here. gpui
  caches the failed lookup but re-materializes the error object on every hit
  — and with `RUST_BACKTRACE=1` (which the GUI sets for crash logs) each hit
  captures a fresh backtrace, thousands of times per frame inside taffy text
  measurement. Stack-sampling showed ~90% of `Window::draw` inside
  `TextSystem::font_id`. Theme init now picks the first actually-installed
  family (sans: Liberation Sans → Noto Sans → …; mono: JetBrainsMono Nerd
  Font → …) via `all_font_names()`. **Window::draw p50 66.3ms → 3.7ms.**

### Added

- `[seance lat] gpui draw` gauge (gpui frame tracing drained every 5s) and
  `gui render→paint` — the probes that located the frame cost.

## [0.10.7] — 2026-07-30

### Fixed

- **Typing lag, part 4 (the GUI half): grid frames now actually schedule a
  window frame.** In this gpui build a pane-entity `notify` never produced a
  frame — the window only repainted on the 240ms sidebar-spinner tick, so a
  keystroke's echo (or any grid update) sat applied-but-unpainted until the
  next tick. Measured: `apply→paint` p50 ~68ms / p95 ~80ms, `render gap`
  pinned at 233–250ms, real-typing `key→paint` p50 90ms / p95 271ms. (The
  0.10.4 spinner slowdown 80ms→240ms silently made this worse — the spinner
  was the app's only frame clock.) The daemon-event batch loop now kicks a
  root notify per applied **visible** grid batch: immediately while a human
  keystroke is in flight (~250ms typing-hot window), throttled to ~30fps for
  plain streams, with a 33ms deferred kick so burst tails never wait for the
  spinner. Root notify → render measured at p50 2.9ms / p95 15.3ms.
- **Bridge events apply in batches — one render cycle per batch.** The old
  loop did one `update` per daemon event, interleaving render work between
  events; a keystroke's echo could queue behind a grid backlog (measured
  bridge→apply p95 198ms). One `update` now drains everything queued
  (cap 512), then decides on a single frame kick.

### Added

- More `[seance lat]` gauges: `gui bridge age`, `gui grid apply`,
  `gui render gap`, `gui paint replay` / `gui paint fresh`,
  `gui apply→paint`, `gui kick→render`.

## [0.10.6] — 2026-07-30

### Fixed

- **Typing lag, part 3: the daemon's PTY I/O loop was the latency floor —
  now event-driven.** Every PTY session thread ran a `sleep(8ms)` poll loop:
  a keystroke waited up to 8ms just to be *written* to the PTY, and its echo
  waited up to another 8ms to be *read* back — measured keystroke→grid-push
  p50 11.7ms / p95 15.8ms inside the daemon, before the GUI ever saw a frame.
  The loop now blocks in `poll(2)` on the PTY master + a self-pipe that every
  write enqueue pokes. Measured after: **p50 0.3ms / p95 0.4ms** idle, and
  **p50 0.4ms / p95 0.7ms / max 3.4ms** while three panes flooded full-rate
  output (engine-mutex wait stayed ≤0.7ms — no contention). Side effect: 22
  session threads no longer wake 125×/s each while idle.
- **Keystroke echo frames bypass the 16ms selected-workspace grid throttle.**
  A human `Input` clears the pane's last-push stamp, so the echo's Wakeup
  pushes immediately instead of riding the coalescing timer. Bounded by
  typing rate — output storms still throttle exactly as before.

### Added

- **Always-on typing-latency probes** (`src/latency_probe.rs`), reported as
  5s aggregates tagged `[seance lat]`: daemon `input lockwait` / `input
  handle` / `input→gridpush` (daemon stderr → daemon-upgrade.log), GUI
  `key→grid-apply` / `key→paint` (gui.stderr.log). Per-session-event pump
  probes are additionally gated behind `SEANCE_DEBUG_RENDER=1` (storms drive
  that loop at 100k+ events/s).

## [0.10.5] — 2026-07-30

### Fixed

- **Typing lag, part 2: 9× cheaper grid reshapes.** gpui's line cache only
  spans two consecutive frames, and grid repaints are spaced by the replay
  path — so every live-pane repaint cold-shaped all visible text (~14ms per
  pane; several busy claude panes saturated the UI thread). Shaped runs now
  live in a durable content-addressed cache (text+style+metrics → ShapedLine,
  shared across panes): a spinner tick re-shapes only the changed run
  (reshape avg 14.4ms → 1.6ms). Paint accounting added to
  `SEANCE_DEBUG_RENDER=1` (`paint-probe`: replays/reshapes/avg).

## [0.10.4] — 2026-07-30

### Fixed

- **Typing lag on many-workspace sessions** — the sidebar working-spinner
  animated at 80ms, and each animation tick re-renders the entire window
  (~55ms/frame at 24 circles), saturating the UI thread so keystroke frames
  queued behind it. Spinner now ticks at 240ms (~85% → ~20-50% CPU). New
  `SEANCE_DEBUG_RENDER=1` prints a renders/s probe to the gui log. The
  underlying frame cost at high workspace counts is tracked separately.

## [0.10.3] — 2026-07-30

### Added

- **cmd+c / cmd+v in terminals on macOS** — native copy/paste chords work
  alongside ctrl+shift+c/v (cmd never reaches the PTY, so nothing is stolen
  from TUIs).

## [0.10.2] — 2026-07-30

### Fixed

- **Quicklaunch from a thin client spawns correctly** — the chip click was
  expanding `~` in the entry's cwd on the CLIENT machine before sending, so a
  mac client sent `/Users/...` to a linux daemon and the spawn died silently.
  cwd now travels raw and expands daemon-side (which the engine already did).
- **Spawns with a missing cwd fall back to `~`** (loudly, in the daemon log)
  instead of failing as a dead click; the GUI also logs daemon-rejected
  requests to `gui.stderr.log`.

## [0.10.1] — 2026-07-30

### Fixed

- **Terminal font fallbacks** — the hardcoded `JetBrainsMono Nerd Font` now
  falls back to Menlo / JetBrains Mono / DejaVu Sans Mono / monospace when
  not installed (fresh thin-client machines rendered with a garbage
  substitute). Install the Nerd Font for the identical-to-desk look.

## [0.10.0] — 2026-07-30

### Added

- **Thin-client / remote mode** (docs/REMOTE.md) — run the GUI on one machine
  (including macOS) against a daemon on another, over a supervised
  ssh-forwarded unix socket. VNC-substitute intent: identical experience on
  both ends.
  - **Launch picker**: with no daemon reachable, choose "connect to remote
    host" (type an ssh destination) or "run locally"; the choice persists at
    `~/.config/seance/launch.json` and prefills next launch. CLI overrides:
    `seance --remote <host>` / `seance --local`.
  - **Tunnel supervisor**: seance spawns and supervises the
    `ssh -N -L` forward (BatchMode, keepalives, respawn with backoff),
    auto-starts the remote daemon (`seance _ensure-daemon` over ssh), and
    tears the tunnel down with the GUI (quit hook; PDEATHSIG on Linux).
    Auth failures print a copy-pasteable `autossh` stopgap.
  - **Strict version handshake**: the daemon refuses `ctl`/`gui` clients whose
    build doesn't exactly match — cross-machine skew fails loudly instead of
    corrupting the protocol (`handoff`/`upgrade` roles exempt).
  - **Everything from the daemon** (fs bridge): file panes (content, watch,
    snapshot history), scratchpads, the shared layout (all windows now share
    tiling via the daemon state dir), host widget chips (polled + selected
    daemon-side, pushed to every window), and GUI event-log writes (human UI
    actions land in the daemon's flight recorder). No GUI-side workspace IO
    remains; bridge ops run off the input path on both ends.
  - **macOS support**: portable uid/socket paths, `open` vs `xdg-open`,
    `ps` vs `/proc`; the pinned gpui revs build clean on darwin.
  - `SEANCE_SOCKET` targets any `seance ctl` at a forwarded socket; the
    daemon always binds its own local path (`bind_socket_path`).

### Fixed

- **Focus after new workspace / summon** — finishing the workspace-name
  rename on an empty circle no longer leaves keyboard focus dead (chords
  like `ctrl+shift+n` and typing did nothing until a click). Focus returns
  to the active pane when one exists, otherwise the app root so capture
  hotkeys keep working; after a summoned pane is named, keys land in the
  terminal without an extra click.

## [0.9.23] — 2026-07-29

### Added

- **`ctrl+shift+r` renames the selected workspace** — focuses the sidebar
  inline rename field; Enter commits and returns keyboard focus to the pane
  that was active (Escape cancels the same way).
- **Durable GUI stderr log** at `~/.local/share/seance/gui.stderr.log` (or
  `$SEANCE_STATE_DIR/`). Desktop launches no longer swallow panics — stderr is
  redirected on GUI start, `RUST_BACKTRACE=1` is set when unset, and the file
  rotates to `gui.stderr.log.1` above ~5 MiB.

### Changed

- **Working circles show a left-icon spinner** instead of a `"working"` text
  badge, so more of the workspace name is visible. `needs` / `done` badges
  are unchanged.

### Fixed

- **`ctrl+shift+w` kills the active pane only** — no longer banishes the whole
  workspace on the first press. The workspace is removed only when that was
  its last pane (or the selected circle is already empty).
- **Terminal copy no longer kills the GUI on Wayland** — `ctrl+shift+c` (and
  mouse-up auto-copy) prefer `wl-copy` so clipboard ownership lives out of
  process; GPUI's in-process write is only a fallback. Caps selection size,
  catch_unwind around notifications, logs copy outcome to `gui.stderr.log`.

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
