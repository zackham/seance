# Contributing

Seance is a **shared live workspace for humans and agents** — not a wrapper
around one vendor CLI. Features should keep work visible, interruptible, and
attributable. Read `README.md` and `seance ctl skill` before changing the
engagement surface (`ask`, `propose`, status, scratchpad, scoping).

## Build

```bash
./scripts/bootstrap-deps.sh   # clones zed @ pinned rev into deps/zed
cargo test
cargo build --release
./target/release/seance
```

First compile of GPUI is slow (~10 min). Incremental builds are fine.

## Layout

- `src/daemon/` + `src/runtime/` — long-lived session runtime (PTYs, grids)
- `src/app.rs` — shared-space UI (sidebar, tiling, ask/whisper/arm)
- `src/remote_term*.rs` — GUI terminal paint path
- `src/fileview.rs` — co-editing file panes
- `src/ctl.rs` — `seance ctl` + `skill` text (canonical agent protocol)
- `docs/` — protocol and design notes

## Session safety

The **daemon** owns every PTY. Prefer:

```bash
cargo build --release && seance upgrade   # runtime changes
seance restart-gui                          # GUI-only
```

Never `pkill -x seance` to reload — that destroys live sessions.

## Style

- Match surrounding code; no drive-by refactors
- Keep docs short; update `docs/` + `SKILL_TEXT` when engagement behavior changes
- `cargo test` before you push

## License

By contributing you agree your work is under the MIT license (see LICENSE).
