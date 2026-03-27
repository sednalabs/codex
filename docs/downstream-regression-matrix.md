# Downstream Regression Matrix

This matrix maps live downstream divergences to the smallest default test lane
that should fail if the behavior regresses. Historical references to
`carry/main` refer to the pre-cutover name for the maintained downstream
branch.

## Core default path

Default local iterative validation path:

- `just core-test-progressive`

Fast lanes used by `core-test-smoke`:

- `core-compile-smoke`
- `core-carry-smoke`
- `core-ledger-smoke`

Focused micro-slices for iterative work on the current carry seams:

- `codex.core-startup-sync-targeted`
- `codex.core-subagent-surface-targeted`
- `codex.core-subagent-notification-contract-targeted`
- `codex.core-subagent-notification-visibility-targeted`
- `codex.core-multi-agent-orchestration-targeted`
- `codex.core-persisted-subagent-descendants-targeted`
- `codex.core-context-serialization-targeted`
- `codex.state-spawn-lineage-contract-targeted`
- `codex.downstream-docs-check`

Focused lane used for protocol/event-history seams:

- `codex.app-server-protocol-test`

## Divergence mapping

| Divergence | Guardrail lane | Primary checks |
| --- | --- | --- |
| Sub-agent override preservation across role reload | `core-carry-smoke` | `spawn_agent_preserves_explicit_model_override_across_role_reload`; `spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role`; `spawn_agent_role_overrides_requested_model_and_reasoning_settings` |
| Sub-agent surface contract (`spawn_agent`/`list_agents` omit raw `model_reasoning_summary`, while `list_agents(include_descendants=true)` surfaces persisted subtree edge status) | `codex.core-subagent-surface-targeted` | `spawn_agent_preserves_explicit_model_override_across_role_reload`; `list_agents_returns_direct_children_with_live_inventory`; `list_agents_include_descendants_reports_persisted_open_and_closed_descendants` |
| Core-side sub-agent notification contract (`codex-core`) | `codex.core-subagent-notification-contract-targeted` | `format_subagent_notification_message_round_trips_completed_status`; `classifies_memory_excluded_fragments`; `drop_last_n_user_turns_ignores_session_prefix_user_messages`; `serializes_memory_rollout_with_agents_removed_but_environment_kept` |
| Sub-agent completion-notification parser + TUI render surface (`protocol` + `tui` + `tui_app_server`) | `codex.core-subagent-notification-visibility-targeted` | `parse_subagent_notification_response_item_*`; `raw_response_subagent_notification_renders_history`; `cargo build -p codex-tui-app-server` |
| Sub-agent inventory + blocking join surface | `codex.core-multi-agent-orchestration-targeted` | `list_agents_returns_direct_children_with_live_inventory`; `wait_agent_allows_return_when_any_and_returns_on_first_final_status`; `wait_agent_allows_return_when_all_and_returns_only_when_all_are_final`; `spawn_wait_and_list_agents_tool_descriptions_have_guidance_updates` |
| Persisted sub-agent descendant status across close + rollout resume | `codex.core-persisted-subagent-descendants-targeted` | `persisted_spawn_descendants_reflect_closed_status` |
| Code-mode declaration formatting + namespaced tool metadata | `core-carry-smoke` | `code_mode_exports_all_tools_metadata_for_builtin_tools`; `code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools` |
| Unified-exec blocking wait semantics | `core-carry-smoke` | `exec_command_wait_until_terminal_returns_exit_metadata`; `exec_command_tool_exposes_blocking_wait_parameters`; `write_stdin_tool_exposes_blocking_wait_parameters` |
| Turn-complete compaction count metadata | `codex.app-server-protocol-test` | `preserves_compaction_only_turn`; broader `TurnCompleteEvent` shape coverage in `codex-core`, `codex-exec`, and `codex-tui` tests keeps `compaction_events_in_turn` wired through downstream consumers |
| TUI queued slash recall + replay ordering | `core-carry-smoke` | `queued_inline_slash_command_runs_with_args_after_task_complete`; `alt_up_restores_most_recent_queued_slash_command` |
| Startup plugin sync bounded wait + completion-signal re-arm + abort checkpointing | `codex.core-startup-sync-targeted` | `startup_remote_plugin_sync_waits_for_late_prerequisites`; `startup_remote_plugin_sync_is_single_flight_before_prerequisites_exist`; `startup_remote_plugin_sync_uses_latest_config_and_auth_snapshot`; `startup_remote_plugin_sync_rearms_after_curated_repo_completion_signal_uses_latest_config_and_auth_snapshot`; `startup_remote_plugin_sync_signals_after_failed_curated_postprocessing`; `startup_remote_plugin_sync_aborts_in_flight_before_stamping_marker`; `startup_remote_plugin_sync_relaunches_immediately_after_abort_even_if_late_completion_signal_arrives` |
| Tool-context serialization for custom/function/abort outputs | `codex.core-context-serialization-targeted` | `custom_tool_calls_should_roundtrip_as_custom_outputs`; `function_payloads_remain_function_outputs`; `aborted_tool_output_serializes_*` |
| Persisted state spawn-edge lineage matches local `usage.sqlite` | `codex.state-spawn-lineage-contract-targeted` | `usage_spawn_lineage_matches_persisted_state_edge_for_child_thread` |
| Usage logging contracts in local `usage.sqlite` | `core-ledger-smoke` | `usage_logger_*` focused table-contract tests in `codex-rs/state/src/runtime/usage.rs` |
| Linux sandbox/core compile seam | `core-compile-smoke` | `cargo check -p codex-linux-sandbox -p codex-core --tests` |
| Postgres ledger ingest + copied-history/source-row regressions | `downstream-ledger-seam` | `ensure_schema.sh`; `ingest_codex_rollouts_to_postgres.sh`; `test_codex_copied_history_filter.sh`; `test_codex_source_row_identity.sh` |

The dedicated `tui_app_server` replay assertions live in source alongside this carry patch, but
the broader `codex-tui-app-server` lib test target still has unrelated compile drift on this
branch. Keep the preset green with the parser test, the exact `codex-tui` notification render
test, and `codex-tui-app-server` compile coverage until that test-target drift is repaired.

## Operator notes

- Prefer blocking joins over transcript polling when the tool surface supports
  them:
  - unified exec: `wait_until_terminal=true` on `exec_command` or
    `write_stdin`
  - sub-agents: inspect `list_agents(...)` first, then use `wait_agent(...)`
    with a real timeout only when you are actually blocked on the result; use
    `return_when=all` only when every child must be terminal
  - build-helper: `*_and_wait` variants or `build_status(...,
    wait_until_terminal=true)`
- Local authoritative usage facts are stored in
  `${CODEX_SQLITE_HOME:-$HOME/.codex}/usage.sqlite`.
- Inspect local usage tables quickly with:

```bash
sqlite3 "${CODEX_SQLITE_HOME:-$HOME/.codex}/usage.sqlite" '.tables'
```

- `downstream-ledger-seam` requires the sibling `agent-usage-ledger` repository
  and a reachable Postgres URL (`LLM_USAGE_DB_URL` or configured MCP Postgres
  `DATABASE_URI`).
- Keep the broad ladders out of the inner loop on this host:
  use the focused `codex.core-*targeted` presets first, then promote to
  `just core-test-smoke`, then `just core-test-progressive` only when you
  intentionally want a broader gate.
