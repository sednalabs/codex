#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

archive_file="${VALIDATION_LAB_NEXTEST_ARCHIVE_FILE:-${RUNNER_TEMP:-/tmp}/codex-core-carry-nextest.tar.zst}"
mkdir -p "$(dirname "${archive_file}")"

cargo nextest archive \
  -p codex-core \
  --test all \
  --archive-file "${archive_file}"

du -h "${archive_file}"
