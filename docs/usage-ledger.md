# LLM Usage Ledger

This repository can normalize three local usage sources into Postgres:

- Codex interactive rollout files under `~/.codex/sessions`
- Gemini MCP usage ledgers written by `GEMINI_MCP_USAGE_LEDGER_PATH`
- Gemini CLI interactive session files under `~/.gemini/tmp/*/chats/session-*.json`

The ingestion scripts live under [scripts/llm_usage](/home/grant/mmm/codex/scripts/llm_usage).

By default they target a dedicated `llm_usage` schema inside whatever Postgres database you point them at. In this repo, the intended production target is the same shared Postgres instance already used by Ops DAS and the Codex `postgres` MCP server, while keeping all ledger objects isolated under their own schema.

## Why this shape

The goal is to avoid inventing a second runtime ledger when durable artifacts already exist:

- Codex rollouts are the authoritative per-thread event log.
- Gemini MCP already emits an append-only JSONL usage ledger.
- Gemini CLI persists interactive session JSON files with per-message token usage.

That gives one Postgres-backed warehouse without having to modify the runtime protocols first.

## Privacy boundary

The ingestors normalize usage and quota metadata only.

They intentionally do not warehouse full prompts, full chat bodies, or full transcript content. The `raw` JSONB column keeps source-specific operational metadata that helps with debugging and attribution, but it is deliberately trimmed so the ledger remains an authoritative usage store rather than a second transcript archive.

## Tables

The schema script creates:

- `llm_usage.llm_usage_events`
- `llm_usage.llm_quota_events`
- `llm_usage.llm_session_usage_summary`
- `llm_usage.llm_latest_rate_limits`
- `llm_usage.llm_latest_quota_events`

`llm_usage_events` is the main normalized fact table.

Key columns:

- `source_system`: `codex` or `gemini`
- `source_kind`: `interactive_turn`, `mcp_tool_call`, or `interactive_message`
- `session_id`
- `turn_id`
- `project_key`
- `project_path`
- `model_used`
- `input_tokens`, `output_tokens`, `cached_input_tokens`, `reasoning_tokens`, `tool_tokens`, `total_tokens`
- `cumulative_total_tokens` when the source exposes a cumulative counter
- Codex rate-limit snapshot fields (`primary_used_percent`, `primary_resets_at`, etc.)

`llm_quota_events` captures two kinds of quota evidence today:

- Gemini interactive tool failures that include reset text such as `Your quota will reset after 21h52m22s`
- Gemini MCP ledger rows that classify the call as `quota_or_rate_limit`

## Source-specific normalization rules

Codex interactive:

- One row is emitted per completed turn.
- Turn boundaries come from explicit `task_started` / `task_complete` events with the same `turn_id`.
- Per-turn tokens are computed as deltas from the latest in-turn cumulative `token_count.info.total_token_usage` snapshot.
- `turn_context` is used only as metadata enrichment, not to define turn boundaries.
- Codex rate-limit snapshots are attached from the latest in-turn `token_count.rate_limits` payload.

Gemini MCP:

- One row is emitted per usage-ledger line.
- Session identity prefers `resolved_session_id`, then `session_id`, then `invocation_id`.
- The ingestor tolerates sparse ledger rows where token counts are unavailable.
- MCP quota/rate-limit failures are copied into `llm_quota_events` even when no reset countdown is exposed.

Gemini interactive CLI:

- One row is emitted per persisted message with a `tokens` object.
- Per-message token values come directly from the session JSON.
- Quota rows are extracted from tool-call failures whose `resultDisplay` includes `quota will reset after ...`.
- The reset duration text is also normalized into `reset_after_seconds`.

## Configuration

Environment variables:

- `LLM_USAGE_DB_URL`: optional explicit Postgres connection string
- `LLM_USAGE_DB_SCHEMA`: target schema name. Defaults to `llm_usage`.
- `CODEX_CONFIG_TOML`: optional override for the Codex config file path. Defaults to `~/.codex/config.toml`.
- `CODEX_USAGE_ROLLOUTS_ROOT`: optional override for Codex rollout discovery
- `GEMINI_CLI_STATE_ROOT`: optional override for Gemini interactive session discovery
- `GEMINI_MCP_USAGE_LEDGER_PATH`: optional override for the Gemini MCP usage ledger
- `LLM_USAGE_LOG_DIR`: optional override for the scheduled ingest log directory
- `LLM_USAGE_LOG_FILE`: optional override for the scheduled ingest log file

If `LLM_USAGE_DB_URL` is not set, the scripts fall back to the `DATABASE_URI` configured under `[mcp_servers.postgres.env]` in `~/.codex/config.toml`.

