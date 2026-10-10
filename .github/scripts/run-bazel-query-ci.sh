#!/usr/bin/env bash

set -euo pipefail

# Run target-discovery queries with the same startup settings as the main
# build/test invocation so they can reuse the same Bazel server. Queries only
# enumerate labels, so they intentionally do not select a CI build/test config
# or remote execution.

if [[ $# -lt 2 || "${@: -2:1}" != "--" ]]; then
  echo "Usage: $0 [<bazel query args>...] -- <query expression>" >&2
  exit 1
fi

query_args=("${@:1:$#-2}")
query_expression="${@: -1}"

run_bazel() {
  if [[ "${RUNNER_OS:-}" == "Windows" ]]; then
    MSYS2_ARG_CONV_EXCL='*' "$(dirname "${BASH_SOURCE[0]}")/run_bazel_with_buildbuddy.py" "$@"
    return
  fi

  "$(dirname "${BASH_SOURCE[0]}")/run_bazel_with_buildbuddy.py" "$@"
}

bazel_query_args=(query)

if [[ "${RUNNER_OS:-}" == "Windows" && "${CODEX_BAZEL_WINDOWS_VOICE_TOOLS:-0}" == "1" ]]; then
  if [[ -z "${VOICE_WINDOWS_BAZEL_REPOSITORY:-}" ]]; then
    echo "Opted-in native Windows CI requires its verified tool repository." >&2
    exit 1
  fi
  voice_tools_root="$(cygpath -u "$VOICE_WINDOWS_BAZEL_REPOSITORY")"
  if [[ ! -f "$voice_tools_root/voice-tools.json" ]]; then
    echo "Verified Windows voice tool manifest is missing." >&2
    exit 1
  fi
  # Query accepts repository injection, not build-only environment or settings.
  bazel_query_args+=("--inject_repository=voice_windows_tools=${VOICE_WINDOWS_BAZEL_REPOSITORY}")
fi

if [[ -n "${BAZEL_REPO_CONTENTS_CACHE:-}" ]]; then
  bazel_query_args+=("--repo_contents_cache=${BAZEL_REPO_CONTENTS_CACHE}")
fi

if [[ -n "${BAZEL_REPOSITORY_CACHE:-}" ]]; then
  bazel_query_args+=("--repository_cache=${BAZEL_REPOSITORY_CACHE}")
fi

if (( ${#query_args[@]} > 0 )); then
  bazel_query_args+=("${query_args[@]}")
fi
bazel_query_args+=("$query_expression")

run_bazel "${bazel_query_args[@]}"
