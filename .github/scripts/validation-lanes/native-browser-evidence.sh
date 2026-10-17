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

# The trusted host owns Cargo argv, target inventory, and exact non-ignored
# outcome reconciliation. Its full results can contain fixture URLs or paths;
# retain only this fixed selector/status/count projection, never diagnostics.
umask 077
results_path="$target_root/rust-tests-v1-results.json"
results_pending=0
runner_log=""
cleanup_named_results() {
  if [[ "$results_pending" == "1" ]]; then
    if ! rm -f -- "$results_path" 2>/dev/null; then
      echo "native-browser-evidence: temporary result cleanup failed" >&2
      return 1
    fi
    results_pending=0
  fi
  if [[ -n "$runner_log" ]]; then
    if ! rm -f -- "$runner_log" 2>/dev/null; then
      echo "native-browser-evidence: temporary runner log cleanup failed" >&2
      return 1
    fi
    runner_log=""
  fi
}
trap cleanup_named_results EXIT

run_fixed_request() {
  local request="$1"
  local runner_status projection_status
  shift
  if [[ -e "$results_path" || -L "$results_path" ]]; then
    echo "native-browser-evidence: refusing to overwrite existing named-test results" >&2
    return 1
  fi
  runner_log="$(mktemp "$runner_temp/native-browser-named-runner.XXXXXX")" || return 1
  results_pending=1
  if env "$@" CARGO_TERM_COLOR=never RUST_BACKTRACE=0 RUST_LIB_BACKTRACE=0 RUST_TEST_NOCAPTURE=0 \
    VALIDATION_PROFILE=rust_integration "RUST_TEST_REQUEST_JSON=$request" \
    python3 "$helper_root/.github/scripts/run_named_rust_tests.py" >"$runner_log" 2>&1; then
    runner_status=0
  else
    runner_status=$?
  fi

  if RUST_TEST_REQUEST_JSON="$request" NAMED_RUNNER_STATUS="$runner_status" python3 - <<'PY'
import json
import os
from pathlib import Path

expected = json.loads(os.environ["RUST_TEST_REQUEST_JSON"])
try:
    result = json.loads(Path("rust-tests-v1-results.json").read_text(encoding="utf-8"))
    if not isinstance(result, dict):
        result = {}
except (OSError, UnicodeError, json.JSONDecodeError):
    result = {}

rows = result.get("tests", [])
if not isinstance(rows, list) or any(not isinstance(row, dict) for row in rows):
    rows = []
identity = result.get("identity", {})
if not isinstance(identity, dict):
    identity = {}
bound = (
    result.get("schema_version") == "rust-tests-v1"
    and result.get("request") == expected
    and result.get("candidate_sha") == os.environ["VALIDATION_TARGET_SHA"]
    and identity.get("harness_sha") == os.environ["VALIDATION_HARNESS_SHA"]
    and [row.get("name") for row in rows] == expected["tests"]
)
accepted = (
    bound
    and os.environ["NAMED_RUNNER_STATUS"] == "0"
    and result.get("status") == "success"
)
for name in expected["tests"]:
    matches = [row for row in rows if row.get("name") == name]
    row = matches[0] if len(matches) == 1 else {}
    counts = row.get("result_counts")
    if not isinstance(counts, dict):
        counts = {}
    safe_counts = {}
    for key in ("passed", "failed", "ignored"):
        value = counts.get(key)
        safe_counts[key] = value if type(value) is int and value >= 0 else None
    passed = (
        bound
        and row.get("status") == "success"
        and row.get("execution_reconciled") is True
        and safe_counts == {"passed": 1, "failed": 0, "ignored": 0}
    )
    accepted = accepted and passed
    print(json.dumps({
        "selector": f"{expected['package']}::{name}",
        "status": "pass" if passed else "fail",
        **safe_counts,
    }, sort_keys=True))
raise SystemExit(0 if accepted else 1)
PY
  then
    projection_status=0
  else
    projection_status=$?
  fi
  cleanup_named_results || return 1
  [[ "$runner_status" == "0" && "$projection_status" == "0" ]]
}

validation_status=0
run_fixed_request '{"schema_version":"rust-tests-v1","profile":"rust_integration","package":"codex-browser-computer-use","target_kind":"lib","target":"","execution_mode":"exact","tests":["tests::hosted_native_browser_tool_flow"]}' \
  "${browser_env_unset[@]}" \
  CODEX_BROWSER_COMPUTER_USE_PROVIDER=playwright \
  "CODEX_BROWSER_PLAYWRIGHT_NODE_PATH=$browser_node_path" \
  "CODEX_BROWSER_PLAYWRIGHT_EXECUTABLE_PATH=$browser_executable_path" \
  "CODEX_BROWSER_PLAYWRIGHT_STATE_DIR=$browser_state_dir" \
  CODEX_BROWSER_PLAYWRIGHT_HEADLESS=1 \
  "NODE_PATH=$playwright_prefix/node_modules" \
  "PLAYWRIGHT_BROWSERS_PATH=$RUNNER_TEMP/native-browser-browsers" || validation_status=1

run_fixed_request '{"schema_version":"rust-tests-v1","profile":"rust_integration","package":"codex-tui","target_kind":"lib","target":"","execution_mode":"exact","tests":["dynamic_tools_mcp::native_computer_use_registration_tests::configured_native_provider_specs_are_registered_for_each_transport","app::tests::session_lifecycle_requests::browser_dynamic_tool_image_is_forwarded_to_the_app_server_response"]}' \
  "${browser_env_unset[@]}" || validation_status=1
run_fixed_request '{"schema_version":"rust-tests-v1","profile":"rust_integration","package":"codex-exec","target_kind":"lib","target":"","execution_mode":"exact","tests":["tests::exec_dispatches_browser_provider_and_preserves_typed_images"]}' \
  "${browser_env_unset[@]}" || validation_status=1
exit "$validation_status"
