#!/usr/bin/env bash
# agent-collab-test — bootstrap an *in-seance* orchestrator pane
#
# This script does NOT drive workers itself. It only:
#   1. opens a workspace + file panes for the human to watch
#   2. spawns one orchestrator agent pane (claude by default)
#   3. injects a brief: "you are the master — spawn claude/grok/codex,
#      give them a product task, wait, synthesize, finish"
#   4. waits for the orchestrator to status=done
#   5. dumps pads + a run dir so a human/outer agent can *then* interview
#      everyone about ergonomics (workers must NOT know that interview is coming)
#
# Ergonomics interviews are intentionally OUT of band: after this script exits,
# the outer agent (or you) injects a second prompt into each pane.
#
# Usage:
#   ./scripts/agent-collab-test.sh
#   ./scripts/agent-collab-test.sh --timeout 1200
#   ./scripts/agent-collab-test.sh --orch-agent claude
#
# Docs: docs/AGENT_COLLAB_TEST.md  ·  CLAUDE.md
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SEANCE="${SEANCE_BIN:-seance}"
WS="collab-$(date +%Y%m%d-%H%M%S)"
TIMEOUT="${TIMEOUT:-1200}"
ORCH_AGENT="${ORCH_AGENT:-claude}"
OUT_ROOT="${REPO}/data/agent-collab-runs"
RUN_DIR="${OUT_ROOT}/${WS}"
ORCH_BRIEF="${RUN_DIR}/orchestrator-brief.md"
WORKER_TASK="${RUN_DIR}/worker-product-task.md"
BOOT_LOG="${RUN_DIR}/bootstrap.log"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --workspace) WS="$2"; RUN_DIR="${OUT_ROOT}/${WS}"; ORCH_BRIEF="${RUN_DIR}/orchestrator-brief.md"; WORKER_TASK="${RUN_DIR}/worker-product-task.md"; BOOT_LOG="${RUN_DIR}/bootstrap.log"; shift 2 ;;
    --orch-agent) ORCH_AGENT="$2"; shift 2 ;;
    -h|--help) sed -n '1,28p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

mkdir -p "$RUN_DIR"
exec > >(tee -a "$BOOT_LOG") 2>&1

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }

ctl() { $SEANCE ctl --scope "$WS" "$@"; }

parse_created_slug() {
  local out="$1"
  if [[ "$out" =~ created[[:space:]]+([A-Za-z0-9_-]+) ]]; then
    echo "${BASH_REMATCH[1]}"
    return 0
  fi
  echo "$out" | python3 -c "
import sys,re,json
t=sys.stdin.read()
try:
  d=json.loads(t); data=d.get('data',d) if isinstance(d,dict) else {}
  print(data.get('slug') or data.get('name') or '')
except Exception:
  m=re.search(r'created\\s+([\\w-]+)', t)
  print(m.group(1) if m else '')
" 2>/dev/null
}

log "repo=$REPO workspace=$WS orch_agent=$ORCH_AGENT timeout=${TIMEOUT}s"

# --- pure product task for WORKERS (no ergonomics interview language) ---
cat > "$WORKER_TASK" <<EOF
# seance product task (worker)

You are a worker pane inside **seance**. cwd is the seance repo:
  $REPO

## Do this first
Review real docs and source (do not invent APIs from memory):
- README.md
- docs/ORCHESTRATION.md
- docs/CONTROL.md
- docs/AGENT_COLLAB_TEST.md (methodology only — ignore any stale ergonomics wording)
- seance ctl skill && seance ctl help && seance ctl roster
- src/ctl.rs, src/agency.rs, src/events.rs, src/caps.rs
- src/runtime/engine.rs (send / finish / task / handoff)

## Answer (markdown, ≤ ~80 lines)
1. Highest-leverage product improvements for seance as a human↔agent collab stage
   (visibility, co-presence, multi-agent orchestration) — prioritize ruthlessly.
2. What already works that we must not break.
3. One concrete next ship (pane or API) vs one idea to refuse forever.
4. Cite files/lines where you can.

## Complete
Write the full answer with:
  seance ctl finish --stdin --status done --note product <<'ANS'
  # worker: <claude|grok|codex>
  ...answer...
  ANS
(or finish --file /tmp/ans-\$SEANCE_SESSION.md --status done --note product)

Stay in this repo. Do not spawn siblings. Do not interview anyone.
EOF

# --- orchestrator brief: YOU are the seance master pane ---
cat > "$ORCH_BRIEF" <<EOF
# you are the orchestrator pane

You are a **master agent inside seance**, not an external script. This pane is
live on the human's screen. Your job is multi-agent product work — not a
meta-study of ergonomics.

Workspace is already scoped via \$SEANCE_WORKSPACE ($WS).
Repo: $REPO
Worker product task file (on disk, also opened as a file pane if present):
  $WORKER_TASK

