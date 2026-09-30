#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "inspect_subagent_tail.py"
spec = importlib.util.spec_from_file_location("inspect_subagent_tail", SCRIPT)
assert spec and spec.loader
inspect_subagent_tail = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inspect_subagent_tail)


def metadata(thread_id: str, *, parent: str = "parent-id", agent_path: str = "/root/child") -> dict:
    return {
        "type": "session_meta",
        "payload": {
            "id": thread_id,
            "agent_path": agent_path,
            "source": {"subagent": {"thread_spawn": {
                "parent_thread_id": parent,
                "agent_path": agent_path,
            }}},
        },
    }


def event(kind: str, timestamp: str, **payload: object) -> dict:
    return {"type": "event_msg", "timestamp": timestamp, "payload": {"type": kind, **payload}}


def write_session(path: Path, meta: dict, records: list[dict | bytes]) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as f:
        f.write(json.dumps(meta).encode() + b"\n")
        for record in records:
            if isinstance(record, bytes):
                f.write(record)
            else:
                f.write(json.dumps(record).encode() + b"\n")
    return path


class TimestampParsingTest(unittest.TestCase):
    def test_parse_timestamp_ignores_non_strings(self) -> None:
        self.assertIsNone(inspect_subagent_tail.parse_timestamp(None))
        self.assertIsNone(inspect_subagent_tail.parse_timestamp(123))
        self.assertIsNone(inspect_subagent_tail.parse_timestamp({"timestamp": "2026-01-01T00:00:00Z"}))

    def test_time_since_subtracts_aware_datetimes_directly(self) -> None:
        now = datetime(2026, 1, 1, 0, 2, 3, tzinfo=timezone.utc)
        self.assertEqual(
            inspect_subagent_tail.time_since("2026-01-01T00:00:00Z", now),
            "2m 3s",
        )


class SessionRecordTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_multiline_records_keep_terminal_state_and_bounded_tail(self) -> None:
        path = write_session(self.root / "rollout-a.jsonl", metadata("child-a"), [
            event("task_started", "2026-09-30T00:00:00Z", turn_id="turn-1"),
            {"type": "response_item", "timestamp": "2026-09-30T00:00:01Z", "payload": {
                "type": "function_call", "name": "browser_step",
            }},
            event("task_complete", "2026-09-30T00:00:02Z", turn_id="turn-1"),
        ])
        info = inspect_subagent_tail.inspect_session(path, tail=2)
        self.assertEqual(info["session_state"], "completed")
        self.assertEqual(info["last_terminal"]["event_type"], "task_complete")
        self.assertEqual(len(info["tail_rows"]), 2)
        self.assertEqual(info["records_inspected"], 3)
        self.assertEqual(info["diagnostics"], [])

    def test_interrupted_is_distinct_from_completed(self) -> None:
        path = write_session(self.root / "rollout-b.jsonl", metadata("child-b"), [
            event("task_started", "2026-09-30T00:00:00Z", turn_id="turn-2"),
            event("turn_aborted", "2026-09-30T00:00:01Z", turn_id="turn-2", reason="operator"),
        ])
        info = inspect_subagent_tail.inspect_session(path, tail=8)
        self.assertEqual(info["session_state"], "interrupted")
        self.assertEqual(info["last_terminal"]["reason"], "operator")

    def test_later_start_wins_by_record_order_not_timestamp_lexical_order(self) -> None:
        path = write_session(self.root / "rollout-c.jsonl", metadata("child-c"), [
            event("task_complete", "2026-09-30T00:00:02Z", turn_id="old"),
            event("task_started", "2026-09-30T00:00:01Z", turn_id="new"),
        ])
        info = inspect_subagent_tail.inspect_session(path, tail=8)
        self.assertEqual(info["session_state"], "active")
        self.assertEqual(info["last_task_started"]["turn_id"], "new")

    def test_text_that_mentions_terminal_event_does_not_create_one(self) -> None:
        path = write_session(self.root / "rollout-d.jsonl", metadata("child-d"), [
            event("task_started", "2026-09-30T00:00:00Z", turn_id="turn-4"),
            {"type": "response_item", "timestamp": "2026-09-30T00:00:01Z", "payload": {
                "type": "message", "role": "assistant", "content": [
                    {"type": "text", "text": "the literal words task_complete are explanatory text"},
                ],
            }},
        ])
        info = inspect_subagent_tail.inspect_session(path, tail=8)
        self.assertEqual(info["session_state"], "active")
        self.assertIsNone(info["last_terminal"])

    def test_oversized_record_stops_at_cap_and_makes_status_unknown(self) -> None:
        path = write_session(self.root / "rollout-e.jsonl", metadata("child-e"), [
            b'{"type":"response_item","payload":{"type":"message","text":"' + b"x" * 4096 + b'"}}\n',
            event("task_complete", "2026-09-30T00:00:02Z", turn_id="hidden"),
        ])
        with patch.object(inspect_subagent_tail, "MAX_RECORD_BYTES", 512):
            info = inspect_subagent_tail.inspect_session(path, tail=8)
        self.assertEqual(info["session_state"], "unknown")
        self.assertEqual(info["diagnostics"], ["record_oversized"])
        self.assertLessEqual(
            info["transcript_bytes_inspected"],
            inspect_subagent_tail.MAX_RECORD_BYTES + 1 + info["candidate_metadata_bytes"],
        )
        self.assertIsNone(info["last_terminal"])

    def test_oversized_session_is_not_scanned_or_misreported_complete(self) -> None:
        path = write_session(self.root / "rollout-e2.jsonl", metadata("child-e2"), [
            event("task_complete", "2026-09-30T00:00:01Z", turn_id="visible"),
            {"type": "response_item", "payload": {"type": "opaque", "padding": "x" * 4096}},
        ])
        with patch.object(inspect_subagent_tail, "MAX_SESSION_BYTES", 512):
            info = inspect_subagent_tail.inspect_session(path, tail=8)
        self.assertEqual(info["session_state"], "unknown")
        self.assertEqual(info["diagnostics"], ["session_byte_limit_reached"])
        self.assertEqual(info["records_inspected"], 0)
        self.assertEqual(info["transcript_bytes_inspected"], 0)

    def test_truncated_and_malformed_records_are_explicit_unknowns(self) -> None:
        truncated = write_session(self.root / "rollout-f.jsonl", metadata("child-f"), [
            event("task_started", "2026-09-30T00:00:00Z", turn_id="turn-6"),
            b'{"type":"event_msg","payload":{"type":"task_complete"}}',
        ])
        malformed = write_session(self.root / "rollout-g.jsonl", metadata("child-g"), [
            event("task_started", "2026-09-30T00:00:00Z", turn_id="turn-7"),
            b"{ definitely not json }\n",
            event("task_complete", "2026-09-30T00:00:02Z", turn_id="turn-7"),
        ])
        self.assertIn("record_truncated", inspect_subagent_tail.inspect_session(truncated, 8)["diagnostics"])
        malformed_info = inspect_subagent_tail.inspect_session(malformed, 8)
        self.assertIn("record_malformed", malformed_info["diagnostics"])
        self.assertEqual(malformed_info["session_state"], "unknown")

    def test_parent_path_lookup_reads_metadata_before_transcript_records(self) -> None:
        day_dir = self.root / "2026" / "09" / "30"
        wrong = write_session(day_dir / "rollout-wrong.jsonl", metadata("wrong", agent_path="/root/other"), [
            b'{"type":"response_item","payload":{"text":"' + b"z" * 2048 + b'"}}\n',
        ])
        right = write_session(day_dir / "rollout-right.jsonl", metadata("right"), [
            event("task_started", "2026-09-30T00:00:00Z", turn_id="selected"),
        ])
        with patch.object(inspect_subagent_tail, "MAX_RECORD_BYTES", 512), \
             patch.object(inspect_subagent_tail, "SESSIONS_ROOT", self.root):
            selected, stats = inspect_subagent_tail.find_by_parent_and_agent(
                "parent-id", "/root/child", days=1, tail=8,
            )
        self.assertEqual(selected["path"], right)
        self.assertEqual(selected["session_state"], "active")
        self.assertEqual(stats["candidate_files_inspected"], 2)
        self.assertEqual(stats["directory_entries_inspected"], 3)
        self.assertLess(stats["candidate_metadata_bytes"], 2048)
        self.assertLess(stats["candidate_transcript_bytes_inspected"], 1024)
        self.assertLess(selected["transcript_bytes_inspected"], 1024)

    def test_day_directory_search_is_newest_first_and_honors_window(self) -> None:
        for day in ("28", "29", "30"):
            (self.root / "2026" / "09" / day).mkdir(parents=True)
        with patch.object(inspect_subagent_tail, "SESSIONS_ROOT", self.root):
            days, diagnostics, inspected = inspect_subagent_tail.recent_day_dirs(2)
        self.assertEqual([path.name for path in days], ["30", "29"])
        self.assertEqual(diagnostics, [])
        self.assertGreater(inspected, 0)

    def test_directory_enumeration_cap_is_explicit(self) -> None:
        for year in ("2024", "2025", "2026"):
            (self.root / year).mkdir()
        with patch.object(inspect_subagent_tail, "SESSIONS_ROOT", self.root), \
             patch.object(inspect_subagent_tail, "MAX_DIRECTORY_ENTRIES_PER_DIR", 2):
            days, diagnostics, inspected = inspect_subagent_tail.recent_day_dirs(1)
        self.assertEqual(days, [])
        self.assertIn("directory_entry_limit_reached", diagnostics)
        self.assertEqual(inspected, 2)

    def test_session_file_entry_cap_is_explicit(self) -> None:
        day_dir = self.root / "2026" / "09" / "30"
        day_dir.mkdir(parents=True)
        for name in ("noise-a.txt", "noise-b.txt", "noise-c.txt"):
            (day_dir / name).write_text("synthetic")
        with patch.object(inspect_subagent_tail, "SESSIONS_ROOT", self.root), \
             patch.object(inspect_subagent_tail, "MAX_SESSION_FILE_ENTRIES_PER_DIR", 2):
            selected, stats = inspect_subagent_tail.find_by_parent_and_agent(
                "parent-id", "/root/child", days=1, tail=8,
            )
        self.assertIsNone(selected)
        self.assertEqual(stats["lookup_state"], "partial")
        self.assertIn("session_file_entry_limit_reached", stats["diagnostics"])
        self.assertEqual(stats["session_file_entries_inspected"], 2)

    def test_multiple_exact_sessions_report_ambiguity_and_choose_newest(self) -> None:
        thread_id = "same-child"
        first = write_session(self.root / "2026" / "09" / "29" / f"rollout-a-{thread_id}.jsonl", metadata(thread_id), [
            event("task_complete", "2026-09-30T00:00:01Z", turn_id="a"),
        ])
        second = write_session(self.root / "2026" / "09" / "30" / f"rollout-b-{thread_id}.jsonl", metadata(thread_id), [
            event("task_started", "2026-09-30T00:00:02Z", turn_id="b"),
        ])
        with patch.object(inspect_subagent_tail, "SESSIONS_ROOT", self.root):
            selected, stats = inspect_subagent_tail.find_by_child_thread_id(thread_id, 8, days=2)
        self.assertEqual(selected["path"], second)
        self.assertEqual(stats["matched_session_files"], 2)
        self.assertEqual(stats["lookup_state"], "ambiguous")
        self.assertIn("multiple_matching_sessions", stats["diagnostics"])

    def test_aggregate_cap_with_duplicate_cannot_claim_selected_terminal_state(self) -> None:
        thread_id = "same-child-capped"
        first = write_session(
            self.root / "first.jsonl", metadata(thread_id),
            [event("task_complete", "2026-09-30T00:00:01Z", turn_id="visible")],
        )
        second = write_session(
            self.root / "second.jsonl", metadata(thread_id),
            [event("task_started", "2026-09-30T00:00:02Z", turn_id="uninspected")],
        )
        with patch.object(inspect_subagent_tail, "MAX_LOOKUP_TRANSCRIPT_BYTES", first.stat().st_size):
            selected, stats = inspect_subagent_tail._resolve_candidates(
                [first, second], tail=8, child_thread_id=thread_id,
            )

        self.assertEqual(selected["path"], first)
        self.assertEqual(selected["session_state"], "completed")
        self.assertEqual(stats["lookup_state"], "partial")
        self.assertIn("multiple_matching_sessions", stats["diagnostics"])
        self.assertIn("candidate_transcript_limit_reached", stats["diagnostics"])

        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            inspect_subagent_tail._render_result(
                selected, stats,
                datetime(2026, 9, 30, tzinfo=timezone.utc), False, True,
            )
        result = json.loads(output.getvalue())
        self.assertEqual(result["session_state"], "unknown")
        self.assertEqual(result["last_known_candidate_state"], "completed")
        self.assertEqual(result["lookup_state"], "partial")

    def test_json_output_has_versioned_status_and_work_counters(self) -> None:
        path = write_session(self.root / "rollout-h.jsonl", metadata("child-h"), [
            event("task_complete", "2026-09-30T00:00:01Z", turn_id="h"),
        ])
        info = inspect_subagent_tail.inspect_session(path, 8)
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            inspect_subagent_tail._render_result(
                info,
                {"lookup_state": "complete", "diagnostics": [], "matched_session_files": 1,
                 "candidate_files_inspected": 1, "candidate_metadata_bytes": 128},
                datetime(2026, 9, 30, tzinfo=timezone.utc), False, True,
            )
        result = json.loads(output.getvalue())
        self.assertEqual(result["schema_version"], 1)
        self.assertEqual(result["session_state"], "completed")
        self.assertEqual(result["records_inspected"], 1)
        self.assertIn("diagnostics", result)


if __name__ == "__main__":
    unittest.main()
