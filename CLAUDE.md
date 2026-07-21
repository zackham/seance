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
seance ctl roster   # stage projection (owner/status/pad_rev)
```

### Session survival — never hard-kill the daemon

| action | sessions |
|--------|----------|
| restart GUI only | live |
| `seance upgrade` | live (handoff preserves statuses/agency/asks/pad_rev) |
| `pkill -x seance` | **die** |

```bash
cargo build --release && seance upgrade   # runtime / protocol
seance restart-gui                          # UI only
```

### Multi-agent collab test (find this)

Live exercise: spawn claude + grok + codex, inject a docs/source review task,
wait for `finish`, collect pads.

| what | where |
|------|--------|
| **how to run** | `./scripts/agent-collab-test.sh` |
| **docs** | `docs/AGENT_COLLAB_TEST.md` |
| **outputs** | `data/agent-collab-runs/<workspace>/` |

After changing orchestration ergonomics, re-run this test and read the worker
pads before claiming A+. Prefer their evidence over vibes.

## Product rules (don’t regress these)

- **Visibility is the point** — agents work on the human’s screen, not offstage
- Default new pane is a **shell** (human can always take over); agents via `--agent` / `--command`
- Prefer `propose` / `ask` / status badges / scratchpads over silent side effects
- Workspace scoping is default inside a pane; `--all` is explicit cross-circle
- Durable text → scratchpad or file pane; screens are ephemeral
- **Completion is evidence-bound** — `finish` with body; `pad_rev` / since-inject wait
- **Self-only** note/finish/status-set when `$SEANCE_SESSION` is set (orchestrators outside a pane may cross)

## Verifying

- `seance ctl read <pane>` — true rendered grid
- `seance ctl human` / `roster` / `brief` — focus, stage, dense state
- `seance ctl pad PANE --cat` — one-hop pad body
- `SEANCE_DEBUG_IO=1` — PTY I/O on stderr

## Conventions

- Domain modules carry rustdoc headers
- Atomic writes for state/scratch
- Control plane: JSON lines; `from` / `scope` stamped by ctl
- Theme: `SeancePalette` + `docs/THEME.md`
