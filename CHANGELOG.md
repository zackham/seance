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

## [0.25.1] — 2026-08-17

### Changed

- **`ctrl+shift+p` pins the selected circle** (toggle), and pane **popout moves
  to `ctrl+shift+o`**. Pin is a rail verb reached constantly; popout is a
  once-in-a-while move that still has its `⇱` button and its row menu. The web
  client keeps `ctrl+shift+p` for the latency probe — a divergence the grimoire
  already documents.

## [0.25.0] — 2026-08-17

### Added

- **File panes copy.** Two buttons in the pane header: `⧉ path` puts the file's
  full path on the clipboard, `⧉ md` (`⧉ text` for a non-markdown file) puts
  the body there. Reading a doc in a pane and then wanting to hand it to
  someone — a path into a prompt, a section into a message — was a trip back to
  a shell for a file you were already looking at.

  What copies is the **source**, not the render: a folded outline still yields
  the whole document, and while pinned to history it's that snapshot's text —
  which the tooltip says out loud, because a silent copy of not-the-live-file
  is the paste you'd trust wrongly.

### Changed

- The Wayland copy detour (ownership in a `wl-copy` child, because GPUI's
  in-process write has killed the GUI with no panic to show for it) moves out
  of the terminal view into `src/clipboard.rs`. One copy seam, so the next
  surface that copies inherits the hard-won part instead of re-deriving it.

- **Pin leads the circle's right-click menu** (both clients). It's the item
  aimed at the rail the row lives in, and the one reached for often enough to
  want under the cursor.

### Removed

- **`touch (bump recency)`** — the manual bump. Recency already moves on its
  own (typing in a pane, a circle finishing work, a circle being created), so
  the menu item was a hand crank on a motor that runs. The automatic clock is
  untouched.

- **`fork workspace ⑂`, end to end** — never used, in a year of daily driving.
  Gone with it: `seance ctl fork`, `ControlRequest::WorkspaceFork`,
  `GuiRequest::ForkWorkspace`, the engine's respawn-and-copy-scratchpads
  implementation, the `workspace_fork` capability, and the web client's menu
  entry. **This is a wire break** — hence the minor bump, and both clients plus
  the daemon have to land together (see the version-lockstep rule).

## [0.24.0] — 2026-08-16

**Remote seance stops shipping whole screens.** Measured end to end by
replaying a real two-hour session's recorded frames through the new path:
**196 MB → 5.9 MB on the wire, 33x**, and the framing the daemon picks went
from 2,500 full grids to 2.

The starting point was a report that a remote window felt slow while a local
one did not. The daemon was healthy (`input→gridpush` p50 3.0ms) and the link
was not saturated (94 KB/s on a 20Mbps, 36ms path), so neither end was the
problem. What the recorder showed was the shape of the traffic: on a busy pane,
**full grid frames were 16% of frames but 92% of bytes**, averaging 55KB raw —
74KB after the base64 the wire uses — arriving up to four times a second. A
74KB frame is ~60 packets that a keystroke's echo has to queue behind on the
same TCP connection, which is what "slow" felt like.

Four causes, all fixed:

### Added

- **`SCZ3`, a deflate container for grid frames.** Terminal grids are extremely
  compressible: measured over 200 real full frames, 55,551 B → 4,558 B
  (**12.3x**), and row damage 3.8x, at 0.5ms per frame. It is a pure container
  — an ordinary SCG3 frame lives inside — so the recorder, the replay player
  and the frozen-pane store keep reading and writing raw frames, and
  `decode_grid_bin_onto` unwraps it for both clients at once. Level 6 chosen on
  measurement: level 1 gave only 7.9x for a quarter of the time saved.

- **`FRAME_SCROLL`: the screen scrolled, here are the rows that appeared.**
  Output arriving at the bottom of a terminal changes every row, which read as
  "more than half the screen changed" and sent a whole grid. It is now a shift
  the receiver applies before the damage rows. **A scroll frame is exact
  whatever delta the sender guesses**, because every row that does not match
  after the shift is carried in the body — detection quality only ever costs
  bytes, never correctness, which is what makes guessing safe.

- **Header-only frames.** A spinner tick in the OSC title changes no cells and
  used to cost a full grid; it now costs a few dozen bytes. That path alone was
  9.7 MB of one 30-minute segment.

