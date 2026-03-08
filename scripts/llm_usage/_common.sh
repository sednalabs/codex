#!/usr/bin/env bash
set -euo pipefail

llm_usage_script_dir() {
  cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd
}

llm_usage_parser_version() {
  printf '%s\n' '2026-03-07-v2'
}

llm_usage_require_commands() {
  local cmd
  for cmd in "$@"; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
      echo "missing required command: $cmd" >&2
      exit 1
    fi
  done
}

llm_usage_count_lines() {
  local file=$1
  if [ ! -f "$file" ]; then
    echo 0
    return 0
  fi
  awk 'END { print NR + 0 }' "$file"
}

llm_usage_default_codex_config_path() {
  printf '%s\n' "${CODEX_CONFIG_TOML:-$HOME/.codex/config.toml}"
}

llm_usage_db_url_from_codex_config() {
  local config_path=${1:-$(llm_usage_default_codex_config_path)}

  if [ ! -f "$config_path" ]; then
    return 1
  fi

  sed -n '/^\[mcp_servers\.postgres\.env\]/,/^\[/{
    s/^[[:space:]]*DATABASE_URI[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p
  }' "$config_path" | head -n 1
}

llm_usage_resolve_db_url() {
  local db_url=${1:-}
  local config_path

  if [ -n "$db_url" ]; then
    printf '%s\n' "$db_url"
    return 0
  fi

  config_path=$(llm_usage_default_codex_config_path)
  llm_usage_db_url_from_codex_config "$config_path"
}

llm_usage_require_db_url() {
  local db_url=${1:-}
  if [ -z "$db_url" ]; then
    echo "missing Postgres URL; pass --db-url, set LLM_USAGE_DB_URL, or configure [mcp_servers.postgres.env].DATABASE_URI in $(llm_usage_default_codex_config_path)" >&2
    exit 1
  fi
}

