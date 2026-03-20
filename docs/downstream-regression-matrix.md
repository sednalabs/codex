# Downstream Regression Matrix

This matrix maps live `carry/main` divergences to the smallest default test lane
that should fail if the behavior regresses.

## Core default path

Default build-helper preset for iterative validation:

- `codex.core-test` (`just core-test-progressive`)

Fast lanes used by `core-test-smoke`:

- `core-compile-smoke`
- `core-carry-smoke`
- `core-ledger-smoke`

## Divergence mapping

| Divergence | Guardrail lane | Primary checks |
| --- | --- | --- |
| Sub-agent override preservation across role reload | `core-carry-smoke` | `spawn_agent_preserves_explicit_model_override_across_role_reload`; `spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role`; `spawn_agent_role_overrides_requested_model_and_reasoning_settings` |
| Code-mode declaration formatting + namespaced tool metadata | `core-carry-smoke` | `code_mode_exports_all_tools_metadata_for_builtin_tools`; `code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools` |
| Unified-exec blocking wait semantics | `core-carry-smoke` | `exec_command_wait_until_terminal_returns_exit_metadata`; `exec_command_tool_exposes_blocking_wait_parameters`; `write_stdin_tool_exposes_blocking_wait_parameters` |
| TUI queued slash recall + replay ordering | `core-carry-smoke` | `queued_inline_slash_command_runs_with_args_after_task_complete`; `alt_up_restores_most_recent_queued_slash_command` |
| Usage logging contracts in local `usage.sqlite` | `core-ledger-smoke` | `usage_logger_*` focused table-contract tests in `codex-rs/state/src/runtime/usage.rs` |
| Linux sandbox/core compile seam | `core-compile-smoke` | `cargo check -p codex-linux-sandbox -p codex-core --tests` |
| Postgres ledger ingest + copied-history/source-row regressions | `downstream-ledger-seam` | `ensure_schema.sh`; `ingest_codex_rollouts_to_postgres.sh`; `test_codex_copied_history_filter.sh`; `test_codex_source_row_identity.sh` |

## Operator notes

- Prefer blocking joins over transcript polling when the tool surface supports
  them:
  - unified exec: `wait_until_terminal=true` on `exec_command` or
    `write_stdin`
  - sub-agents: `wait_agent(...)` with a real timeout when you are actually
    blocked on the result
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
