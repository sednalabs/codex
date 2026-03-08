#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)

interval_minutes=15
db_schema=${LLM_USAGE_DB_SCHEMA:-llm_usage}
enable_now=1

usage() {
  cat <<'USAGE'
Usage: install_user_timer.sh [options]

Options:
  --interval-minutes N  Run cadence in minutes. Defaults to 15.
  --schema NAME         Schema passed to the scheduled runner. Defaults to LLM_USAGE_DB_SCHEMA or llm_usage.
  --no-enable           Install unit files but do not enable/start the timer.
  --help                Show this help.
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --interval-minutes)
      interval_minutes=${2:-}
      shift 2
      ;;
    --schema)
      db_schema=${2:-}
      shift 2
      ;;
    --no-enable)
      enable_now=0
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

if [[ ! "$interval_minutes" =~ ^[0-9]+$ ]] || [ "$interval_minutes" -lt 1 ]; then
  echo "invalid --interval-minutes value: $interval_minutes" >&2
  exit 1
fi

if [[ ! "$db_schema" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
  echo "invalid --schema value: $db_schema" >&2
  exit 1
fi

user_systemd_dir=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
env_dir=${XDG_CONFIG_HOME:-$HOME/.config}/codex
env_file=$env_dir/llm-usage-ingest.env
service_file=$user_systemd_dir/codex-llm-usage-ingest.service
timer_file=$user_systemd_dir/codex-llm-usage-ingest.timer

umask 077
mkdir -p -- "$user_systemd_dir" "$env_dir"

if [ ! -f "$env_file" ]; then
  cat > "$env_file" <<ENV
# Optional overrides for the scheduled LLM usage ledger ingest.
# LLM_USAGE_DB_URL=
# CODEX_CONFIG_TOML=
# CODEX_USAGE_ROLLOUTS_ROOT=
# GEMINI_CLI_STATE_ROOT=
# GEMINI_MCP_USAGE_LEDGER_PATH=
# LLM_USAGE_LOG_DIR=
# LLM_USAGE_LOG_FILE=
ENV
fi
chmod 600 "$env_file"

cat > "$service_file" <<SERVICE
[Unit]
Description=Codex LLM usage ledger ingest
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
WorkingDirectory=$repo_root
EnvironmentFile=-%h/.config/codex/llm-usage-ingest.env
ExecStart=$repo_root/scripts/llm_usage/run_scheduled_ingest.sh --schema $db_schema
SERVICE
chmod 644 "$service_file"

cat > "$timer_file" <<TIMER
[Unit]
Description=Run Codex LLM usage ledger ingest every $interval_minutes minute(s)

[Timer]
OnBootSec=5m
OnUnitActiveSec=${interval_minutes}m
RandomizedDelaySec=2m
Persistent=true
Unit=codex-llm-usage-ingest.service

[Install]
WantedBy=timers.target
TIMER
chmod 644 "$timer_file"

printf 'Installed %s\n' "$service_file"
printf 'Installed %s\n' "$timer_file"
printf 'Environment template: %s\n' "$env_file"

if [ "$enable_now" -eq 0 ]; then
  exit 0
fi

if ! command -v systemctl >/dev/null 2>&1; then
  echo "systemctl not found; installed unit files but did not enable the timer" >&2
  exit 1
fi

if ! "$repo_root/scripts/llm_usage/ensure_schema.sh" --schema "$db_schema"; then
  echo "warning: schema bootstrap failed during timer install; recurring runs will still try the ingest path" >&2
fi

systemctl --user daemon-reload
systemctl --user enable --now codex-llm-usage-ingest.timer
systemctl --user status codex-llm-usage-ingest.timer --no-pager
