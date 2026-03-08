#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: ingest_gemini_mcp_usage_to_postgres.sh [options]

Options:
  --db-url URL     Postgres connection string. Defaults to LLM_USAGE_DB_URL or the postgres MCP DATABASE_URI in ~/.codex/config.toml.
  --schema NAME    Target schema. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --ledger PATH    Gemini MCP usage ledger. Defaults to GEMINI_MCP_USAGE_LEDGER_PATH or ~/.local/state/gemini-cli-mcp/token-usage.jsonl.
  --dry-run        Generate normalized rows and print counts without touching Postgres.
  --skip-schema    Do not apply schema before ingesting rows.
  --help           Show this help.
USAGE
}

db_url=${LLM_USAGE_DB_URL:-}
db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}
ledger=${GEMINI_MCP_USAGE_LEDGER_PATH:-$HOME/.local/state/gemini-cli-mcp/token-usage.jsonl}
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
    --ledger)
      ledger=${2:-}
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

llm_usage_require_commands jq mktemp awk python3 stat
awk 'BEGIN { exit 0 }' >/dev/null 2>&1
llm_usage_require_schema_name "$db_schema"

if [ "$dry_run" -eq 0 ]; then
  db_url=$(llm_usage_resolve_db_url "$db_url" || true)
  llm_usage_require_commands psql
  llm_usage_require_db_url "$db_url"
fi

if [ ! -f "$ledger" ]; then
  echo "Gemini MCP usage ledger not found: $ledger; skipping"
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
known_size=''
known_mtime=''
known_row_count=''

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
    --arg script_name "ingest_gemini_mcp_usage_to_postgres.sh" \
    --arg parser_version "$(llm_usage_parser_version)" \
    --arg source_system "gemini" \
    --arg source_kind "mcp_tool_call" \
    --arg started_at "$run_started_at" \
    --arg completed_at "$completed_at" \
    --arg status "$status" \
    --arg error_text "$error_text" \
    --arg ledger "$ledger" \
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
        ledger: $ledger
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
  llm_usage_fetch_artifact_state "$db_url" "$db_schema" gemini mcp_tool_call "$artifact_state_lookup"
  while IFS=$'\t' read -r path_hash size mtime row_count; do
    [ -n "$path_hash" ] || continue
    if [ "$path_hash" = "$(llm_usage_hash_string "$ledger")" ]; then
      known_size=$size
      known_mtime=$mtime
      known_row_count=$row_count
      break
    fi
  done < "$artifact_state_lookup"
fi

read -r source_size source_mtime < <(llm_usage_file_metadata "$ledger")
current_row_count=$(llm_usage_count_lines "$ledger")
ledger_hash=$(llm_usage_hash_string "$ledger")
start_line=1
line_offset=0

if [ "$dry_run" -eq 0 ] && [ -n "$known_size" ] && [ -n "$known_mtime" ] && [ "$known_size" = "$source_size" ] && [ "$known_mtime" = "$source_mtime" ]; then
  skipped_artifacts=1
  echo "Gemini MCP ledger unchanged; skipping"
  run_status='succeeded'
  exit 0
fi

if [ "$dry_run" -eq 0 ] && [ -n "$known_row_count" ] && [ "$current_row_count" -ge "$known_row_count" ] && [ "$source_size" -ge "${known_size:-0}" ]; then
  start_line=$((known_row_count + 1))
  line_offset=$((start_line - 1))
fi

