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
- `core-carry-smoke` now includes the `codex-tui`
  replayed-queue and selected-agent footer regressions, so
  downstream interactive behavior fails during the PR smoke pass.
- `core-ledger-smoke` now includes the `usage.sqlite` cleanup, turn-reset, and
  restart-lineage regressions, so downstream accounting behavior
  fails during the same smoke pass.

Focused micro-slices for iterative work on the current carry seams:

- `codex.core-startup-sync-targeted`
- `codex.core-subagent-surface-targeted`
- `codex.core-subagent-notification-contract-targeted`
- `codex.core-subagent-notification-visibility-targeted`
- `codex.core-multi-agent-orchestration-targeted`
- `codex.core-persisted-subagent-descendants-targeted`
- `codex.core-context-serialization-targeted`
- `codex.core-attestation-targeted`
- `codex.state-spawn-lineage-contract-targeted`
- `codex.downstream-docs-check`

Industrial validation workflow reference:

- `docs/industrial_carry_workflow.md`

Focused lane used for protocol/event-history seams:

- `codex.app-server-protocol-test`

GitHub Actions lane naming (`.github/workflows/sedna-heavy-tests.yml`):

- Workflow shard names intentionally mirror this document's guardrail lane
  identifiers where possible.
- `workflow_dispatch` input `lane` uses these lane IDs directly (`all` runs
  every shard).
- Where the local `justfile` recipe name differs, workflow shards still retain
  the docs lane ID and only translate at command execution time.

## GitHub trigger policy

- `rust-ci.yml` is the default PR fail-fast workflow.
  - It runs on every `pull_request` update and `merge_group`.
  - `CI results (required)` is the single required gate, and it enforces
    `Downstream smoke` when downstream-facing paths change.
  - PR and merge-group matrix jobs fail fast and cap their runner fan-out so a
    known-bad head stops tying up the shared GitHub Actions pool.
  - Protected-branch pushes still keep fuller in-lane failure signal where that
    is more valuable than early cancellation.
- `sedna-heavy-tests.yml` is the downstream-heavy lane workflow.
  - On ordinary PR updates, it auto-selects only the heavy lanes implied by the
    changed path class.
  - Non-doc heavy runs must clear `core-test-smoke` before the broader lane
    matrix fans out, and the heavy matrix itself is capped and fail-fast on PRs.
  - Changes to workflow wiring or the `justfile` run the smoke gate plus a small
    representative workflow-validation lane set instead of promoting the PR to
    the full heavy matrix.
  - Applying the `ci:heavy` label promotes the PR to the full heavy matrix.
  - `merge_group`, nightly schedule, `push` to `main`, and
    `workflow_dispatch lane=all` all run the full heavy matrix.
  - `workflow_dispatch` can still run one named lane when a single shard is the
    right debugging tool.
- `validation-lab.yml` is the dispatch-only remote validation surface.
  - Use it for scratch refs, integration refs, orphan-branch experiments, and
    non-PR seam validation.
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
- `sedna-branch-build.yml` is the preview artifact path.
  - It is manual-dispatch only.
  - Treat it as artifact validation, not the primary downstream correctness
    gate.

## Divergence mapping