## Protocol (learn it for real)
1. \`seance ctl skill\` and \`seance ctl doctor\`
2. Prefer structure over screens: brief / roster / wait / send --file / pad --cat / task
3. \`send --file\` for long payloads (shell expands \$VARS in bare send text)
4. \`wait PANE… --status done\` is evidence-bound (pad must grow since inject)
5. Workers complete with \`finish\` (body required for done)

## What to do
1. Spawn three worker panes in **this** workspace, cwd=$REPO:
     seance ctl new --name w-claude --cwd $REPO --agent claude --wait-ready
     seance ctl new --name w-grok   --cwd $REPO --agent grok   --wait-ready
     seance ctl new --name w-codex  --cwd $REPO --agent codex  --wait-ready
   (Use unique names if needed; record the real slugs from \`created …\`.)
2. Inject the **product task only** — send the contents of:
     $WORKER_TASK
   via \`seance ctl send <slug> --file $WORKER_TASK\`
   Do **not** add ergonomics / debrief / interview instructions. Workers should
   only think about seance product improvements.
3. Fan-in: \`seance ctl wait <slugs…> --status done --timeout 900\`
   If a pane stalls on a paste "Enter:send" UI, \`send-raw <slug> \$'\\r'\` once.
4. Collect answers: \`seance ctl pad <slug> --cat\` for each worker.
5. Write a short synthesis on **your** scratchpad (orchestrator view: what they
   agreed on, disagreements, what you'd ship next). Then:
     seance ctl finish --stdin --status done --note orch-synthesis <<'SYN'
     # orchestrator synthesis
     ...
     worker slugs: …
     SYN

## Rules
- You drive workers with seance ctl from **this** pane (you have \$SEANCE_SESSION).
- Prefer --file / finish / wait / roster over read loops.
- Do not kill panes you did not create.
- Do not ask workers about their "experience using seance" — product answers only.
- When fully done, your status must be **done** with a non-empty finish body.

Begin now.
EOF

# file panes for human watch
ctl new --name worker-task --file "$WORKER_TASK" 2>&1 || true
ctl new --name orch-brief --file "$ORCH_BRIEF" 2>&1 || true

# spawn orchestrator *pane* (the point of this harness)
STAMP=$(date +%H%M%S)
ORCH_NAME="orch-${STAMP}"
log "spawn orchestrator $ORCH_NAME --agent $ORCH_AGENT --wait-ready"
out=$(ctl new --name "$ORCH_NAME" --cwd "$REPO" --agent "$ORCH_AGENT" --wait-ready 2>&1) || {
  log "WARN: orch wait-ready: $out"
}
echo "$out"
ORCH_SLUG=$(parse_created_slug "$out")
[[ -n "$ORCH_SLUG" ]] || ORCH_SLUG="$ORCH_NAME"
log "orchestrator slug=$ORCH_SLUG"

log "inject orchestrator brief via --file"
ctl send "$ORCH_SLUG" --file "$ORCH_BRIEF" 2>&1 || {
  log "send failed — Enter nudge"
  ctl send-raw "$ORCH_SLUG" $'\r' 2>&1 || true
}

log "roster:"
ctl roster 2>&1 || true

log "waiting for orchestrator $ORCH_SLUG --status done (timeout ${TIMEOUT}s)"
if ctl wait "$ORCH_SLUG" --status done --timeout "$TIMEOUT" 2>&1; then
  log "orchestrator done"
else
  log "orchestrator wait timeout/partial — one Enter nudge then short rewait"
  ctl send-raw "$ORCH_SLUG" $'\r' 2>&1 || true
  ctl wait "$ORCH_SLUG" --status done --timeout 300 2>&1 || log "still not done"
fi

# dump state for outer interviewer
{
  echo "# collab run — $WS"
  echo
  echo "- date: $(date -Iseconds)"
  echo "- orchestrator: $ORCH_SLUG (agent=$ORCH_AGENT)"
  echo "- workspace: $WS"
  echo "- methodology: in-seance orchestrator; product task only; ergonomics interview AFTER"
  echo
  echo "## roster"
  echo '```'
  ctl roster 2>&1 || true
  echo '```'
  echo
  echo "## orchestrator pad"
  echo
  ctl pad "$ORCH_SLUG" --cat 2>/dev/null || true
  echo
  echo "## panes (for post-run interview)"
  echo
  ctl brief --json 2>/dev/null | python3 -c "
import sys,json
d=json.load(sys.stdin); data=d.get('data',d)
for p in data.get('panes',[]):
  if p.get('kind')!='terminal': continue
  print(f\"- slug={p.get('slug')} status={p.get('status')} pad={p.get('scratchpad_bytes')} title={(p.get('title') or '')[:60]}\")
" 2>/dev/null || true
} > "$RUN_DIR/RUN.md"

# collect every terminal pad
for slug in $(ctl brief --json 2>/dev/null | python3 -c "
import sys,json
d=json.load(sys.stdin); data=d.get('data',d)
for p in data.get('panes',[]):
  if p.get('kind')=='terminal':
    print(p.get('slug'))
" 2>/dev/null); do
  ctl pad "$slug" --cat > "$RUN_DIR/${slug}.md" 2>/dev/null || true
done

ctl new --name run-summary --file "$RUN_DIR/RUN.md" 2>&1 || true

# machine-readable handoff for the outer interviewer
cat > "$RUN_DIR/handoff.json" <<EOF
{
  "workspace": "$WS",
  "orchestrator_slug": "$ORCH_SLUG",
  "run_dir": "$RUN_DIR",
  "interview_pending": true,
  "note": "Product phase complete (or timed out). Outer agent should now interview orchestrator + workers about seance ergonomics without re-running product work."
}
EOF

log "DONE product phase"
log "workspace=$WS orchestrator=$ORCH_SLUG"
log "run dir: $RUN_DIR"
log "NEXT: outer agent interviews panes about ergonomics (not done by this script)"
echo "$RUN_DIR"
echo "ORCH_SLUG=$ORCH_SLUG"
echo "WORKSPACE=$WS"
