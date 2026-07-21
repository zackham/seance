# File panes

The second seance pane kind (terminals were the first): a live, **read-only**
viewer bound to one file on disk. Use case — human and agent are co-editing a
document in the circle; the agent (or human) opens a file pane so **edits appear
live on the shared screen**, with history to step back through. Same visibility
principle as terminals: work happens in the open.

Implemented in `src/fileview.rs` as `FileView` (a gpui `Render` view). The pane
system constructs it with `FileView::new(path, window, cx)` and reads back the
bound path via `FileView::path()`.

## Behavior

**Header strip.** Filename (basename) with the full path as a dimmed suffix; a
live/history indicator (`● live` in sage / `◐ history` in violet); and history
controls: `◀` (older), an `N/M` counter, `▶` (newer), and `⦿ live` (jump back to
the live tail). Controls are plain styled `div`s in the house style — not
`gpui-component` `Button` — so the pane pulls in no icon/asset dependencies.

**Body.** The file content, scrollable (`overflow_y_scroll` on an `id`'d div).

- `.md` / `.markdown` / `.mdown` / `.mkd` → YAML frontmatter (if present) is
  peeled into a GitHub-style key/value box (custom `seance-fm` markdown block),
  then the body is rendered via `gpui_component::text::markdown().scrollable(true)`
  (**virtualized** — only on-screen blocks shape/paint). Frontmatter is block 0
  of the same list so it scrolls away with the document (not sticky chrome).
  Theme from `cx.theme()`. Fit-content mode is intentionally not used for file
  panes — large docs re-laid-out the entire tree on every scroll/resize.
- Anything else → **preserved-newline monospace** via a virtualized `list`
  (one row per line; blank lines become spacers).
- Empty or unreadable file → a friendly placeholder ("this file is empty" /
  "can't read this file — it may have been moved or deleted"). The watcher keeps
  polling, so a file that later appears or becomes readable lights up on its own.

**Live mode.** A 1-second mtime poll (a self-rescheduling gpui background task,
the same pattern `src/scratchpad.rs` uses — no `notify`, no OS threads). On a
detected change the live content reloads **and** a history snapshot is appended.
A bare `touch` (mtime bumps but content is identical) does **not** record a
snapshot.

**History mode.** `◀`/`▶` step through snapshots read back off disk. Stepping
`▶` past the newest snapshot returns to live. While pinned to history:

- external changes are still **recorded** (history keeps growing) but do **not**
  yank the view off the snapshot you're reading;
- a subtle hint shows `viewing history N/M`, an optional `+A/-B lines vs.
  previous` line-count delta, and `file has changed since` when the live file
  moved on underneath the pin;
- **diff is the default** when stepping into history (when a predecessor
  exists): unified line-diff vs. the previous snapshot, monospace, sage `+` /
  danger `-`, **hunk-collapsed** like VS Code / git (≈3 context lines; longer
  unchanged runs as `⋯ N unchanged lines`). **click the hint bar** to toggle
  back to normal content (or to re-enable diff). Diff turns off automatically
  at the oldest snapshot and when returning to live;
- `⦿ live` returns to the tail and reloads the current file.

**Changed-lines affordance.** The hint bar still shows a cheap order-insensitive
line-multiset delta (`+added/-removed`) between the viewed snapshot and the one
immediately before it. The click-to-toggle body is a real ordered unified line
diff (LCS under 2500 lines per side; greedy lookahead beyond that).

## History storage

Plain, uncompressed file copies — no diffs — under:

```
~/.local/share/seance/filehist/<hash>/NNNN.snap
```

- `<hash>` is a 64-bit FNV-1a hash of the file's **canonical** absolute path
  (falling back to the raw path when the file doesn't exist yet), rendered as 16
  hex digits. Same file → same directory across sessions, so **history persists
  across re-open**; different files never collide.
- `NNNN` is a zero-padded, monotonically increasing snapshot index. `0000` is
  taken at open (skipped if the current content already equals the newest
  existing snapshot, so re-opening an unchanged file doesn't grow history). Each
  subsequent recorded change appends the next index.
- Snapshots are read back on demand when stepping through history; nothing is
  held in memory except the index list and the currently displayed content.

## Limits

- **Cap: `MAX_SNAPSHOTS = 200`** per file. Recording past the cap prunes the
  oldest `.snap` files (indices keep climbing; the on-disk filenames are the
  source of truth, re-derived by listing the dir).
- **Read-only.** The pane never writes the watched file — only its own history
  copies. There is no merge/conflict logic because there are no local edits.
- **Poll latency ≤ 1s.** Changes are visible within one poll interval; this is
  not an inotify-tight loop (a deliberate match to the scratchpad pattern).
- **Whole-file snapshots.** A large file edited many times stores many full
  copies (bounded by the cap). Fine for reports/markdown; not intended for
  multi-megabyte binaries.
- **Not virtualized.** The body renders all lines/markdown for the current
  content into one scroll container (so frontmatter can scroll with the body).
  Comfortable for documents; a pathologically huge file would be heavy.
  (`TextView` has a virtualized `.scrollable(true)` mode, but that owns its own
  scroller and would pin a sibling frontmatter box above the list.)

## Multi-pane / split performance

Terminal paint is still CPU-side (batched `shape_line`, not a GPU cell atlas).
Cost scales with **how many terminal grids are repainting**, not just one busy
PTY:

- **1–2 tiled panes** in a workspace: usually fine.
- **3+ tiled panes** (auto-grid becomes a 2-column split): N paints share the
  UI thread. A spinning TUI in a neighbor pane was enough to make typing in a
  quiet bash feel laggy.

Mitigations (see also `docs/PERF-TERMINAL.md`):

- live grids use **SCG2 binary RLE** (`grid_bin` event), not JSON cell soup
- only the **focused** terminal gets live repaint
- unfocused *visible* neighbors are throttled (~5fps; ~2fps when 3+ tiles)
- panes in **other workspaces** drop grid events until you switch back
- daemon grid pushes are rate-limited (~30fps coalesce)

If a workspace still feels heavy, shelve or pop out the spinner TUI, or keep
agent sessions in their own workspace so the grid you’re typing into is 1–2
panes. Long-term: shm cell buffers + GPU glyph atlas.
