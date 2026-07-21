# Agent collab test

Live multi-agent ergonomics exercise for seance. Spawns **claude**, **grok**,
and **codex** as visible worker panes, injects a task that requires reviewing
**this repo’s docs and source**, waits for `finish`, and writes a durable run
under `data/agent-collab-runs/`.

## Why this exists

Orchestration quality is only real when exercised against real agent CLIs on
the human’s screen. This test is the regression harness for:

- `send --file` (no shell `$` expansion)
- inject → `status=working` + pad baseline
- `wait` fan-in on `--status done`
- `finish` / `pad --cat` / `roster`
- codex pad+socket reachability
- worker self-report vs evidence (`pad_rev`, since-inject)

## Prerequisites

```bash
# seance daemon + GUI running
cargo build --release && seance upgrade   # if you just changed the binary
seance ctl doctor                         # claude / grok / codex all ok
```

## Run

```bash
# from repo root
./scripts/agent-collab-test.sh

# longer timeout (default 720s)
./scripts/agent-collab-test.sh --timeout 900
```

Script is executable; location: `scripts/agent-collab-test.sh`.

## Outputs

| path | content |
|------|---------|
| `data/agent-collab-runs/<workspace>/task.md` | exact inject payload |
| `data/agent-collab-runs/<workspace>/w-*.md` | each worker pad |
| `data/agent-collab-runs/<workspace>/SYNTHESIS.md` | roster + all answers |
| `data/agent-collab-runs/<workspace>/orchestrator.log` | full ctl transcript |

The script also opens file panes (`task-doc`, `synthesis`, optional roadmap)
in the workspace so a human can watch live.

## What workers are asked

1. Review README, `docs/ORCHESTRATION.md`, `docs/CONTROL.md`, core `src/`
2. Answer design questions (gaps, next ship, refuse forever)
3. Debrief worker ergonomics (A+ / pain / one ask)
4. Complete via `seance ctl finish --stdin` (or `--file`)

## Manual one-liner equivalent

```bash
WS=my-run
seance ctl new --name w-claude --workspace $WS --cwd ~/work/seance --agent claude --wait-ready
seance ctl send w-claude --file /path/to/task.md
seance ctl wait w-claude --status done --timeout 600
seance ctl pad w-claude --cat
seance ctl roster --scope $WS   # note: --scope is a global flag → seance ctl --scope $WS roster
```

## Interpreting results

- **Happy path:** zero `read` on the orchestrator; all three `status=done`;
  pads have attributed `finish` stamps and `pad_rev` ≥ 1.
- **Grok paste stall:** multi-line inject sometimes sits on “Enter:send” —
  script nudges with `send-raw $'\r'`.
- **Partial timeout:** pads under the run dir still hold partial answers.

## Related

- `seance ctl skill` — agent contract
- `docs/ORCHESTRATION.md` — A+ playbook
- `CLAUDE.md` — points here for coding agents working on seance
