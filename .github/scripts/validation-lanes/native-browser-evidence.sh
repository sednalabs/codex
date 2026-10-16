#!/usr/bin/env bash
set -euo pipefail

# This lane is intentionally hosted-only. Keep Playwright and its browser
# cache outside the checkout so validation never changes the repository lockfile
# or dependency graph.
playwright_version="1.55.0"
runner_temp="${RUNNER_TEMP:?RUNNER_TEMP must be set}"

# The named-test host supplies identities after materializing and verifying
# both checkouts. This proof must fail closed if either binding is absent.
target_root="$(git rev-parse --show-toplevel)"
cd "$target_root"
if [[ ! -f codex-rs/Cargo.toml ]] || [[ ! "${VALIDATION_TARGET_SHA:-}" =~ ^[0-9a-fA-F]{40}$ ]] || [[ "$(git rev-parse --verify HEAD)" != "${VALIDATION_TARGET_SHA}" ]]; then
  echo "native-browser-evidence: target checkout identity mismatch" >&2
  exit 1
fi
helper_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
helper_root="$(git -C "${helper_dir}" rev-parse --show-toplevel)"
if [[ ! "${VALIDATION_HARNESS_SHA:-}" =~ ^[0-9a-fA-F]{40}$ ]] || [[ "$(git -C "${helper_root}" rev-parse --verify HEAD)" != "${VALIDATION_HARNESS_SHA}" ]]; then
  echo "native-browser-evidence: trusted helper checkout identity mismatch" >&2
  exit 1
fi

playwright_prefix="$(mktemp -d "$runner_temp/native-browser-playwright.XXXXXX")"
npm install --prefix "$playwright_prefix" --ignore-scripts "playwright@${playwright_version}"
PLAYWRIGHT_BROWSERS_PATH="${RUNNER_TEMP}/native-browser-browsers" \
  "$playwright_prefix/node_modules/.bin/playwright" install chromium --with-deps

export NODE_PATH="$playwright_prefix/node_modules"
export PLAYWRIGHT_BROWSERS_PATH="${RUNNER_TEMP}/native-browser-browsers"

browser_node_path="$playwright_prefix/node_modules"
browser_executable_path="$(node -e 'process.stdout.write(require("playwright").chromium.executablePath())')"
browser_state_dir="$(mktemp -d "$runner_temp/native-browser-state.XXXXXX")"
if [[ ! -d "$browser_node_path" || -z "$browser_executable_path" || ! -x "$browser_executable_path" || ! -d "$browser_state_dir" ]]; then
  echo "native-browser-evidence: pinned Playwright runtime preflight failed" >&2
  exit 1
fi

browser_runtime_env_names=(
  CODEX_BROWSER_COMPUTER_USE_PROVIDER
  CODEX_BROWSER_COMPUTER_USE_COMMAND
  CODEX_BROWSER_COMPUTER_USE_NODE
  CODEX_BROWSER_COMPUTER_USE_TIMEOUT_SECS
  CODEX_BROWSER_PLAYWRIGHT_NODE_PATH
  CODEX_BROWSER_PLAYWRIGHT_EXECUTABLE_PATH
  CODEX_BROWSER_PLAYWRIGHT_STATE_DIR
  CODEX_BROWSER_PLAYWRIGHT_HEADLESS
  CODEX_BROWSER_PLAYWRIGHT_CHANNEL
  CODEX_BROWSER_PLAYWRIGHT_DISPLAY
  CODEX_BROWSER_PLAYWRIGHT_CAPTURE_MODE
  CODEX_BROWSER_PLAYWRIGHT_VIEWPORT_WIDTH
  CODEX_BROWSER_PLAYWRIGHT_VIEWPORT_HEIGHT
  CODEX_BROWSER_PLAYWRIGHT_LOCK_TIMEOUT_MS
  NODE_PATH
  PLAYWRIGHT_BROWSERS_PATH
)
browser_env_unset=()
for name in "${browser_runtime_env_names[@]}"; do
  browser_env_unset+=(-u "$name")
done

# This diagnostic host runs only the fixed, source-bound synthetic Browser
# fixture. Keep Cargo/build stderr private: it can include source and compiler
# details. Default libtest stdout is the existing assertion-failure channel;
# this fixture prints no provider responses, images, artifacts, or environment.
test_name="tests::hosted_native_browser_tool_flow"
test_stdout="$(mktemp "$runner_temp/native-browser-libtest.XXXXXX")"
cargo_stderr="$(mktemp "$runner_temp/native-browser-cargo.XXXXXX")"

if env "${browser_env_unset[@]}" \
  CODEX_BROWSER_COMPUTER_USE_PROVIDER=playwright \
  "CODEX_BROWSER_PLAYWRIGHT_NODE_PATH=$browser_node_path" \
  "CODEX_BROWSER_PLAYWRIGHT_EXECUTABLE_PATH=$browser_executable_path" \
  "CODEX_BROWSER_PLAYWRIGHT_STATE_DIR=$browser_state_dir" \
  CODEX_BROWSER_PLAYWRIGHT_HEADLESS=1 \
  "NODE_PATH=$playwright_prefix/node_modules" \
  "PLAYWRIGHT_BROWSERS_PATH=$RUNNER_TEMP/native-browser-browsers" \
  CARGO_TERM_COLOR=never RUST_BACKTRACE=0 RUST_LIB_BACKTRACE=0 RUST_TEST_NOCAPTURE=0 \
  cargo test --manifest-path codex-rs/Cargo.toml --locked -p codex-browser-computer-use --lib "$test_name" -- --exact --test-threads=1 >"$test_stdout" 2>"$cargo_stderr"; then
  cargo_status=0
else
  cargo_status=$?
fi

if [[ "$cargo_status" != "0" ]]; then
  printf 'native-browser-evidence: cargo_exit=%s\n' "$cargo_status" >&2
  if LC_ALL=C grep -Fxq -- 'running 1 test' "$test_stdout"; then
    echo "native-browser-evidence: failure_stage=selected_synthetic_libtest" >&2
    cat "$test_stdout"
  else
    echo "native-browser-evidence: failure_stage=cargo_or_setup_without_selected_libtest_startup" >&2
    echo "native-browser-evidence: Cargo/build stderr remains private and is not uploaded" >&2
  fi
  exit "$cargo_status"
fi

passed_count="$(LC_ALL=C grep -Fxc -- "test ${test_name} ... ok" "$test_stdout" || true)"
summary_count="$(LC_ALL=C grep -Fc -- 'test result: ok. 1 passed; 0 failed; 0 ignored;' "$test_stdout" || true)"
if [[ "$passed_count" != "1" || "$summary_count" != "1" ]]; then
  echo "native-browser-evidence: Rust selector did not execute exactly once without ignored tests: codex-browser-computer-use::${test_name}" >&2
  exit 1
fi
printf 'native-browser-evidence: Rust selector passed exactly once: codex-browser-computer-use::%s\n' "$test_name"
