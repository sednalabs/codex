#!/usr/bin/env bash
set -euo pipefail

cd codex-rs
CODEX_RELEASE_VERSION="${CODEX_RELEASE_VERSION:-0.0.0-sedna.smoke}" exec cargo build --locked --target x86_64-unknown-linux-gnu --release --bin codex --bin codex-responses-api-proxy
