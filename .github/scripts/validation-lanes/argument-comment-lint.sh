#!/usr/bin/env bash
set -euo pipefail

run_lint() {
  local log_path="$1"
  ./tools/argument-comment-lint/run-prebuilt-linter.py -- --all-targets --ignore-rust-version 2>&1 | tee "${log_path}"
}

is_transient_cargo_fetch_failure() {
  local log_path="$1"
  grep -Eiq \
    'cargo metadata.*exited with an error|failed to get .* as a dependency|unable to update registry|download of .* failed|curl failed' \
    "${log_path}"
}

first_log="$(mktemp)"
if run_lint "${first_log}"; then
  exit 0
fi

if ! is_transient_cargo_fetch_failure "${first_log}"; then
  exit 1
fi

echo "argument-comment-lint hit a transient Cargo fetch/metadata failure; retrying once" >&2
retry_log="$(mktemp)"
run_lint "${retry_log}"
