#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: report_usage_summary.sh [options]

Options:
  --db-url URL          Postgres connection string. Defaults to LLM_USAGE_DB_URL or the postgres MCP DATABASE_URI in ~/.codex/config.toml.
  --schema NAME         Target schema. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --report NAME         One of: all, freshness, session, model, provider, cost, reconciliation. Defaults to all.
  --cost-view NAME      One of: billing, observed. Defaults to billing.
  --days N              Limit usage queries to the last N days. Use 0 for all time. Defaults to 30.
  --limit N             Limit rows for session/model/provider reports. Defaults to 20.
  --help                Show this help.
USAGE
}

db_url=${LLM_USAGE_DB_URL:-}
db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}
report_name=all
cost_view_name=billing
days=30
limit=20

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
    --report)
      report_name=${2:-}
      shift 2
      ;;
    --cost-view)
      cost_view_name=${2:-}
      shift 2
      ;;
    --days)
      days=${2:-}
      shift 2
      ;;
    --limit)
      limit=${2:-}
      shift 2
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

llm_usage_require_commands psql mktemp python3
llm_usage_require_db_url "$db_url"
llm_usage_require_schema_name "$db_schema"

if [[ ! "$days" =~ ^[0-9]+$ ]]; then
  echo "invalid --days value: $days" >&2
  exit 1
fi

if [[ ! "$limit" =~ ^[0-9]+$ ]] || [ "$limit" -lt 1 ]; then
  echo "invalid --limit value: $limit" >&2
  exit 1
fi

case "$report_name" in
  all|freshness|session|model|provider|cost|reconciliation)
    ;;
  *)
    echo "invalid --report value: $report_name" >&2
    exit 1
    ;;
esac

case "$cost_view_name" in
  billing)
    cost_view_relation='__LLM_SCHEMA__.llm_usage_public_api_costs'
    cost_view_label='billing canonical'
    ;;
  observed)
    cost_view_relation='__LLM_SCHEMA__.llm_usage_public_api_costs_observed'
    cost_view_label='observed rows'
    ;;
  *)
    echo "invalid --cost-view value: $cost_view_name" >&2
    exit 1
    ;;
esac

if [ "$days" -eq 0 ]; then
  usage_time_filter=true
  session_time_filter=true
  window_label='all time'
else
  usage_time_filter="event_ts >= now() - interval '${days} days'"
  session_time_filter="last_event_at >= now() - interval '${days} days'"
  window_label="last ${days} days"
fi

sql_file=$(mktemp)
rendered_sql="$sql_file.rendered"
trap 'rm -f "$sql_file" "$rendered_sql"' EXIT

case "$report_name" in
  freshness)
    cat > "$sql_file" <<SQL
\pset pager off
select
  source_system,
  source_kind,
  count(*) as row_count,
  min(event_ts) as first_event_ts,
  max(event_ts) as last_event_ts,
  max(ingested_at) as last_ingested_at
from __LLM_SCHEMA__.llm_usage_events
group by 1, 2
order by source_system, source_kind;

select
  source_system,
  source_kind,
  count(*) as row_count,
  min(event_ts) as first_event_ts,
  max(event_ts) as last_event_ts,
  max(ingested_at) as last_ingested_at
from __LLM_SCHEMA__.llm_quota_events
group by 1, 2
order by source_system, source_kind;
SQL
    ;;
  session)
    cat > "$sql_file" <<SQL
\pset pager off
select
  source_system,
  source_kind,
  session_id,
  project_key,
  provider,
  model,
  started_at,
  last_event_at,
  session_total_tokens,
  input_tokens,
  cached_input_tokens,
  output_tokens,
  reasoning_tokens,
  tool_tokens,
  tool_call_count,
  tool_call_count_known_event_count,
  tool_call_count_unknown_event_count,
  tool_call_count_coverage,
  succeeded_event_count,
  aborted_event_count,
  failed_event_count
