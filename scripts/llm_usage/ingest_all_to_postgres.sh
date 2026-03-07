#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

usage() {
  cat <<'USAGE'
Usage: ingest_all_to_postgres.sh [options]

Options:
  --db-url URL          Postgres connection string. Defaults to LLM_USAGE_DB_URL or the postgres MCP DATABASE_URI in ~/.codex/config.toml.
  --schema NAME        Target schema. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --sessions-root PATH  Codex rollout root. Defaults to CODEX_USAGE_ROLLOUTS_ROOT or ~/.codex/sessions.
  --state-root PATH     Gemini CLI state root. Defaults to GEMINI_CLI_STATE_ROOT or ~/.gemini/tmp.
  --ledger PATH         Gemini MCP usage ledger. Defaults to GEMINI_MCP_USAGE_LEDGER_PATH or ~/.local/state/gemini-cli-mcp/token-usage.jsonl.
  --dry-run             Generate normalized rows and print counts without touching Postgres.
  --help                Show this help.
USAGE
}

db_url=${LLM_USAGE_DB_URL:-}
db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}
sessions_root=${CODEX_USAGE_ROLLOUTS_ROOT:-$HOME/.codex/sessions}
state_root=${GEMINI_CLI_STATE_ROOT:-$HOME/.gemini/tmp}
ledger=${GEMINI_MCP_USAGE_LEDGER_PATH:-$HOME/.local/state/gemini-cli-mcp/token-usage.jsonl}
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
    --state-root)
      state_root=${2:-}
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

common_args=(--schema "$db_schema")
if [ -n "$db_url" ]; then
  common_args+=(--db-url "$db_url")
fi
if [ "$dry_run" -eq 1 ]; then
  common_args+=(--dry-run)
fi

echo "Ingesting Codex rollout usage..."
"$script_dir/ingest_codex_rollouts_to_postgres.sh" \
  "${common_args[@]}" \
  --sessions-root "$sessions_root"

echo "Ingesting Gemini interactive usage..."
"$script_dir/ingest_gemini_cli_sessions_to_postgres.sh" \
  "${common_args[@]}" \
  --state-root "$state_root"

if [ -f "$ledger" ]; then
  echo "Ingesting Gemini MCP usage..."
  "$script_dir/ingest_gemini_mcp_usage_to_postgres.sh" \
    "${common_args[@]}" \
    --ledger "$ledger"
else
  echo "Skipping Gemini MCP usage: ledger not found at $ledger"
fi
