set working-directory := "codex-rs"
set positional-arguments

# Display help
help:
    just -l

# `codex`
alias c := codex
codex *args:
    cargo run --bin codex -- "$@"

# `codex exec`
exec *args:
    cargo run --bin codex -- exec "$@"

# Run the CLI version of the file-search crate.
file-search *args:
    cargo run --bin codex-file-search -- "$@"

# Build the CLI and run the app-server test client
app-server-test-client *args:
    cargo build -p codex-cli
    cargo run -p codex-app-server-test-client -- --codex-bin ./target/debug/codex "$@"

# format code
fmt:
    cargo fmt -- --config imports_granularity=Item 2>/dev/null

fix *args:
    cargo clippy --fix --tests --allow-dirty "$@"

clippy *args:
    cargo clippy --tests "$@"

install:
    rustup show active-toolchain
    cargo fetch

# Run `cargo nextest` since it's faster than `cargo test`, though including
# --no-fail-fast is important to ensure all tests are run.
#
# Run `cargo install cargo-nextest` if you don't have it installed.
# Prefer this for routine local runs; use explicit `cargo test --all-features`
# only when you specifically need full feature coverage.
test:
    cargo nextest run --no-fail-fast

# Compile-focused guardrail for high-churn core + sandbox seams.
core-compile-smoke:
    cargo check -p codex-linux-sandbox -p codex-core --tests

# Carry-only downstream behavior smoke checks.
core-carry-smoke:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core spawn_agent_preserves_explicit_model_override_across_role_reload --lib -- --exact
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::subagent_notifications::spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::subagent_notifications::spawn_agent_role_overrides_requested_model_and_reasoning_settings -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::code_mode::code_mode_exports_all_tools_metadata_for_builtin_tools -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::code_mode::code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::unified_exec::exec_command_wait_until_terminal_returns_exit_metadata -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-tui queued_inline_slash_command_runs_with_args_after_task_complete -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-tui alt_up_restores_most_recent_queued_slash_command -- --exact --test-threads=1
    cargo test -p codex-tui-app-server app::tests::replayed_turn_complete_submits_restored_queued_follow_up -- --exact --test-threads=1
    cargo test -p codex-tui-app-server app::tests::replayed_turn_complete_opens_restored_queued_approvals_command -- --exact --test-threads=1
    cargo test -p codex-tui-app-server app::tests::replayed_turn_complete_keeps_message_after_restored_queued_command_runs_first -- --exact --test-threads=1
    cargo test -p codex-tui-app-server app::tests::sync_active_agent_label_updates_footer_for_selected_thread -- --exact --test-threads=1

# Focused startup sync regression slice for bounded-wait and abort/re-arm behavior.
core-startup-sync-targeted:
    cargo test -p codex-core --lib startup_remote_plugin_sync_ -- --test-threads=1

# Focused downstream sub-agent surface contract slice.
core-subagent-surface-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core spawn_agent_preserves_explicit_model_override_across_role_reload --lib -- --exact
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core list_agents_returns_direct_children_with_live_inventory --lib -- --exact
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core list_agents_include_descendants_hydrates_live_nested_descendant_inventory --lib -- --exact
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core list_agents_include_descendants_reports_persisted_open_and_closed_descendants --lib -- --exact

# Focused core-side sub-agent notification contract slice.
core-subagent-notification-contract-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core format_subagent_notification_message_round_trips_completed_status --lib -- --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core classifies_memory_excluded_fragments --lib -- --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core drop_last_n_user_turns_ignores_session_prefix_user_messages --lib -- --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core serializes_memory_rollout_with_agents_removed_but_environment_kept --lib -- --test-threads=1

# Focused sub-agent completion-notification parser + TUI render slice.
# Keep `tui_app_server` at compile coverage for now while its broader lib test
# target still has unrelated drift on this branch.
core-subagent-notification-visibility-targeted:
    cargo test -p codex-protocol parse_subagent_notification_response_item_ --lib -- --test-threads=1
    cargo test -p codex-tui raw_response_subagent_notification_renders_history -- --exact --test-threads=1
    cargo build -p codex-tui-app-server

# Focused multi-agent orchestration slice covering wait semantics and tool guidance.
core-multi-agent-orchestration-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core wait_agent_allows_return_when_ --lib -- --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::spawn_agent_description::spawn_wait_and_list_agents_tool_descriptions_have_guidance_updates -- --exact --test-threads=1

# Focused persisted-descendant inventory slice for subtree close/resume behavior.
core-persisted-subagent-descendants-targeted:
    cargo test -p codex-core persisted_spawn_descendants_reflect_closed_status --lib -- --test-threads=1

# Focused tool-context serialization slice for custom/function/abort outputs.
core-context-serialization-targeted:
    cargo test -p codex-core tools::context::tests::custom_tool_calls_should_roundtrip_as_custom_outputs --lib -- --exact
    cargo test -p codex-core tools::context::tests::function_payloads_remain_function_outputs --lib -- --exact
    cargo test -p codex-core tools::context::tests::aborted_tool_output_serializes_ --lib -- --test-threads=1

