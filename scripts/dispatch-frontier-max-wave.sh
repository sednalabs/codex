#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/dispatch-frontier-max-wave.sh [options]

Dispatch the widest current remote validation wave for a validated ref.

For host-workflow-driven jobs such as `validation-lab` and `sedna-branch-build`,
the workflow logic is loaded from `--host-ref` while the validated code comes
from `--ref`.

This is intended for "fan it out" validation where we want:
- rust-ci
- rust-ci-full
- sedna-heavy-tests
- sedna-branch-build
- validation-lab full/all checkpoint
- validation-lab frontier/all
- validation-lab frontier family slices

Options:
  --ref <remote-ref>           Branch/tag/commit ref to validate (required)
  --repo <owner/name>          GitHub repo (default: sednalabs/codex)
  --host-ref <ref>             Workflow host ref for validation-lab and branch build (default: main)
  --platform-scope <scope>     rust-ci platform scope (default: full-cross-platform)
  --heavy-lane <lane>          sedna-heavy-tests lane (default: all)
  --include-explicit-lanes     Include explicit-only validation-lab seams (default: true)
  --dry-run                    Print commands without executing them
  -h, --help                   Show this help

Example:
  scripts/dispatch-frontier-max-wave.sh \
    --ref validation/frontier-max-app-server-bundles-20260411T212333Z
EOF
}

ref=""
repo="sednalabs/codex"
host_ref="main"
platform_scope="full-cross-platform"
heavy_lane="all"
include_explicit_lanes="true"
dry_run="false"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ref)
      ref="$2"
      shift 2
      ;;
    --repo)
      repo="$2"
      shift 2
      ;;
    --host-ref)
      host_ref="$2"
      shift 2
      ;;
    --platform-scope)
      platform_scope="$2"
      shift 2
      ;;
    --heavy-lane)
      heavy_lane="$2"
      shift 2
      ;;
    --include-explicit-lanes)
      include_explicit_lanes="${2:-true}"
      shift 2
      ;;
    --dry-run)
      dry_run="true"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -z "$ref" ]]; then
  echo "--ref is required" >&2
  usage >&2
  exit 1
fi

dispatches=(
  "gh workflow run rust-ci.yml --repo ${repo} --ref ${ref} -f platform_scope=${platform_scope}"
  "gh workflow run rust-ci-full.yml --repo ${repo} --ref ${ref} -f platform_scope=${platform_scope}"
  "gh workflow run sedna-branch-build.yml --repo ${repo} --ref ${host_ref} -f ref=${ref}"
  "gh workflow run sedna-heavy-tests.yml --repo ${repo} --ref ${ref} -f ref=${ref} -f lane=${heavy_lane}"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=full -f lane_set=all -f artifact_build=true -f supersession_mode=compare"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=broad -f lane_set=all -f artifact_build=false -f include_explicit_lanes=${include_explicit_lanes} -f supersession_mode=compare"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=frontier -f lane_set=all -f artifact_build=false -f include_explicit_lanes=${include_explicit_lanes} -f supersession_mode=compare"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=frontier -f lane_set=core-carry -f artifact_build=false -f include_explicit_lanes=${include_explicit_lanes} -f supersession_mode=compare"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=frontier -f lane_set=startup-sync -f artifact_build=false -f include_explicit_lanes=${include_explicit_lanes} -f supersession_mode=compare"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=frontier -f lane_set=subagents -f artifact_build=false -f include_explicit_lanes=${include_explicit_lanes} -f supersession_mode=compare"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=frontier -f lane_set=ui-protocol -f artifact_build=false -f include_explicit_lanes=${include_explicit_lanes} -f supersession_mode=compare"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=frontier -f lane_set=ledger -f artifact_build=false -f include_explicit_lanes=${include_explicit_lanes} -f supersession_mode=compare"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=frontier -f lane_set=attestation -f artifact_build=false -f include_explicit_lanes=${include_explicit_lanes} -f supersession_mode=compare"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=frontier -f lane_set=docs -f artifact_build=false -f include_explicit_lanes=${include_explicit_lanes} -f supersession_mode=compare"
  "gh workflow run validation-lab.yml --repo ${repo} --ref ${host_ref} -f ref=${ref} -f profile=frontier -f lane_set=release -f artifact_build=false -f include_explicit_lanes=${include_explicit_lanes} -f supersession_mode=compare"
)

printf 'Validated ref: %s\n' "$ref"
printf 'Workflow host ref: %s\n' "$host_ref"
printf 'Dispatch count: %s\n' "${#dispatches[@]}"

for dispatch in "${dispatches[@]}"; do
  printf 'Command: %s\n' "$dispatch"
  if [[ "$dry_run" == "true" ]]; then
    continue
  fi
  eval "$dispatch"
done

if [[ "$dry_run" != "true" ]]; then
  echo "Dispatch wave submitted."
fi
