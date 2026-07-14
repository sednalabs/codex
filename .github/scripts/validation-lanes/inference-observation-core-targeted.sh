#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

cargo nextest run --locked \
  -p codex-api \
  --no-tests=fail \
  -E 'test(/(responses|sse)/)'

cargo nextest run --locked \
  -p codex-core \
  --lib \
  --no-tests=fail \
  -E 'test(/(inference|response_stream|pending_setup|websocket_)/)'
