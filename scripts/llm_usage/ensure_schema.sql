CREATE SCHEMA IF NOT EXISTS __LLM_SCHEMA__;

CREATE TABLE IF NOT EXISTS __LLM_SCHEMA__.llm_usage_events (
  record_hash text PRIMARY KEY,
  source_system text NOT NULL,
  source_kind text NOT NULL,
  source_path text NOT NULL,
  source_row_id text,
  ingested_at timestamptz NOT NULL DEFAULT now(),
  event_ts timestamptz NOT NULL,
  session_id text NOT NULL,
  turn_id text,
  project_key text,
  project_path text,
  cwd text,
  tool_name text,
  actor text,
  provider text,
  model_requested text,
  model_used text,
  ok boolean,
  error_category text,
  input_tokens bigint,
  cached_input_tokens bigint,
  output_tokens bigint,
  reasoning_tokens bigint,
  tool_tokens bigint,
  total_tokens bigint,
  cumulative_input_tokens bigint,
  cumulative_cached_input_tokens bigint,
  cumulative_output_tokens bigint,
  cumulative_reasoning_tokens bigint,
  cumulative_total_tokens bigint,
  context_window bigint,
  rate_limit_id text,
  rate_limit_name text,
  primary_used_percent double precision,
  primary_window_minutes bigint,
  primary_resets_at timestamptz,
  secondary_used_percent double precision,
  secondary_window_minutes bigint,
  secondary_resets_at timestamptz,
  credits_balance text,
  credits_unlimited boolean,
  raw jsonb NOT NULL
);

CREATE INDEX IF NOT EXISTS llm_usage_events_source_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (source_system, source_kind, event_ts DESC);

CREATE INDEX IF NOT EXISTS llm_usage_events_session_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (session_id, event_ts DESC);

CREATE INDEX IF NOT EXISTS llm_usage_events_model_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (model_used, event_ts DESC);

CREATE INDEX IF NOT EXISTS llm_usage_events_project_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (project_key, event_ts DESC);

CREATE TABLE IF NOT EXISTS __LLM_SCHEMA__.llm_quota_events (
  record_hash text PRIMARY KEY,
  source_system text NOT NULL,
  source_kind text NOT NULL,
  source_path text NOT NULL,
  source_row_id text,
  ingested_at timestamptz NOT NULL DEFAULT now(),
  event_ts timestamptz NOT NULL,
  session_id text NOT NULL,
  project_key text,
  project_path text,
  model_used text,
  tool_name text,
  error_message text NOT NULL,
  reset_after_text text,
  reset_after_seconds bigint,
  raw jsonb NOT NULL
);

CREATE INDEX IF NOT EXISTS llm_quota_events_session_event_ts_idx
  ON __LLM_SCHEMA__.llm_quota_events (session_id, event_ts DESC);

CREATE INDEX IF NOT EXISTS llm_quota_events_model_event_ts_idx
  ON __LLM_SCHEMA__.llm_quota_events (model_used, event_ts DESC);

CREATE OR REPLACE VIEW __LLM_SCHEMA__.llm_session_usage_summary AS
SELECT
  source_system,
  source_kind,
  session_id,
  project_key,
  project_path,
  coalesce(model_used, model_requested, 'unknown') AS model,
  min(event_ts) AS started_at,
  max(event_ts) AS last_event_at,
  count(*) AS event_count,
  sum(coalesce(input_tokens, 0)) AS input_tokens,
  sum(coalesce(cached_input_tokens, 0)) AS cached_input_tokens,
  sum(coalesce(output_tokens, 0)) AS output_tokens,
  sum(coalesce(reasoning_tokens, 0)) AS reasoning_tokens,
  sum(coalesce(tool_tokens, 0)) AS tool_tokens,
  sum(coalesce(total_tokens, 0)) AS summed_event_tokens,
  max(cumulative_total_tokens) AS max_cumulative_total_tokens,
  coalesce(max(cumulative_total_tokens), sum(coalesce(total_tokens, 0))) AS session_total_tokens
FROM __LLM_SCHEMA__.llm_usage_events
GROUP BY
  source_system,
  source_kind,
  session_id,
  project_key,
  project_path,
  coalesce(model_used, model_requested, 'unknown');

CREATE OR REPLACE VIEW __LLM_SCHEMA__.llm_latest_rate_limits AS
SELECT DISTINCT ON (
  source_system,
  session_id,
  coalesce(model_used, model_requested, 'unknown'),
  coalesce(rate_limit_id, '')
)
  record_hash,
  source_system,
  source_kind,
  session_id,
  project_key,
  project_path,
  coalesce(model_used, model_requested, 'unknown') AS model,
  rate_limit_id,
  rate_limit_name,
  primary_used_percent,
  primary_window_minutes,
  primary_resets_at,
  secondary_used_percent,
  secondary_window_minutes,
  secondary_resets_at,
  credits_balance,
  credits_unlimited,
  event_ts,
  raw
FROM __LLM_SCHEMA__.llm_usage_events
WHERE
  primary_used_percent IS NOT NULL
  OR secondary_used_percent IS NOT NULL
  OR rate_limit_id IS NOT NULL
ORDER BY
  source_system,
  session_id,
  coalesce(model_used, model_requested, 'unknown'),
  coalesce(rate_limit_id, ''),
  event_ts DESC;

CREATE OR REPLACE VIEW __LLM_SCHEMA__.llm_latest_quota_events AS
SELECT DISTINCT ON (
  source_system,
  session_id,
  coalesce(model_used, 'unknown'),
  coalesce(tool_name, '')
)
  record_hash,
  source_system,
  source_kind,
  session_id,
  project_key,
  project_path,
  coalesce(model_used, 'unknown') AS model,
  tool_name,
  error_message,
  reset_after_text,
  reset_after_seconds,
  event_ts,
  raw
FROM __LLM_SCHEMA__.llm_quota_events
ORDER BY
  source_system,
  session_id,
  coalesce(model_used, 'unknown'),
  coalesce(tool_name, ''),
  event_ts DESC;
