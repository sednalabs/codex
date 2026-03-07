#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: ingest_gemini_cli_sessions_to_postgres.sh [options]

Options:
  --db-url URL      Postgres connection string. Defaults to LLM_USAGE_DB_URL or the postgres MCP DATABASE_URI in ~/.codex/config.toml.
  --schema NAME        Target schema. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --state-root PATH Gemini CLI state root. Defaults to GEMINI_CLI_STATE_ROOT or ~/.gemini/tmp.
  --dry-run         Generate normalized rows and print counts without touching Postgres.
  --help            Show this help.
USAGE
}

db_url=${LLM_USAGE_DB_URL:-}
db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}
state_root=${GEMINI_CLI_STATE_ROOT:-$HOME/.gemini/tmp}
dry_run=0

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
    --state-root)
      state_root=${2:-}
      shift 2
      ;;
    --dry-run)
      dry_run=1
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

db_url=$(llm_usage_resolve_db_url "$db_url" || true)

llm_usage_require_commands jq find mktemp awk
llm_usage_require_schema_name "$db_schema"
if [ "$dry_run" -eq 0 ]; then
  llm_usage_require_commands psql
  llm_usage_require_db_url "$db_url"
fi

if [ ! -d "$state_root" ]; then
  echo "Gemini CLI state root does not exist: $state_root" >&2
  exit 1
fi

usage_file=$(mktemp)
quota_file=$(mktemp)
trap 'rm -f "$usage_file" "$quota_file"' EXIT

while IFS= read -r -d '' file; do
  session_dir=$(dirname -- "$(dirname -- "$file")")
  project_root_file="$session_dir/.project_root"
  project_root=''
  if [ -f "$project_root_file" ]; then
    project_root=$(tr -d '\n' < "$project_root_file")
  fi

  jq -c --arg source_path "$file" --arg project_root "$project_root" '
    . as $session
    | .messages[]?
    | select((.tokens | type) == "object")
    | {
        source_system: "gemini",
        source_kind: "interactive_message",
        source_path: $source_path,
        source_row_id: (.id // null),
        event_ts: (.timestamp // $session.lastUpdated // $session.startTime),
        session_id: ($session.sessionId),
        turn_id: null,
        project_key: ($session.projectHash // ($project_root | select(length > 0)) // null),
        project_path: (($project_root | select(length > 0)) // null),
        cwd: (($project_root | select(length > 0)) // null),
        tool_name: (
          if (.toolCalls | type) == "array" and ((.toolCalls | length) == 1) then .toolCalls[0].name else null end
        ),
        actor: (.type // .role // null),
        provider: "gemini",
        model_requested: null,
        model_used: (.model // null),
        ok: true,
        error_category: null,
        input_tokens: (.tokens.input // null),
        cached_input_tokens: (.tokens.cached // null),
        output_tokens: (.tokens.output // null),
        reasoning_tokens: (.tokens.thoughts // null),
        tool_tokens: (.tokens.tool // null),
        total_tokens: (.tokens.total // null),
        cumulative_input_tokens: null,
        cumulative_cached_input_tokens: null,
        cumulative_output_tokens: null,
        cumulative_reasoning_tokens: null,
        cumulative_total_tokens: null,
        context_window: null,
        rate_limit_id: null,
        rate_limit_name: null,
        primary_used_percent: null,
        primary_window_minutes: null,
        primary_resets_at: null,
        secondary_used_percent: null,
        secondary_window_minutes: null,
        secondary_resets_at: null,
        credits_balance: null,
        credits_unlimited: null,
        raw: {
          message_type: (.type // null),
          role: (.role // null),
          tool_call_count: (
            if (.toolCalls | type) == "array" then (.toolCalls | length) else 0 end
          )
        }
      }
  ' "$file" >> "$usage_file"

  jq -c --arg source_path "$file" --arg project_root "$project_root" '
    def parse_reset_seconds(text):
      (text | capture("^(?:(?<days>[0-9]+)d)?(?:(?<hours>[0-9]+)h)?(?:(?<minutes>[0-9]+)m)?(?:(?<seconds>[0-9]+)s)?$")) as $parts
      | (($parts.days // "0") | tonumber) * 86400
      + (($parts.hours // "0") | tonumber) * 3600
      + (($parts.minutes // "0") | tonumber) * 60
      + (($parts.seconds // "0") | tonumber);

    . as $session
    | .messages[]?
    | . as $message
    | (
        if (.toolCalls | type) == "array" then .toolCalls[] else empty end
      ) as $tool
    | (($tool.resultDisplay // $tool.description // "") | tostring) as $display
    | select($display | test("quota will reset after"; "i"))
    | ($display | capture("[Qq]uota will reset after (?<reset>[0-9dhms]+)")) as $quota
    | {
        source_system: "gemini",
        source_kind: "interactive_quota_error",
        source_path: $source_path,
        source_row_id: (($message.id // "message") + "#" + ($tool.id // $tool.name // "tool")),
        event_ts: ($tool.timestamp // $message.timestamp // $session.lastUpdated // $session.startTime),
        session_id: ($session.sessionId),
        project_key: ($session.projectHash // ($project_root | select(length > 0)) // null),
        project_path: (($project_root | select(length > 0)) // null),
        model_used: ($message.model // null),
        tool_name: ($tool.name // null),
        error_message: $display,
        reset_after_text: $quota.reset,
        reset_after_seconds: parse_reset_seconds($quota.reset),
        raw: {
          message_type: ($message.type // null),
          tool_status: ($tool.status // null),
          tool_display_name: ($tool.displayName // null)
        }
      }
  ' "$file" >> "$quota_file"
done < <(find "$state_root" -type f -path '*/chats/session-*.json' -print0 | sort -z)

usage_rows=$(llm_usage_count_lines "$usage_file")
quota_rows=$(llm_usage_count_lines "$quota_file")
echo "Generated $usage_rows Gemini interactive usage row(s) from $state_root"
echo "Generated $quota_rows Gemini interactive quota row(s) from $state_root"

if [ "$dry_run" -eq 1 ]; then
  if [ "$usage_rows" -gt 0 ]; then
    echo "Sample Gemini interactive usage row:"
    sed -n '1p' "$usage_file"
  fi
  if [ "$quota_rows" -gt 0 ]; then
    echo "Sample Gemini interactive quota row:"
    sed -n '1p' "$quota_file"
  fi
  exit 0
fi

llm_usage_apply_schema "$db_url" "$db_schema"
llm_usage_run_usage_ingest "$db_url" "$db_schema" "$usage_file"
llm_usage_run_quota_ingest "$db_url" "$db_schema" "$quota_file"
