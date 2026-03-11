#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: test_cost_view_canonicalization.sh [options]

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
  db_schema="llm_usage_test_canonical_$(date +%s)"
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
  turn_id,
  forked_from_session_id,
  project_key,
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
  'codex_turn_original',
  'codex',
  'interactive_turn',
  'test-source',
  'turn-1',
  timestamptz '2026-03-10 01:00:00+00',
  'session-a',
  'turn-1',
  null,
  'project-a',
  'openai',
  'gpt-5.4',
  1000,
  0,
  50,
  1050,
  '{"source_session_id":"session-a"}'::jsonb,
  'codex|interactive_turn|session-a|turn-1|turn-1',
  'test',
  'test-source-hash',
  'succeeded'
),
(
  'codex_turn_latest',
  'codex',
  'interactive_turn',
  'test-source',
  'turn-1',
  timestamptz '2026-03-10 02:00:00+00',
  'session-b',
  'turn-1',
  'session-a',
  'project-a',
  'openai',
  'gpt-5.4',
  1200,
  200,
  75,
  1275,
  '{"source_session_id":"session-b","forked_from_session_id":"session-a"}'::jsonb,
  'codex|interactive_turn|session-b|turn-1|turn-1',
  'test',
  'test-source-hash',
  'succeeded'
),
(
  'codex_turn_no_id',
  'codex',
  'interactive_turn',
  'test-source',
  'row-no-turn',
  timestamptz '2026-03-10 03:00:00+00',
  'session-c',
  null,
  null,
  'project-a',
  'openai',
  'gpt-5.4',
  300,
  0,
  10,
  310,
  '{}'::jsonb,
  'codex|interactive_turn|session-c||row-no-turn',
  'test',
  'test-source-hash',
  'succeeded'
),
(
  'gemini_rollup',
  'gemini',
  'mcp_tool_call',
  'test-source',
  'gemini-row-rollup',
  timestamptz '2026-03-10 04:00:00+00',
  'gemini-session',
  null,
  null,
  'project-g',
  'gemini',
  'gemini-3-flash-preview',
  500,
  0,
  20,
  520,
  '{}'::jsonb,
  'gemini|mcp_tool_call|gemini-session||gemini-row-rollup',
  'test',
  'test-source-hash',
  'succeeded'
),
(
  'gemini_detail',
  'gemini',
  'interactive_message',
  'test-source',
  'gemini-row-detail',
  timestamptz '2026-03-10 04:00:01+00',
  'gemini-session',
  null,
  null,
  'project-g',
  'gemini',
  'gemini-3-flash-preview',
  250,
  0,
  5,
  255,
  '{}'::jsonb,
  'gemini|interactive_message|gemini-session||gemini-row-detail',
  'test',
  'test-source-hash',
  'succeeded'
),
(
  'legacy_codex_turn_old_key',
  'codex',
  'interactive_turn',
  'legacy-source',
  'turn-legacy',
  timestamptz '2026-03-10 05:00:00+00',
  'legacy-session',
  'turn-legacy',
  null,
  'project-legacy',
  'openai',
  'gpt-5.4',
  400,
  0,
  20,
  420,
  '{}'::jsonb,
  'codex|interactive_turn|legacy-session|turn-legacy',
  'test',
  'legacy-source-hash',
  'succeeded'
),
(
  'legacy_codex_turn_new_key',
  'codex',
  'interactive_turn',
  'legacy-source',
  'turn-legacy',
  timestamptz '2026-03-10 05:00:00+00',
  'legacy-session',
  'turn-legacy',
  null,
  'project-legacy',
  'openai',
  'gpt-5.4',
  400,
  0,
  20,
  420,
  '{}'::jsonb,
  'codex|interactive_turn|legacy-session|turn-legacy|turn-legacy',
  'test',
  'legacy-source-hash',
  'succeeded'
);
SQL

llm_usage_render_sql_template "$seed_sql" "$db_schema" "$seed_sql_rendered"
llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$seed_sql_rendered" >/dev/null
rm -f "$seed_sql" "$seed_sql_rendered"

llm_usage_apply_schema "$db_url" "$db_schema" >/dev/null

assert_sql=$(mktemp)
assert_sql_rendered="$assert_sql.rendered"
cat > "$assert_sql" <<'SQL'
DO $$
DECLARE
  observed_count integer;
  canonical_count integer;
  observed_row record;
  latest_observed_row record;
  canonical_row record;
  fallback_row record;
  gemini_count integer;
  legacy_count integer;
