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

    def _version_display_projection(
        self,
        actual: str,
        expected: str,
        *,
        requested: tuple[str, ...] = (named_tests.VERSION_OUTPUT_SELECTOR,),
        assertion: str = "assertion `left == right` failed",
    ) -> tuple[dict[str, object], dict[str, object]]:
        selector = named_tests.VERSION_OUTPUT_SELECTOR
        output = (
            f"test {selector} ... FAILED\n"
            f"---- {selector} stdout ----\n"
            f"{assertion}\n"
            f'  left: "{actual}"\n'
            f'  right: "{expected}"\n'
            "failures:\n"
        )
        evidence = self._evidence(
            output, requested=requested, known={selector}
        )
        safe = named_tests._safe_failure_evidence(
            evidence, {selector}, set(requested)
        )
        return evidence, safe

    def test_exact_version_selector_projects_only_valid_public_display_pair(self) -> None:
        selector = named_tests.VERSION_OUTPUT_SELECTOR
        actual = "codex Sedna v" + "a" * 40
        expected = "codex Sedna 1.2.3-rc.1+build.7"

        evidence, safe = self._version_display_projection(actual, expected)

        self.assertEqual(
            evidence["blocks"][0]["public_version_display"],
            {"actual": actual, "expected": expected},
        )
        self.assertEqual(
            safe["blocks"][0]["public_version_display"],
            {"actual": actual, "expected": expected},
        )
        self.assertEqual(safe["blocks"][0]["name"], selector)

    def test_version_display_projection_suppresses_invalid_unrequested_and_non_equality_values(self) -> None:
        selector = named_tests.VERSION_OUTPUT_SELECTOR
        valid = "codex Sedna dev"
        overlong = "codex Sedna v1.2.3+" + "x" * 120
        invalid_pairs = [
            ("codex Sedna /invalid", valid, (selector,), "assertion `left == right` failed"),
            (valid, "codex Sedna /invalid", (selector,), "assertion `left == right` failed"),
            (overlong, valid, (selector,), "assertion `left == right` failed"),
            ("codex Sedna v1.02.3", valid, (selector,), "assertion `left == right` failed"),
            ("codex Sedna v" + "A" * 40, valid, (selector,), "assertion `left == right` failed"),
            (valid, valid, (), "assertion `left == right` failed"),
            (valid, valid, (selector,), "assertion `left != right` failed"),
        ]

        for actual, expected, requested, assertion in invalid_pairs:
            with self.subTest(requested=bool(requested), assertion=assertion):
                evidence, safe = self._version_display_projection(
                    actual, expected, requested=requested, assertion=assertion
                )
                encoded_evidence = json.dumps(evidence, sort_keys=True)
                encoded_safe = json.dumps(safe, sort_keys=True)
                self.assertNotIn("public_version_display", encoded_evidence)
                self.assertNotIn("public_version_display", encoded_safe)
                self.assertNotIn(actual, encoded_evidence)
                self.assertNotIn(expected, encoded_evidence)

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


