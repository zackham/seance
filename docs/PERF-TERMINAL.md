# Terminal performance

## Goal

Make multi-pane seance (including 3+ tile splits and spinning agent TUIs)
feel like separate Wayland terminals for normal use — then stop at diminishing
returns until a GPU glyph-atlas compositor is justified.

## What was wrong

Naive path: **full grid → fat JSON → CPU shape every cell → repaint N panes**.

Cost ≈ `visible_terminals × cells × paint_rate`. Cliffs at **3 tiles**
(auto-grid becomes a 2-col split) plus any spinner neighbor.

## What we ship now

### Wire (SCG3)

- Live event: `grid_bin` = base64 of binary blob (not JSON cells).
- **FULL** frame or **DAMAGE** (only dirty rows).
- Daemon caches last cells per pane; skips identical frames; cursor-only →
  damage of cursor row.
- Encode: RLE blanks / repeats (`crates/seance-core/src/snapshot.rs` — the
  codec moved into the shared sans-io crate in 0.11 so wasm decodes it too).

### Daemon scheduling

| Pane | Live grid push |
|------|----------------|
| Any pane in **selected** workspace | ~60fps cap (16ms) |
| Owner window in overview | ~15fps (66ms) |
| Other workspaces | **no push** until workspace selected (then flush) |

Pre-batch paint used to throttle unfocused neighbors (~4fps / ~2fps in 3+
splits). That crisis throttle is gone — if it's on screen, it runs live.

### GUI

- Drop grids for panes outside selected workspace.
- All visible panes: live paint (no focus/split FPS cap).
- Batched `shape_line` + `force_width` cell snap; skip blank rows.
- **Shaped paint cache** — re-paint without reshape when grid/bounds unchanged
  (sidebar DnD forces full `window.refresh` every move; cache keeps it cheap).
- Resize hysteresis (no col 120↔121 thrash).
- `Arc` snapshots; cached cell metrics (shape `█` once).
- **Local echo** for printable keys on focus (daemon frame wins by rev).
- **Typing hot** (`src/term_shared.rs`) — a human keystroke marks the app hot
  for 250ms; applied grids paint immediately while hot, ~30fps otherwise
  (echo latency when it matters, stream smoothness when it doesn't).

### Still not ghostty-16×

Remaining for absolute ceiling:

1. Shared-memory cell buffers (zero-copy)  
2. GPU glyph atlas + instanced quads  

At current measured load, those are **diminishing returns** for typical agent
workflows; do them when we need 16 full-screen spinning TUIs as the default.

## File panes

Markdown uses `scrollable(true)` virtualization (only on-screen blocks paint).
Plain text and history diffs use `gpui::list` line virtualization. Fit-content
markdown was a scroll/resize cliff on large docs.

## Recorder tap (0.11)

The replay recorder is a second consumer of the same frames. Cost is bounded
by design: the engine clones a pane snapshot for it at most once per 33ms per
pane (`Engine::record_grid_tap`, forced only on human input / title / exit),
the recorder thread coalesces again at 33ms, and the whole path is
**output-driven** — an idle pane produces no messages, so idle panes are free.
The handle is an unbounded channel and fire-and-forget: the recorder can never
block or kill the engine.

## Web path

The browser client has its own renderer (WebGL2 glyph atlas) and its own
numbers — paint p95 0.4ms, echo p50 9.1ms via a 2ms bridge poll plus the same
typing-hot immediate paint. Measured figures and the probe overlay live in
[docs/WEB.md](WEB.md); the wire (SCG3) is shared with the native path.

## Measured (idle multi-pane soak)

| Scenario | GUI CPU | Daemon CPU |
|----------|---------|------------|
| Idle | ~1–2% | ~1% |
| 6 panes + 2 spinners, focused workspace | ~1.5–2.5% | ~1% |
| Pre-batch-paint (historical) | ~90% | high |

```bash
cargo build --release && seance upgrade
```

## Code map

- `crates/seance-core/src/snapshot.rs` — SCG3 encode/decode, dirty_rows  
- `src/runtime/engine/gui.rs` — workspace throttle, damage broadcast, recorder tap  
- `src/runtime/recorder.rs` — replay ring writer (coalesce, keyframes)  
- `src/remote_term.rs` / `remote_term_view.rs` — paint, echo, cache  
- `src/term_shared.rs` — typing-hot window  
- `src/fileview.rs` — virtualized markdown / plain / diff  
- `src/app/mod.rs` — grid_bin apply, visibility, paint pacing  
- `crates/seance-web/src/renderer.rs` — WebGL2 glyph atlas (web path)
