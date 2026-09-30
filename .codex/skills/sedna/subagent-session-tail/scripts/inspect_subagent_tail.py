#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import sqlite3
from collections import deque
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

SESSIONS_ROOT = Path.home() / ".codex" / "sessions"
USAGE_DB = Path.home() / ".codex" / "usage_1.sqlite"

# Session metadata is small and is the only content needed to resolve a candidate.
# Matched transcript records are separately capped so one pathological JSONL line
# cannot force unbounded allocation or hide a later status transition.
MAX_METADATA_BYTES = 64 * 1024
MAX_RECORD_BYTES = 256 * 1024
MAX_SESSION_BYTES = 16 * 1024 * 1024
MAX_LOOKUP_TRANSCRIPT_BYTES = 16 * 1024 * 1024
MAX_SESSION_RECORDS = 50_000
MAX_SEARCH_DAYS = 30
MAX_CANDIDATE_FILES = 500
MAX_DIRECTORY_ENTRIES_PER_DIR = 500
MAX_DIRECTORY_ENTRIES_TOTAL = 10_000
MAX_SESSION_FILE_ENTRIES_PER_DIR = 5_000
MAX_SESSION_FILE_ENTRIES_TOTAL = 10_000
MAX_TAIL_ROWS = 100


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Inspect a subagent's recent session tail and usage heartbeat.")
    parser.add_argument("--child-thread-id")
    parser.add_argument("--parent-thread-id")
    parser.add_argument("--agent-path")
    parser.add_argument("--days", type=int, default=3)
    parser.add_argument("--tail", type=int, default=10)
    parser.add_argument("--no-usage", action="store_true")
    parser.add_argument("--json", action="store_true", help="emit the versioned machine-readable result")
    args = parser.parse_args()
    if not args.child_thread_id and not (args.parent_thread_id and args.agent_path):
        parser.error("provide --child-thread-id, or both --parent-thread-id and --agent-path")
    if args.agent_path and not args.parent_thread_id:
        parser.error("--agent-path requires --parent-thread-id")
    if not 1 <= args.tail <= MAX_TAIL_ROWS:
        parser.error(f"--tail must be between 1 and {MAX_TAIL_ROWS}")
    if not 1 <= args.days <= MAX_SEARCH_DAYS:
        parser.error(f"--days must be between 1 and {MAX_SEARCH_DAYS}")
    return args


def _bounded_child_dirs(parent: Path, remaining_entries: int) -> tuple[list[Path], int, list[str]]:
    limit = min(MAX_DIRECTORY_ENTRIES_PER_DIR, remaining_entries)
    children = []
    inspected = 0
    diagnostics = []
    if limit <= 0:
        return children, inspected, ["directory_entry_limit_reached"]
    try:
        with os.scandir(parent) as entries:
            for entry in entries:
                if inspected >= limit:
                    diagnostics.append("directory_entry_limit_reached")
                    break
                inspected += 1
                if entry.is_dir(follow_symlinks=False):
                    children.append(Path(entry.path))
    except OSError:
        diagnostics.append("session_directory_unreadable")
    children.sort(key=lambda path: path.name, reverse=True)
    return children, inspected, diagnostics


def recent_day_dirs(days: int) -> tuple[list[Path], list[str], int]:
    """Find recent YYYY/MM/DD directories without traversing the whole archive."""
    wanted = max(1, min(days, MAX_SEARCH_DAYS))
    found: list[Path] = []
    inspected_total = 0
    years, inspected, issues = _bounded_child_dirs(SESSIONS_ROOT, MAX_DIRECTORY_ENTRIES_TOTAL)
    inspected_total += inspected
    if issues:
        return [], issues, inspected_total

    for year in years:
        if len(year.name) != 4 or not year.name.isdigit():
            continue
        months, inspected, issues = _bounded_child_dirs(
            year, MAX_DIRECTORY_ENTRIES_TOTAL - inspected_total,
        )
        inspected_total += inspected
        if issues:
            return [], issues, inspected_total
        for month in months:
            if len(month.name) != 2 or not month.name.isdigit() or not 1 <= int(month.name) <= 12:
                continue
            day_dirs, inspected, issues = _bounded_child_dirs(
                month, MAX_DIRECTORY_ENTRIES_TOTAL - inspected_total,
            )
            inspected_total += inspected
            if issues:
                return [], issues, inspected_total
            for day in day_dirs:
                if len(day.name) != 2 or not day.name.isdigit() or not 1 <= int(day.name) <= 31:
                    continue
                found.append(day)
                if len(found) >= wanted:
                    return found, [], inspected_total
    return found, [], inspected_total