- **A send window (`GuiRequest::GridAck`).** The 0.23 coalescer bounds the
  daemon's own queue, but ~2.4MB sits downstream of it that the daemon cannot
  see or merge: the unix send buffer, ssh's 2MB per-channel window, the TCP
  send buffer. That is where the 1053ms in the coalescer's own measurement was
  hiding. Clients now ack the frames they receive and the daemon keeps at most
  8 in flight, merging the rest — so staleness is capped at frames rather than
  megabytes. A client that never acks degrades to the old behavior after a 2s
  stall rather than freezing, and this closes the same hole in the web bridge
  (whose ws pump has its own unbounded queue) without touching it.

- **Instrumentation for the stage that was dark.** The daemon measured up to
  `gridpush` and the GUI measured from `bridge age`; nothing measured the queue
  and socket in between. Now `daemon grid queue wait` and `daemon socket write`
  do, plus a `[seance vol]` line carrying wire bytes and rate — the number that
  had to be reconstructed from `ss` counters to find this in the first place.

- **`-C` on the ssh forward.** Frames arrive deflated now, but they ride base64
  inside JSON lines and the control chatter around them is plain text.

### Changed

- **Framing is decided per connection, not globally.** Damage is only
  meaningful to a receiver holding the base it names — a per-connection fact
  that was being answered for everyone at once. Three amplifiers came out of
  that: a frame was promoted to a full grid for *every* window whenever any
  recipient lacked a base (an overview thumb watcher was enough); `Attach`
  cleared the daemon's shared last-frame cache, so **one window reconnecting
  cost every other window a full grid for every pane it was watching**; and the
  post-attach `ForceFullGrid` fired at everyone. Each is now scoped to the
  window that actually needs it. If the per-connection base is ever wrong the
  receiver's decode fails and it asks for a refresh — the error path is a
  resync, not a corrupt screen.

- **Wire version is a break.** Old clients cannot decode `SCZ3` or
  `FRAME_SCROLL`; the daemon's exact-build hello check already refuses them.
  Rebuild the web bundle (`./scripts/build-web.sh release`) with the daemon.

### Fixed

- **Grid frames coalesce per connection, so a bad link no longer makes you
  watch a backlog drain.** The daemon handed every push to an unbounded
  `mpsc` feeding a blocking socket write. On a healthy link that's free — the
  queue is always empty. On a degraded one it was the whole problem: the write
  blocks, the engine keeps generating frames at up to 62fps, and they pile up
  with no ceiling, so every frame that arrives is already stale. Measured on a
  bad wifi association: `key→grid-apply` p50 **1053ms**, max **8355ms**, while
  every GUI-side stage stayed under a millisecond — `bridge age` 0.0ms, `grid
  apply` 0.2ms, `gpui draw` 7.5ms. The GUI was fine. It was drinking from a
  queue.

  `runtime::outqueue` keeps **one pending frame per pane** and merges into it.
  The merge is exact, not lossy, which is what makes it cheap: damage is a
  list of dirty *row indices* and the payload is the current snapshot
  restricted to those rows — not a diff against a base. So composing N queued
  frames is "union the row sets, encode against the newest snapshot". No delta
  chain to break, and therefore **no full-frame resync** — which would have
  meant sending *more* bytes exactly when the link can least carry them.

  Healthy link: the queue is empty when each frame arrives, nothing merges,
  behavior is identical to before. Degraded: the backlog collapses to the
  newest state per pane. Recovery is immediate and automatic — there's no
  threshold, no hysteresis, and deliberately **no rate controller**, because
  once the queue can't back up the effective frame rate already *is* the
  link's measured capacity. A controller on top of that is two feedback loops
  fighting.

  Promotes to a full frame on a reflow (row indices stop naming the same
  cells) and once the union passes half the rows — the same threshold
  `broadcast_grid` already uses to pick damage over full in the first place.

  Encoding moved to drain time, so a backed-up connection encodes once per
  frame it actually sends instead of once per frame it never sends. The CPU
  saving lands on the machine that's already struggling.

  Only grids coalesce. `State`, `Ack`, `PaneSpawned`, `RailPrefs` hold strict
  FIFO — dropping one is a correctness bug. `State` was left uncoalesced on
  purpose despite being a complete latest-wins snapshot: it fires on discrete
  events (exit, title, spawn), not on the output path, so it isn't a volume
  problem, and a smaller change is the right one to make to a daemon someone
  else now depends on.

  Daemon-side only — no wire change, so `seance upgrade` alone delivers this
  to already-running GUIs.

