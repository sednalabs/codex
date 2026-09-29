#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

archive_file="${VALIDATION_LAB_NEXTEST_ARCHIVE_FILE:-${RUNNER_TEMP:-/tmp}/codex-core-carry-nextest.tar.zst}"
helper_stage_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/codex-core-carry-helpers.XXXXXX")"
trap 'rm -rf "${helper_stage_dir}"' EXIT
tests=(
  suite::subagent_notifications::spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role
  suite::subagent_notifications::spawn_agent_role_overrides_requested_model_and_reasoning_settings
  suite::code_mode::code_mode_exports_all_tools_metadata_for_builtin_tools
  suite::code_mode::code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools
  suite::unified_exec::exec_command_reports_chunk_and_exit_metadata
  suite::unified_exec::write_stdin_returns_exit_metadata_and_clears_session
)

if [[ -n "${VALIDATION_LAB_NEXTEST_ARCHIVE_FILE:-}" ]]; then
  if [[ ! -f "${archive_file}" ]]; then
    echo "validation-lab nextest archive not found: ${archive_file}" >&2
    exit 1
  fi
  echo "Using validation-lab nextest archive: ${VALIDATION_LAB_NEXTEST_ARCHIVE_ARTIFACT:-unknown}"
else
  mkdir -p "$(dirname "${archive_file}")"
  cargo nextest archive \
    -p codex-core \
    --test all \
    --archive-file "${archive_file}"
  du -h "${archive_file}"
fi

helper_archive_prefix="target/validation-lab/helpers"
tar --zstd -xf "${archive_file}" -C "${helper_stage_dir}" "${helper_archive_prefix}/manifest" \
  "${helper_archive_prefix}/codex-code-mode-host" \
  "${helper_archive_prefix}/test_stdio_server"
manifest="${helper_stage_dir}/${helper_archive_prefix}/manifest"
if [[ ! -f "${manifest}" ]]; then
  echo "nextest archive helper manifest is missing" >&2
  exit 1
fi

# GITHUB_SHA identifies the workflow host and can differ when this lane
# validates a checked-out cross-ref. Prefer explicit validation metadata, or
# derive the identity from the exact checkout that is running this wrapper.
expected_sha="${VALIDATION_LAB_EXPECTED_SHA:-$(git rev-parse HEAD)}"
archive_sha="$(sed -n 's/^validation_sha=//p' "${manifest}")"
if [[ -z "${expected_sha}" || -z "${archive_sha}" || "${expected_sha}" != "${archive_sha}" ]]; then
  echo "nextest archive validation identity is missing or disagrees (expected=${expected_sha:-missing} archive=${archive_sha:-missing})" >&2
  exit 1
fi
for helper in codex-code-mode-host test_stdio_server; do
  helper_path="${helper_stage_dir}/${helper_archive_prefix}/${helper}"
  expected_digest="$(sed -n "s/^${helper}=//p" "${manifest}")"
  if [[ ! -x "${helper_path}" || -z "${expected_digest}" ]]; then
    echo "nextest archive helper is missing or not executable: ${helper}" >&2
    exit 1
  fi
  actual_digest="$(sha256sum "${helper_path}" | awk '{print $1}')"
  if [[ "${actual_digest}" != "${expected_digest}" ]]; then
    echo "nextest archive helper identity disagrees: ${helper}" >&2
    exit 1
  fi
done

export CARGO_BIN_EXE_codex_code_mode_host="${helper_stage_dir}/${helper_archive_prefix}/codex-code-mode-host"
export CARGO_BIN_EXE_test_stdio_server="${helper_stage_dir}/${helper_archive_prefix}/test_stdio_server"

RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}" \
  CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-$(command -v node)}" \
  cargo nextest run \
    --archive-file "${archive_file}" \
    --workspace-remap "${PWD}" \
    --no-fail-fast \
    -- "${tests[@]}" --exact