def ensure_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


def read_meta(path: Path) -> tuple[dict, str | None, int]:
    """Read only the bounded first JSONL record used for candidate identity."""
    try:
        with path.open("rb") as f:
            line = f.readline(MAX_METADATA_BYTES + 1)
    except OSError:
        return {}, "metadata_unreadable", 0
    meta, error = parse_meta_line(line)
    return meta, error, len(line)


def parse_meta_line(line: bytes) -> tuple[dict, str | None]:
    if len(line) > MAX_METADATA_BYTES:
        return {}, "metadata_oversized"
    if not line.endswith(b"\n"):
        return {}, "metadata_truncated"
    try:
        obj = json.loads(line)
    except (UnicodeDecodeError, json.JSONDecodeError, RecursionError):
        return {}, "metadata_malformed"
    if not isinstance(obj, dict) or obj.get("type") != "session_meta":
        return {}, "metadata_missing"
    return ensure_dict(obj.get("payload")), None


def shorten(text: str, limit: int = 140) -> str:
    clean = " ".join(text.split())
    return clean if len(clean) <= limit else clean[: limit - 3] + "..."


def parse_timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        normalized = value.removesuffix("Z") + "+00:00" if value.endswith("Z") else value
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=timezone.utc)
    return parsed


def format_duration_seconds(total_seconds: float) -> str:
    seconds = int(abs(total_seconds))
    days, seconds = divmod(seconds, 86400)
    hours, seconds = divmod(seconds, 3600)
    minutes, seconds = divmod(seconds, 60)

    parts: list[str] = []
    if days:
        parts.append(f"{days}d")
    if hours:
        parts.append(f"{hours}h")
    if minutes:
        parts.append(f"{minutes}m")
    if seconds or not parts:
        parts.append(f"{seconds}s")
    return " ".join(parts)


def time_since(timestamp: str | None, now: datetime) -> str | None:
    parsed = parse_timestamp(timestamp)
    if not parsed:
        return None
    if now.tzinfo is None:
        now = now.replace(tzinfo=timezone.utc)
    delta_seconds = (now - parsed).total_seconds()
    duration = format_duration_seconds(delta_seconds)
    if delta_seconds < 0:
        return f"-{duration} (last event is in the future)"
    return duration


def summarize_output(out: Any) -> str:
    if isinstance(out, str):
        first = out.splitlines()[0] if out else ""
        return shorten(first)
    if isinstance(out, list):
        if not out:
            return "[]"
        preview = out[0]
        if isinstance(preview, str):
            return f"[{shorten(preview)}]"
        return shorten(json.dumps(preview, ensure_ascii=False))
    if isinstance(out, dict):
        return shorten(json.dumps(out, ensure_ascii=False))
    return shorten(str(out))


def summarize_record(obj: dict) -> str | None:
    ts = obj.get("timestamp", "?")
    typ = obj.get("type")
    payload = ensure_dict(obj.get("payload"))
    if typ == "event_msg":
        event_type = payload.get("type")
        if event_type == "agent_message":
            message = payload.get("message")
            return f"{ts} event agent_message {shorten(message)}" if isinstance(message, str) else f"{ts} event agent_message"
        if event_type in {"task_started", "task_complete"}:
            return f"{ts} event {event_type}"
        if event_type == "turn_aborted":
            reason = payload.get("reason") or "-"
            return f"{ts} event turn_aborted reason={shorten(str(reason))}"
        if event_type == "token_count":
            info = ensure_dict(payload.get("info"))
            total = ensure_dict(info.get("total_token_usage")).get("total_tokens")
            model = payload.get("model_used")
            return f"{ts} event token_count model={model} total_tokens={total}"
        return None
    if typ != "response_item":
        return None
    item_type = payload.get("type")
    if item_type in {"function_call", "custom_tool_call"}:
        return f"{ts} call {payload.get('name')}"
    if item_type in {"function_call_output", "custom_tool_call_output"}:
        return f"{ts} output {summarize_output(payload.get('output', ''))}"
    if item_type == "message":
        role = payload.get("role")
        content = payload.get("content", [])
        if not isinstance(content, list):
            return None
        texts = [part["text"] for part in content if isinstance(part, dict) and isinstance(part.get("text"), str)]
        if texts:
            return f"{ts} message {role} {shorten(' '.join(texts))}"
    return None


