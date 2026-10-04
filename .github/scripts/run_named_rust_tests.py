#!/usr/bin/env python3
"""Run a typed, exact Rust test request on a hosted validation runner.

The request is deliberately narrower than a shell command.  It selects a
Cargo target from the committed command catalog and names fully-qualified
tests to reconcile.  The runner inventories the target first, refuses to run
when a requested name is missing or ambiguous, and executes only the
catalog-owned target argv.  This prevents request data from reaching Cargo.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import stat
import subprocess
import sys
from pathlib import Path
from typing import Any


SCHEMA_VERSION = "rust-tests-v1"
PACKAGE_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
TARGET_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
TEST_RE = re.compile(r"^[A-Za-z0-9_:.\-]{1,255}$")
ALLOWED_PROFILES = {"rust_minimal", "rust_integration"}
ALLOWED_TARGET_KINDS = {"lib", "integration"}
MANIFEST_SCHEMA_VERSION = "rust-tests-command-manifest-v1"
MANIFEST_NAME = "validation-named-tests.json"
MAX_TESTS = 64
MAX_REQUEST_CHARS = 32768
MAX_DIAGNOSTIC_CHARS = 4096
MAX_MATCHED_TEST_LINES = 4
MAX_MATCHED_TEST_LINE_CHARS = 512
MAX_FAILURE_NAMES = 1024
MAX_FAILURE_BLOCKS = 8
MAX_FAILURE_EVIDENCE_BYTES = 350 * 1024
MAX_FAILURE_BLOCK_EVIDENCE_CHARS = 2048
MAX_FAILURE_MARKERS_PER_BLOCK = 16
MAX_CARGO_SUMMARIES_PER_CHANNEL = 4
MAX_CARGO_SUMMARY_COUNT_DIGITS = 12
CORE_RUNTIME_TARGET = ("codex-core", "integration", "all")
CORE_RUNTIME_BUILDS = (
    (
        "codex-code-mode-host",
        (
            "cargo",
            "build",
            "--locked",
            "-p",
            "codex-code-mode-host",
            "--bin",
            "codex-code-mode-host",
        ),
    ),
    (
        "codex",
        ("cargo", "build", "--locked", "-p", "codex-cli", "--bin", "codex"),
    ),
)
CORE_RUNTIME_ENV_KEYS = {
    "codex-code-mode-host": (
        "CARGO_BIN_EXE_codex-code-mode-host",
        "CARGO_BIN_EXE_codex_code_mode_host",
    ),
    "codex": ("CARGO_BIN_EXE_codex",),
}
RUNTIME_PREPARATION_ONLY_ENV = "VALIDATION_RUNTIME_PREPARATION_ONLY"
TEST_RESULT_RE = re.compile(
    r"test result:\s+\w+\.\s+"
    r"(?P<passed>\d+) passed;\s+"
    r"(?P<failed>\d+) failed;\s+"
    r"(?P<ignored>\d+) ignored;\s+"
    r"(?P<measured>\d+) measured;\s+"
    r"(?P<filtered>\d+) filtered out"
)
TEST_OUTCOME_RE = re.compile(r"^test (?P<name>.+?) \.\.\. (?P<status>ok|FAILED|ignored)$")
FAILURE_HEADER_RE = re.compile(
    r"^---- (?P<name>.{1,256}) (?P<channel>stdout|stderr) ----$"
)
FAILURE_MARKER_PATTERNS = (
    ("permission-denied", re.compile(r"\bpermission denied\b", re.IGNORECASE)),
    (
        "operation-not-permitted",
        re.compile(r"\boperation not permitted\b", re.IGNORECASE),
    ),
    ("no-such-file", re.compile(r"\bno such file or directory\b", re.IGNORECASE)),
    ("not-found", re.compile(r"\bnot found\b", re.IGNORECASE)),
    ("broken-pipe", re.compile(r"\bbroken pipe\b", re.IGNORECASE)),
    ("connection-refused", re.compile(r"\bconnection refused\b", re.IGNORECASE)),
    ("connection-reset", re.compile(r"\bconnection reset\b", re.IGNORECASE)),
    ("timed-out", re.compile(r"\btimed out\b|\btimeout\b", re.IGNORECASE)),
    (
        "resource-unavailable",
        re.compile(
            r"\bresource temporarily unavailable\b|\bwould block\b",
            re.IGNORECASE,
        ),
    ),
    (
        "assertion-failed",
        re.compile(r"\bassertion(?: `[^`]{0,64}`)? failed\b", re.IGNORECASE),
    ),
    (
        "unwrap-error",
        re.compile(r"\bcalled `(?:Result|Option)::unwrap\(\)`", re.IGNORECASE),
    ),
    ("panicked", re.compile(r"\bpanicked\b|\bthread .* panicked\b", re.IGNORECASE)),
)
OS_ERROR_CODE_RE = re.compile(r"\bOs\s*\{\s*code:\s*(\d{1,5})\b")
OS_ERROR_TEXT_CODE_RE = re.compile(r"\(os error (\d{1,5})\)")
HTTP_STATUS_CODE_RE = re.compile(
    r"\b(?:HTTP(?:/\d(?:\.\d)?)?\s+|status(?: code)?[:= ]+)([1-5]\d{2})\b",
    re.IGNORECASE,
)
PROCESS_EXIT_CODE_RE = re.compile(
    r"\b(?:exit(?:ed)? (?:with )?status|exit code)[:= ]+(-?\d{1,5})\b",
    re.IGNORECASE,
)


def bounded_diagnostic(value: str | None) -> str:
    """Keep failure evidence actionable without duplicating unbounded logs."""

    text = str(value or "").strip()
    if len(text) <= MAX_DIAGNOSTIC_CHARS:
        return text
    return "...[truncated; only the bounded captured tail is retained]...\n" + text[
        -MAX_DIAGNOSTIC_CHARS:
    ]


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


def cargo_summary_channel_evidence(output: str) -> dict[str, Any]:
    """Expose bounded numeric Cargo summary counts without retaining log text."""

    match_count = 0
    summaries = []
    for match in TEST_RESULT_RE.finditer(output):
        match_count += 1
        if len(summaries) >= MAX_CARGO_SUMMARIES_PER_CHANNEL:
            continue
        raw_counts = {
            name: match.group(name)
            for name in ("passed", "failed", "ignored", "measured", "filtered")
        }
        if any(
            len(value) > MAX_CARGO_SUMMARY_COUNT_DIGITS
            for value in raw_counts.values()
        ):
            continue
        summaries.append({name: int(value) for name, value in raw_counts.items()})
    return {
        "match_count": match_count,
        "summaries": summaries,
        "omitted_count": match_count - len(summaries),
        "truncated": match_count > len(summaries),
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


def parse_request(
    raw: str, expected_profile: str
) -> tuple[dict[str, Any] | None, dict[str, Any] | None]:
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


def load_request() -> tuple[dict[str, Any] | None, dict[str, Any] | None]:
    return parse_request(
        os.environ.get("RUST_TEST_REQUEST_JSON", ""),
        os.environ.get("VALIDATION_PROFILE", ""),
    )


def request_requires_core_runtime(
    raw: str,
    expected_profile: str,
    manifest: dict[tuple[str, str, str], dict[str, Any]],
) -> bool:
    request, error = parse_request(raw, expected_profile)
    if error is not None or request is None:
        return False
    try:
        select_target(request, manifest)
    except ValueError:
        return False
    return (
        request["package"],
        request["target_kind"],
        request["target"],
    ) == CORE_RUNTIME_TARGET


def cargo_args(
    request: dict[str, Any],
    *,
    list_only: bool,
    test_name: str = "",
    command_record: dict[str, Any] | None = None,
) -> list[str]:
    """Return a complete argv tuple from the closed target command catalog.

    A test name is a reconciliation selector, not an argv fragment.  The
    runner executes the immutable target command once and matches requested
    names against its exact per-test output.  Rejecting ``test_name`` here
    prevents callers from accidentally reintroducing a request-derived sink.
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


