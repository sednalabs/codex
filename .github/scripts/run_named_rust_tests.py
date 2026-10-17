#!/usr/bin/env python3
"""Run a typed Rust test request on a hosted validation runner.

The request is deliberately narrower than a shell command.  It selects a
Cargo target from the committed command catalog and names fully-qualified
tests to reconcile.  The runner inventories the target first and refuses to
run when a requested name is missing or ambiguous.  Requests normally execute
the catalog-owned target command; the explicit ``exact`` mode runs each
inventory-confirmed selector separately with libtest's exact-match flag.
Request selectors are validated and passed as argv elements, never shell text.
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
ALLOWED_TARGET_KINDS = {"lib", "integration", "bin"}
MANIFEST_SCHEMA_VERSION = "rust-tests-command-manifest-v1"
MANIFEST_NAME = "validation-named-tests.json"
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
# Some TUI tests write directly to the terminal while libtest is printing the
# outcome. Keep terminal-control sequences as separators so the final status
# still has a token boundary; arbitrary text ending in "ok" is not an outcome.
TEST_OUTCOME_RE = re.compile(
    r"^test (?P<name>.+?) \.\.\..*(?:[ \t]|\x00)"
    r"(?P<status>ok|FAILED|ignored)$"
)
ANSI_CSI_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


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
    if not matches:
        return None
    # Cross-process fixtures can emit child libtest summaries. The enclosing
    # target emits its summary last; named outcomes and exit status are still
    # reconciled separately before accepting any requested test.
    match = matches[-1]
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


def manifest_path(repo_root: Path) -> Path:
    """Return the candidate-owned closed command catalog."""

    return repo_root / ".github" / MANIFEST_NAME


def target_key(package: str, target_kind: str, target: str) -> tuple[str, str, str]:
    return package, target_kind, target


def expected_commands(
    package: str, target_kind: str, target: str
) -> tuple[list[str], list[str]]:
    """Build the only two command shapes accepted by the catalog validator.

    The package and target values here come from the committed catalog, not
    the dispatch request.  The result is used only to validate that the
    catalog contains complete, non-templated argv tuples.
    """

    command = ["cargo", "test", "--locked", "-p", package]
    if target_kind == "lib":
        command.append("--lib")
    elif target_kind == "integration":
        command.extend(["--test", target])
    elif target_kind == "bin":
        command.extend(["--bin", target])
    else:
        raise ValueError("Cargo target kind is not supported")
    inventory = [*command, "--", "--list"]
    execution = [*command, "--", "--test-threads=1"]
    return inventory, execution


def load_manifest(repo_root: Path) -> dict[tuple[str, str, str], dict[str, Any]]:
    """Load and validate the closed target/argv manifest before any Cargo call."""

    try:
        payload = json.loads(manifest_path(repo_root).read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"named-test command manifest is unavailable: {exc}") from exc
    if not isinstance(payload, dict) or payload.get("schema_version") != MANIFEST_SCHEMA_VERSION:
        raise ValueError("named-test command manifest schema is unsupported")
    rows = payload.get("targets")
    if not isinstance(rows, list) or not rows:
        raise ValueError("named-test command manifest must contain targets")
    manifest: dict[tuple[str, str, str], dict[str, Any]] = {}
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError("named-test command manifest rows must be objects")
        package = row.get("package")
        target_kind = row.get("target_kind")
        target = row.get("target", "")
        profiles = row.get("profiles")
        inventory_argv = row.get("inventory_argv")
        execution_argv = row.get("execution_argv")
        if not isinstance(package, str) or not PACKAGE_RE.fullmatch(package):
            raise ValueError("manifest package is not a safe Cargo package name")
        if target_kind not in ALLOWED_TARGET_KINDS:
            raise ValueError("manifest target kind is not supported")
        if target_kind in {"integration", "bin"}:
            if not isinstance(target, str) or not TARGET_RE.fullmatch(target):
                raise ValueError("manifest named Cargo target is not safe")
        elif target != "":
            raise ValueError("manifest lib targets must use an empty target")
        if (
            not isinstance(profiles, list)
            or not profiles
            or not all(isinstance(profile, str) for profile in profiles)
            or not set(profiles) <= ALLOWED_PROFILES
        ):
            raise ValueError("manifest profiles are not in the hosted allowlist")
        if len(set(profiles)) != len(profiles):
            raise ValueError("manifest profiles must be unique")
        if (
            not isinstance(inventory_argv, list)
            or not isinstance(execution_argv, list)
            or not inventory_argv
            or not execution_argv
            or not all(isinstance(value, str) for value in [*inventory_argv, *execution_argv])
        ):
            raise ValueError("manifest commands must be complete argv string lists")
        expected_inventory, expected_execution = expected_commands(
            package, target_kind, target
        )
        if inventory_argv != expected_inventory or execution_argv != expected_execution:
            raise ValueError(
                "manifest command tuples must exactly match the fixed Cargo shapes"
            )
        key = target_key(package, target_kind, target)
        if key in manifest:
            raise ValueError("named-test command manifest contains a duplicate target")
        manifest[key] = {
            "package": package,
            "target_kind": target_kind,
            "target": target,
            "profiles": tuple(profiles),
            "inventory_argv": tuple(inventory_argv),
            "execution_argv": tuple(execution_argv),
        }
    return manifest


def select_target(
    request: dict[str, Any], manifest: dict[tuple[str, str, str], dict[str, Any]]
) -> dict[str, Any]:
    package = request.get("package")
    target_kind = request.get("target_kind")
    target = request.get("target", "")
    if not isinstance(package, str) or not isinstance(target_kind, str) or not isinstance(target, str):
        raise ValueError("request target selector is incomplete")
    record = manifest.get(target_key(package, target_kind, target))
    if record is None:
        raise ValueError("request target selector is not in the committed command catalog")
    profile = request.get("profile")
    if profile not in record["profiles"]:
        raise ValueError("request profile is not enabled for the selected target")
    return record


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
    execution_mode = payload.get("execution_mode", "target")
    if not isinstance(package, str) or not PACKAGE_RE.fullmatch(package):
        return None, fail("package_invalid", "package must be a safe Cargo package name")
    if target_kind not in ALLOWED_TARGET_KINDS:
        return None, fail("target_kind_invalid", "target_kind must be lib, integration, or bin")
    if target_kind in {"integration", "bin"}:
        if not isinstance(target, str) or not TARGET_RE.fullmatch(target):
            return None, fail("target_invalid", "named Cargo target must be a safe target name")
    elif target not in ("", None, "lib"):
        return None, fail("target_invalid", "lib requests must not name a named Cargo target")
    if profile not in ALLOWED_PROFILES:
        return None, fail("profile_invalid", "profile is not in the hosted allowlist")
    if not isinstance(execution_mode, str) or execution_mode not in {"target", "exact"}:
        return None, fail("execution_mode_invalid", "execution_mode must be target or exact")
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
    if execution_mode == "exact":
        normalized["execution_mode"] = "exact"
    return normalized, None


def cargo_args(
    request: dict[str, Any],
    *,
    list_only: bool,
    test_name: str = "",
    command_record: dict[str, Any] | None = None,
) -> list[str]:
    """Return the catalog-owned command argv for one target.

    A test name is a reconciliation selector, not an argv fragment.  The
    default target mode executes this immutable command once and matches
    requested names against its exact per-test output.  Exact mode adds one
    validated selector around this command separately.  Rejecting ``test_name``
    here prevents callers from accidentally reintroducing a request-derived
    sink.
    """

    if test_name:
        raise ValueError("test selectors are not command arguments")
    record = command_record
    if record is None:
        manifest_root = Path(__file__).resolve().parents[2]
        record = select_target(request, load_manifest(manifest_root))
    key = "inventory_argv" if list_only else "execution_argv"
    command = record.get(key)
    if not isinstance(command, tuple) or not all(isinstance(value, str) for value in command):
        raise ValueError("command catalog entry is not a complete argv tuple")
    return list(command)


def exact_cargo_args(
    request: dict[str, Any],
    test_name: str,
    *,
    command_record: dict[str, Any],
) -> list[str]:
    """Build an exact-filter command for one inventory-confirmed selector."""

    if (
        not isinstance(test_name, str)
        or not TEST_RE.fullmatch(test_name)
        or test_name not in request.get("tests", [])
    ):
        raise ValueError("exact test selector is not one of the validated request names")
    command = cargo_args(request, list_only=False, command_record=command_record)
    try:
        separator = command.index("--")
    except ValueError as exc:
        raise ValueError("catalog command is missing the test-argument separator") from exc
    return [
        *command[:separator],
        test_name,
        "--",
        "--exact",
        *command[separator + 1 :],
    ]


def listed_tests(stdout: str) -> list[str]:
    names: list[str] = []
    for line in stdout.splitlines():
        value = line.strip()
        if value.endswith(": test"):
            name = value[: -len(": test")]
            if name:
                names.append(name)
    return names


def test_outcomes(output: str) -> dict[str, list[str]]:
    """Index each exact Cargo test result without trusting a summary count."""

    outcomes: dict[str, list[str]] = {}
    for line in ANSI_CSI_RE.sub("\x00", output).splitlines():
        match = TEST_OUTCOME_RE.match(line.strip())
        if match:
            outcomes.setdefault(match.group("name"), []).append(match.group("status"))
    return outcomes


def run_request(request: dict[str, Any], repo_root: Path) -> dict[str, Any]:
    manifest_root = repo_root / "codex-rs"
    try:
        command_record = select_target(request, load_manifest(repo_root))
    except ValueError as exc:
        return fail("target_selector_unknown", str(exc))
    env = os.environ.copy()
    # These are the established hosted-runner contracts.  Do not accept them
    # from the request: the request selects tests, never runner capabilities.
    env.setdefault("RUST_MIN_STACK", "8388608")
    # Both subprocess commands are complete tuples from the closed catalog.
    # Request fields select a catalog entry and never form an argv element.
    inventory_command = cargo_args(
        request, list_only=True, command_record=command_record
    )
    inventory = subprocess.run(
        inventory_command,
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
    execution_mode = request.get("execution_mode", "target")
    if execution_mode == "exact":
        test_commands = [
            (name, exact_cargo_args(request, name, command_record=command_record))
            for name in request["tests"]
        ]
    else:
        test_commands = [
            (
                "",
                cargo_args(request, list_only=False, command_record=command_record),
            )
        ]

    for test_index, (exact_name, test_command) in enumerate(test_commands):
        completed = subprocess.run(
            test_command,
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
        outcomes = test_outcomes(output)
        selected_names = [exact_name] if exact_name else request["tests"]
        for name in selected_names:
            observed = outcomes.get(name, [])
            outcome = observed[0] if len(observed) == 1 else ""
            exact_count_matches = (
                execution_mode != "exact"
                or (
                    counts is not None
                    and counts["passed"] == 1
                    and counts["failed"] == 0
                    and counts["ignored"] == 0
                    and counts["filtered"] == len(names) - 1
                )
            )
            execution_reconciled = (
                completed.returncode == 0
                and counts is not None
                and len(observed) == 1
                and outcome == "ok"
                and exact_count_matches
            )
            status = "success" if execution_reconciled else "failure"
            if len(observed) != 1 or counts is None or not exact_count_matches:
                failure_code = "execution_reconciliation_failed"
            elif outcome == "ignored":
                failure_code = "named_test_ignored"
            elif completed.returncode != 0 or outcome == "FAILED":
                failure_code = "named_test_failed"
            else:
                failure_code = ""
            result["tests"].append(
                {
                    "name": name,
                    "status": status,
                    "exit_code": completed.returncode,
                    "execution_reconciled": execution_reconciled,
                    "observed_outcomes": observed,
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
                if execution_mode != "exact":
                    break
        if execution_mode == "exact" and counts is None:
            # A command that produced no libtest summary usually failed before
            # executing the selected test (for example, a compile error).
            # Repeating that failure for every selector would waste hosted
            # time without producing additional test evidence.
            result["unattempted_tests"] = request["tests"][test_index + 1 :]
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
