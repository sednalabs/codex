#!/usr/bin/env bash
set -euo pipefail

just write-app-server-schema
test -f codex-rs/app-server-protocol/schema/precomputed/app-server-exports-stable.json.zst