def _core_runtime_target_dir(manifest_root: Path, env: dict[str, str]) -> Path | None:
    """Resolve only target-directory forms whose Cargo output location is exact."""

    if env.get("CARGO_BUILD_TARGET"):
        return None
    configured_target_dir = env.get("CARGO_TARGET_DIR")
    if configured_target_dir is None:
        target_dir = manifest_root / "target"
    else:
        target_dir = Path(configured_target_dir)
        if not target_dir.is_absolute():
            return None
    return target_dir / "debug"


def _core_runtime_binary_state(path: Path) -> tuple[bool, bool]:
    """Return regular-file and executable state without exposing the path."""

    try:
        is_regular_file = stat.S_ISREG(path.lstat().st_mode)
    except OSError:
        return False, False
    return is_regular_file, is_regular_file and os.access(path, os.X_OK)


def _runtime_binary_evidence(
    name: str,
    env: dict[str, str],
    *,
    before: tuple[bool, bool] | None,
    after: tuple[bool, bool] | None,
) -> dict[str, Any]:
    return {
        "name": name,
        "env_key_set": {
            key: key in env for key in CORE_RUNTIME_ENV_KEYS[name]
        },
        "regular_file_before": None if before is None else before[0],
        "executable_before": None if before is None else before[1],
        "regular_file_after": None if after is None else after[0],
        "executable_after": None if after is None else after[1],
    }