from __LLM_SCHEMA__.llm_session_usage_summary
where $session_time_filter
order by last_event_at desc
limit $limit;
SQL
    ;;
  model)
    cat > "$sql_file" <<SQL
\pset pager off
select
  source_system,
  source_kind,
  coalesce(provider, 'unknown') as provider,
  coalesce(model_used, model_requested, 'unknown') as model,
  count(*) as event_count,
  count(distinct session_id) as session_count,
  sum(coalesce(total_tokens, 0)) as total_tokens,
  sum(coalesce(input_tokens, 0)) as input_tokens,
  sum(coalesce(cached_input_tokens, 0)) as cached_input_tokens,
  sum(coalesce(output_tokens, 0)) as output_tokens,
  sum(coalesce(reasoning_tokens, 0)) as reasoning_tokens,
  max(event_ts) as last_event_at
from __LLM_SCHEMA__.llm_canonical_usage_events
where $usage_time_filter
group by 1, 2, 3, 4
order by total_tokens desc, last_event_at desc
limit $limit;
SQL
    ;;
  provider)
    cat > "$sql_file" <<SQL
\pset pager off
select
  source_system,
  coalesce(provider, 'unknown') as provider,
  count(*) as event_count,
  count(distinct session_id) as session_count,
  sum(coalesce(total_tokens, 0)) as total_tokens,
  sum(coalesce(input_tokens, 0)) as input_tokens,
  sum(coalesce(cached_input_tokens, 0)) as cached_input_tokens,
  sum(coalesce(output_tokens, 0)) as output_tokens,
  sum(coalesce(reasoning_tokens, 0)) as reasoning_tokens,
  max(event_ts) as last_event_at
from __LLM_SCHEMA__.llm_canonical_usage_events
where $usage_time_filter
group by 1, 2
order by total_tokens desc, last_event_at desc
limit $limit;
SQL
    ;;
  cost)
    cat > "$sql_file" <<SQL
\pset pager off
select
  coalesce(provider, 'unknown') as provider,
  model_key,
  cost_status,
  count(*) as event_count,
  round(sum(coalesce(aud_total_cost, 0)), 8) as aud_total_cost,
  round(sum(coalesce(source_total_cost, 0)), 8) as source_total_cost,
  max(event_ts) as last_event_at
from $cost_view_relation
where $usage_time_filter
group by 1, 2, 3
order by aud_total_cost desc nulls last, source_total_cost desc nulls last, last_event_at desc
limit $limit;
SQL
    ;;
  reconciliation)
    cat > "$sql_file" <<SQL
\pset pager off
select
  parser_version,
  source_system,
  source_kind,
  event_status,
  count(*) as row_count,
  min(event_ts) as first_event_ts,
  max(event_ts) as last_event_ts,
  max(ingested_at) as last_ingested_at
from __LLM_SCHEMA__.llm_usage_events
group by 1, 2, 3, 4
order by parser_version desc, source_system, source_kind, event_status;

select
  parser_version,
  source_system,
  source_kind,
  count(*) as row_count,
  min(event_ts) as first_event_ts,
  max(event_ts) as last_event_ts,
  max(ingested_at) as last_ingested_at
from __LLM_SCHEMA__.llm_quota_events
group by 1, 2, 3
order by parser_version desc, source_system, source_kind;
SQL
    ;;
  all)
    cat > "$sql_file" <<SQL
\pset pager off
\echo == Freshness ==
select
  source_system,
  source_kind,
  count(*) as row_count,
  min(event_ts) as first_event_ts,
  max(event_ts) as last_event_ts,
  max(ingested_at) as last_ingested_at
from __LLM_SCHEMA__.llm_usage_events
group by 1, 2
order by source_system, source_kind;

\echo
\echo == Quota Freshness ==
select
  source_system,
  source_kind,
  count(*) as row_count,
  min(event_ts) as first_event_ts,
  max(event_ts) as last_event_ts,
  max(ingested_at) as last_ingested_at
