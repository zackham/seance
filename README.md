# seance

**A shared space where humans and agents work together, live.**

Seance is a candlelit multi-pane terminal for Linux. Every pane is on the
human’s screen. Agents (Claude, Codex, Grok, any CLI) and shells sit beside
you — not hidden in a background job. They can see each other, ask you
questions, propose commands for your approval, and leave notes on a scratchpad
you both flip into. Visibility is the point.

Native app on [GPUI](https://www.gpui.rs/). Sessions live in a long-lived
daemon; the window is disposable.

![seance](docs/screenshot.png)

**License:** MIT · **Platform:** Linux (Wayland / X11) · **Status:** 0.9.7

## Why it exists

Most agent tooling optimizes for *the agent alone*. Seance optimizes for
**engagement in a shared space**:

| human | agent |
|-------|--------|
| watches every pane live | runs in a real terminal on that screen |
| flips notes, steers, takes over a shell | drives siblings via `seance ctl` |
| answers `ask` toasts; Enter/Esc on ghost commands | prefers `propose` over silent risk |
| triages by status badges | reports `planning` / `working` / `needs-human` |
| steps file-history when an agent edits a doc | opens file panes so edits appear live |

Attribution is first-class: actions are logged as `human` / `agent:<pane>` /
`cli`. The timeline answers “what happened while I was looking elsewhere?”

Any command is a pane. Default summon is a **shell** (so you can always take
the keyboard). Point `--command` at whatever agent CLI you use.

## Features

- **Live multi-pane terminals** — real PTYs, selection, scrollback; auto-grid tile/shelve
- **Workspaces** — keep circles of work apart; sidebar drag-reorder
- **Notes on the back of every pane** — shared markdown (`$SEANCE_SCRATCHPAD`); human and agent both read/write
- **File panes** — live markdown/text + history/diff when you’re co-editing a document
- **Control plane** — `seance ctl` so any pane (or external script) can spawn, send, wait, harvest
- **Orchestrator A+** — `--agent` profiles, `wait --status done` (evidence-bound), `send --file`, task envelopes, `harvest`
- **Human-in-the-loop** — `ask` (blocking choices), `propose` (ghost command until you accept), `human` (where is focus?)
- **Status + timeline** — agent self-report badges; attributed event log
- **Co-presence** — human keystrokes steal keys; seize / release / drive
- **Daemon architecture** — upgrade the binary without killing the circle
- **Event bus** — sequenced, attributable events + `seance ctl watch` subscriptions
- **Causal tint** — left gutter shows who last wrote stdin (human / agent / propose)
- **Capabilities** — `policy open|propose_required|locked` + per-principal grants

## Quick start

```bash
./scripts/bootstrap-deps.sh    # pinned gpui checkout — see docs/PLAYBOOK.md
cargo build --release          # first build can take ~10 min
./target/release/seance

ln -sf "$(pwd)/target/release/seance" ~/.local/bin/seance   # optional
```

Requirements: recent Rust, Vulkan-capable drivers, a monospace font
(default *CaskaydiaMono Nerd Font Mono* — change in `src/term_font.rs`).

```bash
seance ctl skill                 # agent-facing protocol (⚡ arm / paste)
seance ctl doctor                # claude / grok / codex profiles
seance ctl roster                # slug · owner · status · task · pad@rev
seance ctl new --name w --agent claude --wait-ready
seance ctl send w --file /tmp/task.md          # verbatim; returns task_id
seance ctl wait w --status done --timeout 600 --cat   # evidence + harvest
seance ctl harvest w1 w2 w3 --timeout 900      # fan-in done + pad bodies
seance ctl task                                # durable inject body (self)
seance ctl finish --stdin --status done <<'EOF'
answer
EOF
```

Multi-agent live test (in-seance orchestrator): `./scripts/agent-collab-test.sh`
— see `docs/AGENT_COLLAB_TEST.md`.

## Keybinds

| key | action |
|-----|--------|
| ctrl+shift+n | new pane (shell by default) |
| ctrl+shift+s | flip notes ↔ face |
| ctrl+shift+p | pop pane to its own window |
| ctrl+pageup / pagedown | cycle workspaces |
| ctrl+shift+v | paste |
| ⚡ | arm agent (`ctl skill` orientation) |
| 💬 | whisper — compose a steer into the pane |

## Architecture (short)

| process | role |
|---------|------|
| `seance daemon` | owns PTYs, grids, state; Unix socket |
| `seance` (GUI) | shared space UI; reconnects safely |
| `seance ctl …` | JSON-lines client for agents, shells, scripts |

**Do not** `pkill -x seance` to reload — that kills every session. Prefer
`cargo build --release && seance upgrade`, or `seance restart-gui` for UI-only.

| path | |
|------|--|
| state | `~/.local/share/seance/state.json` |
| scratchpads | `~/.local/share/seance/scratch/<slug>.md` |
| file history | `~/.local/share/seance/filehist/` |
| events | `~/.local/share/seance/events.jsonl` |
| socket | `$XDG_RUNTIME_DIR/seance.sock` |

Injected into every pane: `SEANCE_SESSION`, `SEANCE_WORKSPACE`,
`SEANCE_SCRATCHPAD`, `SEANCE_SOCKET`. Workspace scoping is automatic inside a
pane — agents only see their circle unless you pass `--all`.

## Docs

| doc | |
|-----|--|
| [docs/CONTROL.md](docs/CONTROL.md) | control plane + how agents engage the human |
| [docs/DAEMON.md](docs/DAEMON.md) | daemon / GUI split, upgrade path |
| [docs/FILE-PANES.md](docs/FILE-PANES.md) | co-editing documents in the circle |
| [docs/SHELL-INTEGRATION.md](docs/SHELL-INTEGRATION.md) | structured command boundaries |
| [docs/PERF-TERMINAL.md](docs/PERF-TERMINAL.md) | multi-pane paint notes |
| [docs/THEME.md](docs/THEME.md) | candlelit palette |
| [docs/PLAYBOOK.md](docs/PLAYBOOK.md) | GPUI pin / build |
| [docs/ORCHESTRATION.md](docs/ORCHESTRATION.md) | multi-agent swarm playbook (claude/codex/grok) |
| [CLAUDE.md](CLAUDE.md) | notes for coding agents working *on* this repo |

Canonical agent instructions ship in the binary: **`seance ctl skill`**.

## Develop

```bash
./scripts/bootstrap-deps.sh
cargo test
cargo build --release && seance upgrade
```

Pin discipline: `gpui-component` rev-pinned; zed patched to `deps/zed` at
`1a246efd…`. Bump only as a pair — PLAYBOOK.

## Not yet

- OSC 8 hyperlinks / OSC-133 shell markers (bash hooks work today)
- manually resizable splits (grid is auto-balanced)
- GPU glyph atlas (CPU path is already multi-pane smooth)
- new pane kinds (roster / timeline / review — see vita roadmap)
- worktree-backed agent rooms, best-of-N

## License

MIT — see [LICENSE](LICENSE).

Uses [zed’s alacritty fork](https://github.com/zed-industries/alacritty)
(Apache-2.0), [GPUI](https://github.com/zed-industries/zed), and
[gpui-component](https://github.com/longbridge/gpui-component).
