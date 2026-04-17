# Downstream Native Tool Surface Matrix

This matrix compares the product-native tool surface on the downstream branch
(historically `carry/main`, now `main`) against `upstream/main`.

It intentionally excludes session-only developer wrappers such as
`multi_tool_use.parallel`; those are runtime conveniences, not fork
divergences.

Last reviewed: `2026-04-17`

Review baseline:

- `upstream/main`: `fe7c959e90d46abb8311e4a0b369e6cb32bf337e`
- `main` (`origin/main`): `88b12a0e145af4533b58cf1a8b67369795eb7786`

| Surface | `upstream/main` | `main` | Live divergence? | Guardrails |
| --- | --- | --- | --- | --- |
| `exec_command` | PTY execution plus `cmd`, `workdir`, `shell`, `tty`, `yield_time_ms`, `max_output_tokens`, `login`, and approval parameters | Upstream fields plus `wait_until_terminal`, `max_wait_ms`, and `heartbeat_interval_ms` | yes | `exec_command_wait_until_terminal_returns_exit_metadata`; `exec_command_tool_exposes_blocking_wait_parameters` |
| `write_stdin` | `session_id`, `chars`, `yield_time_ms`, `max_output_tokens` | Upstream fields plus `wait_until_terminal`, `max_wait_ms`, and `heartbeat_interval_ms`; empty `chars` can be used with `wait_until_terminal` | yes | `write_stdin_tool_exposes_blocking_wait_parameters` |
| `spawn_agent` request semantics | Explicit `model` and `reasoning_effort` overrides are supported | Same base support, plus documented role-lock precedence for `model`, `model_provider`, `model_reasoning_effort`, `model_verbosity`, and `model_reasoning_summary`, with explicit guidance that active-profile/role overrides retain priority when they provide these fields. | yes | `spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role`; `spawn_agent_role_overrides_requested_model_and_reasoning_settings` |
| `spawn_agent` response schema | `agent_id`, `nickname` | Full inventory item: `agent_id`, `nickname`, `role`, `status`, `identity_source`, `effective_model`, `effective_reasoning_effort`, `effective_model_provider_id` | yes | `codex.core-subagent-surface-targeted` |
| `list_agents` | `MultiAgentV2` live inventory tool when the feature is enabled; path-scoped rows include `agent_name`, `agent_status`, and `last_task_message` | Always available on the downstream collab surface, using the upstream live handler shape for the cheap in-memory view in both v1 and v2, plus `has_active_subagents` and `active_subagent_count` so callers can notice when a row still owns active nested work | yes | `multi_agent_v2_list_agents_returns_completed_status_and_last_task_message`; `multi_agent_v2_list_agents_filters_by_relative_path_prefix`; `multi_agent_v2_list_agents_omits_closed_agents`; `multi_agent_v2_list_agents_flags_active_descendants`; `test_build_specs_collab_tools_enabled` |
| `inspect_agent_tree` | absent | Compact nested subtree inspection with `live`/`stale`/`all` scope, optional branch filters via `agent_roots`, per-row child counts, and bounded depth/row limits so agents can inspect descendant state without dumping large inventories | yes | `inspect_agent_tree_defaults_to_current_subtree_for_live_nested_agents`; `inspect_agent_tree_can_mix_live_and_stale_descendants`; `inspect_agent_tree_filters_to_requested_agent_branches`; `inspect_agent_tree_tool_exposes_scope_and_compact_tree_fields` |
| `wait_agent` arguments and output | Arguments: `ids`, `timeout_ms`; output: `status`, `timed_out` | Adds `return_when=any|all`; output also includes `requested_ids`, `pending_ids`, and `completion_reason` | yes | `wait_agent_allows_return_when_any_and_returns_on_first_final_status`; `wait_agent_allows_return_when_all_and_returns_only_when_all_are_final`; `test_wait_agent_tool_schema_and_description_document_return_when` |
| `apply_patch` | Freeform patch grammar | Same freeform patch grammar | no | `prompt.md` guidance and apply-patch handler tests |
| `js_repl` | Same freeform JavaScript grammar when the feature flag is enabled | Same freeform JavaScript grammar when the feature flag is enabled | no | `docs/js_repl.md`; `js_repl_*` runtime tests |

Notes:

- `main` keeps the higher-signal operator surfaces where the tool contract
  absorbs waiting or inventory inspection instead of forcing transcript polling.
- The `spawn_agent` divergence is narrower than the historical carry commit
  titles suggest: upstream already absorbed the base override capability, while
  carry adds stronger precedence guarantees plus richer returned metadata.
- The raw `spawn_agent` response intentionally does not expose
  `model_reasoning_summary`; that remains covered by the focused
  `codex.core-subagent-surface-targeted` slice so surface drift can be checked
  without running the broader ladders.
- `list_agents` used to be described here as a downstream-only richer inventory
  surface, but the live handler itself is already the upstream `MultiAgentV2`
  path. The downstream divergence is now that this cheap live view is available
  across both collab surfaces instead of only behind the upstream v2 feature.
- `inspect_agent_tree` is the deliberate downstream observability surface for
  compact nested inspection, stale-descendant visibility, and branch-focused
  filtering via `agent_roots`, rather than overloading `list_agents` with
  provenance-heavy output by default.
- `apply_patch` and `js_repl` are included as control rows so future audits do
  not misclassify them as carry-only behavior.
