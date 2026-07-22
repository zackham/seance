#!/usr/bin/env bash
# Repo health gate: formatting, zero-warning compile, full test suite.
# Run before committing. Keep this green — warnings are treated as errors.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== cargo fmt --check"
cargo fmt --check

echo "== cargo check (deny warnings)"
RUSTFLAGS="-D warnings" cargo check --all-targets

echo "== cargo test"
cargo test

echo "OK — fmt clean, zero warnings, all tests green"
