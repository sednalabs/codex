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

# Start `codex exec-server` and run codex-tui.
[no-cd]
tui-with-exec-server *args:
    {{ justfile_directory() }}/scripts/run_tui_with_exec_server.sh "$@"

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

core-websocket-targeted:
    set -euo pipefail; \
    export CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}"; \
    cargo test -p codex-core --test all suite::agent_websocket -- --exact --test-threads=1; \
    cargo test -p codex-core --test all suite::client_websockets -- --exact --test-threads=1; \
    cargo test -p codex-core --test all suite::websocket_fallback -- --exact --test-threads=1; \
    cargo test -p codex-core --test all suite::turn_state::websocket_turn_state_persists_within_turn_and_resets_after -- --exact --test-threads=1

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
# Prefer this for routine local runs. Workspace crate features are banned, so
# there should be no need to add `--all-features`.
test:
    cargo nextest run --no-fail-fast

# Compile-focused guardrail for high-churn core + sandbox seams.
core-compile-smoke:
    cargo check -p codex-linux-sandbox -p codex-core --tests

# Carry-only downstream behavior smoke checks (core-only seam).
core-carry-core-smoke:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --test all -- suite::subagent_notifications::spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role suite::subagent_notifications::spawn_agent_role_overrides_requested_model_and_reasoning_settings suite::code_mode::code_mode_exports_all_tools_metadata_for_builtin_tools suite::code_mode::code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools suite::unified_exec::exec_command_reports_chunk_and_exit_metadata suite::unified_exec::write_stdin_returns_exit_metadata_and_clears_session --exact
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core multi_agent_v2_wait_agent_honors_return_when_all --lib -- --exact --test-threads=1

# Carry-only downstream behavior smoke checks (TUI/UI seam).
core-carry-ui-smoke:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-tui --no-fail-fast -- chatwidget::tests::slash_approvals_enter_queues_while_task_running_and_replays_on_completion chatwidget::tests::alt_up_restores_most_recent_queued_slash_command app::tests::replayed_turn_complete_submits_restored_queued_follow_up app::agent_navigation::tests::active_agent_label_tracks_current_thread --exact

# Compatibility wrapper while callers migrate to split core/UI smoke lanes.
core-carry-smoke:
    just core-carry-core-smoke
    just core-carry-ui-smoke

# Focused startup sync regression slice for bounded-wait and abort/re-arm behavior.
core-startup-sync-targeted:
    cargo test -p codex-core --lib startup_remote_plugin_sync_ -- --test-threads=1

# Focused downstream sub-agent surface contract slice.
core-subagent-surface-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --lib -- multi_agent_v2_list_agents_returns_completed_status_and_last_task_message multi_agent_v2_list_agents_keeps_active_descendant_hint_under_path_filter multi_agent_v2_list_agents_flags_active_descendants test_build_specs_multi_agent_v2_uses_task_names_and_hides_resume test_gpt_5_defaults test_gpt_5_1_defaults test_codex_5_1_mini_defaults test_gpt_5_1_codex_max_unified_exec_web_search test_full_toolset_specs_for_gpt5_codex_unified_exec_web_search test_gpt_5_1_codex_max_defaults

# Focused core-side sub-agent notification contract slice.
core-subagent-notification-contract-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --lib -- format_subagent_notification_message_round_trips_completed_status classifies_memory_excluded_fragments drop_last_n_user_turns_ignores_session_prefix_user_messages serializes_memory_rollout_with_agents_removed_but_environment_kept

# Focused sub-agent completion-notification parser + TUI render slice after the
# tui_app_server -> tui cutover.
core-subagent-notification-visibility-targeted:
    cargo test -p codex-protocol parse_subagent_notification_response_item_ --lib -- --test-threads=1
    cargo test -p codex-tui raw_response_subagent_notification_renders_history -- --exact --test-threads=1

# Focused TUI thread-session approval persistence slice.
tui-thread-session-policy-targeted:
    cargo test -p codex-tui app::tests::store_active_thread_receiver_persists_per_thread_policy_overrides --lib -- --exact --test-threads=1

