#!/usr/bin/env bash
set -euo pipefail

cd codex-rs
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
cargo build --locked -p codex-code-mode-host --bin codex-code-mode-host
export CARGO_BIN_EXE_codex_code_mode_host="$CARGO_TARGET_DIR/debug/codex-code-mode-host"
test -x "$CARGO_BIN_EXE_codex_code_mode_host"
cargo test --locked -p codex-core --test code_mode_compaction_callback -- --test-threads=1
