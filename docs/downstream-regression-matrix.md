# Downstream Regression Matrix

This matrix maps live downstream divergences to the smallest default test lane
that should fail if the behavior regresses. Historical references to
`carry/main` refer to the pre-cutover name for the maintained downstream
branch.

## Core default path

Default broader local validation path when you intentionally want a repo-side
gate:

- `just core-test-progressive`

Fast lanes used by `core-test-smoke` locally and by the remote smoke matrix:

- `core-compile-smoke`
- `core-carry-core-smoke`
- `core-carry-ui-smoke`
- `core-ledger-smoke`
- `core-runtime-surface-smoke`
- `core-carry-core-smoke` isolates downstream core/tool-surface carry checks so
  core regressions fail without waiting on TUI smoke.
- `core-carry-ui-smoke` isolates `codex-tui` replayed-queue and selected-agent
  footer regressions, so downstream interactive behavior still fails during the
  PR smoke pass without obscuring core-only timing.
- `core-ledger-smoke` now includes the `usage.sqlite` cleanup, turn-reset, and
  restart-lineage regressions, so downstream accounting behavior
  fails during the same smoke pass.
- `core-runtime-surface-smoke` isolates the fragile codex-core runtime seams in
  their own shard, so remote smoke runs can fail the exact runtime bucket
  without serializing the whole smoke pass behind one `just` recipe.

Focused targeted lanes for iterative work on the current carry seams:

- `codex.core-startup-sync-targeted`
- `codex.core-subagent-surface-targeted`
- `codex.core-subagent-notification-contract-targeted`
- `codex.core-subagent-notification-visibility-targeted`
- `codex.core-multi-agent-orchestration-targeted`
- `codex.blocking-waits-targeted`
- `codex.custom-prompts-targeted`
- `codex.mcp-safety-targeted`
- `codex.agent-workflow-sanity`
- `codex.shell-tool-mcp-ci`
- `codex.build-policy-sanity`
- `codex.core-subagent-spawn-approval-targeted`
- `codex.core-persisted-subagent-descendants-targeted`
- `codex.code-mode-declaration-targeted`
- `codex.core-context-serialization-targeted`
- `codex.core-attestation-targeted`
- `codex.state-spawn-lineage-contract-targeted`
- `codex.tui-esc-interrupt-targeted`
- `codex.tui-transcript-viewport-targeted`
- `codex.tui-agent-picker-targeted`
- `codex.spawn-agent-tool-model-surface-targeted`
- `codex.spawn-agent-description-model-surface-targeted`
- `codex.tui-agent-picker-model-surface-targeted`
- `codex.tui-agent-picker-tree-targeted`
- `codex.tui-agent-usage-totals-targeted`
- `codex.downstream-docs-check`

Validation workflow reference:

- `docs/validation_workflow.md`

Focused lane used for protocol/event-history seams:

- `codex.app-server-protocol-test`
- `codex.app-server-thread-cwd-targeted`

GitHub Actions lane naming (`.github/workflows/sedna-heavy-tests.yml`):

- Workflow shard names intentionally mirror this document's guardrail lane
  identifiers where possible.
- `workflow_dispatch` input `lane` uses these lane IDs directly (`all` runs
  every shard).
- Where the local `justfile` recipe name differs, workflow shards still retain
  the docs lane ID and only translate at command execution time.

## GitHub trigger policy

- `rust-ci.yml` is the default PR fail-fast workflow.
  - It runs on `pull_request`, scheduled hygiene sweeps, and manual dispatch.
  - `Rust CI lane planner (informational)` explains lane selection and should
    not be treated as a merge gate.
  - `Rust CI required gate` is the single required merge gate.
  - Tiny mapped initial PRs and already-green follow-up pushes may reuse
    incremental targeted validation instead of rerunning the full `rust-ci`
    bundle.
  - Workflow and route-map edits also run cheap planner fixture tests so
    follow-up lane selection does not silently drift.
  - Any ambiguous or high-risk follow-up falls back to the normal `rust-ci`
    path.
