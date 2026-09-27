#!/usr/bin/env bash
set -euo pipefail

corepack enable
pnpm install --frozen-lockfile
pnpm --filter @openai/codex-shell-tool-mcp run format
pnpm --filter @openai/codex-shell-tool-mcp test
pnpm --filter @openai/codex-shell-tool-mcp run build
