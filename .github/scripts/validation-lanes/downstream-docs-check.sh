#!/usr/bin/env bash
set -euo pipefail

git diff --check -- \
  docs/downstream.md \
  docs/native-computer-use.md \
  docs/carry-divergence-ledger.md \
  docs/downstream-divergence-tracking.md \
  docs/downstream-regression-matrix.md \
  docs/downstream-tool-surface-matrix.md \
  docs/divergences/index.yaml

python3 -m json.tool docs/divergences/index.yaml >/dev/null

python3 .github/scripts/check_markdown_links.py
