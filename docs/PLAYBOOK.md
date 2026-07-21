# Building seance (GPUI pin guide)

How to get a reproducible GPUI build. Reference revs:

| dep | rev |
|-----|-----|
| zed / gpui / gpui_platform | `1a246efd7e1b83ab568ec5e3e6c1a43a42e1abba` |
| gpui-component (longbridge) | `b5eef62336f88bb6c1ee45bf32f73c9895d49f8d` |
| alacritty_terminal (zed fork) | `4c129667ce56611becdc82de6e28218c80e2e88f` |

## Quickstart

```bash
./scripts/bootstrap-deps.sh   # → deps/zed at the pinned rev
cargo build --release         # first build ~10 min
./target/release/seance
```

Or point `deps/zed` at an existing checkout of that rev:

```bash
ln -sfn /path/to/zed-at-1a246efd deps/zed
```

## Why the local patch exists

`gpui-component` depends on `gpui` from zed **without a rev** (tracks HEAD).
Cargo ignores the transitive crate’s lockfile, so zed HEAD drift breaks builds.

Cargo also **rejects** patching a git source with the same git URL at a fixed
rev. The working pattern is a **path patch**:

```toml
[patch."https://github.com/zed-industries/zed"]
gpui = { path = "deps/zed/crates/gpui" }
gpui_platform = { path = "deps/zed/crates/gpui_platform" }
gpui_macros = { path = "deps/zed/crates/gpui_macros" }
```

`deps/zed` is gitignored; `bootstrap-deps.sh` clones it.

## Bumping the pin pair

1. Choose a new `gpui-component` rev; update `Cargo.toml`.
2. In that rev’s `Cargo.lock`, find the zed commit it resolved.
3. Move `deps/zed` to that commit; update comments / this doc.
4. Delete `Cargo.lock` once if the patch source changed, then `cargo build`.

## Linux notes

- Features on `gpui_platform`: `font-kit`, `x11`, `wayland`, `runtime_shaders`
- Dev profile: `opt-level = 3` for `gpui` / `gpui_platform` / `taffy` (unoptimized GPUI is unusable)
- Default term font: `CaskaydiaMono Nerd Font Mono` (`src/term_font.rs`)
- Set `WindowOptions.app_id` (seance uses `"seance"`) so the desktop entry matches

## API discipline

GPUI moves fast. Before calling an API, **grep `deps/zed`** (and a
gpui-component checkout at the same rev). Do not trust model-memory signatures.
