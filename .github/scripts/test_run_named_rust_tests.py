#!/usr/bin/env python3
"""Unit tests for the hosted rust-tests-v1 execution reconciliation."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import tempfile
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

    def test_named_outcome_parser_preserves_control_sequence_status_boundary(self) -> None:
        output = "test suite::known ... \x1b[0 qok\n"
        self.assertEqual(
            MODULE.test_outcomes(output),
            {"suite::known": ["ok"]},
        )

    def test_named_outcome_parser_rejects_status_suffix_inside_word(self) -> None:
        output = "test suite::known ... terminal book\n"
        self.assertEqual(MODULE.test_outcomes(output), {})

    def test_exact_result_reconciles_terminal_rendering_before_status(self) -> None:
        result = self.run_request(
            "test suite::known ... \x1b[?2026h\x1b[1;1H› terminal UI"
            "\x1b[?2026l\x1b[0 qok\n"
            "\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; "
            "0 measured; 0 filtered out\n"
        )

        self.assertEqual(result["status"], "success")
        self.assertTrue(result["tests"][0]["execution_reconciled"])
        self.assertEqual(result["tests"][0]["observed_outcomes"], ["ok"])

    def test_terminal_rendering_does_not_hide_failed_or_ignored_status(self) -> None:
        for status in ("FAILED", "ignored"):
            with self.subTest(status=status):
                output = (
                    "test suite::known ... \x1b[?2026h\x1b[1;1H› terminal UI"
                    f"\x1b[?2026l\x1b[0 q{status}\n"
                )
                self.assertEqual(
                    MODULE.test_outcomes(output),
                    {"suite::known": [status]},
                )

    def test_inventory_failure_preserves_actionable_bounded_diagnostics(self) -> None:
        inventory = self.completed(
            stderr=(
                "manifest could not be loaded\n"
                "error[E0599]: hidden compiler message\n"
                " --> core/src/lib.rs:88:13\n"
            ),
            code=101,
        )
        with (
            mock.patch.object(MODULE.subprocess, "run", return_value=inventory),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(REQUEST, Path("/validation-target"))

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "inventory_failed")
        self.assertIn("manifest could not be loaded", result["inventory"]["diagnostics"]["stderr_tail"])
        self.assertEqual(
            result["inventory"]["diagnostics"]["rustc_error_locations"],
            [{"code": "E0599", "location": "codex-rs/core/src/lib.rs:88:13"}],
        )

    def test_rustc_synopsis_keeps_only_error_codes_and_primary_repo_locations(self) -> None:
        stderr = """error[E0505]: private compiler prose
 --> /home/runner/work/codex/codex/codex-rs/core/src/session/tests.rs:3098:19
  ::: /home/runner/work/codex/codex/codex-rs/core/src/session/tests.rs:3000:7
error[E0382]: another private compiler message
 --> core/src/tools/handlers/multi_agents_v2/wait.rs:1135:31
"""

        summary = MODULE.rustc_error_locations(stderr)

        self.assertEqual(
            summary,
            {
                "entries": [
                    {
                        "code": "E0505",
                        "location": "codex-rs/core/src/session/tests.rs:3098:19",
                    },
                    {
                        "code": "E0382",
                        "location": "codex-rs/core/src/tools/handlers/multi_agents_v2/wait.rs:1135:31",
                    },
                ],
                "truncated": False,
            },
        )
        rendered = json.dumps(summary)
        self.assertNotIn("/home/runner", rendered)
        self.assertNotIn("private compiler", rendered)

    def test_rustc_synopsis_discards_unrelated_absolute_paths(self) -> None:
        stderr = """error[E0425]: private path must not escape
 --> /tmp/private-checkout/src/lib.rs:77:9