# Focused native dynamic-tool registration slice across protocol, TUI, and
# app-server resume/fork paths.
dynamic-tool-registration-targeted:
    cargo test --locked -p codex-app-server-protocol client_request_thread_resume_dynamic_tools_is_marked_experimental -- --exact --test-threads=1
    cargo test --locked -p codex-app-server-protocol client_request_thread_fork_dynamic_tools_is_marked_experimental -- --exact --test-threads=1
    cargo test --locked -p codex-tui thread_lifecycle_params_omit_local_overrides_for_remote_sessions --lib -- --exact --test-threads=1
    cargo test --locked -p codex-app-server --test all suite::v2::dynamic_tools::thread_start_injects_dynamic_tools_into_model_requests -- --exact --test-threads=1
    cargo test --locked -p codex-app-server --test all suite::v2::dynamic_tools::thread_resume_injects_dynamic_tools_into_model_requests -- --exact --test-threads=1
    cargo test --locked -p codex-app-server --test all suite::v2::dynamic_tools::thread_fork_injects_dynamic_tools_into_model_requests -- --exact --test-threads=1

# Focused TUI config-refresh session-state persistence slice.
tui-config-refresh-session-targeted:
    cargo test -p codex-tui app::tests::refresh_in_memory_config_from_disk_preserves_active_thread_session_state --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::fresh_session_config_uses_current_session_state --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::fresh_session_config_preserves_policy_mutability --lib -- --exact --test-threads=1

# Focused /agent picker usage and remaining-context visibility slice.
tui-agent-picker-targeted:
    cargo test -p codex-tui app::tests::open_agent_picker_marks_loaded_threads_open --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::inactive_thread_started_notification_initializes_replay_session --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_falls_back_to_thread_id_without_usage --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_compact_token_usage_when_present --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_remaining_context_when_known --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_compact_age_when_known --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_model_effort_and_task_when_available --lib -- --exact --test-threads=1

# Focused shared picker-model tool-description slice for upgradeable legacy
# visibility without widening to the TUI/app-server build graph.
spawn-agent-tool-model-surface-targeted:
    cargo test -p codex-tools spawn_agent_tool_v2_requires_task_name_and_lists_visible_models --lib -- --exact --test-threads=1
    cargo test -p codex-tools spawn_agent_tool_v2_lists_upgradeable_legacy_models --lib -- --exact --test-threads=1

# Focused shared picker-model spawned-agent-description slice for upgradeable
# legacy visibility without widening to the TUI/app-server build graph.
spawn-agent-description-model-surface-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::spawn_agent_description::spawn_agent_description_lists_visible_models_and_reasoning_efforts -- --exact --test-threads=1

# Compatibility wrapper for the picker-model shared surface. The interactive
# TUI consumer still shares the same protocol helper, but this exact lane
# intentionally avoids compiling codex-tui while app-server drift contaminates
# small mapped picker-model runs.
tui-agent-picker-model-surface-targeted:
    just --justfile ../justfile spawn-agent-tool-model-surface-targeted
    just --justfile ../justfile spawn-agent-description-model-surface-targeted

# Focused /agent picker hierarchy visibility slice.
tui-agent-picker-tree-targeted:
    cargo test -p codex-tui app::tests::open_agent_picker_marks_loaded_threads_open --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::inactive_thread_started_notification_initializes_replay_session --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::agent_navigation::tests::picker_tree_prefixes_reflect_nested_agent_paths --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::loaded_threads::tests::finds_loaded_subagent_tree_for_primary_thread --lib -- --exact --test-threads=1

# Focused /agent picker usage and remaining-context visibility slice.
tui-agent-picker-usage-targeted:
    cargo test -p codex-tui app::tests::agent_picker_thread_token_usage_reads_inactive_thread_store --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::agent_picker_thread_token_usage_prefers_live_active_thread_usage --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::agent_picker_thread_token_usage_does_not_fallback_when_active_live_usage_is_zero --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::open_agent_picker_marks_loaded_threads_open --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::inactive_thread_started_notification_initializes_replay_session --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_falls_back_to_thread_id_without_usage --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_compact_token_usage_when_present --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_remaining_context_when_known --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_compact_age_when_known --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_selected_description_includes_permission_details_when_available --lib -- --exact --test-threads=1

# Focused TUI combined session-vs-thread token usage slice.
tui-agent-usage-totals-targeted:
    cargo test -p codex-tui app::tests::sync_session_tree_token_usage_updates_combined_status_line_items --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::sync_session_tree_token_usage_prefers_selected_subagent_usage_for_status_line --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::status_and_layout::status_line_combined_token_items_use_session_totals --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::status_and_layout::status_line_combined_used_tokens_footer_snapshot --lib -- --exact --test-threads=1
    cargo test -p codex-tui status::tests::status_snapshot_distinguishes_session_and_thread_token_usage --lib -- --exact --test-threads=1

