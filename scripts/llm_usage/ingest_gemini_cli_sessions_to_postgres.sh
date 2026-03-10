#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: ingest_gemini_cli_sessions_to_postgres.sh [options]

Options:
  --db-url URL       Postgres connection string. Defaults to LLM_USAGE_DB_URL or the postgres MCP DATABASE_URI in ~/.codex/config.toml.
  --schema NAME      Target schema. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --state-root PATH  Gemini CLI state root. Defaults to GEMINI_CLI_STATE_ROOT or ~/.gemini/tmp.
  --dry-run          Generate normalized rows and print counts without touching Postgres.
  --skip-schema      Do not apply schema before ingesting rows.
  --help             Show this help.
USAGE
}

db_url=${LLM_USAGE_DB_URL:-}
db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}
state_root=${GEMINI_CLI_STATE_ROOT:-$HOME/.gemini/tmp}
dry_run=0
skip_schema=0

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
    --skip-schema)
      skip_schema=1
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

llm_usage_require_commands jq find mktemp awk python3 stat
llm_usage_require_schema_name "$db_schema"

if [ "$dry_run" -eq 0 ]; then
  db_url=$(llm_usage_resolve_db_url "$db_url" || true)
  llm_usage_require_commands psql
  llm_usage_require_db_url "$db_url"
fi

if [ ! -d "$state_root" ]; then
  echo "Gemini CLI state root does not exist: $state_root; skipping"
  exit 0
fi

usage_file=$(mktemp)
quota_file=$(mktemp)
artifact_state_file=$(mktemp)
artifact_state_lookup=$(mktemp)
cleanup_files=("$usage_file" "$quota_file" "$artifact_state_file" "$artifact_state_lookup")
run_id=''
run_started_at=''
run_status='failed'
run_error=''
generated_rows=0
processed_artifacts=0
skipped_artifacts=0

declare -A known_size

declare -A known_mtime

declare -A known_parser_version

persist_run_record() {
  local status=$1
  local error_text=${2:-}
  local completed_at=$3
  local run_file

  if [ "$dry_run" -eq 1 ] || [ -z "$run_id" ]; then
    return 0
  fi

  run_file=$(mktemp)
  jq -nc \
    --arg run_id "$run_id" \
    --arg script_name "ingest_gemini_cli_sessions_to_postgres.sh" \
    --arg parser_version "$(llm_usage_parser_version)" \
    --arg source_system "gemini" \
    --arg source_kind "interactive_message" \
    --arg started_at "$run_started_at" \
    --arg completed_at "$completed_at" \
    --arg status "$status" \
    --arg error_text "$error_text" \
    --arg state_root "$state_root" \
    --argjson dry_run false \
    --argjson processed_artifacts "$processed_artifacts" \
    --argjson skipped_artifacts "$skipped_artifacts" \
    --argjson generated_rows "$generated_rows" \
    '{
      run_id: $run_id,
      script_name: $script_name,
      parser_version: $parser_version,
      source_system: $source_system,
      source_kind: $source_kind,
      dry_run: $dry_run,
      started_at: $started_at,
      completed_at: (if ($completed_at | length) > 0 then $completed_at else null end),
      status: $status,
      processed_artifacts: $processed_artifacts,
      skipped_artifacts: $skipped_artifacts,
      generated_rows: $generated_rows,
      error_text: (if ($error_text | length) > 0 then $error_text else null end),
      raw: {
        state_root: $state_root
      }
    }' > "$run_file"
  llm_usage_run_ingest_run_ingest "$db_url" "$db_schema" "$run_file" || true
  rm -f "$run_file"
}

finish_run() {
  local exit_code=$?
  local completed_at
  completed_at=$(date -Is)

  if [ "$dry_run" -eq 0 ] && [ -n "$run_id" ]; then
    if [ "$exit_code" -ne 0 ] && [ -z "$run_error" ]; then
      run_error="script exited with code $exit_code"
      run_status='failed'
    fi
    persist_run_record "$run_status" "$run_error" "$completed_at"
  fi

  rm -f "${cleanup_files[@]}"
  exit "$exit_code"
}
trap finish_run EXIT

if [ "$dry_run" -eq 0 ] && [ "$skip_schema" -eq 0 ]; then
  llm_usage_apply_schema "$db_url" "$db_schema"
fi

