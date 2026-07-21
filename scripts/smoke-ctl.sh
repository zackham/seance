#!/usr/bin/env bash
# Headless smoke after upgrade — does not touch live meta-demo workspaces.
# Usage: ./scripts/smoke-ctl.sh
set -euo pipefail

WS="smoke-$(date +%Y%m%d-%H%M%S)"
echo "== seance smoke ($WS) =="

seance ctl doctor --json >/dev/null || seance ctl doctor
seance ctl roster --all >/dev/null

# shell pane (fast)
slug=$(seance ctl new --name "smoke-sh" --workspace "$WS" --agent shell --json \
  | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("data",d).get("slug",""))' 2>/dev/null \
  || true)
if [[ -z "${slug:-}" ]]; then
  # human output path
  out=$(seance ctl new --name "smoke-sh" --workspace "$WS" --agent shell)
  slug=$(echo "$out" | awk '{print $1; exit}')
fi
echo "created $slug"

seance ctl send "$slug" --file <(echo 'echo smoke-ok')
sleep 0.4
seance ctl read "$slug" --lines 8 | head -20 || true
seance ctl finish "$slug" --stdin --status done --empty-ok <<'EOF' || \
  seance ctl status-set --session "$slug" idle smoke 2>/dev/null || true
smoke pad body
EOF

seance ctl roster --scope "$WS" || seance ctl --scope "$WS" roster
seance ctl prompts arm | head -5
seance ctl export-session --workspace "$WS" --title "smoke $WS" || true

echo "ok smoke complete (workspace $WS) — kill when ready:"
echo "  seance ctl kill $slug"
echo "  # or banish workspace via GUI"