def inspect_session(path: Path, tail: int, meta: dict | None = None,
                    byte_limit: int | None = None) -> dict[str, Any]:
    diagnostics = []
    meta_bytes = 0
    rows: deque[str] = deque(maxlen=max(1, min(tail, MAX_TAIL_ROWS)))
    last_timestamp = None
    last_task_started = None
    last_terminal = None
    start_sequence = -1
    terminal_sequence = -1
    records_inspected = 0
    transcript_bytes = 0
    byte_limit = MAX_SESSION_BYTES if byte_limit is None else max(0, min(MAX_SESSION_BYTES, byte_limit))
    try:
        if path.stat().st_size > byte_limit:
            diagnostics.append("session_byte_limit_reached")
            return {
                "path": path, "meta": ensure_dict(meta), "tail_rows": [],
                "last_timestamp": None, "last_task_started": None, "last_terminal": None,
                "session_state": "unknown", "diagnostics": diagnostics,
                "records_inspected": 0, "transcript_bytes_inspected": 0,
                "candidate_metadata_bytes": meta_bytes,
            }
    except OSError:
        diagnostics.append("session_unreadable")
        return {
            "path": path, "meta": ensure_dict(meta), "tail_rows": [],
            "last_timestamp": None, "last_task_started": None, "last_terminal": None,
            "session_state": "unknown", "diagnostics": diagnostics,
            "records_inspected": 0, "transcript_bytes_inspected": 0,
            "candidate_metadata_bytes": meta_bytes,
        }
    try:
        with path.open("rb") as f:
            header = f.readline(MAX_METADATA_BYTES + 1)
            transcript_bytes += len(header)
            header_meta, header_error = parse_meta_line(header)
            meta_bytes = len(header)
            if header_error:
                diagnostics.append(header_error)
            if meta is None:
                meta = header_meta
            elif header_error or header_meta != meta:
                diagnostics.append("metadata_changed")
            while True:
                if records_inspected >= MAX_SESSION_RECORDS:
                    if f.read(1):
                        diagnostics.append("session_record_limit_reached")
                    break
                remaining = byte_limit - transcript_bytes
                if remaining <= 0:
                    if f.read(1):
                        diagnostics.append("session_byte_limit_reached")
                    break
                line = f.readline(min(MAX_RECORD_BYTES + 1, remaining + 1))
                if not line:
                    break
                if len(line) > remaining:
                    transcript_bytes += len(line)
                    diagnostics.append("session_byte_limit_reached")
                    break
                transcript_bytes += len(line)
                if len(line) > MAX_RECORD_BYTES:
                    diagnostics.append("record_oversized")
                    break
                if not line.endswith(b"\n"):
                    diagnostics.append("record_truncated")
                    break
                records_inspected += 1
                try:
                    obj = json.loads(line)
                except (UnicodeDecodeError, json.JSONDecodeError, RecursionError):
                    diagnostics.append("record_malformed")
                    continue
                if not isinstance(obj, dict):
                    diagnostics.append("record_malformed")
                    continue
                sequence = records_inspected
                timestamp = obj.get("timestamp")
                if isinstance(timestamp, str) and timestamp:
                    last_timestamp = timestamp
                payload = ensure_dict(obj.get("payload"))
                if obj.get("type") == "event_msg":
                    event_type = payload.get("type")
                    if event_type == "task_started":
                        last_task_started = {"timestamp": timestamp, "turn_id": payload.get("turn_id")}
                        start_sequence = sequence
                    elif event_type == "task_complete":
                        last_terminal = {
                            "state": "completed", "event_type": event_type, "timestamp": timestamp,
                            "turn_id": payload.get("turn_id"), "reason": None,
                        }
                        terminal_sequence = sequence
                    elif event_type == "turn_aborted":
                        last_terminal = {
                            "state": "interrupted", "event_type": event_type, "timestamp": timestamp,
                            "turn_id": payload.get("turn_id"), "reason": payload.get("reason"),
                        }
                        terminal_sequence = sequence
                try:
                    row = summarize_record(obj)
                except (TypeError, ValueError, RecursionError):
                    diagnostics.append("record_summary_failed")
                    row = None
                if row:
                    rows.append(shorten(row, limit=200))
    except OSError:
        diagnostics.append("session_unreadable")

    if diagnostics:
        session_state = "unknown"
    elif start_sequence > terminal_sequence:
        session_state = "active"
    elif terminal_sequence >= 0:
        session_state = str(last_terminal.get("state") or "unknown")
    elif last_timestamp:
        session_state = "active"
    else:
        session_state = "unknown"

    return {
        "path": path,
        "meta": ensure_dict(meta),
        "tail_rows": list(rows),
        "last_timestamp": last_timestamp,
        "last_task_started": last_task_started,
        "last_terminal": last_terminal,
        "session_state": session_state,
        "diagnostics": sorted(set(diagnostics)),
        "records_inspected": records_inspected,
        "transcript_bytes_inspected": transcript_bytes,
        "candidate_metadata_bytes": meta_bytes,
    }