- **⌘Q quits the macOS app.** Shipping the `.app` in 0.23.0 removed the only
  way to stop seance without adding one: there's no terminal to ctrl-C, and
  closing the window doesn't quit on macOS. The app had no menu bar at all, so
  ⌘Q hit nothing and force quit was the only exit. There is now a Seance menu
  with a Quit item; `cx.on_app_quit` already tore the ssh tunnel down, so the
  action only has to ask.

  Quit only. Hide / Hide Others / Show All are zed's own actions rather than
  gpui's and would each need a real implementation — a menu item that looks
  standard and does nothing is worse than no item.

## [0.23.0] — 2026-08-13

Your rail follows you to the other machine. And seance is an app on macOS.

### Changed

- **The daemon owns the rail arrangement.** Active/parked, pins, `seen`, and
  fold state moved from each client's `~/.config/seance/subscriptions.json`
  into the daemon's state dir beside `layout.json`, reached by two new
  `FsOp`s (`SubsLoad` / `SubsSave`). Open a window anywhere — desk, mac thin
  client — and the same circles are there, in the same bands, with the same
  ones pinned. Pin at the desk and the laptop's rail updates while you watch:
  a save broadcasts `GuiEvent::RailPrefs` to every attached window.

  This reverses a deliberate 0.12 decision, so it's worth saying why it was
  wrong. The old reasoning was that the active band is *this window's chrome*
  — true of tiling, which is why layout went daemon-side, and false of the
  rail. Which circles you keep in front of you is a fact about what you're
  working on, not about which machine you opened. Eleven pinned circles at
  the desk and a blank rail on the laptop wasn't a second view of the work,
  it was the arrangement failing to exist anywhere but one box.

  The local file survives as a **seed cache**, demoted from source of truth.
  It's read before connecting because `Attach` needs a seed, and opening on
  the arrangement you last saw beats attaching to all 47 circles and
  unsubscribing 35 of them a beat later. The daemon's copy is read straight
  after connecting and wins.

  Two edges worth knowing. A window that adopts the daemon's arrangement is
  still *attached* on whatever it seeded with, so the next `State` pushes the
  arrangement onto the connection rather than folding the connection's
  subscriptions into it — otherwise a fresh window, which attaches to
  everything, would bloom its rail back to every circle the instant it
  opened. And the broadcast goes to every window including the sender, so
  receiving one adopts without saving; saving in response would put one pin
  into an endless round trip.

  A daemon with no copy yet is seeded by the first window that connects,
  which donates its arrangement instead of everyone starting from a blank
  rail. Pushes run off the UI thread — pinning a circle shouldn't wait on a
  socket, least of all over ssh from the mac.

### Added

- **`./scripts/bundle-macos.sh` builds `/Applications/Seance.app`.** Finder,
  Spotlight, the Dock, ⌘-tab — the mac ways of starting a program all reach
  seance now, instead of only a terminal that has the repo's `target/release`
  on its PATH. `--user` installs to `~/Applications` for a box where
  `/Applications` isn't writable, `--no-build` bundles what's already built,
  `--dest` puts it anywhere.

  The bundle holds a **copy** of the binary, which makes this the mac build
  command rather than a step after one — a bare `cargo build --release`
  leaves the app stale. A symlink into `target/` would have kept them in
  sync, but the main executable of a signed bundle has to live inside it, and
  the ad-hoc signature is what lets the app hold onto its identity — and its
  TCC grants — across reinstalls. With a warm target dir the whole script is
  a few seconds, so re-running it is cheaper than remembering which of two
  commands you last ran.

  Panes needed nothing special to survive the move. They spawn `/bin/bash
  -lc`, so a login shell rebuilds `PATH` from the user's dotfiles and `claude`
  resolves the same from Finder as from a terminal — the usual GUI-app
  environment problem doesn't arise, and no `LSEnvironment` hardcoding was
  added to fake it.