llm_usage_require_schema_name() {
  local db_schema=${1:-}
  if [ -z "$db_schema" ]; then
    echo "missing schema name; pass --schema or set LLM_USAGE_DB_SCHEMA" >&2
    exit 1
  fi
  if [[ ! "$db_schema" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
    echo "invalid schema name: $db_schema" >&2
    echo "schema names must match ^[A-Za-z_][A-Za-z0-9_]*$" >&2
    exit 1
  fi
}

llm_usage_render_sql_template() {
  local template_file=$1
  local db_schema=$2
  local rendered_file=$3

  llm_usage_require_schema_name "$db_schema"
  sed "s/__LLM_SCHEMA__/$db_schema/g" "$template_file" > "$rendered_file"
}

llm_usage_hash_string() {
  local value=${1:-}
  if command -v sha256sum >/dev/null 2>&1; then
    printf '%s' "$value" | sha256sum | awk '{print $1}'
    return 0
  fi
  if command -v shasum >/dev/null 2>&1; then
    printf '%s' "$value" | shasum -a 256 | awk '{print $1}'
    return 0
  fi

  echo "missing required command: sha256sum or shasum" >&2
  exit 1
}

llm_usage_file_metadata() {
  local path=$1
  if stat -c '%s %Y' "$path" >/dev/null 2>&1; then
    stat -c '%s %Y' "$path"
    return 0
  fi
  stat -f '%z %m' "$path"
}

llm_usage_new_run_id() {
  python3 - <<'PY'
import uuid
print(uuid.uuid4())
PY
}

llm_usage_psql() {
  local db_url=$1
  shift

  local service_dir
  service_dir=$(mktemp -d)

  (
    set -euo pipefail
    trap 'rm -rf "$service_dir"' EXIT

    eval "$(python3 - "$db_url" "$service_dir" <<'PY'
import os
import shlex
import sys
from urllib.parse import parse_qsl, unquote, urlparse

SERVICE_NAME = "llm_usage"
OVERRIDE_ENV_VARS = [
    "PGHOST",
    "PGHOSTADDR",
    "PGPORT",
    "PGDATABASE",
    "PGUSER",
    "PGPASSWORD",
    "PGSSLMODE",
    "PGSSLROOTCERT",
    "PGSSLCERT",
    "PGSSLKEY",
    "PGAPPNAME",
    "PGOPTIONS",
    "PGCONNECT_TIMEOUT",
    "PGTARGETSESSIONATTRS",
]
PREFERRED_KEYS = [
    "host",
    "hostaddr",
    "port",
    "user",
    "dbname",
    "sslmode",
    "sslrootcert",
    "sslcert",
    "sslkey",
    "application_name",
    "options",
    "connect_timeout",
    "target_session_attrs",
]


def emit(name, value):
    if value is None or value == "":
        print(f"unset {name}")
    else:
        print(f"export {name}={shlex.quote(str(value))}")


def ensure_scalar(name, value):
    if any(char in value for char in "\r\n\0"):
        raise SystemExit(f"unsupported newline or NUL in Postgres parameter {name!r}")
    return value


def escape_pgpass(value):
    return ensure_scalar("pgpass", value).replace("\\", "\\\\").replace(":", "\\:")


def parse_connection(db_url):
    if "://" in db_url:
        parsed = urlparse(db_url)
        if parsed.scheme not in {"postgres", "postgresql"}:
            raise SystemExit(f"unsupported Postgres URL scheme: {parsed.scheme!r}")

        params = dict(parse_qsl(parsed.query, keep_blank_values=True))
        if parsed.hostname:
            params["host"] = parsed.hostname
        if parsed.port is not None:
            params["port"] = str(parsed.port)
        if parsed.username:
            params["user"] = unquote(parsed.username)
        if parsed.path:
            dbname = unquote(parsed.path[1:]) if parsed.path.startswith("/") else unquote(parsed.path)
            if dbname:
                params["dbname"] = dbname
        password = unquote(parsed.password or "")
        return params, password

    params = {}
    for token in shlex.split(db_url):
        if "=" not in token:
            continue
        key, value = token.split("=", 1)
        params[key] = value
    password = params.pop("password", "")
    return params, password


def write_service_file(service_dir, params):
    service_path = os.path.join(service_dir, "pg_service.conf")
    written = set()
    with open(service_path, "w", encoding="utf-8") as handle:
        handle.write(f"[{SERVICE_NAME}]\n")
        for key in PREFERRED_KEYS:
            value = params.get(key)
            if value is None or value == "":
                continue
            handle.write(f"{key}={ensure_scalar(key, str(value))}\n")
            written.add(key)
        for key in sorted(params):
            if key in written or key == "service":
                continue
            value = params[key]
            if value is None or value == "":
                continue
            handle.write(f"{key}={ensure_scalar(key, str(value))}\n")
    os.chmod(service_path, 0o600)
    return service_path


def write_passfile(service_dir, params, password):
    if password == "":
        return None

    passfile_path = os.path.join(service_dir, "pgpass")
    fields = [
        params.get("host") or params.get("hostaddr") or "*",
        params.get("port") or "*",
        params.get("dbname") or "*",
        params.get("user") or "*",
        password,
    ]
    with open(passfile_path, "w", encoding="utf-8") as handle:
        handle.write(":".join(escape_pgpass(str(field)) for field in fields) + "\n")
    os.chmod(passfile_path, 0o600)
    return passfile_path


def main() -> None:
    db_url = sys.argv[1]
    service_dir = sys.argv[2]
    params, password = parse_connection(db_url)
    service_path = write_service_file(service_dir, params)
    passfile_path = write_passfile(service_dir, params, password)

    for env_var in OVERRIDE_ENV_VARS:
        print(f"unset {env_var}")
    emit("PGSERVICEFILE", service_path)
    emit("PGSERVICE", SERVICE_NAME)
    emit("PGPASSFILE", passfile_path)


if __name__ == "__main__":
    main()
PY
)"
    psql -X "$@"
  )
  local status=$?
  return "$status"
}

llm_usage_apply_schema() {
  local db_url=$1
  local db_schema=$2
  local script_dir rendered_sql

  script_dir=$(llm_usage_script_dir)
  rendered_sql=$(mktemp)
  llm_usage_render_sql_template "$script_dir/ensure_schema.sql" "$db_schema" "$rendered_sql"
  llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$rendered_sql"
  rm -f "$rendered_sql"
}

llm_usage_fetch_artifact_state() {
  local db_url=$1
  local db_schema=$2
  local source_system=$3
  local source_kind=$4
  local output_file=$5
  local sql_file rendered_sql

  sql_file=$(mktemp)
  rendered_sql="$sql_file.rendered"
  cat > "$sql_file" <<SQL
\pset pager off
select
  source_path_hash,
  coalesce(source_size_bytes::text, ''),
  coalesce(source_mtime_epoch::text, ''),
  coalesce(source_row_count::text, '')
from __LLM_SCHEMA__.llm_source_artifacts
where source_system = '$source_system'
  and source_kind = '$source_kind';
SQL
  llm_usage_render_sql_template "$sql_file" "$db_schema" "$rendered_sql"
  llm_usage_psql "$db_url" -At -F $'\t' -v ON_ERROR_STOP=1 -f "$rendered_sql" > "$output_file"
  rm -f "$sql_file" "$rendered_sql"
}

llm_usage_normalize_json_file() {
  local json_file=$1
  local normalized_file
  normalized_file=$(mktemp)

  python3 - "$json_file" "$normalized_file" "$(llm_usage_parser_version)" <<'PY'
import hashlib
import json
import os
import sys

source_path = sys.argv[1]
out_path = sys.argv[2]
parser_version = sys.argv[3]


def digest(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def redact_path(path):
    if not path:
        return None, None
    normalized = os.path.normpath(path)
    base = os.path.basename(normalized) or normalized.strip(os.sep) or "root"
    hashed = digest(normalized)
    return hashed, f"{base}#{hashed[:12]}"


with open(source_path, "r", encoding="utf-8") as handle, open(out_path, "w", encoding="utf-8") as out:
    for raw_line in handle:
        line = raw_line.strip()
        if not line:
            continue
        payload = json.loads(line)
        payload["parser_version"] = parser_version

        logical_key = payload.get("logical_key")
        if not logical_key:
            parts = [
                payload.get("source_system") or "",
                payload.get("source_kind") or "",
                payload.get("session_id") or "",
                payload.get("turn_id") or "",
                payload.get("source_row_id") or "",
            ]
            payload["logical_key"] = "|".join(parts)

        source_hash, source_label = redact_path(payload.get("source_path"))
        if source_hash:
            payload["source_path_hash"] = source_hash
            payload["source_path"] = source_label

        original_project_path = payload.get("project_path")
        project_hash, project_label = redact_path(original_project_path)
        if project_hash:
            project_key = payload.get("project_key")
            if not project_key or project_key == original_project_path:
                payload["project_key"] = project_hash
            payload["project_path"] = project_label

        cwd_hash, cwd_label = redact_path(payload.get("cwd"))
        if cwd_hash:
            payload["cwd"] = cwd_label

        if payload.get("source_kind") in {"interactive_turn", "interactive_message", "mcp_tool_call"} and not payload.get("event_status"):
            if payload.get("error_category") == "interrupted":
                payload["event_status"] = "aborted"
            elif payload.get("ok") is True:
                payload["event_status"] = "succeeded"
            elif payload.get("ok") is False:
                payload["event_status"] = "failed"
            else:
                payload["event_status"] = "observed"

        out.write(json.dumps(payload, separators=(",", ":")))
        out.write("\n")
PY

  mv "$normalized_file" "$json_file"
}

llm_usage_run_usage_ingest() {
  local db_url=$1
  local db_schema=$2
  local usage_file=$3
  local ingest_sql

  if [ ! -s "$usage_file" ]; then
    echo "No usage rows to ingest."
    return 0
  fi

  ingest_sql=$(mktemp)
  cat > "$ingest_sql" <<SQL
DROP TABLE IF EXISTS pg_temp.llm_usage_events_stage;

CREATE TEMP TABLE pg_temp.llm_usage_events_stage (
  raw jsonb NOT NULL
);

\copy pg_temp.llm_usage_events_stage (raw) FROM '$usage_file' WITH (FORMAT csv, DELIMITER E'\x02', QUOTE E'\x01', ESCAPE E'\x01')

INSERT INTO __LLM_SCHEMA__.llm_usage_events (
  record_hash,
  logical_key,
  parser_version,
  ingest_run_id,
  source_system,
  source_kind,
  source_path,
  source_path_hash,
  source_row_id,
  event_ts,
  session_id,
  turn_id,
  project_key,
  project_path,
  cwd,
  tool_name,
  actor,
  provider,
  model_requested,
  model_used,
  ok,
  event_status,
  error_category,
  input_tokens,
  cached_input_tokens,
  output_tokens,
  reasoning_tokens,
  tool_tokens,
  total_tokens,
  cumulative_input_tokens,
  cumulative_cached_input_tokens,
  cumulative_output_tokens,
  cumulative_reasoning_tokens,
  cumulative_total_tokens,
  context_window,
  rate_limit_id,
  rate_limit_name,
  primary_used_percent,
  primary_window_minutes,
  primary_resets_at,
  secondary_used_percent,
  secondary_window_minutes,
  secondary_resets_at,
  credits_balance,
  credits_unlimited,
  raw
)
SELECT
  md5(concat_ws('|', raw->>'logical_key', raw::text)) AS record_hash,
  raw->>'logical_key' AS logical_key,
  coalesce(nullif(raw->>'parser_version', ''), 'unknown') AS parser_version,
  nullif(raw->>'ingest_run_id', '') AS ingest_run_id,
  raw->>'source_system' AS source_system,
  raw->>'source_kind' AS source_kind,
  raw->>'source_path' AS source_path,
  nullif(raw->>'source_path_hash', '') AS source_path_hash,
  nullif(raw->>'source_row_id', '') AS source_row_id,
  (raw->>'event_ts')::timestamptz AS event_ts,
  raw->>'session_id' AS session_id,
  nullif(raw->>'turn_id', '') AS turn_id,
  nullif(raw->>'project_key', '') AS project_key,
  nullif(raw->>'project_path', '') AS project_path,
  nullif(raw->>'cwd', '') AS cwd,
  nullif(raw->>'tool_name', '') AS tool_name,
  nullif(raw->>'actor', '') AS actor,
  nullif(raw->>'provider', '') AS provider,
  nullif(raw->>'model_requested', '') AS model_requested,
  nullif(raw->>'model_used', '') AS model_used,
  CASE WHEN raw ? 'ok' THEN (raw->>'ok')::boolean ELSE NULL END AS ok,
  nullif(raw->>'event_status', '') AS event_status,
  nullif(raw->>'error_category', '') AS error_category,
  nullif(raw->>'input_tokens', '')::bigint AS input_tokens,
  nullif(raw->>'cached_input_tokens', '')::bigint AS cached_input_tokens,
  nullif(raw->>'output_tokens', '')::bigint AS output_tokens,
  nullif(raw->>'reasoning_tokens', '')::bigint AS reasoning_tokens,
  nullif(raw->>'tool_tokens', '')::bigint AS tool_tokens,
  nullif(raw->>'total_tokens', '')::bigint AS total_tokens,
  nullif(raw->>'cumulative_input_tokens', '')::bigint AS cumulative_input_tokens,
  nullif(raw->>'cumulative_cached_input_tokens', '')::bigint AS cumulative_cached_input_tokens,
  nullif(raw->>'cumulative_output_tokens', '')::bigint AS cumulative_output_tokens,
  nullif(raw->>'cumulative_reasoning_tokens', '')::bigint AS cumulative_reasoning_tokens,
  nullif(raw->>'cumulative_total_tokens', '')::bigint AS cumulative_total_tokens,
  nullif(raw->>'context_window', '')::bigint AS context_window,
  nullif(raw->>'rate_limit_id', '') AS rate_limit_id,
  nullif(raw->>'rate_limit_name', '') AS rate_limit_name,
  nullif(raw->>'primary_used_percent', '')::double precision AS primary_used_percent,
  nullif(raw->>'primary_window_minutes', '')::bigint AS primary_window_minutes,
  nullif(raw->>'primary_resets_at', '')::timestamptz AS primary_resets_at,
  nullif(raw->>'secondary_used_percent', '')::double precision AS secondary_used_percent,
  nullif(raw->>'secondary_window_minutes', '')::bigint AS secondary_window_minutes,
  nullif(raw->>'secondary_resets_at', '')::timestamptz AS secondary_resets_at,
  nullif(raw->>'credits_balance', '') AS credits_balance,
  CASE WHEN raw ? 'credits_unlimited' THEN (raw->>'credits_unlimited')::boolean ELSE NULL END AS credits_unlimited,
  raw AS raw
FROM pg_temp.llm_usage_events_stage
ON CONFLICT (logical_key) DO UPDATE SET
  record_hash = EXCLUDED.record_hash,
  parser_version = EXCLUDED.parser_version,
  ingest_run_id = EXCLUDED.ingest_run_id,
  source_system = EXCLUDED.source_system,
  source_kind = EXCLUDED.source_kind,
  source_path = EXCLUDED.source_path,
  source_path_hash = EXCLUDED.source_path_hash,
  source_row_id = EXCLUDED.source_row_id,
  event_ts = EXCLUDED.event_ts,
  session_id = EXCLUDED.session_id,
  turn_id = EXCLUDED.turn_id,
  project_key = EXCLUDED.project_key,
  project_path = EXCLUDED.project_path,
  cwd = EXCLUDED.cwd,
  tool_name = EXCLUDED.tool_name,
  actor = EXCLUDED.actor,
  provider = EXCLUDED.provider,
  model_requested = EXCLUDED.model_requested,
  model_used = EXCLUDED.model_used,
  ok = EXCLUDED.ok,
  event_status = EXCLUDED.event_status,
  error_category = EXCLUDED.error_category,
  input_tokens = EXCLUDED.input_tokens,
  cached_input_tokens = EXCLUDED.cached_input_tokens,
  output_tokens = EXCLUDED.output_tokens,
  reasoning_tokens = EXCLUDED.reasoning_tokens,
  tool_tokens = EXCLUDED.tool_tokens,
  total_tokens = EXCLUDED.total_tokens,
  cumulative_input_tokens = EXCLUDED.cumulative_input_tokens,
  cumulative_cached_input_tokens = EXCLUDED.cumulative_cached_input_tokens,
  cumulative_output_tokens = EXCLUDED.cumulative_output_tokens,
  cumulative_reasoning_tokens = EXCLUDED.cumulative_reasoning_tokens,
  cumulative_total_tokens = EXCLUDED.cumulative_total_tokens,
  context_window = EXCLUDED.context_window,
  rate_limit_id = EXCLUDED.rate_limit_id,
  rate_limit_name = EXCLUDED.rate_limit_name,
  primary_used_percent = EXCLUDED.primary_used_percent,
  primary_window_minutes = EXCLUDED.primary_window_minutes,
  primary_resets_at = EXCLUDED.primary_resets_at,
  secondary_used_percent = EXCLUDED.secondary_used_percent,
  secondary_window_minutes = EXCLUDED.secondary_window_minutes,
  secondary_resets_at = EXCLUDED.secondary_resets_at,
  credits_balance = EXCLUDED.credits_balance,
  credits_unlimited = EXCLUDED.credits_unlimited,
  raw = EXCLUDED.raw,
  ingested_at = now();
SQL

  llm_usage_render_sql_template "$ingest_sql" "$db_schema" "$ingest_sql.rendered"
  llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$ingest_sql.rendered"
  rm -f "$ingest_sql" "$ingest_sql.rendered"
}

llm_usage_run_quota_ingest() {
  local db_url=$1
  local db_schema=$2
  local quota_file=$3
  local ingest_sql

  if [ ! -s "$quota_file" ]; then
    echo "No quota rows to ingest."
    return 0
  fi

  ingest_sql=$(mktemp)
  cat > "$ingest_sql" <<SQL
DROP TABLE IF EXISTS pg_temp.llm_quota_events_stage;

CREATE TEMP TABLE pg_temp.llm_quota_events_stage (
  raw jsonb NOT NULL
);

\copy pg_temp.llm_quota_events_stage (raw) FROM '$quota_file' WITH (FORMAT csv, DELIMITER E'\x02', QUOTE E'\x01', ESCAPE E'\x01')

INSERT INTO __LLM_SCHEMA__.llm_quota_events (
  record_hash,
  logical_key,
  parser_version,
  ingest_run_id,
  source_system,
  source_kind,
  source_path,
  source_path_hash,
  source_row_id,
  event_ts,
  session_id,
  project_key,
  project_path,
  model_used,
  tool_name,
  error_message,
  reset_after_text,
  reset_after_seconds,
  raw
)
SELECT
  md5(concat_ws('|', raw->>'logical_key', raw::text)) AS record_hash,
  raw->>'logical_key' AS logical_key,
  coalesce(nullif(raw->>'parser_version', ''), 'unknown') AS parser_version,
  nullif(raw->>'ingest_run_id', '') AS ingest_run_id,
  raw->>'source_system' AS source_system,
  raw->>'source_kind' AS source_kind,
  raw->>'source_path' AS source_path,
  nullif(raw->>'source_path_hash', '') AS source_path_hash,
  nullif(raw->>'source_row_id', '') AS source_row_id,
  (raw->>'event_ts')::timestamptz AS event_ts,
  raw->>'session_id' AS session_id,
  nullif(raw->>'project_key', '') AS project_key,
  nullif(raw->>'project_path', '') AS project_path,
  nullif(raw->>'model_used', '') AS model_used,
  nullif(raw->>'tool_name', '') AS tool_name,
  raw->>'error_message' AS error_message,
  nullif(raw->>'reset_after_text', '') AS reset_after_text,
  nullif(raw->>'reset_after_seconds', '')::bigint AS reset_after_seconds,
  raw AS raw
FROM pg_temp.llm_quota_events_stage
ON CONFLICT (logical_key) DO UPDATE SET
  record_hash = EXCLUDED.record_hash,
  parser_version = EXCLUDED.parser_version,
  ingest_run_id = EXCLUDED.ingest_run_id,
  source_system = EXCLUDED.source_system,
  source_kind = EXCLUDED.source_kind,
  source_path = EXCLUDED.source_path,
  source_path_hash = EXCLUDED.source_path_hash,
  source_row_id = EXCLUDED.source_row_id,
  event_ts = EXCLUDED.event_ts,
  session_id = EXCLUDED.session_id,
  project_key = EXCLUDED.project_key,
  project_path = EXCLUDED.project_path,
  model_used = EXCLUDED.model_used,
  tool_name = EXCLUDED.tool_name,
  error_message = EXCLUDED.error_message,
  reset_after_text = EXCLUDED.reset_after_text,
  reset_after_seconds = EXCLUDED.reset_after_seconds,
  raw = EXCLUDED.raw,
  ingested_at = now();
SQL

  llm_usage_render_sql_template "$ingest_sql" "$db_schema" "$ingest_sql.rendered"
  llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$ingest_sql.rendered"
  rm -f "$ingest_sql" "$ingest_sql.rendered"
}

llm_usage_run_artifact_state_ingest() {
  local db_url=$1
  local db_schema=$2
  local state_file=$3
  local ingest_sql

  if [ ! -s "$state_file" ]; then
    return 0
  fi

  ingest_sql=$(mktemp)
  cat > "$ingest_sql" <<SQL
DROP TABLE IF EXISTS pg_temp.llm_source_artifacts_stage;

CREATE TEMP TABLE pg_temp.llm_source_artifacts_stage (
  raw jsonb NOT NULL
);

\copy pg_temp.llm_source_artifacts_stage (raw) FROM '$state_file' WITH (FORMAT csv, DELIMITER E'\x02', QUOTE E'\x01', ESCAPE E'\x01')

INSERT INTO __LLM_SCHEMA__.llm_source_artifacts (
  source_system,
  source_kind,
  source_path_hash,
  source_path,
  source_size_bytes,
  source_mtime_epoch,
  source_row_count,
  parser_version,
  last_ingest_run_id,
  last_ingested_at,
  status,
  raw
)
SELECT
  raw->>'source_system' AS source_system,
  raw->>'source_kind' AS source_kind,
  raw->>'source_path_hash' AS source_path_hash,
  raw->>'source_path' AS source_path,
  nullif(raw->>'source_size_bytes', '')::bigint AS source_size_bytes,
  nullif(raw->>'source_mtime_epoch', '')::bigint AS source_mtime_epoch,
  nullif(raw->>'source_row_count', '')::bigint AS source_row_count,
  coalesce(nullif(raw->>'parser_version', ''), 'unknown') AS parser_version,
  nullif(raw->>'last_ingest_run_id', '') AS last_ingest_run_id,
  coalesce(nullif(raw->>'last_ingested_at', '')::timestamptz, now()) AS last_ingested_at,
  coalesce(nullif(raw->>'status', ''), 'processed') AS status,
  coalesce(raw->'raw', '{}'::jsonb) AS raw
FROM pg_temp.llm_source_artifacts_stage
ON CONFLICT (source_system, source_kind, source_path_hash) DO UPDATE SET
  source_path = EXCLUDED.source_path,
  source_size_bytes = EXCLUDED.source_size_bytes,
  source_mtime_epoch = EXCLUDED.source_mtime_epoch,
  source_row_count = EXCLUDED.source_row_count,
  parser_version = EXCLUDED.parser_version,
  last_ingest_run_id = EXCLUDED.last_ingest_run_id,
  last_ingested_at = EXCLUDED.last_ingested_at,
  status = EXCLUDED.status,
  raw = EXCLUDED.raw;
SQL

  llm_usage_render_sql_template "$ingest_sql" "$db_schema" "$ingest_sql.rendered"
  llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$ingest_sql.rendered"
  rm -f "$ingest_sql" "$ingest_sql.rendered"
}

llm_usage_run_ingest_run_ingest() {
  local db_url=$1
  local db_schema=$2
  local run_file=$3
  local ingest_sql

  if [ ! -s "$run_file" ]; then
    return 0
  fi

  ingest_sql=$(mktemp)
  cat > "$ingest_sql" <<SQL
DROP TABLE IF EXISTS pg_temp.llm_ingest_runs_stage;

CREATE TEMP TABLE pg_temp.llm_ingest_runs_stage (
  raw jsonb NOT NULL
);

\copy pg_temp.llm_ingest_runs_stage (raw) FROM '$run_file' WITH (FORMAT csv, DELIMITER E'\x02', QUOTE E'\x01', ESCAPE E'\x01')

INSERT INTO __LLM_SCHEMA__.llm_ingest_runs (
  run_id,
  script_name,
  parser_version,
  source_system,
  source_kind,
  dry_run,
  started_at,
  completed_at,
  status,
  processed_artifacts,
  skipped_artifacts,
  generated_rows,
  error_text,
  raw
)
SELECT
  raw->>'run_id' AS run_id,
  raw->>'script_name' AS script_name,
  coalesce(nullif(raw->>'parser_version', ''), 'unknown') AS parser_version,
  nullif(raw->>'source_system', '') AS source_system,
  nullif(raw->>'source_kind', '') AS source_kind,
  CASE WHEN raw ? 'dry_run' THEN (raw->>'dry_run')::boolean ELSE false END AS dry_run,
  coalesce(nullif(raw->>'started_at', '')::timestamptz, now()) AS started_at,
  nullif(raw->>'completed_at', '')::timestamptz AS completed_at,
  coalesce(nullif(raw->>'status', ''), 'running') AS status,
  nullif(raw->>'processed_artifacts', '')::bigint AS processed_artifacts,
  nullif(raw->>'skipped_artifacts', '')::bigint AS skipped_artifacts,
  nullif(raw->>'generated_rows', '')::bigint AS generated_rows,
  nullif(raw->>'error_text', '') AS error_text,
  coalesce(raw->'raw', '{}'::jsonb) AS raw
FROM pg_temp.llm_ingest_runs_stage
ON CONFLICT (run_id) DO UPDATE SET
  script_name = EXCLUDED.script_name,
  parser_version = EXCLUDED.parser_version,
  source_system = EXCLUDED.source_system,
  source_kind = EXCLUDED.source_kind,
  dry_run = EXCLUDED.dry_run,
  started_at = EXCLUDED.started_at,
  completed_at = EXCLUDED.completed_at,
  status = EXCLUDED.status,
  processed_artifacts = EXCLUDED.processed_artifacts,
  skipped_artifacts = EXCLUDED.skipped_artifacts,
  generated_rows = EXCLUDED.generated_rows,
  error_text = EXCLUDED.error_text,
  raw = EXCLUDED.raw;
SQL

  llm_usage_render_sql_template "$ingest_sql" "$db_schema" "$ingest_sql.rendered"
  llm_usage_psql "$db_url" -v ON_ERROR_STOP=1 -f "$ingest_sql.rendered"
  rm -f "$ingest_sql" "$ingest_sql.rendered"
}
