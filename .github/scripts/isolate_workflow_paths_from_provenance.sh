#!/usr/bin/env bash
set -euo pipefail

repo_root="${1:?repository root is required}"
exclude_file="$(git -C "${repo_root}" rev-parse --git-path info/exclude)"
if [[ "${exclude_file}" != /* ]]; then
  exclude_file="${repo_root}/${exclude_file}"
fi

{
  echo "/.workflow-src/"
  echo "/.sccache/"
} >> "${exclude_file}"

source_status="$(git -C "${repo_root}" status --porcelain --untracked-files=normal)"
if [[ -n "${source_status}" ]]; then
  echo "Workflow setup left unexpected source-tree changes." >&2
  printf '%s\n' "${source_status}" >&2
  exit 1
fi