- `sedna-heavy-tests.yml` is the downstream-heavy lane workflow.
  - On ordinary PR updates, it auto-selects only the heavy lanes implied by the
    changed path class.
  - Non-doc heavy runs must clear the runtime smoke bundle
    (`core-compile-smoke`, `core-carry-core-smoke`, `core-carry-ui-smoke`,
    `core-ledger-smoke`, `core-runtime-surface-smoke`) before the broader lane
    matrix fans out.
  - Both the smoke gate and the selected heavy lane fanout now split by
    `setup_class` (`workflow`, `node`, `rust_minimal`, `rust_integration`,
    `release`) so cheap workflow/docs shards do not queue behind heavier Rust
    runners.
  - The heavy matrix remains capped and fail-fast on PRs.
  - Changes to workflow wiring or the `justfile` run the smoke gate plus a small
    representative workflow-validation lane set instead of promoting the PR to
    the full heavy matrix.
  - Applying the `ci:heavy` label promotes the PR to the full heavy matrix.
  - `merge_group` and `workflow_dispatch lane=all` run the full heavy matrix.
  - `workflow_dispatch lane=<named-lane>` runs that shard directly when a single
    heavy lane is the right debugging tool, instead of forcing the full smoke
    gate ahead of the explicit manual request.
  - The workflow now uploads a top-level `validation-summary` artifact and
    step summary so operators can read one structured blocker report instead of
    trawling lane logs.
- `validation-lab.yml` is the dispatch-only remote validation surface.
  - Use it for scratch refs, integration refs, orphan-branch experiments, and
    non-PR seam validation.
  - Its workflow summary now records the selected profile intent, short profile
    notes, and a compact lane-selection summary for easier babysitting.
  - It consumes the same explicit script-backed lane catalog as
    `sedna-heavy-tests.yml`, so lane setup requirements are declared rather than
    inferred from inline commands.
  - `profile=smoke` and `profile=targeted` are the default inner-loop remote
    validation tools.
  - `profile=frontier` is the bounded next-blocker harvest mode to use only
    after a recent trusted smoke or targeted baseline.
  - `profile=broad` and `profile=full` are for explicit broader questions, not
    routine iteration.
  - `profile=artifact` or `artifact_build=true` is the right way to request a
    disposable preview build on a non-PR ref without promoting that ref into the
    normal PR/main check surface.
  - It uploads a compact `validation-summary` artifact so watchers and follow-up
    tooling can prefer structured first-failure signal over raw log scraping.
- `docs-sanity.yml` is the cheap documentation-link workflow.
  - Use it when the real question is whether markdown links still resolve after
    touching `README.md` or `docs/**`.
- `sedna-branch-build.yml` is the preview artifact path.
  - It is manual-dispatch only.
  - Treat it as artifact validation, not the primary downstream correctness
    gate.

## Divergence mapping

