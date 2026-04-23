#!/usr/bin/env bash
set -euo pipefail

exec git diff --check -- docs/downstream.md docs/carry-divergence-ledger.md docs/downstream-regression-matrix.md docs/downstream-tool-surface-matrix.md docs/divergences/index.yaml
