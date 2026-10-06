#!/usr/bin/env python3
"""Run a typed, exact Rust test request on a hosted validation runner.

The request is deliberately narrower than a shell command.  It selects a
Cargo target from the committed command catalog and names fully-qualified
tests to reconcile. The runner inventories the target first and refuses to
run when a requested name is missing or ambiguous. By default it runs the
complete catalog-owned target once; an explicit exact_tests mode runs only
the verified requested selectors. Failure output is projected to fixed
classes and verified repository locations; raw test output is never published.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
from pathlib import Path, PurePosixPath
from typing import Any


SCHEMA_VERSION = "rust-tests-v1"
PACKAGE_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
TARGET_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
TEST_RE = re.compile(r"^[A-Za-z0-9_:.\-]{1,255}$")
ALLOWED_PROFILES = {"rust_minimal", "rust_integration"}
ALLOWED_TARGET_KINDS = {"lib", "integration"}
ALLOWED_EXECUTION_MODES = {"full_target", "exact_tests"}
TUI_DIAGNOSTIC_TESTS = (
    "app::agents_overview::tests::"
    "agents_overview_reasoning_uses_existing_events_and_expires_with_attachment",
    "app::agents_overview::tests::agents_overview_details_render_markdown",
)
MANIFEST_SCHEMA_VERSION = "rust-tests-command-manifest-v1"
MANIFEST_NAME = "validation-named-tests.json"
MAX_TESTS = 64
MAX_REQUEST_CHARS = 32768
TEST_RESULT_RE = re.compile(
    r"test result:\s+\w+\.\s+"
    r"(?P<passed>\d+) passed;\s+"
    r"(?P<failed>\d+) failed;\s+"
    r"(?P<ignored>\d+) ignored;\s+"
    r"(?P<measured>\d+) measured;\s+"
    r"(?P<filtered>\d+) filtered out"
)
TEST_OUTCOME_RE = re.compile(r"^test (?P<name>.+?) \.\.\. (?P<status>ok|FAILED|ignored)$")
SOURCE_LOCATION_RE = re.compile(
    r"^thread '[^'\r\n]+' panicked at (?P<path>(?:(?:/[A-Za-z0-9_.-]+)*/)?"
    r"(?:codex-rs/tui/src/|tui/src/|src/app/)"
    r"(?:[A-Za-z0-9_.-]+/)*[A-Za-z0-9_.-]+\.rs):"
    r"(?P<line>[1-9][0-9]{0,6}):(?P<column>[1-9][0-9]{0,6})",
    re.MULTILINE,
)


def verified_repo_location(
    raw_path: str, line: int, column: int, repo_root: Path
) -> str | None:
    """Return only an existing repo-relative TUI source location."""

    marker = "codex-rs/tui/src/"
    if marker in raw_path:
        relative = marker + raw_path.split(marker, 1)[1]
    elif raw_path.startswith("tui/src/"):
        relative = "codex-rs/" + raw_path
    elif raw_path.startswith("src/app/"):
        relative = "codex-rs/tui/" + raw_path
    else:
        return None
    candidate_path = PurePosixPath(relative)
    if candidate_path.is_absolute() or ".." in candidate_path.parts:
        return None
    root = repo_root.resolve()
    resolved = (root / Path(*candidate_path.parts)).resolve()
    if not resolved.is_relative_to(root) or not resolved.is_file():
        return None
    if resolved.suffix != ".rs":
        return None
    return f"{candidate_path.as_posix()}:{line}:{column}"


def failure_projection(output: str, repo_root: Path) -> dict[str, Any]:
    """Project failure output to fixed classes and verified public locations."""

    lowered = output.lower()
    if "snapshot assertion" in lowered or "snapshot mismatch" in lowered:
        failure_class = "snapshot_assertion_failed"
    elif "assertion left == right failed" in lowered:
        failure_class = "assertion_equal_failed"
    elif "assertion failed" in lowered:
        failure_class = "assertion_failed"
    elif "panicked at" in lowered:
        failure_class = "panic"
    else:
        failure_class = "test_failure"

    source_location = None
    for match in SOURCE_LOCATION_RE.finditer(output):
        source_location = verified_repo_location(
            match.group("path"),
            int(match.group("line")),
            int(match.group("column")),
            repo_root,
        )
        if source_location:
            break

    return {
        "class": failure_class,
        "source_location": source_location,
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
    else:
        command.extend(["--test", target])
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
        if target_kind == "integration":
            if not isinstance(target, str) or not TARGET_RE.fullmatch(target):
                raise ValueError("manifest integration target is not safe")
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
    execution_mode = payload.get("execution_mode", "full_target")
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
    if (
        not isinstance(execution_mode, str)
        or execution_mode not in ALLOWED_EXECUTION_MODES
    ):
        return None, fail("execution_mode_invalid", "execution_mode is not in the hosted allowlist")
    expected_profile = os.environ.get("VALIDATION_PROFILE", "")
    if expected_profile and profile != expected_profile:
        return None, fail("profile_mismatch", "request profile does not match workflow profile")
    if not isinstance(tests, list) or not tests or len(tests) > MAX_TESTS:
        return None, fail("tests_invalid", f"tests must contain 1..{MAX_TESTS} names")
    if any(not isinstance(name, str) or not TEST_RE.fullmatch(name) for name in tests):
        return None, fail("test_name_invalid", "test names must be fully-qualified safe names")
    if len(set(tests)) != len(tests):
        return None, fail("test_names_duplicate", "test names must be unique")
    if execution_mode == "exact_tests" and (
        package != "codex-tui"
        or target_kind != "lib"
        or target not in ("", None, "lib")
        or profile != "rust_minimal"
        or tests != list(TUI_DIAGNOSTIC_TESTS)
    ):
        return None, fail(
            "execution_mode_scope_invalid",
            "exact_tests is restricted to the approved TUI diagnostic selectors",
        )
    normalized = {
        "schema_version": SCHEMA_VERSION,
        "profile": profile,
        "package": package,
        "target_kind": target_kind,
        "target": "" if target in (None, "lib") else target,
        "tests": tests,
    }
    if "execution_mode" in payload:
        normalized["execution_mode"] = execution_mode
    return normalized, None


def cargo_args(
    request: dict[str, Any],
    *,
    list_only: bool,
    exact_test: str = "",
    command_record: dict[str, Any] | None = None,
) -> list[str]:
    """Return a closed command tuple with at most one verified exact selector."""

    if exact_test and (
        list_only
        or not TEST_RE.fullmatch(exact_test)
        or exact_test not in request["tests"]
    ):
        raise ValueError("exact test selector was not validated by inventory")
    record = command_record
    if record is None:
        manifest_root = Path(__file__).resolve().parents[2]
        record = select_target(request, load_manifest(manifest_root))
    key = "inventory_argv" if list_only else "execution_argv"
    command = record.get(key)
    if not isinstance(command, tuple) or not all(isinstance(value, str) for value in command):
        raise ValueError("command catalog entry is not a complete argv tuple")
    result = list(command)
    if exact_test:
        result.extend(["--exact", exact_test])
    return result


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
    for line in output.splitlines():
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
            "exit_code": inventory.returncode,
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
    execution_mode = request.get("execution_mode", "full_target")
    executions: dict[
        str,
        tuple[
            subprocess.CompletedProcess[str],
            str,
            dict[str, int] | None,
            list[str],
            bool,
        ],
    ] = {}
    if execution_mode == "exact_tests":
        for name in request["tests"]:
            # Inventory above proves each requested name is unique in this target.
            # Request values are appended only after safe-name and inventory checks.
            test_command = cargo_args(
                request,
                list_only=False,
                exact_test=name,
                command_record=command_record,
            )
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
            executions[name] = (
                completed,
                output,
                test_result_counts(output),
                test_outcomes(output).get(name, []),
                True,
            )
    else:
        # Preserve the established default: one complete immutable target run,
        # with requested selectors reconciled against its unfiltered results.
        test_command = cargo_args(
            request, list_only=False, command_record=command_record
        )
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
        result_counts = test_result_counts(output)
        outcomes = test_outcomes(output)
        for name in request["tests"]:
            executions[name] = (
                completed,
                output,
                result_counts,
                outcomes.get(name, []),
                False,
            )

    for name in request["tests"]:
        completed, output, result_counts, observed, isolated = executions[name]
        outcome = observed[0] if len(observed) == 1 else ""
        execution_reconciled = (
            completed.returncode == 0
            and result_counts is not None
            and len(observed) == 1
            and outcome == "ok"
        )
        status = "success" if execution_reconciled else "failure"
        failure_code = ""
        if len(observed) != 1:
            failure_code = "execution_reconciliation_failed"
        elif outcome == "ignored":
            failure_code = "named_test_ignored"
        elif completed.returncode != 0 or outcome == "FAILED":
            failure_code = "named_test_failed"
        elif result_counts is None:
            failure_code = "execution_reconciliation_failed"
        diagnostics = {"exit_code": completed.returncode}
        if not execution_reconciled and isolated:
            diagnostics.update(failure_projection(output, repo_root))
        test_result = {
            "name": name,
            "status": status,
            "exit_code": completed.returncode,
            "execution_reconciled": execution_reconciled,
            "observed_outcomes": observed,
            "result_counts": result_counts,
            "diagnostics": diagnostics,
        }
        if failure_code:
            test_result["failure_code"] = failure_code
        result["tests"].append(test_result)
        if status != "success":
            result["status"] = "failure"
            if "failure_code" not in result:
                result["failure_code"] = failure_code
                result["message"] = (
                    "one or more named tests did not produce exactly one "
                    "non-ignored passing result"
                )
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
    safe_tests = []
    allowed_failure_classes = {
        "snapshot_assertion_failed",
        "assertion_equal_failed",
        "assertion_failed",
        "panic",
        "test_failure",
    }
    for test in result.get("tests", []):
        if not isinstance(test, dict):
            continue
        name = test.get("name")
        status = test.get("status")
        if not isinstance(name, str) or not TEST_RE.fullmatch(name):
            continue
        if status not in {"success", "failure"}:
            continue
        observed = test.get("observed_outcomes")
        observed_outcome = (
            observed[0]
            if isinstance(observed, list)
            and len(observed) == 1
            and observed[0] in {"ok", "FAILED", "ignored"}
            else "unknown"
        )
        safe_test = {
            "selector": name,
            "status": status,
            "observed_outcome": observed_outcome,
        }
        if status == "failure":
            failure_code = test.get("failure_code")
            if isinstance(failure_code, str) and re.fullmatch(r"[a-z_]{1,64}", failure_code):
                safe_test["failure_code"] = failure_code
            diagnostics = test.get("diagnostics")
            if isinstance(diagnostics, dict):
                failure_class = diagnostics.get("class")
                if failure_class in allowed_failure_classes:
                    safe_test["failure_class"] = failure_class
                source_location = diagnostics.get("source_location")
                if isinstance(source_location, str):
                    location_match = re.fullmatch(
                        r"(?P<path>codex-rs/tui/src/[A-Za-z0-9_./-]+\.rs):"
                        r"(?P<line>[1-9][0-9]{0,6}):(?P<column>[1-9][0-9]{0,6})",
                        source_location,
                    )
                    if location_match:
                        verified_location = verified_repo_location(
                            location_match.group("path"),
                            int(location_match.group("line")),
                            int(location_match.group("column")),
                            Path.cwd().resolve(),
                        )
                        if verified_location == source_location:
                            safe_test["source_location"] = source_location
        safe_tests.append(safe_test)
    inventory_summary = result.get("inventory")
    if not isinstance(inventory_summary, dict):
        inventory_summary = {}
    request_summary = result.get("request")
    if not isinstance(request_summary, dict):
        request_summary = {}
    safe_failure_code = result.get("failure_code", "")
    if not isinstance(safe_failure_code, str) or not re.fullmatch(
        r"[a-z_]{1,64}", safe_failure_code
    ):
        safe_failure_code = ""
    safe_summary = {
        "status": result.get("status")
        if result.get("status") in {"success", "failure"}
        else "failure",
        "failure_code": safe_failure_code,
        "execution_mode": request_summary.get("execution_mode", "full_target"),
        "inventory": {
            "status": inventory_summary.get("status", "not-run"),
            "test_count": inventory_summary.get("test_count", 0),
        },
        "tests": safe_tests,
    }
    if safe_summary["execution_mode"] == "full_target" and safe_tests:
        test_results = result.get("tests", [])
        if (
            test_results
            and isinstance(test_results[0], dict)
            and isinstance(test_results[0].get("result_counts"), dict)
        ):
            safe_summary["full_target_summary"] = test_results[0]["result_counts"]
    print(json.dumps(safe_summary, sort_keys=True))
    return 0 if result.get("status") == "success" else 1


if __name__ == "__main__":
    raise SystemExit(main())
