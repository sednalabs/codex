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
  forked_from_session_id text,
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
  ADD COLUMN IF NOT EXISTS event_status text,
  ADD COLUMN IF NOT EXISTS forked_from_session_id text;

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
  parser_version = coalesce(events.parser_version, '2026-03-13-v4'),
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

CREATE INDEX IF NOT EXISTS llm_usage_events_codex_turn_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (turn_id, event_ts DESC, ingested_at DESC)
  WHERE source_system = 'codex'
    AND source_kind = 'interactive_turn'
    AND turn_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS llm_usage_events_codex_turn_segment_event_ts_idx
  ON __LLM_SCHEMA__.llm_usage_events (turn_id, source_row_id, event_ts DESC, ingested_at DESC)
  WHERE source_system = 'codex'
    AND source_kind = 'interactive_turn_segment'
    AND turn_id IS NOT NULL;

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

CREATE TABLE IF NOT EXISTS __LLM_SCHEMA__.llm_public_model_pricing_history (
  pricing_id bigserial PRIMARY KEY,
  provider text NOT NULL,
  public_api_model_id text NOT NULL,
  pricing_tier text NOT NULL,
  pricing_currency text NOT NULL,
  input_tokens_min bigint NOT NULL DEFAULT 0,
  input_tokens_max bigint NOT NULL DEFAULT 9223372036854775807,
  input_rate_per_1m numeric(18, 8) NOT NULL,
  cached_input_rate_per_1m numeric(18, 8) NOT NULL,
  output_rate_per_1m numeric(18, 8) NOT NULL,
  effective_from timestamptz NOT NULL,
  effective_to timestamptz,
  source_url text NOT NULL,
  source_observed_at timestamptz NOT NULL,
  notes text,
  raw jsonb NOT NULL DEFAULT '{}'::jsonb,
  CHECK (input_tokens_min >= 0),
  CHECK (input_tokens_max >= input_tokens_min)
);

CREATE UNIQUE INDEX IF NOT EXISTS llm_public_model_pricing_history_natural_key_idx
  ON __LLM_SCHEMA__.llm_public_model_pricing_history (
    provider,
    public_api_model_id,
    pricing_tier,
    input_tokens_min,
    input_tokens_max,
    effective_from
  );

CREATE INDEX IF NOT EXISTS llm_public_model_pricing_history_lookup_idx
  ON __LLM_SCHEMA__.llm_public_model_pricing_history (
    provider,
    public_api_model_id,
    pricing_tier,
    effective_from DESC,
    effective_to
  );

INSERT INTO __LLM_SCHEMA__.llm_public_model_pricing_history (
  provider,
  public_api_model_id,
  pricing_tier,
  pricing_currency,
  input_tokens_min,
  input_tokens_max,
  input_rate_per_1m,
  cached_input_rate_per_1m,
  output_rate_per_1m,
  effective_from,
  source_url,
  source_observed_at,
  notes
)
SELECT
  seed.provider,
  seed.public_api_model_id,
  seed.pricing_tier,
  seed.pricing_currency,
  seed.input_tokens_min,
  seed.input_tokens_max,
  seed.input_rate_per_1m,
  seed.cached_input_rate_per_1m,
  seed.output_rate_per_1m,
  seed.effective_from,
  seed.source_url,
  seed.source_observed_at,
  seed.notes
