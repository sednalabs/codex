#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: test_tool_call_summary.sh [options]

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
  db_schema="llm_usage_test_tool_calls_$(date +%s)"
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
  provider,
  model_used,
  ok,
  event_status,
  input_tokens,
  output_tokens,
  total_tokens,
  raw,
  logical_key,
  parser_version,
  source_path_hash
) VALUES
(
  'gemini-tool-1',
  'gemini',
  'interactive_message',
  'test-source',
  'msg-1',
  timestamptz '2026-03-14 00:00:00+00',
  'session-gemini',
  null,
  'project-a',
  'project-a',
  'gemini',
  'gemini-3-flash-preview',
  true,
  'succeeded',
  100,
  10,
  110,
  '{"tool_call_count":2}'::jsonb,
  'gemini|interactive_message|session-gemini||msg-1',
  'test',
  'hash-1'
),
(
  'gemini-tool-2',
  'gemini',
  'interactive_message',
  'test-source',
  'msg-2',
  timestamptz '2026-03-14 00:01:00+00',
  'session-gemini',
  null,
  'project-a',
  'project-a',
  'gemini',
  'gemini-3-flash-preview',
  true,
  'succeeded',
  90,
  5,
  95,
  '{"tool_call_count":0}'::jsonb,
  'gemini|interactive_message|session-gemini||msg-2',
  'test',
  'hash-2'
),
(
  'gemini-mcp-1',
  'gemini',
  'mcp_tool_call',
  'test-source',
  'mcp-1',
  timestamptz '2026-03-14 00:02:00+00',
  'session-mcp',
  null,
  'project-a',
  'project-a',
  'gemini',
  'gemini-3-flash-preview',
  true,
  'succeeded',
  50,
  5,
  55,
  '{}'::jsonb,
  'gemini|mcp_tool_call|session-mcp||mcp-1',
  'test',
  'hash-3'
),
(
  'codex-turn-1',
  'codex',
  'interactive_turn_segment',
  'test-source',
  'turn-1:seg-0',
  timestamptz '2026-03-14 00:03:00+00',
  'session-codex',
  'turn-1',
  'project-b',
  'project-b',
  'openai',
  'gpt-5.4',
  true,
  'succeeded',
  70,
  7,
  77,
  '{}'::jsonb,
  'codex|interactive_turn_segment|session-codex|turn-1|turn-1:seg-0',
  'test',
  'hash-4'
);
SQL
llm_usage_render_sql_template "$seed_sql" "$db_schema" "$seed_sql_rendered"
llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$seed_sql_rendered" >/dev/null
rm -f "$seed_sql" "$seed_sql_rendered"

assert_sql=$(mktemp)
assert_sql_rendered="$assert_sql.rendered"
cat > "$assert_sql" <<'SQL'
DO $$
DECLARE
  gemini_summary record;
  gemini_tools record;
  mcp_summary record;
  codex_summary record;
BEGIN
  SELECT *
  INTO gemini_summary
  FROM __LLM_SCHEMA__.llm_session_usage_summary
  WHERE source_system = 'gemini'
    AND source_kind = 'interactive_message'
    AND session_id = 'session-gemini';

  IF gemini_summary.tool_call_count <> 2 THEN
    RAISE EXCEPTION 'expected gemini tool_call_count 2, got %', gemini_summary.tool_call_count;
  END IF;
  IF gemini_summary.tool_call_count_known_event_count <> 2 THEN
    RAISE EXCEPTION 'expected gemini known event count 2, got %', gemini_summary.tool_call_count_known_event_count;
  END IF;
  IF gemini_summary.tool_call_count_unknown_event_count <> 0 THEN
    RAISE EXCEPTION 'expected gemini unknown event count 0, got %', gemini_summary.tool_call_count_unknown_event_count;
  END IF;
  IF gemini_summary.tool_call_count_coverage <> 'complete' THEN
    RAISE EXCEPTION 'expected gemini coverage complete, got %', gemini_summary.tool_call_count_coverage;
  END IF;

  SELECT *
  INTO gemini_tools
  FROM __LLM_SCHEMA__.llm_session_tool_call_summary
  WHERE source_system = 'gemini'
    AND source_kind = 'interactive_message'
    AND session_id = 'session-gemini';

  IF gemini_tools.tool_call_count <> 2 THEN
    RAISE EXCEPTION 'expected helper view tool_call_count 2, got %', gemini_tools.tool_call_count;
  END IF;

  SELECT *
  INTO mcp_summary
  FROM __LLM_SCHEMA__.llm_session_usage_summary
  WHERE source_system = 'gemini'
    AND source_kind = 'mcp_tool_call'
    AND session_id = 'session-mcp';

  IF mcp_summary.tool_call_count <> 0 THEN
    RAISE EXCEPTION 'expected mcp tool_call_count 0, got %', mcp_summary.tool_call_count;
  END IF;
  IF mcp_summary.tool_call_count_coverage <> 'unsupported' THEN
    RAISE EXCEPTION 'expected mcp coverage unsupported, got %', mcp_summary.tool_call_count_coverage;
  END IF;

  SELECT *
  INTO codex_summary
  FROM __LLM_SCHEMA__.llm_session_usage_summary
  WHERE source_system = 'codex'
    AND source_kind = 'interactive_turn_segment'
    AND session_id = 'session-codex';

  IF codex_summary.tool_call_count <> 0 THEN
    RAISE EXCEPTION 'expected codex tool_call_count 0, got %', codex_summary.tool_call_count;
  END IF;
  IF codex_summary.tool_call_count_coverage <> 'unsupported' THEN
    RAISE EXCEPTION 'expected codex coverage unsupported, got %', codex_summary.tool_call_count_coverage;
  END IF;
END $$;
SQL
llm_usage_render_sql_template "$assert_sql" "$db_schema" "$assert_sql_rendered"
llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$assert_sql_rendered" >/dev/null
rm -f "$assert_sql" "$assert_sql_rendered"

echo "PASS: tool call summary assertions passed in schema $db_schema"
