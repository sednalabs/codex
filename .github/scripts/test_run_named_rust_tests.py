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
STDIO_SERVER_REQUEST = {
    "schema_version": "rust-tests-v1",
    "profile": "rust_integration",
    "package": "codex-core",
    "target_kind": "integration",
    "target": "all",
    "tests": [MODULE.STDIO_SERVER_TEST],
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

    def test_more_than_one_executed_test_fails_closed(self) -> None:
        result = self.run_request(
            "test suite::known ... ok\n"
            "test suite::other ... ok\n"
            "\n"
            "test result: ok. 2 passed; 0 failed; 0 ignored; "
            "0 measured; 0 filtered out\n"
        )

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "execution_reconciliation_failed")

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
        with self.assertRaises(ValueError):
            MODULE.cargo_args(REQUEST, list_only=False, test_name="--ignored")
        with self.assertRaises(ValueError):
            MODULE.cargo_args(REQUEST, list_only=False)

    def test_command_builder_returns_only_catalog_commands(self) -> None:
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
                "--",
                "suite::known",
                "--exact",
                "--test-threads=1",
            ],
        )

    def test_execution_uses_one_shell_free_exact_test_command(self) -> None:
        inventory = self.completed(stdout="suite::known: test\n")
        execution = self.completed(
            stdout=(
                "test suite::known ... ok\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; "
                "0 measured; 0 filtered out\n"
            )
        )
        with (
            mock.patch.object(
                MODULE.subprocess, "run", side_effect=[inventory, execution]
            ) as run,
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(REQUEST, Path("/validation-target"))

        self.assertEqual(result["status"], "success")
        execution_call = run.call_args_list[1]
        self.assertEqual(
            execution_call.args[0][-4:],
            ["--", "suite::known", "--exact", "--test-threads=1"],
        )
        self.assertIs(execution_call.kwargs["shell"], False)

    def test_unknown_target_fails_before_cargo(self) -> None:
        request = {**REQUEST, "package": "not-in-catalog"}
        with (
            mock.patch.object(MODULE.subprocess, "run") as run,
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(request, Path("/validation-target"))
        self.assertEqual(result["failure_code"], "target_selector_unknown")
        run.assert_not_called()

    def test_stdio_server_build_is_gated_by_the_exact_legacy_request(self) -> None:
        self.assertTrue(MODULE.requires_stdio_server_build(STDIO_SERVER_REQUEST))

        mutations = (
            {"profile": "rust_minimal"},
            {"package": "codex-rmcp-client"},
            {"target_kind": "lib"},
            {"target": "other"},
            {"tests": ["suite::other"]},
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.assertFalse(
                    MODULE.requires_stdio_server_build(
                        {**STDIO_SERVER_REQUEST, **mutation}
                    )
                )

    def test_stdio_server_build_uses_same_cargo_context_before_exact_test(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target_dir = root / "cargo-target"
            executable = target_dir / "debug" / "test_stdio_server"
            executable.parent.mkdir(parents=True)
            executable.write_text("hosted test fixture", encoding="utf-8")
            executable.chmod(0o755)
            artifact = {
                "reason": "compiler-artifact",
                "target": {"name": "test_stdio_server", "kind": ["bin"]},
                "executable": str(executable),
            }
            inventory = self.completed(stdout=f"{MODULE.STDIO_SERVER_TEST}: test\n")
            build = self.completed(stdout=json.dumps(artifact) + "\n")
            execution = self.completed(
                stdout=(
                    f"test {MODULE.STDIO_SERVER_TEST} ... ok\n"
                    "test result: ok. 1 passed; 0 failed; 0 ignored; "
                    "0 measured; 0 filtered out\n"
                )
            )
            with (
                mock.patch.dict(
                    MODULE.os.environ,
                    {
                        "CARGO_INCREMENTAL": "0",
                        "CARGO_TARGET_DIR": str(target_dir),
                        "RUST_MIN_STACK": "8388608",
                    },
                    clear=False,
                ),
                mock.patch.object(
                    MODULE.subprocess,
                    "run",
                    side_effect=[inventory, build, execution],
                ) as run,
                mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
                mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
            ):
                result = MODULE.run_request(STDIO_SERVER_REQUEST, root)

        self.assertEqual(result["status"], "success")
        self.assertEqual(result["prerequisite_build"]["status"], "success")
        self.assertTrue(result["prerequisite_build"]["executable_ready"])
        self.assertEqual(run.call_count, 3)
        inventory_call, build_call, execution_call = run.call_args_list
        self.assertEqual(
            build_call.args[0],
            [
                "cargo",
                "build",
                "--locked",
                "--profile",
                "test",
                "-p",
                "codex-rmcp-client",
                "--bin",
                "test_stdio_server",
                "--message-format=json-render-diagnostics",
            ],
        )
        self.assertEqual(build_call.kwargs["cwd"], root / "codex-rs")
        self.assertEqual(build_call.kwargs["cwd"], inventory_call.kwargs["cwd"])
        self.assertEqual(build_call.kwargs["cwd"], execution_call.kwargs["cwd"])
        self.assertEqual(build_call.kwargs["env"], inventory_call.kwargs["env"])
        self.assertEqual(build_call.kwargs["env"], execution_call.kwargs["env"])
        self.assertEqual(build_call.kwargs["env"]["CARGO_TARGET_DIR"], str(target_dir))
        self.assertIs(build_call.kwargs["shell"], False)
        self.assertEqual(
            execution_call.args[0][-4:],
            ["--", MODULE.STDIO_SERVER_TEST, "--exact", "--test-threads=1"],
        )

    def test_stdio_server_build_failure_prevents_exact_test_execution(self) -> None:
        inventory = self.completed(stdout=f"{MODULE.STDIO_SERVER_TEST}: test\n")
        build = self.completed(stderr="locked helper build failed", code=101)
        with (
            mock.patch.object(
                MODULE.subprocess, "run", side_effect=[inventory, build]
            ) as run,
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(STDIO_SERVER_REQUEST, Path("/validation-target"))

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "prerequisite_build_failed")
        self.assertEqual(result["prerequisite_build"]["exit_code"], 101)
        self.assertEqual(result["tests"], [])
        self.assertEqual(run.call_count, 2)

    def test_stdio_server_build_requires_the_reported_executable_to_exist(self) -> None:
        inventory = self.completed(stdout=f"{MODULE.STDIO_SERVER_TEST}: test\n")
        missing = {
            "reason": "compiler-artifact",
            "target": {"name": "test_stdio_server", "kind": ["bin"]},
            "executable": "/validation-target/codex-rs/target/debug/test_stdio_server",
        }
        build = self.completed(stdout=json.dumps(missing) + "\n")
        with (
            mock.patch.object(
                MODULE.subprocess, "run", side_effect=[inventory, build]
            ) as run,
            mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
            mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
        ):
            result = MODULE.run_request(STDIO_SERVER_REQUEST, Path("/validation-target"))

        self.assertEqual(result["failure_code"], "prerequisite_build_failed")
        self.assertFalse(result["prerequisite_build"]["executable_ready"])
        self.assertEqual(result["tests"], [])
        self.assertEqual(run.call_count, 2)

    def test_stdio_server_build_rejects_a_different_cargo_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "test_stdio_server"
            executable.write_text("hosted test fixture", encoding="utf-8")
            executable.chmod(0o755)
            wrong_target = {
                "reason": "compiler-artifact",
                "target": {"name": "different_server", "kind": ["bin"]},
                "executable": str(executable),
            }
            inventory = self.completed(stdout=f"{MODULE.STDIO_SERVER_TEST}: test\n")
            build = self.completed(stdout=json.dumps(wrong_target) + "\n")
            with (
                mock.patch.object(
                    MODULE.subprocess, "run", side_effect=[inventory, build]
                ) as run,
                mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
                mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
            ):
                result = MODULE.run_request(
                    STDIO_SERVER_REQUEST, Path("/validation-target")
                )

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "prerequisite_build_failed")
        self.assertFalse(result["prerequisite_build"]["executable_ready"])
        self.assertEqual(result["tests"], [])
        self.assertEqual(run.call_count, 2)

    def test_stdio_server_build_rejects_a_non_executable_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "test_stdio_server"
            executable.write_text("hosted test fixture", encoding="utf-8")
            executable.chmod(0o644)
            artifact = {
                "reason": "compiler-artifact",
                "target": {"name": "test_stdio_server", "kind": ["bin"]},
                "executable": str(executable),
            }
            inventory = self.completed(stdout=f"{MODULE.STDIO_SERVER_TEST}: test\n")
            build = self.completed(stdout=json.dumps(artifact) + "\n")
            with (
                mock.patch.object(
                    MODULE.subprocess, "run", side_effect=[inventory, build]
                ) as run,
                mock.patch.object(MODULE, "git_sha", return_value="target-sha"),
                mock.patch.object(MODULE, "load_manifest", return_value=MANIFEST),
            ):
                result = MODULE.run_request(
                    STDIO_SERVER_REQUEST, Path("/validation-target")
                )

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "prerequisite_build_failed")
        self.assertFalse(result["prerequisite_build"]["executable_ready"])
        self.assertEqual(result["tests"], [])
        self.assertEqual(run.call_count, 2)

    def test_hosted_workflow_runs_runner_controls_before_cargo(self) -> None:
        workflow = (
            Path(__file__).resolve().parents[1]
            / "workflows"
            / "_validation-named-tests.yml"
        ).read_text(encoding="utf-8")
        preflight = workflow.index("name: Test named Rust runner controls")
        rust_toolchain = workflow.index("dtolnay/rust-toolchain@")
        cargo_runner = workflow.index("name: Inventory and run exact named tests")

        self.assertIn("python3 .github/scripts/test_run_named_rust_tests.py", workflow)
        self.assertLess(preflight, rust_toolchain)
        self.assertLess(preflight, cargo_runner)


if __name__ == "__main__":
    main()
