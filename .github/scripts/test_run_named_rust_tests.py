#!/usr/bin/env python3
"""Unit tests for the hosted rust-tests-v1 execution reconciliation."""

from __future__ import annotations

import importlib.util
import json
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

    def run_exact(self, test_outputs, *, names=None, request=None):
        names = names or ["suite::known"]
        request = {
            **REQUEST,
            "execution_mode": "exact",
            "tests": names,
            **(request or {}),
        }
        inventory = self.completed(
            stdout="".join(f"{name}: test\n" for name in names)
        )
        executions = [
            self.completed(stdout=output, code=code)
            for output, code in test_outputs
        ]
        with (
            mock.patch.object(
                MODULE.subprocess, "run", side_effect=[inventory, *executions]
            ) as run,
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(request, Path("/validation-target"))
        return result, run

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

    def test_command_builder_rejects_untrusted_values_at_the_sink(self) -> None:
        with self.assertRaises(ValueError):
            MODULE.cargo_args({**REQUEST, "package": "--workspace"}, list_only=True)
        with self.assertRaises(ValueError):
            MODULE.cargo_args(
                {**REQUEST, "target_kind": "integration", "target": "$(touch nope)"},
                list_only=True,
            )
        with self.assertRaises(ValueError):
            MODULE.cargo_args(REQUEST, list_only=False, test_name="suite::known; echo nope")

    def test_command_builder_returns_only_catalog_commands(self) -> None:
        self.assertEqual(
            MODULE.cargo_args(REQUEST, list_only=True),
            ["cargo", "test", "--locked", "-p", "codex-core", "--lib", "--", "--list"],
        )
        self.assertEqual(
            MODULE.cargo_args(REQUEST, list_only=False),
            [
                "cargo",
                "test",
                "--locked",
                "-p",
                "codex-core",
                "--lib",
                "--",
                "--test-threads=1",
            ],
        )

    def test_unknown_target_fails_before_cargo(self) -> None:
        request = {**REQUEST, "package": "not-in-catalog"}
        with (
            mock.patch.object(MODULE.subprocess, "run") as run,
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(request, Path("/validation-target"))
        self.assertEqual(result["failure_code"], "target_selector_unknown")
        run.assert_not_called()

    def test_default_request_keeps_whole_target_report_and_argv(self) -> None:
        result = self.run_request(
            "test suite::known ... ok\n\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; "
            "0 measured; 0 filtered out\n"
        )

        self.assertNotIn("execution_mode", result["request"])
        self.assertNotIn("argv", result["tests"][0])

    def test_exact_mode_runs_one_fixed_argv_per_selector(self) -> None:
        result, run = self.run_exact(
            [
                (
                    "test suite::known ... ok\n\n"
                    "test result: ok. 1 passed; 0 failed; 0 ignored; "
                    "0 measured; 4 filtered out\n",
                    0,
                ),
                (
                    "test suite::other ... ok\n\n"
                    "test result: ok. 1 passed; 0 failed; 0 ignored; "
                    "0 measured; 4 filtered out\n",
                    0,
                ),
            ],
            names=["suite::known", "suite::other"],
        )

        self.assertEqual(result["status"], "success")
        self.assertEqual(len(run.call_args_list), 3)
        for call, name, report in zip(
            run.call_args_list[1:], ["suite::known", "suite::other"], result["tests"]
        ):
            argv = call.args[0]
            separator = argv.index("--")
            self.assertEqual(argv[separator - 1], name)
            self.assertEqual(argv[separator + 1 :], ["--exact", "--test-threads=1"])
            self.assertFalse(call.kwargs["shell"])
            self.assertEqual(report["argv"], argv)
            self.assertEqual(report["requested_selector"], name)

    def test_exact_mode_rejects_option_like_name_even_when_in_inventory(self) -> None:
        request = {**REQUEST, "execution_mode": "exact", "tests": ["-list"]}
        inventory = self.completed(stdout="-list: test\n")
        with (
            mock.patch.object(MODULE.subprocess, "run", return_value=inventory) as run,
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(request, Path("/validation-target"))

        self.assertEqual(result["failure_code"], "exact_selector_invalid")
        self.assertEqual(run.call_count, 1)

    def test_load_request_rejects_unknown_execution_mode(self) -> None:
        request = {**REQUEST, "execution_mode": "shell"}
        with mock.patch.dict(
            MODULE.os.environ,
            {
                "RUST_TEST_REQUEST_JSON": json.dumps(request),
                "VALIDATION_PROFILE": "rust_minimal",
            },
        ):
            _, error = MODULE.load_request()

        self.assertEqual(error["failure_code"], "execution_mode_invalid")

    def test_exact_mode_rejects_unknown_package_and_target(self) -> None:
        for changed in (
            {"package": "not-in-catalog"},
            {"target_kind": "integration", "target": "not-in-catalog"},
        ):
            with self.subTest(changed=changed):
                request = {**REQUEST, "execution_mode": "exact", **changed}
                with (
                    mock.patch.object(MODULE.subprocess, "run") as run,
                    mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
                ):
                    result = MODULE.run_request(request, Path("/validation-target"))
                self.assertEqual(result["failure_code"], "target_selector_unknown")
                run.assert_not_called()

    def test_exact_mode_requires_unique_inventory_match(self) -> None:
        cases = (
            ("", "inventory_reconciliation_failed"),
            (
                "suite::known: test\nsuite::known: test\n",
                "inventory_reconciliation_failed",
            ),
        )
        for listed, failure in cases:
            with self.subTest(listed=listed):
                inventory = self.completed(stdout=listed)
                with (
                    mock.patch.object(MODULE.subprocess, "run", return_value=inventory) as run,
                    mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
                ):
                    result = MODULE.run_request(
                        {**REQUEST, "execution_mode": "exact"},
                        Path("/validation-target"),
                    )
                self.assertEqual(result["failure_code"], failure)
                self.assertEqual(run.call_count, 1)

    def test_exact_mode_rejects_ignored_missing_summary_and_nonzero(self) -> None:
        cases = (
            (
                "test suite::known ... ignored\n\n"
                "test result: ok. 0 passed; 0 failed; 1 ignored; "
                "0 measured; 0 filtered out\n",
                0,
                "named_test_ignored",
            ),
            ("test completed without a summary\n", 0, "execution_reconciliation_failed"),
            (
                "test suite::known ... FAILED\n\n"
                "test result: FAILED. 0 passed; 1 failed; 0 ignored; "
                "0 measured; 0 filtered out\n",
                0,
                "named_test_failed",
            ),
            (
                "test suite::known ... ok\n\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; "
                "0 measured; 0 filtered out\n",
                101,
                "named_test_failed",
            ),
        )
        for output, code, failure in cases:
            with self.subTest(failure=failure):
                result, _ = self.run_exact([(output, code)])
                self.assertEqual(result["status"], "failure")
                self.assertEqual(result["failure_code"], failure)


if __name__ == "__main__":
    main()
