#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/dispatch-validation-lab-snapshot.sh [options]

Create a disposable commit from the current local worktree state, push it to a
remote validation ref, then optionally dispatch validation-lab from a chosen
workflow-host ref against that validated ref.

This is meant for dirty branches, orphan branches, and scratch work where the
exact local tree should be proven remotely without first reshaping local
history.

Options:
  --repo <owner/name>          GitHub repo (default: sednalabs/codex)
  --remote <name>              Git remote to push to (default: origin)
  --dispatch-ref <ref>         Workflow-host ref; must be the trusted main ref
  --base-ref <ref>              Exact comparison base for H/B/T evidence (default: main)
  --profile <name>             validation-lab profile (default: targeted)
  --lane-set <name>            validation-lab lane set (default: all except targeted)
  --lanes <csv>                Optional explicit lane IDs
  --path <path>                Include one explicit worktree path (repeatable)
  --include-all                Explicitly include all dirty paths (never implicit)
  --supersession-mode <mode>   auto|compare|milestone|retain (default: auto)
  --supersession-key <key>     Optional supersession key
  --artifact-build             Request artifact_build=true
  --ref-name <remote-ref>      Explicit remote ref name to create
  --message <text>             Snapshot commit message
  --push-only                  Push snapshot ref but do not dispatch validation-lab
  --dry-run                    Print planned push/dispatch commands without executing them
  -h, --help                   Show this help

Examples:
  scripts/dispatch-validation-lab-snapshot.sh \
    --profile targeted \
    --lanes codex.app-server-protocol-test,codex.app-server-thread-cwd-targeted

  scripts/dispatch-validation-lab-snapshot.sh \
    --profile frontier \
    --lane-set ui-protocol
EOF
}

repo="sednalabs/codex"
remote="origin"
dispatch_ref="main"
trusted_workflow_host_ref="main"
profile="targeted"
lane_set=""
lanes=""
base_ref="main"
paths=()
include_all="false"
supersession_mode="auto"
supersession_key=""
artifact_build="false"
ref_name=""
message=""
push_only="false"
dry_run="false"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      repo="$2"
      shift 2
      ;;
    --remote)
      remote="$2"
      shift 2
      ;;
    --dispatch-ref)
      dispatch_ref="$2"
      shift 2
      ;;
    --profile)
      profile="$2"
      shift 2
      ;;
    --lane-set)
      lane_set="$2"
      shift 2
      ;;
    --lanes)
      lanes="$2"
      shift 2
      ;;
    --base-ref)
      base_ref="$2"
      shift 2
      ;;
    --path)
      paths+=("$2")
      shift 2
      ;;
    --include-all)
      include_all="true"
      shift
      ;;
    --notes)
      shift 2
      ;;
    --supersession-mode)
      supersession_mode="$2"
      shift 2
      ;;
    --supersession-key)
      supersession_key="$2"
      shift 2
      ;;
    --artifact-build)
      artifact_build="true"
      shift
      ;;
    --ref-name)
      ref_name="$2"
      shift 2
      ;;
    --message)
      message="$2"
      shift 2
      ;;
    --push-only)
      push_only="true"
      shift
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

if [[ "$dispatch_ref" != "$trusted_workflow_host_ref" ]]; then
  echo "--dispatch-ref must be exactly ${trusted_workflow_host_ref}" >&2
  exit 2
fi

if [[ -z "$lane_set" && "$profile" != "targeted" ]]; then
  lane_set="all"
fi

if [[ "$push_only" != "true" && "$profile" == "targeted" && -z "$lanes" && ( -z "$lane_set" || "$lane_set" == "all" ) ]]; then
  echo "profile=targeted requires --lane-set <named-set> or --lanes <csv>; --lane-set all is not valid for targeted." >&2
  exit 2
fi

repo_root="$(git rev-parse --show-toplevel)"
git_dir="$(git rev-parse --git-dir)"
repo_prefix="$(git rev-parse --show-prefix)"

if [[ "${include_all}" == "true" && "${#paths[@]}" -gt 0 ]]; then
  echo "--include-all cannot be combined with --path" >&2
  exit 2
fi

if [[ "${#paths[@]}" -eq 0 && "${include_all}" != "true" ]]; then
  dirty_paths="$(git -C "${repo_root}" status --porcelain=v1 --untracked-files=all)"
  if [[ -n "${dirty_paths}" ]]; then
    cat >&2 <<'EOF'
