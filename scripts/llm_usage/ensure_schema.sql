CREATE SCHEMA IF NOT EXISTS __LLM_SCHEMA__;

CREATE TABLE IF NOT EXISTS __LLM_SCHEMA__.llm_ingest_runs (
  run_id text PRIMARY KEY,
  script_name text NOT NULL,
  parser_version text NOT NULL,
  source_system text,
  source_kind text,
  dry_run boolean NOT NULL DEFAULT false,
  started_at timestamptz NOT NULL DEFAULT now(),
  completed_at timestamptz,
  status text NOT NULL,
  processed_artifacts bigint,
  skipped_artifacts bigint,
  generated_rows bigint,
  error_text text,
  raw jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS llm_ingest_runs_started_at_idx
  ON __LLM_SCHEMA__.llm_ingest_runs (started_at DESC);

UPDATE __LLM_SCHEMA__.llm_ingest_runs
SET
  status = 'abandoned',
  completed_at = coalesce(completed_at, now()),
  error_text = coalesce(error_text, 'stale running row normalized during schema bootstrap')
WHERE status = 'running'
  AND completed_at IS NULL
  AND started_at < now() - interval '1 hour';

CREATE TABLE IF NOT EXISTS __LLM_SCHEMA__.llm_source_artifacts (
  source_system text NOT NULL,
  source_kind text NOT NULL,
  source_path_hash text NOT NULL,
  source_path text NOT NULL,
  source_size_bytes bigint,
  source_mtime_epoch bigint,
  source_row_count bigint,
  parser_version text NOT NULL,
  last_ingest_run_id text,
  last_ingested_at timestamptz NOT NULL DEFAULT now(),
  status text NOT NULL,
  raw jsonb NOT NULL DEFAULT '{}'::jsonb,
  PRIMARY KEY (source_system, source_kind, source_path_hash)
);

CREATE INDEX IF NOT EXISTS llm_source_artifacts_last_ingested_at_idx
  ON __LLM_SCHEMA__.llm_source_artifacts (last_ingested_at DESC);

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

ALTER TABLE __LLM_SCHEMA__.llm_usage_events
  ADD COLUMN IF NOT EXISTS logical_key text,
  ADD COLUMN IF NOT EXISTS parser_version text,
  ADD COLUMN IF NOT EXISTS ingest_run_id text,
  ADD COLUMN IF NOT EXISTS source_path_hash text,
  ADD COLUMN IF NOT EXISTS event_status text;

DROP INDEX IF EXISTS __LLM_SCHEMA__.llm_usage_events_logical_key_idx;

WITH usage_candidates AS (
  SELECT
    record_hash,
    concat_ws(
      '|',
      coalesce(source_system, ''),
      coalesce(source_kind, ''),
      coalesce(session_id, ''),
      coalesce(turn_id, ''),
      coalesce(source_row_id, '')
    ) AS desired_logical_key,
    row_number() OVER (
      PARTITION BY concat_ws(
        '|',
        coalesce(source_system, ''),
        coalesce(source_kind, ''),
        coalesce(session_id, ''),
        coalesce(turn_id, ''),
        coalesce(source_row_id, '')
      )
      ORDER BY
        CASE WHEN source_path NOT LIKE '/%' THEN 0 ELSE 1 END,
        ingested_at DESC,
        record_hash DESC
    ) AS keep_rank
  FROM __LLM_SCHEMA__.llm_usage_events
),
usage_dupes AS (
  DELETE FROM __LLM_SCHEMA__.llm_usage_events events
  USING usage_candidates candidates
  WHERE events.record_hash = candidates.record_hash
    AND candidates.keep_rank > 1
)
UPDATE __LLM_SCHEMA__.llm_usage_events events
SET
  logical_key = candidates.desired_logical_key,
  parser_version = coalesce(events.parser_version, '2026-03-07-v2'),
  source_path_hash = coalesce(events.source_path_hash, md5(events.source_path)),
  event_status = coalesce(
    events.event_status,
    CASE
      WHEN events.error_category = 'interrupted' THEN 'aborted'
      WHEN events.ok IS TRUE THEN 'succeeded'
      WHEN events.ok IS FALSE THEN 'failed'
      ELSE 'observed'
    END
  )
FROM usage_candidates candidates
WHERE events.record_hash = candidates.record_hash
  AND candidates.keep_rank = 1
  AND (
    events.logical_key IS DISTINCT FROM candidates.desired_logical_key
    OR events.parser_version IS NULL
    OR events.source_path_hash IS NULL
    OR events.event_status IS NULL
  );

ALTER TABLE __LLM_SCHEMA__.llm_usage_events
  ALTER COLUMN logical_key SET NOT NULL,
  ALTER COLUMN parser_version SET NOT NULL,
  ALTER COLUMN source_path_hash SET NOT NULL,
  ALTER COLUMN event_status SET NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS llm_usage_events_logical_key_idx
  ON __LLM_SCHEMA__.llm_usage_events (logical_key);

CREATE INDEX IF NOT EXISTS llm_usage_events_source_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (source_system, source_kind, event_ts DESC);

CREATE INDEX IF NOT EXISTS llm_usage_events_session_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (session_id, event_ts DESC);

CREATE INDEX IF NOT EXISTS llm_usage_events_model_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (model_used, event_ts DESC);

CREATE INDEX IF NOT EXISTS llm_usage_events_project_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (project_key, event_ts DESC);

CREATE INDEX IF NOT EXISTS llm_usage_events_provider_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (provider, event_ts DESC);

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

ALTER TABLE __LLM_SCHEMA__.llm_quota_events
  ADD COLUMN IF NOT EXISTS logical_key text,
  ADD COLUMN IF NOT EXISTS parser_version text,
  ADD COLUMN IF NOT EXISTS ingest_run_id text,
  ADD COLUMN IF NOT EXISTS source_path_hash text;

DROP INDEX IF EXISTS __LLM_SCHEMA__.llm_quota_events_logical_key_idx;

WITH quota_candidates AS (
  SELECT
    record_hash,
    concat_ws(
      '|',
      coalesce(source_system, ''),
      coalesce(source_kind, ''),
      coalesce(session_id, ''),
      '',
      coalesce(source_row_id, '')
    ) AS desired_logical_key,
    row_number() OVER (
      PARTITION BY concat_ws(
        '|',
        coalesce(source_system, ''),
        coalesce(source_kind, ''),
        coalesce(session_id, ''),
        '',
        coalesce(source_row_id, '')
      )
      ORDER BY
        CASE WHEN source_path NOT LIKE '/%' THEN 0 ELSE 1 END,
        ingested_at DESC,
        record_hash DESC
    ) AS keep_rank
  FROM __LLM_SCHEMA__.llm_quota_events
),
quota_dupes AS (
  DELETE FROM __LLM_SCHEMA__.llm_quota_events events
  USING quota_candidates candidates
  WHERE events.record_hash = candidates.record_hash
    AND candidates.keep_rank > 1
)
UPDATE __LLM_SCHEMA__.llm_quota_events events
SET
  logical_key = candidates.desired_logical_key,
  parser_version = coalesce(events.parser_version, '2026-03-07-v2'),
  source_path_hash = coalesce(events.source_path_hash, md5(events.source_path))
FROM quota_candidates candidates
WHERE events.record_hash = candidates.record_hash
  AND candidates.keep_rank = 1
  AND (
    events.logical_key IS DISTINCT FROM candidates.desired_logical_key
    OR events.parser_version IS NULL
    OR events.source_path_hash IS NULL
  );

ALTER TABLE __LLM_SCHEMA__.llm_quota_events
  ALTER COLUMN logical_key SET NOT NULL,
  ALTER COLUMN parser_version SET NOT NULL,
  ALTER COLUMN source_path_hash SET NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS llm_quota_events_logical_key_idx
  ON __LLM_SCHEMA__.llm_quota_events (logical_key);

CREATE INDEX IF NOT EXISTS llm_quota_events_session_event_ts_idx
  ON __LLM_SCHEMA__.llm_quota_events (session_id, event_ts DESC);

CREATE INDEX IF NOT EXISTS llm_quota_events_model_event_ts_idx
  ON __LLM_SCHEMA__.llm_quota_events (model_used, event_ts DESC);

DROP VIEW IF EXISTS __LLM_SCHEMA__.llm_latest_rate_limits;
DROP VIEW IF EXISTS __LLM_SCHEMA__.llm_latest_quota_events;
DROP VIEW IF EXISTS __LLM_SCHEMA__.llm_session_usage_summary;

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
  count(*) FILTER (WHERE event_status = 'succeeded') AS succeeded_event_count,
  count(*) FILTER (WHERE event_status = 'aborted') AS aborted_event_count,
  count(*) FILTER (WHERE event_status = 'failed') AS failed_event_count,
  sum(coalesce(input_tokens, 0)) AS input_tokens,
  sum(coalesce(cached_input_tokens, 0)) AS cached_input_tokens,
  sum(coalesce(output_tokens, 0)) AS output_tokens,
  sum(coalesce(reasoning_tokens, 0)) AS reasoning_tokens,
  sum(coalesce(tool_tokens, 0)) AS tool_tokens,
  sum(coalesce(total_tokens, 0)) AS summed_event_tokens,
  max(cumulative_total_tokens) AS max_cumulative_total_tokens,
  sum(coalesce(total_tokens, 0)) AS session_total_tokens
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
  logical_key,
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
  event_status,
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
  logical_key,
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
