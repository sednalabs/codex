#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: test_codex_mixed_model_segments.sh [options]

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
sessions_root=

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
  db_schema="llm_usage_test_codex_segments_$(date +%s)"
fi
llm_usage_require_schema_name "$db_schema"

sessions_root=$(mktemp -d)
cleanup() {
  rm -rf "$sessions_root"
  if [ "$keep_schema" -eq 1 ]; then
    echo "Kept schema $db_schema"
    return
  fi
  llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 <<SQL >/dev/null
DROP SCHEMA IF EXISTS "$db_schema" CASCADE;
SQL
}
trap cleanup EXIT

mkdir -p "$sessions_root/2026/03/10"
cat > "$sessions_root/2026/03/10/rollout-mixed-model.jsonl" <<'JSONL'
{"type":"session_meta","payload":{"id":"session-mixed","cwd":"/repo","model":"gpt-5.4","model_provider":"openai"}}
{"timestamp":"2026-03-10T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-0","model_used":"gpt-5.4","provider":"openai","model_context_window":200000}}
{"timestamp":"2026-03-10T00:00:01Z","type":"event_msg","payload":{"type":"token_count","provider":"openai","model_used":"gpt-5.4","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":110},"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":110},"model_context_window":200000},"rate_limits":null}}
{"timestamp":"2026-03-10T00:00:02Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-0","compaction_events_in_turn":0}}
{"timestamp":"2026-03-10T00:01:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1","model_used":"gpt-5.3-codex","provider":"openai","model_context_window":128000}}
{"timestamp":"2026-03-10T00:01:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":140,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":150},"last_token_usage":{"input_tokens":40,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":40},"model_context_window":200000},"rate_limits":null}}
{"timestamp":"2026-03-10T00:01:02Z","type":"turn_context","payload":{"turn_id":"turn-1","cwd":"/repo","model":"gpt-5.3-codex","model_context_window":128000}}
{"timestamp":"2026-03-10T00:01:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":190,"cached_input_tokens":0,"output_tokens":20,"reasoning_output_tokens":0,"total_tokens":210},"last_token_usage":{"input_tokens":50,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":60},"model_context_window":128000},"rate_limits":null}}
{"timestamp":"2026-03-10T00:01:04Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","compaction_events_in_turn":1}}
{"timestamp":"2026-03-10T00:02:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2","model_used":"gpt-5.4","provider":"openai","model_context_window":200000}}
{"timestamp":"2026-03-10T00:02:01Z","type":"event_msg","payload":{"type":"token_count","provider":"openai","model_used":"gpt-5.4","info":{"total_token_usage":{"input_tokens":220,"cached_input_tokens":0,"output_tokens":20,"reasoning_output_tokens":0,"total_tokens":240},"last_token_usage":{"input_tokens":30,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":30},"model_context_window":200000},"rate_limits":null}}
{"timestamp":"2026-03-10T00:02:02Z","type":"turn_context","payload":{"turn_id":"turn-2","cwd":"/repo","model":"gpt-5.1-codex-mini","model_context_window":128000}}
{"timestamp":"2026-03-10T00:02:03Z","type":"event_msg","payload":{"type":"token_count","provider":"openai","model_used":"gpt-5.1-codex-mini","info":{"total_token_usage":{"input_tokens":235,"cached_input_tokens":0,"output_tokens":25,"reasoning_output_tokens":0,"total_tokens":260},"last_token_usage":{"input_tokens":15,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":20},"model_context_window":128000},"rate_limits":null}}
{"timestamp":"2026-03-10T00:02:04Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-2","compaction_events_in_turn":1}}
JSONL

llm_usage_apply_schema "$db_url" "$db_schema" >/dev/null
"$script_dir/ingest_codex_rollouts_to_postgres.sh" \
  --db-url "$db_url" \
  --schema "$db_schema" \
  --sessions-root "$sessions_root" \
  --skip-schema >/dev/null

assert_sql=$(mktemp)
assert_sql_rendered="$assert_sql.rendered"
cat > "$assert_sql" <<'SQL'
DO $$
DECLARE
  segment_count integer;
  turn_one_first record;
  turn_one_second record;
  turn_two_first record;
  turn_two_second record;
  summary_gpt54 record;
  summary_gpt53 record;
  summary_mini record;
BEGIN
  SELECT count(*)
  INTO segment_count
  FROM __LLM_SCHEMA__.llm_usage_events
  WHERE source_system = 'codex'
    AND source_kind = 'interactive_turn_segment';

  IF segment_count <> 5 THEN
    RAISE EXCEPTION 'expected 5 Codex segment rows, got %', segment_count;
  END IF;

  SELECT *
  INTO turn_one_first
  FROM __LLM_SCHEMA__.llm_usage_events
  WHERE turn_id = 'turn-1'
    AND source_row_id = 'turn-1:seg-0';

  IF turn_one_first.model_used <> 'gpt-5.4' THEN
    RAISE EXCEPTION 'unexpected turn-1 seg-0 model %', turn_one_first.model_used;
  END IF;
  IF turn_one_first.total_tokens <> 40 THEN
    RAISE EXCEPTION 'unexpected turn-1 seg-0 total tokens %', turn_one_first.total_tokens;
  END IF;
  IF turn_one_first.event_ts <> timestamptz '2026-03-10 00:01:01+00' THEN
    RAISE EXCEPTION 'unexpected turn-1 seg-0 event_ts %', turn_one_first.event_ts;
  END IF;
  IF turn_one_first.raw->>'model_inference_basis' <> 'pre_turn_previous_model' THEN
    RAISE EXCEPTION 'unexpected turn-1 seg-0 inference basis %', turn_one_first.raw->>'model_inference_basis';
  END IF;
  IF turn_one_first.raw->>'mixed_model_turn' <> 'true' THEN
    RAISE EXCEPTION 'expected turn-1 to be marked mixed-model';
  END IF;
  IF turn_one_first.cumulative_total_tokens IS NOT NULL THEN
    RAISE EXCEPTION 'expected non-final segment to omit cumulative totals';
  END IF;

  SELECT *
  INTO turn_one_second
  FROM __LLM_SCHEMA__.llm_usage_events
  WHERE turn_id = 'turn-1'
    AND source_row_id = 'turn-1:seg-1';

  IF turn_one_second.model_used <> 'gpt-5.3-codex' THEN
    RAISE EXCEPTION 'unexpected turn-1 seg-1 model %', turn_one_second.model_used;
  END IF;
  IF turn_one_second.total_tokens <> 60 THEN
    RAISE EXCEPTION 'unexpected turn-1 seg-1 total tokens %', turn_one_second.total_tokens;
  END IF;
  IF turn_one_second.event_ts <> timestamptz '2026-03-10 00:01:03+00' THEN
    RAISE EXCEPTION 'unexpected turn-1 seg-1 event_ts %', turn_one_second.event_ts;
  END IF;
  IF turn_one_second.raw->>'model_inference_basis' <> 'turn_context_model' THEN
    RAISE EXCEPTION 'unexpected turn-1 seg-1 inference basis %', turn_one_second.raw->>'model_inference_basis';
  END IF;
  IF turn_one_second.cumulative_total_tokens <> 210 THEN
    RAISE EXCEPTION 'expected turn-1 final segment cumulative total 210, got %', turn_one_second.cumulative_total_tokens;
  END IF;

  SELECT *
  INTO turn_two_first
  FROM __LLM_SCHEMA__.llm_usage_events
  WHERE turn_id = 'turn-2'
    AND source_row_id = 'turn-2:seg-0';

  IF turn_two_first.model_used <> 'gpt-5.4' THEN
    RAISE EXCEPTION 'unexpected turn-2 seg-0 model %', turn_two_first.model_used;
  END IF;
  IF turn_two_first.raw->>'model_inference_basis' <> 'token_count_model_used' THEN
    RAISE EXCEPTION 'unexpected turn-2 seg-0 inference basis %', turn_two_first.raw->>'model_inference_basis';
  END IF;
  IF turn_two_first.total_tokens <> 30 THEN
    RAISE EXCEPTION 'unexpected turn-2 seg-0 total tokens %', turn_two_first.total_tokens;
  END IF;

  SELECT *
  INTO turn_two_second
  FROM __LLM_SCHEMA__.llm_usage_events
  WHERE turn_id = 'turn-2'
    AND source_row_id = 'turn-2:seg-1';

  IF turn_two_second.model_used <> 'gpt-5.1-codex-mini' THEN
    RAISE EXCEPTION 'unexpected turn-2 seg-1 model %', turn_two_second.model_used;
  END IF;
  IF turn_two_second.raw->>'model_inference_basis' <> 'token_count_model_used' THEN
    RAISE EXCEPTION 'unexpected turn-2 seg-1 inference basis %', turn_two_second.raw->>'model_inference_basis';
  END IF;
  IF turn_two_second.total_tokens <> 20 THEN
    RAISE EXCEPTION 'unexpected turn-2 seg-1 total tokens %', turn_two_second.total_tokens;
  END IF;

  SELECT *
  INTO summary_gpt54
  FROM __LLM_SCHEMA__.llm_session_usage_summary
  WHERE source_system = 'codex'
    AND session_id = 'session-mixed'
    AND provider = 'openai'
    AND model = 'gpt-5.4';

  IF summary_gpt54.session_total_tokens <> 180 THEN
    RAISE EXCEPTION 'expected gpt-5.4 session total 180, got %', summary_gpt54.session_total_tokens;
  END IF;
  IF summary_gpt54.event_count <> 3 THEN
    RAISE EXCEPTION 'expected gpt-5.4 event count 3, got %', summary_gpt54.event_count;
  END IF;

  SELECT *
  INTO summary_gpt53
  FROM __LLM_SCHEMA__.llm_session_usage_summary
  WHERE source_system = 'codex'
    AND session_id = 'session-mixed'
    AND provider = 'openai'
    AND model = 'gpt-5.3-codex';

  IF summary_gpt53.session_total_tokens <> 60 THEN
    RAISE EXCEPTION 'expected gpt-5.3-codex session total 60, got %', summary_gpt53.session_total_tokens;
  END IF;

  SELECT *
  INTO summary_mini
  FROM __LLM_SCHEMA__.llm_session_usage_summary
  WHERE source_system = 'codex'
    AND session_id = 'session-mixed'
    AND provider = 'openai'
    AND model = 'gpt-5.1-codex-mini';

  IF summary_mini.session_total_tokens <> 20 THEN
    RAISE EXCEPTION 'expected gpt-5.1-codex-mini session total 20, got %', summary_mini.session_total_tokens;
  END IF;
END $$;
SQL

llm_usage_render_sql_template "$assert_sql" "$db_schema" "$assert_sql_rendered"
llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$assert_sql_rendered" >/dev/null
rm -f "$assert_sql" "$assert_sql_rendered"

echo "Codex mixed-model segment attribution assertions passed for schema $db_schema"
