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
cargo test --locked -p codex-app-server --test all suite::v2::sleep::exact_turn_steer_wakes_sleep_with_untrusted_context -- --exact --test-threads=1
