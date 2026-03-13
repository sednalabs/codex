#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: ingest_codex_rollouts_to_postgres.sh [options]

Options:
  --db-url URL           Postgres connection string. Defaults to LLM_USAGE_DB_URL or the postgres MCP DATABASE_URI in ~/.codex/config.toml.
  --schema NAME          Target schema. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --sessions-root PATH   Codex rollout root. Defaults to CODEX_USAGE_ROLLOUTS_ROOT or ~/.codex/sessions.
  --dry-run              Generate normalized rows and print counts without touching Postgres.
  --skip-schema          Do not apply schema before ingesting rows.
  --help                 Show this help.
USAGE
}

db_url=${LLM_USAGE_DB_URL:-}
db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}
sessions_root=${CODEX_USAGE_ROLLOUTS_ROOT:-$HOME/.codex/sessions}
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
    --sessions-root)
      sessions_root=${2:-}
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

if [ ! -d "$sessions_root" ]; then
  echo "Codex rollout root does not exist: $sessions_root; skipping"
  exit 0
fi

usage_file=$(mktemp)
artifact_state_file=$(mktemp)
artifact_state_lookup=$(mktemp)
cleanup_files=("$usage_file" "$artifact_state_file" "$artifact_state_lookup")
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
    --arg script_name "ingest_codex_rollouts_to_postgres.sh" \
    --arg parser_version "$(llm_usage_parser_version)" \
    --arg source_system "codex" \
    --arg source_kind "interactive_turn_segment" \
    --arg started_at "$run_started_at" \
    --arg completed_at "$completed_at" \
    --arg status "$status" \
    --arg error_text "$error_text" \
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
      completed_at: $completed_at,
      status: $status,
      processed_artifacts: $processed_artifacts,
      skipped_artifacts: $skipped_artifacts,
      generated_rows: $generated_rows,
      error_text: (if ($error_text | length) > 0 then $error_text else null end),
      raw: {
        sessions_root: $ENV.CODEX_USAGE_ROLLOUTS_ROOT,
        skip_schema: ($ENV.LLM_USAGE_SKIP_SCHEMA // null)
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
  llm_usage_fetch_artifact_state "$db_url" "$db_schema" codex interactive_turn_segment "$artifact_state_lookup"
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
  before_rows=$(llm_usage_count_lines "$usage_file")

  jq -sc --arg source_path "$file" --arg run_id "$run_id" '
    def zero_usage:
      {
        input_tokens: 0,
        cached_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
        total_tokens: 0
      };

    def add_usage(curr; delta_usage):
      {
        input_tokens: ((curr.input_tokens // 0) + (delta_usage.input_tokens // 0)),
        cached_input_tokens: ((curr.cached_input_tokens // 0) + (delta_usage.cached_input_tokens // 0)),
        output_tokens: ((curr.output_tokens // 0) + (delta_usage.output_tokens // 0)),
        reasoning_output_tokens: ((curr.reasoning_output_tokens // 0) + (delta_usage.reasoning_output_tokens // 0)),
        total_tokens: ((curr.total_tokens // 0) + (delta_usage.total_tokens // 0))
      };

    def delta(curr; prev):
      if curr == null then null
      elif prev == null then curr
      elif curr < prev then null
      else curr - prev
      end;

    def fallback_usage(curr; prev):
      {
        input_tokens: delta(curr.input_tokens; prev.input_tokens),
        cached_input_tokens: delta(curr.cached_input_tokens; prev.cached_input_tokens),
        output_tokens: delta(curr.output_tokens; prev.output_tokens),
        reasoning_output_tokens: delta(curr.reasoning_output_tokens; prev.reasoning_output_tokens),
        total_tokens: delta(curr.total_tokens; prev.total_tokens)
      };

    def unique_non_null(curr; value):
      if value == null then
        (curr // [])
      else
        (((curr // []) + [value]) | unique)
      end;

    def segment_model_key($provider; $model):
      (($provider // "unknown") + "::" + ($model // "unknown"));

    def ensure_segment($turn; $provider; $model; $context_window; $timestamp):
      ($turn.current_segment_id // null) as $current_segment_id
      | ($turn.segments // {}) as $segments
      | if $current_segment_id != null
          and (($segments[$current_segment_id].provider // null) == $provider)
          and (($segments[$current_segment_id].model // null) == $model)
        then
          $turn
          | if $context_window != null then
              .segments[$current_segment_id].model_context_window = $context_window
            else
              .
            end
        else
          (($turn.segment_seq // -1) + 1) as $next_index
          | ("seg-" + ($next_index | tostring)) as $segment_label
          | (($turn.turn_id // "turn") + ":" + $segment_label) as $source_row_id
          | $turn + {
              segment_seq: $next_index,
              current_segment_id: $source_row_id,
              segments: ($segments + {
                ($source_row_id): {
                  source_row_id: $source_row_id,
                  segment_index: $next_index,
                  provider: $provider,
                  model: $model,
                  model_context_window: $context_window,
                  usage_acc: zero_usage,
                  token_count_events: 0,
                  first_token_count_at: $timestamp,
                  last_token_count_at: $timestamp
                }
              })
            }
        end;

    def build_usage_row($session; $ctx; $turn; $segment; $segment_count; $row; $ok; $error_category; $event_status; $turn_usage; $is_last):
      {
        logical_key: ("codex|interactive_turn_segment|" + ($session.id // ($turn.turn_id // "turn")) + "|" + ($turn.turn_id // "") + "|" + ($segment.source_row_id // ($turn.turn_id // "segment"))),
        ingest_run_id: (if ($run_id | length) > 0 then $run_id else null end),
        source_system: "codex",
        source_kind: "interactive_turn_segment",
        source_path: $source_path,
        source_row_id: ($segment.source_row_id // ($turn.turn_id // "segment")),
        event_ts: ($segment.first_token_count_at // $segment.last_token_count_at // $row.timestamp),
        session_id: ($session.id // ($turn.turn_id // "turn")),
        turn_id: ($turn.turn_id // null),
        forked_from_session_id: ($session.forked_from_id // null),
        project_key: ($ctx.cwd // $session.cwd // null),
        project_path: ($ctx.cwd // $session.cwd // null),
        cwd: ($ctx.cwd // $session.cwd // null),
        tool_name: null,
        actor: "assistant",
        provider: ($segment.provider // $ctx.provider // $turn.fallback_provider // $session.model_provider // "openai"),
        model_requested: ($segment.model // $ctx.model // $turn.fallback_model // $session.model // null),
        model_used: ($segment.model // $ctx.model // $turn.fallback_model // $session.model // null),
        ok: $ok,
        event_status: $event_status,
        error_category: $error_category,
        input_tokens: ($segment.usage_acc.input_tokens // null),
        cached_input_tokens: ($segment.usage_acc.cached_input_tokens // null),
        output_tokens: ($segment.usage_acc.output_tokens // null),
        reasoning_tokens: ($segment.usage_acc.reasoning_output_tokens // null),
        tool_tokens: null,
        total_tokens: ($segment.usage_acc.total_tokens // null),
        cumulative_input_tokens: (if $is_last and ($turn_usage.input_tokens != null) then $turn_usage.input_tokens else null end),
        cumulative_cached_input_tokens: (if $is_last and ($turn_usage.cached_input_tokens != null) then $turn_usage.cached_input_tokens else null end),
        cumulative_output_tokens: (if $is_last and ($turn_usage.output_tokens != null) then $turn_usage.output_tokens else null end),
        cumulative_reasoning_tokens: (if $is_last and ($turn_usage.reasoning_output_tokens != null) then $turn_usage.reasoning_output_tokens else null end),
        cumulative_total_tokens: (if $is_last and ($turn_usage.total_tokens != null) then $turn_usage.total_tokens else null end),
        context_window: ($segment.model_context_window // $turn.model_context_window // $ctx.model_context_window // null),
        rate_limit_id: ($turn.rate_limits.limit_id // $turn.rate_limits.id // $turn.rate_limits.name // null),
        rate_limit_name: ($turn.rate_limits.limit_name // $turn.rate_limits.name // null),
        primary_used_percent: ($turn.rate_limits.primary.used_percent // null),
        primary_window_minutes: ($turn.rate_limits.primary.window_minutes // null),
        primary_resets_at: (($turn.rate_limits.primary.resets_at // null) | if . == null then null else todateiso8601 end),
        secondary_used_percent: ($turn.rate_limits.secondary.used_percent // null),
        secondary_window_minutes: ($turn.rate_limits.secondary.window_minutes // null),
        secondary_resets_at: (($turn.rate_limits.secondary.resets_at // null) | if . == null then null else todateiso8601 end),
        credits_balance: ($turn.rate_limits.credits.balance // null),
        credits_unlimited: ($turn.rate_limits.credits.unlimited // null),
        raw: {
          started_at: ($turn.started_at // null),
          first_token_count_at: ($segment.first_token_count_at // null),
          last_token_count_at: ($segment.last_token_count_at // null),
          turn_last_token_count_at: ($turn.last_token_count_at // null),
          token_count_events: ($turn.token_count_events // 0),
          segment_token_count_events: ($segment.token_count_events // 0),
          segment_index: ($segment.segment_index // 0),
          segment_count: $segment_count,
          segment_model_key: segment_model_key(($segment.provider // null); ($segment.model // null)),
          mixed_model_turn: (($turn.models_seen // []) | length) > 1,
          models_seen: ($turn.models_seen // []),
          has_turn_context: ($turn.has_turn_context // false),
          pre_turn_model: ($turn.pre_turn_model // null),
          pre_turn_provider: ($turn.pre_turn_provider // null),
          turn_context_model: ($ctx.model // null),
          turn_context_provider: ($ctx.provider // null),
          compaction_events_in_turn: ($row.payload.compaction_events_in_turn // null),
          source_session_id: ($session.id // null),
          forked_from_session_id: ($session.forked_from_id // null),
          interrupted_reason: (if $event_status == "aborted" then $error_category else null end),
          has_last_token_usage: ($turn.has_last_usage // false),
          model_inference_basis: (
            if $segment.model == null then
              "unknown"
            elif ($segment.explicit_model // false) then
              "token_count_model_used"
            elif ($segment.pre_turn_segment // false) then
              "pre_turn_previous_model"
            elif ($turn.has_turn_context // false) then
              "turn_context_model"
            else
              "turn_started_or_session_model"
            end
          )
        }
      };

    def emit_turn($session; $row; $ok; $error_category; $event_status):
      ($row.payload.turn_id) as $turn_id
      | (.open_turns[$turn_id]) as $turn
      | (.contexts[$turn_id] // {}) as $ctx
      | ($turn.latest_total_usage // {}) as $turn_usage
      | (.last_accounted_cumulative // {}) as $prev
      | (($turn.segments // {}) | to_entries | sort_by(.value.segment_index // 0)) as $segment_entries
      | (if ($turn.has_last_usage // false) then ($segment_entries | length) else 0 end) as $existing_segment_count
      | (if $existing_segment_count > 0 then
          ($segment_entries | map(.value))
        else
          [
            {
              source_row_id: (($turn_id // "turn") + ":seg-0"),
              segment_index: 0,
              provider: ($ctx.provider // $turn.fallback_provider // $session.model_provider // "openai"),
              model: ($ctx.model // $turn.fallback_model // $session.model // null),
              model_context_window: ($turn.model_context_window // $ctx.model_context_window // null),
              usage_acc: (fallback_usage($turn_usage; $prev)),
              token_count_events: 0,
              first_token_count_at: ($turn.started_at // $row.timestamp),
              last_token_count_at: ($turn.last_token_count_at // $row.timestamp),
              explicit_model: false,
              pre_turn_segment: false
            }
          ]
        end) as $segments
      | ($segments | length) as $segment_count
      | ._emit = [
          range(0; $segment_count) as $idx
          | build_usage_row(
              $session;
              $ctx;
              $turn;
              ($segments[$idx]);
              $segment_count;
              $row;
              $ok;
              $error_category;
              $event_status;
              $turn_usage;
              ($idx == ($segment_count - 1))
            )
        ]
      | if $turn_usage.total_tokens != null then
          .last_accounted_cumulative = {
            input_tokens: ($turn_usage.input_tokens // null),
            cached_input_tokens: ($turn_usage.cached_input_tokens // null),
            output_tokens: ($turn_usage.output_tokens // null),
            reasoning_output_tokens: ($turn_usage.reasoning_output_tokens // null),
            total_tokens: ($turn_usage.total_tokens // null)
          }
        else
          .
        end
      | .last_completed_model = ($ctx.model // $turn.fallback_model // $session.model // null)
      | .last_completed_provider = ($ctx.provider // $turn.fallback_provider // $session.model_provider // "openai")
      | del(.open_turns[$turn_id])
      | if .active_turn_id == $turn_id then .active_turn_id = null else . end;

    . as $rows
    | (map(select(.type == "session_meta") | .payload)[0] // {}) as $session
    | foreach $rows[] as $row (
        {
          session_meta: $session,
          contexts: {},
          open_turns: {},
          active_turn_id: null,
          last_accounted_cumulative: null,
          last_completed_model: null,
          last_completed_provider: null,
          _emit: []
        };
        ._emit = []
        | if $row.type == "turn_context" and ($row.payload.turn_id? != null) then
            .contexts[$row.payload.turn_id] = {
              cwd: ($row.payload.cwd // $session.cwd // null),
              model: ($row.payload.model // $session.model // null),
              provider: ($session.model_provider // null),
              model_context_window: ($row.payload.model_context_window // null)
            }
            | if .open_turns[$row.payload.turn_id] != null then
                (.open_turns[$row.payload.turn_id]) |= (
                  . + {
                    fallback_model: ($row.payload.model // .fallback_model // $session.model // null),
                    fallback_provider: (.fallback_provider // $session.model_provider // "openai"),
                    has_turn_context: true
                  }
                )
              else
                .
              end
          elif $row.type == "event_msg" and $row.payload.type == "task_started" and ($row.payload.turn_id? != null) then
            .open_turns[$row.payload.turn_id] = {
              turn_id: $row.payload.turn_id,
              started_at: ($row.timestamp // null),
              model_context_window: (
                $row.payload.model_context_window
                // .contexts[$row.payload.turn_id].model_context_window
                // null
              ),
              latest_total_usage: (.open_turns[$row.payload.turn_id].latest_total_usage // null),
              rate_limits: (.open_turns[$row.payload.turn_id].rate_limits // null),
              token_count_events: (.open_turns[$row.payload.turn_id].token_count_events // 0),
              last_token_count_at: (.open_turns[$row.payload.turn_id].last_token_count_at // null),
              has_last_usage: (.open_turns[$row.payload.turn_id].has_last_usage // false),
              has_turn_context: (.open_turns[$row.payload.turn_id].has_turn_context // false),
              fallback_model: ($row.payload.model_used // .contexts[$row.payload.turn_id].model // $session.model // null),
              fallback_provider: ($row.payload.provider // .contexts[$row.payload.turn_id].provider // $session.model_provider // "openai"),
              pre_turn_model: (.last_completed_model // $session.model // null),
              pre_turn_provider: (.last_completed_provider // $session.model_provider // "openai"),
              segment_seq: (.open_turns[$row.payload.turn_id].segment_seq // -1),
              current_segment_id: (.open_turns[$row.payload.turn_id].current_segment_id // null),
              segments: (.open_turns[$row.payload.turn_id].segments // {}),
              models_seen: (.open_turns[$row.payload.turn_id].models_seen // [])
            }
            | .active_turn_id = $row.payload.turn_id
          elif $row.type == "event_msg" and $row.payload.type == "token_count" and (.active_turn_id != null) and (.open_turns[.active_turn_id] != null) then
            (.open_turns[.active_turn_id]) |= (
              (if (.has_turn_context // false) then
                 {
                   provider: ($row.payload.provider // .fallback_provider // $session.model_provider // "openai"),
                   model: ($row.payload.model_used // .fallback_model // $session.model // null),
                   pre_turn_segment: false
                 }
               else
                 {
                   provider: ($row.payload.provider // .pre_turn_provider // .fallback_provider // $session.model_provider // "openai"),
                   model: ($row.payload.model_used // .pre_turn_model // .fallback_model // $session.model // null),
                   pre_turn_segment: true
                 }
               end) as $usage_model
              | . + {
                  model_context_window: ($row.payload.info.model_context_window // .model_context_window),
                  latest_total_usage: ($row.payload.info.total_token_usage // .latest_total_usage),
                  rate_limits: ($row.payload.rate_limits // .rate_limits),
                  token_count_events: ((.token_count_events // 0) + 1),
                  last_token_count_at: ($row.timestamp // null),
                  has_last_usage: ((.has_last_usage // false) or (($row.payload.info.last_token_usage? | type) == "object"))
                }
              | if ($row.payload.info.last_token_usage? | type) == "object" then
                  ensure_segment(.; $usage_model.provider; $usage_model.model; ($row.payload.info.model_context_window // null); ($row.timestamp // null))
                  | (.segments[.current_segment_id]) |= (
                      . + {
                        token_count_events: ((.token_count_events // 0) + 1),
                        last_token_count_at: ($row.timestamp // null),
                        first_token_count_at: (.first_token_count_at // ($row.timestamp // null)),
                        explicit_model: ($row.payload.model_used != null),
                        pre_turn_segment: ($usage_model.pre_turn_segment // false),
                        model_context_window: ($row.payload.info.model_context_window // .model_context_window),
                        usage_acc: add_usage((.usage_acc // zero_usage); $row.payload.info.last_token_usage)
                      }
                    )
                  | .models_seen = unique_non_null((.models_seen // []); segment_model_key($usage_model.provider; $usage_model.model))
                else
                  .
                end
            )
          elif $row.type == "event_msg" and $row.payload.type == "task_complete" and ($row.payload.turn_id? != null) and (.open_turns[$row.payload.turn_id] != null) then
            emit_turn($session; $row; true; null; "succeeded")
          elif $row.type == "event_msg" and $row.payload.type == "turn_aborted" and ($row.payload.turn_id? != null) and (.open_turns[$row.payload.turn_id] != null) then
            emit_turn($session; $row; false; ($row.payload.reason // "interrupted"); "aborted")
          else
            .
          end;
        ._emit[]?
      )
  ' "$file" >> "$usage_file"

  after_rows=$(llm_usage_count_lines "$usage_file")
  artifact_generated_rows=$((after_rows - before_rows))
  jq -nc \
    --arg source_system "codex" \
    --arg source_kind "interactive_turn_segment" \
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
done < <(find "$sessions_root" -type f -name 'rollout-*.jsonl' -print0 | sort -z)

llm_usage_normalize_json_file "$usage_file"
generated_rows=$(llm_usage_count_lines "$usage_file")

echo "Generated $generated_rows Codex interactive segment usage row(s) from $sessions_root"

if [ "$dry_run" -eq 1 ]; then
  if [ "$generated_rows" -gt 0 ]; then
    echo "Sample Codex row:"
    sed -n '1p' "$usage_file"
  fi
  run_status='succeeded'
  exit 0
fi

llm_usage_run_usage_ingest "$db_url" "$db_schema" "$usage_file"
llm_usage_run_artifact_state_ingest "$db_url" "$db_schema" "$artifact_state_file"
run_status='succeeded'
