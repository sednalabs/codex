# Downstream Native Tool Surface Matrix

This matrix compares the product-native tool surface on `carry/main` against
`upstream/main`.

It intentionally excludes session-only developer wrappers such as
`multi_tool_use.parallel`; those are runtime conveniences, not fork
divergences.

Last reviewed: `2026-03-21`

Review baseline:

- `upstream/main`: `e5f4d1fef59a5bef16ae768e3ef7d4c5dc526c9d`
- `carry/main`: `a8d9e4ea8033826104e1cfec8b737da0203029df`

| Surface | `upstream/main` | `carry/main` | Live divergence? | Guardrails |
| --- | --- | --- | --- | --- |
| `exec_command` | PTY execution plus `cmd`, `workdir`, `shell`, `tty`, `yield_time_ms`, `max_output_tokens`, `login`, and approval parameters | Upstream fields plus `wait_until_terminal`, `max_wait_ms`, and `heartbeat_interval_ms` | yes | `exec_command_wait_until_terminal_returns_exit_metadata`; `exec_command_tool_exposes_blocking_wait_parameters` |
| `write_stdin` | `session_id`, `chars`, `yield_time_ms`, `max_output_tokens` | Upstream fields plus `wait_until_terminal`, `max_wait_ms`, and `heartbeat_interval_ms`; empty `chars` can be used with `wait_until_terminal` | yes | `write_stdin_tool_exposes_blocking_wait_parameters` |
| `spawn_agent` request semantics | Explicit `model` and `reasoning_effort` overrides are supported | Same base support, plus documented role-lock precedence for `model`, `model_provider`, `model_reasoning_effort`, and `model_verbosity` | yes | `spawn_agent_preserves_explicit_model_override_across_role_reload`; `spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role`; `spawn_agent_role_overrides_requested_model_and_reasoning_settings` |
| `spawn_agent` response schema | `agent_id`, `nickname` | Full inventory item: `agent_id`, `nickname`, `role`, `status`, `identity_source`, `effective_model`, `effective_reasoning_effort`, `effective_model_provider_id` | yes | `spawn_agent_preserves_explicit_model_override_across_role_reload` |
| `list_agents` | absent | Direct-child inventory tool with live status, provenance (`identity_source`), and effective-setting metadata | yes | `list_agents_returns_direct_children_with_live_inventory`; `list_agents_respects_optional_id_filter`; `list_agents_id_filter_returns_not_found_entries_for_missing_or_invisible_ids` |
| `wait_agent` arguments and output | Arguments: `ids`, `timeout_ms`; output: `status`, `timed_out` | Adds `return_when=any|all`; output also includes `requested_ids`, `pending_ids`, and `completion_reason` | yes | `wait_agent_allows_return_when_any_and_returns_on_first_final_status`; `wait_agent_allows_return_when_all_and_returns_only_when_all_are_final`; `test_wait_agent_tool_schema_and_description_document_return_when` |
| `apply_patch` | Freeform patch grammar | Same freeform patch grammar | no | `prompt.md` guidance and apply-patch handler tests |
| `js_repl` | Same freeform JavaScript grammar when the feature flag is enabled | Same freeform JavaScript grammar when the feature flag is enabled | no | `docs/js_repl.md`; `js_repl_*` runtime tests |

Notes:

- `carry/main` keeps the higher-signal operator surfaces where the tool contract
  absorbs waiting or inventory inspection instead of forcing transcript polling.
- The `spawn_agent` divergence is narrower than the historical carry commit
  titles suggest: upstream already absorbed the base override capability, while
  carry adds stronger precedence guarantees plus richer returned metadata.
- `apply_patch` and `js_repl` are included as control rows so future audits do
  not misclassify them as carry-only behavior.