# Codex authoritative usage.sqlite logging contracts.
core-ledger-smoke:
    cargo test -p codex-state runtime::tests::init_removes_legacy_logs_and_usage_db_files -- --exact --test-threads=1
    cargo test -p codex-state runtime::usage::tests::usage_logger_records_requested_model_and_quota_snapshot -- --exact --test-threads=1
    cargo test -p codex-state runtime::usage::tests::usage_logger_tracks_tool_call_lifecycle -- --exact --test-threads=1
    cargo test -p codex-state runtime::usage::tests::usage_logger_captures_spawn_request_and_fork_snapshot -- --exact --test-threads=1
    cargo test -p codex-state runtime::usage::tests::usage_logger_resolves_root_thread_from_parent_or_fork -- --exact --test-threads=1
    cargo test -p codex-state runtime::usage::tests::usage_logger_clears_turn_snapshot_after_turn_complete -- --exact --test-threads=1
    cargo test -p codex-state runtime::usage::tests::usage_logger_resolves_root_thread_from_persisted_lineage_after_restart -- --exact --test-threads=1

# Focused persisted-state/usage lineage contract slice for subagent graph adoption.
core-state-spawn-lineage-contract-targeted:
    cargo test -p codex-state usage_spawn_lineage_matches_persisted_state_edge_for_child_thread -- --test-threads=1

# Cross-repo ledger seam validation (agent-usage-ledger + Postgres).
[no-cd]
downstream-ledger-seam:
    #!/usr/bin/env bash
    set -euo pipefail
    ledger_repo_root="${LEDGER_REPO_ROOT:-../agent-usage-ledger}"
    if [[ ! -d "${ledger_repo_root}" ]]; then
        echo "Skipping downstream-ledger-seam: missing ledger repo at ${ledger_repo_root}"
        exit 0
    fi
    if ! command -v psql >/dev/null 2>&1; then
        echo "Skipping downstream-ledger-seam: missing psql"
        exit 0
    fi
    "${ledger_repo_root}/scripts/llm_usage/ensure_schema.sh" --schema "${LLM_USAGE_DB_SCHEMA:-llm_usage}"
    "${ledger_repo_root}/scripts/llm_usage/ingest_codex_rollouts_to_postgres.sh" --schema "${LLM_USAGE_DB_SCHEMA:-llm_usage}" --skip-schema
    "${ledger_repo_root}/scripts/llm_usage/test_codex_copied_history_filter.sh"
    "${ledger_repo_root}/scripts/llm_usage/test_codex_source_row_identity.sh"

[no-cd]
downstream-docs-check:
    git diff --check -- docs/downstream.md docs/carry-divergence-ledger.md docs/downstream-regression-matrix.md docs/downstream-tool-surface-matrix.md

# Fast smoke checks for fragile codex-core integration buckets.
core-test-smoke:
    just core-compile-smoke
    just core-carry-smoke
    just core-ledger-smoke
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::rmcp_client::stdio_server_round_trip -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::code_mode::code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::plugins::plugin_mcp_tools_are_listed -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::truncation::mcp_tool_call_output_exceeds_limit_truncated_for_model -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::client::usage_limit_error_emits_rate_limit_event -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::client_websockets::responses_websocket_usage_limit_error_emits_rate_limit_event -- --exact --test-threads=1

# Progressive codex-core ladder:
# 1) smoke gate, 2) high-churn buckets, 3) full suite.
core-test-progressive:
    just core-test-smoke
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::rmcp_client:: -- --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::code_mode:: -- --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::truncation:: -- --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::plugins:: -- --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" CARGO_BUILD_JOBS=1 cargo test -p codex-core -j1 -- --test-threads=1

# Build and run Codex from source using Bazel.
# Note we have to use the combination of `[no-cd]` and `--run_under="cd $PWD &&"`
# to ensure that Bazel runs the command in the current working directory.
[no-cd]
bazel-codex *args:
    bazel run //codex-rs/cli:codex --run_under="cd $PWD &&" -- "$@"

[no-cd]
bazel-lock-update:
    bazel mod deps --lockfile_mode=update

[no-cd]
bazel-lock-check:
    ./scripts/check-module-bazel-lock.sh

bazel-test:
    bazel test //... --keep_going

bazel-remote-test:
    bazel test //... --config=remote --platforms=//:rbe --keep_going

build-for-release:
    bazel build //codex-rs/cli:release_binaries --config=remote

# Run the MCP server
mcp-server-run *args:
    cargo run -p codex-mcp-server -- "$@"

# Regenerate the json schema for config.toml from the current config types.
write-config-schema:
    cargo run -p codex-core --bin codex-write-config-schema

# Regenerate vendored app-server protocol schema artifacts.
write-app-server-schema *args:
    cargo run -p codex-app-server-protocol --bin write_schema_fixtures -- "$@"

[no-cd]
write-hooks-schema:
    cargo run --manifest-path ./codex-rs/Cargo.toml -p codex-hooks --bin write_hooks_schema_fixtures

# Run the argument-comment Dylint checks across codex-rs.
[no-cd]
argument-comment-lint *args:
    ./tools/argument-comment-lint/run-prebuilt-linter.sh "$@"

[no-cd]
argument-comment-lint-from-source *args:
    ./tools/argument-comment-lint/run.sh "$@"

# Tail logs from the state SQLite database
log *args:
    if [ "${1:-}" = "--" ]; then shift; fi; cargo run -p codex-state --bin logs_client -- "$@"