def prepare_core_integration_runtime(
    request: dict[str, Any],
    manifest_root: Path,
    env: dict[str, str],
    source_sha: str,
) -> dict[str, Any] | None:
    """Build and prove the fixed cross-package binaries for core/all only."""

    if (
        request["package"],
        request["target_kind"],
        request["target"],
    ) != CORE_RUNTIME_TARGET:
        return None

    supplied_sha = env.get("VALIDATION_TARGET_SHA", "")
    expected_sha = (
        supplied_sha if re.fullmatch(r"[0-9a-f]{40}", supplied_sha) else ""
    )
    evidence: dict[str, Any] = {
        "status": "failure",
        "source_sha": source_sha,
        "expected_target_sha": expected_sha,
        "source_identity_matches": bool(
            re.fullmatch(r"[0-9a-f]{40}", source_sha)
            and re.fullmatch(r"[0-9a-f]{40}", expected_sha)
            and source_sha == expected_sha
        ),
        "target_dir_context": "unsupported",
        "builds": [],
        "binaries": [],
    }
    if not evidence["source_identity_matches"]:
        evidence["failure_reason"] = "source_identity_mismatch"
        return evidence

    binary_dir = _core_runtime_target_dir(manifest_root, env)
    if binary_dir is None:
        evidence["failure_reason"] = "unsupported_target_dir_context"
        evidence["binaries"] = [
            _runtime_binary_evidence(name, env, before=None, after=None)
            for name, _ in CORE_RUNTIME_BUILDS
        ]
        return evidence
    evidence["target_dir_context"] = (
        "absolute_override" if "CARGO_TARGET_DIR" in env else "workspace_default"
    )

    before_states: dict[str, tuple[bool, bool]] = {}
    for name, _ in CORE_RUNTIME_BUILDS:
        before_states[name] = _core_runtime_binary_state(binary_dir / name)

    evidence["binaries"] = [
        _runtime_binary_evidence(
            name,
            env,
            before=before_states[name],
            after=None,
        )
        for name, _ in CORE_RUNTIME_BUILDS
    ]
    if any(
        key in env
        for name, _ in CORE_RUNTIME_BUILDS
        for key in CORE_RUNTIME_ENV_KEYS[name]
    ):
        evidence["failure_reason"] = "unexpected_binary_environment_override"
        return evidence
    if any(regular or executable for regular, executable in before_states.values()):
        evidence["failure_reason"] = "binary_present_before_build"
        return evidence

    for name, command in CORE_RUNTIME_BUILDS:
        try:
            completed = subprocess.run(
                list(command),
                cwd=manifest_root,
                env=env,
                text=True,
                capture_output=True,
                check=False,
                shell=False,
            )
        except OSError:
            evidence["builds"].append(
                {"name": name, "status": "launch_failed", "exit_code": None}
            )
            evidence["failure_reason"] = "binary_build_failed"
            after_states = {
                item_name: _core_runtime_binary_state(binary_dir / item_name)
                for item_name, _ in CORE_RUNTIME_BUILDS
            }
            evidence["binaries"] = [
                _runtime_binary_evidence(
                    item_name,
                    env,
                    before=before_states[item_name],
                    after=after_states[item_name],
                )
                for item_name, _ in CORE_RUNTIME_BUILDS
            ]
            return evidence
        build_succeeded = completed.returncode == 0
        build_record: dict[str, Any] = {
            "name": name,
            "status": "success" if build_succeeded else "failure",
            "exit_code": completed.returncode,
        }
        if not build_succeeded:
            build_record["diagnostics"] = command_diagnostics(completed)
        evidence["builds"].append(build_record)
        if not build_succeeded:
            evidence["failure_reason"] = "binary_build_failed"
            after_states = {
                item_name: _core_runtime_binary_state(binary_dir / item_name)
                for item_name, _ in CORE_RUNTIME_BUILDS
            }
            evidence["binaries"] = [
                _runtime_binary_evidence(
                    item_name,
                    env,
                    before=before_states[item_name],
                    after=after_states[item_name],
                )
                for item_name, _ in CORE_RUNTIME_BUILDS
            ]
            return evidence

    after_states = {
        name: _core_runtime_binary_state(binary_dir / name)
        for name, _ in CORE_RUNTIME_BUILDS
    }
    evidence["binaries"] = [
        _runtime_binary_evidence(
            name,
            env,
            before=before_states[name],
            after=after_states[name],
        )
        for name, _ in CORE_RUNTIME_BUILDS
    ]
    if not all(
        regular and executable for regular, executable in after_states.values()
    ):
        evidence["failure_reason"] = "binary_missing_or_not_executable"
        return evidence
    evidence["status"] = "success"
    return evidence


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