processed_artifacts=1
if [ "$start_line" -le "$current_row_count" ]; then
  tail -n +"$start_line" "$ledger" | jq -c --arg source_path "$ledger" --arg run_id "$run_id" --argjson line_offset "$line_offset" '
    {
      logical_key: (
        "gemini|mcp_tool_call|"
        + (.invocation_id // ("line:" + ((input_line_number + $line_offset) | tostring)))
      ),
      ingest_run_id: (if ($run_id | length) > 0 then $run_id else null end),
      source_system: "gemini",
      source_kind: "mcp_tool_call",
      source_path: $source_path,
      source_row_id: (.invocation_id // ("line:" + ((input_line_number + $line_offset) | tostring))),
      event_ts: ((.timestamp_ms / 1000) | todateiso8601),
      session_id: (.resolved_session_id // .session_id // .invocation_id // ("ledger:" + (.timestamp_ms | tostring))),
      turn_id: null,
      project_key: (
        if (.effective_scope_roots | type) == "array" then .effective_scope_roots[0] else null end
      ),
      project_path: (
        if (.effective_scope_roots | type) == "array" then .effective_scope_roots[0] else null end
      ),
      cwd: (
        if (.effective_scope_roots | type) == "array" then .effective_scope_roots[0] else null end
      ),
      tool_name: (.tool_name // null),
      actor: "assistant",
      provider: "gemini",
      model_requested: (.model_requested // null),
      model_used: (.model_used // .model_requested // null),
      ok: (.ok // null),
      event_status: (
        if (.ok // null) == true then "succeeded"
        elif (.ok // null) == false then "failed"
        else "observed"
        end
      ),
      error_category: (.error_category // .failure_class // null),
      input_tokens: (.input_tokens // null),
      cached_input_tokens: (.cache_read_tokens // null),
      output_tokens: (.output_tokens // null),
      reasoning_tokens: (.reasoning_tokens // null),
      tool_tokens: null,
      total_tokens: (.total_tokens // null),
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
        version: (.version // null),
        duration_ms: (.duration_ms // null),
        gemini_invocations: (.gemini_invocations // null),
        retry_count: (.retry_count // null),
        usage_source: (.usage_source // null),
        fallback_mode: (.fallback_mode // null),
        fallback_reason: (.fallback_reason // null),
        resume_requested: (.resume_requested // null),
        resume_selector: (.resume_selector // null),
        resume_strategy: (.resume_strategy // null),
        resume_applied: (.resume_applied // null),
        resume_outcome: (.resume_outcome // null),
        default_model_applied: (.default_model_applied // null),
        context_window_percent_used: (.context_window_percent_used // null),
        context_window_percent_remaining: (.context_window_percent_remaining // null),
        context_window_source: (.context_window_source // null),
        drift_detected: (.drift_detected // null),
        drift_repaired: (.drift_repaired // null),
        drift_failed: (.drift_failed // null),
        invalid_table_refs: (.invalid_table_refs // null),
        citation_missing: (.citation_missing // null),
        nested_mcp_policy: (.nested_mcp_policy // null),
        absolute_line_number: (input_line_number + $line_offset)
      }
    }
  ' >> "$usage_file"

  tail -n +"$start_line" "$ledger" | jq -c --arg source_path "$ledger" --arg run_id "$run_id" --argjson line_offset "$line_offset" '
    select((.error_category // .failure_class // "") == "quota_or_rate_limit")
    | {
        logical_key: (
          "gemini|mcp_quota_error|"
          + (.invocation_id // ("line:" + ((input_line_number + $line_offset) | tostring)))
        ),
        ingest_run_id: (if ($run_id | length) > 0 then $run_id else null end),
        source_system: "gemini",
        source_kind: "mcp_quota_error",
        source_path: $source_path,
        source_row_id: (.invocation_id // ("line:" + ((input_line_number + $line_offset) | tostring))),
        event_ts: ((.timestamp_ms / 1000) | todateiso8601),
        session_id: (.resolved_session_id // .session_id // .invocation_id // ("ledger:" + (.timestamp_ms | tostring))),
        project_key: (
          if (.effective_scope_roots | type) == "array" then .effective_scope_roots[0] else null end
        ),
        project_path: (
          if (.effective_scope_roots | type) == "array" then .effective_scope_roots[0] else null end
        ),
        model_used: (.model_used // .model_requested // null),
        tool_name: (.tool_name // null),
        error_message: (.fallback_reason // .error_category // .failure_class // "quota_or_rate_limit"),
        reset_after_text: null,
        reset_after_seconds: null,
        raw: {
          duration_ms: (.duration_ms // null),
          gemini_invocations: (.gemini_invocations // null),
          retry_count: (.retry_count // null),
          usage_source: (.usage_source // null),
          absolute_line_number: (input_line_number + $line_offset)
        }
      }
  ' >> "$quota_file"
fi

llm_usage_normalize_json_file "$usage_file"
llm_usage_normalize_json_file "$quota_file"
usage_rows=$(llm_usage_count_lines "$usage_file")
quota_rows=$(llm_usage_count_lines "$quota_file")
generated_rows=$((usage_rows + quota_rows))

echo "Generated $usage_rows Gemini MCP usage row(s) from $ledger"
echo "Generated $quota_rows Gemini MCP quota row(s) from $ledger"

if [ "$dry_run" -eq 1 ]; then
  if [ "$usage_rows" -gt 0 ]; then
    echo "Sample Gemini MCP usage row:"
    sed -n '1p' "$usage_file"
  fi
  if [ "$quota_rows" -gt 0 ]; then
    echo "Sample Gemini MCP quota row:"
    sed -n '1p' "$quota_file"
  fi
  run_status='succeeded'
  exit 0
fi

llm_usage_run_usage_ingest "$db_url" "$db_schema" "$usage_file"
llm_usage_run_quota_ingest "$db_url" "$db_schema" "$quota_file"
jq -nc \
  --arg source_system "gemini" \
  --arg source_kind "mcp_tool_call" \
  --arg source_path "$ledger" \
  --arg source_path_hash "$ledger_hash" \
  --arg parser_version "$(llm_usage_parser_version)" \
  --arg last_ingest_run_id "$run_id" \
  --arg last_ingested_at "$(date -Is)" \
  --arg status "processed" \
  --argjson source_size_bytes "$source_size" \
  --argjson source_mtime_epoch "$source_mtime" \
  --argjson source_row_count "$current_row_count" \
  --argjson start_line "$start_line" \
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
      start_line: $start_line
    }
  }' > "$artifact_state_file"
llm_usage_run_artifact_state_ingest "$db_url" "$db_schema" "$artifact_state_file"
run_status='succeeded'
