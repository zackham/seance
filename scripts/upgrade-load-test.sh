#!/usr/bin/env bash
# Multi-pane upgrade load test (from w-upgrade smoke recipe).
# WARNING: upgrades the LIVE daemon repeatedly. Sessions should survive.
set -euo pipefail
WS="load-$(date +%H%M%S)"
echo "workspace $WS"
PIDS=()
for i in 1 2 3 4 5; do
  seance ctl new --name "load-$i" --workspace "$WS" --command "bash -lc 'while true; do echo seance-$i; sleep 1; done'" >/tmp/load-new-$i.txt 2>&1 || true
done
seance ctl --scope "$WS" roster || true
BASE=$(seance ctl --scope "$WS" roster 2>/dev/null | wc -l)

for r in 1 2 3; do
  echo "=== round $r concurrent upgrade ==="
  seance upgrade >/tmp/up-$r-a.txt 2>&1 &
  seance upgrade >/tmp/up-$r-b.txt 2>&1 &
  wait
  cat /tmp/up-$r-a.txt /tmp/up-$r-b.txt | head -20
  seance ctl --scope "$WS" roster || true
done

echo "done — kill load panes when ready:"
seance ctl --scope "$WS" roster
echo "  seance ctl --scope $WS list  # then kill each"
