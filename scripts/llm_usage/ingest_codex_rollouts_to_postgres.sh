#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: ingest_codex_rollouts_to_postgres.sh [options]

Options:
  --db-url URL          Postgres connection string. Defaults to LLM_USAGE_DB_URL or the postgres MCP DATABASE_URI in ~/.codex/config.toml.
  --schema NAME        Target schema. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --sessions-root PATH  Codex rollout root. Defaults to CODEX_USAGE_ROLLOUTS_ROOT or ~/.codex/sessions.
  --dry-run             Generate normalized rows and print counts without touching Postgres.
  --help                Show this help.
USAGE
}

db_url=${LLM_USAGE_DB_URL:-}
db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}
sessions_root=${CODEX_USAGE_ROLLOUTS_ROOT:-$HOME/.codex/sessions}
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
    --sessions-root)
      sessions_root=${2:-}
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

if [ ! -d "$sessions_root" ]; then
  echo "Codex rollout root does not exist: $sessions_root" >&2
  exit 1
fi

usage_file=$(mktemp)
trap 'rm -f "$usage_file"' EXIT

while IFS= read -r -d '' file; do
  jq -sc --arg source_path "$file" '
    def delta(curr; prev):
      if curr == null then null
      elif prev == null then curr
      elif curr < prev then null
      else curr - prev
      end;

    . as $rows
    | (map(select(.type == "session_meta") | .payload)[0] // {}) as $session
    | foreach $rows[] as $row (
        {
          session_meta: $session,
          contexts: {},
          open_turns: {},
          active_turn_id: null,
          last_completed_cumulative: null,
          _emit: null
        };
        ._emit = null
        | if $row.type == "turn_context" and ($row.payload.turn_id? != null) then
            .contexts[$row.payload.turn_id] = {
              cwd: ($row.payload.cwd // $session.cwd // null),
              model: ($row.payload.model // $session.model // null),
              provider: ($session.model_provider // null),
              model_context_window: ($row.payload.model_context_window // null)
            }
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
              last_token_count_at: (.open_turns[$row.payload.turn_id].last_token_count_at // null)
            }
            | .active_turn_id = $row.payload.turn_id
          elif $row.type == "event_msg" and $row.payload.type == "token_count" and (.active_turn_id != null) and (.open_turns[.active_turn_id] != null) then
            .open_turns[.active_turn_id] |= (
              . + {
                model_context_window: ($row.payload.info.model_context_window // .model_context_window),
                latest_total_usage: ($row.payload.info.total_token_usage // .latest_total_usage),
                rate_limits: ($row.payload.rate_limits // .rate_limits),
                token_count_events: ((.token_count_events // 0) + 1),
                last_token_count_at: ($row.timestamp // null)
              }
            )
          elif $row.type == "event_msg" and $row.payload.type == "task_complete" and ($row.payload.turn_id? != null) and (.open_turns[$row.payload.turn_id] != null) then
            (.open_turns[$row.payload.turn_id]) as $turn
            | (.contexts[$row.payload.turn_id] // {}) as $ctx
            | ($turn.latest_total_usage // {}) as $usage
            | (.last_completed_cumulative // {}) as $prev
            | ._emit = {
                source_system: "codex",
                source_kind: "interactive_turn",
                source_path: $source_path,
                source_row_id: $row.payload.turn_id,
                event_ts: ($row.timestamp),
                session_id: ($session.id // $row.payload.turn_id),
                turn_id: $row.payload.turn_id,
                project_key: ($ctx.cwd // $session.cwd // null),
                project_path: ($ctx.cwd // $session.cwd // null),
                cwd: ($ctx.cwd // $session.cwd // null),
                tool_name: null,
                actor: "assistant",
                provider: ($ctx.provider // $session.model_provider // "openai"),
                model_requested: ($ctx.model // $session.model // null),
                model_used: ($ctx.model // $session.model // null),
                ok: true,
                error_category: null,
                input_tokens: delta($usage.input_tokens; $prev.input_tokens),
                cached_input_tokens: delta($usage.cached_input_tokens; $prev.cached_input_tokens),
                output_tokens: delta($usage.output_tokens; $prev.output_tokens),
                reasoning_tokens: delta($usage.reasoning_output_tokens; $prev.reasoning_output_tokens),
                tool_tokens: null,
                total_tokens: delta($usage.total_tokens; $prev.total_tokens),
                cumulative_input_tokens: ($usage.input_tokens // null),
                cumulative_cached_input_tokens: ($usage.cached_input_tokens // null),
                cumulative_output_tokens: ($usage.output_tokens // null),
                cumulative_reasoning_tokens: ($usage.reasoning_output_tokens // null),
                cumulative_total_tokens: ($usage.total_tokens // null),
                context_window: ($turn.model_context_window // $ctx.model_context_window // null),
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
                  last_token_count_at: ($turn.last_token_count_at // null),
                  token_count_events: ($turn.token_count_events // 0),
                  compaction_events_in_turn: ($row.payload.compaction_events_in_turn // null),
                  source_session_id: ($session.id // null)
                }
              }
            | if $usage.total_tokens != null then
                .last_completed_cumulative = {
                  input_tokens: ($usage.input_tokens // null),
                  cached_input_tokens: ($usage.cached_input_tokens // null),
                  output_tokens: ($usage.output_tokens // null),
                  reasoning_output_tokens: ($usage.reasoning_output_tokens // null),
                  total_tokens: ($usage.total_tokens // null)
                }
              else
                .
              end
            | del(.open_turns[$row.payload.turn_id])
            | if .active_turn_id == $row.payload.turn_id then
                .active_turn_id = null
              else
                .
              end
          else
            .
          end;
        ._emit // empty
      )
  ' "$file" >> "$usage_file"
done < <(find "$sessions_root" -type f -name 'rollout-*.jsonl' -print0 | sort -z)

usage_rows=$(llm_usage_count_lines "$usage_file")
echo "Generated $usage_rows Codex interactive usage row(s) from $sessions_root"

if [ "$dry_run" -eq 1 ]; then
  if [ "$usage_rows" -gt 0 ]; then
    echo "Sample Codex row:"
    sed -n '1p' "$usage_file"
  fi
  exit 0
fi

llm_usage_apply_schema "$db_url" "$db_schema"
llm_usage_run_usage_ingest "$db_url" "$db_schema" "$usage_file"