def session_sort_key(info: dict[str, Any]) -> tuple[str, int, str]:
    state = info.get("session_state")
    active_rank = 1 if state == "active" else 0
    return (str(info.get("last_timestamp") or ""), active_rank, str(info.get("path") or ""))


def _prefer_session(best: dict[str, Any] | None, candidate: dict[str, Any]) -> dict[str, Any]:
    if best is None or session_sort_key(candidate) > session_sort_key(best):
        return candidate
    return best


def select_best_session(infos: Iterable[dict[str, Any]]) -> dict[str, Any] | None:
    best = None
    for info in infos:
        best = _prefer_session(best, info)
    return best


def _meta_matches(meta: dict, *, child_thread_id: str | None = None,
                  parent_thread_id: str | None = None, agent_path: str | None = None) -> bool:
    if child_thread_id is not None:
        return meta.get("id") == child_thread_id
    source = ensure_dict(meta.get("source"))
    subagent = ensure_dict(source.get("subagent"))
    spawn = ensure_dict(subagent.get("thread_spawn"))
    return (
        spawn.get("parent_thread_id") == parent_thread_id
        and (meta.get("agent_path") == agent_path or spawn.get("agent_path") == agent_path)
    )


def _resolve_candidates(paths: Iterable[Path], *, tail: int,
                        child_thread_id: str | None = None,
                        parent_thread_id: str | None = None,
                        agent_path: str | None = None) -> tuple[dict[str, Any] | None, dict[str, Any]]:
    best = None
    matches = 0
    candidates_inspected = 0
    metadata_bytes = 0
    unreadable_metadata = 0
    candidate_limit_reached = False
    candidate_transcript_bytes = 0
    transcript_limit_reached = False

    for path in paths:
        if candidates_inspected >= MAX_CANDIDATE_FILES:
            candidate_limit_reached = True
            break
        candidates_inspected += 1
        meta, error, read_bytes = read_meta(path)
        metadata_bytes += read_bytes
        if error:
            unreadable_metadata += 1
            continue
        if not _meta_matches(
            meta, child_thread_id=child_thread_id,
            parent_thread_id=parent_thread_id, agent_path=agent_path,
        ):
            continue
        matches += 1
        remaining_transcript_budget = MAX_LOOKUP_TRANSCRIPT_BYTES - candidate_transcript_bytes
        info = inspect_session(path, tail, meta, byte_limit=remaining_transcript_budget)
        candidate_transcript_bytes += info.get("transcript_bytes_inspected", 0)
        if "session_byte_limit_reached" in info.get("diagnostics", []):
            transcript_limit_reached = True
        info["candidate_metadata_bytes"] = read_bytes
        best = _prefer_session(best, info)

    diagnostics = []
    lookup_state = "complete"
    if matches > 1:
        diagnostics.append("multiple_matching_sessions")
        lookup_state = "ambiguous"
    if unreadable_metadata:
        diagnostics.append("candidate_metadata_unreadable")
        if lookup_state == "complete":
            lookup_state = "partial"
    if candidate_limit_reached:
        diagnostics.append("candidate_limit_reached")
        lookup_state = "partial"
    if transcript_limit_reached:
        diagnostics.append("candidate_transcript_limit_reached")
        lookup_state = "partial"
    return best, {
        "matched_session_files": matches,
        "candidate_files_inspected": candidates_inspected,
        "candidate_metadata_bytes": metadata_bytes,
        "candidate_transcript_bytes_inspected": candidate_transcript_bytes,
        "unreadable_candidate_metadata": unreadable_metadata,
        "lookup_state": lookup_state,
        "diagnostics": diagnostics,
    }