# Focused TUI interrupt confirmation slice for Alt/meta-safe Esc handling.
tui-esc-interrupt-targeted:
    cargo nextest run -p codex-tui --no-fail-fast -- bottom_pane::tests::esc_requires_double_press_for_interrupt_when_running_task_by_default bottom_pane::tests::first_esc_renders_again_to_interrupt_hint bottom_pane::tests::esc_release_does_not_confirm_interrupt bottom_pane::tests::esc_with_alt_does_not_interrupt_running_task bottom_pane::tests::esc_single_press_interrupts_when_double_press_disabled --exact

# Focused TUI queued-follow-up front-insert slice.
tui-front-queue-submit-targeted:
    cargo test -p codex-tui bottom_pane::chat_composer::tests::ctrl_shift_q_queues_front_when_task_running --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::front_queued_follow_up_runs_before_back_queued_follow_up --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::replayed_turn_complete_submits_restored_front_queued_follow_up_first --lib -- --exact --test-threads=1
    cargo test -p codex-tui footer_snapshots -- --exact --test-threads=1
    cargo test -p codex-tui footer_collapse_snapshots -- --exact --test-threads=1

# Focused TUI transcript viewport redraw and clipping slice.
tui-transcript-viewport-targeted:
    cargo test -p codex-tui --test all suite::vt100_history::tmux_like_viewport_preserves_preexisting_history_content -- --exact --test-threads=1
    cargo test -p codex-tui --test all suite::vt100_history::android_style_narrow_viewport_keeps_url_content_from_being_clipped -- --exact --test-threads=1
    cargo test -p codex-tui --test all suite::vt100_history::committed_rows_survive_redraw_and_viewport_pressure -- --exact --test-threads=1

# Focused multi-agent orchestration slice covering wait semantics and tool guidance.
core-multi-agent-orchestration-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core multi_agent_v2_list_agents_returns_completed_status_and_last_task_message --lib -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core multi_agent_v2_wait_agent_honors_return_when_all --lib -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::spawn_agent_description::spawn_wait_and_list_agents_tool_descriptions_have_guidance_updates -- --exact --test-threads=1

# Focused blocking-wait slice covering direct unified-exec waits, agent waits,
# app-server command execution completion ordering, and MCP task completion.
blocking-waits-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -j 1 -p codex-core --test all -- suite::unified_exec::exec_command_reports_chunk_and_exit_metadata --exact
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -j 1 -p codex-core --test all -- suite::unified_exec::write_stdin_returns_exit_metadata_and_clears_session --exact
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core multi_agent_v2_wait_agent_honors_return_when_all --lib -- --exact --test-threads=1
    cargo nextest run -j 1 -p codex-app-server --test all -- suite::v2::turn_start::command_execution_completion_precedes_turn_completion_and_preserves_process_id --exact
    cargo nextest run -j 1 -p codex-mcp-server --test all -- suite::codex_tool::shell_command_approval_emits_task_complete_before_tool_response --exact

# Focused custom-prompt discovery and review-flow slice.
custom-prompts-targeted:
    cargo test -p codex-core custom_prompts::tests:: --lib -- --test-threads=1
    cargo test -p codex-core review_prompts::tests:: --lib -- --test-threads=1
    cargo test -p codex-tui chatwidget::tests::review_mode::review_popup_custom_prompt_action_sends_event --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::review_mode::custom_prompt_submit_sends_review_op --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::review_mode::custom_prompt_enter_empty_does_not_send --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::review_mode::review_custom_prompt_escape_navigates_back_then_dismisses --lib -- --exact --test-threads=1

# Focused downstream MCP safety slice for config mutability and OAuth fallback
# hardening.
mcp-safety-targeted:
    cargo test -p codex-core config::edit_tests::blocking_replace_mcp_servers_round_trips --lib -- --exact --test-threads=1
    cargo test -p codex-core config::edit_tests::blocking_replace_mcp_servers_serializes_tool_approval_overrides --lib -- --exact --test-threads=1
    cargo test -p codex-core config::service_tests::write_value_supports_custom_mcp_server_default_tool_approval_mode --lib -- --exact --test-threads=1
    cargo test -p codex-rmcp-client load_oauth_tokens_ --lib -- --test-threads=1