def test_outcome_lines(output: str) -> dict[str, list[str]]:
    """Retain exact, bounded-selector evidence lines from Cargo's test output."""

    lines: dict[str, list[str]] = {}
    for line in output.splitlines():
        value = line.strip()
        match = TEST_OUTCOME_RE.match(value)
        if match:
            lines.setdefault(match.group("name"), []).append(value)
    return lines


def _is_header_candidate(line: str) -> bool:
    return line.startswith("---- ") and (
        line.endswith(" stdout ----") or line.endswith(" stderr ----")
    )


def _failure_markers_for_line(line: str) -> tuple[set[str], set[tuple[str, int]]]:
    markers = {
        marker
        for marker, pattern in FAILURE_MARKER_PATTERNS
        if pattern.search(line)
    }
    numeric_captures: set[tuple[str, int]] = set()
    for kind, pattern in (
        ("os-error-code", OS_ERROR_CODE_RE),
        ("os-error-code", OS_ERROR_TEXT_CODE_RE),
        ("http-status-code", HTTP_STATUS_CODE_RE),
        ("process-exit-code", PROCESS_EXIT_CODE_RE),
    ):
        numeric_captures.update((kind, int(value)) for value in pattern.findall(line))
    if any(kind == "http-status-code" for kind, _ in numeric_captures):
        markers.add("http-error")
    return markers, numeric_captures


