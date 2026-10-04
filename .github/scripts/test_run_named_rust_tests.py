"""Deterministic selector-evidence tests for the hosted Rust runner."""

from __future__ import annotations

import copy
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch


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


class CoreRuntimeRequestClassificationTests(unittest.TestCase):
    core_manifest = {
        ("codex-core", "integration", "all"): {
            "profiles": ("rust_integration",),
        }
    }

    def test_exact_valid_core_integration_request_requires_runtime(self) -> None:
        request = {
            "schema_version": named_tests.SCHEMA_VERSION,
            "profile": "rust_integration",
            "package": "codex-core",
            "target_kind": "integration",
            "target": "all",
            "tests": ["suite::selected"],
        }

        self.assertTrue(
            named_tests.request_requires_core_runtime(
                json.dumps(request), "rust_integration", self.core_manifest
            )
        )

    def test_malformed_or_invalid_request_does_not_require_runtime(self) -> None:
        invalid_requests = [
            "not json",
            json.dumps(
                {
                    "schema_version": named_tests.SCHEMA_VERSION,
                    "profile": "rust_integration",
                    "package": "codex-core",
                    "target_kind": "integration",
                    "target": "all",
                }
            ),
            json.dumps(
                {
                    "schema_version": named_tests.SCHEMA_VERSION,
                    "profile": "rust_minimal",
                    "package": "codex-core",
                    "target_kind": "integration",
                    "target": "all",
                    "tests": ["suite::selected"],
                }
            ),
        ]

        for raw in invalid_requests:
            with self.subTest(raw=raw):
                self.assertFalse(
                    named_tests.request_requires_core_runtime(
                        raw, "rust_integration", self.core_manifest
                    )
                )

    def test_other_valid_targets_do_not_require_runtime(self) -> None:
        request = {
            "schema_version": named_tests.SCHEMA_VERSION,
            "profile": "rust_minimal",
            "package": "codex-test",
            "target_kind": "lib",
            "target": "",
            "tests": ["suite::selected"],
        }

        self.assertFalse(
            named_tests.request_requires_core_runtime(
                json.dumps(request),
                "rust_minimal",
                {
                    ("codex-test", "lib", ""): {
                        "profiles": ("rust_minimal",),
                    }
                },
            )
        )

    def test_unknown_exact_core_selector_does_not_require_runtime(self) -> None:
        request = {
            "schema_version": named_tests.SCHEMA_VERSION,
            "profile": "rust_integration",
            "package": "codex-core",
            "target_kind": "integration",
            "target": "all",
            "tests": ["suite::selected"],
        }

        self.assertFalse(
            named_tests.request_requires_core_runtime(
                json.dumps(request), "rust_integration", {}
            )
        )

    def test_cli_classifies_from_actual_target_cwd_manifest(self) -> None:
        script = Path(named_tests.__file__).resolve()
        with tempfile.TemporaryDirectory() as directory:
            target_root = Path(directory) / "validation-target"
            manifest_path = target_root / ".github" / named_tests.MANIFEST_NAME
            manifest_path.parent.mkdir(parents=True)

            def write_manifest(
                package: str, target_kind: str, target: str, profile: str
            ) -> None:
                inventory, execution = named_tests.expected_commands(
                    package, target_kind, target
                )
                manifest_path.write_text(
                    json.dumps(
                        {
                            "schema_version": named_tests.MANIFEST_SCHEMA_VERSION,
                            "targets": [
                                {
                                    "package": package,
                                    "target_kind": target_kind,
                                    "target": target,
                                    "profiles": [profile],
                                    "inventory_argv": inventory,
                                    "execution_argv": execution,
                                }
                            ],
                        }
                    ),
                    encoding="utf-8",
                )

            def classify(request: dict[str, object], profile: str) -> str:
                completed = subprocess.run(
                    [sys.executable, str(script), "--requires-core-runtime"],
                    cwd=target_root,
                    env={
                        **os.environ,
                        "RUST_TEST_REQUEST_JSON": json.dumps(request),
                        "VALIDATION_PROFILE": profile,
                    },
                    text=True,
                    capture_output=True,
                    check=False,
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                return completed.stdout.strip()

            core_request = {
                "schema_version": named_tests.SCHEMA_VERSION,
                "profile": "rust_integration",
                "package": "codex-core",
                "target_kind": "integration",
                "target": "all",
                "tests": ["suite::selected"],
            }
            write_manifest("codex-core", "integration", "all", "rust_integration")
            self.assertEqual(classify(core_request, "rust_integration"), "true")

            other_request = {
                **core_request,
                "profile": "rust_minimal",
                "package": "codex-test",
                "target_kind": "lib",
                "target": "",
            }
            write_manifest("codex-test", "lib", "", "rust_minimal")
            self.assertEqual(classify(other_request, "rust_minimal"), "false")

            # The core request is not admitted unless this target checkout
            # contains its exact closed selector row.
            self.assertEqual(classify(core_request, "rust_integration"), "false")


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
        stdout = (
            f"test {name} ... FAILED\n"
            "failures:\n"
            f"    {name}\n\n"
            f"---- {name} stdout ----\npermission denied\n"
            "failures:\n"
            f"    {name}\n"
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n"
        )
        stderr = f"---- {name} stderr ----\nconnection refused\n"

        evidence = self._evidence(
            stdout,
            stderr,
            requested=(name,),
            known={name},
            summary=named_tests.test_result_counts(stdout),
        )

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
        self.assertEqual(evidence["blocks"][0]["name"], name)
        self.assertTrue(evidence["blocks"][0]["requested_failed_selector"])
        self.assertIn("permission-denied", evidence["blocks"][0]["markers"])
        self.assertFalse(evidence["capture_truncated"])
        self.assertEqual(evidence["upstream_output_truncation"], "unknown")
        self.assertEqual(evidence["cargo_summary_failed_count"], 1)

    def test_cargo_summary_channels_are_numeric_bounded_and_private(self) -> None:
        private = "https://private.example/token=secret /home/runner/user"
        summary = (
            "test result: FAILED. 1 passed; 2 failed; 3 ignored; "
            "4 measured; 5 filtered out"
        )
        evidence = self._evidence(
            "\n".join([f"{summary} {private}" for _ in range(6)]),
            f"{summary} {private}",
        )
        channels = evidence["cargo_summary_channels"]
        self.assertEqual(channels["stdout"]["match_count"], 6)
        self.assertEqual(channels["stdout"]["omitted_count"], 2)
        self.assertTrue(channels["stdout"]["truncated"])
        self.assertEqual(len(channels["stdout"]["summaries"]), 4)
        self.assertEqual(channels["stderr"]["match_count"], 1)
        self.assertEqual(channels["stderr"]["omitted_count"], 0)
        self.assertFalse(channels["stderr"]["truncated"])
        self.assertEqual(
            channels["stdout"]["summaries"][0],
            {"passed": 1, "failed": 2, "ignored": 3, "measured": 4, "filtered": 5},
        )
        rendered = json.dumps(evidence)
        for secret in ("private.example", "token=", "/home/runner"):
            self.assertNotIn(secret, rendered)

    def test_cargo_summary_channels_report_zero_and_multiple_combined_matches(self) -> None:
        none = self._evidence("no summary", "still no summary")
        self.assertEqual(none["cargo_summary_status"], "none")
        self.assertEqual(none["cargo_summary_channels"]["stdout"]["match_count"], 0)
        self.assertEqual(none["cargo_summary_channels"]["stderr"]["match_count"], 0)
        summary = (
            "test result: ok. 1 passed; 0 failed; 0 ignored; "
            "0 measured; 0 filtered out"
        )
        multiple = self._evidence(summary, summary)
        self.assertEqual(multiple["cargo_summary_status"], "unrecognized")
        self.assertEqual(multiple["cargo_summary_channels"]["stdout"]["match_count"], 1)
        self.assertEqual(multiple["cargo_summary_channels"]["stderr"]["match_count"], 1)

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


class CoreIntegrationRuntimePreparationTests(unittest.TestCase):
    source_sha = "a" * 40
    request = {
        "schema_version": named_tests.SCHEMA_VERSION,
        "profile": "rust_integration",
        "package": "codex-core",
        "target_kind": "integration",
        "target": "all",
        "tests": ["suite::selected"],
    }
    command_record = {
        "inventory_argv": (
            "cargo",
            "test",
            "--locked",
            "-p",
            "codex-core",
            "--test",
            "all",
            "--",
            "--list",
        ),
        "execution_argv": (
            "cargo",
            "test",
            "--locked",
            "-p",
            "codex-core",
            "--test",
            "all",
            "--",
            "--test-threads=1",
        ),
    }

    @classmethod
    def _fixture_manifest(
        cls, request: dict[str, object]
    ) -> dict[tuple[str, str, str], dict[str, object]]:
        package = str(request["package"])
        target_kind = str(request["target_kind"])
        target = str(request["target"])
        inventory, execution = named_tests.expected_commands(
            package, target_kind, target
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_file = root / ".github" / named_tests.MANIFEST_NAME
            manifest_file.parent.mkdir(parents=True)
            manifest_file.write_text(
                json.dumps(
                    {
                        "schema_version": named_tests.MANIFEST_SCHEMA_VERSION,
                        "targets": [
                            {
                                "package": package,
                                "target_kind": target_kind,
                                "target": target,
                                "profiles": [str(request["profile"])],
                                "inventory_argv": inventory,
                                "execution_argv": execution,
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            return named_tests.load_manifest(root)

    def _run_with_builds(
        self,
        build_results: list[subprocess.CompletedProcess[str]],
        binary_states: list[tuple[bool, bool]],
        *,
        source_sha: str | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> tuple[dict[str, object], Mock, Mock]:
        env = {"VALIDATION_TARGET_SHA": self.source_sha}
        env.update(extra_env or {})
        inventory = subprocess.CompletedProcess(
            ["cargo", "test"], 0, stdout="suite::selected: test\n", stderr=""
        )
        execution = subprocess.CompletedProcess(
            ["cargo", "test"],
            0,
            stdout=(
                "test suite::selected ... ok\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; "
                "0 measured; 0 filtered out\n"
            ),
            stderr="",
        )
        command_results = [*build_results, inventory, execution]
        with (
            patch.dict("os.environ", env, clear=True),
            patch.object(
                named_tests,
                "load_manifest",
                return_value=self._fixture_manifest(self.request),
            ),
            patch.object(
                named_tests,
                "git_sha",
                return_value=self.source_sha if source_sha is None else source_sha,
            ),
            patch.object(
                named_tests,
                "_core_runtime_binary_state",
                side_effect=binary_states,
            ) as binary_state,
            patch.object(
                named_tests.subprocess, "run", side_effect=command_results
            ) as run,
        ):
            result = named_tests.run_request(self.request, Path("/host/target"))
        return result, run, binary_state

    def test_core_all_builds_exact_binaries_before_unchanged_inventory_and_test(
        self,
    ) -> None:
        build_results = [
            subprocess.CompletedProcess(list(command), 0, stdout="", stderr="")
            for _, command in named_tests.CORE_RUNTIME_BUILDS
        ]
        result, run, binary_state = self._run_with_builds(
            build_results,
            [(False, False), (False, False), (True, True), (True, True)],
            extra_env={"VALIDATION_RUNTIME_PREPARATION_ONLY": "false"},
        )

        self.assertEqual(result["status"], "success")
        self.assertNotIn("result_kind", result)
        self.assertEqual(result["runtime_preparation"]["status"], "success")
        self.assertEqual(result["runtime_preparation"]["source_sha"], self.source_sha)
        self.assertTrue(result["runtime_preparation"]["source_identity_matches"])
        self.assertEqual(
            [call.args[0] for call in run.call_args_list],
            [list(command) for _, command in named_tests.CORE_RUNTIME_BUILDS]
            + [
                list(self.command_record["inventory_argv"]),
                list(self.command_record["execution_argv"]),
            ],
        )
        for call in run.call_args_list:
            self.assertEqual(call.kwargs["cwd"], Path("/host/target") / "codex-rs")
            self.assertEqual(
                call.kwargs["env"],
                {
                    "VALIDATION_TARGET_SHA": self.source_sha,
                    "RUST_MIN_STACK": "8388608",
                },
            )
            self.assertFalse(call.kwargs["shell"])
            self.assertTrue(call.kwargs["capture_output"])
        self.assertEqual(
            [
                binary["regular_file_before"]
                for binary in result["runtime_preparation"]["binaries"]
            ],
            [False, False],
        )
        self.assertEqual(
            [
                binary["executable_after"]
                for binary in result["runtime_preparation"]["binaries"]
            ],
            [True, True],
        )
        self.assertEqual(
            [call.args[0] for call in binary_state.call_args_list],
            [
                Path("/host/target/codex-rs/target/debug") / name
                for name, _ in named_tests.CORE_RUNTIME_BUILDS
            ]
            * 2,
        )

    def test_missing_preflight_mode_preserves_legacy_full_target_path(self) -> None:
        build_results = [
            subprocess.CompletedProcess(list(command), 0, stdout="", stderr="")
            for _, command in named_tests.CORE_RUNTIME_BUILDS
        ]
        result, run, _ = self._run_with_builds(
            build_results,
            [(False, False), (False, False), (True, True), (True, True)],
        )

        self.assertEqual(result["status"], "success")
        self.assertNotIn("result_kind", result)
        self.assertEqual(result["inventory"]["status"], "success")
        self.assertEqual(
            [call.args[0] for call in run.call_args_list],
            [list(command) for _, command in named_tests.CORE_RUNTIME_BUILDS]
            + [
                list(self.command_record["inventory_argv"]),
                list(self.command_record["execution_argv"]),
            ],
        )

    def test_runtime_preflight_stops_after_exact_fixed_builds(self) -> None:
        build_results = [
            subprocess.CompletedProcess(list(command), 0, stdout="", stderr="")
            for _, command in named_tests.CORE_RUNTIME_BUILDS
        ]
        result, run, _ = self._run_with_builds(
            build_results,
            [(False, False), (False, False), (True, True), (True, True)],
            extra_env={"VALIDATION_RUNTIME_PREPARATION_ONLY": "true"},
        )

        self.assertEqual(result["status"], "success")
        self.assertEqual(result["result_kind"], "runtime_preflight")
        request_fingerprint = hashlib.sha256(
            json.dumps(
                self.request, sort_keys=True, separators=(",", ":")
            ).encode("utf-8")
        ).hexdigest()
        self.assertEqual(result["request_fingerprint"], request_fingerprint)
        self.assertEqual(result["candidate_sha"], self.source_sha)
        self.assertEqual(result["runtime_preparation"]["status"], "success")
        self.assertEqual(result["inventory"], {"status": "not-run", "tests": []})
        self.assertEqual(result["tests"], [])
        self.assertEqual(
            [call.args[0] for call in run.call_args_list],
            [list(command) for _, command in named_tests.CORE_RUNTIME_BUILDS],
        )
        for call in run.call_args_list:
            self.assertEqual(call.kwargs["cwd"], Path("/host/target") / "codex-rs")
            self.assertEqual(
                call.kwargs["env"],
                {
                    "VALIDATION_TARGET_SHA": self.source_sha,
                    "RUST_MIN_STACK": "8388608",
                },
            )

    def test_preflight_mode_rejects_malformed_value_before_build(self) -> None:
        result, run, _ = self._run_with_builds(
            [], [], extra_env={"VALIDATION_RUNTIME_PREPARATION_ONLY": "TRUE"}
        )

        self.assertEqual(result["status"], "failure")
        self.assertEqual(
            result["failure_code"], "runtime_preparation_mode_invalid"
        )
        self.assertEqual(result["result_kind"], "runtime_preflight")
        self.assertEqual(result["inventory"], {"status": "not-run", "tests": []})
        self.assertEqual(result["tests"], [])
        self.assertEqual(len(run.call_args_list), 0)

    def test_preflight_mode_rejects_non_core_target_before_build(self) -> None:
        request = {
            **self.request,
            "package": "codex-test",
            "target_kind": "lib",
            "target": "",
        }
        with (
            patch.dict(
                "os.environ",
                {"VALIDATION_RUNTIME_PREPARATION_ONLY": "true"},
                clear=True,
            ),
            patch.object(
                named_tests,
                "load_manifest",
                return_value=self._fixture_manifest(request),
            ),
            patch.object(named_tests.subprocess, "run") as run,
        ):
            result = named_tests.run_request(request, Path("/host/target"))

        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["result_kind"], "runtime_preflight")
        self.assertEqual(
            result["failure_code"], "runtime_preparation_only_unsupported_target"
        )
        self.assertEqual(result["inventory"], {"status": "not-run", "tests": []})
        self.assertEqual(result["tests"], [])
        run.assert_not_called()

    def test_failed_binary_build_stops_before_inventory(self) -> None:
        stderr = "compiler failure " + "x" * (
            named_tests.MAX_DIAGNOSTIC_CHARS + 50
        )
        failed_build = subprocess.CompletedProcess(
            list(named_tests.CORE_RUNTIME_BUILDS[0][1]),
            17,
            stdout="fixed build stdout diagnostic",
            stderr=stderr,
        )
        result, run, _ = self._run_with_builds(
            [failed_build],
            [(False, False), (False, False), (False, False), (False, False)],
            extra_env={"PRIVATE_DIAGNOSTIC_SECRET": "private"},
        )

        self.assertEqual(result["failure_code"], "runtime_preparation_failed")
        self.assertEqual(
            result["runtime_preparation"]["failure_reason"], "binary_build_failed"
        )
        self.assertEqual(result["inventory"]["status"], "not-run")
        self.assertEqual(len(run.call_args_list), 1)
        self.assertNotIn("private", json.dumps(result))
        build = result["runtime_preparation"]["builds"][0]
        self.assertEqual(
            build["diagnostics"], named_tests.command_diagnostics(failed_build)
        )
        self.assertIn(
            "fixed build stdout diagnostic", build["diagnostics"]["stdout_tail"]
        )
        self.assertTrue(
            build["diagnostics"]["stderr_tail"].endswith(
                "x" * named_tests.MAX_DIAGNOSTIC_CHARS
            )
        )
        self.assertLessEqual(
            len(build["diagnostics"]["stderr_tail"]),
            named_tests.MAX_DIAGNOSTIC_CHARS
            + len("...[truncated; only the bounded captured tail is retained]...\n"),
        )
        self.assertNotIn("argv", build["diagnostics"])
        self.assertNotIn("cwd", build["diagnostics"])
        self.assertNotIn("env", build["diagnostics"])

    def test_runtime_preflight_second_build_failure_retains_its_diagnostics(
        self,
    ) -> None:
        first_build = subprocess.CompletedProcess(
            list(named_tests.CORE_RUNTIME_BUILDS[0][1]), 0, stdout="", stderr=""
        )
        second_build = subprocess.CompletedProcess(
            list(named_tests.CORE_RUNTIME_BUILDS[1][1]),
            101,
            stdout="second build stdout",
            stderr="second build compiler diagnostic",
        )
        result, run, _ = self._run_with_builds(
            [first_build, second_build],
            [(False, False), (False, False), (False, False), (False, False)],
            extra_env={"VALIDATION_RUNTIME_PREPARATION_ONLY": "true"},
        )

        self.assertEqual(result["failure_code"], "runtime_preparation_failed")
        self.assertEqual(result["result_kind"], "runtime_preflight")
        self.assertEqual(result["inventory"]["status"], "not-run")
        self.assertEqual(result["tests"], [])
        self.assertEqual(
            [call.args[0] for call in run.call_args_list],
            [list(command) for _, command in named_tests.CORE_RUNTIME_BUILDS],
        )
        self.assertEqual(
            result["runtime_preparation"]["builds"][1]["diagnostics"],
            named_tests.command_diagnostics(second_build),
        )
        self.assertEqual(len(result["runtime_preparation"]["builds"]), 2)
        self.assertEqual(len(run.call_args_list), 2)

    def test_runtime_preflight_launch_failure_has_no_fabricated_diagnostics(
        self,
    ) -> None:
        result, run, binary_state = self._run_with_builds(
            [OSError("private launch detail")],
            [(False, False), (False, False), (False, False), (False, False)],
            extra_env={"VALIDATION_RUNTIME_PREPARATION_ONLY": "true"},
        )

        self.assertEqual(result["failure_code"], "runtime_preparation_failed")
        self.assertEqual(result["result_kind"], "runtime_preflight")
        self.assertEqual(result["candidate_sha"], self.source_sha)
        self.assertEqual(
            result["runtime_preparation"]["source_sha"], self.source_sha
        )
        self.assertTrue(result["runtime_preparation"]["source_identity_matches"])
        self.assertEqual(
            result["runtime_preparation"]["failure_reason"], "binary_build_failed"
        )
        self.assertEqual(result["inventory"], {"status": "not-run", "tests": []})
        self.assertEqual(result["tests"], [])
        self.assertEqual(
            result["runtime_preparation"]["builds"],
            [
                {
                    "name": "codex-code-mode-host",
                    "status": "launch_failed",
                    "exit_code": None,
                }
            ],
        )
        self.assertNotIn("private launch detail", json.dumps(result))
        self.assertNotIn("diagnostics", result["runtime_preparation"]["builds"][0])
        self.assertEqual(len(run.call_args_list), 1)
        self.assertEqual(
            [call.args[0] for call in binary_state.call_args_list],
            [
                Path("/host/target/codex-rs/target/debug") / name
                for name, _ in named_tests.CORE_RUNTIME_BUILDS
            ]
            * 2,
        )

    def test_missing_or_nonexecutable_output_stops_before_inventory(self) -> None:
        build_results = [
            subprocess.CompletedProcess(list(command), 0, stdout="", stderr="")
            for _, command in named_tests.CORE_RUNTIME_BUILDS
        ]
        for bad_state in ((False, False), (True, False)):
            with self.subTest(bad_state=bad_state):
                result, run, _ = self._run_with_builds(
                    build_results,
                    [(False, False), (False, False), bad_state, (True, True)],
                )
                self.assertEqual(result["failure_code"], "runtime_preparation_failed")
                self.assertEqual(
                    result["runtime_preparation"]["failure_reason"],
                    "binary_missing_or_not_executable",
                )
                self.assertEqual(result["inventory"]["status"], "not-run")
                self.assertEqual(len(run.call_args_list), 2)

    def test_source_or_target_directory_mismatch_stops_before_build(self) -> None:
        result, run, _ = self._run_with_builds([], [], source_sha="b" * 40)
        self.assertEqual(result["failure_code"], "runtime_preparation_failed")
        self.assertEqual(
            result["runtime_preparation"]["failure_reason"],
            "source_identity_mismatch",
        )
        self.assertEqual(len(run.call_args_list), 0)

        result, run, _ = self._run_with_builds(
            [], [], extra_env={"CARGO_TARGET_DIR": "relative/target"}
        )
        self.assertEqual(result["failure_code"], "runtime_preparation_failed")
        self.assertEqual(
            result["runtime_preparation"]["failure_reason"],
            "unsupported_target_dir_context",
        )
        self.assertEqual(len(run.call_args_list), 0)

    def test_unexpected_binary_override_stops_before_build(self) -> None:
        result, run, _ = self._run_with_builds(
            [],
            [(False, False), (False, False)],
            extra_env={"CARGO_BIN_EXE_codex-code-mode-host": "/private/host"},
        )

        self.assertEqual(result["failure_code"], "runtime_preparation_failed")
        self.assertEqual(
            result["runtime_preparation"]["failure_reason"],
            "unexpected_binary_environment_override",
        )
        self.assertIn(
            "CARGO_BIN_EXE_codex-code-mode-host",
            json.dumps(result["runtime_preparation"]["binaries"]),
        )
        self.assertNotIn("/private/host", json.dumps(result))
        self.assertEqual(len(run.call_args_list), 0)

    def test_preexisting_binary_is_not_accepted_as_same_target_build_output(
        self,
    ) -> None:
        result, run, _ = self._run_with_builds(
            [], [(True, True), (False, False)]
        )

        self.assertEqual(result["failure_code"], "runtime_preparation_failed")
        self.assertEqual(
            result["runtime_preparation"]["failure_reason"],
            "binary_present_before_build",
        )
        self.assertEqual(
            [
                binary["regular_file_before"]
                for binary in result["runtime_preparation"]["binaries"]
            ],
            [True, False],
        )
        self.assertEqual(len(run.call_args_list), 0)


class ExistingFailureObserverRegressionCarryover(unittest.TestCase):
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

    def test_markers_and_numeric_codes_are_allowlisted_only(self) -> None:
        name = "suite::safe"
        private = "https://private.example/path?token=secret /home/runner/alice alice@example.com"
        output = (
            f"---- {name} stdout ----\n"
            f"permission denied {private}\n"
            f"Os {{ code: 13, kind: PermissionDenied }} (os error 2) HTTP status 503 {private}\n"
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
            {"kind": "os-error-code", "value": 2}, block["numeric_captures"]
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
        with (
            patch.object(
                named_tests,
                "load_manifest",
                return_value=CoreIntegrationRuntimePreparationTests._fixture_manifest(
                    request
                ),
            ),
            patch.object(named_tests, "git_sha", return_value="a" * 40),
            patch.object(named_tests.subprocess, "run", side_effect=[inventory, execution]),
        ):
            result = named_tests.run_request(request, Path("."))

        self.assertEqual(result["tests"][0]["observed_outcomes"], ["ok"])
        self.assertEqual(result["tests"][0]["status"], "failure")
        self.assertEqual(result["failure_code"], "named_test_failed")
        self.assertEqual(result["failure_evidence"]["cargo_summary_failed_count"], 1)
        self.assertEqual(result["failure_evidence"]["failed_name_distinct_count"], 1)

    def test_red_target_serializes_all_selectors_and_preserves_first_failure(self) -> None:
        first, second = "suite::first", "suite::second"
        request = {
            "schema_version": named_tests.SCHEMA_VERSION,
            "profile": "rust_minimal",
            "package": "codex-test",
            "target_kind": "lib",
            "target": "",
            "tests": [first, second],
        }
        inventory = subprocess.CompletedProcess(
            ["cargo", "test", "--list"],
            0,
            stdout=f"{first}: test\n{second}: test\n",
            stderr="",
        )
        outputs = (
            (f"test {second} ... FAILED\n", 1, "execution_reconciliation_failed"),
            (
                f"test {first} ... ignored\ntest {second} ... FAILED\n",
                1,
                "named_test_ignored",
            ),
            (
                f"test {first} ... FAILED\ntest {second} ... ignored\n",
                1,
                "named_test_failed",
            ),
            (
                f"test {first} ... ok\ntest {second} ... ok\n",
                0,
                "execution_reconciliation_failed",
            ),
        )
        manifest = CoreIntegrationRuntimePreparationTests._fixture_manifest(request)
        for output, exit_code, first_failure in outputs:
            with self.subTest(output=output):
                execution = subprocess.CompletedProcess(
                    ["cargo", "test"], exit_code, stdout=output, stderr=""
                )
                with (
                    patch.object(named_tests, "load_manifest", return_value=manifest),
                    patch.object(named_tests, "git_sha", return_value="a" * 40),
                    patch.object(
                        named_tests.subprocess,
                        "run",
                        side_effect=[inventory, execution],
                    ),
                ):
                    result = named_tests.run_request(request, Path("."))

                self.assertEqual(len(result["tests"]), 2)
                self.assertEqual(
                    [test["name"] for test in result["tests"]], [first, second]
                )
                self.assertEqual(result["status"], "failure")
                self.assertEqual(result["failure_code"], first_failure)

    def test_multiple_summaries_with_overlong_count_remain_structured_failure(self) -> None:
        first, second = "suite::first", "suite::second"
        request = {
            "schema_version": named_tests.SCHEMA_VERSION,
            "profile": "rust_minimal",
            "package": "codex-test",
            "target_kind": "lib",
            "target": "",
            "tests": [first, second],
        }
        inventory = subprocess.CompletedProcess(
            ["cargo", "test", "--list"],
            0,
            stdout=f"{first}: test\n{second}: test\n",
            stderr="",
        )
        summary = (
            "test result: ok. 1 passed; 0 failed; 0 ignored; "
            "0 measured; 0 filtered out"
        )
        overlong = (
            "test result: ok. "
            + "9" * 5000
            + " passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
        )
        execution = subprocess.CompletedProcess(
            ["cargo", "test"],
            0,
            stdout=(
                f"test {first} ... ok\n"
                f"test {second} ... ignored\n"
                f"{overlong}\n{summary}\n"
            ),
            stderr="",
        )
        manifest = CoreIntegrationRuntimePreparationTests._fixture_manifest(request)
        with (
            patch.object(named_tests, "load_manifest", return_value=manifest),
            patch.object(named_tests, "git_sha", return_value="a" * 40),
            patch.object(
                named_tests.subprocess,
                "run",
                side_effect=[inventory, execution],
            ),
        ):
            result = named_tests.run_request(request, Path("."))

        channel = result["failure_evidence"]["cargo_summary_channels"]["stdout"]
        self.assertEqual(result["status"], "failure")
        self.assertEqual(result["failure_code"], "execution_reconciliation_failed")
        self.assertEqual([test["name"] for test in result["tests"]], [first, second])
        self.assertEqual(
            [test["observed_outcomes"] for test in result["tests"]],
            [["ok"], ["ignored"]],
        )
        self.assertEqual(channel["match_count"], 2)
        self.assertEqual(channel["omitted_count"], 1)
        self.assertTrue(channel["truncated"])
        self.assertEqual(
            channel["summaries"],
            [{"passed": 1, "failed": 0, "ignored": 0, "measured": 0, "filtered": 0}],
        )

    def test_non_core_target_does_not_prebuild_runtime_binaries(self) -> None:
        request = {
            **CoreIntegrationRuntimePreparationTests.request,
            "package": "codex-test",
            "target_kind": "lib",
            "target": "",
        }
        inventory = subprocess.CompletedProcess(
            ["cargo", "test"], 0, stdout="suite::selected: test\n", stderr=""
        )
        execution = subprocess.CompletedProcess(
            ["cargo", "test"],
            0,
            stdout=(
                "test suite::selected ... ok\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; "
                "0 measured; 0 filtered out\n"
            ),
            stderr="",
        )
        record = CoreIntegrationRuntimePreparationTests._fixture_manifest(request)[
            named_tests.target_key("codex-test", "lib", "")
        ]
        source_sha = CoreIntegrationRuntimePreparationTests.source_sha
        with (
            patch.dict(
                "os.environ", {"VALIDATION_TARGET_SHA": source_sha}, clear=True
            ),
            patch.object(
                named_tests,
                "load_manifest",
                return_value=CoreIntegrationRuntimePreparationTests._fixture_manifest(
                    request
                ),
            ),
            patch.object(named_tests, "git_sha", return_value=source_sha),
            patch.object(
                named_tests.subprocess, "run", side_effect=[inventory, execution]
            ) as run,
        ):
            result = named_tests.run_request(request, Path("/host/target"))

        self.assertEqual(result["status"], "success")
        self.assertNotIn("runtime_preparation", result)
        self.assertEqual(
            [call.args[0] for call in run.call_args_list],
            [list(record["inventory_argv"]), list(record["execution_argv"])],
        )

    def test_unknown_target_is_rejected_without_runtime_action(self) -> None:
        with (
            patch.object(
                named_tests,
                "load_manifest",
                return_value={},
            ),
            patch.object(named_tests.subprocess, "run") as run,
        ):
            result = named_tests.run_request(
                CoreIntegrationRuntimePreparationTests.request, Path("/host/target")
            )

        self.assertEqual(result["failure_code"], "target_selector_unknown")
        run.assert_not_called()


class PublicArtifactBoundaryTests(unittest.TestCase):
    root = Path(named_tests.__file__).resolve().parents[2]
    identity = {"harness_sha": "b" * 40, "base_sha": "c" * 40,
                "target_sha": "a" * 40, "base_ref": "validation/base",
                "run_id": "123", "run_attempt": "2"}
    request = {"schema_version": named_tests.SCHEMA_VERSION, "profile": "rust_minimal",
               "package": "codex-cli", "target_kind": "bin", "target": "codex",
               "tests": ["suite::selected"]}

    def _minimal(self) -> dict[str, object]:
        return {"schema_version": named_tests.SCHEMA_VERSION, "status": "success",
                "identity": dict(self.identity), "candidate_sha": "a" * 40,
                "request": dict(self.request), "request_fingerprint": hashlib.sha256(
                    json.dumps(self.request, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
                "inventory": {"status": "success", "test_count": 1, "tests": ["suite::selected"]},
                "tests": [{"name": "suite::selected", "status": "success", "exit_code": 0,
                           "execution_reconciled": True, "observed_outcomes": ["ok"],
                           "matched_line_count": 1, "matched_lines": ["test suite::selected ... ok"],
                           "matched_lines_truncated": False,
                           "result_counts": {"passed": 1, "failed": 0, "ignored": 0, "measured": 0, "filtered": 0}}]}

    def _expected(self) -> dict[str, object]:
        diagnostic = {"original_char_count": None, "captured_char_count": 0,
                      "markers": {"values": [], "original_count": 0, "omitted_count": 0, "truncated": False},
                      "numeric_captures": {"values": [], "original_count": 0, "omitted_count": 0, "truncated": False},
                      "captured_text_omitted": True, "truncated": True}
        expected = self._minimal()
        expected.update(failure_code="", result_kind="named_tests", omitted_field_count=0)
        expected["identity"]["omitted_field_count"] = 0
        expected["request"].update(catalog_validated=True, omitted_field_count=0,
                                   request_fingerprint=expected["request_fingerprint"],
                                   selectors={"original_count": 1, "omitted_count": 0, "truncated": False})
        expected["inventory"].update(omitted_field_count=0, original_count=1, omitted_count=0, truncated=False)
        expected["tests"][0].update(omitted_field_count=0, outcome_original_count=1, outcome_omitted_count=0,
                                     diagnostics={"exit_code": None, "stdout": copy.deepcopy(diagnostic),
                                                  "stderr": copy.deepcopy(diagnostic), "omitted_field_count": 0})
        expected["test_projection"] = {"original_count": 1, "omitted_count": 0, "truncated": False}
        return expected

    def _bytes(self, value: dict[str, object]) -> bytes:
        return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")

    def _run(self, request: dict[str, object], inventory_text: str, output: str, exit_code: int = 0, inventory_exit: int = 0) -> dict[str, object]:
        record = named_tests.select_target(request, named_tests.load_manifest(self.root))
        expected_calls = [list(record["inventory_argv"]), ["git", "rev-parse", "HEAD"], list(record["execution_argv"])]
        calls = []

        def fake_run(argv: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
            calls.append(argv)
            if argv == ["git", "rev-parse", "HEAD"]:
                self.assertEqual(kwargs["cwd"], self.root)
                return subprocess.CompletedProcess(argv, 0, "a" * 40 + "\n", "")
            self.assertEqual(kwargs["cwd"], self.root / "codex-rs")
            self.assertEqual({key: kwargs[key] for key in ("text", "capture_output", "check", "shell")},
                             {"text": True, "capture_output": True, "check": False, "shell": False})
            self.assertEqual(kwargs["env"], {"RUST_MIN_STACK": "8388608"})
            if argv == expected_calls[0]:
                return subprocess.CompletedProcess(argv, inventory_exit, inventory_text, "")
            self.assertEqual(argv, expected_calls[2])
            return subprocess.CompletedProcess(argv, exit_code, output, "")

        with patch.dict(os.environ, {}, clear=True), patch.object(named_tests.subprocess, "run", side_effect=fake_run):
            result = named_tests.run_request(request, self.root)
        expected_count = 1 if inventory_exit else 2 if result.get("failure_code") == "inventory_reconciliation_failed" else 3
        self.assertEqual(calls, expected_calls[:expected_count])
        self.assertTrue(all("suite::selected" not in argv for argv in calls))
        result["identity"] = dict(self.identity)
        return result

    def test_complete_positive_and_critical_failure_bytes(self) -> None:
        self.assertEqual(named_tests.public_artifact_bytes(self._minimal()), self._bytes(self._expected()))
        for code in ("named_test_failed", "named_test_ignored", "execution_reconciliation_failed"):
            with self.subTest(code=code):
                source, expected = self._minimal(), self._expected()
                source.update(status="failure", failure_code=code, message="PRIVATE_MESSAGE")
                expected.update(status="failure", failure_code=code, omitted_field_count=1)
                outcomes = ["FAILED"] if code == "named_test_failed" else ["ignored"] if code == "named_test_ignored" else ["ok", "FAILED"]
                counts = {"passed": int("ok" in outcomes), "failed": int("FAILED" in outcomes),
                          "ignored": int("ignored" in outcomes), "measured": 0, "filtered": 0}
                record = {"status": "failure", "exit_code": 101 if code == "named_test_failed" else 0,
                          "execution_reconciled": False, "observed_outcomes": outcomes,
                          "matched_line_count": len(outcomes), "result_counts": counts,
                          "matched_lines": [f"test suite::selected ... {outcome}" for outcome in outcomes]}
                source["tests"][0].update(record)
                expected["tests"][0].update(record, outcome_original_count=len(outcomes))
                self.assertEqual(named_tests.public_artifact_bytes(source), self._bytes(expected))

    def test_complete_inventory_failure_and_missing_selector_bytes(self) -> None:
        source = named_tests.fail("inventory_failed", "PRIVATE_MESSAGE")
        source["identity"] = dict(self.identity)
        source["inventory"] = {"status": "failure", "tests": [], "diagnostics": named_tests.command_diagnostics(subprocess.CompletedProcess([], 101, "", ""))}
        expected = self._expected()
        diagnostic = copy.deepcopy(expected["tests"][0]["diagnostics"])
        diagnostic["exit_code"] = 101
        for stream in ("stdout", "stderr"):
            diagnostic[stream].update(original_char_count=0, truncated=False)
        expected.update(status="failure", failure_code="inventory_failed", candidate_sha="", request_fingerprint="", omitted_field_count=1,
            request={"tests": [], "selectors": {"original_count": 0, "omitted_count": 0, "truncated": False}, "catalog_validated": False, "request_fingerprint": "", "omitted_field_count": 0},
            inventory={"status": "failure", "tests": [], "original_count": 0, "omitted_count": 0, "truncated": False, "omitted_field_count": 0, "diagnostics": diagnostic},
            tests=[], test_projection={"original_count": 0, "omitted_count": 0, "truncated": False})
        self.assertEqual(named_tests.public_artifact_bytes(source), self._bytes(expected))
        source, expected = self._minimal(), self._expected()
        source["request"]["tests"] = ["suite::absent"]
        fingerprint = hashlib.sha256(json.dumps(source["request"], sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        source.update(status="failure", failure_code="inventory_reconciliation_failed", tests=[],
                      request_fingerprint=fingerprint, missing_tests=["suite::absent"], ambiguous_tests=["suite::absent"])
        expected.update(status="failure", failure_code="inventory_reconciliation_failed", tests=[], request_fingerprint=fingerprint,
                        test_projection={"original_count": 0, "omitted_count": 0, "truncated": False},
                        inventory_reconciliation={key: {"names": [], "original_count": 1, "omitted_count": 1, "truncated": True} for key in ("missing_tests", "ambiguous_tests")})
        expected["request"].update(tests=[], request_fingerprint=fingerprint, selectors={"original_count": 1, "omitted_count": 1, "truncated": True})
        self.assertEqual(named_tests.public_artifact_bytes(source), self._bytes(expected))

    def test_actual_new_and_existing_targets_keep_closed_argv(self) -> None:
        for package, kind, target in (("codex-cli", "bin", "codex"), ("codex-diagnostics", "lib", ""), ("codex-state", "lib", "")):
            request = {**self.request, "package": package, "target_kind": kind, "target": target}
            parsed, error = named_tests.parse_request(json.dumps(request), "rust_minimal")
            self.assertIsNone(error)
            result = self._run(parsed, "suite::selected: test\n", "test suite::selected ... ok\n"
                               "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n")
            public = json.loads(named_tests.public_artifact_bytes(result))
            self.assertEqual(public["status"], "success")
            self.assertFalse(named_tests.request_requires_core_runtime(json.dumps(request), "rust_minimal", named_tests.load_manifest(self.root)))
            self.assertNotIn("runtime_preparation", result)

    def test_bin_request_and_manifest_fail_closed(self) -> None:
        for target in ("", None, "../codex", "codex --all", "x;echo", ["codex"]):
            request, error = named_tests.parse_request(json.dumps({**self.request, "target": target}), "rust_minimal")
            self.assertIsNone(request)
            self.assertEqual(error["failure_code"], "target_invalid")
        payload = json.loads((self.root / ".github" / named_tests.MANIFEST_NAME).read_text())
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / ".github" / named_tests.MANIFEST_NAME
            path.parent.mkdir()
            for target in ("", "../codex"):
                altered = copy.deepcopy(payload)
                row = next(row for row in altered["targets"] if row["target_kind"] == "bin")
                row["target"] = target
                path.write_text(json.dumps(altered))
                with self.assertRaises(ValueError):
                    named_tests.load_manifest(Path(directory))
        with self.assertRaises(ValueError):
            named_tests.cargo_args(self.request, list_only=False, test_name="suite::selected")

    def test_only_lib_normalizes_target_and_old_commands_are_unchanged(self) -> None:
        for kind in ("bin", "integration"):
            request, error = named_tests.parse_request(json.dumps({**self.request, "target_kind": kind, "target": "lib"}), "rust_minimal")
            self.assertIsNone(error)
            self.assertEqual(request["target"], "lib")
        for target in ("", None, "lib"):
            request, error = named_tests.parse_request(json.dumps({**self.request, "target_kind": "lib", "target": target}), "rust_minimal")
            self.assertIsNone(error)
            self.assertEqual(request["target"], "")
        for kind, target, flag in (("lib", "", "--lib"), ("integration", "all", "--test"), ("bin", "codex", "--bin")):
            prefix = ["cargo", "test", "--locked", "-p", "codex-cli", flag]
            if kind != "lib":
                prefix.append(target)
            self.assertEqual(named_tests.expected_commands("codex-cli", kind, target),
                             ([*prefix, "--", "--list"], [*prefix, "--", "--test-threads=1"]))

    def test_actual_full_target_red_and_reconciliation_controls(self) -> None:
        summary = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
        cases = [
            ("suite::selected: test\nsuite::other: test\n", "test suite::selected ... ok\ntest suite::other ... FAILED\n"
             "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n", 101, "named_test_failed"),
            ("suite::other: test\n", "", 0, "inventory_reconciliation_failed"),
            ("suite::selected: test\nsuite::selected: test\n", "", 0, "inventory_reconciliation_failed"),
            ("suite::selected: test\n", "test suite::selected ... ignored\n" + summary, 0, "named_test_ignored"),
            ("suite::selected: test\n", "test suite::selected ... ok\ntest suite::selected ... FAILED\n" + summary, 0, "execution_reconciliation_failed"),
            ("suite::selected: test\n", "test suite::selected ... ok\nmalformed summary\n", 0, "execution_reconciliation_failed"),
            ("suite::selected: test\n", "test suite::selected ... ok\n" + summary * 2, 0, "execution_reconciliation_failed"),
            ("suite::selected: test\n", "test suite::selected ... ok\n" + summary, 101, "named_test_failed"),
        ]
        for inventory, output, exit_code, code in cases:
            with self.subTest(code=code, output=output):
                public = json.loads(named_tests.public_artifact_bytes(self._run(self.request, inventory, output, exit_code)))
                self.assertEqual((public["status"], public["failure_code"]), ("failure", code))
                if code == "inventory_reconciliation_failed":
                    facts = public["inventory_reconciliation"]
                    self.assertEqual(facts["ambiguous_tests"]["original_count"], 1)
                    self.assertEqual(facts["missing_tests"]["omitted_count"], int("selected" not in inventory))
                else:
                    self.assertFalse(public["tests"][0]["execution_reconciled"])
                if "suite::other ... FAILED" in output:
                    self.assertEqual(public["failure_evidence"]["failed_names"]["names"], ["suite::other"])

    def test_whole_bytes_omit_canaries_on_every_nested_surface(self) -> None:
        private = "PRIVATE_BODY credential-token /home/private/file https://private.invalid/secret"
        text = "permission denied (os error 13) HTTP status 503 exit code: 7 " + private
        diagnostic = named_tests.command_diagnostics(subprocess.CompletedProcess([], 101, text, text * 100))
        result = self._minimal()
        result.update(status="failure", failure_code="named_test_failed", message=private, unknown={"body": private})
        result["identity"]["url"] = private
        result["request"]["body"] = private
        result["inventory"]["diagnostics"] = diagnostic
        result["tests"][0].update(diagnostics=diagnostic, matched_lines=[private], body=private)
        result["runtime_preparation"] = {"status": "failure", "failure_reason": "binary_build_failed",
            "source_sha": "a" * 40, "expected_target_sha": "a" * 40, "source_identity_matches": True,
            "target_dir_context": "workspace_default", "path": private,
            "builds": [{"name": "codex", "status": "failure", "exit_code": 101, "diagnostics": diagnostic, "body": private}],
            "binaries": [{"name": "codex", "env_key_set": {"CARGO_BIN_EXE_codex": False, private: private}, "path": private}]}
        result["failure_evidence"] = named_tests.failure_evidence(self.request, {"suite::selected"},
            "test suite::selected ... FAILED\n---- suite::selected stdout ----\n" + text,
            "", None, "")
        result["failure_evidence"]["blocks"][0].update(body=private, markers=["permission-denied", private],
            numeric_captures=[{"kind": "os-error-code", "value": 13}, {"kind": private, "value": 7}, {"kind": "http-status-code", "value": True}])
        data = named_tests.public_artifact_bytes(result)
        for canary in private.split():
            self.assertNotIn(canary.encode(), data)
        self.assertNotIn(b"stdout_tail", data)
        self.assertNotIn(hashlib.sha256(private.encode()).hexdigest().encode(), data)
        public = json.loads(data)
        stdout = public["tests"][0]["diagnostics"]["stdout"]
        self.assertEqual(stdout["original_char_count"], len(text))
        self.assertEqual(stdout["markers"]["values"], ["http-error", "permission-denied"])
        self.assertEqual(stdout["numeric_captures"]["values"], [{"kind": "http-status-code", "value": 503}, {"kind": "os-error-code", "value": 13}, {"kind": "process-exit-code", "value": 7}])
        self.assertTrue(public["runtime_preparation"]["builds"][0]["diagnostics"]["stderr"]["truncated"])
        self.assertEqual(public["failure_evidence"]["blocks"][0]["numeric_captures"]["omitted_count"], 2)

    def test_actual_inventory_failure_has_only_safe_structured_diagnostics(self) -> None:
        result = self._run(self.request, "permission denied (os error 13) PRIVATE_INVENTORY /private\n", "", inventory_exit=101)
        data = named_tests.public_artifact_bytes(result)
        public = json.loads(data)
        self.assertEqual((public["status"], public["failure_code"], public["tests"]), ("failure", "inventory_failed", []))
        self.assertEqual(public["inventory"]["diagnostics"]["stdout"]["numeric_captures"]["values"], [{"kind": "os-error-code", "value": 13}])
        self.assertNotIn(b"PRIVATE_INVENTORY", data)
        self.assertNotIn(b"/private", data)

    def test_malformed_material_fields_cannot_serialize_success(self) -> None:
        for field, value in (("candidate_sha", "bad /private"), ("request_fingerprint", "f" * 64), ("identity", {}), ("result_kind", "unknown")):
            source = self._minimal()
            source[field] = value
            public = json.loads(named_tests.public_artifact_bytes(source))
            self.assertEqual((public["status"], public["failure_code"]), ("failure", "public_projection_incomplete"))
        source, expected = self._minimal(), self._expected()
        source["identity"].pop("target_sha")
        expected["identity"].pop("target_sha")
        expected.update(status="failure", failure_code="public_projection_incomplete", incomplete=True)
        self.assertEqual(named_tests.public_artifact_bytes(source), self._bytes(expected))
        for source_field in ("status", "exit_code", "execution_reconciled", "observed_outcomes", "result_counts", "matched_line_count"):
            source = self._minimal()
            source["tests"][0][source_field] = {"private": "PRIVATE_BODY"}
            self.assertEqual(json.loads(named_tests.public_artifact_bytes(source))["status"], "failure")

    def test_source_mismatch_runtime_projection_remains_red(self) -> None:
        result = self._minimal()
        result.update(status="failure", failure_code="runtime_preparation_failed")
        result["runtime_preparation"] = named_tests.prepare_core_integration_runtime(
            CoreIntegrationRuntimePreparationTests.request, self.root / "codex-rs", {"VALIDATION_TARGET_SHA": "a" * 40}, "d" * 40)
        public = json.loads(named_tests.public_artifact_bytes(result))
        self.assertEqual(public["status"], "failure")
        self.assertEqual(public["runtime_preparation"]["source_sha"], "d" * 40)
        self.assertFalse(public["runtime_preparation"]["source_identity_matches"])

    def test_actual_preflight_projection_and_missing_join_are_not_named_success(self) -> None:
        fixture = CoreIntegrationRuntimePreparationTests()
        builds = [subprocess.CompletedProcess(list(command), 0, "", "") for _, command in named_tests.CORE_RUNTIME_BUILDS]
        result, run, states = fixture._run_with_builds(builds, [(False, False), (False, False), (True, True), (True, True)],
                                                     extra_env={"VALIDATION_RUNTIME_PREPARATION_ONLY": "true"})
        self.assertEqual([call.args[0] for call in run.call_args_list], [list(command) for _, command in named_tests.CORE_RUNTIME_BUILDS])
        self.assertEqual(states.call_count, 4)
        result["identity"] = dict(self.identity)
        public = json.loads(named_tests.public_artifact_bytes(result))
        self.assertEqual((public["status"], public["result_kind"], public["tests"]), ("success", "runtime_preflight", []))
        self.assertEqual(public["request"]["selectors"]["omitted_count"], 1)
        for key in ("runtime_preparation", "candidate_sha"):
            damaged = copy.deepcopy(result)
            damaged.pop(key)
            self.assertEqual(json.loads(named_tests.public_artifact_bytes(damaged))["failure_code"], "public_projection_incomplete")
        damaged = copy.deepcopy(result)
        damaged["runtime_preparation"]["builds"].append(dict(damaged["runtime_preparation"]["builds"][0]))
        self.assertEqual(json.loads(named_tests.public_artifact_bytes(damaged))["failure_code"], "public_projection_incomplete")

    def test_size_cap_checks_final_bytes_and_constant_fallback(self) -> None:
        source = self._minimal()
        names = [f"suite::{index:04}_{'x' * 238}" for index in range(named_tests.MAX_PUBLIC_INVENTORY_NAMES)]
        source["inventory"].update(tests=names, test_count=len(names))
        data = named_tests.public_artifact_bytes(source)
        public = json.loads(data)
        self.assertEqual(public["failure_code"], "public_result_overflow")
        self.assertTrue(public["truncated"])
        self.assertGreater(public["original_serialized_bytes"], named_tests.MAX_PUBLIC_RESULT_BYTES)
        self.assertLessEqual(len(data), named_tests.MAX_PUBLIC_RESULT_BYTES)
        self.assertNotIn(names[-1].encode(), data)
        with patch.object(named_tests, "MAX_PUBLIC_RESULT_BYTES", 200):
            with self.assertRaises(named_tests.PublicResultError):
                named_tests.public_artifact_bytes(self._minimal())
            fallback = named_tests._fixed_tiny_overflow_failure_bytes()
            self.assertEqual(fallback, b'{"schema_version":"rust-tests-v1","status":"failure","failure_code":"public_result_overflow","omitted":"full_result_not_emitted","truncated":true}\n')
            self.assertLessEqual(len(fallback), 200)
        with patch.object(named_tests, "MAX_PUBLIC_RESULT_BYTES", 1):
            with self.assertRaises(named_tests.PublicResultError):
                named_tests._fixed_tiny_overflow_failure_bytes()

    def test_main_exception_and_only_persistent_write_are_public_bytes(self) -> None:
        env = {"VALIDATION_HARNESS_SHA": "b" * 40, "VALIDATION_BASE_REF": "validation/base",
               "VALIDATION_BASE_SHA": "c" * 40, "VALIDATION_TARGET_SHA": "a" * 40,
               "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2"}
        expected = {"schema_version": named_tests.SCHEMA_VERSION, "status": "failure",
            "failure_code": "runner_unexpected_exception", "result_kind": "named_tests", "omitted_field_count": 1,
            "request_fingerprint": "", "candidate_sha": "", "identity": {**self.identity, "omitted_field_count": 0},
            "request": {"tests": [], "selectors": {"original_count": 0, "omitted_count": 0, "truncated": False}, "catalog_validated": False, "request_fingerprint": "", "omitted_field_count": 0},
            "inventory": {"status": "not-run", "tests": [], "omitted_field_count": 0, "original_count": 0, "omitted_count": 0, "truncated": False},
            "tests": [], "test_projection": {"original_count": 0, "omitted_count": 0, "truncated": False}}
        with patch.dict(os.environ, env, clear=True), patch.object(sys, "argv", ["runner"]), \
                patch.object(named_tests, "load_request", return_value=(self.request, None)), \
                patch.object(named_tests, "run_request", side_effect=RuntimeError("PRIVATE_EXCEPTION /private credential-token")) as run, \
                patch.object(Path, "write_bytes") as write, patch("builtins.print") as output:
            self.assertEqual(named_tests.main(), 1)
        run.assert_called_once_with(self.request, Path.cwd().resolve())
        write.assert_called_once_with(self._bytes(expected))
        output.assert_called_once_with('{"failure_code": "runner_unexpected_exception", "status": "failure"}')

    def test_main_uses_projected_status_and_never_writes_over_cap(self) -> None:
        env = {"VALIDATION_HARNESS_SHA": "b" * 40, "VALIDATION_BASE_REF": "validation/base",
               "VALIDATION_BASE_SHA": "c" * 40, "VALIDATION_TARGET_SHA": "a" * 40,
               "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2"}
        for cap, exit_code in ((named_tests.MAX_PUBLIC_RESULT_BYTES, 0), (200, 1), (1, None)):
            with self.subTest(cap=cap), patch.dict(os.environ, env, clear=True), \
                    patch.object(sys, "argv", ["runner"]), patch.object(named_tests, "MAX_PUBLIC_RESULT_BYTES", cap), \
                    patch.object(named_tests, "load_request", return_value=(self.request, None)), \
                    patch.object(named_tests, "run_request", return_value=self._minimal()), \
                    patch.object(Path, "write_bytes") as write, patch("builtins.print") as output:
                if exit_code is None:
                    with self.assertRaises(named_tests.PublicResultError):
                        named_tests.main()
                    write.assert_not_called()
                    output.assert_not_called()
                else:
                    self.assertEqual(named_tests.main(), exit_code)
                    expected = self._bytes(self._expected()) if exit_code == 0 else named_tests._fixed_tiny_overflow_failure_bytes()
                    write.assert_called_once_with(expected)
                    public = json.loads(expected)
                    output.assert_called_once_with(json.dumps({"status": public["status"], "failure_code": public["failure_code"]}, sort_keys=True))
                    self.assertLessEqual(len(expected), cap)


if __name__ == "__main__":
    unittest.main()