FROM (
  VALUES
    (
      'openai',
      'gpt-5.4',
      'standard',
      'USD',
      0::bigint,
      9223372036854775807::bigint,
      2.50::numeric(18, 8),
      0.25::numeric(18, 8),
      15.00::numeric(18, 8),
      timestamptz '2026-03-10 00:00:00+00',
      'https://openai.com/api/pricing/',
      timestamptz '2026-03-10 00:00:00+00',
      'Verified on 2026-03-10; earlier intervals require explicit backfill.'
    ),
    (
      'openai',
      'gpt-5.3-codex',
      'standard',
      'USD',
      0::bigint,
      9223372036854775807::bigint,
      1.75::numeric(18, 8),
      0.175::numeric(18, 8),
      14.00::numeric(18, 8),
      timestamptz '2026-03-10 00:00:00+00',
      'https://developers.openai.com/api/docs/models/gpt-5.3-codex',
      timestamptz '2026-03-10 00:00:00+00',
      'Verified on 2026-03-10; earlier intervals require explicit backfill.'
    ),
    (
      'openai',
      'gpt-5.2',
      'standard',
      'USD',
      0::bigint,
      9223372036854775807::bigint,
      1.75::numeric(18, 8),
      0.175::numeric(18, 8),
      14.00::numeric(18, 8),
      timestamptz '2026-03-10 00:00:00+00',
      'https://platform.openai.com/docs/pricing/',
      timestamptz '2026-03-10 00:00:00+00',
      'Verified on 2026-03-10; earlier intervals require explicit backfill.'
    ),
    (
      'openai',
      'gpt-5.2-codex',
      'standard',
      'USD',
      0::bigint,
      9223372036854775807::bigint,
      1.75::numeric(18, 8),
      0.175::numeric(18, 8),
      14.00::numeric(18, 8),
      timestamptz '2026-03-10 00:00:00+00',
      'https://platform.openai.com/docs/pricing/',
      timestamptz '2026-03-10 00:00:00+00',
      'Verified on 2026-03-10; earlier intervals require explicit backfill.'
    ),
    (
      'openai',
      'gpt-5.1-codex-mini',
      'standard',
      'USD',
      0::bigint,
      9223372036854775807::bigint,
      0.25::numeric(18, 8),
      0.025::numeric(18, 8),
      2.00::numeric(18, 8),
      timestamptz '2026-03-10 00:00:00+00',
      'https://developers.openai.com/api/docs/models/gpt-5.1-codex-mini',
      timestamptz '2026-03-10 00:00:00+00',
      'Verified on 2026-03-10; earlier intervals require explicit backfill.'
    ),
    (
      'gemini',
      'gemini-3-flash-preview',
      'standard',
      'USD',
      0::bigint,
      9223372036854775807::bigint,
      0.50::numeric(18, 8),
      0.05::numeric(18, 8),
      3.00::numeric(18, 8),
      timestamptz '2026-03-10 00:00:00+00',
      'https://ai.google.dev/gemini-api/docs/pricing',
      timestamptz '2026-03-10 00:00:00+00',
      'Verified on 2026-03-10; earlier intervals require explicit backfill.'
    ),
    (
      'gemini',
      'gemini-2.5-flash-lite',
      'standard',
      'USD',
      0::bigint,
      9223372036854775807::bigint,
      0.10::numeric(18, 8),
      0.01::numeric(18, 8),
      0.40::numeric(18, 8),
      timestamptz '2026-03-10 00:00:00+00',
      'https://ai.google.dev/gemini-api/docs/pricing',
      timestamptz '2026-03-10 00:00:00+00',
      'Verified on 2026-03-10; earlier intervals require explicit backfill.'
    ),
    (
      'gemini',
      'gemini-2.5-pro',
      'standard',
      'USD',
      0::bigint,
      200000::bigint,
      1.25::numeric(18, 8),
      0.125::numeric(18, 8),
      10.00::numeric(18, 8),
      timestamptz '2026-03-10 00:00:00+00',
      'https://ai.google.dev/gemini-api/docs/pricing',
      timestamptz '2026-03-10 00:00:00+00',
      'Verified on 2026-03-10; earlier intervals require explicit backfill.'
    ),
    (
      'gemini',
      'gemini-2.5-pro',
      'standard',
      'USD',
      200001::bigint,
      9223372036854775807::bigint,
      2.50::numeric(18, 8),
      0.25::numeric(18, 8),
      15.00::numeric(18, 8),
      timestamptz '2026-03-10 00:00:00+00',
      'https://ai.google.dev/gemini-api/docs/pricing',
      timestamptz '2026-03-10 00:00:00+00',
      'Verified on 2026-03-10; earlier intervals require explicit backfill.'
    )
) AS seed (
  provider,
  public_api_model_id,
  pricing_tier,
  pricing_currency,
  input_tokens_min,
  input_tokens_max,
  input_rate_per_1m,
  cached_input_rate_per_1m,
  output_rate_per_1m,
  effective_from,
  source_url,
  source_observed_at,
  notes
)
WHERE NOT EXISTS (
  SELECT 1
  FROM __LLM_SCHEMA__.llm_public_model_pricing_history existing
  WHERE existing.provider = seed.provider
    AND existing.public_api_model_id = seed.public_api_model_id
    AND existing.pricing_tier = seed.pricing_tier
    AND existing.input_tokens_min = seed.input_tokens_min
    AND existing.input_tokens_max = seed.input_tokens_max
    AND existing.effective_from = seed.effective_from
);

