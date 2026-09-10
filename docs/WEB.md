# seance web — the browser thin client

Seance from anywhere: the daemon owns the sessions; the browser is a
projection. A wasm client (Rust, shared `seance-core` wire code) renders
terminals on WebGL2 and speaks the same GUI protocol the native app speaks,
over a websocket bridged onto the daemon's unix socket.

## Run

```bash
seance web                 # bridge on 127.0.0.1:9666, serves the client + /ws
seance web --print-token   # token + ready-to-open URL
seance web --bind 100.x.y.z:9666   # expose on the tailnet (tailscale-only policy)
```

Open the printed URL. The token rides `?token=` once, is stored in
localStorage, and is stripped from the address bar. `--regen-token` rotates it.

## Architecture

```
browser (wasm)                      native
┌───────────────────────────┐       ┌─────────────┐      ┌────────┐
│ lib.rs app core (rAF loop)│  ws   │ seance web  │ unix │ daemon │
│ conn.rs ── JSON lines ────┼──────▶│ bridge      │─────▶│ engine │
│ state.rs (ClientState)    │       │ token auth  │      └────────┘
│ renderer.rs (WebGL2 atlas)│       │ static files│
│ ui.rs chrome / probe.rs   │       └─────────────┘
└───────────────────────────┘
```

- **Wire**: identical to the native GUI — `Hello` (strict version match), then
  `GuiRequest`/`GuiEvent` JSON lines; grids arrive as SCG3 binary inside an
  `SCZ3` deflate container (base64) and are decoded by the same `seance-core`
  codec the daemon encodes with. One ws text message = one line; the bridge is
  a dumb pump.
- **Flow control** (0.24): every grid frame carries a `seq`; the client echoes
  the highest one back as `GridAck` and the daemon keeps at most 8 in flight,
  merging the rest. This is what bounds the bridge's ws pump — it hands frames
  to an unbounded channel, so before the window existed the daemon never felt
  a slow browser's backpressure at all.
- **Auth**: daemon-minted bearer token (`<state-dir>/web-token`, 0600, 64-hex).
  The bridge accepts the ws upgrade, then closes `4401` on a bad token
  (constant-time compare, `seance_core::auth`). Transport policy is
  tailscale-only for now; the token is in the protocol so that policy can
  change without wire surgery.
- **Renderer**: glyph-atlas WebGL2, two draw calls per frame (bg pass + glyph
  pass), damage-driven repaints, DPR-exact metrics. Steady-state allocates
  nothing per frame.
- **Windows**: a web attach is a normal second GUI window. Nothing is owned,
  and the browser subscribes to every circle, so the same circle can be live in
  the browser and on the desktop at once. The rail arrangement — pins, folds,
  which circles this browser has looked at — persists in
  `localStorage["seance_active"]` (`{seen, pinned, collapsed}`), with the
  daemon's copy winning on connect.
- **PR chips**: `#topbar` carries `#pr-chips` (`div.pr-chips`, horizontal
  scroll) — one `button.pr-chip[.needs|.done]` per scraped PR of the selected
  circle, most recent first (that chip keeps `id="pr-chip"`). Click opens the
  PR; hover shows a `div.pr-tip` details popover (URL, state, ci, review,
  age, last review/comment); right-click opens a menu: *open PR* · *remove
  this PR ref* (sticky dismissal) · *clear all PR refs* (`PrLinkClear` over
  the ctl seam). No links = no chips in the DOM. Chip text leads with the
  repo (`repo#12`), `org/`-prefixed only when the links in view span more
  than one org.
- **PR board**: the sidebar carries `#pr-board-btn` (`button.foot-prs`,
  `PRs (N)`) above the footer, present only when N > 0. It toggles `#pr-board`
  — a dimmed full-viewport overlay appended to `<body>`, built once and
  cached, holding `#pr-board-card` (`#pr-board-head` with `#pr-board-counts`
  and `#pr-board-close`, then `#pr-board-list`). Backdrop click, the ✕, or
  Escape (`App::escape_topmost`) dismiss it. Grouping/ordering/staleness live
  in `pr_board.rs`'s pure half, shared in spirit with the native
  `src/app/prboard.rs`.
- **Probe**: the `probe` topbar button (or ctrl+shift+P) overlays echo
  p50/p95, paint time, ws rtt, rx rate — same philosophy as the native
  latency probe: performance is measured, not claimed.

## Build

```bash
./scripts/build-web.sh release   # → crates/seance-web/dist (~1.4M, committed)
```

Needs the rustup toolchain (`wasm32-unknown-unknown`) and `wasm-bindgen-cli`
0.2.126 (matching the pinned crate). `dist/` is committed so `seance web`
works on machines without the wasm toolchain.

**Version lockstep**: every workspace crate inherits `workspace.package.version`,
and the daemon enforces an exact build match on hello — rebuild the web bundle
whenever the version bumps, or web clients are refused with a version-skew
error (by design).

## Measured (2026-07-31)

- paint p50 0.3ms / p95 0.4ms; ws rtt 0.6ms (localhost)
- echo p50 9.1ms / p95 16.3ms (was 51.5 / 53.2 at first light) — two fixes:
  the bridge polls the ws for daemon→client traffic every 2ms (`WS_POLL`,
  `src/webbridge.rs`) instead of coalescing, and the client paints a grid
  arriving within 250ms of a keystroke *immediately* rather than waiting for
  the next rAF (typing hot — the native `term_shared::typing_hot` equivalent,
  `crates/seance-web/src/lib.rs`).

## Chrome parity (2026-07-31)

The native chrome is replicated sincerely: auto-sorted workspace lister
(working band + touch recency, attention badges, inline rename, banish ×,
pinned rows above a rule with pin / unpin row menus),
◈+ create-workspace, quicklaunch strip (daemon-side json via the fs bridge,
chips, editor modal, right-click), the claude-accounts host strip, footer
(+ summon / ≋ activity / ? grimoire), per-row and per-tile context menus,
zoom, and the native chord table — every ctrl+shift chord also binds its
`alt+` twin because browsers reserve ctrl+shift+n/w (the grimoire documents
both spellings; web ctrl+shift+p/alt+p = probe). Deliberate divergences:
CSS pulse instead of the braille spinner, context-menu moves instead of
drag-and-drop, no "send to new window".

## Phone chrome

Everything under `@media (max-width: 820px)` is plain JS in `www/index.html`,
appended to `<body>` — `ui.rs` clears `#topbar`/`#sidebar` on every re-render
and would eat anything parented there. It reaches the websocket only through
the four `seance_mobile_*` exports (`lib.rs`): drawer, circle menu, swipe
between circles, touch scrollback, voice→prompt, and the keyboard surface —
a 1px focused `#m-catch` input that raises the system keyboard, plus a
two-row accessory bar of modifiers / control codes / arrows with `paste` and
`done` in a right-hand column. Paste goes through `navigator.clipboard`
(hence https, i.e. the tunnel, not the raw tailnet http URL) and falls back to
a long-press textarea sheet when the browser refuses.

## Not yet (honest gaps)

- Scratchpad/file panes, overview mode, prompt/jump palettes, notes flip,
  popout (native-only; listed in the grimoire).
- Double-click word select; IME composition untested.
- The intended base for a future native iOS client is `seance-core`, same as
  this client.
- Bridge serves plain HTTP; TLS is delegated to tailscale (`tailscale serve`
  works in front of it).
