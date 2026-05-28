#!/usr/bin/env bash
set -euo pipefail

corepack enable
pnpm install --frozen-lockfile
pnpm -r --filter ./sdk/typescript run build
pnpm -r --filter ./sdk/typescript run lint
pnpm --filter ./sdk/typescript exec jest tests/exec.test.ts