def _finish_failure_block(block: dict[str, Any]) -> dict[str, Any]:
    markers = sorted(block.pop("_markers"))
    numeric_captures = sorted(block.pop("_numeric_captures"))
    markers_truncated = len(markers) > MAX_FAILURE_MARKERS_PER_BLOCK
    numeric_truncated = len(numeric_captures) > MAX_FAILURE_MARKERS_PER_BLOCK
    block["markers"] = markers[:MAX_FAILURE_MARKERS_PER_BLOCK]
    block["numeric_captures"] = [
        {"kind": kind, "value": value}
        for kind, value in numeric_captures[:MAX_FAILURE_MARKERS_PER_BLOCK]
    ]
    block["markers_truncated"] = markers_truncated
    block["numeric_captures_truncated"] = numeric_truncated
    block["evidence_truncated"] = markers_truncated or numeric_truncated

    def encoded_size() -> int:
        return len(json.dumps(block, sort_keys=True, separators=(",", ":")))

    while encoded_size() > MAX_FAILURE_BLOCK_EVIDENCE_CHARS:
        if block["numeric_captures"]:
            block["numeric_captures"].pop()
            block["numeric_captures_truncated"] = True
        elif block["markers"]:
            block["markers"].pop()
            block["markers_truncated"] = True
        else:
            break
        block["evidence_truncated"] = True
    block["evidence_char_count"] = encoded_size()
    return block


def parse_failure_blocks(
    stdout: str, stderr: str, known_test_names: set[str]
) -> dict[str, Any]:
    """Extract bounded, safe observations from captured Rust failure blocks.

    Stream-local ordinals are deterministic provenance, not a cross-stream
    chronology claim. Captured block text is scanned but never retained.
    """

    parsed_blocks: list[dict[str, Any]] = []
    stream_metadata: dict[str, dict[str, int | str]] = {}
    for stream_name, source in (("stdout", stdout), ("stderr", stderr)):
        candidate_count = 0
        unrecognized_count = 0
        stream_block_count = 0
        active: dict[str, Any] | None = None

        def finish_active() -> None:
            nonlocal active
            if active is not None:
                parsed_blocks.append(_finish_failure_block(active))
                active = None

        for raw_line in source.splitlines(keepends=True):
            line = raw_line.rstrip("\r\n")
            if line == "failures:":
                finish_active()
                if stream_block_count:
                    break
                continue
            if _is_header_candidate(line):
                finish_active()
                candidate_count += 1
                match = FAILURE_HEADER_RE.fullmatch(line)
                if (
                    match is None
                    or not TEST_RE.fullmatch(match.group("name"))
                    or match.group("name") not in known_test_names
                ):
                    unrecognized_count += 1
                    continue
                stream_block_count += 1
                active = {
                    "name": match.group("name"),
                    "source_stream": stream_name,
                    "stream_ordinal": stream_block_count,
                    "channel": match.group("channel"),
                    "original_char_count": 0,
                    "_markers": set(),
                    "_numeric_captures": set(),
                }
                continue
            if active is not None:
                active["original_char_count"] += len(raw_line)
                markers, numeric_captures = _failure_markers_for_line(line)
                active["_markers"].update(markers)
                active["_numeric_captures"].update(numeric_captures)
        finish_active()
        status = (
            "parsed"
            if stream_block_count
            else "unrecognized"
            if candidate_count
            else "none"
        )
        stream_metadata[stream_name] = {
            "status": status,
            "source_char_count": len(source),
            "header_candidate_count": candidate_count,
            "parsed_header_count": stream_block_count,
            "unrecognized_header_count": unrecognized_count,
            "capture_truncated": False,
        }

    if parsed_blocks:
        source_status = "parsed"
    elif any(stream["header_candidate_count"] for stream in stream_metadata.values()):
        source_status = "unrecognized"
    else:
        source_status = "none"
    return {
        "source_status": source_status,
        "streams": stream_metadata,
        "blocks": parsed_blocks,
        "block_count": len(parsed_blocks),
        "unrecognized_block_count": sum(
            int(stream["unrecognized_header_count"])
            for stream in stream_metadata.values()
        ),
    }


