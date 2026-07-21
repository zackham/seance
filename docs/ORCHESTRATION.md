# Multi-agent orchestration in seance

How a master agent (or human at a shell) runs Claude / Codex / Grok as
**visible sibling panes** and drives them — with **transferrable agency** so a
human can watch and jump into any session without restarting it.

Validated live 2026-07-20 on seance **0.9.7+** (in-seance orchestrator + ⚡ arm);
through **0.9.12**:

| Agent | Profile (`--agent`) | Result |
|-------|---------------------|--------|
| Claude Code | `claude` | ✅ boot + task + finish |
| Grok Build | `grok` | ✅ boot + task + finish |
| Codex CLI | `codex` (danger-full-access) | ✅ pad + socket reachable |

Related: [decap](https://github.com/zackham/decap) solved *headless print-mode*
for Claude by wrapping interactive Claude in a PTY + hooks + bracketed paste.
Seance already **is** that PTY surface — the remaining work is orchestration
hygiene, not re-implementing decap.

Live harness: [`docs/AGENT_COLLAB_TEST.md`](AGENT_COLLAB_TEST.md) /
`./scripts/agent-collab-test.sh` (orchestrator **pane**, not external script).

---

## Orchestrator A+ (0.9.3–0.9.7) — for agents driving the stage

What makes *silicon* orchestration A+ (not human watching):

```bash
seance ctl doctor
seance ctl roster                       # slug · owner · status · task · pad@rev
seance ctl new --name w --agent claude --wait-ready
# FOOTGUN: shell expands $VARS in bare send text — use --file
# NOTE: created slug may be w-2 if w exists — use the id `created` prints
cat > /tmp/task.md <<'EOF'
Review README.md + docs/ORCHESTRATION.md. Answer in markdown.
When done: seance ctl finish --stdin --status done
QUESTION: ...
EOF
seance ctl send w --file /tmp/task.md   # → task=task-N status=working
seance ctl wait w --status done --timeout 600 --cat   # evidence + harvest
# fan-in harvest:
seance ctl harvest w-claude-4 w-grok-4 w-codex-4 --timeout 900
```

| Anti-pattern | A+ pattern |
|--------------|------------|
| `sleep 5; read; grep` | `wait --status done` / `--scratchpad` |
| 4× `read` for state | `roster` / `brief --json` once |
| hand-rolled absolute paths | `--agent claude --wait-ready` |
| bare `send` with `$SEANCE_*` | `send --file` / `--stdin` |
| path → cat → read for pad | `wait … --cat` / `harvest` / `pad --cat` |
| badge-only “done” | evidence-bound wait (pad grew since inject) |
| external bash as master | **orchestrator pane** inside seance (⚡ arm first) |
| codex sandbox blocks pad | profile uses `danger-full-access` + `finish` |
| re-task false-ready on old done | inject auto-sets `status=working` |

---

## Co-presence (0.9.2) — human jumps in

| API | Effect |
|-----|--------|
| Human key / paste | **always steals** keys (`owner=human`) |
| `ctl send` while human owns | **denied** for 3s idle grace (or until `release`) |
| `ctl seize PANE` | claim keys as human |
| `ctl release PANE` | `owner=none` — either may drive |
| `ctl drive PANE locked_human` | agents cannot inject even after idle |
| `ctl human` | lists owner / drive_mode / exited per pane |
| `ctl wait PANE --owner none` | poll until free for inject |
| process exit | **tombstone** retained until `kill` |

Chrome: pane border / strip shows `⌨ you` / `⚡ agent` / `☠ exit N`.

---

## What works today (live smoke)

Workspace `orchestrate`, master = external `seance ctl` (same ops a pane master
would use under `$SEANCE_WORKSPACE` scope).

```bash
# 1. spawn (absolute paths + permission flags)
seance ctl new --name w-claude --workspace orchestrate --cwd ~/work/seance \
  --command '/home/zack/.local/bin/claude --dangerously-skip-permissions'
seance ctl new --name w-grok --workspace orchestrate --cwd ~/work/seance \
  --command '/home/zack/.grok/bin/grok --always-approve'
seance ctl new --name w-codex --workspace orchestrate --cwd /tmp \
  --command '/path/to/codex -a never -s workspace-write'

# 2. clear boot dialogs
seance ctl send-raw w-claude $'\r'          # trust folder → Yes
seance ctl send-raw w-codex  $'2\r'         # update menu → Skip

# 3. drive
seance ctl send w-claude '…task…'
seance ctl send w-grok   '…task…'
seance ctl send w-codex  '…task…'

# 4. poll
seance ctl read w-claude --lines 40
seance ctl status w-claude --json   # title often reflects activity
```

**Claude + Grok** completed a trivial parallel reply task in ~3s after send.
**Codex** accepted the prompt but failed with:

```text
The 'gpt-5.6-sol' model requires a newer version of Codex.
Please upgrade to the latest app or CLI and try again.
```

Codex on this host is **0.94.0**; latest advertised was **0.144.x**. That's a
host/config problem, not a seance inject problem.

---

## Failure modes discovered

### 1. Boot friction (every cold worker)

| Surface | Dialog | Clear |
|---------|--------|-------|
| Claude | trust this folder | Enter (`send-raw $'\r'`) |
| Codex | update available | `send-raw $'2\r'` (Skip) |
| PATH | `claude` as shell alias | **aliases never apply** in `bash -lc 'exec …'` — use absolute path |

If the process exits during boot, seance **auto-closes the pane**
(`pane_exited` → kill). Timeline is the debug tool:

```bash
seance ctl timeline --since 5m
# look for pane_exited with exit codes: 126=not exec, 127=not found, 2=app error
```

### 2. Completion false positives

`read | grep TOKEN` matches the **injected prompt still on screen**, not only
the agent reply. In the smoke, Codex looked "done" because the marker was in
the prompt while the model error sat below.

**Honest idle checks:**

1. Screen hash / text stable across 2–3 polls spaced ≥1s, **and**
2. Idle prompt glyph reappeared (`❯` / `›` / empty input box), **or**
3. Durable artifact exists (scratchpad line / output file) that wasn't there
   before the send (best).

Terminal **titles** update with activity on Claude/Grok (`status --json` →
`title`) — useful secondary signal, not a contract.

### 3. Permission / trust policy mismatch

Unattended multi-agent **requires** skip/always-approve on workers or they hang
on tool confirmations. That fights seance's `policy propose_required` if you
also want ghost-approve for shell risk. Recommended split:

- **Agent TUI workers:** skip-permissions flags (they are the sandboxed actors).
- **Shell panes** the human cares about: seance `propose` / `policy` for inject.

### 4. Command string hygiene

`seance ctl new --command` is interpolated into `bash -lc "exec …"`. Paths with
spaces need care; never feed `ls` output into `--command` (eza headers became a
literal command in one failed smoke). Prefer absolute paths with no spaces.

### 5. Codex host debt (this machine)

Interactive Codex works only after upgrade or model pin to something 0.94
supports. Until then, treat Codex as non-ready for swarm, or use `codex exec`
in a **shell** pane (less visible TUI, but functional).

---

## Mapping to decap

| Decap technique | Seance equivalent |
|-----------------|-------------------|
| Real PTY via forkpty | daemon `PtySession` |
| libghostty-vt screen state | alacritty grid → `ctl read` rendered text |
| Bracketed paste inject | `ctl send` (app-side `\x1b[200~…\x1b[201~` + settle + CR) |
| SessionStart / Stop hooks | **missing** — we poll screens / titles / scratchpads |
| Transcript offset parse | **missing** — no structured turn boundary |
| print/json/stream-json | not the product; human watches the TUI |

**Conclusion:** do not port decap wholesale. Steal the *lifecycle* idea later
(structured turn begin/end via hooks or OSC) if polling stays painful. For now
the control plane is enough for a careful master agent.

---

## Recommended master agent playbook

Canonical copy also lives in `seance ctl skill` (multi-agent section).

1. Open / select a dedicated workspace (`--workspace lab` or inherit yours).
2. Spawn workers with **absolute paths + permission flags** (recipes in skill).
3. `read` each; clear boot dialogs before any real task.
4. Task with an **output contract**: write summary to `$SEANCE_SCRATCHPAD` or a
   shared file; open a file pane so the human sees it live.
5. Drive in parallel; `status-set` yourself; optional `watch` for exits.
6. Detect done via artifact / stable idle — never grep-only.
7. `kill` workers you created when truly finished.

---

## Open product decisions (for roadmap)

See the seance roadmap working doc. High-signal choices:

1. **Agent profiles** — `ctl new --agent claude` expands to path+flags?
2. **`ctl wait`** — first-class stable-screen / contains-after-seq helper?
3. **Keep pane on process exit** (tombstone) vs auto-close (current)?
4. **Claude SessionStart/Stop hooks** for true turn boundaries (decap-style)?
5. Default **permission posture** for swarm vs solo?

---

## Cleanup of the 2026-07-20 smoke

```bash
seance ctl kill w-claude --all
seance ctl kill w-grok --all
seance ctl kill w-codex --all
seance ctl kill orch --all
```