BEGIN
  SELECT count(*)
  INTO observed_count
  FROM __LLM_SCHEMA__.llm_usage_public_api_costs_observed
  WHERE source_system = 'codex'
    AND turn_id = 'turn-1';

  IF observed_count <> 2 THEN
    RAISE EXCEPTION 'expected 2 observed rows for turn-1, got %', observed_count;
  END IF;

  SELECT count(*)
  INTO canonical_count
  FROM __LLM_SCHEMA__.llm_usage_public_api_costs
  WHERE source_system = 'codex'
    AND turn_id = 'turn-1';

  IF canonical_count <> 1 THEN
    RAISE EXCEPTION 'expected 1 canonical row for turn-1, got %', canonical_count;
  END IF;

  SELECT *
  INTO observed_row
  FROM __LLM_SCHEMA__.llm_usage_public_api_costs_observed
  WHERE record_hash = 'codex_turn_original';

  IF NOT observed_row.billing_is_origin_observation THEN
    RAISE EXCEPTION 'expected original row to be marked origin observation';
  END IF;
  IF observed_row.billing_is_latest_observation THEN
    RAISE EXCEPTION 'expected original replayed row to be non-latest';
  END IF;
  IF observed_row.billing_is_selected_observation THEN
    RAISE EXCEPTION 'expected original replayed row to be non-selected';
  END IF;
  IF observed_row.billing_duplicate_row_count <> 2 THEN
    RAISE EXCEPTION 'expected duplicate row count 2, got %', observed_row.billing_duplicate_row_count;
  END IF;
  IF observed_row.billing_duplicate_session_count <> 2 THEN
    RAISE EXCEPTION 'expected duplicate session count 2, got %', observed_row.billing_duplicate_session_count;
  END IF;
  IF NOT observed_row.billing_replay_suspected THEN
    RAISE EXCEPTION 'expected replay suspicion on original observed row';
  END IF;
  IF observed_row.billing_origin_event_ts <> timestamptz '2026-03-10 01:00:00+00' THEN
    RAISE EXCEPTION 'unexpected billing origin event ts %', observed_row.billing_origin_event_ts;
  END IF;
  IF observed_row.pricing_basis_event_ts <> timestamptz '2026-03-10 01:00:00+00' THEN
    RAISE EXCEPTION 'unexpected pricing basis event ts %', observed_row.pricing_basis_event_ts;
  END IF;

  SELECT *
  INTO latest_observed_row
  FROM __LLM_SCHEMA__.llm_usage_public_api_costs_observed
  WHERE record_hash = 'codex_turn_latest';

  IF latest_observed_row.record_hash IS NULL THEN
    RAISE EXCEPTION 'missing latest observed row';
  END IF;
  IF latest_observed_row.billing_origin_rank <> 2 THEN
    RAISE EXCEPTION 'expected latest observed row origin rank 2, got %', latest_observed_row.billing_origin_rank;
  END IF;
  IF latest_observed_row.billing_latest_rank <> 1 THEN
    RAISE EXCEPTION 'expected latest observed row latest rank 1, got %', latest_observed_row.billing_latest_rank;
  END IF;
  IF latest_observed_row.billing_payload_rank <> 1 THEN
    RAISE EXCEPTION 'expected latest observed row payload rank 1, got %', latest_observed_row.billing_payload_rank;
  END IF;
  IF latest_observed_row.billing_turn_rank <> 1 THEN
    RAISE EXCEPTION 'expected latest observed row billing_turn_rank 1, got %', latest_observed_row.billing_turn_rank;
  END IF;
  IF latest_observed_row.billing_is_origin_observation THEN
    RAISE EXCEPTION 'expected latest observed row to be non-origin';
  END IF;
  IF NOT latest_observed_row.billing_is_latest_observation THEN
    RAISE EXCEPTION 'expected latest observed row to be marked latest';
  END IF;
  IF NOT latest_observed_row.billing_is_selected_observation THEN
    RAISE EXCEPTION 'expected latest observed row to be selected';
  END IF;

  SELECT *
  INTO canonical_row
  FROM __LLM_SCHEMA__.llm_usage_public_api_costs
  WHERE record_hash = 'codex_turn_latest';

  IF canonical_row.record_hash IS NULL THEN
    RAISE EXCEPTION 'missing selected canonical row';
  END IF;
  IF canonical_row.billing_identity_key <> 'turn-1' THEN
    RAISE EXCEPTION 'expected billing identity key turn-1, got %', canonical_row.billing_identity_key;
  END IF;
  IF canonical_row.billing_attribution_mode <> 'codex_turn_origin' THEN
    RAISE EXCEPTION 'expected codex_turn_origin attribution, got %', canonical_row.billing_attribution_mode;
  END IF;
  IF canonical_row.billing_observation_selection_mode <> 'richest_payload_origin_time' THEN
    RAISE EXCEPTION 'expected richest_payload_origin_time, got %', canonical_row.billing_observation_selection_mode;
  END IF;
  IF canonical_row.event_ts <> timestamptz '2026-03-10 01:00:00+00' THEN
    RAISE EXCEPTION 'expected canonical event ts at origin, got %', canonical_row.event_ts;
  END IF;
  IF canonical_row.pricing_basis_event_ts <> timestamptz '2026-03-10 01:00:00+00' THEN
    RAISE EXCEPTION 'expected canonical pricing basis at origin, got %', canonical_row.pricing_basis_event_ts;
  END IF;
  IF canonical_row.session_id <> 'session-b' THEN
    RAISE EXCEPTION 'expected canonical row to keep selected payload session, got %', canonical_row.session_id;
  END IF;
  IF canonical_row.billing_origin_session_id <> 'session-a' THEN
    RAISE EXCEPTION 'expected billing_origin_session_id session-a, got %', canonical_row.billing_origin_session_id;
  END IF;
  IF canonical_row.billing_latest_session_id <> 'session-b' THEN
    RAISE EXCEPTION 'expected billing_latest_session_id session-b, got %', canonical_row.billing_latest_session_id;
  END IF;
  IF canonical_row.billing_selected_record_hash <> 'codex_turn_latest' THEN
    RAISE EXCEPTION 'expected selected record hash codex_turn_latest, got %', canonical_row.billing_selected_record_hash;
  END IF;
  IF canonical_row.billing_origin_record_hash <> 'codex_turn_original' THEN
    RAISE EXCEPTION 'expected origin record hash codex_turn_original, got %', canonical_row.billing_origin_record_hash;
  END IF;
  IF canonical_row.billing_latest_record_hash <> 'codex_turn_latest' THEN
    RAISE EXCEPTION 'expected latest record hash codex_turn_latest, got %', canonical_row.billing_latest_record_hash;
  END IF;
  IF canonical_row.input_tokens <> 1200 OR canonical_row.cached_input_tokens <> 200 OR canonical_row.output_tokens <> 75 THEN
    RAISE EXCEPTION 'expected canonical row to keep richest payload, got input=% cached=% output=%', canonical_row.input_tokens, canonical_row.cached_input_tokens, canonical_row.output_tokens;
  END IF;
  IF canonical_row.forked_from_session_id <> 'session-a' THEN
    RAISE EXCEPTION 'expected forked_from_session_id session-a, got %', canonical_row.forked_from_session_id;
  END IF;
  IF NOT canonical_row.billing_payload_conflict THEN
    RAISE EXCEPTION 'expected payload conflict flag for differing duplicate payloads';
  END IF;

  SELECT *
  INTO fallback_row
  FROM __LLM_SCHEMA__.llm_usage_public_api_costs
  WHERE record_hash = 'codex_turn_no_id';

  IF fallback_row.record_hash IS NULL THEN
    RAISE EXCEPTION 'missing null-turn fallback row';
  END IF;
  IF fallback_row.billing_attribution_mode <> 'codex_logical_key_fallback' THEN
    RAISE EXCEPTION 'expected codex_logical_key_fallback, got %', fallback_row.billing_attribution_mode;
  END IF;
  IF fallback_row.billing_identity_key <> 'codex|interactive_turn|session-c||row-no-turn' THEN
    RAISE EXCEPTION 'unexpected fallback identity key %', fallback_row.billing_identity_key;
  END IF;
  IF NOT fallback_row.billing_is_selected_observation THEN
    RAISE EXCEPTION 'expected null-turn fallback row to stay selected';
  END IF;
  IF fallback_row.pricing_basis_event_ts <> fallback_row.event_ts THEN
    RAISE EXCEPTION 'expected null-turn pricing basis to match event_ts';
  END IF;

  SELECT count(*)
  INTO gemini_count
  FROM __LLM_SCHEMA__.llm_usage_public_api_costs_observed
  WHERE session_id = 'gemini-session';

  IF gemini_count <> 1 THEN
    RAISE EXCEPTION 'expected gemini observed view to keep only rollup row, got %', gemini_count;
  END IF;

  SELECT count(*)
  INTO legacy_count
  FROM __LLM_SCHEMA__.llm_usage_events
  WHERE session_id = 'legacy-session'
    AND turn_id = 'turn-legacy';

  IF legacy_count <> 1 THEN
    RAISE EXCEPTION 'expected legacy logical-key cleanup to keep 1 row, got %', legacy_count;
  END IF;
END $$;
SQL

llm_usage_render_sql_template "$assert_sql" "$db_schema" "$assert_sql_rendered"
llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$assert_sql_rendered" >/dev/null
rm -f "$assert_sql" "$assert_sql_rendered"

echo "PASS: canonical billing regression checks passed in schema $db_schema"