def failure_evidence(
    request: dict[str, Any],
    known_test_names: set[str],
    stdout: str,
    stderr: str,
    summary_counts: dict[str, int] | None,
    combined_output: str,
) -> dict[str, Any]:
    """Build one bounded top-level failure observation without raw test text."""

    stream_outcomes = {
        "stdout": test_outcomes(stdout),
        "stderr": test_outcomes(stderr),
    }
    all_failed_names: list[str] = []
    failed_occurrences_by_name: dict[str, int] = {}
    for stream_name in ("stdout", "stderr"):
        for name, statuses in stream_outcomes[stream_name].items():
            failed_count = statuses.count("FAILED")
            if not failed_count:
                continue
            if name not in failed_occurrences_by_name:
                all_failed_names.append(name)
                failed_occurrences_by_name[name] = 0
            failed_occurrences_by_name[name] += failed_count

    safe_failed_names = [
        name
        for name in all_failed_names
        if TEST_RE.fullmatch(name) and name in known_test_names
    ]
    invalid_failed_name_count = len(all_failed_names) - len(safe_failed_names)
    requested_failed_names = [
        name
        for name in request["tests"]
        if name in failed_occurrences_by_name
        and TEST_RE.fullmatch(name)
        and name in known_test_names
    ]
    ordered_names = [
        *requested_failed_names,
        *(name for name in safe_failed_names if name not in requested_failed_names),
    ]

    blocks = parse_failure_blocks(stdout, stderr, known_test_names)
    requested_order = {
        name: index for index, name in enumerate(requested_failed_names)
    }
    stream_order = {"stdout": 0, "stderr": 1}
    requested_blocks = sorted(
        (
            block
            for block in blocks["blocks"]
            if block["name"] in requested_order
        ),
        key=lambda block: (
            requested_order[block["name"]],
            stream_order[block["source_stream"]],
            block["stream_ordinal"],
        ),
    )
    other_blocks = sorted(
        (
            block
            for block in blocks["blocks"]
            if block["name"] not in requested_order
        ),
        key=lambda block: (
            stream_order[block["source_stream"]],
            block["stream_ordinal"],
        ),
    )
    prioritized_blocks = [*requested_blocks, *other_blocks]
    selected_blocks = prioritized_blocks[:MAX_FAILURE_BLOCKS]
    for block in selected_blocks:
        block["requested_failed_selector"] = block["name"] in requested_order
        block["evidence_char_count"] = 0
        while True:
            actual_chars = len(json.dumps(block, sort_keys=True, separators=(",", ":")))
            if block["evidence_char_count"] == actual_chars:
                break
            block["evidence_char_count"] = actual_chars
    matched_requested_blocks = {block["name"] for block in requested_blocks}
    unmatched_requested = [
        name for name in requested_failed_names if name not in matched_requested_blocks
    ]

    summary_match_count = len(list(TEST_RESULT_RE.finditer(combined_output)))
    evidence: dict[str, Any] = {
        "source_status": blocks["source_status"],
        "streams": blocks["streams"],
        "failed_name_ordering": (
            "requested selectors first, then first-seen stdout and stderr outcomes"
        ),
        "parsed_block_count": blocks["block_count"],
        "emitted_block_count": len(selected_blocks),
        "omitted_block_count": blocks["block_count"] - len(selected_blocks),
        "unrecognized_block_count": blocks["unrecognized_block_count"],
        "block_ordering": (
            "requested failed selectors first, then stdout/stderr stream-local "
            "ordinals; not chronology"
        ),
        "blocks_truncated": len(selected_blocks) < blocks["block_count"],
        "blocks": selected_blocks,
        "failed_name_occurrence_count": sum(failed_occurrences_by_name.values()),
        "failed_name_distinct_count": len(failed_occurrences_by_name),
        "safe_failed_name_count": len(safe_failed_names),
        "unsafe_or_unknown_failed_name_count": invalid_failed_name_count,
        "failed_names": ordered_names[:MAX_FAILURE_NAMES],
        "failed_names_omitted_count": max(0, len(ordered_names) - MAX_FAILURE_NAMES),
        "failed_names_truncated": len(ordered_names) > MAX_FAILURE_NAMES,
        "requested_failed_selectors": requested_failed_names,
        "requested_failed_selectors_without_blocks": unmatched_requested,
        "cargo_summary_status": (
            "parsed"
            if summary_match_count == 1
            else "none"
            if summary_match_count == 0
            else "unrecognized"
        ),
        "cargo_summary_failed_count": (
            summary_counts["failed"] if summary_counts is not None else None
        ),
        "cargo_summary_channels": {
            "stdout": cargo_summary_channel_evidence(stdout),
            "stderr": cargo_summary_channel_evidence(stderr),
        },
        "capture_truncated": False,
        "upstream_output_truncation": "unknown",
    }

    evidence["structured_evidence_bytes"] = 0
    while True:
        actual_size = len(json.dumps(evidence, sort_keys=True).encode("utf-8"))
        if actual_size <= MAX_FAILURE_EVIDENCE_BYTES:
            if evidence["structured_evidence_bytes"] != actual_size:
                evidence["structured_evidence_bytes"] = actual_size
                continue
            break
        if evidence["failed_names"]:
            evidence["failed_names"].pop()
            evidence["failed_names_omitted_count"] += 1
            evidence["failed_names_truncated"] = True
            continue
        if evidence["blocks"]:
            evidence["blocks"].pop()
            evidence["emitted_block_count"] -= 1
            evidence["omitted_block_count"] += 1
            evidence["blocks_truncated"] = True
            continue
        break
    return evidence


