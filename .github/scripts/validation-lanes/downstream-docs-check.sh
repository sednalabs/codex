#!/usr/bin/env bash
set -euo pipefail

if git remote get-url upstream >/dev/null 2>&1; then
  git remote set-url upstream https://github.com/openai/codex.git
else
  git remote add upstream https://github.com/openai/codex.git
fi

git fetch --no-tags --prune origin upstream-main
git fetch --no-tags --prune upstream main

upstream_main_ref="refs/remotes/upstream/main"
origin_mirror_ref="refs/remotes/origin/upstream-main"
mirror_audit_args=(
  --mirror-remote origin
  --mirror-branch upstream-main
)

sync_origin_mirror() {
  if [ -n "${SYNC_UPSTREAM_PUSH_TOKEN:-}" ]; then
    git remote set-url origin "https://x-access-token:${SYNC_UPSTREAM_PUSH_TOKEN}@github.com/${GITHUB_REPOSITORY:-sednalabs/codex}.git"
  elif [ "${GITHUB_ACTIONS:-}" = "true" ]; then
    echo "origin/upstream-main is stale and this GitHub Actions lane has no SEDNA_SYNC_UPSTREAM_PUSH_TOKEN secret for mirror writes; auditing against read-only upstream/main" >&2
    mirror_audit_args=(--mirror-ref "${upstream_main_ref}")
    return 0
  fi

  git push origin "${upstream_main_ref}:refs/heads/upstream-main"
  git fetch --no-tags --prune origin upstream-main
}

if git show-ref --verify --quiet "${origin_mirror_ref}"; then
  if ! git merge-base --is-ancestor "${origin_mirror_ref}" "${upstream_main_ref}"; then
    echo "origin/upstream-main is not an ancestor of upstream/main; refusing non-fast-forward mirror sync" >&2
    exit 1
  fi

  if [ "$(git rev-parse "${origin_mirror_ref}")" != "$(git rev-parse "${upstream_main_ref}")" ]; then
    sync_origin_mirror
  fi
else
  sync_origin_mirror
fi

git diff --check "${upstream_main_ref}...HEAD" -- \
  docs/downstream.md \
  docs/carry-divergence-ledger.md \
  docs/downstream-divergence-tracking.md \
  docs/downstream-regression-matrix.md \
  docs/downstream-tool-surface-matrix.md \
  docs/divergences/index.yaml

expected_mirror_sha="$(git rev-parse "${upstream_main_ref}")"
downstream_ref="$(git rev-parse HEAD)"

python3 scripts/downstream-divergence-audit.py \
  --repo "$PWD" \
  --downstream-ref "${downstream_ref}" \
  "${mirror_audit_args[@]}" \
  --upstream-remote upstream \
  --upstream-branch main \
  --expected-mirror-sha "${expected_mirror_sha}" \
  --registry-path docs/divergences/index.yaml \
  --output-dir target/downstream-divergence-audit \
  --format both \
  --code-only \
  --enforce-registry
