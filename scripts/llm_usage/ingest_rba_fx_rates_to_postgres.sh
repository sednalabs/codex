#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: ingest_rba_fx_rates_to_postgres.sh [options]

Options:
  --db-url URL          Postgres connection string. Defaults to LLM_USAGE_DB_URL or the postgres MCP DATABASE_URI in ~/.codex/config.toml.
  --schema NAME         Target schema. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --from YYYY-MM-DD     Inclusive local-Australia date lower bound. Defaults to the first usage event date in the target schema.
  --to YYYY-MM-DD       Inclusive local-Australia date upper bound. Defaults to today in Australia/Sydney.
  --dry-run             Fetch and parse rates but do not write to Postgres.
  --skip-schema         Do not apply schema before ingesting.
  --help                Show this help.
USAGE
}

db_url=${LLM_USAGE_DB_URL:-}
db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}
from_date=
to_date=
dry_run=0
skip_schema=0
rba_csv_url='https://www.rba.gov.au/statistics/tables/csv/f11.1-data.csv'
rba_source_name='rba_f11_1'

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
    --from)
      from_date=${2:-}
      shift 2
      ;;
    --to)
      to_date=${2:-}
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

llm_usage_require_commands python3 mktemp
llm_usage_require_schema_name "$db_schema"
db_url=$(llm_usage_resolve_db_url "$db_url" || true)
if [ "$dry_run" -eq 0 ]; then
  llm_usage_require_commands psql
  llm_usage_require_db_url "$db_url"
  if [ "$skip_schema" -eq 0 ]; then
    llm_usage_apply_schema "$db_url" "$db_schema"
  fi
fi

today_sydney=$(python3 - <<'PY'
from datetime import datetime
from zoneinfo import ZoneInfo
print(datetime.now(ZoneInfo("Australia/Sydney")).date().isoformat())
PY
)

if [ -z "$to_date" ]; then
  to_date=$today_sydney
fi

if [ -z "$from_date" ]; then
  if [ "$dry_run" -eq 1 ]; then
    from_date=$today_sydney
  else
    sql_file=$(mktemp)
    rendered_sql="$sql_file.rendered"
    cat > "$sql_file" <<'SQL'
select coalesce(
  min((event_ts at time zone 'Australia/Sydney')::date)::text,
  (now() at time zone 'Australia/Sydney')::date::text
)
from __LLM_SCHEMA__.llm_usage_events;
SQL
    llm_usage_render_sql_template "$sql_file" "$db_schema" "$rendered_sql"
    from_date=$(llm_usage_psql "$db_url" -At -v ON_ERROR_STOP=1 -f "$rendered_sql" 2>/dev/null || true)
    rm -f "$sql_file" "$rendered_sql"
    if [ -z "$from_date" ]; then
      from_date=$today_sydney
    fi
  fi
fi

python3 - "$from_date" "$to_date" <<'PY'
from datetime import date
import sys

start = date.fromisoformat(sys.argv[1])
end = date.fromisoformat(sys.argv[2])
if start > end:
    raise SystemExit(f"--from {start} must be on or before --to {end}")
PY

tmp_dir=$(mktemp -d)
stage_file="$tmp_dir/rba-fx-stage.csv"
trap 'rm -rf "$tmp_dir"' EXIT