def matched_test_evidence(lines: list[str]) -> dict[str, Any]:
    """Bound selector evidence while retaining the total exact line count."""

    excerpts = lines[:MAX_MATCHED_TEST_LINES]
    bounded_lines = [line[:MAX_MATCHED_TEST_LINE_CHARS] for line in excerpts]
    return {
        "matched_line_count": len(lines),
        "matched_lines": bounded_lines,
        "matched_lines_truncated": len(lines) > MAX_MATCHED_TEST_LINES
        or any(len(line) > MAX_MATCHED_TEST_LINE_CHARS for line in excerpts),
    }


def run_request(request: dict[str, Any], repo_root: Path) -> dict[str, Any]:
    manifest_root = repo_root / "codex-rs"
    try:
        command_record = select_target(request, load_manifest(repo_root))
    except ValueError as exc:
        return fail("target_selector_unknown", str(exc))
    runtime_preparation_only_value = os.environ.get(RUNTIME_PREPARATION_ONLY_ENV)
    if runtime_preparation_only_value is None:
        runtime_preparation_only = False
    elif runtime_preparation_only_value == "true":
        runtime_preparation_only = True
    elif runtime_preparation_only_value == "false":
        runtime_preparation_only = False
    else:
        return {
            "schema_version": SCHEMA_VERSION,
            "status": "failure",
            "result_kind": "runtime_preflight",
            "failure_code": "runtime_preparation_mode_invalid",
            "message": "runtime preparation mode must be exactly 'true' or 'false'",
            "request": request,
            "request_fingerprint": hashlib.sha256(
                json.dumps(
                    request, sort_keys=True, separators=(",", ":")
                ).encode("utf-8")
            ).hexdigest(),
            "inventory": {"status": "not-run", "tests": []},
            "tests": [],
        }
    request_fingerprint = hashlib.sha256(
        json.dumps(request, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()
    if runtime_preparation_only and (
        request["package"],
        request["target_kind"],
        request["target"],
    ) != CORE_RUNTIME_TARGET:
        return {
            "schema_version": SCHEMA_VERSION,
            "status": "failure",
            "result_kind": "runtime_preflight",
            "failure_code": "runtime_preparation_only_unsupported_target",
            "message": (
                "runtime preparation only supports codex-core integration target all"
            ),
            "request": request,
            "request_fingerprint": request_fingerprint,
            "inventory": {"status": "not-run", "tests": []},
            "tests": [],
        }
    env = os.environ.copy()
    env.pop(RUNTIME_PREPARATION_ONLY_ENV, None)
    # These are the established hosted-runner contracts.  Do not accept them
    # from the request: the request selects tests, never runner capabilities.
    env.setdefault("RUST_MIN_STACK", "8388608")
    source_sha = ""
    runtime_preparation = None
    if (
        request["package"],
        request["target_kind"],
        request["target"],
    ) == CORE_RUNTIME_TARGET:
        source_sha = git_sha(repo_root)
        runtime_preparation = prepare_core_integration_runtime(
            request, manifest_root, env, source_sha
        )
    if runtime_preparation is not None and runtime_preparation["status"] != "success":
        return {
            "schema_version": SCHEMA_VERSION,
            "status": "failure",
            **(
                {"result_kind": "runtime_preflight"}
                if runtime_preparation_only
                else {}
            ),
            "failure_code": "runtime_preparation_failed",
            "message": "required core integration runtime preparation failed",
            "request": request,
            "request_fingerprint": request_fingerprint,
            "candidate_sha": source_sha,
            "runtime_preparation": runtime_preparation,
            "inventory": {"status": "not-run", "tests": []},
            "tests": [],
        }
    if runtime_preparation_only:
        return {
            "schema_version": SCHEMA_VERSION,
            "status": "success",
            "result_kind": "runtime_preflight",
            "request": request,
            "request_fingerprint": request_fingerprint,
            "candidate_sha": source_sha,
            "runtime_preparation": runtime_preparation,
            "inventory": {"status": "not-run", "tests": []},
            "tests": [],
        }
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
        "request_fingerprint": request_fingerprint,
        "candidate_sha": source_sha or git_sha(repo_root),
        **(
            {"runtime_preparation": runtime_preparation}
            if runtime_preparation is not None
            else {}
        ),
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
    # Run the complete immutable target command.  This keeps the command
    # surface closed while the requested names remain exact post-run selectors.
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
    counts = test_result_counts(output)
    outcomes = test_outcomes(output)
    outcome_lines = test_outcome_lines(output)
    result["failure_evidence"] = failure_evidence(
        request,
        {name for name in names if TEST_RE.fullmatch(name)},
        completed.stdout,
        completed.stderr,
        counts,
        output,
    )
    for name in request["tests"]:
        observed = outcomes.get(name, [])
        matched_lines = outcome_lines.get(name, [])
        outcome = observed[0] if len(observed) == 1 else ""
        execution_reconciled = (
            completed.returncode == 0
            and counts is not None
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
        elif counts is None:
            failure_code = "execution_reconciliation_failed"
        result["tests"].append(
            {
                "name": name,
                "status": status,
                "exit_code": completed.returncode,
                "execution_reconciled": execution_reconciled,
                "observed_outcomes": observed,
                **matched_test_evidence(matched_lines),
                "result_counts": counts,
                "diagnostics": command_diagnostics(completed),
            }
        )
        if status != "success" and result["status"] == "success":
            result["status"] = "failure"
            result["failure_code"] = failure_code
            result["message"] = (
                "named test did not produce exactly one non-ignored passing result; "
                "only bounded captured diagnostics are retained"
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
    if sys.argv[1:] == ["--requires-core-runtime"]:
        try:
            manifest = load_manifest(Path.cwd().resolve())
        except ValueError:
            manifest = {}
        print(
            str(
                request_requires_core_runtime(
                    os.environ.get("RUST_TEST_REQUEST_JSON", ""),
                    os.environ.get("VALIDATION_PROFILE", ""),
                    manifest,
                )
            ).lower()
        )
        return 0
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
