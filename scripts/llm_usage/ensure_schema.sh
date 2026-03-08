#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./_common.sh
source "$script_dir/_common.sh"

usage() {
  cat <<'USAGE'
Usage: ensure_schema.sh [options]

Options:
  --db-url URL    Postgres connection string. Defaults to LLM_USAGE_DB_URL or the postgres MCP DATABASE_URI in ~/.codex/config.toml.
  --schema NAME   Target schema. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --help          Show this help.
USAGE
}

db_url=${LLM_USAGE_DB_URL:-}
db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}

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

llm_usage_require_commands psql python3
llm_usage_require_schema_name "$db_schema"
db_url=$(llm_usage_resolve_db_url "$db_url" || true)
llm_usage_require_db_url "$db_url"
llm_usage_apply_schema "$db_url" "$db_schema"