row_count=$(python3 - "$rba_csv_url" "$rba_source_name" "$from_date" "$to_date" "$stage_file" <<'PY'
import csv
from datetime import date, datetime, timezone
from decimal import Decimal, ROUND_HALF_UP
import io
import json
import re
import sys
import urllib.request

csv_url = sys.argv[1]
source_name = sys.argv[2]
from_date = date.fromisoformat(sys.argv[3])
to_date = date.fromisoformat(sys.argv[4])
out_path = sys.argv[5]

with urllib.request.urlopen(csv_url, timeout=30) as response:
    text = response.read().decode("utf-8-sig")

rows = list(csv.reader(io.StringIO(text)))
title_row = next(row for row in rows if row and row[0] == "Title")
source_row = next(row for row in rows if row and row[0] == "Source")
publication_row = next(row for row in rows if row and row[0] == "Publication date")
series_row = next(row for row in rows if row and row[0] == "Series ID")
usd_column = title_row.index("A$1=USD")
date_pattern = re.compile(r"^\d{2}-[A-Za-z]{3}-\d{4}$")
observed_at = datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
quantizer = Decimal("0.0000000001")
count = 0

with open(out_path, "w", encoding="utf-8", newline="") as handle:
    writer = csv.writer(handle)
    for row in rows:
        if not row or not row[0] or not date_pattern.match(row[0]):
            continue
        rate_date = datetime.strptime(row[0], "%d-%b-%Y").date()
        if rate_date < from_date or rate_date > to_date:
            continue
        aud_per_usd_text = row[usd_column].strip() if usd_column < len(row) else ""
        if not aud_per_usd_text:
            continue
        aud_per_usd = Decimal(aud_per_usd_text)
        usd_to_aud = (Decimal("1") / aud_per_usd).quantize(quantizer, rounding=ROUND_HALF_UP)
        raw = {
            "dataset": "F11.1",
            "series_id": series_row[usd_column],
            "quoted_series": title_row[usd_column],
            "quoted_value": aud_per_usd_text,
            "derived_rate_direction": "USD->AUD",
            "publication_date": publication_row[usd_column],
            "source": source_row[usd_column],
        }
        writer.writerow(
            [
                "USD",
                "AUD",
                rate_date.isoformat(),
                format(usd_to_aud, "f"),
                source_name,
                csv_url,
                observed_at,
                json.dumps(raw, separators=(",", ":")),
            ]
        )
        count += 1

print(count)
PY
)

if [ "$row_count" -eq 0 ]; then
  echo "No RBA FX rows found for ${from_date}..${to_date}."
  exit 0
fi

if [ "$dry_run" -eq 1 ]; then
  echo "Prepared ${row_count} RBA FX row(s) for ${from_date}..${to_date}."
  exit 0
fi

ingest_sql=$(mktemp)
rendered_ingest_sql="$ingest_sql.rendered"
cat > "$ingest_sql" <<SQL
DROP TABLE IF EXISTS pg_temp.llm_fx_rate_history_stage;

CREATE TEMP TABLE pg_temp.llm_fx_rate_history_stage (
  base_currency text NOT NULL,
  quote_currency text NOT NULL,
  rate_date date NOT NULL,
  rate_value numeric(18, 10) NOT NULL,
  source_name text NOT NULL,
  source_url text NOT NULL,
  source_observed_at timestamptz NOT NULL,
  raw jsonb NOT NULL
);

\copy pg_temp.llm_fx_rate_history_stage (base_currency, quote_currency, rate_date, rate_value, source_name, source_url, source_observed_at, raw) FROM '$stage_file' WITH (FORMAT csv)

INSERT INTO __LLM_SCHEMA__.llm_fx_rate_history (
  base_currency,
  quote_currency,
  rate_date,
  rate_value,
  source_name,
  source_url,
  source_observed_at,
  raw
)
SELECT
  base_currency,
  quote_currency,
  rate_date,
  rate_value,
  source_name,
  source_url,
  source_observed_at,
  raw
FROM pg_temp.llm_fx_rate_history_stage
ON CONFLICT (base_currency, quote_currency, rate_date, source_name) DO UPDATE SET
  rate_value = EXCLUDED.rate_value,
  source_url = EXCLUDED.source_url,
  source_observed_at = EXCLUDED.source_observed_at,
  raw = EXCLUDED.raw;
SQL
llm_usage_render_sql_template "$ingest_sql" "$db_schema" "$rendered_ingest_sql"
llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$rendered_ingest_sql"
rm -f "$ingest_sql" "$rendered_ingest_sql"

echo "Upserted ${row_count} RBA FX row(s) for ${from_date}..${to_date}."
