#!/usr/bin/env python3
"""Unit tests for the hosted rust-tests-v1 execution reconciliation."""

from __future__ import annotations

import importlib.util
import io
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

    def test_ansi_csi_controls_are_normalized_for_parsing_not_raw_evidence(
        self,
    ) -> None:
        output = (
            "\x1b[32mtest suite::known ... \x1b[0 qok\x1b[0m\n\n"
            "\x1b[32mtest result: ok. 1 passed; 0 failed; 0 ignored; "
            "0 measured; 0 filtered out\x1b[0m\n"
        )
        result, _ = self.run_exact([(output, 0)])

        self.assertEqual(result["status"], "success")
        self.assertTrue(result["tests"][0]["execution_reconciled"])
        self.assertIn("\x1b[0 q", result["tests"][0]["diagnostics"]["stdout_tail"])

    def test_ansi_normalization_preserves_exact_name_and_one_test_gates(self) -> None:
        cases = (
            (
                "test suite::other ... \x1b[0 qok\n\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; "
                "0 measured; 0 filtered out\n",
                0,
                "execution_reconciliation_failed",
            ),
            (
                "test suite::known ... \x1b[0 qok\n"
                "test suite::other ... ok\n\n"
                "test result: ok. 2 passed; 0 failed; 0 ignored; "
                "0 measured; 0 filtered out\n",
                0,
                "execution_reconciliation_failed",
            ),
            (
                "test suite::known ... \x1b[0 qFAILED\n\n"
                "test result: FAILED. 0 passed; 1 failed; 0 ignored; "
                "0 measured; 0 filtered out\n",
                101,
                "named_test_failed",
            ),
            (
                "\x1b]0;title\x07test suite::known ... ok\n\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; "
                "0 measured; 0 filtered out\n",
                0,
                "execution_reconciliation_failed",
            ),
        )
        for output, code, failure_code in cases:
            with self.subTest(failure_code=failure_code, output=output):
                result, _ = self.run_exact([(output, code)])
                self.assertEqual(result["status"], "failure")
                self.assertEqual(result["failure_code"], failure_code)

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

    def test_inventory_failure_emits_full_stderr_while_artifact_stays_bounded(self) -> None:
        early_diagnostic = "early compiler diagnostic outside the retained tail"
        stderr = f"{early_diagnostic}\n" + "x" * (MODULE.MAX_DIAGNOSTIC_CHARS + 100)
        inventory = self.completed(stderr=stderr, code=101)
        step_stderr = io.StringIO()
        with (
            mock.patch.object(MODULE.subprocess, "run", return_value=inventory) as run,
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
            mock.patch.object(MODULE.sys, "stderr", step_stderr),
        ):
            result = MODULE.run_request(REQUEST, Path("/validation-target"))

        artifact_stderr = result["inventory"]["diagnostics"]["stderr_tail"]
        self.assertEqual(result["failure_code"], "inventory_failed")
        self.assertEqual(step_stderr.getvalue(), stderr)
        self.assertNotIn(early_diagnostic, artifact_stderr)
        self.assertIn("see hosted job log", artifact_stderr)
        self.assertLessEqual(
            len(artifact_stderr), MODULE.MAX_DIAGNOSTIC_CHARS + 60
        )
        run.assert_called_once()

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
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
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

    def test_load_request_accepts_exact_continue_mode(self) -> None:
        request = {**REQUEST, "execution_mode": "exact_continue"}
        with mock.patch.dict(
            MODULE.os.environ,
            {
                "RUST_TEST_REQUEST_JSON": json.dumps(request),
                "VALIDATION_PROFILE": "rust_minimal",
            },
        ):
            normalized, error = MODULE.load_request()

        self.assertIsNone(error)
        self.assertEqual(normalized["execution_mode"], "exact_continue")

    def test_exact_mode_rejects_unknown_package_and_target(self) -> None:
        for changed in (
            {"package": "not-in-catalog"},
            {"target_kind": "integration", "target": "not-in-catalog"},
        ):
            with self.subTest(changed=changed):
                for mode in ("exact", "exact_continue"):
                    with self.subTest(mode=mode):
                        request = {**REQUEST, "execution_mode": mode, **changed}
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
            for mode in ("exact", "exact_continue"):
                with self.subTest(listed=listed, mode=mode):
                    inventory = self.completed(stdout=listed)
                    with (
                        mock.patch.object(MODULE.subprocess, "run", return_value=inventory) as run,
                        mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
                        mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
                    ):
                        result = MODULE.run_request(
                            {**REQUEST, "execution_mode": mode},
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

    def test_exact_continue_collects_all_results_and_keeps_failed_aggregate(
        self,
    ) -> None:
        failed = (
            "test suite::first ... FAILED\n\n"
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; "
            "0 measured; 4 filtered out\n",
            101,
        )
        passed = (
            "test suite::second ... ok\n\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; "
            "0 measured; 4 filtered out\n",
            0,
        )
        failed_again = (
            "test suite::third ... FAILED\n\n"
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; "
            "0 measured; 4 filtered out\n",
            101,
        )
        result, run = self.run_exact(
            [failed, passed, failed_again],
            names=["suite::first", "suite::second", "suite::third"],
            request={"execution_mode": "exact_continue"},
        )

        self.assertEqual(len(run.call_args_list), 4)
        self.assertEqual(
            [test["status"] for test in result["tests"]],
            ["failure", "success", "failure"],
        )
        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "named_test_failed")
        self.assertEqual(
            [test["requested_selector"] for test in result["tests"]],
            ["suite::first", "suite::second", "suite::third"],
        )

    def test_exact_mode_keeps_stop_on_first_failure_behavior(self) -> None:
        failed = (
            "test suite::first ... FAILED\n\n"
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; "
            "0 measured; 4 filtered out\n",
            101,
        )
        result, run = self.run_exact(
            [failed],
            names=["suite::first", "suite::second"],
            request={"execution_mode": "exact"},
        )

        self.assertEqual(result["status"], "failure")
        self.assertEqual(len(run.call_args_list), 2)
        self.assertEqual([test["name"] for test in result["tests"]], ["suite::first"])

    def test_exact_continue_succeeds_only_when_every_test_passes(self) -> None:
        passed_known = (
            "test suite::known ... ok\n\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; "
            "0 measured; 4 filtered out\n",
            0,
        )
        passed_other = (
            "test suite::other ... ok\n\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; "
            "0 measured; 4 filtered out\n",
            0,
        )
        result, run = self.run_exact(
            [passed_known, passed_other],
            names=["suite::known", "suite::other"],
            request={"execution_mode": "exact_continue"},
        )

        self.assertEqual(result["status"], "success")
        self.assertEqual(
            [test["status"] for test in result["tests"]], ["success", "success"]
        )
        self.assertEqual(len(run.call_args_list), 3)

    def test_exact_continue_stops_on_unreconciled_execution_and_marks_remainder(
        self,
    ) -> None:
        cases = (
            (
                "compiler failed before test summary\n",
                101,
                "execution_reconciliation_failed",
            ),
            (
                "test suite::known ... FAILED\n\n"
                "test result: FAILED. 0 passed; 1 failed; 0 ignored; "
                "0 measured; 4 filtered out\n",
                0,
                "execution_reconciliation_failed",
            ),
            (
                "test suite::known ... ignored\n\n"
                "test result: ok. 0 passed; 0 failed; 1 ignored; "
                "0 measured; 4 filtered out\n",
                0,
                "named_test_ignored",
            ),
        )
        for output, code, failure_code in cases:
            with self.subTest(code=code, output=output):
                result, run = self.run_exact(
                    [(output, code)],
                    names=["suite::known", "suite::other", "suite::last"],
                    request={"execution_mode": "exact_continue"},
                )

                self.assertEqual(result["status"], "failure")
                self.assertEqual(result["failure_code"], failure_code)
                self.assertEqual(len(run.call_args_list), 2)
                self.assertEqual(
                    [test["status"] for test in result["tests"]],
                    ["failure", "not_run", "not_run"],
                )

    def test_exact_continue_stops_on_command_start_failure(self) -> None:
        inventory = self.completed(stdout="suite::known: test\nsuite::other: test\n")
        with (
            mock.patch.object(
                MODULE.subprocess,
                "run",
                side_effect=[inventory, OSError("cargo executable unavailable")],
            ) as run,
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(
                {
                    **REQUEST,
                    "execution_mode": "exact_continue",
                    "tests": ["suite::known", "suite::other"],
                },
                Path("/validation-target"),
            )

        self.assertEqual(result["failure_code"], "execution_prerequisite_failed")
        self.assertEqual(len(run.call_args_list), 2)
        self.assertEqual(
            [test["status"] for test in result["tests"]], ["failure", "not_run"]
        )
        self.assertIn(
            "cargo executable unavailable", result["tests"][0]["diagnostics"]["error"]
        )

    def test_exact_continue_reuses_exact_selector_gates(self) -> None:
        request = {
            **REQUEST,
            "execution_mode": "exact_continue",
            "tests": ["-list"],
        }
        inventory = self.completed(stdout="-list: test\n")
        with (
            mock.patch.object(MODULE.subprocess, "run", return_value=inventory) as run,
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(request, Path("/validation-target"))

        self.assertEqual(result["failure_code"], "exact_selector_invalid")
        self.assertEqual(run.call_count, 1)


if __name__ == "__main__":
    main()
