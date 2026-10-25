#!/usr/bin/env python3
"""Unit tests for the hosted rust-tests-v1 execution reconciliation."""

from __future__ import annotations

import importlib.util
import subprocess
from pathlib import Path
from unittest import TestCase, main, mock


SCRIPT = Path(__file__).with_name("run_named_rust_tests.py")
SPEC = importlib.util.spec_from_file_location("run_named_rust_tests", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


REQUEST = {
    "schema_version": "rust-tests-v1",
    "profile": "rust_minimal",
    "package": "codex-core",
    "target_kind": "lib",
    "target": "",
    "tests": ["suite::known"],
}
MANIFEST = MODULE.load_manifest(Path(__file__).resolve().parents[2])


class NamedRustTests(TestCase):
    def completed(self, stdout: str = "", stderr: str = "", code: int = 0):
        return subprocess.CompletedProcess(
            args=["cargo", "test"],
            returncode=code,
            stdout=stdout,
            stderr=stderr,
        )

    def run_request(self, test_output: str, *, test_code: int = 0):
        inventory = self.completed(stdout="suite::known: test\n")
        execution = self.completed(stdout=test_output, code=test_code)
        with (
            mock.patch.object(MODULE.subprocess, "run", side_effect=[inventory, execution]),
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            return MODULE.run_request(REQUEST, Path("/validation-target"))

    def test_success_requires_one_non_ignored_pass(self) -> None:
        result = self.run_request(
            "test suite::known ... ok\n"
            "\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; "
            "0 measured; 0 filtered out\n"
        )

        self.assertEqual(result["status"], "success")
        self.assertTrue(result["tests"][0]["execution_reconciled"])
        self.assertEqual(result["tests"][0]["result_counts"]["passed"], 1)

    def test_ignored_test_cannot_report_success_from_exit_zero(self) -> None:
        result = self.run_request(
            "test suite::known ... ignored\n"
            "\n"
            "test result: ok. 0 passed; 0 failed; 1 ignored; "
            "0 measured; 0 filtered out\n"
        )

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "named_test_ignored")
        self.assertFalse(result["tests"][0]["execution_reconciled"])

    def test_missing_summary_fails_closed_with_bounded_diagnostics(self) -> None:
        result = self.run_request("cargo completed without a test summary\n")

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "execution_reconciliation_failed")
        self.assertIn("hosted job log", result["message"])

    def test_inventory_failure_preserves_actionable_bounded_diagnostics(self) -> None:
        inventory = self.completed(stderr="manifest could not be loaded", code=101)
        with (
            mock.patch.object(MODULE.subprocess, "run", return_value=inventory),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(REQUEST, Path("/validation-target"))

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "inventory_failed")
        self.assertIn("manifest could not be loaded", result["inventory"]["diagnostics"]["stderr_tail"])

    def test_diagnostic_tail_is_bounded(self) -> None:
        value = MODULE.bounded_diagnostic("x" * (MODULE.MAX_DIAGNOSTIC_CHARS + 100))

        self.assertLessEqual(len(value), MODULE.MAX_DIAGNOSTIC_CHARS + 60)
        self.assertIn("see hosted job log", value)

    def test_command_builder_rejects_unvalidated_values_at_the_sink(self) -> None:
        with self.assertRaises(ValueError):
            MODULE.cargo_args({**REQUEST, "package": "--workspace"}, list_only=True)
        with self.assertRaises(ValueError):
            MODULE.cargo_args(
                {**REQUEST, "target_kind": "integration", "target": "$(touch nope)"},
                list_only=True,
            )
        with self.assertRaises(ValueError):
            MODULE.cargo_args(REQUEST, list_only=False, test_name="suite::known; echo nope")
        with self.assertRaises(ValueError):
            MODULE.cargo_args(REQUEST, list_only=False)

    def test_command_builder_adds_one_exact_inventory_validated_filter(self) -> None:
        self.assertEqual(
            MODULE.cargo_args(REQUEST, list_only=True),
            ["cargo", "test", "--locked", "-p", "codex-core", "--lib", "--", "--list"],
        )
        self.assertEqual(
            MODULE.cargo_args(REQUEST, list_only=False, test_name="suite::known"),
            [
                "cargo",
                "test",
                "--locked",
                "-p",
                "codex-core",
                "--lib",
                "suite::known",
                "--",
                "--test-threads=1",
                "--exact",
            ],
        )

    def test_request_runs_each_requested_name_with_an_exact_filter(self) -> None:
        request = {**REQUEST, "tests": ["suite::known", "suite::other"]}
        inventory = self.completed(stdout="suite::known: test\nsuite::other: test\n")
        one_test = self.completed(
            stdout=(
                "test suite::known ... ok\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; "
                "0 measured; 1 filtered out\n"
            )
        )
        other_test = self.completed(
            stdout=(
                "test suite::other ... ok\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; "
                "0 measured; 1 filtered out\n"
            )
        )
        with (
            mock.patch.object(
                MODULE.subprocess,
                "run",
                side_effect=[inventory, one_test, other_test],
            ) as run,
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(request, Path("/validation-target"))

        self.assertEqual(result["status"], "success")
        self.assertEqual([test["name"] for test in result["tests"]], request["tests"])
        self.assertEqual(run.call_count, 3)
        self.assertEqual(
            run.call_args_list[1].args[0],
            [
                "cargo", "test", "--locked", "-p", "codex-core", "--lib",
                "suite::known", "--", "--test-threads=1", "--exact",
            ],
        )
        self.assertEqual(
            run.call_args_list[2].args[0],
            [
                "cargo", "test", "--locked", "-p", "codex-core", "--lib",
                "suite::other", "--", "--test-threads=1", "--exact",
            ],
        )

    def test_unfiltered_target_cannot_satisfy_exact_named_test(self) -> None:
        request = {**REQUEST, "tests": ["suite::known"]}
        inventory = self.completed(stdout="suite::known: test\nsuite::other: test\n")
        unfiltered = self.completed(
            stdout=(
                "test suite::known ... ok\n"
                "test suite::other ... ok\n"
                "test result: ok. 2 passed; 0 failed; 0 ignored; "
                "0 measured; 0 filtered out\n"
            )
        )
        with (
            mock.patch.object(MODULE.subprocess, "run", side_effect=[inventory, unfiltered]),
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(request, Path("/validation-target"))

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "execution_reconciliation_failed")
        self.assertEqual(result["tests"][0]["result_counts"]["filtered"], 0)
        self.assertEqual(result["tests"][0]["expected_filtered_count"], 1)

    def test_unknown_target_fails_before_cargo(self) -> None:
        request = {**REQUEST, "package": "not-in-catalog"}
        with (
            mock.patch.object(MODULE.subprocess, "run") as run,
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(request, Path("/validation-target"))
        self.assertEqual(result["failure_code"], "target_selector_unknown")
        run.assert_not_called()


    def test_nested_summary_uses_the_enclosing_target_counts(self) -> None:
        output = (
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 234 filtered out\n"
            "test result: ok. 235 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n"
        )
        self.assertEqual(MODULE.test_result_counts(output), {
            "passed": 235, "failed": 0, "ignored": 1, "measured": 0, "filtered": 0,
        })

    def test_nested_summary_retains_named_outcome_and_exit_guards(self) -> None:
        for code, outcome, expected in [(0, "ok", "success"), (1, "ok", "failure"), (0, "ignored", "failure")]:
            with self.subTest(code=code, outcome=outcome):
                output = (
                    "test child::fixture ... ok\n"
                    "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 234 filtered out\n"
                    f"test suite::known ... {outcome}\n"
                    "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
                )
                result = self.run_request(output, test_code=code)
                self.assertEqual(result["status"], expected)
                self.assertEqual(
                    result["tests"][0]["result_counts"],
                    {"passed": 1, "failed": 0, "ignored": 0, "measured": 0, "filtered": 0},
                )
                self.assertEqual(result["tests"][0]["observed_outcomes"], [outcome])


if __name__ == "__main__":
    main()
