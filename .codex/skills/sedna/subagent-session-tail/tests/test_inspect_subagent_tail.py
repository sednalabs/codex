#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import sqlite3
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "inspect_subagent_tail.py"
spec = importlib.util.spec_from_file_location("inspect_subagent_tail", SCRIPT)
assert spec and spec.loader
inspect_subagent_tail = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inspect_subagent_tail)


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


class OutputSummarizationTest(unittest.TestCase):
    def test_summarizes_outputs_without_exposing_payload_text(self) -> None:
        cases = [
            ("private token\nsecond line", "text output (2 lines)"),
            (
                [{"type": "text", "text": "private token"}, {"type": "image", "data": "opaque"}],
                "list output (2 items: text, image)",
            ),
            (["private token"], "list output (1 items: str)"),
            (
                ["a", "b", "c", "d", "e", "private token"],
                "list output (6 items: str, str, str, str, str, ...)",
            ),
            ({"secret": "private token"}, "object output (1 fields)"),
            (7, "int output"),
            (None, "empty output"),
        ]

        for output, expected in cases:
            with self.subTest(output=output):
                summary = inspect_subagent_tail.summarize_output(output)
                self.assertEqual(summary, expected)
                self.assertNotIn("private token", summary)


class UsageDatabaseSelectionTest(unittest.TestCase):
    def test_explicit_database_overrides_environment(self) -> None:
        expected = Path(tempfile.gettempdir()) / "selected" / "usage_1.sqlite"

        selected = inspect_subagent_tail.resolve_usage_database(
            expected,
            environ={
                "CODEX_SQLITE_HOME": str(
                    Path(tempfile.gettempdir()) / "environment-state"
                ),
                "CODEX_HOME": str(Path(tempfile.gettempdir()) / "codex-home"),
            },
            home=Path(tempfile.gettempdir()) / "user-home",
        )

        self.assertEqual(selected, expected)

    def test_database_argument_is_available_from_cli(self) -> None:
        database = Path(tempfile.gettempdir()) / "effective" / "usage_1.sqlite"

        args = inspect_subagent_tail.parse_args(
            ["--child-thread-id", "child-thread", "--database", str(database)]
        )

        self.assertEqual(args.database, str(database))

    def test_sqlite_home_environment_precedes_codex_home(self) -> None:
        temp_home = Path(tempfile.gettempdir())
        selected = inspect_subagent_tail.resolve_usage_database(
            environ={
                "CODEX_SQLITE_HOME": str(temp_home / "codex-dev-state"),
                "CODEX_HOME": str(temp_home / "codex-home"),
            },
            home=temp_home / "user-home",
        )

        self.assertEqual(
            selected,
            temp_home / "codex-dev-state" / "usage_1.sqlite",
        )

    def test_codex_home_is_used_when_sqlite_home_is_unset(self) -> None:
        temp_home = Path(tempfile.gettempdir())
        codex_home = temp_home / "codex-home"
        selected = inspect_subagent_tail.resolve_usage_database(
            environ={"CODEX_HOME": str(codex_home)},
            home=temp_home / "user-home",
        )

        self.assertEqual(selected, codex_home / "usage_1.sqlite")

    def test_user_home_is_only_the_last_fallback(self) -> None:
        home = Path(tempfile.gettempdir()) / "user-home"
        selected = inspect_subagent_tail.resolve_usage_database(
            environ={},
            home=home,
        )

        self.assertEqual(selected, home / ".codex" / "usage_1.sqlite")

    def test_usage_summary_reads_the_selected_custom_database(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_home = Path(temp_dir)
            selected_home = temp_home / "selected-state"
            selected_home.mkdir()
            database = selected_home / "usage_1.sqlite"
            with sqlite3.connect(database) as connection:
                connection.execute(
                    """
                    CREATE TABLE usage_provider_calls (
                        thread_id TEXT,
                        turn_id TEXT,
                        started_at TEXT,
                        actual_model_used TEXT,
                        input_tokens_uncached INTEGER,
                        input_tokens_cached INTEGER,
                        output_tokens INTEGER,
                        total_tokens INTEGER
                    )
                    """
                )
                connection.execute(
                    """
                    INSERT INTO usage_provider_calls (
                        thread_id, turn_id, started_at, actual_model_used,
                        input_tokens_uncached, input_tokens_cached,
                        output_tokens, total_tokens
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    (
                        "child-thread",
                        "turn-1",
                        "2026-10-30T03:00:00Z",
                        "gpt-5.6-luna",
                        7,
                        2,
                        3,
                        12,
                    ),
                )
            connection.close()

            rows = inspect_subagent_tail.usage_summary(
                "child-thread",
                database,
                environ={"CODEX_SQLITE_HOME": str(temp_home / "other-state")},
                home=temp_home / "user-home",
            )

        self.assertEqual(len(rows), 1)
        self.assertIn("model=gpt-5.6-luna", rows[0])
        self.assertIn("total=12", rows[0])


if __name__ == "__main__":
    unittest.main()