class CoreRuntimeDiagnosticTests(unittest.TestCase):
    startup_name = "session::startup_diagnostic::tests::startup_site_witness_preserves_results_and_rejects_payloads"
    startup_inventory_command = ["cargo", "test", "--locked", "--message-format=json", "-p", "codex-core", "--lib", "--", "--list"]
    startup_command = ["cargo", "test", "--locked", "-p", "codex-core", "--lib", startup_name,
                       "--", "--exact", "--test-threads=1"]
    startup_sites = ("time_provider", "thread_persistence", "local_rollout_path", "agents_md_refresh",
                     "network_proxy", "hooks_new", "mcp_initial_install", "referenced_rollout_materialization")
    producer_names = (
        "test_codex::tests::builder_error_categories_never_include_error_payloads",
        "test_codex::tests::builder_error_categories_cover_complete_payload_free_enum",
    )
    producer_inventory_command = ["cargo", "test", "--locked", "-p", "core_test_support", "--lib", "--", "--list"]
    producer_commands = [["cargo", "test", "--locked", "-p", "core_test_support", "--lib", name,
                          "--", "--exact", "--test-threads=1"] for name in producer_names]
    # Independent complete fixed-Rust discriminant labels, not error payloads.
    io_classes = (
        "not_found", "permission_denied", "connection_refused", "connection_reset",
        "host_unreachable", "network_unreachable", "connection_aborted", "not_connected",
        "addr_in_use", "addr_not_available", "network_down", "broken_pipe",
        "already_exists", "would_block", "not_a_directory", "is_a_directory",
        "directory_not_empty", "read_only_filesystem", "filesystem_loop", "stale_network_file_handle",
        "invalid_input", "invalid_data", "timed_out", "write_zero",
        "storage_full", "not_seekable", "quota_exceeded", "file_too_large",
        "resource_busy", "executable_file_busy", "deadlock", "crosses_devices",
        "too_many_links", "invalid_filename", "argument_list_too_long", "interrupted",
        "unsupported", "unexpected_eof", "out_of_memory", "in_progress",
        "io_other", "io_uncategorized",
    )
    # Independent complete CodexErrKind serde snake_case vocabulary, including Linux-only kinds.
    codex_classes = (
        "codex_turn_aborted", "codex_session_budget_exceeded", "codex_stream", "codex_content_filter",
        "codex_rate_limit_exceeded", "codex_context_window_exceeded", "codex_thread_not_found",
        "codex_agent_limit_reached", "codex_session_configured_not_first_event", "codex_timeout",
        "codex_request_timeout", "codex_spawn", "codex_interrupted", "codex_unexpected_status",
        "codex_invalid_request", "codex_invalid_prompt", "codex_tool_collision", "codex_invalid_image_request",
        "codex_usage_limit_reached", "codex_server_overloaded", "codex_flex_unavailable", "codex_cyber_policy",
        "codex_bio_policy", "codex_misalignment_policy_violation", "codex_response_stream_failed",
        "codex_connection_failed", "codex_quota_exceeded", "codex_usage_not_included",
        "codex_internal_server_error", "codex_retry_limit", "codex_internal_agent_died", "codex_sandbox",
        "codex_landlock_sandbox_executable_not_provided", "codex_unsupported_operation", "codex_refresh_token_failed",
        "codex_fatal", "codex_io", "codex_json", "codex_landlock_ruleset", "codex_landlock_path_fd",
        "codex_tokio_join", "codex_env_var", "unlisted_codex_kind",
    )
    root = Path(named_tests.__file__).resolve().parents[2]
    identity = {"harness_sha": "b" * 40, "base_sha": "c" * 40, "target_sha": "a" * 40,
                "base_ref": "validation/base", "run_id": "123", "run_attempt": "2"}
    request = {"schema_version": named_tests.SCHEMA_VERSION, "profile": "rust_integration",
               "package": "codex-core", "target_kind": "integration", "target": "all",
               "tests": [name for _, name, _ in named_tests.CORE_DIAGNOSTIC_CASES]}

    def _markers(self, case: str) -> str:
        stages = next(stages for key, _, stages in named_tests.CORE_DIAGNOSTIC_CASES if key == case)
        return "\n".join(f"codex-core-runtime-diagnostic-v1 case={case} stage={stage} state={state} error=none"
                         for stage in stages for state in ("entered", "returned"))

    def _builder_phases(self, case: str) -> tuple[str, ...]:
        # Independent fixture inventory from the actual Rust builder call sites.
        phases = ("auto_env_selection", "config_preparation", "linux_runtime_path_resolution",
                  "environment_manager_creation")
        return (*phases, *(("workspace_setup",) if case == "project_docs" else ()),
                "state_database_optional_initialization", "installation_id_resolution",
                "thread_manager_construction", "ordinary_conversation_start")

    def _builder_markers(self, case: str) -> str:
        return "\n".join(f"codex-core-runtime-diagnostic-builder-v1 case={case} phase={phase} state={state} class=none"
                         for phase in self._builder_phases(case) for state in ("entered", "returned"))

    def _producer_markers(self, case: str, builder: str | None = None) -> str:
        outer = self._markers(case).splitlines()
        nested = self._builder_markers(case) if builder is None else builder
        # The nested build_with_auto_env runs inside the outer builder stage.
        return "\n".join([*outer[:5], *nested.splitlines(), *outer[5:]])

    def _builder_error_markers(self, case: str, phase: str, error_class: str) -> str:
        index = self._builder_phases(case).index(phase)
        nested = self._builder_markers(case).splitlines()[:2 * index + 1]
        nested.append(f"codex-core-runtime-diagnostic-builder-v1 case={case} phase={phase} state=error class={error_class}")
        outer_class = error_class if error_class in named_tests.CORE_DIAGNOSTIC_ERRORS - {"none", "other"} else "other"
        return "\n".join([*self._markers(case).splitlines()[:5], *nested,
                          f"codex-core-runtime-diagnostic-v1 case={case} stage=builder state=error error={outer_class}"])

    def _sample(self, case: str, outcome: str = "ok", stderr: str | None = None) -> subprocess.CompletedProcess[str]:
        name = next(name for key, name, _ in named_tests.CORE_DIAGNOSTIC_CASES if key == case)
        output = (f"test {name} ... {outcome}\n"
                  f"test result: ok. {int(outcome == 'ok')} passed; {int(outcome == 'FAILED')} failed; "
                  f"{int(outcome == 'ignored')} ignored; 0 measured; 1 filtered out\n")
        return subprocess.CompletedProcess(named_tests.core_diagnostic_command(name),
                                          101 if outcome == "FAILED" else 0, output,
                                          self._producer_markers(case) if stderr is None else stderr)

    def _producer_control(self, index: int, outcome: str = "ok") -> subprocess.CompletedProcess[str]:
        output = (f"test {self.producer_names[index]} ... {outcome}\n"
                  f"test result: {'FAILED' if outcome == 'FAILED' else 'ok'}. {int(outcome == 'ok')} passed; {int(outcome == 'FAILED')} failed; "
                  f"{int(outcome == 'ignored')} ignored; 0 measured; 2 filtered out\n")
        return subprocess.CompletedProcess(self.producer_commands[index], 101 if outcome == "FAILED" else 0, output, "")

    def _startup_control(self, outcome: str = "ok") -> subprocess.CompletedProcess[str]:
        output = (f"test {self.startup_name} ... {outcome}\n"
                  f"test result: {'FAILED' if outcome == 'FAILED' else 'ok'}. {int(outcome == 'ok')} passed; "
                  f"{int(outcome == 'FAILED')} failed; {int(outcome == 'ignored')} ignored; 0 measured; 1 filtered out\n")
        return subprocess.CompletedProcess(self.startup_command, 101 if outcome == "FAILED" else 0, output, "")

    def _run(self, *, request: dict[str, object] | None = None, extra_env: dict[str, str] | None = None,
             samples: list[object] | None = None, inventory: str | None = None, inventory_exit: int = 0,
             build_exit: int = 0, producer_inventory: str | None = None,
             producer_inventory_response: object | None = None,
             producer_controls: list[object] | None = None, startup_inventory: str | None = None,
             startup_inventory_response: object | None = None,
             startup_control: object | None = None) -> tuple[dict[str, object], list[list[str]]]:
        request = self.request if request is None else request
        env = {variable: self.identity[key] for key, variable in named_tests.VALIDATION_IDENTITY_ENV.items()}
        env.update({named_tests.CORE_DIAGNOSTIC_ONLY_ENV: "true", named_tests.CORE_DIAGNOSTIC_CASE_ENV: "INHERITED_PRIVATE"})
        env.update(extra_env or {})
        record = named_tests.select_target(self.request, named_tests.load_manifest(self.root))
        expected = [["git", "rev-parse", "HEAD"], *[list(argv) for _, argv in named_tests.CORE_RUNTIME_BUILDS],
                    list(record["inventory_argv"]), self.startup_inventory_command, self.startup_command,
                    self.producer_inventory_command, *self.producer_commands,
                    *[named_tests.core_diagnostic_command(name) for _, name, _ in named_tests.CORE_DIAGNOSTIC_CASES]]
        inventory = "\n".join(f"{name}: test" for name in self.request["tests"]) if inventory is None else inventory
        producer_inventory = "\n".join(f"{name}: test" for name in (*self.producer_names,
            "test_codex::tests::custom_tool_call_output_text_returns_output_text")) if producer_inventory is None else producer_inventory
        producer_controls = [self._producer_control(index) for index in range(2)] if producer_controls is None else producer_controls
        startup_inventory = f"{self.startup_name}: test\nsession::tests::other: test\n" if startup_inventory is None else startup_inventory
        startup_control = self._startup_control() if startup_control is None else startup_control
        samples = [self._sample(case) for case, _, _ in named_tests.CORE_DIAGNOSTIC_CASES] if samples is None else samples
        calls = []

        def fake_run(argv: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
            index = len(calls)
            self.assertLess(index, len(expected), "unexpected subprocess call")
            self.assertEqual(argv, expected[index])
            calls.append(argv)
            self.assertEqual({key: kwargs[key] for key in ("text", "capture_output", "check")},
                             {"text": True, "capture_output": True, "check": False})
            self.assertEqual(kwargs["cwd"], self.root if index == 0 else self.root / "codex-rs")
            if index == 0:
                self.assertNotIn("env", kwargs)
                return subprocess.CompletedProcess(argv, 0, "a" * 40 + "\n", "")
            child_env = {key: value for key, value in env.items() if key not in
                         (named_tests.CORE_DIAGNOSTIC_ONLY_ENV, named_tests.CORE_DIAGNOSTIC_CASE_ENV, named_tests.RUNTIME_PREPARATION_ONLY_ENV)}
            child_env["RUST_MIN_STACK"] = "8388608"
            if index >= 9:
                child_env[named_tests.CORE_DIAGNOSTIC_CASE_ENV] = named_tests.CORE_DIAGNOSTIC_CASES[index - 9][0]
            self.assertEqual(kwargs["env"], child_env)
            self.assertFalse(kwargs["shell"])
            if index < 3:
                return subprocess.CompletedProcess(argv, build_exit if index == 1 else 0, "", "PRIVATE_BUILD")
            if index == 3:
                return subprocess.CompletedProcess(argv, inventory_exit, inventory, "PRIVATE_INVENTORY")
            if index == 4:
                returned = startup_inventory_response if startup_inventory_response is not None else subprocess.CompletedProcess(
                    argv, 0, startup_inventory, "PRIVATE_STARTUP_INVENTORY")
            elif index == 5:
                returned = startup_control
            elif index == 6:
                returned = producer_inventory_response if producer_inventory_response is not None else subprocess.CompletedProcess(
                    argv, 0, producer_inventory, "PRIVATE_PRODUCER_INVENTORY")
            elif index < 9:
                returned = producer_controls[index - 7]
            else:
                returned = samples[index - 9]
            if isinstance(returned, OSError):
                raise returned
            return returned

        states = [(False, False), (False, False), (True, True), (True, True)]
        paths = [self.root / "codex-rs" / "target" / "debug" / name for name, _ in named_tests.CORE_RUNTIME_BUILDS] * 2
        with patch.dict(os.environ, env, clear=True), patch.object(named_tests.subprocess, "run", side_effect=fake_run), \
                patch.object(named_tests, "_core_runtime_binary_state", side_effect=states) as binary_state:
            result = named_tests.run_request(request, self.root)
        self.assertEqual([call.args[0] for call in binary_state.call_args_list], paths if calls else [])
        result["identity"] = dict(self.identity)
        return result, calls

    def test_fixed_calls_and_positive_public_sample_are_not_whole_target(self) -> None:
        result, calls = self._run()
        public = json.loads(named_tests.public_artifact_bytes(result))
        self.assertEqual(11, len(calls))
        self.assertEqual(("success", "core_runtime_diagnostic", True, False, "not_attempted", True, []),
                         tuple(public[key] for key in ("status", "result_kind", "diagnostic_only", "full_target_execution", "qualification_status", "sampling_complete", "tests")))
        self.assertEqual([case for case, _, _ in named_tests.CORE_DIAGNOSTIC_CASES], [item["case"] for item in public["diagnostic_samples"]])
        self.assertTrue(all(item["stage_evidence"]["attribution_complete"] for item in public["diagnostic_samples"]))

    def test_producer_controls_use_owning_lib_inventory_and_exact_commands_before_samples(self) -> None:
        result, calls = self._run()
        public = json.loads(named_tests.public_artifact_bytes(result))
        controls = public["producer_controls"]
        self.assertEqual(self.producer_names, named_tests.CORE_DIAGNOSTIC_PRODUCER_TESTS)
        self.assertEqual([self.producer_inventory_command, *self.producer_commands], calls[6:9])
        self.assertEqual(("success", True, 2, 0), tuple(controls[key] for key in
                         ("status", "reconciled", "original_count", "omitted_count")))
        self.assertEqual((3, 3, 3, 0, list(self.producer_names), [], []), tuple(controls["inventory"][key] for key in
                         ("test_count", "original_count", "unique_count", "invalid_name_count", "selected_tests", "missing_tests", "ambiguous_tests")))
        self.assertEqual(2, public["inventory"]["test_count"])
        for index, record in enumerate(controls["tests"]):
            self.assertEqual((self.producer_names[index], self.producer_commands[index], True, True, 0, True, ["ok"], 1, 0, False),
                             tuple(record[key] for key in ("name", "argv", "command_returned", "command_matches", "exit_code",
                                                          "execution_reconciled", "observed_outcomes", "matched_line_count",
                                                          "unexpected_outcome_count", "matched_lines_truncated")))
            self.assertEqual({"passed": 1, "failed": 0, "ignored": 0, "measured": 0, "filtered": 2}, record["result_counts"])

    def test_startup_owning_lib_control_is_real_counted_preflight_not_a_sample(self) -> None:
        result, calls = self._run()
        public = json.loads(named_tests.public_artifact_bytes(result))
        self.assertEqual(calls[4:9], [self.startup_inventory_command, self.startup_command,
                                     self.producer_inventory_command, *self.producer_commands])
        control = public["startup_control"]
        self.assertEqual(("success", True, 1, 0, 2, [self.startup_name]),
                         (control["status"], control["reconciled"], control["original_count"], control["omitted_count"],
                          control["inventory"]["test_count"], control["inventory"]["selected_tests"]))
        record = control["tests"][0]
        self.assertEqual((self.startup_name, self.startup_command, True, True, 0, ["ok"], 1, 0,
                          {"passed": 1, "failed": 0, "ignored": 0, "measured": 0, "filtered": 1}),
                         tuple(record[key] for key in ("name", "argv", "command_returned", "command_matches", "exit_code",
                                                      "observed_outcomes", "matched_line_count", "unexpected_outcome_count", "result_counts")))
        valid = f"{self.startup_name}: test\nsession::tests::other: test\n"
        inventories = [{"startup_inventory": ""}, {"startup_inventory": valid + f"{self.startup_name}: test\n"},
                       {"startup_inventory": valid + "/private/PRIVATE: test\n"},
                       {"startup_inventory_response": OSError("PRIVATE_LAUNCH")},
                       {"startup_inventory_response": subprocess.CompletedProcess(self.startup_inventory_command, 101, valid, "PRIVATE_BODY")},
                       {"startup_inventory_response": subprocess.CompletedProcess(self.producer_inventory_command, 0, valid, "")}]
        passing = self._startup_control()
        executions = [self._startup_control("FAILED"), self._startup_control("ignored"), OSError("PRIVATE_LAUNCH"),
                      subprocess.CompletedProcess(self.startup_command, 101, passing.stdout, "PRIVATE_ERROR"),
                      subprocess.CompletedProcess(self.producer_commands[0], 0, passing.stdout, ""),
                      subprocess.CompletedProcess(self.startup_command, 0, passing.stdout.replace("1 filtered", "0 filtered"), ""),
                      subprocess.CompletedProcess(self.startup_command, 0, passing.stdout + passing.stdout, ""),
                      subprocess.CompletedProcess(self.startup_command, 0, passing.stdout + "test PRIVATE_EXTRA ... ok\n", "")]
        for options, count in [*((item, 5) for item in inventories), *(({"startup_control": item}, 6) for item in executions)]:
            with self.subTest(options=list(options)):
                failed, calls = self._run(**options)
                data = named_tests.public_artifact_bytes(failed)
                projected = json.loads(data)
                self.assertEqual((count, "failure", "core_diagnostic_producer_controls_failed", [], "not-run", False),
                                 (len(calls), projected["status"], projected["failure_code"], projected["diagnostic_samples"],
                                  projected["producer_controls"]["status"], projected["startup_control"]["reconciled"]))
                self.assertNotIn(b"PRIVATE", data)
        mutations = [lambda value: value.pop("startup_control"),
                     lambda value: value["startup_control"].update(status="not-run"),
                     lambda value: value["startup_control"]["inventory"].update(test_count=True),
                     lambda value: value["startup_control"]["inventory"].update(argv=self.producer_inventory_command),
                     lambda value: value["startup_control"]["tests"].clear(),
                     lambda value: value["startup_control"]["tests"].append(value["startup_control"]["tests"][0]),
                     lambda value: value["startup_control"]["tests"][0].update(argv=self.producer_commands[0]),
                     lambda value: value["startup_control"]["tests"][0].update(command_returned=False),
                     lambda value: value["startup_control"]["tests"][0]["result_counts"].update(filtered=0)]
        for mutate in mutations:
            damaged = copy.deepcopy(result)
            mutate(damaged)
            projected = json.loads(named_tests.public_artifact_bytes(damaged))
            self.assertEqual(("failure", "public_projection_incomplete", False),
                             (projected["status"], projected["failure_code"], projected["startup_control"]["reconciled"]))
        failed, _ = self._run(startup_control=self._startup_control("FAILED"))
        failed["startup_control"]["tests"][0].update(body="PRIVATE_BODY", exception="PRIVATE_ERROR", raw_argv=["PRIVATE_ARGV"])
        data = named_tests.public_artifact_bytes(failed)
        env = {variable: self.identity[key] for key, variable in named_tests.VALIDATION_IDENTITY_ENV.items()}
        env[named_tests.CORE_DIAGNOSTIC_ONLY_ENV] = "true"
        with patch.dict(os.environ, env, clear=True), patch.object(sys, "argv", ["runner"]), \
                patch.object(named_tests, "load_request", return_value=(self.request, None)), \
                patch.object(named_tests, "run_request", return_value=failed), \
                patch.object(Path, "write_bytes") as write, patch("builtins.print") as output:
            self.assertEqual(1, named_tests.main())
        write.assert_called_once_with(data)
        output.assert_called_once_with('{"failure_code": "core_diagnostic_producer_controls_failed", "status": "failure"}')
        self.assertNotIn(b"PRIVATE", data)

    def test_startup_inventory_publishes_only_safe_compiler_classes_and_locations(self) -> None:
        stderr = "\n".join((
            "error[E0432]: unresolved import `PRIVATE_IMPORT_NAME`",
            f"  --> {self.root / 'codex-rs/core/src/lib.rs'}:4:5",
            "   |",
            "4  | use PRIVATE_CRATE::private_item;",
            "   |     ^^^^^^^^^^^^^^^^^^^^^^^^^",
            "error[E0599]: no method named `PRIVATE_METHOD_NAME` found",
            "  --> /home/runner/.cargo/registry/src/private-dependency/src/lib.rs:9:2",
            "error[E0609]: no field `PRIVATE_FIELD_NAME` on this value",
            "  --> /tmp/private/codex-rs/core/src/PRIVATE_TENANT_NAME.rs:7:2",
            "error: could not compile `codex-core` (lib test) due to 3 previous errors",
        ))
        response = subprocess.CompletedProcess(self.startup_inventory_command, 101, "", stderr)
        result, _ = self._run(startup_inventory_response=response)
        data = named_tests.public_artifact_bytes(result, self.root)
        public = json.loads(data)
        compiler = public["startup_control"]["inventory"]["diagnostics"]["compiler"]
        self.assertEqual({
            "error_count": 3,
            "errors": [
                {"class": "unresolved_import", "file": "codex-rs/core/src/lib.rs", "line": 4, "column": 5},
                {"class": "missing_method", "file": "unavailable",
                 "line": None, "column": None},
                {"class": "missing_field", "file": "unavailable", "line": None, "column": None},
            ],
            "omitted_count": 0,
            "unlocated_count": 2,
        }, compiler)
        for private_value in ("PRIVATE_IMPORT_NAME", "PRIVATE_CRATE", "private_item", "PRIVATE_METHOD_NAME",
                              "PRIVATE_FIELD_NAME", "PRIVATE_TENANT_NAME", "/home/runner", ".cargo/registry",
                              "/tmp/private", "could not compile"):
            self.assertNotIn(private_value.encode(), data)

    def test_cargo_json_compiler_diagnostics_project_only_allowlisted_fields(self) -> None:
        records = [
            {
                "reason": "compiler-message",
                "message": {
                    "level": "error",
                    "code": {"code": "E0432", "explanation": "PRIVATE_EXPLANATION"},
                    "message": "PRIVATE_IMPORT_AND_TOKEN token=secret",
                    "rendered": "PRIVATE_RENDERED /home/runner/private.rs",
                    "spans": [{
                        "file_name": str(self.root / "codex-rs/core/src/lib.rs"),
                        "line_start": 6,
                        "column_start": 7,
                        "is_primary": True,
                    }],
                },
            },
            {
                "reason": "compiler-message",
                "message": {
                    "level": "error",
                    "code": {"code": "E0599"},
                    "message": "PRIVATE_DEPENDENCY_METHOD",
                    "spans": [{
                        "file_name": "/home/runner/.cargo/registry/private/src/lib.rs",
                        "line_start": 9,
                        "column_start": 2,
                        "is_primary": True,
                    }],
                },
            },
            {
                "reason": "compiler-message",
                "message": {
                    "level": "error",
                    "code": {"code": "E0609"},
                    "message": "PRIVATE_TENANT_NAME",
                    "spans": [{
                        "file_name": "/tmp/private/codex-rs/core/src/PRIVATE_TENANT_NAME.rs",
                        "line_start": 11,
                        "column_start": 4,
                        "is_primary": True,
                    }],
                },
            },
        ]
        stdout = "\n".join(json.dumps(record) for record in records)
        response = subprocess.CompletedProcess(self.startup_inventory_command, 101, stdout, "PRIVATE_CARGO_SUMMARY")
        result, _ = self._run(startup_inventory_response=response)
        data = named_tests.public_artifact_bytes(result, self.root)
        public = json.loads(data)
        compiler = public["startup_control"]["inventory"]["diagnostics"]["compiler"]
        self.assertEqual({
            "error_count": 3,
            "errors": [
                {"class": "unresolved_import", "file": "codex-rs/core/src/lib.rs", "line": 6, "column": 7},
                {"class": "missing_method", "file": "unavailable", "line": None, "column": None},
                {"class": "missing_field", "file": "unavailable", "line": None, "column": None},
            ],
            "omitted_count": 0,
            "unlocated_count": 2,
        }, compiler)
        for private_value in ("PRIVATE_EXPLANATION", "PRIVATE_IMPORT_AND_TOKEN", "token=secret",
                              "PRIVATE_RENDERED", "/home/runner", ".cargo/registry",
                              "PRIVATE_DEPENDENCY_METHOD", "PRIVATE_TENANT_NAME", "/tmp/private",
                              "PRIVATE_CARGO_SUMMARY"):
            self.assertNotIn(private_value.encode(), data)

    def test_startup_sites_join_combined_producer_stderr_to_only_the_failed_result(self) -> None:
        result, _ = self._run()
        baseline = json.loads(named_tests.public_artifact_bytes(result))
        self.assertEqual("success", baseline["status"])
        self.assertTrue(all(item["startup_evidence"]["observation_status"] == "absent"
                            and item["startup_evidence"]["execution_matches"] for item in baseline["diagnostic_samples"]))
        self.assertEqual(set(self.startup_sites), named_tests.CORE_STARTUP_SITES)
        for case, _, _ in named_tests.CORE_DIAGNOSTIC_CASES:
            for site in self.startup_sites:
                marker = f"codex-core-runtime-diagnostic-startup-v1 case={case} site={site}"
                stream = self._builder_error_markers(case, "ordinary_conversation_start", "io_other")
                terminal = f"codex-core-runtime-diagnostic-builder-v1 case={case} phase=ordinary_conversation_start state=error"
                stream = stream.replace(terminal, marker + "\n" + terminal)
                samples = [self._sample(key, "FAILED", stream) if key == case else self._sample(key)
                           for key, _, _ in named_tests.CORE_DIAGNOSTIC_CASES]
                failed, calls = self._run(samples=samples)
                data = named_tests.public_artifact_bytes(failed)
                public = json.loads(data)
                sample = next(item for item in public["diagnostic_samples"] if item["case"] == case)
                expected = {"designated_channel": "stderr", "observation_status": "error_site",
                    "records": [{"case": case, "site": site}], "stderr_candidate_count": 1, "invalid_marker_count": 0,
                    "omitted_marker_count": 0, "stdout_ambiguity_count": 0, "projection_omitted_marker_count": 0,
                    "omitted_field_count": 0, "error_site": site, "scope": "failed_result_boundary_only",
                    "cross_stream_chronology": "unknown", "writer_process_identity": "unknown", "execution_matches": True}
                self.assertEqual(expected, sample["startup_evidence"])
                self.assertEqual((11, "failure", False, "not_attempted", 6, "error", "ordinary_conversation_start", "io_other"),
                                 (len(calls), public["status"], public["full_target_execution"], public["qualification_status"],
                                  sample["stage_evidence"]["stderr_candidate_count"], sample["builder_evidence"]["sequence_status"],
                                  sample["builder_evidence"]["terminal_phase"], sample["builder_evidence"]["terminal_class"]))
                self.assertEqual("success", next(item for item in public["diagnostic_samples"] if item["case"] != case)["status"])
                damaged = copy.deepcopy(failed)
                item = next(item for item in damaged["diagnostic_samples"] if item["case"] == case)
                item["observed_outcomes"].append("PRIVATE_OMITTED_OUTCOME")
                projected = named_tests.public_artifact_bytes(damaged)
                rejected = next(item for item in json.loads(projected)["diagnostic_samples"] if item["case"] == case)["startup_evidence"]
                self.assertEqual(("invalid", "", False),
                                 (rejected["observation_status"], rejected["error_site"], rejected["execution_matches"]))
                self.assertNotIn(b"PRIVATE", projected)
                damaged = copy.deepcopy(failed)
                item = next(item for item in damaged["diagnostic_samples"] if item["case"] == case)
                item["startup_evidence"]["records"][0]["payload"] = "PRIVATE_BODY"
                projected = named_tests.public_artifact_bytes(damaged)
                rejected = next(item for item in json.loads(projected)["diagnostic_samples"] if item["case"] == case)["startup_evidence"]
                self.assertEqual(("invalid", "", False),
                                 (rejected["observation_status"], rejected["error_site"], rejected["execution_matches"]))
                self.assertNotIn(b"PRIVATE", projected)
                damaged = copy.deepcopy(failed)
                item = next(item for item in damaged["diagnostic_samples"] if item["case"] == case)
                item.update(exit_code=0, observed_outcomes=["ok"], result_counts={"passed": 1, "failed": 0,
                    "ignored": 0, "measured": 0, "filtered": 1})
                rejected = next(item for item in json.loads(named_tests.public_artifact_bytes(damaged))["diagnostic_samples"]
                                if item["case"] == case)["startup_evidence"]
                self.assertEqual(("invalid", "", False),
                                 (rejected["observation_status"], rejected["error_site"], rejected["execution_matches"]))

    def test_startup_invalid_namespace_caps_projection_and_payloads_cannot_qualify(self) -> None:
        case = "restricted"
        marker = "codex-core-runtime-diagnostic-startup-v1 case=restricted site=agents_md_refresh"
        stream = self._builder_error_markers(case, "ordinary_conversation_start", "io_other")
        terminal = "codex-core-runtime-diagnostic-builder-v1 case=restricted phase=ordinary_conversation_start state=error"
        variants = [("", marker.replace("-v1", "-v2")), ("", marker.replace("agents_md_refresh", "unknown")),
                    ("", marker.replace("restricted", "unknown")), ("", marker.replace("restricted", "project_docs")),
                    ("", marker + "\n" + marker), ("", marker + "\n" + marker.replace("agents_md_refresh", "time_provider")),
                    ("", marker + " state=error"), ("", "prefix " + marker),
                    ("", marker.replace("agents_md_refresh", "refusal")),
                    ("", marker + " PRIVATE_BODY /private/path https://private.invalid github_pat_PRIVATE"),
                    (marker, ""), (marker, marker), ("", "\n".join([marker] * 32))]
        for stdout, witness in variants:
            with self.subTest(stdout=bool(stdout), witnesses=witness.count("startup-")):
                failed_sample = self._sample(case, "FAILED", stream.replace(terminal, witness + "\n" + terminal))
                failed_sample.stdout += stdout
                failed, calls = self._run(samples=[failed_sample, self._sample("project_docs")])
                data = named_tests.public_artifact_bytes(failed)
                public = json.loads(data)
                evidence = public["diagnostic_samples"][0]["startup_evidence"]
                self.assertEqual((11, "failure", "invalid", "", False, "success"),
                                 (len(calls), public["status"], evidence["observation_status"], evidence["error_site"],
                                  evidence["execution_matches"], public["diagnostic_samples"][1]["status"]))
                self.assertLessEqual(len(evidence["records"]), 1)
                self.assertEqual(6, public["diagnostic_samples"][0]["stage_evidence"]["stderr_candidate_count"])
                for canary in (b"PRIVATE", b"/private", b"https://", b"github_pat_", b"stdout_tail", b"stderr_tail",
                               hashlib.sha256(witness.encode()).hexdigest().encode()):
                    self.assertNotIn(canary, data)
        baseline, _ = self._run()
        self.assertEqual("success", json.loads(named_tests.public_artifact_bytes(baseline))["status"])
        mutations = [lambda item: item.pop("startup_evidence"),
                     lambda item: item.update(startup_evidence="PRIVATE_BODY"),
                     lambda item: item["startup_evidence"].update(stderr_candidate_count=True),
                     lambda item: item["startup_evidence"].update(designated_channel="stdout"),
                     lambda item: item["startup_evidence"].update(stdout_ambiguity_count=1),
                     lambda item: item["startup_evidence"].update(omitted_marker_count=1),
                     lambda item: item["startup_evidence"].update(observation_status="error_site", error_site="PRIVATE_SITE"),
                     lambda item: item["startup_evidence"].update(execution_matches=False),
                     lambda item: item["startup_evidence"]["records"].append({"case": "restricted", "site": "agents_md_refresh"})]
        for mutate in mutations:
            damaged = copy.deepcopy(baseline)
            mutate(damaged["diagnostic_samples"][0])
            data = named_tests.public_artifact_bytes(damaged)
            self.assertEqual(("failure", "public_projection_incomplete"),
                             tuple(json.loads(data)[key] for key in ("status", "failure_code")))
            self.assertNotIn(b"PRIVATE", data)

    def test_producer_inventory_rejects_before_control_execution_or_sampling(self) -> None:
        valid = "\n".join(f"{name}: test" for name in self.producer_names)
        variants = [({"producer_inventory": ""}, list(self.producer_names), []),
                    ({"producer_inventory": f"{self.producer_names[0]}: test"}, [self.producer_names[1]], []),
                    ({"producer_inventory": valid + f"\n{self.producer_names[0]}: test"}, [], [self.producer_names[0]]),
                    ({"producer_inventory": valid + "\n/private/PRIVATE_TOKEN: test"}, [], []),
                    ({"producer_inventory": valid + "\n" + "\n".join(f"suite::extra_{index}: test" for index in
                        range(named_tests.MAX_PUBLIC_INVENTORY_NAMES))}, [], []),
                    ({"producer_inventory_response": subprocess.CompletedProcess(self.producer_inventory_command, 101, valid, "PRIVATE_BUILD")}, [], []),
                    ({"producer_inventory_response": subprocess.CompletedProcess(["PRIVATE_ARGV"], 0, valid, "")}, [], []),
                    ({"producer_inventory_response": OSError("PRIVATE_LAUNCH github_pat_PRIVATE")}, list(self.producer_names), [])]
        for options, missing, ambiguous in variants:
            with self.subTest(options=list(options)):
                result, calls = self._run(**options)
                data = named_tests.public_artifact_bytes(result)
                public = json.loads(data)
                controls = public["producer_controls"]
                self.assertEqual((7, "failure", "core_diagnostic_producer_controls_failed", False, [], []),
                                 (len(calls), public["status"], public["failure_code"], public["sampling_complete"],
                                  public["diagnostic_samples"], controls["tests"]))
                self.assertEqual(("failure", self.producer_inventory_command, missing, ambiguous),
                                 tuple(controls["inventory"][key] for key in ("status", "argv", "missing_tests", "ambiguous_tests")))
                self.assertFalse(controls["reconciled"])
                for canary in (b"PRIVATE", b"/private", b"github_pat_", b"suite::extra_", b"stdout_tail", b"stderr_tail"):
                    self.assertNotIn(canary, data)

    def test_producer_execution_red_keeps_second_control_but_never_samples(self) -> None:
        valid = self._producer_control(0)
        variants = [self._producer_control(0, "FAILED"), self._producer_control(0, "ignored"),
                    OSError("PRIVATE_LAUNCH /private/path"),
                    subprocess.CompletedProcess(self.producer_commands[0], 101, valid.stdout, "PRIVATE_EXCEPTION"),
                    subprocess.CompletedProcess(["PRIVATE_ARGV"], 0, valid.stdout, ""),
                    subprocess.CompletedProcess(self.producer_commands[0], 0, valid.stdout.replace("2 filtered", "1 filtered"), ""),
                    subprocess.CompletedProcess(self.producer_commands[0], 0, valid.stdout.replace("1 passed", "2 passed"), ""),
                    subprocess.CompletedProcess(self.producer_commands[0], 0, "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out", ""),
                    subprocess.CompletedProcess(self.producer_commands[0], 0, valid.stdout + f"test {self.producer_names[0]} ... ok\n", ""),
                    subprocess.CompletedProcess(self.producer_commands[0], 0, valid.stdout + "test PRIVATE_UNEXPECTED ... ok\n", ""),
                    subprocess.CompletedProcess(self.producer_commands[0], 0, valid.stdout + valid.stdout, "")]
        for first in variants:
            with self.subTest(first=type(first).__name__):
                result, calls = self._run(producer_controls=[first, self._producer_control(1)])
                data = named_tests.public_artifact_bytes(result)
                public = json.loads(data)
                controls = public["producer_controls"]
                self.assertEqual((9, "failure", "core_diagnostic_producer_controls_failed", False, [], False, "not_attempted"),
                                 (len(calls), public["status"], public["failure_code"], public["sampling_complete"],
                                  public["diagnostic_samples"], public["full_target_execution"], public["qualification_status"]))
                self.assertEqual(["failure", "success"], [item["status"] for item in controls["tests"]])
                self.assertEqual(self.producer_commands, [item["argv"] for item in controls["tests"]])
                self.assertFalse(controls["reconciled"])
                for canary in (b"PRIVATE", b"/private", b"stdout_tail", b"stderr_tail"):
                    self.assertNotIn(canary, data)
        second_failed, calls = self._run(producer_controls=[self._producer_control(0), self._producer_control(1, "FAILED")])
        public = json.loads(named_tests.public_artifact_bytes(second_failed))
        self.assertEqual((9, "failure", [], ["success", "failure"]),
                         (len(calls), public["status"], public["diagnostic_samples"],
                          [item["status"] for item in public["producer_controls"]["tests"]]))

    def test_producer_public_projection_rechecks_material_command_and_count_evidence(self) -> None:
        result, _ = self._run()
        self.assertEqual("success", json.loads(named_tests.public_artifact_bytes(result))["status"])
        mutations = [lambda value: value.pop("producer_controls"),
                     lambda value: value.update(producer_controls="PRIVATE_RECORDS"),
                     lambda value: value["producer_controls"].update(status="not-run"),
                     lambda value: value["producer_controls"]["inventory"].update(test_count=True),
                     lambda value: value["producer_controls"]["inventory"].update(argv=["PRIVATE_ARGV"]),
                     lambda value: value["producer_controls"]["inventory"].update(command_returned=False),
                     lambda value: value["producer_controls"]["inventory"].update(command_matches=False),
                     lambda value: value["producer_controls"]["inventory"].update(exit_code=101),
                     lambda value: value["producer_controls"]["inventory"]["tests"].append(self.producer_names[0]),
                     lambda value: value["producer_controls"]["inventory"]["tests"].append("PRIVATE_UNEXPECTED"),
                     lambda value: value["producer_controls"]["tests"].reverse(),
                     lambda value: value["producer_controls"]["tests"].pop(),
                     lambda value: value["producer_controls"]["tests"].append(value["producer_controls"]["tests"][0]),
                     lambda value: value["producer_controls"]["tests"][0].update(name="PRIVATE_TEST"),
                     lambda value: value["producer_controls"]["tests"][0].update(argv=["PRIVATE_ARGV"]),
                     lambda value: value["producer_controls"]["tests"][0].update(command_matches=False),
                     lambda value: value["producer_controls"]["tests"][0].update(command_returned=False),
                     lambda value: value["producer_controls"]["tests"][0].update(exit_code=True),
                     lambda value: value["producer_controls"]["tests"][0].update(execution_reconciled=False),
                     lambda value: value["producer_controls"]["tests"][0].update(observed_outcomes=["ok", "ok"]),
                     lambda value: value["producer_controls"]["tests"][0].update(matched_line_count=2),
                     lambda value: value["producer_controls"]["tests"][0].update(matched_lines_truncated=True),
                     lambda value: value["producer_controls"]["tests"][0].update(unexpected_outcome_count=1),
                     lambda value: value["producer_controls"]["tests"][0]["result_counts"].update(filtered=1)]
        for mutate in mutations:
            damaged = copy.deepcopy(result)
            mutate(damaged)
            data = named_tests.public_artifact_bytes(damaged)
            public = json.loads(data)
            self.assertEqual(("failure", "public_projection_incomplete", False, "not_attempted"),
                             tuple(public[key] for key in ("status", "failure_code", "full_target_execution", "qualification_status")))
            self.assertFalse(public["producer_controls"]["reconciled"])
            self.assertNotIn(b"PRIVATE", data)

    def test_producer_control_failure_persists_safe_actionable_evidence_and_exits_red(self) -> None:
        failed = self._producer_control(0, "FAILED")
        failed.stdout += "\nPRIVATE_BODY /private/path https://private.invalid github_pat_PRIVATE_TOKEN"
        failed.stderr = "PRIVATE_EXCEPTION"
        result, _ = self._run(producer_controls=[failed, self._producer_control(1)])
        result["producer_controls"]["inventory"].update(raw_output="PRIVATE_INVENTORY", exception="PRIVATE_EXCEPTION")
        result["producer_controls"]["tests"][0].update(raw_output="PRIVATE_BODY", exception="PRIVATE_EXCEPTION", raw_argv=["PRIVATE_ARGV"])
        data = named_tests.public_artifact_bytes(result)
        public = json.loads(data)
        record = public["producer_controls"]["tests"][0]
        self.assertEqual((self.producer_commands[0], 101, ["FAILED"], 1, True), tuple(record[key] for key in
                         ("argv", "exit_code", "observed_outcomes", "matched_line_count", "command_returned")))
        self.assertEqual({"passed": 0, "failed": 1, "ignored": 0, "measured": 0, "filtered": 2}, record["result_counts"])
        for canary in (b"PRIVATE", b"/private", b"https://", b"github_pat_", b"stdout_tail", b"stderr_tail",
                       hashlib.sha256(failed.stdout.encode()).hexdigest().encode(), hashlib.sha256(failed.stderr.encode()).hexdigest().encode()):
            self.assertNotIn(canary, data)
        env = {variable: self.identity[key] for key, variable in named_tests.VALIDATION_IDENTITY_ENV.items()}
        env[named_tests.CORE_DIAGNOSTIC_ONLY_ENV] = "true"
        with patch.dict(os.environ, env, clear=True), patch.object(sys, "argv", ["runner"]), \
                patch.object(named_tests, "load_request", return_value=(self.request, None)), \
                patch.object(named_tests, "run_request", return_value=result), \
                patch.object(Path, "write_bytes") as write, patch("builtins.print") as output:
            self.assertEqual(1, named_tests.main())
        write.assert_called_once_with(data)
        output.assert_called_once_with('{"failure_code": "core_diagnostic_producer_controls_failed", "status": "failure"}')

    def test_closed_mode_tuple_selectors_and_identity_reject_before_any_call(self) -> None:
        variants = [(self.request, {named_tests.CORE_DIAGNOSTIC_ONLY_ENV: "TRUE"}),
                    (self.request, {named_tests.RUNTIME_PREPARATION_ONLY_ENV: "true"}),
                    (self.request, {named_tests.RUNTIME_PREPARATION_ONLY_ENV: "TRUE"}),
                    (self.request, {"VALIDATION_BASE_SHA": "PRIVATE_BAD_IDENTITY"}),
                    ({**self.request, "tests": self.request["tests"][:1]}, {}),
                    ({**self.request, "tests": list(reversed(self.request["tests"]))}, {}),
                    ({**self.request, "tests": [*self.request["tests"], "suite::extra"]}, {}),
                    ({**self.request, "package": "codex-cli", "target_kind": "bin", "target": "codex", "profile": "rust_minimal"}, {})]
        for request, env in variants:
            with self.subTest(request=request, env=env):
                result, calls = self._run(request=request, extra_env=env)
                self.assertEqual(("failure", []), (result["status"], calls))
        with self.assertRaises(ValueError):
            named_tests.core_diagnostic_command("suite::untrusted")

    def test_classifier_rejects_bad_diagnostic_before_v8_route(self) -> None:
        env = {variable: self.identity[key] for key, variable in named_tests.VALIDATION_IDENTITY_ENV.items()}
        env[named_tests.CORE_DIAGNOSTIC_ONLY_ENV] = "true"
        manifest = named_tests.load_manifest(self.root)
        with patch.dict(os.environ, env, clear=True):
            self.assertTrue(named_tests.request_requires_core_runtime(json.dumps(self.request), "rust_integration", manifest))
            self.assertFalse(named_tests.request_requires_core_runtime(json.dumps({**self.request, "tests": ["suite::other"]}), "rust_integration", manifest))

    def test_first_red_ignored_or_launch_failure_retains_second(self) -> None:
        error = "\n".join(self._markers("restricted").splitlines()[:5]) + "\ncodex-core-runtime-diagnostic-v1 case=restricted stage=builder state=error error=permission_denied"
        for first in (self._sample("restricted", "FAILED"), self._sample("restricted", "FAILED", error), self._sample("restricted", "ignored", ""), OSError("PRIVATE_LAUNCH")):
            with self.subTest(first=type(first).__name__):
                result, calls = self._run(samples=[first, self._sample("project_docs")])
                public = json.loads(named_tests.public_artifact_bytes(result))
                self.assertEqual(11, len(calls))
                self.assertEqual("failure", public["status"])
                self.assertEqual("success", public["diagnostic_samples"][1]["status"])
                self.assertEqual(not isinstance(first, OSError), public["sampling_complete"])
                self.assertNotIn(b"PRIVATE_LAUNCH", named_tests.public_artifact_bytes(result))

    def test_build_or_inventory_rejection_runs_no_sample(self) -> None:
        for options, count in (({"build_exit": 101}, 2), ({"inventory_exit": 101}, 4),
                               ({"inventory": ""}, 4), ({"inventory": "\n".join(f"{self.request['tests'][0]}: test" for _ in range(2))}, 4)):
            with self.subTest(options=options):
                result, calls = self._run(**options)
                self.assertEqual(("failure", count, []), (result["status"], len(calls), result["diagnostic_samples"]))

    def test_stderr_order_and_stdout_ambiguity_never_manufacture_attribution(self) -> None:
        valid = self._markers("restricted")
        variants = [(valid, ""), (valid, valid), (valid, valid.replace("case=restricted", "case=project_docs")),
                    (valid, valid.replace("state=entered", "state=error", 1)), ("", valid + "\n" + valid),
                    ("", "\n".join(reversed(valid.splitlines()))), ("", valid.replace("-v1", "-v2")),
                    ("", valid.replace("case=restricted", "case=project_docs"))]
        for stdout, stderr in variants:
            with self.subTest(stdout=bool(stdout), stderr=bool(stderr)):
                evidence = named_tests.core_stage_evidence("restricted", stdout, stderr)
                self.assertFalse(evidence["attribution_complete"])
                self.assertEqual("unknown", evidence["cross_stream_chronology"])
                self.assertEqual("unknown", evidence["writer_process_identity"])
        partial = "\n".join(valid.splitlines()[:5])
        self.assertEqual(("partial", "builder"), tuple(named_tests.core_stage_evidence("restricted", "", partial)[key] for key in ("sequence_status", "last_entered_stage")))
        error = partial + "\ncodex-core-runtime-diagnostic-v1 case=restricted stage=builder state=error error=permission_denied"
        self.assertEqual("permission_denied", named_tests.core_stage_evidence("restricted", "", error)["records"][-1]["error"])

    def test_incomplete_markers_counts_or_outcomes_keep_sample_red(self) -> None:
        for first in (self._sample("restricted", stderr=""), self._sample("restricted", stderr=self._markers("restricted").splitlines()[0]),
                      subprocess.CompletedProcess([], 0, self._sample("restricted").stdout.replace("1 filtered", "0 filtered"), self._producer_markers("restricted")),
                      subprocess.CompletedProcess([], 0, "PRIVATE_PANIC", self._producer_markers("restricted"))):
            result, _ = self._run(samples=[first, self._sample("project_docs")])
            self.assertEqual("failure", json.loads(named_tests.public_artifact_bytes(result))["status"])

    def test_whole_projection_privacy_identity_and_sample_laundering(self) -> None:
        first = self._sample("restricted")
        first.stdout += "\nPRIVATE_BODY /private/secret"
        first.stderr += "\nPRIVATE_PANIC https://private.invalid github_pat_PRIVATE"
        result, _ = self._run(samples=[first, self._sample("project_docs")])
        sample = result["diagnostic_samples"][0]
        sample.update(raw_output="PRIVATE_BODY", exception="PRIVATE_EXCEPTION", argv=["/private/secret"])
        sample["stage_evidence"].update(raw_stderr="github_pat_PRIVATE_CREDENTIAL", process_identity="PRIVATE_PID")
        sample["stage_evidence"]["records"][0]["raw"] = "https://private.invalid/PRIVATE_URL"
        data = named_tests.public_artifact_bytes(result)
        for canary in (b"PRIVATE", b"/private", b"github_pat_", b"https://", b"stdout_tail", b"stderr_tail"):
            self.assertNotIn(canary, data)
        env = {variable: self.identity[key] for key, variable in named_tests.VALIDATION_IDENTITY_ENV.items()}
        env[named_tests.CORE_DIAGNOSTIC_ONLY_ENV] = "true"
        with patch.dict(os.environ, env, clear=True), patch.object(sys, "argv", ["runner"]), \
                patch.object(named_tests, "load_request", return_value=(self.request, None)), \
                patch.object(named_tests, "run_request", return_value=result) as run, \
                patch.object(Path, "write_bytes") as write, patch("builtins.print") as output:
            self.assertEqual(0, named_tests.main())
        run.assert_called_once_with(self.request, Path.cwd().resolve())
        write.assert_called_once_with(data)
        output.assert_called_once_with('{"failure_code": "", "status": "success"}')
        for mutate in (lambda value: value["identity"].update(target_sha="PRIVATE_ID"),
                       lambda value: value.update(full_target_execution=True),
                       lambda value: value["diagnostic_samples"][0]["stage_evidence"].update(stdout_ambiguity_count=1),
                       lambda value: value["diagnostic_samples"].reverse()):
            damaged = copy.deepcopy(result)
            mutate(damaged)
            self.assertEqual("public_projection_incomplete", json.loads(named_tests.public_artifact_bytes(damaged))["failure_code"])
        normal = PublicArtifactBoundaryTests()._minimal()
        normal.update(named_tests.core_diagnostic_fields())
        normal["result_kind"] = "named_tests"
        self.assertEqual("failure", json.loads(named_tests.public_artifact_bytes(normal))["status"])

    def test_diagnostic_overflow_keeps_nonqualification_scope(self) -> None:
        result, _ = self._run()
        names = [f"suite::{index:04}_{'x' * 238}" for index in range(named_tests.MAX_PUBLIC_INVENTORY_NAMES)]
        result["inventory"].update(tests=names, test_count=len(names))
        public = json.loads(named_tests.public_artifact_bytes(result))
        self.assertEqual(("failure", "public_result_overflow", "core_runtime_diagnostic", True, False, "not_attempted"),
                         tuple(public[key] for key in ("status", "failure_code", "result_kind", "diagnostic_only", "full_target_execution", "qualification_status")))

    def test_builder_actual_combined_producer_has_independent_exact_inventories(self) -> None:
        for case, limit in (("restricted", 16), ("project_docs", 18)):
            self.assertEqual(self._builder_phases(case), named_tests.CORE_BUILDER_PHASES[case])
            stream = self._producer_markers(case)
            outer = named_tests.core_stage_evidence(case, "", stream)
            nested = named_tests.core_builder_evidence(case, "", stream)
            self.assertEqual(("complete", 12, 12, 0),
                             (outer["sequence_status"], outer["stderr_candidate_count"], len(outer["records"]), outer["omitted_marker_count"]))
            self.assertEqual(("complete", limit, limit, True),
                             (nested["sequence_status"], nested["stderr_candidate_count"], len(nested["records"]), nested["completed_path"]))
            self.assertTrue(all(item["class"] == "none" for item in nested["records"]))
        result, calls = self._run()
        public = json.loads(named_tests.public_artifact_bytes(result))
        self.assertEqual((11, "success", False, "not_attempted"),
                         (len(calls), public["status"], public["full_target_execution"], public["qualification_status"]))
        self.assertEqual([16, 18], [len(item["builder_evidence"]["records"]) for item in public["diagnostic_samples"]])
        self.assertTrue(all(item["builder_evidence"]["completed_path"] for item in public["diagnostic_samples"]))

    def test_builder_each_result_terminal_is_retained_as_observed_diagnostic_red(self) -> None:
        result_phases = {"auto_env_selection", "config_preparation", "linux_runtime_path_resolution",
                         "workspace_setup", "installation_id_resolution", "ordinary_conversation_start"}
        self.assertEqual(result_phases, named_tests.CORE_BUILDER_RESULT_PHASES)
        for case, _, _ in named_tests.CORE_DIAGNOSTIC_CASES:
            for phase in self._builder_phases(case):
                if phase not in result_phases:
                    continue
                for error_class in ("no_io_cause", "unlisted_io_kind", *self.io_classes, *self.codex_classes):
                    with self.subTest(case=case, phase=phase, error_class=error_class):
                        samples = [self._sample(key, "FAILED", self._builder_error_markers(key, phase, error_class))
                                   if key == case else self._sample(key) for key, _, _ in named_tests.CORE_DIAGNOSTIC_CASES]
                        result, calls = self._run(samples=samples)
                        public = json.loads(named_tests.public_artifact_bytes(result))
                        sample = next(item for item in public["diagnostic_samples"] if item["case"] == case)
                        evidence = sample["builder_evidence"]
                        self.assertEqual((11, "failure", True, "not_attempted", []),
                                         (len(calls), public["status"], public["sampling_complete"], public["qualification_status"], public["tests"]))
                        self.assertEqual(("error", False, phase, error_class),
                                         tuple(evidence[key] for key in ("sequence_status", "completed_path", "terminal_phase", "terminal_class")))
                        self.assertEqual((phase, "error", error_class),
                                         tuple(evidence["records"][-1][key] for key in ("phase", "state", "class")))
                        if error_class in self.codex_classes:
                            self.assertEqual("other", sample["stage_evidence"]["records"][-1]["error"])
                            self.assertEqual((False, False, "unknown", "unknown"),
                                             tuple(evidence[key] for key in ("completed_path", "attribution_complete",
                                                                            "cross_stream_chronology", "writer_process_identity")))
                        self.assertEqual("success", next(item for item in public["diagnostic_samples"] if item["case"] != case)["status"])

    def test_builder_closed_error_classes_do_not_create_self_option_or_infallible_errors(self) -> None:
        classes = {"no_io_cause", "unlisted_io_kind", *self.io_classes, *self.codex_classes}
        self.assertEqual(set(self.io_classes), named_tests.CORE_BUILDER_IO_CLASSES)
        self.assertEqual(classes | {"none"}, named_tests.CORE_BUILDER_CLASSES)
        for error_class in classes:
            stream = self._builder_error_markers("restricted", "config_preparation", error_class)
            evidence = named_tests.core_builder_evidence("restricted", "", stream)
            self.assertEqual(("error", error_class), (evidence["sequence_status"], evidence["terminal_class"]))
        for phase in ("environment_manager_creation", "state_database_optional_initialization", "thread_manager_construction"):
            for error_class in ("no_io_cause", *self.codex_classes):
                stream = self._builder_error_markers("restricted", phase, error_class)
                self.assertEqual("invalid", named_tests.core_builder_evidence("restricted", "", stream)["sequence_status"])
        for error_class in ("none", "other", "unknown", "PRIVATE_ERROR"):
            stream = self._builder_error_markers("restricted", "config_preparation", error_class)
            self.assertFalse(named_tests.core_builder_evidence("restricted", "", stream)["completed_path"])
            self.assertEqual("invalid", named_tests.core_builder_evidence("restricted", "", stream)["sequence_status"])

    def test_builder_codex_unknown_payload_and_refusal_are_isolated_public_negatives(self) -> None:
        stream = self._builder_error_markers("restricted", "ordinary_conversation_start", "codex_fatal")
        baseline, calls = self._run(samples=[self._sample("restricted", "FAILED", stream), self._sample("project_docs")])
        public = json.loads(named_tests.public_artifact_bytes(baseline))
        self.assertEqual((11, "failure", True, False, "not_attempted"),
                         (len(calls), public["status"], public["sampling_complete"], public["full_target_execution"], public["qualification_status"]))
        self.assertEqual(("error", "ordinary_conversation_start", "codex_fatal"),
                         tuple(public["diagnostic_samples"][0]["builder_evidence"][key]
                               for key in ("sequence_status", "terminal_phase", "terminal_class")))
        self.assertEqual("success", public["diagnostic_samples"][1]["status"])
        for invalid in ("codex_unknown_category", "codex_refusal", "codex_sandbox_executable_not_provided",
                        "codex_fatal:PRIVATE_PAYLOAD", "codex_landlock_sandbox_executable_not_provided:PRIVATE_PAYLOAD",
                        "codex_fatal PRIVATE_BODY /private/path https://private.invalid github_pat_PRIVATE"):
            rejected_stream = stream.replace("class=codex_fatal", "class=" + invalid)
            self.assertEqual("partial", named_tests.core_stage_evidence("restricted", "", rejected_stream)["sequence_status"])
            result, calls = self._run(samples=[self._sample("restricted", "FAILED", rejected_stream), self._sample("project_docs")])
            data = named_tests.public_artifact_bytes(result)
            rejected = json.loads(data)
            self.assertEqual((11, "failure", True, False, "not_attempted"),
                             (len(calls), rejected["status"], rejected["sampling_complete"], rejected["full_target_execution"], rejected["qualification_status"]))
            self.assertEqual(("invalid", False, ""), tuple(rejected["diagnostic_samples"][0]["builder_evidence"][key]
                             for key in ("sequence_status", "completed_path", "terminal_class")))
            damaged = copy.deepcopy(baseline)
            damaged["diagnostic_samples"][0]["builder_evidence"]["records"][-1]["class"] = invalid
            projected = named_tests.public_artifact_bytes(damaged)
            self.assertEqual("invalid", json.loads(projected)["diagnostic_samples"][0]["builder_evidence"]["sequence_status"])
            for output in (data, projected):
                for token in (invalid.encode(), b"PRIVATE", b"/private", b"https://", b"github_pat_",
                              hashlib.sha256(invalid.encode()).hexdigest().encode()):
                    self.assertNotIn(token, output)

    def test_builder_entered_only_prefix_reports_incomplete_without_fabricating_error(self) -> None:
        for case, _, _ in named_tests.CORE_DIAGNOSTIC_CASES:
            for index, phase in enumerate(self._builder_phases(case)):
                prefix = "\n".join(self._builder_markers(case).splitlines()[:2 * index + 1])
                evidence = named_tests.core_builder_evidence(case, "", prefix)
                projected = named_tests._safe_core_builder_evidence(case, evidence)
                self.assertEqual(("partial", False, phase, "", ""),
                                 tuple(projected[key] for key in ("sequence_status", "completed_path", "last_entered_phase", "terminal_phase", "terminal_class")))
                self.assertEqual("entered", projected["records"][-1]["state"])

    def test_builder_missing_duplicate_misordered_and_post_error_records_fail_closed(self) -> None:
        for case, _, _ in named_tests.CORE_DIAGNOSTIC_CASES:
            lines = self._builder_markers(case).splitlines()
            error = [line for line in self._builder_error_markers(case, "config_preparation", "no_io_cause").splitlines()
                     if line.startswith("codex-core-runtime-diagnostic-builder-")]
            variants = [lines[:2] + lines[4:], lines[:2] + lines[:2] + lines[2:],
                        lines[2:4] + lines[:2] + lines[4:], list(reversed(lines)),
                        [*error, *lines[4:6]]]
            for nested in variants:
                with self.subTest(case=case, count=len(nested)):
                    stream = self._producer_markers(case, "\n".join(nested))
                    self.assertEqual("invalid", named_tests.core_builder_evidence(case, "", stream)["sequence_status"])
                    self.assertEqual("complete", named_tests.core_stage_evidence(case, "", stream)["sequence_status"])

    def test_builder_zero_and_one_setup_grammars_reject_other_branches(self) -> None:
        for case, _, _ in named_tests.CORE_DIAGNOSTIC_CASES:
            valid = self._builder_markers(case)
            wrong_case_grammar = self._builder_markers("project_docs" if case == "restricted" else "restricted").replace(
                "case=project_docs" if case == "restricted" else "case=restricted", f"case={case}")
            variants = [wrong_case_grammar, valid.replace("phase=ordinary_conversation_start", "phase=resume"),
                        valid.replace("phase=ordinary_conversation_start", "phase=start_with_shell_override")]
            if case == "project_docs":
                setup = [line for line in valid.splitlines() if "phase=workspace_setup " in line]
                variants.append(valid + "\n" + "\n".join(setup))
            for nested in variants:
                stream = self._producer_markers(case, nested)
                self.assertEqual("invalid", named_tests.core_builder_evidence(case, "", stream)["sequence_status"])
                self.assertEqual("complete", named_tests.core_stage_evidence(case, "", stream)["sequence_status"])

    def test_builder_malformed_frame_vocabulary_and_injection_never_become_outer_records(self) -> None:
        valid = self._builder_markers("restricted")
        first = valid.splitlines()[0]
        replacements = [("-builder-v1", "-builder-v2"), ("case=restricted", "case=untrusted"),
                        ("case=restricted", "case=project_docs"),
                        ("phase=auto_env_selection", "phase=untrusted"), ("state=entered", "state=cancelled"),
                        ("class=none", "class=permission_denied"), (" class=none", ""),
                        ("class=none", "class=PRIVATE_CREDENTIAL /private/secret https://private.invalid")]
        variants = [valid.replace(old, new, 1) for old, new in replacements]
        variants.extend([first + " PRIVATE_BODY\n" + valid, "PRIVATE_PREFIX " + first + "\n" + valid,
                         valid + "\ncodex-core-runtime-diagnostic-builder-v1 PRIVATE_BACKTRACE github_pat_PRIVATE"])
        for nested in variants:
            stream = self._producer_markers("restricted", nested)
            outer = named_tests.core_stage_evidence("restricted", "", stream)
            builder = named_tests.core_builder_evidence("restricted", "", stream)
            self.assertEqual(("complete", 12), (outer["sequence_status"], outer["stderr_candidate_count"]))
            self.assertEqual("invalid", builder["sequence_status"])
            self.assertFalse(builder["completed_path"])
            result, _ = self._run(samples=[self._sample("restricted", stderr=stream), self._sample("project_docs")])
            data = named_tests.public_artifact_bytes(result)
            self.assertEqual("failure", json.loads(data)["status"])
            for canary in (b"PRIVATE", b"/private", b"https://", b"github_pat_"):
                self.assertNotIn(canary, data)

    def test_builder_stdout_ambiguity_is_separate_and_never_attests_order_or_pid(self) -> None:
        case = "restricted"
        stream = self._producer_markers(case)
        for stdout, expected_outer, expected_builder in ((self._builder_markers(case), "complete", "invalid"),
                (self._markers(case), "invalid", "complete"), (stream, "invalid", "invalid")):
            outer = named_tests.core_stage_evidence(case, stdout, stream)
            nested = named_tests.core_builder_evidence(case, stdout, stream)
            self.assertEqual((expected_outer, expected_builder), (outer["sequence_status"], nested["sequence_status"]))
            self.assertEqual(("unknown", "unknown"), (nested["cross_stream_chronology"], nested["writer_process_identity"]))
            first = self._sample(case)
            first.stdout += "\n" + stdout
            result, _ = self._run(samples=[first, self._sample("project_docs")])
            self.assertEqual("failure", json.loads(named_tests.public_artifact_bytes(result))["status"])
        joined = self._markers(case).splitlines()[0] + " " + self._builder_markers(case).splitlines()[0]
        self.assertEqual("invalid", named_tests.core_stage_evidence(case, "", stream + "\n" + joined)["sequence_status"])
        self.assertEqual("invalid", named_tests.core_builder_evidence(case, "", stream + "\n" + joined)["sequence_status"])

    def test_builder_cannot_replace_missing_outer_frames_or_relax_outer_twelve_record_cap(self) -> None:
        for case, _, _ in named_tests.CORE_DIAGNOSTIC_CASES:
            stream = self._producer_markers(case)
            variants = [self._builder_markers(case), "\n".join(stream.splitlines()[:-1]),
                        stream + "\n" + self._markers(case).splitlines()[0], stream.replace("diagnostic-v1", "diagnostic-v2")]
            for stderr in variants:
                outer = named_tests.core_stage_evidence(case, "", stderr)
                self.assertFalse(outer["attribution_complete"])
                self.assertLessEqual(len(outer["records"]), 12)
                self.assertTrue(named_tests.core_builder_evidence(case, "", stderr)["completed_path"])
                samples = [self._sample(key, stderr=stderr) if key == case else self._sample(key)
                           for key, _, _ in named_tests.CORE_DIAGNOSTIC_CASES]
                result, _ = self._run(samples=samples)
                self.assertEqual("failure", json.loads(named_tests.public_artifact_bytes(result))["status"])

    def test_builder_extra_records_are_omitted_truthfully_without_increasing_outer_candidates(self) -> None:
        for case, limit in (("restricted", 16), ("project_docs", 18)):
            nested = self._builder_markers(case)
            stream = self._producer_markers(case, nested + "\n" + nested)
            evidence = named_tests.core_builder_evidence(case, "", stream)
            self.assertEqual(("invalid", limit, limit * 2, limit),
                             (evidence["sequence_status"], len(evidence["records"]), evidence["stderr_candidate_count"], evidence["omitted_marker_count"]))
            projected = named_tests._safe_core_builder_evidence(case, evidence)
            self.assertEqual(("invalid", False, limit),
                             (projected["sequence_status"], projected["completed_path"], projected["omitted_marker_count"]))
            self.assertEqual(12, named_tests.core_stage_evidence(case, "", stream)["stderr_candidate_count"])

    def test_builder_missing_evidence_and_launch_failure_stay_red_with_second_case_retained(self) -> None:
        for first in (self._sample("restricted", stderr=self._markers("restricted")), OSError("PRIVATE_ERROR /private/path")):
            result, calls = self._run(samples=[first, self._sample("project_docs")])
            data = named_tests.public_artifact_bytes(result)
            public = json.loads(data)
            evidence = public["diagnostic_samples"][0]["builder_evidence"]
            self.assertEqual((11, "failure", "missing", False, [], ""),
                             (len(calls), public["status"], evidence["sequence_status"], evidence["completed_path"], evidence["records"], evidence["terminal_class"]))
            self.assertEqual("success", public["diagnostic_samples"][1]["status"])
            self.assertEqual(not isinstance(first, OSError), public["sampling_complete"])
            self.assertNotIn(b"PRIVATE", data)

    def test_builder_whole_public_projection_rechecks_material_types_counts_and_grammar(self) -> None:
        result, _ = self._run()
        self.assertEqual("success", json.loads(named_tests.public_artifact_bytes(result))["status"])
        mutations = [lambda item: item.pop("builder_evidence"),
                     lambda item: item.update(builder_evidence="PRIVATE_RECORDS"),
                     lambda item: item["builder_evidence"].update(records="PRIVATE_RECORDS"),
                     lambda item: item["builder_evidence"].update(stderr_candidate_count=True),
                     lambda item: item["builder_evidence"].update(omitted_marker_count=1),
                     lambda item: item["builder_evidence"].update(invalid_marker_count=1),
                     lambda item: item["builder_evidence"].update(stdout_ambiguity_count=1),
                     lambda item: item["builder_evidence"].update(designated_channel="stdout"),
                     lambda item: item["builder_evidence"].update(sequence_status="error"),
                     lambda item: item["builder_evidence"].update(completed_path=1),
                     lambda item: item["builder_evidence"].update(attribution_complete=False),
                     lambda item: item["builder_evidence"]["records"][0].update(case="project_docs"),
                     lambda item: item["builder_evidence"]["records"][0].update(phase="PRIVATE_PATH"),
                     lambda item: item["builder_evidence"]["records"][0].update(state="error", **{"class": "none"}),
                     lambda item: item["builder_evidence"]["records"][0].update(**{"class": "other"}),
                     lambda item: item["builder_evidence"]["records"].reverse()]
        for mutate in mutations:
            damaged = copy.deepcopy(result)
            mutate(damaged["diagnostic_samples"][0])
            data = named_tests.public_artifact_bytes(damaged)
            public = json.loads(data)
            self.assertEqual(("failure", "public_projection_incomplete", False, "not_attempted"),
                             tuple(public[key] for key in ("status", "failure_code", "full_target_execution", "qualification_status")))
            self.assertFalse(public["diagnostic_samples"][0]["builder_evidence"]["completed_path"])
            self.assertNotIn(b"PRIVATE", data)

    def test_builder_complete_persisted_bytes_omit_private_fields_and_rederive_attribution(self) -> None:
        result, _ = self._run()
        canary = "PRIVATE_BODY /private/error https://private.invalid github_pat_PRIVATE_CREDENTIAL BACKTRACE"
        evidence = result["diagnostic_samples"][0]["builder_evidence"]
        evidence.update(raw_stdout=canary, raw_stderr=canary, error=canary, exception=canary, path=canary,
                        terminal_phase=canary, terminal_class=canary, last_entered_phase=canary,
                        cross_stream_chronology=canary, writer_process_identity=canary)
        evidence["records"][0].update(body=canary, cause=canary, argv=[canary])
        data = named_tests.public_artifact_bytes(result)
        public = json.loads(data)
        self.assertEqual("success", public["status"])
        projected = public["diagnostic_samples"][0]["builder_evidence"]
        self.assertEqual(("", "", "ordinary_conversation_start", "unknown", "unknown"),
                         tuple(projected[key] for key in ("terminal_phase", "terminal_class", "last_entered_phase", "cross_stream_chronology", "writer_process_identity")))
        for token in (b"PRIVATE", b"/private", b"https://", b"github_pat_", b"BACKTRACE",
                      hashlib.sha256(canary.encode()).hexdigest().encode()):
            self.assertNotIn(token, data)
        env = {variable: self.identity[key] for key, variable in named_tests.VALIDATION_IDENTITY_ENV.items()}
        env[named_tests.CORE_DIAGNOSTIC_ONLY_ENV] = "true"
        with patch.dict(os.environ, env, clear=True), patch.object(sys, "argv", ["runner"]), \
                patch.object(named_tests, "load_request", return_value=(self.request, None)) as load, \
                patch.object(named_tests, "run_request", return_value=result) as run, \
                patch.object(Path, "write_bytes") as write, patch("builtins.print") as output:
            self.assertEqual(0, named_tests.main())
        load.assert_called_once_with()
        run.assert_called_once_with(self.request, Path.cwd().resolve())
        write.assert_called_once_with(data)
        output.assert_called_once_with('{"failure_code": "", "status": "success"}')

    def test_builder_red_public_bytes_drive_main_exit_and_bounded_overflow_scope(self) -> None:
        result, _ = self._run(samples=[self._sample("restricted", "FAILED", self._builder_error_markers(
            "restricted", "ordinary_conversation_start", "no_io_cause")), self._sample("project_docs")])
        data = named_tests.public_artifact_bytes(result)
        env = {variable: self.identity[key] for key, variable in named_tests.VALIDATION_IDENTITY_ENV.items()}
        env[named_tests.CORE_DIAGNOSTIC_ONLY_ENV] = "true"
        with patch.dict(os.environ, env, clear=True), patch.object(sys, "argv", ["runner"]), \
                patch.object(named_tests, "load_request", return_value=(self.request, None)) as load, \
                patch.object(named_tests, "run_request", return_value=result) as run, \
                patch.object(Path, "write_bytes") as write, patch("builtins.print") as output:
            self.assertEqual(1, named_tests.main())
        load.assert_called_once_with()
        run.assert_called_once_with(self.request, Path.cwd().resolve())
        write.assert_called_once_with(data)
        output.assert_called_once_with('{"failure_code": "core_diagnostic_sample_failed", "status": "failure"}')
        with patch.object(named_tests, "MAX_PUBLIC_RESULT_BYTES", 4000):
            overflow_data = named_tests.public_artifact_bytes(result)
        overflow = json.loads(overflow_data)
        self.assertLessEqual(len(overflow_data), 4000)
        self.assertEqual(("failure", "public_result_overflow", True, False, "not_attempted", []),
                         tuple(overflow[key] for key in ("status", "failure_code", "diagnostic_only", "full_target_execution", "qualification_status", "diagnostic_samples")))


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
