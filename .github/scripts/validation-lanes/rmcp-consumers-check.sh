#!/usr/bin/env bash
set -euo pipefail
cd codex-rs
cargo check --locked --all-targets \
  -p codex-app-server-protocol -p codex-app-server -p codex-cli \
  -p codex-mcp -p codex-core -p codex-mcp-server \
  -p mcp_test_support -p codex-rmcp-client -p codex-tools -p codex-tui
