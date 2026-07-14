#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

cargo nextest run --locked \
  -p codex-core \
  --test all \
  --no-tests=fail \
  -E 'test(/suite::inference_observations::/)'
