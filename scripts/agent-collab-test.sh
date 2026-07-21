#!/usr/bin/env bash
# agent-collab-test — live multi-agent ergonomics exercise for seance
#
# Spawns claude + grok + codex as visible worker panes, injects a task that
# requires reviewing THIS repo's docs + source, waits for finish, and writes
# a synthesis under data/agent-collab-runs/.
#
# Usage (from anywhere, seance daemon must be running):
#   ./scripts/agent-collab-test.sh
#   ./scripts/agent-collab-test.sh --timeout 900
#   SEANCE_BIN=~/.local/bin/seance ./scripts/agent-collab-test.sh
#
# Documented in: docs/AGENT_COLLAB_TEST.md  ·  pointed from CLAUDE.md
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SEANCE="${SEANCE_BIN:-seance}"
WS="collab-test-$(date +%Y%m%d-%H%M%S)"
TIMEOUT="${TIMEOUT:-720}"
OUT_ROOT="${REPO}/data/agent-collab-runs"
RUN_DIR="${OUT_ROOT}/${WS}"
TASK_FILE="${RUN_DIR}/task.md"
ORCH_LOG="${RUN_DIR}/orchestrator.log"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --workspace) WS="$2"; RUN_DIR="${OUT_ROOT}/${WS}"; TASK_FILE="${RUN_DIR}/task.md"; ORCH_LOG="${RUN_DIR}/orchestrator.log"; shift 2 ;;
    -h|--help)
      sed -n '1,20p' "$0"
      exit 0
      ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

mkdir -p "$RUN_DIR"
exec > >(tee -a "$ORCH_LOG") 2>&1

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }

# Always scope ctl to this workspace (avoids name collisions with prior demos).
ctl() {
  $SEANCE ctl --scope "$WS" "$@"
}

# Parse slug from `created <slug>` or JSON.
parse_created_slug() {
  local out="$1"
  if [[ "$out" =~ created[[:space:]]+([A-Za-z0-9_-]+) ]]; then
    echo "${BASH_REMATCH[1]}"
    return 0
  fi
  echo "$out" | python3 -c "
import sys,json,re
t=sys.stdin.read()
try:
  d=json.loads(t)
  data=d.get('data',d) if isinstance(d,dict) else {}
  print(data.get('slug') or data.get('name') or '')
except Exception:
  m=re.search(r'created\\s+([\\w-]+)', t)
  print(m.group(1) if m else '')
" 2>/dev/null
}

log "repo=$REPO workspace=$WS timeout=${TIMEOUT}s"

# --- task ---
cat > "$TASK_FILE" <<'EOF'
You are a worker pane in **seance** (cwd should be the seance repo).

## Before answering — REVIEW THE CODEBASE AND DOCS
Do not invent APIs from memory. Skim at least:
- README.md
- docs/ORCHESTRATION.md
- docs/CONTROL.md
- docs/AGENT_COLLAB_TEST.md (this exercise)
- src/ctl.rs (CLI surface)
- src/agency.rs (ownership)
- src/events.rs (bus)
- src/runtime/engine.rs (send / finish / note / handoff / pad rev)
- src/caps.rs
- `seance ctl skill` and `seance ctl help`

Version under test: **0.9.5** (lifecycle persist, pad_rev, finish --stdin,
self-only status/note/finish, roster, wait --scratchpad since-inject, atomic pads).

## Your job (≤ ~90 lines markdown)

### A. Product / design (after reading real code)
1. **What still blocks A+ multi-agent collab** after 0.9.5? Point at files/lines.
2. **One pane or API to ship next week** vs **one idea to refuse forever**.
3. Does **roster + pad_rev + finish** close the orchestration loop, or what's missing?

### B. Ergonomics (you as WORKER in this run)
4. What felt A+ receiving/finishing this task?
5. What was still painful?
6. One change you'd want most as a worker.

## Completion contract (required)
1. Write the FULL answer via the control plane (preferred):
   ```
   seance ctl finish --stdin --status done --note collab-test <<'ANS'
   # worker: <claude|grok|codex>
   ...your answer...
   ANS
   ```
   Or: `finish --file /tmp/ans-$SEANCE_SESSION.md --status done --note collab-test`
