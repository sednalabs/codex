"""Deterministic selector-evidence tests for the hosted Rust runner."""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch


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


class NamedFailureObserverTests(unittest.TestCase):
    def _evidence(
        self,
        stdout: str,
        stderr: str = "",
        *,
        requested: tuple[str, ...] = (),
        known: set[str] | None = None,
        summary: dict[str, int] | None = None,
    ) -> dict[str, object]:
        known_names = known or set()
        output = "\n".join(value for value in (stdout, stderr) if value)
        return named_tests.failure_evidence(
            {"tests": list(requested)},
            known_names,
            stdout,
            stderr,
            summary,
            output,
        )

    def test_failed_name_counts_keep_safe_459_inventory_names(self) -> None:
        names = [f"suite::test_{index:03}" for index in range(459)]
        output = "\n".join(
            [*(f"test {name} ... FAILED" for name in names), f"test {names[0]} ... FAILED"]
        )

        evidence = self._evidence(output, known=set(names))

        self.assertEqual(evidence["failed_name_distinct_count"], 459)
        self.assertEqual(evidence["failed_name_occurrence_count"], 460)
        self.assertEqual(evidence["safe_failed_name_count"], 459)
        self.assertEqual(evidence["failed_names"], names)
        self.assertEqual(evidence["failed_names_omitted_count"], 0)

        overflow_names = [f"suite::overflow_{index:04}" for index in range(1100)]
        overflow = self._evidence(
            "\n".join(f"test {name} ... FAILED" for name in overflow_names),
            known=set(overflow_names),
        )
        self.assertEqual(len(overflow["failed_names"]), named_tests.MAX_FAILURE_NAMES)
        self.assertEqual(overflow["failed_names_omitted_count"], 76)
        self.assertTrue(overflow["failed_names_truncated"])

    def test_structured_data_byte_bound_trims_only_with_truthful_counts(self) -> None:
        names = [f"suite::{index:04}_{'x' * 238}" for index in range(1024)]
        output = "\n".join(f"test {name} ... FAILED" for name in names)
        with patch.object(named_tests, "MAX_FAILURE_EVIDENCE_BYTES", 30000):
            evidence = self._evidence(output, known=set(names))

        self.assertEqual(evidence["failed_name_distinct_count"], 1024)
        self.assertEqual(evidence["failed_name_occurrence_count"], 1024)
        self.assertTrue(evidence["failed_names_truncated"])
        self.assertGreater(evidence["failed_names_omitted_count"], 0)
        self.assertLessEqual(evidence["structured_evidence_bytes"], 30000)
        self.assertEqual(
            evidence["structured_evidence_bytes"],
            len(json.dumps(evidence, sort_keys=True).encode("utf-8")),
        )
        diagnostic = named_tests.bounded_diagnostic(
            "x" * (named_tests.MAX_DIAGNOSTIC_CHARS + 20)
        )
        self.assertNotIn("hosted job log", diagnostic)
        self.assertIn("only the bounded captured tail is retained", diagnostic)
        self.assertTrue(
            diagnostic.endswith("x" * named_tests.MAX_DIAGNOSTIC_CHARS)
        )

    def test_requested_failed_selector_after_eight_blocks_is_prioritized(self) -> None:
        names = [f"suite::test_{index}" for index in range(9)]
        output = "\n".join(
            [
                *(f"test {name} ... FAILED" for name in names),
                *(f"---- {name} stdout ----\nassertion failed" for name in names),
                "failures:",
            ]
        )

        evidence = self._evidence(output, requested=(names[-1],), known=set(names))

        self.assertEqual(evidence["emitted_block_count"], 8)
        self.assertEqual(evidence["omitted_block_count"], 1)
        self.assertEqual(evidence["blocks"][0]["name"], names[-1])
        self.assertTrue(evidence["blocks"][0]["requested_failed_selector"])
        self.assertNotIn(names[-1], evidence["requested_failed_selectors_without_blocks"])

    def test_stream_provenance_is_deterministic_and_not_claimed_chronology(self) -> None:
        name = "suite::same"
        stdout = f"---- {name} stdout ----\npermission denied\n"
        stderr = f"---- {name} stderr ----\nconnection refused\n"

        evidence = self._evidence(stdout, stderr, known={name})

        self.assertEqual(
            [
                (block["source_stream"], block["stream_ordinal"])
                for block in evidence["blocks"]
            ],
            [("stdout", 1), ("stderr", 1)],
        )
        self.assertEqual(evidence["streams"]["stdout"]["source_char_count"], len(stdout))
        self.assertEqual(evidence["streams"]["stderr"]["source_char_count"], len(stderr))
        self.assertEqual(evidence["source_status"], "parsed")
        self.assertFalse(evidence["capture_truncated"])
        self.assertEqual(evidence["upstream_output_truncation"], "unknown")

    def test_header_status_and_sensitive_body_handling(self) -> None:
        missing = self._evidence("ordinary output with no block")
        invalid = "---- private/path?token=secret stdout ----\nsecret body\n"
        unrecognized = self._evidence(invalid)
        unknown = "private token=sample /home/runner/test"
        unknown_name = self._evidence(f"test {unknown} ... FAILED", known={"suite::known"})

        self.assertEqual(missing["source_status"], "none")
        self.assertEqual(unrecognized["source_status"], "unrecognized")
        self.assertEqual(unrecognized["unrecognized_block_count"], 1)
        self.assertEqual(unrecognized["blocks"], [])
        self.assertNotIn("private", json.dumps(unrecognized))
        self.assertNotIn("secret", json.dumps(unrecognized))
        self.assertEqual(unknown_name["failed_name_distinct_count"], 1)
        self.assertEqual(unknown_name["failed_name_occurrence_count"], 1)
        self.assertEqual(unknown_name["unsafe_or_unknown_failed_name_count"], 1)
        self.assertEqual(unknown_name["failed_names"], [])
        rendered = json.dumps(unknown_name)
        self.assertNotIn("private", rendered)
        self.assertNotIn("sample", rendered)
        self.assertNotIn("/home/runner", rendered)

    def test_markers_and_numeric_codes_are_allowlisted_only(self) -> None:
        name = "suite::safe"
        private = "https://private.example/path?token=secret /home/runner/alice alice@example.com"
        output = (
            f"---- {name} stdout ----\n"
            f"permission denied {private}\n"
            f"Os {{ code: 13, kind: PermissionDenied }} HTTP status 503 {private}\n"
            f"exit code: 7 {private}\n"
        )

        evidence = self._evidence(output, known={name})
        rendered = json.dumps(evidence)
        block = evidence["blocks"][0]

        self.assertIn("permission-denied", block["markers"])
        self.assertIn("http-error", block["markers"])
        self.assertIn(
            {"kind": "os-error-code", "value": 13}, block["numeric_captures"]
        )
        self.assertIn(
            {"kind": "http-status-code", "value": 503}, block["numeric_captures"]
        )
        self.assertIn(
            {"kind": "process-exit-code", "value": 7}, block["numeric_captures"]
        )
        for secret in ("private.example", "token=", "/home/runner", "alice@example.com"):
            self.assertNotIn(secret, rendered)
        self.assertLessEqual(
            block["evidence_char_count"],
            named_tests.MAX_FAILURE_BLOCK_EVIDENCE_CHARS,
        )
        self.assertLessEqual(
            evidence["structured_evidence_bytes"],
            named_tests.MAX_FAILURE_EVIDENCE_BYTES,
        )

    def test_selected_ok_does_not_pass_a_red_full_target(self) -> None:
        selected = "suite::selected_ok"
        inventory = subprocess.CompletedProcess(
            ["cargo", "test", "--list"], 0, stdout=f"{selected}: test\n", stderr=""
        )
        execution = subprocess.CompletedProcess(
            ["cargo", "test"],
            1,
            stdout=(
                f"test {selected} ... ok\n"
                "test suite::other ... FAILED\n"
                "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n"
            ),
            stderr="",
        )
        request = {
            "schema_version": named_tests.SCHEMA_VERSION,
            "profile": "rust_minimal",
            "package": "codex-test",
            "target_kind": "lib",
            "target": "",
            "tests": [selected],
        }
        record = {
            "inventory_argv": ("cargo", "test", "--list"),
            "execution_argv": ("cargo", "test"),
        }
        with (
            patch.object(named_tests, "load_manifest", return_value={}),
            patch.object(named_tests, "select_target", return_value=record),
            patch.object(named_tests, "git_sha", return_value="a" * 40),
            patch.object(named_tests.subprocess, "run", side_effect=[inventory, execution]),
        ):
            result = named_tests.run_request(request, Path("."))

        self.assertEqual(result["tests"][0]["observed_outcomes"], ["ok"])
        self.assertEqual(result["tests"][0]["status"], "failure")
        self.assertEqual(result["failure_code"], "named_test_failed")
        self.assertEqual(result["failure_evidence"]["cargo_summary_failed_count"], 1)
        self.assertEqual(result["failure_evidence"]["failed_name_distinct_count"], 1)


if __name__ == "__main__":
    unittest.main()
