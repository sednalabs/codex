#!/usr/bin/env bash
set -euo pipefail

# This lane is intentionally hosted-only. Keep Playwright and its browser
# cache outside the checkout so validation never changes the repository lockfile
# or dependency graph.
playwright_version="1.55.0"
runner_temp="${RUNNER_TEMP:?RUNNER_TEMP must be set}"

# The named-test host supplies identities after materializing and verifying
# both checkouts. This proof must fail closed if either binding is absent.
if [[ ! "${VALIDATION_TARGET_SHA:-}" =~ ^[0-9a-fA-F]{40}$ ]] || [[ "$(git rev-parse --verify HEAD)" != "${VALIDATION_TARGET_SHA}" ]]; then
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
playwright_venv="$(mktemp -d "$runner_temp/native-browser-skill-venv.XXXXXX")"
native_browser_evidence_dir="$runner_temp/native-browser-evidence"
mkdir -p "$native_browser_evidence_dir"
npm install --prefix "$playwright_prefix" --ignore-scripts "playwright@${playwright_version}"
PLAYWRIGHT_BROWSERS_PATH="${RUNNER_TEMP}/native-browser-browsers" \
  "$playwright_prefix/node_modules/.bin/playwright" install chromium --with-deps

export NODE_PATH="$playwright_prefix/node_modules${NODE_PATH:+:${NODE_PATH}}"
export PLAYWRIGHT_BROWSERS_PATH="${RUNNER_TEMP}/native-browser-browsers"
export NATIVE_BROWSER_EVIDENCE_DIR="$native_browser_evidence_dir"

shopt -s nullglob
test_files=(codex-rs/browser-computer-use/src/*_test.mjs)
shopt -u nullglob
if ((${#test_files[@]} > 0)); then
  node --test "${test_files[@]}"
else
  echo "native-browser-evidence: no standalone Node tests in target; required Rust Chromium flow follows"
fi

python3 -m venv "$playwright_venv"
"$playwright_venv/bin/pip" install --quiet 'PyYAML==6.0.2'
"$playwright_venv/bin/python" \
  codex-rs/skills/src/assets/samples/skill-creator/scripts/quick_validate.py \
  .codex/skills/use-native-browser

browser_node_path="$playwright_prefix/node_modules"
browser_executable_path="$(node -e 'process.stdout.write(require("playwright").chromium.executablePath())')"
browser_state_dir="$(mktemp -d "$runner_temp/native-browser-state.XXXXXX")"
if [[ ! -d "$browser_node_path" || -z "$browser_executable_path" || ! -x "$browser_executable_path" || ! -d "$browser_state_dir" ]]; then
  echo "native-browser-evidence: pinned Playwright runtime preflight failed" >&2
  exit 1
fi

run_rust_selector() {
  local package="$1"
  local test_name="$2"
  local log_path
  shift 2
  log_path="$(mktemp "$runner_temp/native-browser-rust-test.XXXXXX")"

  if ! env "$@" CARGO_TERM_COLOR=never \
    cargo --manifest-path codex-rs/Cargo.toml test --locked -p "$package" --lib "$test_name" -- --exact --test-threads=1 >"$log_path" 2>&1; then
    echo "native-browser-evidence: Rust selector failed: ${package}::${test_name} (test output redacted)" >&2
    LC_ALL=C grep -E '^test result: ' "$log_path" | tail -n 1 >&2 || true
    return 1
  fi

  local passed_count
  local summary_count
  passed_count="$(LC_ALL=C grep -Fxc -- "test ${test_name} ... ok" "$log_path" || true)"
  summary_count="$(LC_ALL=C grep -Fc -- 'test result: ok. 1 passed; 0 failed; 0 ignored;' "$log_path" || true)"
  if [[ "$passed_count" != "1" || "$summary_count" != "1" ]]; then
    echo "native-browser-evidence: Rust selector did not execute exactly once without ignored tests: ${package}::${test_name} (test output redacted)" >&2
    LC_ALL=C grep -E '^test result: ' "$log_path" | tail -n 1 >&2 || true
    return 1
  fi
  printf 'native-browser-evidence: Rust selector passed exactly once: %s::%s\n' "$package" "$test_name"
}

run_rust_selector codex-browser-computer-use tests::hosted_native_browser_tool_flow \
  CODEX_BROWSER_COMPUTER_USE_PROVIDER=playwright \
  "CODEX_BROWSER_PLAYWRIGHT_NODE_PATH=$browser_node_path" \
  "CODEX_BROWSER_PLAYWRIGHT_EXECUTABLE_PATH=$browser_executable_path" \
  "CODEX_BROWSER_PLAYWRIGHT_STATE_DIR=$browser_state_dir" \
  CODEX_BROWSER_PLAYWRIGHT_HEADLESS=1

browser_env_names=(
  CODEX_BROWSER_COMPUTER_USE_PROVIDER
  CODEX_BROWSER_PLAYWRIGHT_NODE_PATH
  CODEX_BROWSER_PLAYWRIGHT_EXECUTABLE_PATH
  CODEX_BROWSER_PLAYWRIGHT_STATE_DIR
  CODEX_BROWSER_PLAYWRIGHT_HEADLESS
)
browser_env_unset=()
for name in "${browser_env_names[@]}"; do
  browser_env_unset+=(-u "$name")
done

run_rust_selector codex-tui dynamic_tools_mcp::native_computer_use_registration_tests::configured_native_provider_specs_are_registered_for_each_transport \
  "${browser_env_unset[@]}"
run_rust_selector codex-tui app::tests::session_lifecycle_requests::browser_dynamic_tool_image_is_forwarded_to_the_app_server_response \
  "${browser_env_unset[@]}"
run_rust_selector codex-exec tests::exec_dispatches_browser_provider_and_preserves_typed_images \
  "${browser_env_unset[@]}"