"""

        self.assertEqual(
            MODULE.rustc_error_locations(stderr),
            {"entries": [], "truncated": False},
        )

    def test_rustc_synopsis_parses_ansi_colored_compiler_lines(self) -> None:
        stderr = (
            "\x1b[1;31merror[E0425]: private compiler prose\x1b[0m\n"
            " \x1b[1;94m-->\x1b[0m core/src/session/tests.rs:2858:10\n"
        )

        summary = MODULE.rustc_error_locations(stderr)

        self.assertEqual(
            summary,
            {
                "entries": [
                    {
                        "code": "E0425",
                        "location": "codex-rs/core/src/session/tests.rs:2858:10",
                    }
                ],
                "truncated": False,
            },
        )
        rendered = json.dumps(summary)
        self.assertNotIn("private compiler prose", rendered)
        self.assertNotIn("\x1b", rendered)

    def test_rustc_synopsis_caps_entries_and_marks_truncation(self) -> None:
        stderr = "".join(
            f"error[E0001]: hidden {index}\n --> core/src/lib.rs:{index + 1}:1\n"
            for index in range(MODULE.MAX_COMPILER_DIAGNOSTICS + 1)
        )

        summary = MODULE.rustc_error_locations(stderr)

        self.assertEqual(len(summary["entries"]), MODULE.MAX_COMPILER_DIAGNOSTICS)
        self.assertTrue(summary["truncated"])

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

    def test_bin_target_commands_are_catalog_owned(self) -> None:
        request = {
            **REQUEST,
            "package": "codex-cli",
            "target_kind": "bin",
            "target": "codex",
        }
        inventory, execution = MODULE.expected_commands("codex-cli", "bin", "codex")
        with tempfile.TemporaryDirectory() as directory:
            repo_root = Path(directory)
            catalog_path = repo_root / ".github" / MODULE.MANIFEST_NAME
            catalog_path.parent.mkdir()
            catalog_path.write_text(
                json.dumps(
                    {
                        "schema_version": MODULE.MANIFEST_SCHEMA_VERSION,
                        "targets": [
                            {
                                "package": "codex-cli",
                                "target_kind": "bin",
                                "target": "codex",
                                "profiles": ["rust_minimal"],
                                "inventory_argv": inventory,
                                "execution_argv": execution,
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            record = MODULE.select_target(request, MODULE.load_manifest(repo_root))
        self.assertEqual(MODULE.cargo_args(request, list_only=True, command_record=record), inventory)
        self.assertEqual(MODULE.cargo_args(request, list_only=False, command_record=record), execution)

    def test_bin_request_requires_a_safe_catalog_target(self) -> None:
        request = {**REQUEST, "target_kind": "bin", "target": "--workspace"}
        with mock.patch.dict(
            MODULE.os.environ,
            {
                "RUST_TEST_REQUEST_JSON": json.dumps(request),
                "VALIDATION_PROFILE": "rust_minimal",
            },
        ):
            result, error = MODULE.load_request()
        self.assertIsNone(result)
        self.assertEqual(error["failure_code"], "target_invalid")

    def test_exact_mode_runs_only_inventory_confirmed_selectors(self) -> None:
        request = {
            **REQUEST,
            "execution_mode": "exact",
            "tests": ["suite::known", "suite::other"],
        }
        inventory = self.completed(
            stdout="suite::known: test\nsuite::other: test\nsuite::unrelated: test\n"
        )
        selected = self.completed(
            stdout=(
                "test suite::known ... ok\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; "
                "0 measured; 2 filtered out\n"
            )
        )
        selected_again = self.completed(
            stdout=(
                "test suite::other ... ok\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; "
                "0 measured; 2 filtered out\n"
            )
        )
        with (
            mock.patch.object(
                MODULE.subprocess,
                "run",
                side_effect=[inventory, selected, selected_again],
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
                "cargo",
                "test",
                "--locked",
                "-p",
                "codex-core",
                "--lib",
                "suite::known",
                "--",
                "--exact",
                "--test-threads=1",
            ],
        )
        self.assertEqual(
            run.call_args_list[2].args[0][6:],
            ["suite::other", "--", "--exact", "--test-threads=1"],
        )

    def test_exact_command_rejects_selector_outside_request(self) -> None:
        with self.assertRaises(ValueError):
            MODULE.exact_cargo_args(
                REQUEST,
                "suite::not_requested",
                command_record=MANIFEST[("codex-core", "lib", "")],
            )

    def test_exact_mode_stops_after_command_without_test_summary(self) -> None:
        request = {
            **REQUEST,
            "execution_mode": "exact",
            "tests": ["suite::known", "suite::other"],
        }
        inventory = self.completed(stdout="suite::known: test\nsuite::other: test\n")
        compile_failure = self.completed(
            stdout="error: could not compile `codex-core`\n", code=101
        )
        with (
            mock.patch.object(
                MODULE.subprocess,
                "run",
                side_effect=[inventory, compile_failure],
            ) as run,
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(request, Path("/validation-target"))

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "execution_reconciliation_failed")
        self.assertEqual(result["unattempted_tests"], ["suite::other"])
        self.assertEqual(run.call_count, 2)

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
        expected_counts = {
            "passed": 235, "failed": 0, "ignored": 1, "measured": 0, "filtered": 0,
        }
        for code, outcome, expected in [(0, "ok", "success"), (1, "ok", "failure"), (0, "ignored", "failure")]:
            with self.subTest(code=code, outcome=outcome):
                output = (
                    "test child::fixture ... ok\n"
                    "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 234 filtered out\n"
                    f"test suite::known ... {outcome}\n"
                    "test result: ok. 235 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n"
                )
                result = self.run_request(output, test_code=code)
                self.assertEqual(result["status"], expected)
                self.assertEqual(result["tests"][0]["result_counts"], expected_counts)
                self.assertEqual(result["tests"][0]["observed_outcomes"], [outcome])


if __name__ == "__main__":
    main()