CREATE TABLE IF NOT EXISTS __LLM_SCHEMA__.llm_fx_rate_history (
  fx_rate_id bigserial PRIMARY KEY,
  base_currency text NOT NULL,
  quote_currency text NOT NULL,
  rate_date date NOT NULL,
  rate_value numeric(18, 10) NOT NULL,
  source_name text NOT NULL,
  source_url text NOT NULL,
  source_observed_at timestamptz NOT NULL,
  raw jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE UNIQUE INDEX IF NOT EXISTS llm_fx_rate_history_natural_key_idx
  ON __LLM_SCHEMA__.llm_fx_rate_history (
    base_currency,
    quote_currency,
    rate_date,
    source_name
  );

CREATE INDEX IF NOT EXISTS llm_fx_rate_history_lookup_idx
  ON __LLM_SCHEMA__.llm_fx_rate_history (
    base_currency,
    quote_currency,
    rate_date DESC
  );

DROP VIEW IF EXISTS __LLM_SCHEMA__.llm_latest_rate_limits;
DROP VIEW IF EXISTS __LLM_SCHEMA__.llm_latest_quota_events;
DROP VIEW IF EXISTS __LLM_SCHEMA__.llm_session_usage_summary;
DROP VIEW IF EXISTS __LLM_SCHEMA__.llm_session_tool_call_summary;
DROP VIEW IF EXISTS __LLM_SCHEMA__.llm_canonical_usage_events;
DROP VIEW IF EXISTS __LLM_SCHEMA__.llm_usage_public_api_costs;
DROP VIEW IF EXISTS __LLM_SCHEMA__.llm_usage_public_api_costs_observed;

CREATE OR REPLACE VIEW __LLM_SCHEMA__.llm_canonical_usage_events AS
WITH base_events AS (
  SELECT
    events.*,
    coalesce(events.model_used, events.model_requested, 'unknown') AS model_key
  FROM __LLM_SCHEMA__.llm_usage_events events
),
gemini_rollup_presence AS (
  SELECT
    session_id,
    project_key,
    model_key,
    bool_or(source_kind = 'mcp_tool_call') AS has_rollup
  FROM base_events
  WHERE source_system = 'gemini'
  GROUP BY session_id, project_key, model_key
),
codex_segment_presence AS (
  SELECT
    turn_id,
    bool_or(source_kind = 'interactive_turn_segment') AS has_segments
  FROM base_events
  WHERE source_system = 'codex'
    AND turn_id IS NOT NULL
  GROUP BY turn_id
)
SELECT events.*
FROM base_events events
LEFT JOIN gemini_rollup_presence rollups
  ON events.source_system = 'gemini'
 AND events.session_id = rollups.session_id
 AND events.project_key IS NOT DISTINCT FROM rollups.project_key
 AND events.model_key = rollups.model_key
LEFT JOIN codex_segment_presence segments
  ON events.source_system = 'codex'
 AND events.turn_id IS NOT NULL
 AND events.turn_id = segments.turn_id
WHERE
  (events.source_system = 'codex' AND events.source_kind = 'interactive_turn_segment')
  OR (
    events.source_system = 'codex'
    AND events.source_kind = 'interactive_turn'
    AND (events.turn_id IS NULL OR coalesce(segments.has_segments, false) = false)
  )
  OR (events.source_system = 'gemini' AND events.source_kind = 'mcp_tool_call')
  OR (
    events.source_system = 'gemini'
    AND events.source_kind = 'interactive_message'
    AND coalesce(rollups.has_rollup, false) = false
  );

CREATE OR REPLACE VIEW __LLM_SCHEMA__.llm_session_tool_call_summary AS
WITH session_tool_call_events AS (
  SELECT
    source_system,
    source_kind,
    session_id,
    project_key,
    project_path,
    coalesce(provider, 'unknown') AS provider,
    coalesce(model_used, model_requested, 'unknown') AS model,
    event_ts,
    CASE
      WHEN source_system = 'gemini' AND source_kind = 'interactive_message'
        THEN coalesce(nullif(raw->>'tool_call_count', '')::bigint, 0)
      ELSE NULL
    END AS tool_call_count
  FROM __LLM_SCHEMA__.llm_canonical_usage_events
)
SELECT
  source_system,
  source_kind,
  session_id,
  project_key,
  project_path,
  provider,
  model,
  min(event_ts) AS started_at,
  max(event_ts) AS last_event_at,
  sum(coalesce(tool_call_count, 0)) AS tool_call_count,
  count(*) FILTER (WHERE tool_call_count IS NOT NULL) AS tool_call_count_known_event_count,
  count(*) FILTER (WHERE tool_call_count IS NULL) AS tool_call_count_unknown_event_count,
  CASE
    WHEN count(*) FILTER (WHERE tool_call_count IS NULL) = 0 THEN 'complete'
    WHEN count(*) FILTER (WHERE tool_call_count IS NOT NULL) = 0 THEN 'unsupported'
    ELSE 'partial'
  END AS tool_call_count_coverage
FROM session_tool_call_events
GROUP BY
  source_system,
  source_kind,
  session_id,
  project_key,
  project_path,
  provider,
  model;

CREATE OR REPLACE VIEW __LLM_SCHEMA__.llm_session_usage_summary AS
WITH usage_summary AS (
  SELECT
    source_system,
    source_kind,
    session_id,
    project_key,
    project_path,
    coalesce(provider, 'unknown') AS provider,
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
  FROM __LLM_SCHEMA__.llm_canonical_usage_events
  GROUP BY
    source_system,
    source_kind,
    session_id,
    project_key,
    project_path,
    coalesce(provider, 'unknown'),
    coalesce(model_used, model_requested, 'unknown')
)
SELECT
  usage_summary.source_system,
  usage_summary.source_kind,
  usage_summary.session_id,
  usage_summary.project_key,
  usage_summary.project_path,
  usage_summary.provider,
  usage_summary.model,
  usage_summary.started_at,
  usage_summary.last_event_at,
  usage_summary.event_count,
  usage_summary.succeeded_event_count,
  usage_summary.aborted_event_count,
  usage_summary.failed_event_count,
  usage_summary.input_tokens,
  usage_summary.cached_input_tokens,
  usage_summary.output_tokens,
  usage_summary.reasoning_tokens,
  usage_summary.tool_tokens,
  usage_summary.summed_event_tokens,
  usage_summary.max_cumulative_total_tokens,
  usage_summary.session_total_tokens,
  coalesce(tool_calls.tool_call_count, 0) AS tool_call_count,
  coalesce(tool_calls.tool_call_count_known_event_count, 0) AS tool_call_count_known_event_count,
  coalesce(tool_calls.tool_call_count_unknown_event_count, usage_summary.event_count) AS tool_call_count_unknown_event_count,
  coalesce(tool_calls.tool_call_count_coverage, 'unsupported') AS tool_call_count_coverage
FROM usage_summary
LEFT JOIN __LLM_SCHEMA__.llm_session_tool_call_summary tool_calls
  ON usage_summary.source_system = tool_calls.source_system
 AND usage_summary.source_kind = tool_calls.source_kind
 AND usage_summary.session_id = tool_calls.session_id
 AND usage_summary.project_key IS NOT DISTINCT FROM tool_calls.project_key
 AND usage_summary.project_path IS NOT DISTINCT FROM tool_calls.project_path
 AND usage_summary.provider = tool_calls.provider
 AND usage_summary.model = tool_calls.model;

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

CREATE OR REPLACE VIEW __LLM_SCHEMA__.llm_usage_public_api_costs_observed AS
WITH base_events AS (
  SELECT
    events.*,
    coalesce(events.model_used, events.model_requested, 'unknown') AS model_key,
    greatest(coalesce(events.input_tokens, 0) - coalesce(events.cached_input_tokens, 0), 0) AS billable_uncached_input_tokens,
    coalesce(events.cached_input_tokens, 0) AS billable_cached_input_tokens,
    coalesce(events.output_tokens, 0) AS billable_output_tokens,
    CASE
      WHEN events.source_system = 'codex'
        AND events.source_kind = 'interactive_turn_segment'
        AND events.turn_id IS NOT NULL
      THEN coalesce(events.turn_id || '|' || events.source_row_id, events.logical_key)
      WHEN events.source_system = 'codex'
        AND events.source_kind = 'interactive_turn'
        AND events.turn_id IS NOT NULL
      THEN events.turn_id
      ELSE events.logical_key
    END AS billing_identity_key,
    CASE
      WHEN events.source_system = 'codex'
        AND events.source_kind = 'interactive_turn_segment'
        AND events.turn_id IS NOT NULL
      THEN 'codex_turn_segment_observed'
      WHEN events.source_system = 'codex'
        AND events.source_kind = 'interactive_turn'
        AND events.turn_id IS NOT NULL
      THEN 'codex_turn_observed'
      WHEN events.source_system = 'codex'
        AND events.source_kind = 'interactive_turn'
      THEN 'codex_logical_key_fallback'
      WHEN events.source_system = 'gemini'
        AND events.source_kind = 'mcp_tool_call'
      THEN 'gemini_rollup'
      WHEN events.source_system = 'gemini'
        AND events.source_kind = 'interactive_message'
      THEN 'gemini_detail_fallback'
      ELSE 'logical_key_observed'
    END AS billing_attribution_mode
  FROM __LLM_SCHEMA__.llm_usage_events events
),
gemini_rollup_presence AS (
  SELECT
    session_id,
    project_key,
    model_key,
    bool_or(source_kind = 'mcp_tool_call') AS has_rollup
  FROM base_events
  WHERE source_system = 'gemini'
  GROUP BY session_id, project_key, model_key
),
codex_segment_presence AS (
  SELECT
    turn_id,
    bool_or(source_kind = 'interactive_turn_segment') AS has_segments
  FROM base_events
  WHERE source_system = 'codex'
    AND turn_id IS NOT NULL
  GROUP BY turn_id
),
observed_events AS (
  SELECT events.*
  FROM base_events events
  LEFT JOIN gemini_rollup_presence rollups
    ON events.source_system = 'gemini'
   AND events.session_id = rollups.session_id
   AND events.project_key IS NOT DISTINCT FROM rollups.project_key
   AND events.model_key = rollups.model_key
  LEFT JOIN codex_segment_presence segments
    ON events.source_system = 'codex'
   AND events.turn_id IS NOT NULL
   AND events.turn_id = segments.turn_id
  WHERE
    (events.source_system = 'codex' AND events.source_kind = 'interactive_turn_segment')
    OR (
      events.source_system = 'codex'
      AND events.source_kind = 'interactive_turn'
      AND (events.turn_id IS NULL OR coalesce(segments.has_segments, false) = false)
    )
    OR (events.source_system = 'gemini' AND events.source_kind = 'mcp_tool_call')
    OR (
      events.source_system = 'gemini'
      AND events.source_kind = 'interactive_message'
      AND coalesce(rollups.has_rollup, false) = false
    )
),
codex_identity_ranked AS (
  SELECT
    events.record_hash,
    events.billing_identity_key,
    row_number() OVER (
      PARTITION BY events.billing_identity_key
      ORDER BY events.event_ts ASC, events.ingested_at ASC, events.record_hash ASC
    ) AS billing_origin_rank,
    row_number() OVER (
      PARTITION BY events.billing_identity_key
      ORDER BY events.event_ts DESC, events.ingested_at DESC, events.record_hash DESC
    ) AS billing_latest_rank,
    row_number() OVER (
      PARTITION BY events.billing_identity_key
      ORDER BY
        (
          CASE WHEN events.provider IS NOT NULL THEN 1 ELSE 0 END
          + CASE WHEN events.model_requested IS NOT NULL THEN 1 ELSE 0 END
          + CASE WHEN events.model_used IS NOT NULL THEN 1 ELSE 0 END
          + CASE WHEN events.input_tokens IS NOT NULL THEN 1 ELSE 0 END
          + CASE WHEN events.cached_input_tokens IS NOT NULL THEN 1 ELSE 0 END
          + CASE WHEN events.output_tokens IS NOT NULL THEN 1 ELSE 0 END
          + CASE WHEN events.reasoning_tokens IS NOT NULL THEN 1 ELSE 0 END
          + CASE WHEN events.tool_tokens IS NOT NULL THEN 1 ELSE 0 END
          + CASE WHEN events.total_tokens IS NOT NULL THEN 1 ELSE 0 END
          + CASE WHEN events.context_window IS NOT NULL THEN 1 ELSE 0 END
          + CASE WHEN events.raw IS NOT NULL AND events.raw <> '{}'::jsonb THEN 1 ELSE 0 END
        ) DESC,
        coalesce(events.total_tokens, 0) DESC,
        (
          coalesce(events.input_tokens, 0)
          + coalesce(events.cached_input_tokens, 0)
          + coalesce(events.output_tokens, 0)
          + coalesce(events.reasoning_tokens, 0)
          + coalesce(events.tool_tokens, 0)
        ) DESC,
        events.parser_version DESC NULLS LAST,
        events.event_ts DESC,
        events.ingested_at DESC,
        events.record_hash DESC
    ) AS billing_payload_rank
  FROM observed_events events
  WHERE events.source_system = 'codex'
    AND events.turn_id IS NOT NULL
),
codex_identity_stats AS (
  SELECT
    events.billing_identity_key,
    count(*) AS billing_duplicate_row_count,
    count(DISTINCT events.session_id) AS billing_duplicate_session_count,
    max(events.event_ts) FILTER (WHERE ranked.billing_origin_rank = 1) AS billing_origin_event_ts,
    max(events.event_ts) FILTER (WHERE ranked.billing_latest_rank = 1) AS billing_latest_event_ts,
    max(events.session_id) FILTER (WHERE ranked.billing_origin_rank = 1) AS billing_origin_session_id,
    max(events.session_id) FILTER (WHERE ranked.billing_latest_rank = 1) AS billing_latest_session_id,
    max(events.forked_from_session_id) FILTER (WHERE ranked.billing_origin_rank = 1) AS billing_origin_forked_from_session_id,
    max(events.forked_from_session_id) FILTER (WHERE ranked.billing_latest_rank = 1) AS billing_latest_forked_from_session_id,
    max(events.record_hash) FILTER (WHERE ranked.billing_origin_rank = 1) AS billing_origin_record_hash,
    max(events.record_hash) FILTER (WHERE ranked.billing_latest_rank = 1) AS billing_latest_record_hash,
    max(events.record_hash) FILTER (WHERE ranked.billing_payload_rank = 1) AS billing_selected_record_hash,
    max(events.parser_version) FILTER (WHERE ranked.billing_payload_rank = 1) AS billing_selected_parser_version,
    max(events.logical_key) FILTER (WHERE ranked.billing_payload_rank = 1) AS billing_selected_logical_key,
    count(DISTINCT jsonb_build_array(
      events.provider,
      events.model_requested,
      events.model_used,
      events.input_tokens,
      events.cached_input_tokens,
      events.output_tokens,
      events.reasoning_tokens,
      events.tool_tokens,
      events.total_tokens,
      events.context_window
    )::text) > 1 AS billing_payload_conflict
  FROM observed_events events
  JOIN codex_identity_ranked ranked
    ON events.record_hash = ranked.record_hash
  GROUP BY events.billing_identity_key
),
attributed_events AS (
  SELECT
    events.*,
    coalesce(ranked.billing_origin_rank, 1) AS billing_origin_rank,
    coalesce(ranked.billing_latest_rank, 1) AS billing_latest_rank,
    coalesce(ranked.billing_payload_rank, 1) AS billing_payload_rank,
    coalesce(ranked.billing_latest_rank, 1) AS billing_turn_rank,
    coalesce(stats.billing_duplicate_row_count, 1) AS billing_duplicate_row_count,
    coalesce(stats.billing_duplicate_session_count, 1) AS billing_duplicate_session_count,
    coalesce(ranked.billing_origin_rank, 1) = 1 AS billing_is_origin_observation,
    coalesce(ranked.billing_latest_rank, 1) = 1 AS billing_is_latest_observation,
    coalesce(ranked.billing_payload_rank, 1) = 1 AS billing_is_selected_observation,
    coalesce(stats.billing_duplicate_session_count, 1) > 1 AS billing_replay_suspected,
    coalesce(stats.billing_origin_event_ts, events.event_ts) AS billing_origin_event_ts,
    coalesce(stats.billing_latest_event_ts, events.event_ts) AS billing_latest_event_ts,
    coalesce(stats.billing_origin_session_id, events.session_id) AS billing_origin_session_id,
    coalesce(stats.billing_latest_session_id, events.session_id) AS billing_latest_session_id,
    coalesce(stats.billing_origin_forked_from_session_id, events.forked_from_session_id) AS billing_origin_forked_from_session_id,
    coalesce(stats.billing_latest_forked_from_session_id, events.forked_from_session_id) AS billing_latest_forked_from_session_id,
    coalesce(stats.billing_origin_record_hash, events.record_hash) AS billing_origin_record_hash,
    coalesce(stats.billing_latest_record_hash, events.record_hash) AS billing_latest_record_hash,
    coalesce(stats.billing_selected_record_hash, events.record_hash) AS billing_selected_record_hash,
    coalesce(stats.billing_selected_parser_version, events.parser_version) AS billing_selected_parser_version,
    coalesce(stats.billing_selected_logical_key, events.logical_key) AS billing_selected_logical_key,
    coalesce(stats.billing_payload_conflict, false) AS billing_payload_conflict,
    CASE
      WHEN events.source_system = 'codex'
        AND events.turn_id IS NOT NULL
      THEN 'richest_payload_origin_time'
      ELSE 'observed_row'
    END AS billing_observation_selection_mode,
    CASE
      WHEN events.source_system = 'codex'
        AND events.turn_id IS NOT NULL
      THEN coalesce(stats.billing_origin_event_ts, events.event_ts)
      ELSE events.event_ts
    END AS pricing_basis_event_ts
  FROM observed_events events
  LEFT JOIN codex_identity_ranked ranked
    ON events.record_hash = ranked.record_hash
  LEFT JOIN codex_identity_stats stats
    ON events.billing_identity_key = stats.billing_identity_key
   AND events.source_system = 'codex'
   AND events.turn_id IS NOT NULL
),
priced_events AS (
  SELECT
    events.*,
    pricing.public_api_model_id,
    pricing.pricing_tier,
    pricing.pricing_currency AS source_pricing_currency,
    pricing.input_rate_per_1m,
    pricing.cached_input_rate_per_1m,
    pricing.output_rate_per_1m,
    pricing.effective_from AS pricing_effective_from,
    pricing.effective_to AS pricing_effective_to,
    pricing.source_url AS pricing_source_url,
    pricing.source_observed_at AS pricing_source_observed_at
  FROM attributed_events events
  LEFT JOIN LATERAL (
    SELECT history.*
    FROM __LLM_SCHEMA__.llm_public_model_pricing_history history
    WHERE history.provider = events.provider
      AND history.public_api_model_id = events.model_key
      AND history.pricing_tier = 'standard'
      AND events.pricing_basis_event_ts >= history.effective_from
      AND (history.effective_to IS NULL OR events.pricing_basis_event_ts < history.effective_to)
      AND coalesce(events.input_tokens, 0) BETWEEN history.input_tokens_min AND history.input_tokens_max
    ORDER BY history.effective_from DESC, history.pricing_id DESC
    LIMIT 1
  ) pricing ON true
),
fx_events AS (
  SELECT
    events.*,
    fx.rate_date AS fx_rate_date,
    fx.rate_value AS pricing_to_aud_rate,
    fx.source_url AS fx_source_url
  FROM priced_events events
  LEFT JOIN LATERAL (
    SELECT history.*
    FROM __LLM_SCHEMA__.llm_fx_rate_history history
    WHERE history.base_currency = 'USD'
      AND history.quote_currency = 'AUD'
      AND history.rate_date <= (events.pricing_basis_event_ts AT TIME ZONE 'Australia/Sydney')::date
    ORDER BY history.rate_date DESC, history.fx_rate_id DESC
    LIMIT 1
  ) fx ON events.source_pricing_currency = 'USD'
),
cost_components AS (
  SELECT
    events.*,
    CASE
      WHEN events.input_rate_per_1m IS NULL THEN NULL
      ELSE round((events.billable_uncached_input_tokens::numeric / 1000000::numeric) * events.input_rate_per_1m, 8)
    END AS source_uncached_input_cost,
    CASE
      WHEN events.cached_input_rate_per_1m IS NULL THEN NULL
      ELSE round((events.billable_cached_input_tokens::numeric / 1000000::numeric) * events.cached_input_rate_per_1m, 8)
    END AS source_cached_input_cost,
    CASE
      WHEN events.output_rate_per_1m IS NULL THEN NULL
      ELSE round((events.billable_output_tokens::numeric / 1000000::numeric) * events.output_rate_per_1m, 8)
    END AS source_output_cost
  FROM fx_events events
)
SELECT
  events.record_hash,
  events.logical_key,
  events.parser_version,
  events.ingest_run_id,
  events.source_system,
  events.source_kind,
  events.source_path,
  events.source_path_hash,
  events.source_row_id,
  events.ingested_at,
  events.event_ts,
  events.session_id,
  events.turn_id,
  events.forked_from_session_id,
  events.project_key,
  events.project_path,
  events.cwd,
  events.tool_name,
  events.actor,
  events.provider,
  events.model_requested,
  events.model_used,
  events.model_key,
  events.ok,
  events.event_status,
  events.error_category,
  events.input_tokens,
  events.cached_input_tokens,
  events.output_tokens,
  events.reasoning_tokens,
  events.tool_tokens,
  events.total_tokens,
  events.billable_uncached_input_tokens,
  events.billable_cached_input_tokens,
  events.billable_output_tokens,
  events.cumulative_input_tokens,
  events.cumulative_cached_input_tokens,
  events.cumulative_output_tokens,
  events.cumulative_reasoning_tokens,
  events.cumulative_total_tokens,
  events.context_window,
  events.rate_limit_id,
  events.rate_limit_name,
  events.primary_used_percent,
  events.primary_window_minutes,
  events.primary_resets_at,
  events.secondary_used_percent,
  events.secondary_window_minutes,
  events.secondary_resets_at,
  events.credits_balance,
  events.credits_unlimited,
  events.raw,
  events.billing_identity_key,
  events.billing_attribution_mode,
  events.billing_duplicate_row_count,
  events.billing_duplicate_session_count,
  events.billing_origin_rank,
  events.billing_latest_rank,
  events.billing_payload_rank,
  events.billing_turn_rank,
  events.billing_is_origin_observation,
  events.billing_is_latest_observation,
  events.billing_is_selected_observation,
  events.billing_replay_suspected,
  events.billing_origin_event_ts,
  events.billing_latest_event_ts,
  events.billing_origin_session_id,
  events.billing_latest_session_id,
  events.billing_origin_forked_from_session_id,
  events.billing_latest_forked_from_session_id,
  events.billing_origin_record_hash,
  events.billing_latest_record_hash,
  events.billing_selected_record_hash,
  events.billing_selected_parser_version,
  events.billing_selected_logical_key,
  events.billing_observation_selection_mode,
  events.billing_payload_conflict,
  events.pricing_basis_event_ts,
  events.public_api_model_id,
  events.pricing_tier,
  events.source_pricing_currency,
  events.pricing_effective_from,
  events.pricing_effective_to,
  events.pricing_source_url,
  events.pricing_source_observed_at,
  events.fx_rate_date,
  events.fx_source_url,
  CASE
    WHEN events.source_pricing_currency = 'AUD' THEN 1::numeric(18, 10)
    WHEN events.source_pricing_currency = 'USD' THEN events.pricing_to_aud_rate
    ELSE NULL
  END AS pricing_to_aud_rate,
  events.source_uncached_input_cost,
  events.source_cached_input_cost,
  events.source_output_cost,
  CASE
    WHEN events.source_uncached_input_cost IS NULL
      OR events.source_cached_input_cost IS NULL
      OR events.source_output_cost IS NULL
    THEN NULL
    ELSE round(
      events.source_uncached_input_cost
      + events.source_cached_input_cost
      + events.source_output_cost,
      8
    )
  END AS source_total_cost,
  'AUD'::text AS reporting_currency,
  CASE
    WHEN events.source_pricing_currency = 'AUD' THEN events.source_uncached_input_cost
    WHEN events.source_uncached_input_cost IS NULL OR events.pricing_to_aud_rate IS NULL THEN NULL
    ELSE round(events.source_uncached_input_cost * events.pricing_to_aud_rate, 8)
  END AS aud_uncached_input_cost,
  CASE
    WHEN events.source_pricing_currency = 'AUD' THEN events.source_cached_input_cost
    WHEN events.source_cached_input_cost IS NULL OR events.pricing_to_aud_rate IS NULL THEN NULL
    ELSE round(events.source_cached_input_cost * events.pricing_to_aud_rate, 8)
  END AS aud_cached_input_cost,
  CASE
    WHEN events.source_pricing_currency = 'AUD' THEN events.source_output_cost
    WHEN events.source_output_cost IS NULL OR events.pricing_to_aud_rate IS NULL THEN NULL
    ELSE round(events.source_output_cost * events.pricing_to_aud_rate, 8)
  END AS aud_output_cost,
  CASE
    WHEN events.source_pricing_currency = 'AUD' THEN round(
      events.source_uncached_input_cost
      + events.source_cached_input_cost
      + events.source_output_cost,
      8
    )
    WHEN events.source_uncached_input_cost IS NULL
      OR events.source_cached_input_cost IS NULL
      OR events.source_output_cost IS NULL
      OR events.pricing_to_aud_rate IS NULL
    THEN NULL
    ELSE round(
      (
        events.source_uncached_input_cost
        + events.source_cached_input_cost
        + events.source_output_cost
      ) * events.pricing_to_aud_rate,
      8
    )
  END AS aud_total_cost,
  CASE
    WHEN events.provider IS NULL THEN 'unpriced_missing_provider'
    WHEN events.model_key = 'unknown' THEN 'unpriced_missing_model'
    WHEN events.source_pricing_currency IS NULL THEN 'unpriced_missing_price_history'
    WHEN events.source_pricing_currency = 'AUD' THEN 'priced_exact'
    WHEN events.source_pricing_currency = 'USD' AND events.pricing_to_aud_rate IS NULL THEN 'unpriced_missing_fx'
    WHEN events.source_pricing_currency = 'USD' THEN 'priced_exact'
    ELSE 'unpriced_missing_fx'
  END AS cost_status
FROM cost_components events;

CREATE OR REPLACE VIEW __LLM_SCHEMA__.llm_usage_public_api_costs AS
SELECT
  events.record_hash,
  events.logical_key,
  events.parser_version,
  events.ingest_run_id,
  events.source_system,
  events.source_kind,
  events.source_path,
  events.source_path_hash,
  events.source_row_id,
  events.ingested_at,
  CASE
    WHEN events.source_system = 'codex'
      AND events.turn_id IS NOT NULL
    THEN events.billing_origin_event_ts
    ELSE events.event_ts
  END AS event_ts,
  events.session_id,
  events.turn_id,
  events.forked_from_session_id,
  events.project_key,
  events.project_path,
  events.cwd,
  events.tool_name,
  events.actor,
  events.provider,
  events.model_requested,
  events.model_used,
  events.model_key,
  events.ok,
  events.event_status,
  events.error_category,
  events.input_tokens,
  events.cached_input_tokens,
  events.output_tokens,
  events.reasoning_tokens,
  events.tool_tokens,
  events.total_tokens,
  events.billable_uncached_input_tokens,
  events.billable_cached_input_tokens,
  events.billable_output_tokens,
  events.cumulative_input_tokens,
  events.cumulative_cached_input_tokens,
  events.cumulative_output_tokens,
  events.cumulative_reasoning_tokens,
  events.cumulative_total_tokens,
  events.context_window,
  events.rate_limit_id,
  events.rate_limit_name,
  events.primary_used_percent,
  events.primary_window_minutes,
  events.primary_resets_at,
  events.secondary_used_percent,
  events.secondary_window_minutes,
  events.secondary_resets_at,
  events.credits_balance,
  events.credits_unlimited,
  events.raw,
  events.billing_identity_key,
  CASE
    WHEN events.source_system = 'codex'
      AND events.source_kind = 'interactive_turn_segment'
      AND events.turn_id IS NOT NULL
    THEN 'codex_turn_segment_origin'
    WHEN events.source_system = 'codex'
      AND events.source_kind = 'interactive_turn'
      AND events.turn_id IS NOT NULL
    THEN 'codex_turn_origin'
    ELSE events.billing_attribution_mode
  END AS billing_attribution_mode,
  events.billing_duplicate_row_count,
  events.billing_duplicate_session_count,
  events.billing_origin_rank,
  events.billing_latest_rank,
  events.billing_payload_rank,
  events.billing_turn_rank,
  events.billing_is_origin_observation,
  events.billing_is_latest_observation,
  events.billing_is_selected_observation,
  events.billing_replay_suspected,
  events.billing_origin_event_ts,
  events.billing_latest_event_ts,
  events.billing_origin_session_id,
  events.billing_latest_session_id,
  events.billing_origin_forked_from_session_id,
  events.billing_latest_forked_from_session_id,
  events.billing_origin_record_hash,
  events.billing_latest_record_hash,
  events.billing_selected_record_hash,
  events.billing_selected_parser_version,
  events.billing_selected_logical_key,
  events.billing_observation_selection_mode,
  events.billing_payload_conflict,
  events.pricing_basis_event_ts,
  events.public_api_model_id,
  events.pricing_tier,
  events.source_pricing_currency,
  events.pricing_effective_from,
  events.pricing_effective_to,
  events.pricing_source_url,
  events.pricing_source_observed_at,
  events.fx_rate_date,
  events.fx_source_url,
  events.pricing_to_aud_rate,
  events.source_uncached_input_cost,
  events.source_cached_input_cost,
  events.source_output_cost,
  events.source_total_cost,
  events.reporting_currency,
  events.aud_uncached_input_cost,
  events.aud_cached_input_cost,
  events.aud_output_cost,
  events.aud_total_cost,
  events.cost_status
FROM __LLM_SCHEMA__.llm_usage_public_api_costs_observed events
WHERE events.billing_is_selected_observation;
