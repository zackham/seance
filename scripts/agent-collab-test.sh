#!/usr/bin/env bash
# agent-collab-test — bootstrap an *in-seance* orchestrator pane
#
# This script does NOT drive workers itself. It only:
#   1. opens a workspace + product task file panes
#   2. spawns one orchestrator agent pane (claude by default)
#   3. ⚡-arms it with SEANCE_ARM_PROMPT (exact text from src/app.rs — same as
#      the arm button), then sends a short task: spawn claude/grok/codex,
#      run the product task, synthesize, finish
#   4. waits for the orchestrator to status=done
#   5. dumps pads so a human/outer agent can *then* interview about ergonomics
#
# The harness does not teach ctl. Arm → `seance ctl skill` should.
# Ergonomics interviews stay OUT of band (after product work).
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

# --- pure product task for WORKERS ---
cat > "$WORKER_TASK" <<EOF
# seance product task

cwd: $REPO

Review the seance repo (README, docs/, src/ as needed — don't invent APIs).
Answer in markdown (≤ ~80 lines):

1. Highest-leverage product improvements for seance as a human↔agent collab stage
   (visibility, co-presence, multi-agent orchestration). Prioritize ruthlessly.
2. What already works that we must not break.
3. One concrete next ship (pane or API) vs one idea to refuse forever.
4. Cite files/lines where you can.

When done, put the full answer on your seance scratchpad and mark yourself done
via seance ctl (finish or pad + status-set — follow seance orientation if armed).

Stay in this repo. Do not spawn siblings.
EOF

# --- ⚡ arm prompt: EXACT copy of SEANCE_ARM_PROMPT in src/app.rs ---
# Extracted at run time so the harness always tests the real arm button text.
ARM_FILE="$RUN_DIR/arm-prompt.md"
python3 - "$REPO" "$ARM_FILE" <<'PY'
import re, sys
from pathlib import Path
repo, out = sys.argv[1], sys.argv[2]
src = (Path(repo) / "src/app.rs").read_text()
i = src.index('const SEANCE_ARM_PROMPT')
i = src.index('= "', i) + 3
chunk = src[i:]
out_chars = []
j = 0
while j < len(chunk):
    c = chunk[j]
    if c == '"':
        break
    if c == "\\" and j + 1 < len(chunk):
        n = chunk[j + 1]
        if n == "\n":
            j += 2
            continue
        if n == '"':
            out_chars.append('"'); j += 2; continue
        if n == "n":
            out_chars.append("\n"); j += 2; continue
        if n == "t":
            out_chars.append("\t"); j += 2; continue
        if n == "\\":
            out_chars.append("\\"); j += 2; continue
        out_chars.append(n); j += 2; continue
    out_chars.append(c); j += 1
Path(out).write_text("".join(out_chars))
print(f"arm prompt {len(out_chars)} chars → {out}")
PY

# --- post-arm task: minimal — orchestration learned from skill, not this brief ---
cat > "$ORCH_BRIEF" <<EOF
Spawn three agent panes in this workspace (claude, grok, and codex) with cwd
$REPO. Give each the product task at:

  $WORKER_TASK

(use send --file so the body is verbatim). Have them complete it, collect their
answers, write a short synthesis to your scratchpad, and mark yourself done.

Repo root: $REPO
EOF

# file panes for human watch
ctl new --name worker-task --file "$WORKER_TASK" 2>&1 || true
ctl new --name orch-task --file "$ORCH_BRIEF" 2>&1 || true

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

# Phase A: same inject as the ⚡ arm button
log "⚡ arm orchestrator (SEANCE_ARM_PROMPT from src/app.rs)"
ctl send "$ORCH_SLUG" --file "$ARM_FILE" 2>&1 || {
  log "arm send failed — Enter nudge"
  ctl send-raw "$ORCH_SLUG" $'\r' 2>&1 || true
}
# Give the agent a beat to run ctl skill / confirm orientation.
# Arm ends with "wait for the next instruction" — then we send the real task.
sleep 8
# If still owned by agent mid-turn, release isn't needed for re-inject from cli
# (same cli principal). Nudge Enter if paste stalled.
ctl send-raw "$ORCH_SLUG" $'\r' 2>/dev/null || true
sleep 2

# Phase B: minimal product orchestration ask
log "inject post-arm task (minimal — no protocol hand-holding)"
ctl send "$ORCH_SLUG" --file "$ORCH_BRIEF" 2>&1 || {
  log "task send failed — Enter nudge"
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
