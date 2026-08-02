# design: workspaces as subscriptions (killing exclusive ownership)

*silicon-zack, 2026-08-01. **APPROVED by zack 2026-08-01 — including every
recommendation in "decisions that are yours"** (per-connection focus; orphans
to the spawning GUI, ctl spawns parked+needs; ctrl+page cycles active only;
delete CollectAll/Transfer/ownership outright). Build handoff:
[SUBSCRIPTIONS-HANDOFF.md](SUBSCRIPTIONS-HANDOFF.md).*

## the problem

`workspace_window` gives each workspace exactly one owning GUI window. that
made sense when seance was one native app with occasional second windows; it
is now the thing standing between us and: a sane elsewhere list, per-GUI
active/parked grouping, watching one workspace from two places, and true
multiplayer. every recent papercut (pull-vs-transfer, foreign rows, empty
second windows) is this model leaking.

## proposed model

- **workspaces are global.** no owner. the daemon holds the one canonical
  list + per-workspace state, exactly as today minus `workspace_window`.
- **each GUI connection holds a subscription set** — the workspaces it wants
  grid streams for — plus its own `selected_workspace`/`focused_pane`. grids
  fan out to every subscriber at that subscriber's rate (selected=16ms,
  visible-but-not-selected=66ms like overview, parked=none).
- **active vs parked is a per-GUI presentation split**, not daemon state:
  the GUI's *active list* (explicitly pinned + auto-added on spawn/select)
  renders as today's sidebar; everything else is one collapsed **parked**
  group underneath, same sort, one click to expand/select (selecting
  auto-subscribes). "pull" becomes "add to active" — no custody transfer,
  nothing happens to any other GUI.
- the active list persists per GUI identity (native: state dir; web:
  localStorage) so a relaunch keeps your arrangement.

## decisions that are yours

1. **what does focus mean with two GUIs on one pane?** my proposal: nothing
   changes — focus/`active_slug` stays per-connection presentation; input
   continues to be attributed per source. two humans typing into one PTY is
   already possible today via ctl; multiplayer just makes it visible. the
   agency/ownership layer (seize/release) already arbitrates *who may drive* —
   it becomes the multiplayer etiquette layer for free.
2. **who gets the "orphan" workspaces?** today orphans attach to the sole
   GUI. proposal: new workspaces auto-enter the *spawning* GUI's active list
   (ctl spawns with no GUI attribution go to parked everywhere and badge
   `needs` so they're noticed).
3. **does ctrl+page cycle active only, or active+parked?** i'd say active
   only — parked is deliberately out of the rotation; that's its point.
4. **kill CollectAll / TransferWorkspace / multi-window ownership chrome?**
   subscription makes them meaningless (Transfer→"add to active there" has no
   remote half). i'd delete rather than shim — the daemon drops
   `workspace_window`, State drops `foreign_workspaces` (everything is just
   `workspace_meta` + your local split). breaking change to the GUI wire,
   fine at our version discipline; the native multi-window use case becomes
   "two windows, two subscription sets", strictly better.

## costs / risks

- grid fan-out: N subscribers × selected panes. the throttle table already
  handles rates; the new cost is only when 2+ GUIs *select the same
  workspace simultaneously* — bounded, and the 2ms-bridge web path proved
  cheap. recorder taps unaffected (pre-fanout).
- engine surgery: `workspace_window` is threaded through
  state_for_window/transfer/collect/kill paths (~10 sites) — a deliberate
  day, plus both sidebars' active/parked rendering.
- migration: none for data; clients seed active list = previously-owned
  workspaces on first run.

## PR links (rides on top, later) — **SHIPPED 0.13.0, 2026-08-01**

*as built: scrape off raw PTY output (ANSI-stripped, chunk-split safe, cap 8,
persisted across restart + upgrade), watcher seam = state-dir `pr_watch.json`
mtime-polled at 2s with vita's `gh` poller supplying verdicts, attention
mapping as designed below, header chip + popover in **both** GUIs, plus
`seance ctl pr-link add|clear` for manual seeding. See CHANGELOG 0.13.0,
docs/DAEMON.md, docs/CONTROL.md, docs/WEB.md.*

- daemon scrapes PR URLs from pane output automatically (`gh pr create`
  prints one; we see every byte) → per-workspace link list (handles
  backend+frontend / ios+android multi-PR naturally).
- a watcher (config-seam like quicklaunch; vita supplies the gh poller) maps
  link states onto the existing attention machinery: changes-requested /
  CI-fail / new-comment → `needs`, approved+green → `done`. a parked
  workspace resurfaces exactly like an agent asking for help — because the
  PR *is* an agent you're waiting on. header chip jumps to the PR.
- this is the time-to-merge play: badge → click → you're in the session.

## sequencing

1. this doc argued over ✦ (30 min of your time)
2. engine: subscriptions replace ownership (one focused session)
3. both sidebars: active/parked split + persistence ✅ 0.12.0
4. PR links + watcher ✅ 0.13.0

*(the ✦ popover + elsewhere-label fixes shipping today are forward-compatible
with all of this — the census UI becomes the multiplayer roster.)*