refusing to sweep dirty paths implicitly; pass one or more --path values, or
use --include-all only when the complete dirty tree is explicitly in scope
EOF
    printf '%s\n' "${dirty_paths}" >&2
    exit 2
  fi
fi

resolved_paths=()
for path in "${paths[@]}"; do
  case "${path}" in
    ""|/*|.|..|../*|*/../*|*/..)
      echo "invalid repository-relative --path: ${path}" >&2
      exit 2
      ;;
  esac
  resolved_paths+=("${repo_prefix}${path}")
done
paths=("${resolved_paths[@]}")

current_branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
if [[ -z "$current_branch" ]]; then
  current_branch="detached"
fi
sanitized_branch="$(printf '%s' "$current_branch" | tr '/:[:space:]' '-' | tr -cd '[:alnum:]._-' | sed 's/^-*//; s/-*$//')"
if [[ -z "$sanitized_branch" ]]; then
  sanitized_branch="scratch"
fi

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
remote_ref="${ref_name:-validation/snapshot-${sanitized_branch}-${timestamp}}"
snapshot_message="${message:-validation snapshot from ${current_branch} at ${timestamp}}"

head_commit="$(git rev-parse --verify HEAD 2>/dev/null || true)"
tmp_index="$(mktemp "${TMPDIR:-/tmp}/codex-validation-index.XXXXXX")"

cleanup() {
  rm -f "$tmp_index"
}
trap cleanup EXIT

export GIT_INDEX_FILE="$tmp_index"
if [[ "${include_all}" == "true" ]]; then
  if [[ -f "${git_dir}/index" ]]; then
    cp "${git_dir}/index" "$tmp_index"
  else
    : > "$tmp_index"
  fi
  git -C "$repo_root" add -A
else
  if [[ -n "${head_commit}" ]]; then
    git -C "$repo_root" read-tree "${head_commit}"
  fi
  if [[ "${#paths[@]}" -gt 0 ]]; then
    git -C "$repo_root" add -- "${paths[@]}"
  fi
fi
tree_id="$(git -C "$repo_root" write-tree)"

if [[ -n "$head_commit" ]]; then
  snapshot_commit="$(printf '%s\n' "$snapshot_message" | git -C "$repo_root" commit-tree "$tree_id" -p "$head_commit")"
else
  snapshot_commit="$(printf '%s\n' "$snapshot_message" | git -C "$repo_root" commit-tree "$tree_id")"
fi

push_cmd=(git -C "$repo_root" push "$remote" "${snapshot_commit}:refs/heads/${remote_ref}")

dispatch_cmd=(
  gh workflow run validation-lab.yml
  --repo "$repo"
  --ref "$dispatch_ref"
  -f "ref=${remote_ref}"
  -f "target_sha=${snapshot_commit}"
  -f "base_ref=${base_ref}"
  -f "profile=${profile}"
  -f "lane_set=${lane_set}"
  -f "artifact_build=${artifact_build}"
  -f "supersession_mode=${supersession_mode}"
)

if [[ -n "$lanes" ]]; then
  dispatch_cmd+=(-f "lanes=${lanes}")
fi
if [[ -n "$supersession_key" ]]; then
  dispatch_cmd+=(-f "supersession_key=${supersession_key}")
fi

echo "Snapshot commit: ${snapshot_commit}"
echo "Validated ref: ${remote_ref}"
echo "Workflow host ref: ${dispatch_ref}"
printf 'Push command:'
printf ' %q' "${push_cmd[@]}"
printf '\n'

if [[ "$push_only" != "true" ]]; then
  printf 'Dispatch command:'
  printf ' %q' "${dispatch_cmd[@]}"
  printf '\n'
fi

if [[ "$dry_run" == "true" ]]; then
  exit 0
fi

"${push_cmd[@]}"

if [[ "$push_only" == "true" ]]; then
  echo "Pushed snapshot ref only."
  echo "Delete it later with:"
  echo "  git push ${remote} :refs/heads/${remote_ref}"
  exit 0
fi

"${dispatch_cmd[@]}"

echo "Dispatched validation-lab from ${dispatch_ref} against ${remote_ref}."
echo "Cleanup when finished:"
echo "  git push ${remote} :refs/heads/${remote_ref}"
