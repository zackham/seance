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
  `GuiRequest`/`GuiEvent` JSON lines; grids arrive as SCG3 binary (base64) and
  are decoded by the same `seance-core` codec the daemon encodes with. One ws
  text message = one line; the bridge is a dumb pump.
- **Auth**: daemon-minted bearer token (`<state-dir>/web-token`, 0600, 64-hex).
  The bridge accepts the ws upgrade, then closes `4401` on a bad token
  (constant-time compare, `seance_core::auth`). Transport policy is
  tailscale-only for now; the token is in the protocol so that policy can
  change without wire surgery.
- **Renderer**: glyph-atlas WebGL2, two draw calls per frame (bg pass + glyph
  pass), damage-driven repaints, DPR-exact metrics. Steady-state allocates
  nothing per frame.
- **Windows**: a web attach is a normal second GUI window. Workspaces owned by
  the desktop window appear under **elsewhere** in the sidebar with a `pull`
  button (`TransferWorkspace`). Pull what you need; the desktop can pull it
  back the same way.
- **Probe**: the `probe` topbar button (or ctrl+shift+P) overlays echo
  p50/p95, paint time, ws rtt, rx rate — same philosophy as the native
  latency probe: performance is measured, not claimed.

## Build

```bash
./scripts/build-web.sh release   # → crates/seance-web/dist (~800K, committed)
```

Needs the rustup toolchain (`wasm32-unknown-unknown`) and `wasm-bindgen-cli`
0.2.126 (matching the pinned crate). `dist/` is committed so `seance web`
works on machines without the wasm toolchain.

**Version lockstep**: every workspace crate inherits `workspace.package.version`,
and the daemon enforces an exact build match on hello — rebuild the web bundle
whenever the version bumps, or web clients are refused with a version-skew
error (by design).

## Measured (2026-07-30, first light)

- paint p50 0.3ms / p95 0.4ms; ws rtt 0.6ms (localhost)
- echo p50 ~51ms — dominated by the daemon's ~30fps grid-push coalescing, not
  the web path. Future: daemon-side immediate push for the originating client
  (the native GUI's "typing hot" equivalent, moved daemon-side).

## Not yet (honest gaps)

- Scratchpad/file panes, host widgets, overview mode (all Fs-bridge chrome).
- Double-click word select; IME composition untested.
- Touch/mobile keyboard (iOS Safari needs a hidden-input shim) — the intended
  base for the future native iOS client is `seance-core`, same as this client.
- Bridge serves plain HTTP; TLS is delegated to tailscale (`tailscale serve`
  works in front of it).
