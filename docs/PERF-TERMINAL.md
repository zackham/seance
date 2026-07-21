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
- Encode: RLE blanks / repeats (`src/runtime/snapshot.rs`).

### Daemon scheduling

| Pane | Live grid push |
|------|----------------|
| Any pane in **selected** workspace | ~60fps cap (16ms) |
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

- `src/runtime/snapshot.rs` — SCG3 encode/decode, dirty_rows  
- `src/runtime/engine.rs` — workspace throttle, damage broadcast  
- `src/remote_term.rs` / `remote_term_view.rs` — paint, echo, cache  
- `src/fileview.rs` — virtualized markdown / plain / diff  
- `src/app.rs` — grid_bin apply, visibility