| Divergence | Guardrail lane | Primary checks |
| --- | --- | --- |
| Sub-agent override preservation across role reload | `core-carry-smoke` | `spawn_agent_preserves_explicit_model_override_across_role_reload`; `spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role`; `spawn_agent_role_overrides_requested_model_and_reasoning_settings` |
| Sub-agent surface contract (`spawn_agent`/`list_agents` omit raw `model_reasoning_summary`, `list_agents(include_descendants=true)` surfaces persisted subtree edge status, and model-visible tool specs include the carried sub-agent inventory surfaces) | `codex.core-subagent-surface-targeted` | `spawn_agent_preserves_explicit_model_override_across_role_reload`; `list_agents_returns_direct_children_with_live_inventory`; `list_agents_include_descendants_reports_persisted_open_and_closed_descendants`; `test_build_specs_multi_agent_v2_uses_task_names_and_hides_resume`; `test_gpt_5_1_defaults`; `test_codex_5_1_mini_defaults`; `test_gpt_5_1_codex_max_unified_exec_web_search`; `test_full_toolset_specs_for_gpt5_codex_unified_exec_web_search`; `test_gpt_5_1_codex_max_defaults` |
| Core-side sub-agent notification contract (`codex-core`) | `codex.core-subagent-notification-contract-targeted` | `format_subagent_notification_message_round_trips_completed_status`; `classifies_memory_excluded_fragments`; `drop_last_n_user_turns_ignores_session_prefix_user_messages`; `serializes_memory_rollout_with_agents_removed_but_environment_kept` |
| Sub-agent completion-notification parser + TUI render surface (`protocol` + `tui`) | `codex.core-subagent-notification-visibility-targeted` | `parse_subagent_notification_response_item_*`; `raw_response_subagent_notification_renders_history` |
| Sub-agent inventory + blocking join surface | `codex.core-multi-agent-orchestration-targeted` | `list_agents_returns_direct_children_with_live_inventory`; `wait_agent_allows_return_when_any_and_returns_on_first_final_status`; `wait_agent_allows_return_when_all_and_returns_only_when_all_are_final`; `spawn_wait_and_list_agents_tool_descriptions_have_guidance_updates` |
| Persisted sub-agent descendant status across close + rollout resume | `codex.core-persisted-subagent-descendants-targeted` | `persisted_spawn_descendants_reflect_closed_status` |
| Code-mode declaration formatting + namespaced tool metadata | `core-carry-smoke` | `code_mode_exports_all_tools_metadata_for_builtin_tools`; `code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools` |
| Unified-exec blocking wait semantics | `core-carry-smoke` | `exec_command_wait_until_terminal_returns_exit_metadata`; `exec_command_tool_exposes_blocking_wait_parameters`; `write_stdin_tool_exposes_blocking_wait_parameters` |
| Turn-complete compaction count metadata | `codex.app-server-protocol-test` | `preserves_compaction_only_turn`; broader `TurnCompleteEvent` shape coverage in `codex-core`, `codex-exec`, and `codex-tui` tests keeps `compaction_events_in_turn` wired through downstream consumers |
| TUI queued slash recall + replay ordering | `core-carry-smoke` | `queued_inline_slash_command_runs_with_args_after_task_complete`; `alt_up_restores_most_recent_queued_slash_command` |
| Startup plugin sync bounded wait + completion-signal re-arm + abort checkpointing | `codex.core-startup-sync-targeted` | `startup_remote_plugin_sync_waits_for_late_prerequisites`; `startup_remote_plugin_sync_is_single_flight_before_prerequisites_exist`; `startup_remote_plugin_sync_uses_latest_config_and_auth_snapshot`; `startup_remote_plugin_sync_rearms_after_curated_repo_completion_signal_uses_latest_config_and_auth_snapshot`; `startup_remote_plugin_sync_signals_after_failed_curated_postprocessing`; `startup_remote_plugin_sync_aborts_in_flight_before_stamping_marker`; `startup_remote_plugin_sync_relaunches_immediately_after_abort_even_if_late_completion_signal_arrives` |
| Tool-context serialization for custom/function/abort outputs | `codex.core-context-serialization-targeted` | `custom_tool_calls_should_roundtrip_as_custom_outputs`; `function_payloads_remain_function_outputs`; `aborted_tool_output_serializes_*` |
| Phase-2 attestation contract | `codex.core-attestation-targeted` | `consolidation_artifacts_ready_rejects_*`; `global_phase2_attestation_requirement_is_root_scoped` |
| Persisted state spawn-edge lineage matches local `usage.sqlite` | `codex.state-spawn-lineage-contract-targeted` | `usage_spawn_lineage_matches_persisted_state_edge_for_child_thread` |
| Usage logging contracts in local `usage.sqlite` | `core-ledger-smoke` | `usage_logger_*` focused table-contract tests in `codex-rs/state/src/runtime/usage.rs` |
| Linux sandbox/core compile seam | `core-compile-smoke` | `cargo check -p codex-linux-sandbox -p codex-core --tests` |
| Postgres ledger ingest + copied-history/source-row regressions | `downstream-ledger-seam` | `ensure_schema.sh`; `ingest_codex_rollouts_to_postgres.sh`; `test_codex_copied_history_filter.sh`; `test_codex_source_row_identity.sh` |

The replay assertions that used to live under `tui_app_server` now ride on the
cut-over `codex-tui` app tests. Keep the preset green with the parser test and
the exact `codex-tui` render/app checks rather than carrying compile coverage
for a removed crate path.

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

- `downstream-ledger-seam` reads the first-party local ledger at
  `${CODEX_SQLITE_HOME:-$HOME/.codex}/usage.sqlite`; external Postgres wiring
  is optional downstream analysis infrastructure, not a prerequisite for the
  seam itself.
- Keep the broad ladders out of the inner loop on this host:
  use the focused `codex.core-*targeted` presets first, then a bounded
  `validation-lab` `profile=frontier` harvest when the baseline is trustworthy,
  then promote to `just core-test-smoke`, then `just core-test-progressive`
  only when you intentionally want a broader gate.
- Remote-first validation should follow this ladder:
  - tiny local checks first
  - `validation-lab` `profile=smoke` or `profile=targeted` on scratch or
    integration refs
  - `validation-lab` `profile=frontier` when you want a bounded queue of likely
    next blockers without paying for a broad checkpoint
  - `validation-lab` `profile=broad` or `profile=full` only when the question
    genuinely spans multiple seams
  - ordinary PR checks once the branch is ready for promotion semantics
  - preview/buildability validation only when the question is "can we ship or
    hand someone a binary?" rather than "did this seam regress?"
- For carry work, keep the operational tracker slice-shaped:
  - one question
  - one commit
  - one exact validation run
  - one recorded conclusion before widening the scope
- Full builds are not the default inner-loop validator here. Treat them as
  promotion/buildability checkpoints on `main`, merge-group, alpha/release
  candidates, or explicit artifact requests.
