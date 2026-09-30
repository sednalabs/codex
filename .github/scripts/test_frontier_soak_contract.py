#!/usr/bin/env python3
"""Check and emit the closed hosted frontier/repeatability matrix."""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Any
from unittest import mock


PLAN_SHA256 = "8a8c5412e5c486246822954165593eb821b5860a3a57c26534ce0ba47300c93c"
HARNESS_BASE_SHA = "a32e5c594e6759185c08fcbd75b0e9f6fdee4f1c"
PRODUCT_SHA = "8272e3951cc5189070ec8ea867fba0ff6b2bb8a9"
COMPARISON_BASE_SHA = "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7"
RUNNER = "ubuntu-24.04"
EXPECTED_PATHS = {
    ".github/validation-frontier-soak.json",
    ".github/workflows/validation-named-tests.yml",
    ".github/workflows/_validation-named-tests.yml",
    ".github/scripts/test_frontier_soak_contract.py",
}
ROOT = Path(__file__).resolve().parents[2]
MANIFEST_PATH = ROOT / ".github" / "validation-frontier-soak.json"
REPETITION_RE = re.compile(r"^[13]$")


def read_plan() -> dict[str, Any]:
    plan = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))
    canonical = json.dumps(plan, ensure_ascii=False, separators=(",", ":")).encode(
        "utf-8"
    )
    if hashlib.sha256(canonical).hexdigest() != PLAN_SHA256:
        raise ValueError("frontier plan does not match the frozen plan digest")
    return plan


def read_target_catalog(target_root: Path) -> dict[tuple[str, str, str], dict[str, Any]]:
    payload = json.loads(
        (target_root / ".github" / "validation-named-tests.json").read_text(
            encoding="utf-8"
        )
    )
    if payload.get("schema_version") != "rust-tests-command-manifest-v1":
        raise ValueError("product named-test catalog schema is unsupported")
    if not isinstance(payload.get("targets"), list) or not payload["targets"]:
        raise ValueError("product named-test catalog must contain target rows")
    rows: dict[tuple[str, str, str], dict[str, Any]] = {}
    for row in payload.get("targets", []):
        package = row.get("package")
        target_kind = row.get("target_kind")
        target = row.get("target", "")
        if not isinstance(package, str) or not re.fullmatch(
            r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$", package
        ):
            raise ValueError("product catalog package is not a safe Cargo name")
        if target_kind not in ("lib", "integration"):
            raise ValueError("product catalog target kind is not supported")
        if target_kind == "lib" and target != "":
            raise ValueError("product catalog lib target must be empty")
        if target_kind == "integration" and (
            not isinstance(target, str)
            or not re.fullmatch(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$", target)
        ):
            raise ValueError("product catalog integration target is unsafe")
        command = ["cargo", "test", "--locked", "-p", package]
        if target_kind == "lib":
            command.append("--lib")
        else:
            command.extend(["--test", target])
        if row.get("inventory_argv") != [*command, "--", "--list"]:
            raise ValueError("product catalog inventory argv is not closed")
        if row.get("execution_argv") != [*command, "--", "--test-threads=1"]:
            raise ValueError("product catalog execution argv is not closed")
        profiles = row.get("profiles")
        if (
            not isinstance(profiles, list)
            or not profiles
            or not set(profiles) <= {"rust_minimal", "rust_integration"}
        ):
            raise ValueError("product catalog profiles are not in the allowlist")
        key = (package, target_kind, target)
        if key in rows:
            raise ValueError("product named-test catalog contains a duplicate target")
        rows[key] = row
    return rows


def target_root_from_env() -> Path:
    value = os.environ.get("VALIDATION_TARGET_ROOT", "")
    if not value:
        raise ValueError("VALIDATION_TARGET_ROOT must identify the exact T checkout")
    return Path(value).resolve()


def current_sha(path: Path) -> str:
    completed = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=path,
        text=True,
        capture_output=True,
        check=False,
        shell=False,
    )
    if completed.returncode != 0:
        raise ValueError(f"cannot resolve checkout identity at {path}")
    return completed.stdout.strip()