def _bounded_session_files(day_dirs: Iterable[Path], child_thread_id: str | None = None
                           ) -> tuple[list[Path], list[str], int]:
    paths: list[Path] = []
    diagnostics: list[str] = []
    inspected_total = 0
    for day_dir in day_dirs:
        per_dir = 0
        try:
            with os.scandir(day_dir) as entries:
                for entry in entries:
                    if (per_dir >= MAX_SESSION_FILE_ENTRIES_PER_DIR
                            or inspected_total >= MAX_SESSION_FILE_ENTRIES_TOTAL):
                        diagnostics.append("session_file_entry_limit_reached")
                        return paths, diagnostics, inspected_total
                    per_dir += 1
                    inspected_total += 1
                    name = entry.name
                    if not name.startswith("rollout-") or not name.endswith(".jsonl"):
                        continue
                    if child_thread_id is not None and not name.endswith(f"{child_thread_id}.jsonl"):
                        continue
                    if entry.is_file(follow_symlinks=False):
                        paths.append(Path(entry.path))
                        if len(paths) > MAX_CANDIDATE_FILES:
                            diagnostics.append("candidate_limit_reached")
                            return paths[:-1], diagnostics, inspected_total
                    else:
                        diagnostics.append("session_candidate_not_regular_file")
        except OSError:
            diagnostics.append("session_directory_unreadable")
            return paths, diagnostics, inspected_total
    return paths, sorted(set(diagnostics)), inspected_total


def _with_directory_search(info: dict[str, Any] | None, lookup: dict[str, Any],
                           days: int, directory_diagnostics: list[str],
                           directory_entries_inspected: int,
                           file_entries_inspected: int = 0) -> tuple[dict[str, Any] | None, dict[str, Any]]:
    lookup["search_window_days"] = days
    lookup["directory_entries_inspected"] = directory_entries_inspected
    lookup["session_file_entries_inspected"] = file_entries_inspected
    if directory_diagnostics:
        lookup["diagnostics"] = sorted(set(lookup["diagnostics"] + directory_diagnostics))
        lookup["lookup_state"] = "partial"
    return info, lookup


def find_by_child_thread_id(child_thread_id: str, tail: int,
                             days: int = 3) -> tuple[dict[str, Any] | None, dict[str, Any]]:
    day_dirs, diagnostics, inspected = recent_day_dirs(days)
    paths, file_diagnostics, file_inspected = _bounded_session_files(day_dirs, child_thread_id)
    info, lookup = _resolve_candidates(paths, tail=tail, child_thread_id=child_thread_id)
    return _with_directory_search(
        info, lookup, days, diagnostics + file_diagnostics, inspected, file_inspected,
    )


def find_by_parent_and_agent(parent_thread_id: str, agent_path: str, days: int,
                             tail: int) -> tuple[dict[str, Any] | None, dict[str, Any]]:
    day_dirs, diagnostics, inspected = recent_day_dirs(days)
    paths, file_diagnostics, file_inspected = _bounded_session_files(day_dirs)
    info, lookup = _resolve_candidates(
        paths, tail=tail, parent_thread_id=parent_thread_id, agent_path=agent_path,
    )
    return _with_directory_search(
        info, lookup, days, diagnostics + file_diagnostics, inspected, file_inspected,
    )


