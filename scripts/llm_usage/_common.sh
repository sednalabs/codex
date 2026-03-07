#!/usr/bin/env bash
set -euo pipefail

llm_usage_script_dir() {
  cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd
}

llm_usage_require_commands() {
  local cmd
  for cmd in "$@"; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
      echo "missing required command: $cmd" >&2
      exit 1
    fi
  done
}

llm_usage_count_lines() {
  local file=$1
  if [ ! -f "$file" ]; then
    echo 0
    return 0
  fi
  awk 'END { print NR + 0 }' "$file"
}

llm_usage_default_codex_config_path() {
  printf '%s\n' "${CODEX_CONFIG_TOML:-$HOME/.codex/config.toml}"
}

llm_usage_db_url_from_codex_config() {
  local config_path=${1:-$(llm_usage_default_codex_config_path)}

  if [ ! -f "$config_path" ]; then
    return 1
  fi

  sed -n '/^\[mcp_servers\.postgres\.env\]/,/^\[/{
    s/^[[:space:]]*DATABASE_URI[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p
  }' "$config_path" | head -n 1
}

llm_usage_resolve_db_url() {
  local db_url=${1:-}
  local config_path

  if [ -n "$db_url" ]; then
    printf '%s\n' "$db_url"
    return 0
  fi

  config_path=$(llm_usage_default_codex_config_path)
  llm_usage_db_url_from_codex_config "$config_path"
}

