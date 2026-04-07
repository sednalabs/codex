#!/usr/bin/env python3
from __future__ import annotations

import argparse
import glob
import json
import sqlite3
from pathlib import Path
from typing import Any, Iterable

SESSIONS_ROOT = Path.home() / ".codex" / "sessions"
USAGE_DB = Path.home() / ".codex" / "usage_1.sqlite"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Inspect a subagent's recent session tail and usage heartbeat.")
    parser.add_argument("--child-thread-id")
    parser.add_argument("--parent-thread-id")
    parser.add_argument("--agent-path")
    parser.add_argument("--days", type=int, default=3)
    parser.add_argument("--tail", type=int, default=10)
    parser.add_argument("--no-usage", action="store_true")
    args = parser.parse_args()
    if not args.child_thread_id and not (args.parent_thread_id and args.agent_path):
        parser.error("provide --child-thread-id, or both --parent-thread-id and --agent-path")
    return args


def recent_day_dirs(days: int) -> list[Path]:
    day_dirs = sorted([p for p in SESSIONS_ROOT.glob("*/*/*") if p.is_dir()], reverse=True)
    return day_dirs[:days]


def ensure_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


def first_json_line(path: Path) -> dict | None:
    try:
        with path.open() as f:
            line = f.readline().strip()
        return json.loads(line) if line else None
    except Exception:
        return None


def shorten(text: str, limit: int = 140) -> str:
    clean = " ".join(text.split())
    return clean if len(clean) <= limit else clean[: limit - 3] + "..."


def summarize_record(obj: dict) -> str | None:
    ts = obj.get("timestamp", "?")
    typ = obj.get("type")
    payload = obj.get("payload", {})
    if typ == "event_msg":
        event_type = payload.get("type")
        if event_type == "agent_message":
            return f"{ts} event agent_message {shorten(payload.get('message', ''))}"
        if event_type == "task_started":
            return f"{ts} event task_started"
        if event_type == "task_complete":
            return f"{ts} event task_complete"
        if event_type == "turn_aborted":
            reason = payload.get("reason") or "-"
            return f"{ts} event turn_aborted reason={reason}"
        if event_type == "token_count":
            info = payload.get("info", {}) or {}
            total = (info.get("total_token_usage") or {}).get("total_tokens")
            model = payload.get("model_used")
            return f"{ts} event token_count model={model} total_tokens={total}"
        return None
    if typ != "response_item":
        return None
    item_type = payload.get("type")
    if item_type in {"function_call", "custom_tool_call"}:
        name = payload.get("name")
        return f"{ts} call {name}"
    if item_type in {"function_call_output", "custom_tool_call_output"}:
        out = payload.get("output", "")
        first = out.splitlines()[0] if out else ""
        return f"{ts} output {shorten(first)}"
    if item_type == "message":
        role = payload.get("role")
        texts = []
        for part in payload.get("content", []):
            text = part.get("text")
            if text:
                texts.append(text)
        if texts:
            return f"{ts} message {role} {shorten(' '.join(texts))}"
    return None


def inspect_session(path: Path, tail: int) -> dict[str, Any]:
    meta = read_meta(path)
    rows: list[str] = []
    last_timestamp = None
    last_task_started = None
    last_terminal = None
    with path.open() as f:
        for line in f:
            try:
                obj = json.loads(line)
            except Exception:
                continue
            timestamp = obj.get("timestamp")
            if timestamp:
                last_timestamp = timestamp
            payload = ensure_dict(obj.get("payload"))
            if obj.get("type") == "event_msg":
                event_type = payload.get("type")
                if event_type == "task_started":
                    last_task_started = {
                        "timestamp": timestamp,
                        "turn_id": payload.get("turn_id"),
                    }
                elif event_type == "task_complete":
                    last_terminal = {
                        "state": "completed",
                        "event_type": event_type,
                        "timestamp": timestamp,
                        "turn_id": payload.get("turn_id"),
                        "reason": None,
                    }
                elif event_type == "turn_aborted":
                    last_terminal = {
                        "state": "interrupted",
                        "event_type": event_type,
                        "timestamp": timestamp,
                        "turn_id": payload.get("turn_id"),
                        "reason": payload.get("reason"),
                    }
            row = summarize_record(obj)
            if row:
                rows.append(row)

    session_state = "unknown"
    if last_task_started and (
        not last_terminal
        or (last_task_started.get("timestamp") or "") > (last_terminal.get("timestamp") or "")
    ):
        session_state = "active"
    elif last_terminal:
        session_state = str(last_terminal.get("state") or "unknown")
    elif last_timestamp:
        session_state = "active"

    return {
        "path": path,
        "meta": meta,
        "tail_rows": rows[-tail:],
        "last_timestamp": last_timestamp,
        "last_task_started": last_task_started,
        "last_terminal": last_terminal,
        "session_state": session_state,
    }


def session_sort_key(info: dict[str, Any]) -> tuple[str, int, str]:
    state = info.get("session_state")
    active_rank = 1 if state == "active" else 0
    return (str(info.get("last_timestamp") or ""), active_rank, str(info.get("path") or ""))


