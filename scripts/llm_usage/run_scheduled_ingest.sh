#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}
log_dir=${LLM_USAGE_LOG_DIR:-$HOME/.local/state/codex/llm-usage}
log_file=${LLM_USAGE_LOG_FILE:-$log_dir/scheduled-ingest.log}
dry_run=0

usage() {
  cat <<'USAGE'
Usage: run_scheduled_ingest.sh [options]

Options:
  --schema NAME    Target schema. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --log-file PATH  Log file. Defaults to LLM_USAGE_LOG_FILE or ~/.local/state/codex/llm-usage/scheduled-ingest.log.
  --dry-run        Run the ingest bundle in dry-run mode.
  --help           Show this help.
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --schema)
      db_schema=${2:-}
      shift 2
      ;;
    --log-file)
      log_file=${2:-}
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

mkdir -p -- "$(dirname -- "$log_file")"
lock_file="$log_dir/scheduled-ingest.lock"
mkdir -p -- "$log_dir"

acquired_lock=0
if command -v flock >/dev/null 2>&1; then
  exec 9>"$lock_file"
  if flock -n 9; then
    acquired_lock=1
  else
    printf '[%s] scheduled ingest already running; skipping\n' "$(date -Is)" >> "$log_file"
    exit 0
  fi
fi

cmd=("$script_dir/ingest_all_to_postgres.sh" --schema "$db_schema")
if [ "$dry_run" -eq 1 ]; then
  cmd+=(--dry-run)
fi

printf '[%s] starting scheduled ledger ingest (schema=%s dry_run=%s)\n' "$(date -Is)" "$db_schema" "$dry_run" >> "$log_file"
if "${cmd[@]}" >> "$log_file" 2>&1; then
  printf '[%s] scheduled ledger ingest completed successfully\n' "$(date -Is)" >> "$log_file"
else
  status=$?
  printf '[%s] scheduled ledger ingest failed with exit code %s\n' "$(date -Is)" "$status" >> "$log_file"
  exit "$status"
fi

if [ "$acquired_lock" -eq 1 ]; then
  flock -u 9
fi