# Focused model-pinning slice for exact spawn-agent model slug preservation.
core-subagent-model-pinning-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core spawn_agent_preserves_exact_model_slug_override_through_role_layering --lib -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core multi_agent_v2_spawn_reports_requested_and_effective_model_metadata --lib -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core spec_tests::test_build_specs_multi_agent_v2_uses_task_names_and_hides_resume --lib -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-tools spawn_agent_tool_ --lib -- --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::subagent_notifications::spawn_agent_preserves_exact_requested_model_slug_through_role_layering -- --exact --test-threads=1

# Focused spawn-approval gate and schema slice.
core-subagent-spawn-approval-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core spawn_agent_requires_user_approval_when_requested --lib -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core multi_agent_v2_spawn_requires_user_approval_when_requested --lib -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core spawn_agent_approval_respects_request_user_input_mode_availability --lib -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core spawn_agent_approval_question_includes_preview_role_and_model_context --lib -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-tools spawn_agent_tool_v2_requires_task_name_and_lists_visible_models --lib -- --exact --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-tools spawn_agent_tool_v1_exposes_runtime_metadata_fields --lib -- --exact --test-threads=1

# Focused persisted-descendant inventory slice for subtree close/resume behavior.
core-persisted-subagent-descendants-targeted:
    cargo test -p codex-state thread_spawn_edges_track_directional_status --lib -- --exact --test-threads=1

# Focused app-server thread surface slice.
app-server-thread-cwd-targeted:
    cargo test --locked -p codex-app-server --test all suite::conversation_summary:: -- --test-threads=1
    cargo test --locked -p codex-app-server --test all suite::v2::thread_list:: -- --test-threads=1
    cargo test --locked -p codex-app-server --test all suite::v2::thread_read::thread_read_returns_summary_without_turns -- --exact --test-threads=1
    cargo test --locked -p codex-app-server --test all suite::v2::thread_resume::thread_resume_returns_rollout_history -- --exact --test-threads=1
    cargo test --locked -p codex-app-server --test all suite::v2::thread_fork::thread_fork_honors_explicit_null_thread_instructions -- --exact --test-threads=1
    cargo test --locked -p codex-app-server --test all suite::v2::turn_start::turn_start_honors_explicit_null_thread_instructions -- --exact --test-threads=1
    cargo test --locked -p codex-app-server --test all suite::v2::turn_start::turn_start_emits_spawn_agent_item_with_requested_model_metadata_when_role_layering_is_present_v2 -- --exact --test-threads=1

# Focused downstream agent-workflow helper sanity slice.
[no-cd]
agent-workflow-sanity:
    cd "{{justfile_directory()}}" && python3 -m py_compile \
        .codex/skills/babysit-pr/scripts/gh_pr_watch.py \
        .codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch.py \
        .codex/skills/babysit-gh-workflow-run/scripts/gh_dispatch_and_watch.py \
        .codex/skills/sedna/subagent-session-tail/scripts/inspect_subagent_tail.py
    cd "{{justfile_directory()}}" && python3 .codex/skills/babysit-gh-workflow-run/tests/test_gh_workflow_run_watch.py
    cd "{{justfile_directory()}}" && python3 .codex/skills/babysit-gh-workflow-run/tests/test_gh_dispatch_and_watch.py
    cd "{{justfile_directory()}}" && python3 .codex/skills/sedna/subagent-session-tail/scripts/inspect_subagent_tail.py --help >/dev/null

# Focused shell-tool-mcp package sanity slice.
[no-cd]
shell-tool-mcp-ci:
    cd "{{justfile_directory()}}" && corepack enable
    cd "{{justfile_directory()}}" && pnpm install --frozen-lockfile
    cd "{{justfile_directory()}}" && pnpm --filter @openai/codex-shell-tool-mcp run format
    cd "{{justfile_directory()}}" && pnpm --filter @openai/codex-shell-tool-mcp test
    cd "{{justfile_directory()}}" && pnpm --filter @openai/codex-shell-tool-mcp run build

# Focused build/config policy sanity slice for install and workspace checks.
[no-cd]
build-policy-sanity:
    cd "{{justfile_directory()}}" && bash -n scripts/install/install.sh
    cd "{{justfile_directory()}}" && python3 -m py_compile scripts/stage_npm_packages.py .github/scripts/verify_bazel_clippy_lints.py .github/scripts/verify_cargo_workspace_manifests.py
    cd "{{justfile_directory()}}" && python3 .github/scripts/verify_bazel_clippy_lints.py
    cd "{{justfile_directory()}}" && python3 .github/scripts/verify_cargo_workspace_manifests.py

