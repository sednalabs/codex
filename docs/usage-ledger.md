# LLM Usage Ledger

This repository normalizes three local usage sources into Postgres:

- Codex interactive rollout files under `~/.codex/sessions`
- Gemini MCP usage ledgers written by `GEMINI_MCP_USAGE_LEDGER_PATH`
- Gemini CLI interactive session files under `~/.gemini/tmp/*/chats/session-*.json`

The ingestion scripts live under [scripts/llm_usage](/home/grant/mmm/codex/scripts/llm_usage).

By default they target a dedicated `llm_usage` schema inside whatever Postgres database you point them at. In this repo, the intended production target is the same shared Postgres instance already used by Ops DAS and the Codex `postgres` MCP server, while keeping all ledger objects isolated under their own schema.

## What changed

The original implementation proved the data could be warehoused, but it had several prototype-grade behaviors:

- Postgres credentials were passed to `psql` on the command line.
- Gemini interactive rows were deduped by content hash rather than stable logical identity.
- Codex emitted completed turns only and ignored interrupted turns.
- Scheduled runs replayed full history with no persisted artifact state.
- Absolute workstation paths were stored directly in the shared database.

The current version hardens those paths.

## Design shape

The ledger now separates concerns more explicitly:

- `ensure_schema.sh` bootstraps or migrates the schema.
- Source-specific ingestors normalize facts from one source each.
- `ingest_all_to_postgres.sh` runs one schema bootstrap and then fan-outs to the source ingestors.
- `run_scheduled_ingest.sh` defaults to the lean recurring path (`--skip-schema`) and relies on the schema already existing.

The runtime Postgres calls now generate a temporary `PGSERVICEFILE` plus `PGPASSFILE` pair rather than passing the DSN directly in argv or exporting the password in the process environment.

## Privacy boundary

The ingestors still avoid warehousing full prompts, full chat bodies, or full transcript content.

The important privacy change is that filesystem paths are now redacted by default before they are inserted into Postgres:

- `source_path` becomes a short display label such as `rollout-...jsonl#abcd1234ef56`
- `project_path` and `cwd` become basename-plus-hash labels such as `codex#abcd1234ef56`
- `project_key` is the stable hashed key used for grouping
- `source_path_hash` stores the full path hash used for checkpoint identity

This keeps the reporting surface usable without leaking raw home-directory paths into the shared database.

## Tables and views

The schema now creates or maintains:

- `llm_usage.llm_ingest_runs`
- `llm_usage.llm_source_artifacts`
- `llm_usage.llm_usage_events`
- `llm_usage.llm_quota_events`
- `llm_usage.llm_session_usage_summary`
- `llm_usage.llm_session_tool_call_summary`
- `llm_usage.llm_latest_rate_limits`
- `llm_usage.llm_latest_quota_events`

### `llm_ingest_runs`

One row per source-ingest execution.

Important columns:

- `run_id`
- `script_name`
- `parser_version`
- `source_system`, `source_kind`
- `started_at`, `completed_at`, `status`
- `processed_artifacts`, `skipped_artifacts`, `generated_rows`
- `error_text`

### `llm_source_artifacts`

One row per discovered source artifact, keyed by `source_system`, `source_kind`, and `source_path_hash`.

Important columns:

- `source_path_hash`
- `source_path` redacted display label
- `source_size_bytes`, `source_mtime_epoch`
- `source_row_count`
- `parser_version`
- `last_ingest_run_id`, `last_ingested_at`
- `status`

This is the persisted checkpoint layer that lets recurring jobs skip unchanged files and resume append-only ledgers incrementally.

### `llm_usage_events`

The main normalized fact table.

Important columns:

- `logical_key`: stable logical identity used for upsert semantics
- `record_hash`: content fingerprint of the normalized row
- `parser_version`
- `ingest_run_id`
- `source_system`, `source_kind`
- `source_path`, `source_path_hash`, `source_row_id`
- `session_id`, `turn_id`
- `project_key`, `project_path`, `cwd`
- `model_requested`, `model_used`
- `ok`, `event_status`, `error_category`
- `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_tokens`, `tool_tokens`, `total_tokens`
- `tool_call_count`, `tool_call_count_known_event_count`, `tool_call_count_unknown_event_count`, `tool_call_count_coverage` in `llm_session_usage_summary` for explicit tool-call activity accounting
- `cumulative_total_tokens` when the source exposes cumulative counters
- Codex rate-limit snapshot fields

