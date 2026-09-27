#!/usr/bin/env python3
"""Run a typed, exact Rust test request on a hosted validation runner.

The request is deliberately narrower than a shell command.  It names a Cargo
package, one allowlisted target kind, and fully-qualified test names.  The
runner inventories the target first and refuses to execute when a requested
name is missing or ambiguous.  This prevents a successful ``cargo test`` with
zero selected tests from being mistaken for coverage.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
from pathlib import Path
from typing import Any


SCHEMA_VERSION = "rust-tests-v1"
PACKAGE_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
TARGET_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
TEST_RE = re.compile(r"^[A-Za-z0-9_:.\-]{1,255}$")
ALLOWED_PROFILES = {"rust_minimal", "rust_integration"}
ALLOWED_TARGET_KINDS = {"lib", "integration"}
MAX_TESTS = 64
MAX_REQUEST_CHARS = 32768
MAX_DIAGNOSTIC_CHARS = 4096
TEST_RESULT_RE = re.compile(
    r"test result:\s+\w+\.\s+"
    r"(?P<passed>\d+) passed;\s+"
    r"(?P<failed>\d+) failed;\s+"
    r"(?P<ignored>\d+) ignored;\s+"
    r"(?P<measured>\d+) measured;\s+"
    r"(?P<filtered>\d+) filtered out"
)


def bounded_diagnostic(value: str | None) -> str:
    """Keep failure evidence actionable without duplicating unbounded logs."""

    text = str(value or "").strip()
    if len(text) <= MAX_DIAGNOSTIC_CHARS:
        return text
    return "...[truncated; see hosted job log]...\n" + text[-MAX_DIAGNOSTIC_CHARS:]


def command_diagnostics(completed: subprocess.CompletedProcess[str]) -> dict[str, Any]:
    return {
        "exit_code": completed.returncode,
        "stdout_tail": bounded_diagnostic(completed.stdout),
        "stderr_tail": bounded_diagnostic(completed.stderr),
    }


def test_result_counts(output: str) -> dict[str, int] | None:
    matches = list(TEST_RESULT_RE.finditer(output))
    if len(matches) != 1:
        return None
    match = matches[0]
    return {
        name: int(match.group(name))
        for name in ("passed", "failed", "ignored", "measured", "filtered")
    }


def fail(code: str, message: str) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "status": "failure",
        "failure_code": code,
        "message": message,
        "inventory": {"status": "not-run", "tests": []},
        "tests": [],
    }


def load_request() -> tuple[dict[str, Any] | None, dict[str, Any] | None]:
    raw = os.environ.get("RUST_TEST_REQUEST_JSON", "")
    if len(raw) > MAX_REQUEST_CHARS:
        return None, fail("request_too_large", "request exceeds the hosted size bound")
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        return None, fail("request_invalid_json", f"request could not be decoded: {exc}")
    if not isinstance(payload, dict):
        return None, fail("request_not_object", "request must be a JSON object")
    if payload.get("schema_version") != SCHEMA_VERSION:
        return None, fail("request_schema_unsupported", "unsupported request schema")
    package = payload.get("package")
    target_kind = payload.get("target_kind")
    target = payload.get("target", "")
    profile = payload.get("profile")
    tests = payload.get("tests")
    if not isinstance(package, str) or not PACKAGE_RE.fullmatch(package):
        return None, fail("package_invalid", "package must be a safe Cargo package name")
    if target_kind not in ALLOWED_TARGET_KINDS:
        return None, fail("target_kind_invalid", "target_kind must be lib or integration")
    if target_kind == "integration":
        if not isinstance(target, str) or not TARGET_RE.fullmatch(target):
            return None, fail("target_invalid", "integration target must be a safe Cargo target name")
    elif target not in ("", None, "lib"):
        return None, fail("target_invalid", "lib requests must not name an integration target")
    if profile not in ALLOWED_PROFILES:
        return None, fail("profile_invalid", "profile is not in the hosted allowlist")
    expected_profile = os.environ.get("VALIDATION_PROFILE", "")
    if expected_profile and profile != expected_profile:
        return None, fail("profile_mismatch", "request profile does not match workflow profile")
    if not isinstance(tests, list) or not tests or len(tests) > MAX_TESTS:
        return None, fail("tests_invalid", f"tests must contain 1..{MAX_TESTS} names")
    if any(not isinstance(name, str) or not TEST_RE.fullmatch(name) for name in tests):
        return None, fail("test_name_invalid", "test names must be fully-qualified safe names")
    if len(set(tests)) != len(tests):
        return None, fail("test_names_duplicate", "test names must be unique")
    normalized = {
        "schema_version": SCHEMA_VERSION,
        "profile": profile,
        "package": package,
        "target_kind": target_kind,
        "target": "" if target in (None, "lib") else target,
        "tests": tests,
    }
    return normalized, None


def cargo_args(request: dict[str, Any], *, list_only: bool, test_name: str = "") -> list[str]:
    # Revalidate at the command-construction boundary as well as at input
    # loading. Every dynamic value is bounded by the typed Rust-name grammar.
    package = request.get("package")
    target_kind = request.get("target_kind")
    target = request.get("target", "")
    if not isinstance(package, str) or not PACKAGE_RE.fullmatch(package):
        raise ValueError("package is not a safe Cargo package name")
    if target_kind not in ALLOWED_TARGET_KINDS:
        raise ValueError("target kind is not in the hosted allowlist")
    if target_kind == "integration":
        if not isinstance(target, str) or not TARGET_RE.fullmatch(target):
            raise ValueError("integration target is not a safe Cargo target name")
    elif target not in ("", None, "lib"):
        raise ValueError("lib requests must not name an integration target")
    if list_only:
        if test_name:
            raise ValueError("inventory requests must not name a test")
    elif not isinstance(test_name, str) or not TEST_RE.fullmatch(test_name):
        raise ValueError("test name is not a safe fully-qualified Rust name")
    # The executable and option names are fixed; validated values are each one
    # argv element, with no shell/eval or option reinterpretation.
    args = ["cargo", "test", "--locked", "-p", package]  # lgtm [py/command-line-injection]
    if target_kind == "lib":
        args.append("--lib")
    else:
        args.extend(["--test", target])  # lgtm [py/command-line-injection]
    args.append("--")
    if list_only:
        args.append("--list")
    else:
        args.extend([test_name, "--exact", "--test-threads=1"])  # lgtm [py/command-line-injection]
    return args


def listed_tests(stdout: str) -> list[str]:
    names: list[str] = []
    for line in stdout.splitlines():
        value = line.strip()
        if value.endswith(": test"):
            name = value[: -len(": test")]
            if name:
                names.append(name)
    return names


def run_request(request: dict[str, Any], repo_root: Path) -> dict[str, Any]:
    manifest_root = repo_root / "codex-rs"
    env = os.environ.copy()
    # These are the established hosted-runner contracts.  Do not accept them
    # from the request: the request selects tests, never runner capabilities.
    env.setdefault("RUST_MIN_STACK", "8388608")
    # The command remains intentionally argv-based (never shell-evaluated);
    # cargo_args revalidates every request-derived value at the sink boundary.
    inventory_command = cargo_args(request, list_only=True)
    inventory = subprocess.run(  # lgtm [py/command-line-injection]
        inventory_command,  # lgtm [py/command-line-injection]
        cwd=manifest_root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
        shell=False,
    )
    names = listed_tests(inventory.stdout)
    if inventory.returncode != 0:
        result = fail("inventory_failed", "Cargo test inventory failed")
        result["inventory"] = {
            "status": "failure",
            "tests": [],
            "diagnostics": command_diagnostics(inventory),
        }
        return result
    counts: dict[str, int] = {}
    for name in names:
        counts[name] = counts.get(name, 0) + 1
    missing = [name for name in request["tests"] if counts.get(name, 0) == 0]
    ambiguous = [name for name in request["tests"] if counts.get(name, 0) != 1]
    result: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "status": "success",
        "request": request,
        "request_fingerprint": hashlib.sha256(
            json.dumps(request, sort_keys=True, separators=(",", ":")).encode("utf-8")
        ).hexdigest(),
        "candidate_sha": git_sha(repo_root),
        "inventory": {
            "status": "success",
            "test_count": len(names),
            "tests": names,
        },
        "tests": [],
    }
    if missing or ambiguous:
        result.update(
            {
                "status": "failure",
                "failure_code": "inventory_reconciliation_failed",
                "missing_tests": missing,
                "ambiguous_tests": ambiguous,
            }
        )
        return result
    for name in request["tests"]:
        # cargo_args revalidates the Rust fully-qualified name before this
        # argv-only invocation; no shell or arbitrary flag can be introduced.
        test_command = cargo_args(request, list_only=False, test_name=name)
        completed = subprocess.run(  # lgtm [py/command-line-injection]
            test_command,  # lgtm [py/command-line-injection]
            cwd=manifest_root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
            shell=False,
        )
        output = "\n".join(
            value for value in (completed.stdout, completed.stderr) if value
        )
        counts = test_result_counts(output)
        execution_reconciled = (
            completed.returncode == 0
            and counts is not None
            and counts["passed"] == 1
            and counts["failed"] == 0
            and counts["ignored"] == 0
            and counts["measured"] == 0
        )
        status = "success" if execution_reconciled else "failure"
        failure_code = ""
        if completed.returncode != 0:
            failure_code = "named_test_failed"
        elif counts is None:
            failure_code = "execution_reconciliation_failed"
        elif counts["ignored"]:
            failure_code = "named_test_ignored"
        elif counts["passed"] != 1 or counts["failed"] or counts["measured"]:
            failure_code = "execution_reconciliation_failed"
        result["tests"].append(
            {
                "name": name,
                "status": status,
                "exit_code": completed.returncode,
                "execution_reconciled": execution_reconciled,
                "result_counts": counts,
                "diagnostics": command_diagnostics(completed),
            }
        )
        if status != "success":
            result["status"] = "failure"
            result["failure_code"] = failure_code
            result["message"] = (
                "named test did not produce exactly one non-ignored passing result; "
                "see bounded diagnostics and the hosted job log"
            )
            break
    return result


def git_sha(repo_root: Path) -> str:
    completed = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=repo_root,
        text=True,
        capture_output=True,
        check=False,
    )
    return completed.stdout.strip() if completed.returncode == 0 else ""


def main() -> int:
    request, error = load_request()
    result = error or run_request(request or {}, Path.cwd().resolve())
    result.setdefault("identity", {})
    result["identity"].update(
        {
            "harness_sha": os.environ.get("VALIDATION_HARNESS_SHA", ""),
            "base_ref": os.environ.get("VALIDATION_BASE_REF", ""),
            "base_sha": os.environ.get("VALIDATION_BASE_SHA", ""),
            "target_sha": os.environ.get("VALIDATION_TARGET_SHA", ""),
            "run_id": os.environ.get("GITHUB_RUN_ID", ""),
        }
    )
    Path("rust-tests-v1-results.json").write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(
        json.dumps(
            {"status": result.get("status"), "failure_code": result.get("failure_code", "")},
            sort_keys=True,
        )
    )
    return 0 if result.get("status") == "success" else 1


if __name__ == "__main__":
    raise SystemExit(main())
