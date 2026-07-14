#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}" cargo nextest run --locked \
  -p codex-core \
  --test all \
  --no-tests=fail \
  -E 'test(/suite::inference_observations::/)'
