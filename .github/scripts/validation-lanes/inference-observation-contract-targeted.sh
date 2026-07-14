#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

cargo nextest run --locked \
  -p codex-protocol \
  --no-tests=fail \
  -E 'test(/inference_call/)'

cargo nextest run --locked \
  -p codex-rollout-trace \
  --no-tests=fail \
  -E 'test(/inference/)'