### `llm_quota_events`

Normalized quota/rate-limit evidence.

Important columns:

- `logical_key`
- `record_hash`
- `parser_version`
- `ingest_run_id`
- `source_system`, `source_kind`
- `source_path`, `source_path_hash`, `source_row_id`
- `session_id`
- `project_key`, `project_path`
- `model_used`, `tool_name`
- `error_message`
- `reset_after_text`, `reset_after_seconds`

## Source-specific normalization rules

### Codex interactive

- Emits one row per contiguous turn/model segment for completed and aborted turns. Older historical rows may still appear as legacy `interactive_turn` fallback rows until reingested.
- Turn boundaries come from explicit `task_started`, `task_complete`, and `turn_aborted` events with the same `turn_id`.
- Segment boundaries are created whenever the active model/provider changes within a turn. Exact `token_count.model_used` / `token_count.provider` metadata wins when present; otherwise ingest falls back to turn-context and pre-turn model inference for model-switch compaction.
- Segment tokens prefer summed `token_count.info.last_token_usage` within the segment.
- The latest `token_count.info.total_token_usage` is still kept as cumulative audit metadata on the final segment row.
- Rate-limit snapshots are attached from the latest in-turn `token_count.rate_limits` payload.

This closes the earlier undercount where interrupted turns disappeared entirely and fixes mixed-model turns being flattened onto a single model.

### Gemini MCP

- Emits one row per usage-ledger line.
- Stable logical identity prefers `invocation_id`, with line-number fallback for sparse rows.
- Uses `llm_source_artifacts.source_row_count` as the append-only checkpoint for incremental ingest.
- Quota or rate-limit failures are copied into `llm_quota_events` even when no reset countdown is exposed.

### Gemini interactive CLI

- Emits one row per persisted message with a `tokens` object.
- Message identity is stable by `session_id` plus `message.id`, with message-index fallback.
- Quota rows are emitted per tool call that exposes `quota will reset after ...` text.
- Quota duration parsing now tolerates whitespace and punctuation before normalizing into `reset_after_seconds`.
- Source rows upsert on logical identity rather than raw-content hash, which prevents double counting when session JSON gains more detail over time.

## Configuration

Environment variables:

- `LLM_USAGE_DB_URL`: optional explicit Postgres connection string
- `LLM_USAGE_DB_SCHEMA`: target schema name. Defaults to `llm_usage`
- `CODEX_CONFIG_TOML`: optional override for the Codex config file path. Defaults to `~/.codex/config.toml`
- `CODEX_USAGE_ROLLOUTS_ROOT`: optional override for Codex rollout discovery
- `GEMINI_CLI_STATE_ROOT`: optional override for Gemini interactive session discovery
- `GEMINI_MCP_USAGE_LEDGER_PATH`: optional override for the Gemini MCP usage ledger
- `LLM_USAGE_LOG_DIR`: optional override for the scheduled ingest log directory
- `LLM_USAGE_LOG_FILE`: optional override for the scheduled ingest log file

If `LLM_USAGE_DB_URL` is not set, the scripts fall back to the `DATABASE_URI` configured under `[mcp_servers.postgres.env]` in `~/.codex/config.toml`.

## Running the ledger

Bootstrap or migrate the schema once:

```bash
./scripts/llm_usage/ensure_schema.sh --schema llm_usage
```

Run the full ingest bundle:

```bash
./scripts/llm_usage/ingest_all_to_postgres.sh --schema llm_usage
```

Run the full bundle without touching schema:

```bash
./scripts/llm_usage/ingest_all_to_postgres.sh --schema llm_usage --skip-schema
```

Run just one source:

```bash
./scripts/llm_usage/ingest_codex_rollouts_to_postgres.sh --schema llm_usage
./scripts/llm_usage/ingest_gemini_mcp_usage_to_postgres.sh --schema llm_usage
./scripts/llm_usage/ingest_gemini_cli_sessions_to_postgres.sh --schema llm_usage
```

All ingestors also support `--dry-run`, which generates normalized rows and prints counts without writing to Postgres.

```bash
./scripts/llm_usage/ingest_all_to_postgres.sh --schema llm_usage --dry-run
```