2. Attribute yourself at the top: `# worker: <claude|grok|codex>`
3. Stay in this repo. Be candid and specific with evidence.
4. Do not kill panes. Do not spawn siblings unless needed for the answer.

Begin now.
EOF

ctl new --name task-doc --file "$TASK_FILE" 2>&1 || true

# Unique agent names per run to avoid global slug collisions.
STAMP=$(date +%H%M%S)
declare -A SLUG=()
PANES=()
for agent in claude grok codex; do
  name="ct-${agent}-${STAMP}"
  log "spawn $name --agent $agent --wait-ready"
  out=$(ctl new --name "$name" --cwd "$REPO" --agent "$agent" --wait-ready 2>&1) || {
    log "WARN: new/wait-ready non-zero for $name: $out"
  }
  echo "$out"
  slug=$(parse_created_slug "$out")
  if [[ -z "$slug" ]]; then
    # fallback: name often becomes slug
    slug="$name"
  fi
  SLUG[$agent]="$slug"
  PANES+=("$slug")
  log "  → slug=$slug"
done

log "roster before inject:"
ctl roster 2>&1 || ctl brief 2>&1 || true

for agent in claude grok codex; do
  slug="${SLUG[$agent]}"
  log "send --file → $slug"
  if ! ctl send "$slug" --file "$TASK_FILE" 2>&1; then
    log "send failed — try Enter for staged paste"
    ctl send-raw "$slug" $'\r' 2>&1 || true
  fi
done

log "brief after inject:"
ctl brief --json 2>&1 | head -c 3000 || true
echo

log "wait ${PANES[*]} --status done --timeout $TIMEOUT"
if ctl wait "${PANES[@]}" --status done --timeout "$TIMEOUT" 2>&1; then
  log "wait: all done"
else
  log "wait: timeout/partial — nudge non-done panes with Enter"
  for slug in "${PANES[@]}"; do
    st=$(ctl brief --json 2>/dev/null | python3 -c "
import sys,json
d=json.load(sys.stdin); data=d.get('data',d)
for p in data.get('panes',[]):
  if p.get('slug')=='$slug' or p.get('name')=='$slug':
    print(p.get('status') or 'none')
" 2>/dev/null || echo none)
    if [[ "$st" != "done" ]]; then
      log "nudge $slug (status=$st)"
      ctl send-raw "$slug" $'\r' 2>&1 || true
    fi
  done
  ctl wait "${PANES[@]}" --status done --timeout 360 2>&1 || true
fi

SYN="${RUN_DIR}/SYNTHESIS.md"
{
  echo "# agent collab test — ${WS}"
  echo
  echo "- date: $(date -Iseconds)"
  echo "- seance: 0.9.5+"
  echo "- repo: ${REPO}"
  echo "- panes: ${PANES[*]}"
  echo
  echo "## roster"
  echo '```'
  ctl roster 2>&1 || true
  echo '```'
  echo
} > "$SYN"

for slug in "${PANES[@]}"; do
  pad_out="${RUN_DIR}/${slug}.md"
  log "pad --cat $slug → $pad_out"
  ctl pad "$slug" --cat > "$pad_out" 2>/dev/null || \
    cat "$HOME/.local/share/seance/scratch/${slug}.md" > "$pad_out" 2>/dev/null || \
    echo "(empty)" > "$pad_out"
  {
    echo "## ${slug}"
    echo
    cat "$pad_out"
    echo
    echo "---"
    echo
  } >> "$SYN"
done

{
  echo "## orchestrator notes"
  echo
  echo "- used: \`ctl --scope WS new --agent --wait-ready\`, \`send --file\`, fan-in \`wait --status done\`, \`pad --cat\`, \`roster\`"
  echo "- unique pane names avoid cross-workspace slug collisions"
  echo "- log: \`${ORCH_LOG}\`"
  echo
} >> "$SYN"

ctl new --name synthesis --file "$SYN" 2>&1 || true

log "DONE — synthesis at $SYN"
log "workspace=$WS"
echo "$RUN_DIR"