if [ "$dry_run" -eq 0 ]; then
  run_id=$(llm_usage_new_run_id)
  run_started_at=$(date -Is)
  persist_run_record 'running' '' ''
  llm_usage_fetch_artifact_state "$db_url" "$db_schema" gemini interactive_message "$artifact_state_lookup"
  while IFS=$'\t' read -r path_hash size mtime parser_version _row_count; do
    [ -n "$path_hash" ] || continue
    known_size["$path_hash"]=$size
    known_mtime["$path_hash"]=$mtime
    known_parser_version["$path_hash"]=$parser_version
  done < "$artifact_state_lookup"
fi

while IFS= read -r -d '' file; do
  read -r source_size source_mtime < <(llm_usage_file_metadata "$file")
  source_path_hash=$(llm_usage_hash_string "$file")
  if [ "$dry_run" -eq 0 ] \
    && [ "${known_size[$source_path_hash]-}" = "$source_size" ] \
    && [ "${known_mtime[$source_path_hash]-}" = "$source_mtime" ] \
    && [ "${known_parser_version[$source_path_hash]-}" = "$(llm_usage_parser_version)" ]; then
    skipped_artifacts=$((skipped_artifacts + 1))
    continue
  fi

  processed_artifacts=$((processed_artifacts + 1))
  before_usage_rows=$(llm_usage_count_lines "$usage_file")
  before_quota_rows=$(llm_usage_count_lines "$quota_file")

  session_dir=$(dirname -- "$(dirname -- "$file")")
  project_root_file="$session_dir/.project_root"
  project_root=''
  if [ -f "$project_root_file" ]; then
    project_root=$(tr -d '\n' < "$project_root_file")
  fi

  jq -c --arg source_path "$file" --arg project_root "$project_root" --arg run_id "$run_id" '
    . as $session
    | (.messages // [])
    | to_entries[]
    | .key as $message_index
    | .value as $message
    | select(($message.tokens | type) == "object")
    | {
        logical_key: (
          "gemini|interactive_message|"
          + ($session.sessionId // "unknown-session")
          + "|"
          + ($message.id // ("message-index:" + ($message_index | tostring)))
        ),
        ingest_run_id: (if ($run_id | length) > 0 then $run_id else null end),
        source_system: "gemini",
        source_kind: "interactive_message",
        source_path: $source_path,
        source_row_id: ($message.id // ("message-index:" + ($message_index | tostring))),
        event_ts: ($message.timestamp // $session.lastUpdated // $session.startTime),
        session_id: ($session.sessionId),
        turn_id: null,
        project_key: ($session.projectHash // (if ($project_root | length) > 0 then $project_root else null end) // null),
        project_path: (if ($project_root | length) > 0 then $project_root else null end),
        cwd: (if ($project_root | length) > 0 then $project_root else null end),
        tool_name: (
          if ($message.toolCalls | type) == "array" and (($message.toolCalls | length) == 1) then $message.toolCalls[0].name else null end
        ),
        actor: ($message.type // $message.role // null),
        provider: "gemini",
        model_requested: null,
        model_used: ($message.model // null),
        ok: true,
        event_status: "succeeded",
        error_category: null,
        input_tokens: ($message.tokens.input // null),
        cached_input_tokens: ($message.tokens.cached // null),
        output_tokens: ($message.tokens.output // null),
        reasoning_tokens: ($message.tokens.thoughts // null),
        tool_tokens: ($message.tokens.tool // null),
        total_tokens: ($message.tokens.total // null),
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
          message_index: $message_index,
          message_type: ($message.type // null),
          role: ($message.role // null),
          tool_call_count: (
            if ($message.toolCalls | type) == "array" then ($message.toolCalls | length) else 0 end
          )
        }
      }
  ' "$file" >> "$usage_file"

  jq -c --arg source_path "$file" --arg project_root "$project_root" --arg run_id "$run_id" '
    def normalize_duration(text):
      text
      | ascii_downcase
      | gsub("[^0-9dhms]"; "")
      | select(length > 0);

    def parse_reset_seconds(text):
      (normalize_duration(text) | capture("^(?:(?<days>[0-9]+)d)?(?:(?<hours>[0-9]+)h)?(?:(?<minutes>[0-9]+)m)?(?:(?<seconds>[0-9]+)s)?$")) as $parts
      | (($parts.days // "0") | tonumber) * 86400
      + (($parts.hours // "0") | tonumber) * 3600
      + (($parts.minutes // "0") | tonumber) * 60
      + (($parts.seconds // "0") | tonumber);

    . as $session
    | (.messages // [])
    | to_entries[]
    | .key as $message_index
    | .value as $message
    | (($message.toolCalls // []) | to_entries[]?)
    | .key as $tool_index
    | .value as $tool
    | (($tool.resultDisplay // $tool.description // "") | tostring) as $display
    | select($display | test("quota will reset after"; "i"))
    | ($display | capture("(?i)quota will reset after (?<reset>[0-9dhms ,]+)")) as $quota
    | (normalize_duration($quota.reset)) as $normalized_reset
    | {
        logical_key: (
          "gemini|interactive_quota_error|"
          + ($session.sessionId // "unknown-session")
          + "|"
          + ($message.id // ("message-index:" + ($message_index | tostring)))
          + "|"
          + ($tool.id // ("tool-index:" + ($tool_index | tostring)))
        ),
        ingest_run_id: (if ($run_id | length) > 0 then $run_id else null end),
        source_system: "gemini",
        source_kind: "interactive_quota_error",
        source_path: $source_path,
        source_row_id: (
          ($message.id // ("message-index:" + ($message_index | tostring)))
          + "#"
          + ($tool.id // ("tool-index:" + ($tool_index | tostring)))
        ),
        event_ts: ($tool.timestamp // $message.timestamp // $session.lastUpdated // $session.startTime),
        session_id: ($session.sessionId),
        project_key: ($session.projectHash // (if ($project_root | length) > 0 then $project_root else null end) // null),
        project_path: (if ($project_root | length) > 0 then $project_root else null end),
        model_used: ($message.model // null),
        tool_name: ($tool.name // null),
        error_message: $display,
        reset_after_text: $normalized_reset,
        reset_after_seconds: (
          if ($normalized_reset | type) == "string" and ($normalized_reset | length) > 0 then
            parse_reset_seconds($normalized_reset)
          else
            null
          end
        ),
        raw: {
          message_index: $message_index,
          tool_index: $tool_index,
          message_type: ($message.type // null),
          tool_status: ($tool.status // null),
          tool_display_name: ($tool.displayName // null),
          raw_reset_fragment: ($quota.reset // null)
        }
      }
  ' "$file" >> "$quota_file"

  after_usage_rows=$(llm_usage_count_lines "$usage_file")
  after_quota_rows=$(llm_usage_count_lines "$quota_file")
  artifact_generated_rows=$(((after_usage_rows - before_usage_rows) + (after_quota_rows - before_quota_rows)))
  jq -nc \
    --arg source_system "gemini" \
    --arg source_kind "interactive_message" \
    --arg source_path "$file" \
    --arg source_path_hash "$source_path_hash" \
    --arg parser_version "$(llm_usage_parser_version)" \
    --arg last_ingest_run_id "$run_id" \
    --arg last_ingested_at "$(date -Is)" \
    --arg status "processed" \
    --argjson source_size_bytes "$source_size" \
    --argjson source_mtime_epoch "$source_mtime" \
    --argjson source_row_count "$artifact_generated_rows" \
    --argjson generated_rows "$artifact_generated_rows" \
    '{
      source_system: $source_system,
      source_kind: $source_kind,
      source_path: $source_path,
      source_path_hash: $source_path_hash,
      source_size_bytes: $source_size_bytes,
      source_mtime_epoch: $source_mtime_epoch,
      source_row_count: $source_row_count,
      parser_version: $parser_version,
      last_ingest_run_id: (if ($last_ingest_run_id | length) > 0 then $last_ingest_run_id else null end),
      last_ingested_at: $last_ingested_at,
      status: $status,
      raw: {
        generated_rows: $generated_rows
      }
    }' >> "$artifact_state_file"
done < <(find "$state_root" -type f -path '*/chats/session-*.json' -print0 | sort -z)

llm_usage_normalize_json_file "$usage_file"
llm_usage_normalize_json_file "$quota_file"
usage_rows=$(llm_usage_count_lines "$usage_file")
quota_rows=$(llm_usage_count_lines "$quota_file")
generated_rows=$((usage_rows + quota_rows))

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
  run_status='succeeded'
  exit 0
fi

llm_usage_run_usage_ingest "$db_url" "$db_schema" "$usage_file"
llm_usage_run_quota_ingest "$db_url" "$db_schema" "$quota_file"
llm_usage_run_artifact_state_ingest "$db_url" "$db_schema" "$artifact_state_file"
run_status='succeeded'
