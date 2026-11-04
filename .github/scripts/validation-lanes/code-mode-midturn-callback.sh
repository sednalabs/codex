#!/usr/bin/env bash
set -euo pipefail

# Reuse the candidate's normal checksum-verified V8 setup; validation-lab's
# generic Rust lane does not run composite actions from the target checkout.
setup_script="$RUNNER_TEMP/code-mode-setup-v8.sh"
python3 -c 'import pathlib, yaml; p = pathlib.Path(".github/actions/setup-rusty-v8/action.yml"); print(yaml.safe_load(p.read_text())["runs"]["steps"][0]["run"])' > "$setup_script"
TARGET=x86_64-unknown-linux-gnu GITHUB_ENV="$RUNNER_TEMP/code-mode-v8-env" bash "$setup_script"
export RUSTY_V8_ARCHIVE="$RUNNER_TEMP/rusty_v8/librusty_v8_ptrcomp_sandbox_release_x86_64-unknown-linux-gnu.a.gz"
export RUSTY_V8_SRC_BINDING_PATH="$RUNNER_TEMP/rusty_v8/src_binding_ptrcomp_sandbox_release_x86_64-unknown-linux-gnu.rs"
test -f "$RUSTY_V8_ARCHIVE"
test -f "$RUSTY_V8_SRC_BINDING_PATH"

cd codex-rs
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
export RUST_MIN_STACK=8388608
cargo build --locked -p codex-code-mode-host --bin codex-code-mode-host
export CARGO_BIN_EXE_codex_code_mode_host="$CARGO_TARGET_DIR/debug/codex-code-mode-host"
test -x "$CARGO_BIN_EXE_codex_code_mode_host"
cargo test --locked -p codex-core --test code_mode_compaction_callback -- --test-threads=1
cargo test --locked -p codex-core --lib code_mode_notification -- --test-threads=1
cargo test --locked -p codex-core --lib untagged_orphan_exec_output_is_not_a_notification -- --test-threads=1
cargo test --locked -p codex-core --lib normalize_removes_orphan_custom_tool_call_output_panics_in_debug -- --test-threads=1
for case in \
  suite::code_mode::yielded_code_mode_tool_callbacks_keep_their_originating_step \
  suite::code_mode::code_mode_background_keeps_running_on_later_turn_without_wait \
  suite::code_mode::code_mode_interrupt_terminates_active_cells_and_nested_tools; do
  cargo test --locked -p codex-core --test all "$case" -- --exact --test-threads=1
done