## Running the ingestors

To run the full bundle:

```bash
./scripts/llm_usage/ingest_all_to_postgres.sh --schema llm_usage
```

To force an explicit database URL instead of using Codex config fallback:

```bash
export LLM_USAGE_DB_URL='postgresql://user:pass@host:5432/dbname'
./scripts/llm_usage/ingest_all_to_postgres.sh \
  --db-url "$LLM_USAGE_DB_URL" \
  --schema llm_usage
```

If you only want one source:

```bash
./scripts/llm_usage/ingest_codex_rollouts_to_postgres.sh --schema llm_usage
./scripts/llm_usage/ingest_gemini_mcp_usage_to_postgres.sh --schema llm_usage
./scripts/llm_usage/ingest_gemini_cli_sessions_to_postgres.sh --schema llm_usage
```

All ingestors also support `--dry-run`, which generates the normalized rows and prints counts without writing to Postgres.

```bash
./scripts/llm_usage/ingest_all_to_postgres.sh --schema llm_usage --dry-run
```

## Reporting

Use the reporting command for quick operator summaries:

```bash
./scripts/llm_usage/report_usage_summary.sh --report all --days 30 --limit 10
./scripts/llm_usage/report_usage_summary.sh --report session --days 7 --limit 20
./scripts/llm_usage/report_usage_summary.sh --report model --days 30 --limit 20
./scripts/llm_usage/report_usage_summary.sh --report provider --days 30 --limit 20
./scripts/llm_usage/report_usage_summary.sh --report freshness
```

Report types:

- `freshness`: row counts and latest ingested/event timestamps
- `session`: per-session totals from `llm_session_usage_summary`
- `model`: token burn by source/model/provider
- `provider`: token burn by source/provider
- `all`: freshness plus provider/model/session summaries

## Scheduled ingestion

To install a user-level systemd timer that runs the ledger ingest every 15 minutes:

```bash
./scripts/llm_usage/install_user_timer.sh --interval-minutes 15 --schema llm_usage
```

That installer writes:

- `~/.config/systemd/user/codex-llm-usage-ingest.service`
- `~/.config/systemd/user/codex-llm-usage-ingest.timer`
- `~/.config/codex/llm-usage-ingest.env`

The timer runs [run_scheduled_ingest.sh](/home/grant/mmm/codex/scripts/llm_usage/run_scheduled_ingest.sh), which:

- uses the same database resolution rules as the manual ingest scripts
- acquires a non-blocking `flock` lock to avoid overlapping runs
- appends logs to `~/.local/state/codex/llm-usage/scheduled-ingest.log` by default

To install the unit files without enabling the timer immediately:

```bash
./scripts/llm_usage/install_user_timer.sh --no-enable
```

To test the scheduled path once without waiting for the timer:

```bash
./scripts/llm_usage/run_scheduled_ingest.sh --schema llm_usage --dry-run
```

## Example SQL queries

Session totals by model across all sources:

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
  input_tokens,
  cached_input_tokens,
  output_tokens,
  reasoning_tokens,
  tool_tokens
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

Latest Codex rate-limit snapshots:

```sql
select
  session_id,
  model,
  rate_limit_id,
  primary_used_percent,
  primary_resets_at,
  secondary_used_percent,
  secondary_resets_at,
  event_ts
from llm_usage.llm_latest_rate_limits
where source_system = 'codex'
order by event_ts desc;
```

Latest Gemini quota signals:

```sql
select
  session_id,
  project_key,
  model,
  tool_name,
  error_message,
  reset_after_text,
  reset_after_seconds,
  event_ts
from llm_usage.llm_latest_quota_events
where source_system = 'gemini'
order by event_ts desc;
```

## Current limits

- Gemini interactive token usage is authoritative because it comes from persisted session JSON artifacts.
- Gemini MCP token usage is authoritative when the MCP usage ledger is enabled.
- Codex token usage is authoritative from rollout `token_count` events.
- Codex quota visibility is represented via rate-limit snapshots already present in rollout `token_count.rate_limits` payloads.
- Gemini quota accounting is improved but still partial. We capture explicit reset signals from persisted interactive errors and quota/rate-limit classifications from the MCP ledger, but the installed `gemini` binary on this host does not currently expose a stable `stats session` command or a durable quota table that can be ingested the same way.

That means the warehouse is complete for Codex usage plus Codex rate limits, complete for Gemini token usage where artifacts exist, and best-effort for Gemini quota until the upstream CLI surfaces a stable quota artifact again.