from __LLM_SCHEMA__.llm_quota_events
group by 1, 2
order by source_system, source_kind;

\echo
\echo == Provider Summary ($window_label) ==
select
  source_system,
  coalesce(provider, 'unknown') as provider,
  count(*) as event_count,
  count(distinct session_id) as session_count,
  sum(coalesce(total_tokens, 0)) as total_tokens,
  sum(coalesce(input_tokens, 0)) as input_tokens,
  sum(coalesce(cached_input_tokens, 0)) as cached_input_tokens,
  sum(coalesce(output_tokens, 0)) as output_tokens,
  sum(coalesce(reasoning_tokens, 0)) as reasoning_tokens,
  max(event_ts) as last_event_at
from __LLM_SCHEMA__.llm_usage_events
where $usage_time_filter
group by 1, 2
order by total_tokens desc, last_event_at desc
limit $limit;

\echo
\echo == Model Summary ($window_label) ==
select
  source_system,
  source_kind,
  coalesce(provider, 'unknown') as provider,
  coalesce(model_used, model_requested, 'unknown') as model,
  count(*) as event_count,
  count(distinct session_id) as session_count,
  sum(coalesce(total_tokens, 0)) as total_tokens,
  sum(coalesce(input_tokens, 0)) as input_tokens,
  sum(coalesce(cached_input_tokens, 0)) as cached_input_tokens,
  sum(coalesce(output_tokens, 0)) as output_tokens,
  sum(coalesce(reasoning_tokens, 0)) as reasoning_tokens,
  max(event_ts) as last_event_at
from __LLM_SCHEMA__.llm_usage_events
where $usage_time_filter
group by 1, 2, 3, 4
order by total_tokens desc, last_event_at desc
limit $limit;

\echo
\echo == Cost Summary ($window_label, $cost_view_label) ==
select
  coalesce(provider, 'unknown') as provider,
  model_key,
  cost_status,
  count(*) as event_count,
  round(sum(coalesce(aud_total_cost, 0)), 8) as aud_total_cost,
  round(sum(coalesce(source_total_cost, 0)), 8) as source_total_cost,
  max(event_ts) as last_event_at
from $cost_view_relation
where $usage_time_filter
group by 1, 2, 3
order by aud_total_cost desc nulls last, source_total_cost desc nulls last, last_event_at desc
limit $limit;

\echo
\echo == Reconciliation ==
select
  parser_version,
  source_system,
  source_kind,
  event_status,
  count(*) as row_count,
  min(event_ts) as first_event_ts,
  max(event_ts) as last_event_ts,
  max(ingested_at) as last_ingested_at
from __LLM_SCHEMA__.llm_usage_events
group by 1, 2, 3, 4
order by parser_version desc, source_system, source_kind, event_status;

\echo
\echo == Quota Reconciliation ==
select
  parser_version,
  source_system,
  source_kind,
  count(*) as row_count,
  min(event_ts) as first_event_ts,
  max(event_ts) as last_event_ts,
  max(ingested_at) as last_ingested_at
from __LLM_SCHEMA__.llm_quota_events
group by 1, 2, 3
order by parser_version desc, source_system, source_kind;

\echo
\echo == Session Summary ($window_label) ==
select
  source_system,
  source_kind,
  session_id,
  project_key,
  model,
  started_at,
  last_event_at,
  session_total_tokens,
  input_tokens,
  cached_input_tokens,
  output_tokens,
  reasoning_tokens,
  tool_tokens,
  succeeded_event_count,
  aborted_event_count,
  failed_event_count
from __LLM_SCHEMA__.llm_session_usage_summary
where $session_time_filter
order by last_event_at desc
limit $limit;
SQL
    ;;
esac

llm_usage_render_sql_template "$sql_file" "$db_schema" "$rendered_sql"
llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$rendered_sql"
