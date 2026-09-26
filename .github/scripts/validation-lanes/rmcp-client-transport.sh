#!/usr/bin/env bash
set -euo pipefail
cd codex-rs
cargo test --locked -p codex-rmcp-client
