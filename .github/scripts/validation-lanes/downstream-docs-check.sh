#!/usr/bin/env bash
set -euo pipefail

upstream_main_ref="refs/remotes/upstream/main"
mirror_state_json="$(
  python3 .github/scripts/sync_upstream_mirror.py \
    --repo "$PWD" \
    --mode read-only-fallback
)"
expected_mirror_sha="$(
  python3 -c 'import json, sys; print(json.load(sys.stdin)["expected_mirror_sha"])' <<< "${mirror_state_json}"
)"
mapfile -t mirror_audit_args < <(
  python3 -c 'import json, sys; [print(arg) for arg in json.load(sys.stdin)["mirror_audit_args"]]' \
    <<< "${mirror_state_json}"
)

git diff --check "${upstream_main_ref}...HEAD" -- \
  docs/downstream.md \
  docs/native-computer-use.md \
  docs/carry-divergence-ledger.md \
  docs/downstream-divergence-tracking.md \
  docs/downstream-regression-matrix.md \
  docs/downstream-tool-surface-matrix.md \
  docs/divergences/index.yaml

downstream_ref="$(git rev-parse HEAD)"

python3 scripts/downstream-divergence-audit.py \
  --repo "$PWD" \
  --downstream-ref "${downstream_ref}" \
  --upstream-remote upstream \
  --upstream-branch main \
  "${mirror_audit_args[@]}" \
  --expected-mirror-sha "${expected_mirror_sha}" \
  --registry-path docs/divergences/index.yaml \
  --output-dir target/downstream-divergence-audit \
  --format both \
  --code-only \
  --enforce-registry
