#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Resume recent non-running Codex sessions into new tmux windows.

Usage:
  codex-resume-recent.sh [options]

Options:
  -n, --count <N>        Number of sessions to open (default: 5)
  -l, --lookback <N>     Lines to inspect from end of history.jsonl (default: 1500)
  -f, --history <PATH>   History file path (default: $CODEX_HOME/history.jsonl)
  -p, --prefix <TEXT>    Tmux window name prefix (default: cx)
  -t, --target <TARGET>  Optional tmux target (session/window target)
  --launch-cwd           Open every window in the current directory instead of
                         each session's recorded cwd
  --include-side         Include side-chat sessions in the selection
  --dry-run              Print selected IDs without opening tmux windows
  -h, --help             Show this help

Examples:
  codex-resume-recent.sh
  codex-resume-recent.sh -n 8
  codex-resume-recent.sh -n 5 --lookback 300 --dry-run
EOF
}

count=5
lookback=1500
prefix="cx"
target=""
dry_run=0
launch_cwd=0
include_side_chats=0
codex_home="${CODEX_HOME:-$HOME/.codex}"
history_file="$codex_home/history.jsonl"

while (($# > 0)); do
  case "$1" in
    -n|--count)
      count="${2:-}"
      shift 2
      ;;
    -l|--lookback)
      lookback="${2:-}"
      shift 2
      ;;
    -f|--history)
      history_file="${2:-}"
      shift 2
      ;;
    -p|--prefix)
      prefix="${2:-}"
      shift 2
      ;;
    -t|--target)
      target="${2:-}"
      shift 2
      ;;
    --launch-cwd)
      launch_cwd=1
      shift
      ;;
    --include-side|--include-side-chats)
      include_side_chats=1
      shift
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if ! [[ "$count" =~ ^[0-9]+$ ]] || ((count < 1)); then
  echo "--count must be a positive integer" >&2
  exit 2
fi

if ! [[ "$lookback" =~ ^[0-9]+$ ]] || ((lookback < 1)); then
  echo "--lookback must be a positive integer" >&2
  exit 2
fi

if [[ ! -f "$history_file" ]]; then
  echo "History file not found: $history_file" >&2
  exit 1
fi

for cmd in jq ps tac awk sort; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "Missing required command: $cmd" >&2
    exit 1
  fi
done

if ! command -v codex >/dev/null 2>&1; then
  echo "Could not find 'codex' on PATH." >&2
  echo "Install it or wrap this script with your preferred codex command." >&2
  exit 1
fi

session_file_for_id() {
  local id="$1"
  find "$codex_home/sessions" -type f -name "*${id}.jsonl" 2>/dev/null | head -n 1 || true
}

session_cwd() {
  local id="$1"
  local recorded_cwd
  local session_file

  session_file="$(session_file_for_id "$id")"
  [[ -z "$session_file" ]] && return 1
  recorded_cwd="$(jq -r 'select(.type == "session_meta") | .payload.cwd // empty' "$session_file" | head -n 1)"
  if [[ -n "$recorded_cwd" && -d "$recorded_cwd" ]]; then
    printf '%s\n' "$recorded_cwd"
    return 0
  fi

  return 1
}

session_meta_marks_side_chat() {
  local session_file="$1"
  jq -n -e '
    any(
      inputs | select(.type == "session_meta");
      (.payload.thread_source? //
       .payload.threadSource? //
       .payload.meta.thread_source? //
       .payload.meta.threadSource? //
       "") == "side"
    )
  ' "$session_file" >/dev/null 2>&1
}

usage_db_marks_side_chat() {
  local id="$1"
  local db
  local marker

  [[ "$id" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] || return 1
  command -v sqlite3 >/dev/null 2>&1 || return 1

  for db in "$codex_home/usage.sqlite" "$codex_home/usage_1.sqlite"; do
    [[ -f "$db" ]] || continue
    marker="$(
      sqlite3 -readonly "$db" \
        "SELECT 1 FROM pragma_table_info('usage_threads') WHERE name = 'thread_source' LIMIT 1;" \
        2>/dev/null || true
    )"
    [[ "$marker" == "1" ]] || continue
    marker="$(
      sqlite3 -readonly "$db" \
        "SELECT 1 FROM usage_threads WHERE thread_id = '$id' AND thread_source = 'side' LIMIT 1;" \
        2>/dev/null || true
    )"
    [[ "$marker" == "1" ]] && return 0
  done

  return 1
}

session_is_side_chat() {
  local id="$1"
  local session_file

  session_file="$(session_file_for_id "$id")"
  if [[ -n "$session_file" ]] && session_meta_marks_side_chat "$session_file"; then
    return 0
  fi
  usage_db_marks_side_chat "$id"
}

mapfile -t running_ids < <(
  ps -eo args= | awk 'BEGIN{IGNORECASE=1}
    /codex/ && / resume / {
      if (match($0, /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/)) {
        print substr($0, RSTART, RLENGTH)
      }
    }' | sort -u
)

mapfile -t recent_ids < <(
  tail -n "$lookback" "$history_file" \
    | jq -r '.session_id // empty' \
    | tac \
    | awk '!seen[$0]++'
)

declare -A running_map=()
for id in "${running_ids[@]}"; do
  [[ -z "$id" ]] && continue
  running_map["$id"]=1
done

selected_ids=()
for id in "${recent_ids[@]}"; do
  [[ -z "$id" ]] && continue
  if [[ -n "${running_map[$id]+x}" ]]; then
    continue
  fi
  if ((include_side_chats == 0)) && session_is_side_chat "$id"; then
    continue
  fi
  selected_ids+=("$id")
  if ((${#selected_ids[@]} >= count)); then
    break
  fi
done

if ((${#selected_ids[@]} == 0)); then
  echo "No non-running session IDs found in the last $lookback history lines."
  exit 0
fi

if ((dry_run == 1)); then
  printf '%s\n' "${selected_ids[@]}"
  exit 0
fi

if ! command -v tmux >/dev/null 2>&1; then
  echo "Missing required command: tmux" >&2
  exit 1
fi

if [[ -z "$target" && -z "${TMUX:-}" ]]; then
  echo "Not inside tmux. Start tmux first, or pass --target <session>." >&2
  exit 1
fi

cwd="$PWD"
shell_cmd="${SHELL:-bash}"

for id in "${selected_ids[@]}"; do
  win_name="${prefix}-${id:0:8}"
  window_cwd="$cwd"
  if ((launch_cwd == 0)); then
    window_cwd="$(session_cwd "$id" || printf '%s\n' "$cwd")"
  fi
  run_cmd="bash -lc 'codex resume $id; rc=\$?; echo; echo \"[codex exited: $id status \$rc]\"; exec $shell_cmd -li'"
  if [[ -n "$target" ]]; then
    tmux new-window -t "$target" -n "$win_name" -c "$window_cwd" "$run_cmd"
  else
    tmux new-window -n "$win_name" -c "$window_cwd" "$run_cmd"
  fi
done

echo "Opened ${#selected_ids[@]} tmux windows."
printf '  %s\n' "${selected_ids[@]}"
