# Agent collab test

Live multi-agent exercise for seance with an **in-seance orchestrator pane**.

## What this is (and is not)

| This harness | Not this |
|--------------|----------|
| Spawns one **orchestrator agent pane** inside seance | External bash driving all three workers |
| Orchestrator uses `seance ctl` to spawn/drive claude+grok+codex | Script hard-codes worker spawn + inject |
| Workers get a **product** task only (seance improvements) | Workers primed for an ergonomics debrief |
| Outer agent (or human) **interviews after** product work | Ergonomics questions baked into the first inject |

The point: exercise the real master path (a pane on the human's screen figuring
out orchestration), and measure ergonomics honestly — workers should not
optimize for an interview while doing product work.

## Phases

```
┌─────────────────────┐     ┌──────────────────────────┐     ┌────────────────────┐
│ 1. Bootstrap script │ --> │ 2. Orchestrator pane     │ --> │ 3. Outer interview │
│    (this repo)      │     │    (claude in seance)    │     │    (you / session) │
└─────────────────────┘     └──────────────────────────┘     └────────────────────┘
  open workspace              spawn w-claude/grok/codex         inject debrief
  spawn orch pane             product task only                 after finish
  inject orch brief           wait + synthesize + finish
  wait orch done
```

### Phase 1 — bootstrap (`./scripts/agent-collab-test.sh`)

- Creates `data/agent-collab-runs/<workspace>/`
- Writes `worker-product-task.md` (pure product) and `orchestrator-brief.md`
- Opens those as file panes
- `new --agent claude --wait-ready` for **one** orchestrator
- `send --file` the orchestrator brief
- `wait` until orchestrator `status=done`
- Dumps pads + `RUN.md` + `handoff.json`

### Phase 2 — orchestrator (inside seance)

Briefed to:

1. `ctl skill` / `doctor`
2. Spawn three workers with `--agent` + `--wait-ready`
3. `send --file` the **product** task only (no ergonomics language)
4. `wait … --status done`
5. Collect pads, write synthesis on **its** pad, `finish`

### Phase 3 — interview (outer agent / human)

**After** phase 1 exits, interview each terminal pane about experience:

- What felt A+ about working *in* seance?
- What was painful (inject, finish, pad, wait, discoverability)?
- One change you'd want most (as worker or as orchestrator)?

Do **not** re-run the product task. Prefer `send --file` of a short debrief
prompt; wait; `pad --cat`.

Suggested debrief inject (workers):

```text
Product work is done — do not redo it.

Short ergonomics debrief only (≤40 lines). You were a worker pane in seance.
1. What felt A+?
2. What was painful?
3. One change you'd want most as a worker.

seance ctl finish --stdin --status done --note debrief <<'ANS'
# debrief: <agent>
...
ANS
```

Suggested debrief for the **orchestrator** pane: same questions from the
master seat (send/wait/fan-in/discoverability).

## Prerequisites

```bash
cargo build --release && seance upgrade
seance ctl doctor    # claude / grok / codex ok
```

## Run

```bash
./scripts/agent-collab-test.sh
./scripts/agent-collab-test.sh --timeout 1200
./scripts/agent-collab-test.sh --orch-agent claude
```

## Outputs

| path | content |
|------|---------|
| `…/orchestrator-brief.md` | master brief |
| `…/worker-product-task.md` | product-only worker task |
| `…/RUN.md` | roster + orch pad + pane list |
| `…/handoff.json` | workspace + orch slug for interview phase |
| `…/<slug>.md` | each terminal pad dump |
| `…/bootstrap.log` | bootstrap transcript |

## Related

- `seance ctl skill` — agent contract
- `docs/ORCHESTRATION.md` — A+ playbook
- `CLAUDE.md` — pointer for coding agents
