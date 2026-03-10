#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: test_cost_view_cached_input.sh [options]

Options:
  --db-url URL       Postgres connection string. Defaults to LLM_USAGE_DB_URL or the postgres MCP DATABASE_URI in ~/.codex/config.toml.
  --schema NAME      Optional schema name. Defaults to a generated temporary schema.
  --keep-schema      Do not drop the test schema after completion.
  --help             Show this help.
USAGE
}

db_url=${LLM_USAGE_DB_URL:-}
db_schema=
keep_schema=0

while [ $# -gt 0 ]; do
  case "$1" in
    --db-url)
      db_url=${2:-}
      shift 2
      ;;
    --schema)
      db_schema=${2:-}
      shift 2
      ;;
    --keep-schema)
      keep_schema=1
      shift
      ;;
    --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

llm_usage_require_commands psql mktemp
db_url=$(llm_usage_resolve_db_url "$db_url" || true)
llm_usage_require_db_url "$db_url"

if [ -z "$db_schema" ]; then
  db_schema="llm_usage_test_cached_input_$(date +%s)"
fi
llm_usage_require_schema_name "$db_schema"

cleanup() {
  if [ "$keep_schema" -eq 1 ]; then
    echo "Kept schema $db_schema"
    return
  fi
  llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 <<SQL >/dev/null
DROP SCHEMA IF EXISTS "$db_schema" CASCADE;
SQL
}
trap cleanup EXIT

llm_usage_apply_schema "$db_url" "$db_schema"

seed_sql=$(mktemp)
seed_sql_rendered="$seed_sql.rendered"
cat > "$seed_sql" <<'SQL'
INSERT INTO __LLM_SCHEMA__.llm_fx_rate_history (
  base_currency,
  quote_currency,
  rate_date,
  rate_value,
  source_name,
  source_url,
  source_observed_at,
  raw
) VALUES (
  'USD',
  'AUD',
  date '2026-03-10',
  1.5,
  'unit_test',
  'https://example.invalid/fx',
  now(),
  '{}'::jsonb
);

INSERT INTO __LLM_SCHEMA__.llm_usage_events (
  record_hash,
  source_system,
  source_kind,
  source_path,
  source_row_id,
  event_ts,
  session_id,
  provider,
  model_used,
  input_tokens,
  cached_input_tokens,
  output_tokens,
  total_tokens,
  raw,
  logical_key,
  parser_version,
  source_path_hash,
  event_status
) VALUES
(
  'test_record_cached_discount_1',
  'codex',
  'interactive_turn',
  'test-source',
  'row-1',
  timestamptz '2026-03-10 01:00:00+00',
  'test-session-1',
  'openai',
  'gpt-5.4',
  1000,
  600,
  200,
  1200,
  '{}'::jsonb,
  'test|row|1',
  'test',
  'test-source-hash',
  'succeeded'
),
(
  'test_record_cached_discount_2',
  'codex',
  'interactive_turn',
  'test-source',
  'row-2',
  timestamptz '2026-03-10 01:01:00+00',
  'test-session-2',
  'openai',
  'gpt-5.4',
  500,
  700,
  0,
  500,
  '{}'::jsonb,
  'test|row|2',
  'test',
  'test-source-hash',
  'succeeded'
);

DO $$
DECLARE
  row1 record;
  row2 record;
BEGIN
  SELECT *
  INTO row1
  FROM __LLM_SCHEMA__.llm_usage_public_api_costs
  WHERE record_hash = 'test_record_cached_discount_1';

  IF row1.record_hash IS NULL THEN
    RAISE EXCEPTION 'missing row1 from llm_usage_public_api_costs';
  END IF;

  IF row1.billable_uncached_input_tokens <> 400 THEN
    RAISE EXCEPTION 'row1 uncached tokens expected 400 got %', row1.billable_uncached_input_tokens;
  END IF;
  IF row1.billable_cached_input_tokens <> 600 THEN
    RAISE EXCEPTION 'row1 cached tokens expected 600 got %', row1.billable_cached_input_tokens;
  END IF;
  IF abs(row1.source_uncached_input_cost - 0.00100000) > 0.00000001 THEN
    RAISE EXCEPTION 'row1 source_uncached_input_cost expected 0.00100000 got %', row1.source_uncached_input_cost;
  END IF;
  IF abs(row1.source_cached_input_cost - 0.00015000) > 0.00000001 THEN
    RAISE EXCEPTION 'row1 source_cached_input_cost expected 0.00015000 got %', row1.source_cached_input_cost;
  END IF;
  IF abs(row1.source_output_cost - 0.00300000) > 0.00000001 THEN
    RAISE EXCEPTION 'row1 source_output_cost expected 0.00300000 got %', row1.source_output_cost;
  END IF;
  IF abs(row1.source_total_cost - 0.00415000) > 0.00000001 THEN
    RAISE EXCEPTION 'row1 source_total_cost expected 0.00415000 got %', row1.source_total_cost;
  END IF;
  IF abs(row1.aud_total_cost - 0.00622500) > 0.00000001 THEN
    RAISE EXCEPTION 'row1 aud_total_cost expected 0.00622500 got %', row1.aud_total_cost;
  END IF;

  SELECT *
  INTO row2
  FROM __LLM_SCHEMA__.llm_usage_public_api_costs
  WHERE record_hash = 'test_record_cached_discount_2';

  IF row2.record_hash IS NULL THEN
    RAISE EXCEPTION 'missing row2 from llm_usage_public_api_costs';
  END IF;

  IF row2.billable_uncached_input_tokens <> 0 THEN
    RAISE EXCEPTION 'row2 uncached tokens expected clamp to 0 got %', row2.billable_uncached_input_tokens;
  END IF;
  IF row2.billable_cached_input_tokens <> 700 THEN
    RAISE EXCEPTION 'row2 cached tokens expected 700 got %', row2.billable_cached_input_tokens;
  END IF;
  IF abs(row2.source_uncached_input_cost - 0.00000000) > 0.00000001 THEN
    RAISE EXCEPTION 'row2 source_uncached_input_cost expected 0 got %', row2.source_uncached_input_cost;
  END IF;
  IF abs(row2.source_cached_input_cost - 0.00017500) > 0.00000001 THEN
    RAISE EXCEPTION 'row2 source_cached_input_cost expected 0.00017500 got %', row2.source_cached_input_cost;
  END IF;
END $$;
SQL

llm_usage_render_sql_template "$seed_sql" "$db_schema" "$seed_sql_rendered"
llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$seed_sql_rendered" >/dev/null
rm -f "$seed_sql" "$seed_sql_rendered"

echo "PASS: cached-input discount regression checks passed in schema $db_schema"
