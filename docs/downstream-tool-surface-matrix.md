# Downstream Native Tool Surface Matrix

This matrix compares the product-native tool surface on the downstream branch
(historically `carry/main`, now `main`) against `upstream/main`.

It intentionally excludes session-only developer wrappers such as
`multi_tool_use.parallel`; those are runtime conveniences, not fork
divergences.

Last reviewed: `2026-03-21`

Review baseline:

- `upstream/main`: `e5f4d1fef59a3309339394575052c7cc1fff0996`
- `carry/main`: `5d474e652d91c7f371a28ad2069cc51a1c5b9ee8`

| Surface | `upstream/main` | `carry/main` | Live divergence? | Guardrails |
| --- | --- | --- | --- | --- |
| `exec_command` | PTY execution plus `cmd`, `workdir`, `shell`, `tty`, `yield_time_ms`, `max_output_tokens`, `login`, and approval parameters | Upstream fields plus `wait_until_terminal`, `max_wait_ms`, and `heartbeat_interval_ms` | yes | `exec_command_wait_until_terminal_returns_exit_metadata`; `exec_command_tool_exposes_blocking_wait_parameters` |
| `write_stdin` | `session_id`, `chars`, `yield_time_ms`, `max_output_tokens` | Upstream fields plus `wait_until_terminal`, `max_wait_ms`, and `heartbeat_interval_ms`; empty `chars` can be used with `wait_until_terminal` | yes | `write_stdin_tool_exposes_blocking_wait_parameters` |
| `spawn_agent` request semantics | Explicit `model` and `reasoning_effort` overrides are supported | Same base support, plus documented role-lock precedence for `model`, `model_provider`, `model_reasoning_effort`, `model_verbosity`, and `model_reasoning_summary`, with explicit guidance that active-profile/role overrides retain priority when they provide these fields. | yes | `spawn_agent_preserves_explicit_model_override_across_role_reload`; `spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role`; `spawn_agent_role_overrides_requested_model_and_reasoning_settings` |
| `spawn_agent` response schema | `agent_id`, `nickname` | Full inventory item: `agent_id`, `nickname`, `role`, `status`, `identity_source`, `effective_model`, `effective_reasoning_effort`, `effective_model_provider_id` | yes | `spawn_agent_preserves_explicit_model_override_across_role_reload`; `codex.core-subagent-surface-targeted` |
| `list_agents` | `MultiAgentV2` live inventory tool when the feature is enabled; path-scoped rows include `agent_name`, `agent_status`, and `last_task_message` | Same live upstream-v2 handler path; no separate downstream `list_agents` handler remains after dead-carry cleanup | no | `multi_agent_v2_list_agents_returns_completed_status_and_last_task_message`; `multi_agent_v2_list_agents_filters_by_relative_path_prefix`; `multi_agent_v2_list_agents_omits_closed_agents` |
| `wait_agent` arguments and output | Arguments: `ids`, `timeout_ms`; output: `status`, `timed_out` | Adds `return_when=any|all`; output also includes `requested_ids`, `pending_ids`, and `completion_reason` | yes | `wait_agent_allows_return_when_any_and_returns_on_first_final_status`; `wait_agent_allows_return_when_all_and_returns_only_when_all_are_final`; `test_wait_agent_tool_schema_and_description_document_return_when` |
| `apply_patch` | Freeform patch grammar | Same freeform patch grammar | no | `prompt.md` guidance and apply-patch handler tests |
| `js_repl` | Same freeform JavaScript grammar when the feature flag is enabled | Same freeform JavaScript grammar when the feature flag is enabled | no | `docs/js_repl.md`; `js_repl_*` runtime tests |

Notes:

- `carry/main` keeps the higher-signal operator surfaces where the tool contract
  absorbs waiting or inventory inspection instead of forcing transcript polling.
- The `spawn_agent` divergence is narrower than the historical carry commit
  titles suggest: upstream already absorbed the base override capability, while
  carry adds stronger precedence guarantees plus richer returned metadata.
- The raw `spawn_agent` response intentionally does not expose
  `model_reasoning_summary`; that remains covered by the focused
  `codex.core-subagent-surface-targeted` slice so surface drift can be checked
  without running the broader ladders.
- `list_agents` used to be described here as a downstream-only richer inventory
  surface, but the live handler is already the upstream `MultiAgentV2` path.
  The remaining inventory divergence now lives in legacy spawn metadata and
  internal control-plane plumbing rather than the live `list_agents` tool.
- `apply_patch` and `js_repl` are included as control rows so future audits do
  not misclassify them as carry-only behavior.