## Reporting

Use the reporting command for operator summaries:

```bash
./scripts/llm_usage/report_usage_summary.sh --report all --days 30 --limit 10
./scripts/llm_usage/report_usage_summary.sh --report session --days 7 --limit 20
./scripts/llm_usage/report_usage_summary.sh --report model --days 30 --limit 20
./scripts/llm_usage/report_usage_summary.sh --report provider --days 30 --limit 20
./scripts/llm_usage/report_usage_summary.sh --report cost --days 30 --limit 20
./scripts/llm_usage/report_usage_summary.sh --report cost --cost-view observed --days 30 --limit 20
./scripts/llm_usage/report_usage_summary.sh --report reconciliation
./scripts/llm_usage/report_usage_summary.sh --report freshness
```

The report command also supports a `reconciliation` mode that groups rows by `parser_version`, `source_system`, `source_kind`, and `event_status` so schema migrations and parser upgrades are easy to audit later.

Cost reporting now defaults to the billing-canonical view. For Codex, legacy `interactive_turn` rows are canonicalized one billable row per `turn_id`, while newer `interactive_turn_segment` rows remain one billable row per segment so mixed-model turns price against the actual model spans that generated usage. Replay-safe `event_ts` and pricing basis stay pinned to the first-seen observation so resumed sessions do not shift spend into later hours. Use `--cost-view observed` when you explicitly want the raw observed rows instead of billing-safe totals. The canonical view also exposes `billing_origin_*`, `billing_latest_*`, `billing_selected_*`, and `pricing_basis_event_ts` columns so replay lineage stays inspectable without corrupting time-based cost reports.

The `session` view now includes:

- `provider`
- `succeeded_event_count`
- `aborted_event_count`
- `failed_event_count`

That makes interrupted-turn behavior visible instead of silently disappearing behind totals.

## Scheduled ingestion

To install a user-level systemd timer that runs the ingest every 15 minutes:

```bash
./scripts/llm_usage/install_user_timer.sh --interval-minutes 15 --schema llm_usage
```

That installer writes:

- `~/.config/systemd/user/codex-llm-usage-ingest.service`
- `~/.config/systemd/user/codex-llm-usage-ingest.timer`
- `~/.config/codex/llm-usage-ingest.env`

The installer now:

- creates the env file with restrictive permissions
- attempts a schema bootstrap during installation
- leaves recurring runs on the leaner `--skip-schema` path

The scheduled runner:

- uses the same DB resolution rules as the manual ingest scripts
- acquires a non-blocking `flock` lock to avoid overlapping runs
- hardens the log file permissions
- appends logs to `~/.local/state/codex/llm-usage/scheduled-ingest.log` by default

To install the unit files without enabling the timer immediately:

```bash
./scripts/llm_usage/install_user_timer.sh --no-enable
```

To test the scheduled path once without waiting for the timer:

```bash
./scripts/llm_usage/run_scheduled_ingest.sh --schema llm_usage --dry-run
```

If you explicitly want a scheduled run to apply schema first:

```bash
./scripts/llm_usage/run_scheduled_ingest.sh --schema llm_usage --ensure-schema
```

## Example SQL queries

Per-session totals with success/abort/failure counts:

```sql
select
  source_system,
  source_kind,
  session_id,
  project_key,
  model,
  started_at,
  last_event_at,
  session_total_tokens,
  tool_call_count,
  tool_call_count_coverage,
  succeeded_event_count,
  aborted_event_count,
  failed_event_count
from llm_usage.llm_session_usage_summary
order by last_event_at desc;
```

Daily token burn by source/model:

```sql
select
  date_trunc('day', event_ts) as day,
  source_system,
  source_kind,
  coalesce(model_used, model_requested, 'unknown') as model,
  sum(coalesce(total_tokens, 0)) as total_tokens
from llm_usage.llm_usage_events
group by 1, 2, 3, 4
order by day desc, total_tokens desc;
```

Recent ingest runs:

```sql
select
  run_id,
  script_name,
  source_system,
  source_kind,
  status,
  started_at,
  completed_at,
  processed_artifacts,
  skipped_artifacts,
  generated_rows,
  error_text
from llm_usage.llm_ingest_runs
order by started_at desc
limit 20;
```