llm_usage_require_db_url() {
  local db_url=${1:-}
  if [ -z "$db_url" ]; then
    echo "missing Postgres URL; pass --db-url, set LLM_USAGE_DB_URL, or configure [mcp_servers.postgres.env].DATABASE_URI in $(llm_usage_default_codex_config_path)" >&2
    exit 1
  fi
}
llm_usage_require_schema_name() {
  local db_schema=${1:-}
  if [ -z "$db_schema" ]; then
    echo "missing schema name; pass --schema or set LLM_USAGE_DB_SCHEMA" >&2
    exit 1
  fi
  if [[ ! "$db_schema" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
    echo "invalid schema name: $db_schema" >&2
    echo "schema names must match ^[A-Za-z_][A-Za-z0-9_]*$" >&2
    exit 1
  fi
}

llm_usage_render_sql_template() {
  local template_file=$1
  local db_schema=$2
  local rendered_file=$3

  llm_usage_require_schema_name "$db_schema"
  sed "s/__LLM_SCHEMA__/$db_schema/g" "$template_file" > "$rendered_file"
}

llm_usage_apply_schema() {
  local db_url=$1
  local db_schema=$2
  local script_dir rendered_sql

  script_dir=$(llm_usage_script_dir)
  rendered_sql=$(mktemp)
  llm_usage_render_sql_template "$script_dir/ensure_schema.sql" "$db_schema" "$rendered_sql"
  psql "$db_url" -v ON_ERROR_STOP=1 -f "$rendered_sql"
  rm -f "$rendered_sql"
}

llm_usage_run_usage_ingest() {
  local db_url=$1
  local db_schema=$2
  local usage_file=$3
  local ingest_sql

  if [ ! -s "$usage_file" ]; then
    echo "No usage rows to ingest."
    return 0
  fi

  ingest_sql=$(mktemp)
  cat > "$ingest_sql" <<SQL
DROP TABLE IF EXISTS llm_usage_events_stage;

CREATE TEMP TABLE llm_usage_events_stage (
  raw jsonb NOT NULL
);

\copy llm_usage_events_stage (raw) FROM '$usage_file' WITH (FORMAT csv, DELIMITER E'\x02', QUOTE E'\x01', ESCAPE E'\x01')

INSERT INTO __LLM_SCHEMA__.llm_usage_events (
  record_hash,
  source_system,
  source_kind,
  source_path,
  source_row_id,
  event_ts,
  session_id,
  turn_id,
  project_key,
  project_path,
  cwd,
  tool_name,
  actor,
  provider,
  model_requested,
  model_used,
  ok,
  error_category,
  input_tokens,
  cached_input_tokens,
  output_tokens,
  reasoning_tokens,
  tool_tokens,
  total_tokens,
  cumulative_input_tokens,
  cumulative_cached_input_tokens,
  cumulative_output_tokens,
  cumulative_reasoning_tokens,
  cumulative_total_tokens,
  context_window,
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
  raw
)
SELECT
  md5(
    concat_ws(
      '|',
      coalesce(raw->>'source_system', ''),
      coalesce(raw->>'source_kind', ''),
      coalesce(raw->>'source_path', ''),
      coalesce(raw->>'source_row_id', ''),
      coalesce(raw->>'event_ts', ''),
      coalesce(raw->>'session_id', ''),
      coalesce(raw->>'turn_id', ''),
      raw::text
    )
  ) AS record_hash,
  raw->>'source_system' AS source_system,
  raw->>'source_kind' AS source_kind,
  raw->>'source_path' AS source_path,
  nullif(raw->>'source_row_id', '') AS source_row_id,
  (raw->>'event_ts')::timestamptz AS event_ts,
  raw->>'session_id' AS session_id,
  nullif(raw->>'turn_id', '') AS turn_id,
  nullif(raw->>'project_key', '') AS project_key,
  nullif(raw->>'project_path', '') AS project_path,
  nullif(raw->>'cwd', '') AS cwd,
  nullif(raw->>'tool_name', '') AS tool_name,
  nullif(raw->>'actor', '') AS actor,
  nullif(raw->>'provider', '') AS provider,
  nullif(raw->>'model_requested', '') AS model_requested,
  nullif(raw->>'model_used', '') AS model_used,
  CASE WHEN raw ? 'ok' THEN (raw->>'ok')::boolean ELSE NULL END AS ok,
  nullif(raw->>'error_category', '') AS error_category,
  nullif(raw->>'input_tokens', '')::bigint AS input_tokens,
  nullif(raw->>'cached_input_tokens', '')::bigint AS cached_input_tokens,
  nullif(raw->>'output_tokens', '')::bigint AS output_tokens,
  nullif(raw->>'reasoning_tokens', '')::bigint AS reasoning_tokens,
  nullif(raw->>'tool_tokens', '')::bigint AS tool_tokens,
  nullif(raw->>'total_tokens', '')::bigint AS total_tokens,
  nullif(raw->>'cumulative_input_tokens', '')::bigint AS cumulative_input_tokens,
  nullif(raw->>'cumulative_cached_input_tokens', '')::bigint AS cumulative_cached_input_tokens,
  nullif(raw->>'cumulative_output_tokens', '')::bigint AS cumulative_output_tokens,
  nullif(raw->>'cumulative_reasoning_tokens', '')::bigint AS cumulative_reasoning_tokens,
  nullif(raw->>'cumulative_total_tokens', '')::bigint AS cumulative_total_tokens,
  nullif(raw->>'context_window', '')::bigint AS context_window,
  nullif(raw->>'rate_limit_id', '') AS rate_limit_id,
  nullif(raw->>'rate_limit_name', '') AS rate_limit_name,
  nullif(raw->>'primary_used_percent', '')::double precision AS primary_used_percent,
  nullif(raw->>'primary_window_minutes', '')::bigint AS primary_window_minutes,
  nullif(raw->>'primary_resets_at', '')::timestamptz AS primary_resets_at,
  nullif(raw->>'secondary_used_percent', '')::double precision AS secondary_used_percent,
  nullif(raw->>'secondary_window_minutes', '')::bigint AS secondary_window_minutes,
  nullif(raw->>'secondary_resets_at', '')::timestamptz AS secondary_resets_at,
  nullif(raw->>'credits_balance', '') AS credits_balance,
  CASE WHEN raw ? 'credits_unlimited' THEN (raw->>'credits_unlimited')::boolean ELSE NULL END AS credits_unlimited,
  raw AS raw
FROM llm_usage_events_stage
ON CONFLICT (record_hash) DO NOTHING;

DROP TABLE IF EXISTS llm_usage_events_stage;
SQL

  llm_usage_render_sql_template "$ingest_sql" "$db_schema" "$ingest_sql.rendered"
  psql "$db_url" \
    -v ON_ERROR_STOP=1 \
    -f "$ingest_sql.rendered"
  rm -f "$ingest_sql" "$ingest_sql.rendered"
}

llm_usage_run_quota_ingest() {
  local db_url=$1
  local db_schema=$2
  local quota_file=$3
  local ingest_sql

  if [ ! -s "$quota_file" ]; then
    echo "No quota rows to ingest."
    return 0
  fi

  ingest_sql=$(mktemp)
  cat > "$ingest_sql" <<SQL
DROP TABLE IF EXISTS llm_quota_events_stage;

CREATE TEMP TABLE llm_quota_events_stage (
  raw jsonb NOT NULL
);

\copy llm_quota_events_stage (raw) FROM '$quota_file' WITH (FORMAT csv, DELIMITER E'\x02', QUOTE E'\x01', ESCAPE E'\x01')

INSERT INTO __LLM_SCHEMA__.llm_quota_events (
  record_hash,
  source_system,
  source_kind,
  source_path,
  source_row_id,
  event_ts,
  session_id,
  project_key,
  project_path,
  model_used,
  tool_name,
  error_message,
  reset_after_text,
  reset_after_seconds,
  raw
)
SELECT
  md5(
    concat_ws(
      '|',
      coalesce(raw->>'source_system', ''),
      coalesce(raw->>'source_kind', ''),
      coalesce(raw->>'source_path', ''),
      coalesce(raw->>'source_row_id', ''),
      coalesce(raw->>'event_ts', ''),
      coalesce(raw->>'session_id', ''),
      raw::text
    )
  ) AS record_hash,
  raw->>'source_system' AS source_system,
  raw->>'source_kind' AS source_kind,
  raw->>'source_path' AS source_path,
  nullif(raw->>'source_row_id', '') AS source_row_id,
  (raw->>'event_ts')::timestamptz AS event_ts,
  raw->>'session_id' AS session_id,
  nullif(raw->>'project_key', '') AS project_key,
  nullif(raw->>'project_path', '') AS project_path,
  nullif(raw->>'model_used', '') AS model_used,
  nullif(raw->>'tool_name', '') AS tool_name,
  raw->>'error_message' AS error_message,
  nullif(raw->>'reset_after_text', '') AS reset_after_text,
  nullif(raw->>'reset_after_seconds', '')::bigint AS reset_after_seconds,
  raw AS raw
FROM llm_quota_events_stage
ON CONFLICT (record_hash) DO NOTHING;

DROP TABLE IF EXISTS llm_quota_events_stage;
SQL

  llm_usage_render_sql_template "$ingest_sql" "$db_schema" "$ingest_sql.rendered"
  psql "$db_url" \
    -v ON_ERROR_STOP=1 \
    -f "$ingest_sql.rendered"
  rm -f "$ingest_sql" "$ingest_sql.rendered"
}
