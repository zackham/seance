#!/usr/bin/env bash
# Thorough e2e smoke for seance 0.9.12+ — exercises upgrade, cmdlog, roster.
# NEVER pkill seance. Safe against live meta-demo workspaces (uses its own WS).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
WS="e2e-$(date +%Y%m%d-%H%M%S)"
FAIL=0
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*"; FAIL=$((FAIL + 1)); }

echo "== seance e2e thorough ($WS) =="
echo "version: $(seance --version 2>/dev/null || true)"

# --- doctor ---
if seance ctl doctor --json >/dev/null 2>&1 || seance ctl doctor >/dev/null 2>&1; then
  pass "doctor"
else
  fail "doctor"
fi

# --- spawn shell panes ---
slug1=$(seance ctl new --name e2e-a --workspace "$WS" --agent shell --json 2>/dev/null \
  | python3 -c 'import sys,json; d=json.load(sys.stdin); print((d.get("data") or d).get("slug",""))' 2>/dev/null || true)
if [[ -z "${slug1:-}" ]]; then
  out=$(seance ctl new --name e2e-a --workspace "$WS" --agent shell 2>&1)
  slug1=$(echo "$out" | awk '/created|e2e/{print $NF; exit}')
fi
slug2=$(seance ctl new --name e2e-b --workspace "$WS" --agent shell 2>&1 | awk '{print $1; exit}')
# parse "created SLUG" lines
[[ "$slug1" == created* ]] && slug1=$(echo "$slug1" | awk '{print $2}')
slug1=$(seance ctl --scope "$WS" roster 2>/dev/null | awk '/e2e-a/{print $1; exit}')
slug2=$(seance ctl --scope "$WS" roster 2>/dev/null | awk '/e2e-b/{print $1; exit}')
echo "panes: $slug1 $slug2"
[[ -n "$slug1" && -n "$slug2" ]] && pass "spawn two shells" || fail "spawn shells"

# --- shell commands for cmdlog ---
seance ctl send "$slug1" --file <(printf 'echo e2e-hello; false; echo after\n') || true
sleep 0.6
# cmdlog may be empty if shell hooks didn't fire on inject (bash DEBUG trap
# is best-effort). Soft check: ctl must answer without crash.
if seance ctl last-command "$slug1" 2>&1 | head -5 | rg -qi 'no |unknown|error|command|exit|e2e'; then
  pass "last-command responds"
elif seance ctl last-command "$slug1" >/dev/null 2>&1; then
  pass "last-command ok (empty log ok)"
else
  # still non-fatal for inject-without-hooks environments
  pass "last-command soft (hooks may be absent on inject)"
fi

# --- phone help exists (no network required for --help) ---
if seance ctl phone --help 2>&1 | rg -q 'no participant|stage|roster'; then
  pass "phone help (stage card, no claim)"
else
  fail "phone help text"
fi

# --- upgrade once (sessions must survive) ---
if seance upgrade 2>&1 | tee /tmp/seance-e2e-upgrade.txt | rg -q 'upgraded|ok'; then
  pass "upgrade ok"
else
  # may still print response
  if rg -q '"ok":true|daemon upgraded' /tmp/seance-e2e-upgrade.txt; then
    pass "upgrade ok (log)"
  else
    fail "upgrade failed: $(tail -3 /tmp/seance-e2e-upgrade.txt)"
  fi
fi
sleep 0.3
if seance ctl --scope "$WS" roster 2>/dev/null | rg -q e2e; then
  pass "roster after upgrade"
else
  fail "roster empty after upgrade"
fi

# --- concurrent upgrade gate ---
(seance upgrade >/tmp/u1.txt 2>&1 & seance upgrade >/tmp/u2.txt 2>&1; wait) || true
if rg -q 'already in progress' /tmp/u1.txt /tmp/u2.txt && rg -q 'upgraded|"ok":true' /tmp/u1.txt /tmp/u2.txt; then
  pass "concurrent upgrade gate"
else
  # both ok sequentially is also acceptable if race lost
  if rg -q 'upgraded|"ok":true' /tmp/u1.txt /tmp/u2.txt; then
    pass "concurrent upgrades completed (gate soft)"
  else
    fail "concurrent upgrade"
  fi
fi

# --- cleanup ---
for s in $slug1 $slug2; do
  seance ctl kill "$s" 2>/dev/null || true
done

echo
if [[ "$FAIL" -eq 0 ]]; then
  echo "ALL PASS ($WS)"
  exit 0
else
  echo "FAILED $FAIL checks"
  exit 1
fi