def usage_summary(child_thread_id: str) -> list[str]:
    if not USAGE_DB.exists():
        return ["usage: database not found"]
    conn = sqlite3.connect(str(USAGE_DB))
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        """
        WITH turn_calls AS (
          SELECT turn_id, MIN(started_at) AS turn_started_at, COUNT(*) AS provider_calls,
                 SUM(input_tokens_uncached) AS uncached_input,
                 SUM(input_tokens_cached) AS cached_input, SUM(output_tokens) AS output_tokens,
                 SUM(total_tokens) AS total_tokens, GROUP_CONCAT(DISTINCT actual_model_used) AS models
          FROM usage_provider_calls WHERE thread_id = ? GROUP BY turn_id
        )
        SELECT turn_id, turn_started_at, provider_calls, models,
               uncached_input, cached_input, output_tokens, total_tokens
        FROM turn_calls ORDER BY turn_started_at DESC LIMIT 3
        """,
        (child_thread_id,),
    ).fetchall()
    conn.close()
    if not rows:
        return ["usage: no provider-call rows yet"]
    return [
        "usage: "
        f"turn={row['turn_id']} started={row['turn_started_at']} calls={row['provider_calls']} "
        f"model={row['models']} uncached={row['uncached_input']} cached={row['cached_input']} "
        f"output={row['output_tokens']} total={row['total_tokens']}"
        for row in rows
    ]


