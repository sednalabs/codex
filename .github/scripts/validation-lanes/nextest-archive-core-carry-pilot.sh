#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

archive_file="${RUNNER_TEMP:-/tmp}/codex-core-carry-nextest.tar.zst"
tests=(
  suite::subagent_notifications::spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role
  suite::subagent_notifications::spawn_agent_role_overrides_requested_model_and_reasoning_settings
  suite::code_mode::code_mode_exports_all_tools_metadata_for_builtin_tools
  suite::code_mode::code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools
  suite::unified_exec::exec_command_reports_chunk_and_exit_metadata
  suite::unified_exec::write_stdin_returns_exit_metadata_and_clears_session
)

cargo nextest archive \
  -p codex-core \
  --test all \
  --archive-file "${archive_file}"

du -h "${archive_file}"

CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-$(command -v node)}" \
  cargo nextest run \
    --archive-file "${archive_file}" \
    --workspace-remap "${PWD}" \
    --no-fail-fast \
    -- "${tests[@]}" --exact
