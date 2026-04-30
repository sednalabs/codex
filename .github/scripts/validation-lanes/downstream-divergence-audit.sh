#!/usr/bin/env bash
set -euo pipefail

bash .github/scripts/validation-lanes/downstream-docs-check.sh

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
