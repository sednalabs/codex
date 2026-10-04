"""Deterministic selector-evidence tests for the hosted Rust runner."""

from __future__ import annotations

import sys
from pathlib import Path
import unittest


sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_named_rust_tests as named_tests


class NamedTestOutcomeEvidenceTests(unittest.TestCase):
    def test_unique_pass_retains_exact_selector_line(self) -> None:
        output = "test browser::visual_flow ... ok\n"

        outcomes = named_tests.test_outcomes(output)
        lines = named_tests.test_outcome_lines(output)

        self.assertEqual(outcomes["browser::visual_flow"], ["ok"])
        self.assertEqual(
            named_tests.matched_test_evidence(lines["browser::visual_flow"]),
            {
                "matched_line_count": 1,
                "matched_lines": ["test browser::visual_flow ... ok"],
                "matched_lines_truncated": False,
            },
        )

    def test_duplicate_missing_ignored_and_failure_outcomes(self) -> None:
        output = "\n".join(
            [
                "test browser::duplicate ... ok",
                "test browser::duplicate ... FAILED",
                "test browser::ignored ... ignored",
                "test browser::failed ... FAILED",
            ]
        )

        outcomes = named_tests.test_outcomes(output)
        lines = named_tests.test_outcome_lines(output)

        self.assertEqual(outcomes["browser::duplicate"], ["ok", "FAILED"])
        self.assertEqual(
            named_tests.matched_test_evidence(lines["browser::duplicate"])[
                "matched_line_count"
            ],
            2,
        )
        self.assertEqual(outcomes["browser::ignored"], ["ignored"])
        self.assertEqual(outcomes["browser::failed"], ["FAILED"])
        self.assertNotIn("browser::missing", outcomes)

    def test_matched_lines_are_counted_and_bounded(self) -> None:
        long_line = "test browser::" + "x" * 600 + " ... ok"
        lines = [long_line, *("test browser::duplicate ... ok" for _ in range(5))]

        evidence = named_tests.matched_test_evidence(lines)

        self.assertEqual(evidence["matched_line_count"], 6)
        self.assertEqual(
            len(evidence["matched_lines"]), named_tests.MAX_MATCHED_TEST_LINES
        )
        self.assertTrue(evidence["matched_lines_truncated"])
        self.assertTrue(
            all(
                len(line) <= named_tests.MAX_MATCHED_TEST_LINE_CHARS
                for line in evidence["matched_lines"]
            )
        )


if __name__ == "__main__":
    unittest.main()