# Focused code-mode declaration rendering and metadata slice.
code-mode-declaration-targeted:
    cargo test --locked -p codex-tools code_mode_ --lib -- --test-threads=1
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --test all -- suite::code_mode::code_mode_exports_all_tools_metadata_for_builtin_tools suite::code_mode::code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools suite::code_mode::code_mode_declaration_normalization_is_layout_tolerant_and_semantically_strict --exact

# Focused tool-context serialization slice for custom/function/abort outputs.
core-context-serialization-targeted:
    cargo test -p codex-core tools::context::tests::custom_tool_calls_should_roundtrip_as_custom_outputs --lib -- --exact
    cargo test -p codex-core tools::context::tests::function_payloads_remain_function_outputs --lib -- --exact
    cargo test -p codex-core tools::context::tests::aborted_tool_output_serializes_ --lib -- --test-threads=1
    cargo test -p codex-core --test all suite::abort_tasks::interrupt_tool_records_history_entries -- --exact --test-threads=1

# Focused attestation contract slice for phase-2 fail-closed reuse semantics.
core-attestation-targeted:
    cargo test -p codex-core consolidation_artifacts_ready_rejects_ --lib -- --test-threads=1
    cargo test -p codex-state global_phase2_attestation_requirement_is_root_scoped -- --exact --test-threads=1

# Codex authoritative usage.sqlite logging contracts.
core-ledger-smoke:
    cargo nextest run -p codex-state --no-fail-fast -- runtime::tests::init_removes_legacy_logs_and_usage_db_files runtime::usage::tests::usage_logger_records_requested_model_and_quota_snapshot runtime::usage::tests::usage_logger_tracks_tool_call_lifecycle runtime::usage::tests::usage_logger_captures_spawn_request_and_fork_snapshot runtime::usage::tests::usage_logger_resolves_root_thread_from_parent_or_fork runtime::usage::tests::usage_logger_clears_turn_snapshot_after_turn_complete runtime::usage::tests::usage_logger_resolves_root_thread_from_persisted_lineage_after_restart --exact

# Fast smoke checks for fragile codex-core integration buckets that still fit
# one bounded runtime shard.
core-runtime-surface-smoke:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --test all -- suite::rmcp_client::stdio_server_round_trip suite::code_mode::code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools suite::plugins::plugin_mcp_tools_are_listed suite::truncation::mcp_tool_call_output_exceeds_limit_truncated_for_model suite::client::usage_limit_error_emits_rate_limit_event suite::client_websockets::responses_websocket_usage_limit_error_emits_rate_limit_event --exact

# Focused persisted-state/usage lineage contract slice for subagent graph adoption.
core-state-spawn-lineage-contract-targeted:
    cargo test -p codex-state usage_spawn_lineage_matches_persisted_state_edge_for_child_thread -- --test-threads=1

# Cross-repo ledger seam validation (agent-usage-ledger + Postgres).
[no-cd]
downstream-ledger-seam:
    ledger_repo_root="${LEDGER_REPO_ROOT:-../agent-usage-ledger}"; \
    ledger_scripts_dir="$ledger_repo_root/scripts/llm_usage"; \
    if [ ! -d "$ledger_repo_root" ]; then \
      echo "Skipping downstream-ledger-seam: missing ledger repo at $ledger_repo_root"; \
      exit 0; \
    fi; \
    if ! command -v psql >/dev/null 2>&1; then \
      echo "Skipping downstream-ledger-seam: missing psql"; \
      exit 0; \
    fi; \
    for required_script in \
      "$ledger_scripts_dir/ensure_schema.sh" \
      "$ledger_scripts_dir/ingest_codex_rollouts_to_postgres.sh" \
      "$ledger_scripts_dir/test_codex_copied_history_filter.sh" \
      "$ledger_scripts_dir/test_codex_source_row_identity.sh"; do \
      if [ ! -x "$required_script" ]; then \
        echo "Skipping downstream-ledger-seam: missing ledger helper $required_script"; \
        exit 0; \
      fi; \
    done; \
    "$ledger_scripts_dir/ensure_schema.sh" --schema "${LLM_USAGE_DB_SCHEMA:-llm_usage}"; \
    "$ledger_scripts_dir/ingest_codex_rollouts_to_postgres.sh" --schema "${LLM_USAGE_DB_SCHEMA:-llm_usage}" --skip-schema; \
    "$ledger_scripts_dir/test_codex_copied_history_filter.sh"; \
    "$ledger_scripts_dir/test_codex_source_row_identity.sh"