def select_best_session(infos: Iterable[dict[str, Any]]) -> dict[str, Any] | None:
    materialized = list(infos)
    if not materialized:
        return None
    return max(materialized, key=session_sort_key)


def find_by_child_thread_id(child_thread_id: str, tail: int) -> tuple[dict[str, Any] | None, int]:
    matches = []
    for path in sorted(SESSIONS_ROOT.rglob(f"rollout-*{child_thread_id}.jsonl")):
        info = inspect_session(path, tail)
        if info.get("meta", {}).get("id") != child_thread_id:
            continue
        matches.append(info)
    return select_best_session(matches), len(matches)


def find_by_parent_and_agent(parent_thread_id: str, agent_path: str, days: int, tail: int) -> tuple[dict[str, Any] | None, int]:
    matches: list[dict[str, Any]] = []
    for day_dir in recent_day_dirs(days):
        for path_str in glob.glob(str(day_dir / "rollout-*.jsonl")):
            path = Path(path_str)
            info = inspect_session(path, tail)
            payload = ensure_dict(info.get("meta"))
            source = ensure_dict(payload.get("source"))
            subagent = ensure_dict(source.get("subagent"))
            spawn = ensure_dict(subagent.get("thread_spawn"))
            if spawn.get("parent_thread_id") != parent_thread_id:
                continue
            if payload.get("agent_path") != agent_path and spawn.get("agent_path") != agent_path:
                continue
            matches.append(info)
    return select_best_session(matches), len(matches)


def usage_summary(child_thread_id: str) -> list[str]:
    if not USAGE_DB.exists():
        return ["usage: database not found"]
    conn = sqlite3.connect(str(USAGE_DB))
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        """
        WITH turn_calls AS (
          SELECT
            turn_id,
            MIN(started_at) AS turn_started_at,
            COUNT(*) AS provider_calls,
            SUM(input_tokens_uncached) AS uncached_input,
            SUM(input_tokens_cached) AS cached_input,
            SUM(output_tokens) AS output_tokens,
            SUM(total_tokens) AS total_tokens,
            GROUP_CONCAT(DISTINCT actual_model_used) AS models
          FROM usage_provider_calls
          WHERE thread_id = ?
          GROUP BY turn_id
        )
        SELECT turn_id, turn_started_at, provider_calls, models,
               uncached_input, cached_input, output_tokens, total_tokens
        FROM turn_calls
        ORDER BY turn_started_at DESC
        LIMIT 3
        """,
        (child_thread_id,),
    ).fetchall()
    conn.close()
    if not rows:
        return ["usage: no provider-call rows yet"]
    out = []
    for row in rows:
        out.append(
            "usage: "
            f"turn={row['turn_id']} started={row['turn_started_at']} calls={row['provider_calls']} "
            f"model={row['models']} uncached={row['uncached_input']} cached={row['cached_input']} "
            f"output={row['output_tokens']} total={row['total_tokens']}"
        )
    return out


def read_meta(path: Path) -> dict:
    obj = first_json_line(path) or {}
    return obj.get("payload", {}) if obj.get("type") == "session_meta" else {}


def main() -> None:
    args = parse_args()
    session_info = None
    match_count = 0
    if args.child_thread_id:
        session_info, match_count = find_by_child_thread_id(args.child_thread_id, args.tail)
    if session_info is None:
        session_info, match_count = find_by_parent_and_agent(
            args.parent_thread_id,
            args.agent_path,
            args.days,
            args.tail,
        )
    if session_info is None:
        raise SystemExit("no matching session file found")

    session_file = session_info["path"]
    meta = ensure_dict(session_info.get("meta"))
    child_thread_id = args.child_thread_id or meta.get("id")
    print(f"session_file: {session_file}")
    if match_count > 1:
        print(f"matched_session_files: {match_count}")
    print(f"child_thread_id: {child_thread_id}")
    print(f"agent_path: {meta.get('agent_path')}")
    print(f"agent_role: {meta.get('agent_role')}")
    print(f"agent_nickname: {meta.get('agent_nickname')}")
    print(f"session_state: {session_info.get('session_state')}")
    current_turn = ensure_dict(session_info.get("last_task_started"))
    if current_turn.get("turn_id"):
        print(f"current_turn_id: {current_turn.get('turn_id')}")
    last_terminal = ensure_dict(session_info.get("last_terminal"))
    if last_terminal.get("event_type"):
        print(f"terminal_event: {last_terminal.get('event_type')}")
        print(f"terminal_at: {last_terminal.get('timestamp')}")
        if last_terminal.get("reason"):
            print(f"terminal_reason: {last_terminal.get('reason')}")
    if session_info.get("last_timestamp"):
        print(f"last_event_at: {session_info.get('last_timestamp')}")
    print("tail:")
    for row in session_info.get("tail_rows") or []:
        print(f"- {row}")
    if not args.no_usage and child_thread_id:
        print("usage:")
        for row in usage_summary(child_thread_id):
            print(f"- {row}")


if __name__ == "__main__":
    main()
