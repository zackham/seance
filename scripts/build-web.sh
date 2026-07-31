#!/usr/bin/env bash
# Build the seance web client → crates/seance-web/dist/
#
# Requires the rustup toolchain with the wasm32-unknown-unknown target and a
# wasm-bindgen CLI matching the crate's pinned wasm-bindgen version (0.2.126).
# System cargo (no rustup) lacks the wasm32 std — prefer ~/.cargo/bin.
set -euo pipefail
cd "$(dirname "$0")/.."

CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
BINDGEN="${BINDGEN:-$HOME/.cargo/bin/wasm-bindgen}"
[ -x "$CARGO" ] || CARGO=cargo
[ -x "$BINDGEN" ] || BINDGEN=wasm-bindgen

PROFILE="${1:-release}"
FLAG=""
TARGET_DIR="target/wasm32-unknown-unknown/debug"
if [ "$PROFILE" = release ]; then
  FLAG=--release
  TARGET_DIR="target/wasm32-unknown-unknown/release"
fi

"$CARGO" build -p seance-web --target wasm32-unknown-unknown $FLAG

DIST=crates/seance-web/dist
rm -rf "$DIST"
mkdir -p "$DIST"
"$BINDGEN" "$TARGET_DIR/seance_web.wasm" \
  --target web --no-typescript --out-dir "$DIST"
cp crates/seance-web/www/* "$DIST/"

# wasm-opt if available (not required).
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -O2 "$DIST/seance_web_bg.wasm" -o "$DIST/seance_web_bg.wasm.tmp" \
    && mv "$DIST/seance_web_bg.wasm.tmp" "$DIST/seance_web_bg.wasm"
fi

echo "built: $DIST ($(du -sh "$DIST" | cut -f1))"
