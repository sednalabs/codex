#!/usr/bin/env bash
set -euo pipefail

cd codex-rs
exec cargo test --locked -p codex-app-server-protocol
