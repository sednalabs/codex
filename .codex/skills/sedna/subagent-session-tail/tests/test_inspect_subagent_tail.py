#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
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


if __name__ == "__main__":
    unittest.main()