def _render_result(session_info: dict[str, Any] | None, lookup: dict[str, Any],
                   now: datetime, include_usage: bool, json_output: bool) -> None:
    if session_info is None:
        diagnostics = list(lookup.get("diagnostics", []))
        diagnostics.append(
            "no_matching_session_within_search_window"
            if lookup.get("search_window_days") is not None
            else "no_matching_session"
        )
        result = {
            "schema_version": 1,
            "session_state": "unknown",
            "lookup_state": lookup.get("lookup_state", "unknown"),
            "matched_session_files": lookup.get("matched_session_files", 0),
            "diagnostics": sorted(set(diagnostics)),
            "candidate_files_inspected": lookup.get("candidate_files_inspected", 0),
            "candidate_metadata_bytes": lookup.get("candidate_metadata_bytes", 0),
            "candidate_transcript_bytes_inspected": lookup.get("candidate_transcript_bytes_inspected", 0),
            "directory_entries_inspected": lookup.get("directory_entries_inspected", 0),
            "session_file_entries_inspected": lookup.get("session_file_entries_inspected", 0),
            "search_window_days": lookup.get("search_window_days"),
        }
        if json_output:
            print(json.dumps(result, sort_keys=True))
        else:
            for key, value in result.items():
                print(f"{key}: {value}")
        return

    meta = ensure_dict(session_info.get("meta"))
    terminal = ensure_dict(session_info.get("last_terminal"))
    current_turn = ensure_dict(session_info.get("last_task_started"))
    child_thread_id = meta.get("id")
    lookup_state = lookup.get("lookup_state", "unknown")
    candidate_state = session_info.get("session_state", "unknown")
    # A selected candidate is not enough to claim the aggregate lookup's state:
    # another exact match may be ambiguous or may not have been fully inspected.
    # Keep the selected status only as explicitly nonterminal evidence.
    session_state = candidate_state if lookup_state == "complete" else "unknown"
    result = {
        "schema_version": 1,
        "session_file": str(session_info.get("path")),
        "child_thread_id": child_thread_id,
        "agent_path": meta.get("agent_path"),
        "agent_role": meta.get("agent_role"),
        "agent_nickname": meta.get("agent_nickname"),
        "session_state": session_state,
        "last_known_candidate_state": candidate_state if lookup_state != "complete" else None,
        "terminal_event": terminal.get("event_type"),
        "terminal_at": terminal.get("timestamp"),
        "terminal_reason": terminal.get("reason"),
        "current_turn_id": current_turn.get("turn_id"),
        "last_event_at": session_info.get("last_timestamp"),
        "matched_session_files": lookup.get("matched_session_files", 0),
        "lookup_state": lookup_state,
        "diagnostics": sorted(set(lookup.get("diagnostics", []) + session_info.get("diagnostics", []))),
        "candidate_files_inspected": lookup.get("candidate_files_inspected", 0),
        "candidate_metadata_bytes": lookup.get("candidate_metadata_bytes", 0),
        "candidate_transcript_bytes_inspected": lookup.get("candidate_transcript_bytes_inspected", 0),
        "directory_entries_inspected": lookup.get("directory_entries_inspected", 0),
        "session_file_entries_inspected": lookup.get("session_file_entries_inspected", 0),
        "search_window_days": lookup.get("search_window_days"),
        "records_inspected": session_info.get("records_inspected", 0),
        "transcript_bytes_inspected": session_info.get("transcript_bytes_inspected", 0),
        "tail": session_info.get("tail_rows", []),
        "system_time": now.isoformat(timespec="seconds"),
    }
    if json_output:
        if include_usage and child_thread_id:
            result["usage"] = usage_summary(child_thread_id)
        print(json.dumps(result, sort_keys=True))
        return

    # Preserve the established text keys and tail layout for existing callers.
    print(f"session_file: {result['session_file']}")
    if lookup.get("matched_session_files", 0) > 1:
        print(f"matched_session_files: {lookup['matched_session_files']}")
    print(f"child_thread_id: {child_thread_id}")
    print(f"agent_path: {meta.get('agent_path')}")
    print(f"agent_role: {meta.get('agent_role')}")
    print(f"agent_nickname: {meta.get('agent_nickname')}")
    print(f"session_state: {result['session_state']}")
    print(f"system_time: {result['system_time']}")
    if current_turn.get("turn_id"):
        print(f"current_turn_id: {current_turn.get('turn_id')}")
    if terminal.get("event_type"):
        print(f"terminal_event: {terminal.get('event_type')}")
        print(f"terminal_at: {terminal.get('timestamp')}")
        if terminal.get("reason"):
            print(f"terminal_reason: {terminal.get('reason')}")
    if session_info.get("last_timestamp"):
        print(f"last_event_at: {session_info.get('last_timestamp')}")
        age = time_since(session_info.get("last_timestamp"), now)
        if age:
            print(f"time_since_last_event: {age}")
    print(f"lookup_state: {result['lookup_state']}")
    print(f"candidate_files_inspected: {result['candidate_files_inspected']}")
    print(f"candidate_metadata_bytes: {result['candidate_metadata_bytes']}")
    print(f"candidate_transcript_bytes_inspected: {result['candidate_transcript_bytes_inspected']}")
    print(f"directory_entries_inspected: {result['directory_entries_inspected']}")
    print(f"session_file_entries_inspected: {result['session_file_entries_inspected']}")
    if result["search_window_days"] is not None:
        print(f"search_window_days: {result['search_window_days']}")
    print(f"records_inspected: {result['records_inspected']}")
    print(f"transcript_bytes_inspected: {result['transcript_bytes_inspected']}")
    if result["diagnostics"]:
        print(f"diagnostics: {','.join(result['diagnostics'])}")
    print("tail:")
    for row in result["tail"]:
        print(f"- {row}")
    if include_usage and child_thread_id:
        print("usage:")
        for row in usage_summary(child_thread_id):
            print(f"- {row}")


def main() -> None:
    args = parse_args()
    now = datetime.now().astimezone()
    session_info = None
    lookup: dict[str, Any] = {"lookup_state": "unknown", "diagnostics": []}
    if args.child_thread_id:
        session_info, lookup = find_by_child_thread_id(args.child_thread_id, args.tail, args.days)
    elif args.parent_thread_id and args.agent_path:
        session_info, lookup = find_by_parent_and_agent(
            args.parent_thread_id, args.agent_path, args.days, args.tail,
        )
    _render_result(session_info, lookup, now, not args.no_usage, args.json)
    if session_info is None:
        raise SystemExit(2)


if __name__ == "__main__":
    main()
