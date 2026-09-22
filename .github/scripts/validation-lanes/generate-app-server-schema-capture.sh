#!/usr/bin/env bash
set -euo pipefail

set -euo pipefail

workspace_root="${GITHUB_WORKSPACE:-$(pwd)}"
cd "${workspace_root}/codex-rs"
CODEX_APP_SERVER_SCHEMA_ROOT="${workspace_root}/codex-rs/app-server-protocol/schema" \
CODEX_APP_SERVER_SCHEMA_EXPERIMENTAL=0 \
  cargo test -p codex-app-server-protocol --lib \
    schema_fixtures_tests::write_schema_fixtures_from_env -- --exact --ignored
test -f "${workspace_root}/codex-rs/app-server-protocol/schema/precomputed/app-server-exports-stable.json.zst"
