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

    def test_missing_summary_fails_closed_and_reports_full_target_scope(self) -> None:
        private_output = "fixture-only diagnostic must not be serialized"
        result = self.run_request(private_output + "\n")

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "execution_reconciliation_failed")
        self.assertEqual(result["execution_scope"], "full_target")
        self.assertEqual(result["full_target_results"]["observed_test_count"], 0)
        self.assertNotIn(private_output, json.dumps(result))

    def test_candidate_inventory_names_are_not_persisted(self) -> None:
        private_output = "private-report-label::secret: test\n"
        inventory = self.completed(stdout=private_output)
        with (
            mock.patch.object(MODULE.subprocess, "run", return_value=inventory),
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(REQUEST, Path("/validation-target"))

        self.assertEqual(result["failure_code"], "inventory_reconciliation_failed")
        self.assertEqual(result["inventory"]["test_count"], 1)
        self.assertNotIn(private_output.strip(), json.dumps(result))
        self.assertNotIn("tests", result["inventory"])

    def test_inventory_failure_preserves_only_status_and_output_size(self) -> None:
        inventory = self.completed(stderr="manifest could not be loaded", code=101)
        with (
            mock.patch.object(MODULE.subprocess, "run", return_value=inventory),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(REQUEST, Path("/validation-target"))

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "inventory_failed")
        diagnostics = result["inventory"]["diagnostics"]
        self.assertEqual(diagnostics["exit_code"], 101)
        self.assertEqual(diagnostics["stderr_bytes"], len("manifest could not be loaded"))
        self.assertNotIn("manifest could not be loaded", json.dumps(result))

    def test_command_builder_rejects_untrusted_values_at_the_sink(self) -> None:
        with self.assertRaises(ValueError):
            MODULE.cargo_args({**REQUEST, "package": "--workspace"}, list_only=True)
        with self.assertRaises(ValueError):
            MODULE.cargo_args(
                {**REQUEST, "target_kind": "integration", "target": "$(touch nope)"},
                list_only=True,
            )
        with self.assertRaises(ValueError):
            MODULE.cargo_args(
                {**REQUEST, "target_kind": "bin", "target": "$(touch nope)"},
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

        bin_request = {
            **REQUEST,
            "package": "codex-cli",
            "target_kind": "bin",
            "target": "codex",
        }
        self.assertEqual(
            MODULE.cargo_args(bin_request, list_only=True),
            [
                "cargo",
                "test",
                "--locked",
                "-p",
                "codex-cli",
                "--bin",
                "codex",
                "--",
                "--list",
            ],
        )
        self.assertEqual(
            MODULE.cargo_args(bin_request, list_only=False),
            [
                "cargo",
                "test",
                "--locked",
                "-p",
                "codex-cli",
                "--bin",
                "codex",
                "--",
                "--test-threads=1",
            ],
        )

    def test_app_server_protocol_library_uses_fixed_catalog_commands(self) -> None:
        request = {
            "schema_version": "rust-tests-v1",
            "profile": "rust_minimal",
            "package": "codex-app-server-protocol",
            "target_kind": "lib",
            "target": "",
            "tests": ["core_turn_item_into_thread_item_converts_supported_variants"],
        }
        self.assertEqual(
            MODULE.cargo_args(request, list_only=True),
            [
                "cargo",
                "test",
                "--locked",
                "-p",
                "codex-app-server-protocol",
                "--lib",
                "--",
                "--list",
            ],
        )
        self.assertEqual(
            MODULE.cargo_args(request, list_only=False),
            [
                "cargo",
                "test",
                "--locked",
                "-p",
                "codex-app-server-protocol",
                "--lib",
                "--",
                "--test-threads=1",
            ],
        )

    def test_missing_candidate_catalog_uses_trusted_host_catalog(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            candidate_root = Path(directory)
            self.assertEqual(
                MODULE.manifest_path(candidate_root),
                Path(__file__).resolve().parents[2] / ".github" / MODULE.MANIFEST_NAME,
            )
            manifest = MODULE.load_manifest(candidate_root)
            self.assertIn(("codex-cli", "bin", "codex"), manifest)
            self.assertIn(("codex-rmcp-client", "lib", ""), manifest)

    def test_candidate_catalog_takes_precedence_over_trusted_host_catalog(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            candidate_root = Path(directory)
            candidate_manifest = candidate_root / ".github" / MODULE.MANIFEST_NAME
            candidate_manifest.parent.mkdir()
            candidate_manifest.write_text('{"candidate": true}', encoding="utf-8")
            self.assertEqual(MODULE.manifest_path(candidate_root), candidate_manifest)
            with self.assertRaises(ValueError):
                MODULE.load_manifest(candidate_root)

    def test_unknown_target_fails_before_cargo(self) -> None:
        request = {**REQUEST, "package": "not-in-catalog"}
        with (
            mock.patch.object(MODULE.subprocess, "run") as run,
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(request, Path("/validation-target"))
        self.assertEqual(result["failure_code"], "target_selector_unknown")
        run.assert_not_called()

    def test_bin_request_accepts_only_a_safe_catalog_target(self) -> None:
        request = {
            "schema_version": "rust-tests-v1",
            "profile": "rust_minimal",
            "package": "codex-cli",
            "target_kind": "bin",
            "target": "codex",
            "tests": ["mcp_cmd::tests::mcp_device_auth_flag_parses_and_conflicts_with_no_browser"],
        }
        with mock.patch.dict(
            MODULE.os.environ,
            {
                "RUST_TEST_REQUEST_JSON": json.dumps(request),
                "VALIDATION_PROFILE": "rust_minimal",
            },
        ):
            parsed, error = MODULE.load_request()

        self.assertIsNone(error)
        self.assertEqual(parsed, request)


if __name__ == "__main__":
    main()
