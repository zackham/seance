# seance — notes for coding agents (working on this repo)

Seance is a **human + agent co-working** app: multi-pane live terminals on
Linux (GPUI), shared scratchpads, file panes, and a Unix-socket control plane
so anyone in the circle can engage everyone else in the open. Product intent
is in `README.md` and `seance ctl skill` — not “Claude wrapper.”

## Pinned revs — do not casually bump

- **GPUI** patched to `deps/zed` @ `1a246efd7e1b83ab568ec5e3e6c1a43a42e1abba`
  (`./scripts/bootstrap-deps.sh`)
- **`gpui-component`** @ `b5eef62336f88bb6c1ee45bf32f73c9895d49f8d`

Bump only as a pair — `docs/PLAYBOOK.md`. Grep `deps/zed` for real APIs.

## Build / test / run

```bash
./scripts/bootstrap-deps.sh
cargo build --release && cargo test
./target/release/seance
seance ctl skill    # agent-facing engagement protocol
seance ctl list --all
```

### Session survival — never hard-kill the daemon

| action | sessions |
|--------|----------|
| restart GUI only | live |
| `seance upgrade` | live (handoff) |
| `pkill -x seance` | **die** |

```bash
cargo build --release && seance upgrade   # runtime / protocol
seance restart-gui                          # UI only
```

## Product rules (don’t regress these)

- **Visibility is the point** — agents work on the human’s screen, not offstage
- Default new pane is a **shell** (human can always take over); agents via `--command`
- Prefer `propose` / `ask` / status badges / scratchpads over silent side effects
- Workspace scoping is default inside a pane; `--all` is explicit cross-circle
- Durable text → scratchpad or file pane; screens are ephemeral

## Verifying

- `seance ctl read <pane>` — true rendered grid
- `seance ctl human` — focus / workspace / pending asks
- `SEANCE_DEBUG_IO=1` — PTY I/O on stderr

## Conventions

- Domain modules carry rustdoc headers
- Atomic writes for state/scratch
- Control plane: JSON lines; `from` / `scope` stamped by ctl
- Theme: `SeancePalette` + `docs/THEME.md`
