#!/usr/bin/env bash
# Fetch the pinned zed checkout that seance patches gpui against.
# See docs/PLAYBOOK.md for why this exists.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ZED_REV="1a246efd7e1b83ab568ec5e3e6c1a43a42e1abba"
DEST="${SEANCE_ZED_PATH:-$ROOT/deps/zed}"

mkdir -p "$(dirname "$DEST")"

if [[ -L "$DEST" ]]; then
  echo "deps/zed is a symlink → $(readlink "$DEST") (leaving it alone)"
  exit 0
fi

if [[ -d "$DEST/.git" ]]; then
  echo "updating existing checkout at $DEST → $ZED_REV"
  git -C "$DEST" fetch --depth 1 origin "$ZED_REV"
  git -C "$DEST" checkout --detach FETCH_HEAD
else
  echo "cloning zed @$ZED_REV into $DEST"
  # Full clone then detach — shallow-by-sha is unreliable across github.
  git clone --filter=blob:none --no-checkout https://github.com/zed-industries/zed.git "$DEST"
  git -C "$DEST" fetch --depth 1 origin "$ZED_REV"
  git -C "$DEST" checkout --detach FETCH_HEAD
fi

echo "ok — gpui at $DEST (rev $ZED_REV)"
echo "next: cargo build --release"
