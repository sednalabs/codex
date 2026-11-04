#!/usr/bin/env bash
set -euo pipefail
cd codex-rs
# The remote HTTP fixture launches codex exec-server from the shared target directory.
cargo build --locked -p codex-cli --bin codex
cargo test --locked -p codex-rmcp-client