[no-cd]
downstream-docs-check:
    git diff --check -- docs/downstream.md docs/carry-divergence-ledger.md docs/downstream-regression-matrix.md docs/downstream-tool-surface-matrix.md docs/divergences/index.yaml

[no-cd]
workflow-ci-sanity:
    cd "{{justfile_directory()}}" && python3 -m py_compile .github/scripts/aggregate_validation_summary.py .github/scripts/check_markdown_links.py .github/scripts/resolve_rust_ci_mode.py .github/scripts/resolve_validation_plan.py .github/scripts/test_ci_planners.py
    cd "{{justfile_directory()}}" && python3 -m unittest discover -s .github/scripts -p 'test_ci_planners.py'
    cd "{{justfile_directory()}}" && ruby -e 'require "yaml"; %w[.github/workflows/_sedna-linux-rust.yml .github/workflows/docs-sanity.yml .github/workflows/rust-ci-full.yml .github/workflows/rust-ci.yml .github/workflows/sedna-heavy-tests.yml .github/workflows/validation-lab.yml].each { |path| YAML.load_file(path) }; puts "yaml-ok"'

[no-cd]
downstream-divergence-audit:
    cd "{{justfile_directory()}}" && python3 scripts/downstream-divergence-audit.py --repo . --downstream-remote origin --downstream-branch main --mirror-remote origin --mirror-branch upstream-main --upstream-remote upstream --upstream-branch main --registry-path docs/divergences/index.yaml --output-dir target/downstream-divergence-audit --format both --code-only --enforce-registry

# Early non-publishing Linux release-build smoke coverage.
sedna-release-linux-smoke:
    CODEX_RELEASE_VERSION="${CODEX_RELEASE_VERSION:-0.0.0-sedna.smoke}" cargo build --locked --target x86_64-unknown-linux-gnu --release --bin codex --bin codex-responses-api-proxy

# Fast smoke checks for fragile codex-core integration buckets.
core-test-smoke:
    just core-compile-smoke
    just core-carry-core-smoke
    just core-carry-ui-smoke
    just core-ledger-smoke
    just core-runtime-surface-smoke

# Progressive codex-core ladder:
# 1) smoke gate, 2) high-churn buckets, 3) full suite.
core-test-progressive:
    just core-test-smoke
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --test all -- suite::rmcp_client::
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --test all -- suite::code_mode::
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --test all -- suite::truncation::
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --test all -- suite::plugins::
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast

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
    {{ justfile_directory() }}/scripts/check-module-bazel-lock.sh

bazel-test:
    bazel test --test_tag_filters=-argument-comment-lint //... --keep_going

[no-cd]
bazel-clippy:
    bazel_targets="$({{ justfile_directory() }}/scripts/list-bazel-clippy-targets.sh)" && bazel build --config=clippy -- ${bazel_targets}

[no-cd]
bazel-argument-comment-lint:
    bazel build --config=argument-comment-lint -- $({{ justfile_directory() }}/tools/argument-comment-lint/list-bazel-targets.sh)

bazel-remote-test:
    bazel test --test_tag_filters=-argument-comment-lint //... --config=remote --platforms=//:rbe --keep_going

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
    cargo run --manifest-path {{ justfile_directory() }}/codex-rs/Cargo.toml -p codex-hooks --bin write_hooks_schema_fixtures

# Run the argument-comment Dylint checks across codex-rs.
[no-cd]
_run-bazel-argument-comment-lint:
    cd "{{justfile_directory()}}" && bazel build --config=argument-comment-lint -- $("{{justfile_directory()}}"/tools/argument-comment-lint/list-bazel-targets.sh)

[no-cd]
argument-comment-lint *args:
    if [ "$#" -eq 0 ]; then \
      bazel build --config=argument-comment-lint -- $({{ justfile_directory() }}/tools/argument-comment-lint/list-bazel-targets.sh); \
    else \
      {{ justfile_directory() }}/tools/argument-comment-lint/run-prebuilt-linter.py "$@"; \
    fi

[no-cd]
argument-comment-lint-from-source *args:
    {{ justfile_directory() }}/tools/argument-comment-lint/run.py "$@"

# Tail logs from the state SQLite database
log *args:
    if [ "${1:-}" = "--" ]; then shift; fi; cargo run -p codex-state --bin logs_client -- "$@"