- **`assets/icons/seance-macos-1024.png`** — the candle at macOS icon
  proportions (824pt of art centered in 1024, matching Apple's grid), so
  `sips` + `iconutil` can build the `.icns` on a mac with no SVG rasterizer
  installed. Committed rather than rendered at bundle time for exactly that
  reason.

## [0.22.0] — 2026-08-10

Links to pages we publish open in scry.

### Added

- **`localhost` and `ham.xyz` links open in scry, in the `general`
  workspace.** Ctrl/middle-click in a terminal, a PR chip, a pad link, a row
  in the PR board — every link in the app goes through one seam now, and the
  hosts we publish to route to [scry](https://github.com/zackham/scry), the
  browser for pages we control. Everything else keeps the default browser
  exactly as before.

  It talks to scry's **control socket** (`~/.local/share/scry/control.sock`,
  JSON lines) rather than shelling `scry ctl`, which needs its repo's
  `run.sh` to find `libcef.so` — that would have meant hardcoding a clone path
  from someone's home directory into this app. The socket is a stable location
  and needs no binary.

  **Every failure path lands in the default browser**: no socket, scry not
  running, a wedged scry, a reply we can't read, an external url scry refuses.
  A link that goes nowhere would be worse than a link in the wrong browser.
  Host matching respects the label boundary, so `ham.xyz.evil.com` is not our
  host, and userinfo can't name one (`https://ham.xyz@evil.com` is evil.com) —
  the same traps scry's own `policy.rs` documents, ported with its tests.

  Loopback *addresses* deliberately aren't routed even though scry blesses
  them, so nothing that only ever spoke to `127.0.0.1` moves without being
  asked. Opening now runs off the calling thread, since the routing decision
  costs a socket round trip and every caller in the GUI is a click handler.

## [0.21.0] — 2026-08-10

The mouse's back and forward buttons walk the circles you've been in.

### Added

- **Mouse back / forward navigate between circles.** The side buttons on the
  mouse now step through this window's visit history — back to the circle you
  were just in, forward to undo it. It works over a terminal, over the rail,
  anywhere in the window; panes forward no button events to the PTY, so
  nothing downstream wanted them.

  This is a *path with a position in it*, not the recency ranking the jump
  palette shows. Recency is a set sorted by a clock, so it reshuffles under
  you while agents finish; back-then-forward has to land you back exactly
  where you left, which only a cursor into an ordered path can promise. Going
  somewhere new after stepping back drops the forward half, same as a browser.

  History is kept by **watching** the selection once per render rather than by
  each caller remembering to record. The selection moves from a dozen places —
  the rail, ctrl+page, the jump palette, clicking a pane that lives in another
  circle, parking the circle you're in, a `ctl` spawn pulling the window
  across — and the daemon can move it without this window asking. Watching
  catches all of them; asking every caller catches the ones I thought of
  today. A circle that's been banished is stepped over, not pruned, so
  forward still retraces the same path. Per window, never persisted — a new
  window starts empty, like a new browser tab.

## [0.20.1] — 2026-08-06

### Changed

- **Markdown panes open folded to their outline** rather than fully expanded —
  every heading on screen, no prose. Seeing a long document's shape is usually
  why it's in a pane. `⊞` opens everything; a section you expand stays expanded
  across rewrites, while one that appears after the pane opened arrives folded,
  so an agent appending to a file can't dump prose into a view you'd arranged.
- **"Fold everything" now means the leaves, not every heading.** Collapsing
  hides a subtree, so the previous behaviour folded a document's `#` title too
  and collapsed the whole thing to a single line — obedient and useless.
  Folding the leaves is the deepest fold that still shows the shape of what was
  folded, and it leaves a parent's preamble visible.

## [0.20.0] — 2026-08-06

Markdown panes fold.

### Added

- **Foldable sections in markdown file panes.** Click a heading's caret to
  collapse its section — the whole subtree, not one level — or use `⊟` / `⊞`
  in the pane header for all of them. A flat 200-line agenda becomes its
  eight-line spine, and a collapsed heading still says `N lines`, because a
  fold that leaves no trace of its size reads like the document ends there.
  The chrome appears only for markdown that actually has headings.

  The fold happens **before** the renderer (`src/mdfold.rs`): a collapsed
  section's lines are never handed to the TextView, and each heading becomes a
  `seance-h` fence the file pane draws itself. So `gpui_component`'s markdown
  stack — ~7k lines of parsing, inline layout and cross-block selection — is
  untouched and unforked; it just receives a shorter document. Selection still
  crosses a folded heading (the custom node carries its markdown as its text).

  **Fold state is keyed by heading path, not by line.** The canonical file pane
  is watching a document an agent is writing while a human reads it, and a
  line-indexed fold set would silently reopen on every write, or fold the wrong
  section. Path keys survive insertions above, edits elsewhere, and the section
  moving; a fold is forgotten only when its heading is renamed, which is the
  one case where there's no honest way to claim it still refers to that
  section. Duplicate headings under one parent fold independently.

## [0.19.0] — 2026-08-06

Jumping remembers where you've been.

### Changed

- **`ctrl+shift+j` lists circles most-recently-active first**, instead of the
  rail's order. The rail groups by band so it can hold still while agents start
  and finish; jumping wants the opposite — the circle you were just in, then
  the one before that — so the hotkey plus arrows walks back through where
  you've actually been. Title says so.

### Fixed

- **The jump palette's three code paths disagreed about what was in the list.**
  The row count arrows wrap against, the item Enter activates, and what is
  drawn were computed separately, and two of them still included panes after
  the list became circles-only (owner decision, 0.17). So the highlighted row
  and the thing you jumped to were different entries, and often the jump landed
  on a *pane* — which is also why it didn't move the rail. One shared source
  now, pinned by tests.
- **Selecting a circle reveals its row in the rail**, rather than only scrolling
  toward it. Scrolling alone can't help when the target sits inside a folded
  band or cluster — there is no row to scroll to, and the sleeping and parked
  bands start folded — so jumping into one left the rail sitting where it was,
  showing no sign of where you'd gone. It now unfolds what hides the row first,
  then scrolls. Applies to every select: `ctrl+shift+pageup/down`, a jump, and a
  host menu creating a circle all land the same way.
- **A host menu's panel opens above its chip again**, instead of parked at the
  foot of the rail. Fixing the click bug moved the panel out of the chip's
  element tree, and the placement went with it — but "not a child of the chip"
  and "not anchored to the chip" are different things, and conflating them is
  what sent it to the corner. The chip now reports its painted bounds through a
  side-channel (the idiom the terminal view already uses for cell metrics) and
  the panel positions itself against them: left edge flush, growing upward,
  clamped so a long list stops short of the window top rather than running off
  it.

## [0.18.2] — 2026-08-06

### Fixed

- **Picking a row in a host menu did nothing.** Two bugs stacked, both from the
  panel being a child of the chip's layout when it is drawn nowhere near it.
  First, dismissal used `on_mouse_down_out` on the chip — which fires in the
  CAPTURE phase for any press outside *the chip's* hitbox, and the panel is
  absolutely positioned outside it. Every click on a row was therefore read as
  a click-away: the menu closed on mouse-down and the row was gone before
  mouse-up could complete the click. Second, the panel was
  `absolute().bottom_full()` inside a `deferred`, whose layout is detached from
  its parent, so once dismissal was fixed the panel resolved to nowhere
  visible. The panel now renders from the app root, positioned against the
  window beside the rail, inside a full-window scrim that owns click-away.

## [0.18.1] — 2026-08-05

### Fixed

- **`restart-gui` was killing the `seance web` bridge.** It found processes
  with `pgrep -x seance` — which matches every surface, since they all share
  the binary and therefore the process name — and spared only cmdlines
  containing "daemon". So each GUI redeploy silently took down any running web
  bridge, and said `stopped 3 gui process(es)` while doing it. Remote access to
  seance disappearing as a side effect of shipping a UI change is the kind of
  breakage you discover much later, from the wrong end. It now kills only a
  cmdline with no subcommand at all (flags like `--local` / `--remote <host>`
  are still the GUI); `web`, `ctl`, `replay`, `daemon` and anything else on the
  binary survive.

## [0.18.0] — 2026-08-05

A host can hand seance a workflow it doesn't understand.

### Added

- **Host menus — a chip in the launch strip that asks its question when you
  click it.** The host bridge already let an outside app own a strip in the
  rail, but only by polling: every item became a permanent chip, so the shape
  only fit small ambient state like which claude account is live. A menu is the
  other half. `menus[]` in `host.json` names a `list_cmd` and a `select_cmd`;
  the chip runs the list on click, drops the items into a dropdown grouped by
  whatever the host said, and runs select on the row you pick. Nothing is
  polled. Twenty rows in the rail would be a wall; twenty rows in a dropdown is
  a list.
- **A menu's `select_cmd` can hand back a `workspace`**, and the rail pins it
  and jumps to it. That is the whole reason a menu can create a circle and have
  it feel like a launch rather than a background event: a circle you just
  deliberately conjured is the one you're about to work in, so landing it
  somewhere you then have to go find defeats the point. Hosts that spawn
  background work send `"pin": false`.
- **First menu (vita's, not seance's): the week's meetings.** Click one and a
  circle opens for that meeting — a claude pane in the vita repo, armed for
  seance, handed the meeting's agenda prompt, and told to put the agenda up as
  a live file pane beside itself so you watch it get written. Seance does not
  know what a meeting is. It ran two commands the host configured; the workflow
  is entirely `vita/scripts/seance_host_meetings.py` talking to `seance ctl`.
  This is the seam working as intended: a host adds a workflow without seance
  learning the workflow.

### Fixed

- **Clicking a pane no longer eats your clipboard.** A bare left click anchored
  a one-cell selection, and releasing the button copied it — so clicking into a
  terminal to focus it silently replaced the clipboard with one character. That
  is a loss you only discover at paste time, when whatever you wanted is
  already gone. Release now copies only when the gesture actually was a
  selection: a drag covering more than one cell, or a double/triple click
  (explicit "select that word/line", so a one-character word still copies). A
  plain click clears the anchor instead of leaving a stray highlight.
- **A pretty-printed host response could be read as an empty one.** Snapshot
  parsing scanned for the last `{…}` line before trying stdout as a whole, and
  every field of the snapshot has a default — so one indented *item* line
  deserialized cleanly into a snapshot with no items. A host that formatted its
  JSON got "no items", which is worse than an error because it looks like an
  answer. Whole-stdout is now tried first, and the leaked-log line scan insists
  on a line carrying `items` or `schema`.

### Changed

- The rail's launch strip is titled `── launch ──` (was `── vita quicklaunch
  ──`). It holds two sources now, and neither of them is vita's by nature —
  vita is just this machine's host.

## [0.17.1] — 2026-08-05

Design pass on the rail. Same theme, same information — far less work to read.

### Changed

- **One left axis.** The band caret, the cluster caret and every row's glyph
  now share a single fixed 15px column, so band titles, cluster titles and
  circle names all begin on the same vertical line. The rail used to have
  three different left edges and your eye had to zig-zag down it.
- **The glyph slot earns its ink or stays empty.** A diamond on every idle row
  was a dozen identical marks that said nothing. Idle now draws nothing, which
  is what makes the rows that *are* doing something impossible to miss. Order:
  needs-human (violet ●) > working (spinner) > selected (◆) > asleep (☾).
  Selection no longer steals the slot from a working circle — the flame anchor
  says "here" instead.
- **Bands and clusters read as different species.** A band is a landmark you
  navigate by: uppercase, letterspaced, 11px, quiet, with air above and none
  below. A cluster header is row-sized, because conceptually it is one of the
  circles it holds.
- **A cluster reads as one object** — a hairline runs down its members, and
  the shared prefix is dimmed on each row (`mtg-`**growth**). The header above
  already says the prefix; repeating it at full strength made you re-read it
  on every line.
- **One hard right edge.** Times and header counts share a fixed 34px
  right-aligned column instead of a ragged gutter. Counts lost their
  parentheses.
- **The selected row has a flame anchor** down the left edge plus a `surface`
  fill — the old fill alone was nearly invisible on a dark screen. The anchor
  lives in the row's left inset, so text never shifts when selection moves.
- **`needs-human` colours the whole row** (violet glyph + violet name) rather
  than hiding in a badge; it is the one state that must not be missed. `done`
  became a proper tinted pill instead of text floating mid-row. Sleeping rows
  recede to 62%.
- **Rows are 28px, down from 41.** Four bands and thirteen circles now fit
  where nine circles used to.

## [0.17.0] — 2026-08-05

Four folding bands, and circles that cluster by the name you give them.

### Added

- **The rail is four folding bands: pinned, active, sleeping, parked.**
  Sleeping is now its own band rather than circles sitting wherever they were
  when they dozed off. A pinned circle stays pinned even asleep — a pin says
  where you want to *look*, and the daemon dozing something is not a reason to
  move it. Each band shows its count, and a folded band carries one dot for
  the loudest thing hiding under it, so an agent asking for help is visible
  without unfolding the pile.

- **Circles cluster by the text before the first hyphen, per band,
  independently.** Name three circles `mtg-growth`, `mtg-ai`, `mtg-carl` and
  they gather under `mtg`. Grouping is a naming convention rather than a
  stored attribute: you get it by typing, you undo it by typing, and it costs
  nothing when you don't want it — which is the point when the grouping only
  matters for an afternoon. A prefix only one circle carries is not a group,
  and a name with no hyphen opts out. The `mtg` circles you're working in and
  the `mtg` circles you've slept are separate piles that fold separately.

  A cluster sits at the position of its **first** member, so it floats exactly
  as high as its most-deserving circle would have on its own — the sort keeps
  meaning what it meant. Matching ignores case; the header shows the prefix as
  you first typed it. Grouping reads the **label**, so retyping a name is how
  you regroup, and the slug underneath never moves.

### Changed

- **`ctrl+page` walks exactly what the rail is showing** — which now means
  folds narrow it. Fold the bands and clusters you're not in and the rotation
  is just your working set. Both GUIs.
- Fold state persists per client (`subscriptions.json` / localStorage).
  Untouched, `sleeping` and `parked` start folded; the first fold makes your
  choices authoritative, so unfolding everything stays unfolded instead of
  springing back to defaults. A cluster that stops existing takes its fold
  with it.

### Removed

- The bespoke pinned-section + parked-accordion rendering in both GUIs, and
  `partition3` — one generic band renderer replaced three special cases, and
  the shared `seance_core::grouping` means the two rails cannot drift.

## [0.16.0] — 2026-08-05

A circle's name stops being its identity.

### Changed

- **A circle has a stable slug and a mutable label.** Panes have always had
  this split — `slug` is the id, `name` is what you read — and it is why
  renaming a pane costs nothing. A circle's display name *was* its key, so a
  rename rewrote its identity and every holder of the old string had to be
  migrated by hand: eight structures in the daemon, six more plus the pin/park
  prefs in the native GUI, the same again in the web client. And one that no
  migration can ever reach — the `SEANCE_WORKSPACE` already baked into a
  running pane's environment. You cannot write into the environment of a
  process that is already running, so under the old model **an agent in a
  renamed circle could no longer tell `seance ctl` where it was**: its scope
  named a circle that no longer existed, `list` came back empty and `send`
  answered "outside your workspace".

  The slug is now minted once, from the name the circle was created with, and
  never rewritten. **Rename sets a label and nothing else** — panes, activity
  clocks, PR links and dismissals, selections, subscriptions, per-GUI pin/park
  prefs and every running pane's environment all keep pointing at the same
  circle, because none of them moved.

- **Addressing accepts either form.** An exact slug wins, then an unambiguous
  label; a label two circles share resolves to neither and says so rather than
  picking one — the same precedence pane lookup got in 0.14.2, for the same
  reason. Resolution happens once at the daemon door, generically over the
  serialized request on **both** the control and GUI planes, so a future op
  carrying a `workspace` inherits it without anyone remembering. A key that
  matches nothing is left verbatim: it goes on matching no circle rather than
  being silently promoted to unscoped.

- **`seance ctl whoami` answers from the pane**, not from the caller's
  environment — the canonical "which circle am I in".

### Added

- `SEANCE_WORKSPACE_NAME` carries the circle's label for display.
  `SEANCE_WORKSPACE` is the slug and is now genuinely stable. `ctl list` shows
  `label (slug)` when they differ; `whoami` reports both.
- **`seance ctl rename-circle [WS] NAME`** — agents can already create circles
  (`ctl new --workspace`), so they can now label them too; rename was
  GUI-only. Addresses by either form and sets the label, leaving the slug
  alone like every other path.
- Labels render in both GUIs — sidebar, overview, PR board, command palette —
  and a new circle keeps the name you typed ("Growth Work" reads as itself
  while being keyed by `growth-work`). The palette matches **either** form, so
  you can find a circle by what you see or by what it is, and rename prefills
  the label so you edit what you're looking at.

### Removed

- `rename_pr_links`, `SubscriptionsPref::rename` and `SubPrefs::rename` — the
  three hand-written rename follow-throughs. Carrying a pin across a rename is
  no longer something that can be got wrong, because the key those pins are
  filed under does not move. Their tests were retargeted to assert the new
  invariant rather than deleted.

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