| Divergence | Guardrail lane | Primary checks |
| --- | --- | --- |
| Downstream divergence audit plumbing and registry reconciliation | `sedna-sync-upstream` | `scripts/downstream-divergence-audit.py`; `.github/workflows/sedna-sync-upstream.yml`; `docs/divergences/index.yaml`; `justfile` |
| Sub-agent override preservation and requested/effective model diagnostics across role reload | `codex.core-subagent-model-pinning-targeted` | `spawn_agent_preserves_exact_model_slug_override_through_role_layering`; `multi_agent_v2_spawn_reports_requested_and_effective_model_metadata`; `test_build_specs_multi_agent_v2_uses_task_names_and_hides_resume`; `spawn_agent_tool_v2_requires_task_name_and_lists_visible_models`; `spawn_agent_tool_v1_exposes_runtime_metadata_fields`; `suite::subagent_notifications::spawn_agent_preserves_exact_requested_model_slug_through_role_layering` |
| Sub-agent spawns can require explicit user approval without bypassing request-user-input mode rules, and the approval prompt carries enough context to approve or decline safely | `codex.core-subagent-spawn-approval-targeted` | `spawn_agent_requires_user_approval_when_requested`; `multi_agent_v2_spawn_requires_user_approval_when_requested`; `spawn_agent_approval_respects_request_user_input_mode_availability`; `spawn_agent_approval_question_includes_preview_role_and_model_context`; `spawn_agent_tool_v2_requires_task_name_and_lists_visible_models`; `spawn_agent_tool_v1_exposes_runtime_metadata_fields` |
| Sub-agent surface contract (`spawn_agent`/`list_agents` omit raw `model_reasoning_summary`, live inventory rows expose active-descendant hints, model-visible tool specs include the carried sub-agent inventory surfaces, and built-in `explorer` guidance stays on the cheap-first downstream default without hard-locking unavailable model settings) | `codex.core-subagent-surface-targeted` | `apply_explorer_role_preserves_model_settings_and_adds_session_flags_layer`; `multi_agent_v2_list_agents_returns_completed_status_and_last_task_message`; `multi_agent_v2_list_agents_flags_active_descendants`; `test_build_specs_multi_agent_v2_uses_task_names_and_hides_resume`; `test_gpt_5_defaults`; `test_gpt_5_1_defaults`; `test_codex_5_1_mini_defaults`; `test_gpt_5_1_codex_max_unified_exec_web_search`; `test_full_toolset_specs_for_gpt5_codex_unified_exec_web_search`; `test_gpt_5_1_codex_max_defaults` |
| Core-side sub-agent notification contract (`codex-core`) | `codex.core-subagent-notification-contract-targeted` | `format_subagent_notification_message_round_trips_completed_status`; `classifies_memory_excluded_fragments`; `drop_last_n_user_turns_ignores_session_prefix_user_messages`; `serializes_memory_rollout_with_agents_removed_but_environment_kept` |
| Sub-agent completion-notification parser + TUI render surface (`protocol` + `tui`) | `codex.core-subagent-notification-visibility-targeted` | `parse_subagent_notification_response_item_*`; `raw_response_subagent_notification_renders_history` |
| Sub-agent inventory + blocking join surface | `codex.core-multi-agent-orchestration-targeted` | `multi_agent_v2_list_agents_returns_completed_status_and_last_task_message`; `multi_agent_v2_wait_agent_honors_return_when_all`; `spawn_wait_and_list_agents_tool_descriptions_have_guidance_updates` |
| Persisted sub-agent descendant status across close + rollout resume | `codex.core-persisted-subagent-descendants-targeted` | `thread_spawn_edges_track_directional_status` |
| Code-mode declaration formatting + namespaced tool metadata | `codex.code-mode-declaration-targeted` | `augment_tool_spec_for_code_mode_*`; `tool_spec_to_code_mode_tool_definition_*`; `code_mode_declaration_normalization_is_layout_tolerant_and_semantically_strict`; `code_mode_exports_all_tools_metadata_for_builtin_tools`; `code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools` |
| Blocking waits across unified exec, agent joins, app-server command execution, and MCP task completion | `core-carry-core-smoke`; `codex.blocking-waits-targeted` | `exec_command_reports_chunk_and_exit_metadata`; `write_stdin_returns_exit_metadata_and_clears_session`; `multi_agent_v2_wait_agent_honors_return_when_all`; `command_execution_completion_precedes_turn_completion_and_preserves_process_id`; `shell_command_approval_emits_task_complete_before_tool_response`; `exec_command_tool_matches_expected_spec`; `write_stdin_tool_matches_expected_spec`; `unified_exec_tools_include_wait_until_terminal_contract_fields`; `exec_command_args_reject_invalid_wait_until_terminal_type`; `write_stdin_wait_until_terminal_requires_empty_chars` |
| Custom prompt discovery and review prompt flow | `codex.custom-prompts-targeted` | `custom_prompts::tests::*`; `review_prompts::tests::*`; `review_popup_custom_prompt_action_sends_event`; `custom_prompt_submit_sends_review_op`; `custom_prompt_enter_empty_does_not_send`; `review_custom_prompt_escape_navigates_back_then_dismisses` |
| MCP server safety policy extensions and OAuth fallback hardening | `codex.mcp-safety-targeted` | `blocking_replace_mcp_servers_round_trips`; `blocking_replace_mcp_servers_serializes_tool_approval_overrides`; `write_value_supports_custom_mcp_server_default_tool_approval_mode`; `load_oauth_tokens_ignores_empty_fallback_file`; `load_oauth_tokens_auto_ignores_corrupt_fallback_when_keyring_errors` |
| Agent-workflow helpers for PR babysitting, workflow dispatch/watch loops, and subagent tail inspection | `codex.agent-workflow-sanity` | `python3 -m py_compile` over `gh_pr_watch.py`, `gh_workflow_run_watch.py`, `gh_dispatch_and_watch.py`, and `inspect_subagent_tail.py`; `test_gh_workflow_run_watch.py`; `test_gh_dispatch_and_watch.py`; `inspect_subagent_tail.py --help` |
| Shell tool MCP helper package CI and packaging sanity | `codex.shell-tool-mcp-ci` | `pnpm --filter @openai/codex-shell-tool-mcp run format`; `pnpm --filter @openai/codex-shell-tool-mcp test`; `pnpm --filter @openai/codex-shell-tool-mcp run build` |
| Build/config policy scripts and workspace guardrails | `codex.build-policy-sanity` | `bash -n scripts/install/install.sh`; `verify_bazel_clippy_lints.py`; `verify_cargo_workspace_manifests.py`; `py_compile` for `stage_npm_packages.py` and the policy verifier scripts |
| Turn-complete compaction count metadata | `codex.app-server-protocol-test`; `codex.app-server-thread-cwd-targeted`; `codex.blocking-waits-targeted` | `preserves_compaction_only_turn`; broader `TurnCompleteEvent` shape coverage in `codex-core`, `codex-exec`, and `codex-tui` tests keeps `compaction_events_in_turn` wired through downstream consumers |
| App-server rollout cwd portability across thread list/read/resume summary surfaces | `codex.app-server-protocol-test`; `codex.app-server-thread-cwd-targeted`; `codex.blocking-waits-targeted` | `get_conversation_summary_by_*`; `thread_list_*`; `thread_read_returns_summary_without_turns`; `thread_resume_returns_rollout_history`; `command_execution_completion_precedes_turn_completion_and_preserves_process_id` |
| TUI queued slash dequeue + replay ordering keeps recalled items out of the visible queued list, including restored-state metadata fallback | `core-carry-ui-smoke` | `queued_popup_command_replay_waits_before_submitting_next_message`; `alt_up_restores_most_recent_queued_slash_command`; `alt_up_restored_state_with_missing_insert_order_preserves_front_back_recall_order` |
| TUI queued follow-up front-insert semantics keep “run next” drafts ahead of append-queued drafts and preserve footer hints/snapshots | `codex.tui-front-queue-submit-targeted` | `ctrl_shift_q_queues_front_when_task_running`; `front_queued_follow_up_runs_before_back_queued_follow_up`; `replayed_turn_complete_submits_restored_front_queued_follow_up_first`; `footer_snapshots`; `footer_collapse_snapshots` |
| Per-thread approval/sandbox/reviewer overrides survive thread switches (`codex-tui`) | `codex.tui-thread-session-policy-targeted` | `store_active_thread_receiver_persists_per_thread_policy_overrides` |
| Double-`Esc` interrupt confirmation protects Alt/meta terminals while preserving explicit single-press override (`codex-tui`) | `codex.tui-esc-interrupt-targeted` | `esc_requires_double_press_for_interrupt_when_running_task_by_default`; `first_esc_renders_again_to_interrupt_hint`; `esc_release_does_not_confirm_interrupt`; `esc_with_alt_does_not_interrupt_running_task`; `esc_single_press_interrupts_when_double_press_disabled` |
| TUI transcript viewport redraw and clipping regressions | `codex.tui-transcript-viewport-targeted` | `suite::vt100_history::tmux_like_viewport_preserves_preexisting_history_content`; `suite::vt100_history::android_style_narrow_viewport_keeps_url_content_from_being_clipped`; `suite::vt100_history::committed_rows_survive_redraw_and_viewport_pressure` |
| Active-thread session state survives config refresh and fresh-session clones keep policy mutability before new-thread/fork flows (`codex-tui`) | `codex.tui-config-refresh-session-targeted` | `refresh_in_memory_config_from_disk_preserves_active_thread_session_state`; `fresh_session_config_uses_current_session_state`; `fresh_session_config_preserves_policy_mutability` |
| `/agent` picker rows expose searchable cached thread metadata, compact age labels, and model/task previews without waiting on broader TUI validation | `codex.tui-agent-picker-targeted` | `open_agent_picker_marks_loaded_threads_open`; `inactive_thread_started_notification_initializes_replay_session`; `picker_description_falls_back_to_thread_id_without_usage`; `picker_description_includes_compact_token_usage_when_present`; `picker_description_includes_remaining_context_when_known`; `picker_description_includes_compact_age_when_known`; `picker_description_includes_model_effort_and_task_when_available` |
| Upgradeable legacy models stay visible across the shared picker-model guidance surfaces without widening a mapped PR into the app-server-contaminated `codex-tui` build graph | `codex.spawn-agent-tool-model-surface-targeted`; `codex.spawn-agent-description-model-surface-targeted` | `spawn_agent_tool_v2_requires_task_name_and_lists_visible_models`; `spawn_agent_tool_v2_lists_upgradeable_legacy_models`; `suite::spawn_agent_description::spawn_agent_description_lists_visible_models_and_reasoning_efforts` |
| `/agent` picker rows show visible parent/child hierarchy for thread-spawned subagents using cached agent paths instead of a flat list (`codex-tui`) | `codex.tui-agent-picker-tree-targeted` | `open_agent_picker_marks_loaded_threads_open`; `inactive_thread_started_notification_initializes_replay_session`; `picker_tree_prefixes_reflect_nested_agent_paths`; `finds_loaded_subagent_tree_for_primary_thread` |
| `/agent` picker rows expose per-thread used-token totals, compact remaining-context visibility, compact age labels, searchable stale-session filtering, and selected-row permission/sandbox detail from existing cached thread metadata without a broader backend contract change (`codex-tui`) | `codex.tui-agent-picker-usage-targeted` | `agent_picker_thread_token_usage_reads_inactive_thread_store`; `agent_picker_thread_token_usage_prefers_live_active_thread_usage`; `agent_picker_thread_token_usage_does_not_fallback_when_active_live_usage_is_zero`; `open_agent_picker_marks_loaded_threads_open`; `inactive_thread_started_notification_initializes_replay_session`; `picker_description_falls_back_to_thread_id_without_usage`; `picker_description_includes_compact_token_usage_when_present`; `picker_description_includes_remaining_context_when_known`; `picker_description_includes_compact_age_when_known`; `picker_selected_description_includes_permission_details_when_available` |
| Combined session token totals remain visible without overwriting current-thread token usage across `/status` and footer/status-line surfaces (`codex-tui`) | `codex.tui-agent-usage-totals-targeted` | `sync_session_tree_token_usage_updates_combined_status_line_items`; `sync_session_tree_token_usage_prefers_selected_subagent_usage_for_status_line`; `status_line_combined_token_items_use_session_totals`; `status_line_combined_used_tokens_footer_snapshot`; `status_snapshot_distinguishes_session_and_thread_token_usage` |
| Startup plugin sync bounded wait + completion-signal re-arm + abort checkpointing | `codex.core-startup-sync-targeted` | `startup_remote_plugin_sync_waits_for_late_prerequisites`; `startup_remote_plugin_sync_is_single_flight_before_prerequisites_exist`; `startup_remote_plugin_sync_uses_latest_config_and_auth_snapshot`; `startup_remote_plugin_sync_rearms_after_curated_repo_completion_signal_uses_latest_config_and_auth_snapshot`; `startup_remote_plugin_sync_signals_after_failed_curated_postprocessing`; `startup_remote_plugin_sync_aborts_in_flight_before_stamping_marker`; `startup_remote_plugin_sync_relaunches_immediately_after_abort_even_if_late_completion_signal_arrives` |
| Tool-context serialization for custom/function/abort outputs | `codex.core-context-serialization-targeted` | `custom_tool_calls_should_roundtrip_as_custom_outputs`; `function_payloads_remain_function_outputs`; `aborted_tool_output_serializes_*`; `interrupt_tool_records_history_entries` |
| Phase-2 attestation contract | `codex.core-attestation-targeted` | `consolidation_artifacts_ready_rejects_*`; `global_phase2_attestation_requirement_is_root_scoped` |
| Persisted state spawn-edge lineage matches local `usage.sqlite` | `codex.state-spawn-lineage-contract-targeted` | `usage_spawn_lineage_matches_persisted_state_edge_for_child_thread` |
| Usage logging contracts in local `usage.sqlite` | `core-ledger-smoke` | `usage_logger_*` focused table-contract tests in `codex-rs/state/src/runtime/usage.rs` |
| Linux sandbox/core compile seam | `core-compile-smoke` | `cargo check -p codex-linux-sandbox -p codex-core --tests` |
| Linux release-build dependency / `Cargo.lock` drift | `sedna.release-linux-smoke` | `cargo build --locked --target x86_64-unknown-linux-gnu --release --bin codex --bin codex-responses-api-proxy` |
| Postgres ledger ingest + copied-history/source-row regressions | `downstream-ledger-seam` | `ensure_schema.sh`; `ingest_codex_rollouts_to_postgres.sh`; `test_codex_copied_history_filter.sh`; `test_codex_source_row_identity.sh` |

The replay assertions that used to live under `tui_app_server` now ride on the
cut-over `codex-tui` app tests. Keep the preset green with the parser test and
the exact `codex-tui` render/app checks rather than carrying compile coverage
for a removed crate path.

## Validation notes

- Prefer the focused `codex.core-*targeted` lanes for seam-local work before
  widening to `core-test-smoke` or `core-test-progressive`.
- Remote-first validation should follow the ladder in
  [`docs/validation_workflow.md`](validation_workflow.md):
  small local checks first, then `validation-lab` `profile=smoke`,
  `targeted`, or `frontier`, and only then broader checkpoint runs when the
  question genuinely spans multiple seams.
- Full builds are buildability or promotion checkpoints, not the default
  inner-loop validator for ordinary carry iteration.