def build_matrix(target_root: Path, run_id: str) -> dict[str, list[dict[str, Any]]]:
    plan = read_plan()
    catalog = read_target_catalog(target_root)
    include: list[dict[str, Any]] = []
    for lane in plan["lanes"]:
        key = (lane["package"], lane["target_kind"], lane["target"])
        target = catalog.get(key)
        if target is None or plan["profile"] not in target["profiles"]:
            raise ValueError(f"frontier lane target is not enabled in T: {lane['id']}")
        tests = lane["tests"]
        if not tests or len(tests) != len(set(tests)):
            raise ValueError(f"frontier lane selectors must be nonempty and unique: {lane['id']}")
        if lane["repetitions"] not in (1, 3):
            raise ValueError(f"frontier lane repetition count is unsupported: {lane['id']}")
        request = {
            "schema_version": "rust-tests-v1",
            "profile": plan["profile"],
            "package": lane["package"],
            "target_kind": lane["target_kind"],
            "target": lane["target"],
            "execution_mode": plan["execution_mode"],
            "tests": tests,
        }
        include.append(
            {
                "lane_id": lane["id"],
                "repetitions": str(lane["repetitions"]),
                "artifact_name": f"validation-frontier-{run_id}-{lane['id']}",
                "request_json": json.dumps(
                    request, ensure_ascii=False, separators=(",", ":")
                ),
            }
        )
    return {"include": include}


def run_frontier_observation(
    runner: list[str],
    result_path: Path,
    output_path: Path,
    iteration: int,
    repetitions: int,
    artifact_name: str,
    manifest_sha256: str,
    catalog_sha256: str,
) -> bool:
    """Run one fresh process and persist only its own result document."""

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.unlink(missing_ok=True)
    result_path.unlink(missing_ok=True)
    started_ns = time.perf_counter_ns()
    try:
        completed = subprocess.run(runner, check=False, shell=False)
        return_code: int | None = completed.returncode
    except OSError:
        return_code = None
    elapsed_ns = time.perf_counter_ns() - started_ns

    payload: dict[str, Any]
    result_document_status = "valid"
    try:
        loaded = json.loads(result_path.read_text(encoding="utf-8"))
        if (
            not isinstance(loaded, dict)
            or loaded.get("schema_version") != "rust-tests-v1"
        ):
            raise ValueError("result document has an unsupported schema")
        payload = loaded
    except FileNotFoundError:
        result_document_status = "missing"
        payload = {}
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, ValueError):
        result_document_status = "invalid"
        payload = {}

    if result_document_status != "valid":
        request: dict[str, Any] = {}
        raw_request = os.environ.get("RUST_TEST_REQUEST_JSON", "")
        try:
            decoded = json.loads(raw_request)
            if isinstance(decoded, dict):
                request = decoded
        except json.JSONDecodeError:
            pass
        identity = {
            "harness_sha": os.environ.get("VALIDATION_HARNESS_SHA", ""),
            "base_ref": os.environ.get("VALIDATION_BASE_REF", ""),
            "base_sha": os.environ.get("VALIDATION_BASE_SHA", ""),
            "target_sha": os.environ.get("VALIDATION_TARGET_SHA", ""),
            "run_id": os.environ.get("GITHUB_RUN_ID", ""),
        }
        payload = {
            "schema_version": "rust-tests-v1",
            "status": "failure",
            "failure_code": f"result_document_{result_document_status}",
            "message": "this iteration produced no fresh valid named-test result",
            "request": request,
            "request_fingerprint": hashlib.sha256(
                json.dumps(request, sort_keys=True, separators=(",", ":")).encode(
                    "utf-8"
                )
            ).hexdigest(),
            "identity": identity,
            "tests": [],
        }
    payload["frontier_iteration"] = {
        "index": iteration,
        "count": repetitions,
        "lane_artifact_name": artifact_name,
        "manifest_sha256": manifest_sha256,
        "target_catalog_sha256": catalog_sha256,
        "elapsed_ns": elapsed_ns,
        "timer": "time.perf_counter_ns",
        "result_document_status": result_document_status,
        "runner_exit_code": return_code,
    }
    output_path.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    return (
        return_code != 0
        or result_document_status != "valid"
        or payload.get("status") != "success"
    )


class FrontierSoakContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.plan = read_plan()
        cls.target_root = target_root_from_env()
        cls.catalog = read_target_catalog(cls.target_root)

    def test_frozen_h_b_t_identities(self) -> None:
        self.assertEqual(self.plan["harness_base_sha"], HARNESS_BASE_SHA)
        self.assertEqual(self.plan["product_sha"], PRODUCT_SHA)
        self.assertEqual(self.plan["comparison_base_sha"], COMPARISON_BASE_SHA)
        self.assertEqual(os.environ.get("VALIDATION_TARGET_SHA"), PRODUCT_SHA)
        self.assertEqual(os.environ.get("VALIDATION_BASE_SHA"), COMPARISON_BASE_SHA)
        self.assertEqual(os.environ.get("VALIDATION_HARNESS_BASE_SHA"), HARNESS_BASE_SHA)
        self.assertEqual(
            current_sha(ROOT), os.environ.get("VALIDATION_HARNESS_SHA")
        )
        self.assertEqual(current_sha(self.target_root), PRODUCT_SHA)

    def test_closed_lane_shape_and_selector_counts(self) -> None:
        self.assertEqual(self.plan["schema"], "codex-frontier-repeatability-v2")
        lanes = self.plan["lanes"]
        self.assertEqual(len(lanes), 8)
        self.assertEqual(sum(len(lane["tests"]) for lane in lanes), 29)
        self.assertEqual(sum(lane["repetitions"] for lane in lanes), 16)
        self.assertEqual(sum(lane["repetitions"] == 3 for lane in lanes), 4)
        self.assertEqual(sum(lane["repetitions"] == 1 for lane in lanes), 4)
        self.assertEqual(len({lane["id"] for lane in lanes}), 8)
        self.assertEqual(self.plan["runner"], RUNNER)
        self.assertFalse(self.plan["matrix_fail_fast"])
        self.assertEqual(self.plan["max_parallel"], 3)
        self.assertEqual(self.plan["execution_mode"], "exact_continue")
        self.assertTrue(self.plan["no_global_capacity_terminal_dependency"])
        self.assertEqual(set(self.plan["harness_paths"]), EXPECTED_PATHS)

    def test_every_lane_joins_the_t_catalog(self) -> None:
        for lane in self.plan["lanes"]:
            with self.subTest(lane=lane["id"]):
                key = (lane["package"], lane["target_kind"], lane["target"])
                row = self.catalog.get(key)
                self.assertIsNotNone(row)
                self.assertIn(self.plan["profile"], row["profiles"])
                self.assertTrue(lane["tests"])
                self.assertEqual(len(lane["tests"]), len(set(lane["tests"])))

    def test_matrix_requests_are_exact_and_bounded(self) -> None:
        matrix = build_matrix(self.target_root, os.environ.get("GITHUB_RUN_ID", "test"))
        self.assertEqual(len(matrix["include"]), 8)
        self.assertEqual(
            len({row["artifact_name"] for row in matrix["include"]}),
            len(matrix["include"]),
        )
        lane_by_id = {lane["id"]: lane for lane in self.plan["lanes"]}
        for row in matrix["include"]:
            with self.subTest(lane=row["lane_id"]):
                lane = lane_by_id[row["lane_id"]]
                request = json.loads(row["request_json"])
                self.assertEqual(request["schema_version"], "rust-tests-v1")
                self.assertEqual(request["execution_mode"], "exact_continue")
                self.assertEqual(request["profile"], "rust_integration")
                self.assertEqual(request["package"], lane["package"])
                self.assertEqual(request["target_kind"], lane["target_kind"])
                self.assertEqual(request["target"], lane["target"])
                self.assertEqual(request["tests"], lane["tests"])
                self.assertRegex(row["repetitions"], REPETITION_RE)
                self.assertIn(row["lane_id"], row["artifact_name"])

    def test_registered_workflow_keeps_closed_named_and_frontier_suites(self) -> None:
        workflow = (ROOT / ".github/workflows/validation-named-tests.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("workflow_dispatch:", workflow)
        self.assertIn("default: named", workflow)
        self.assertIn("- frontier_soak", workflow)
        self.assertIn("inputs.suite == 'named'", workflow)
        self.assertIn("inputs.suite == 'frontier_soak'", workflow)
        self.assertIn("request_json must be empty for frontier_soak", workflow)
        self.assertIn("profile must be rust_integration for frontier_soak", workflow)
        self.assertIn("runs-on: ubuntu-24.04", workflow)
        self.assertIn("fail-fast: false", workflow)
        self.assertIn("max-parallel: 3", workflow)
        self.assertNotIn("self-hosted", workflow)
        self.assertIn("uses: ./.github/workflows/_validation-named-tests.yml", workflow)
        self.assertIn("contents: read", workflow)
        self.assertNotIn("contents: write", workflow)

    def test_reusable_workflow_keeps_legacy_single_pass_default(self) -> None:
        workflow = (ROOT / ".github/workflows/_validation-named-tests.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn('default: "1"', workflow)
        self.assertIn('"${VALIDATION_REPETITIONS}" != "1"', workflow)
        self.assertIn('"${VALIDATION_REPETITIONS}" != "3"', workflow)
        self.assertIn("run_frontier_observation(", workflow)
        self.assertIn("iteration-1.json", workflow)
        self.assertIn("iteration-2.json", workflow)
        self.assertIn("iteration-3.json", workflow)
        helper = Path(__file__).read_text(encoding="utf-8")
        self.assertIn("time.perf_counter_ns()", helper)
        self.assertIn("result_path.unlink(missing_ok=True)", helper)
        self.assertIn('f"result_document_{result_document_status}"', helper)


class FrontierObservationFreshness(unittest.TestCase):
    def test_second_missing_result_cannot_reuse_first_iteration(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result_path = root / "rust-tests-v1-results.json"
            first_output = root / "frontier-results" / "iteration-1.json"
            second_output = root / "frontier-results" / "iteration-2.json"
            original = {
                "schema_version": "rust-tests-v1",
                "status": "success",
                "request_fingerprint": "first-iteration-only",
                "tests": [{"name": "suite::first", "status": "success"}],
            }
            calls = 0

            def runner_side_effect(
                *_args: Any, **_kwargs: Any
            ) -> subprocess.CompletedProcess[str]:
                nonlocal calls
                calls += 1
                if calls == 1:
                    result_path.write_text(json.dumps(original), encoding="utf-8")
                    return subprocess.CompletedProcess(["runner"], 0)
                return subprocess.CompletedProcess(["runner"], 1)

            with mock.patch.object(subprocess, "run", side_effect=runner_side_effect):
                first_failed = run_frontier_observation(
                    ["runner"],
                    result_path,
                    first_output,
                    1,
                    3,
                    "lane",
                    "plan",
                    "catalog",
                )
                second_failed = run_frontier_observation(
                    ["runner"],
                    result_path,
                    second_output,
                    2,
                    3,
                    "lane",
                    "plan",
                    "catalog",
                )

            second = json.loads(second_output.read_text(encoding="utf-8"))
            self.assertFalse(first_failed)
            self.assertTrue(second_failed)
            self.assertEqual(second["status"], "failure")
            self.assertEqual(second["failure_code"], "result_document_missing")
            self.assertEqual(second["tests"], [])
            self.assertNotEqual(
                second.get("request_fingerprint"), "first-iteration-only"
            )
            self.assertEqual(
                second["frontier_iteration"]["result_document_status"], "missing"
            )
            self.assertEqual(second["frontier_iteration"]["runner_exit_code"], 1)


def main() -> int:
    emit_matrix = sys.argv[1:] == ["--emit-matrix"]
    if sys.argv[1:] and not emit_matrix:
        print("usage: test_frontier_soak_contract.py [--emit-matrix]", file=sys.stderr)
        return 2
    suite = unittest.TestSuite(
        (
            unittest.defaultTestLoader.loadTestsFromTestCase(FrontierSoakContract),
            unittest.defaultTestLoader.loadTestsFromTestCase(FrontierObservationFreshness),
        )
    )
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if not result.wasSuccessful():
        return 1
    if emit_matrix:
        matrix = build_matrix(target_root_from_env(), os.environ.get("GITHUB_RUN_ID", ""))
        print(f"matrix={json.dumps(matrix, ensure_ascii=False, separators=(',', ':'))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
