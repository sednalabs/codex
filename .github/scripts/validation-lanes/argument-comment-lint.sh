#!/usr/bin/env bash
set -euo pipefail

commit_sha="$(git rev-parse HEAD 2>/dev/null || printf '%s' "${GITHUB_SHA:-unknown}")"
bazel_targets=()
while IFS= read -r bazel_target; do
  [[ -n "${bazel_target}" ]] || continue
  bazel_targets+=("${bazel_target}")
done < <(./tools/argument-comment-lint/list-bazel-targets.sh)

./.github/scripts/run-bazel-ci.sh \
  -- \
  build \
  --config=argument-comment-lint \
  --keep_going \
  "--build_metadata=COMMIT_SHA=${commit_sha}" \
  -- \
  "${bazel_targets[@]}"
