#!/usr/bin/env python3
"""Join exact package and Browser statuses into one fixed, sanitized result."""

from __future__ import annotations

import json
import os
import re
import stat
import sys
from pathlib import Path
from typing import Any


MAX_RESULT_BYTES = 64 * 1024
SOURCE_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
TARGETS = {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"}
CHECK_NAMES = (
    "package_identity",
    "package_archives",
    "packaged_cli_version_command",
    "progressive_cli_identity",
    "device_auth_help_surface",
    "code_mode_host_help",
    "packaged_companions",
    "app_server_manifest",
    "app_server_executable_invoked",
    "smoke_symbols_omitted",
    "browser_consumer",
)
RESULT_KEYS = {
    "schema_version",
    "source_sha",
    "workflow_sha",
    "run_id",
    "target",
    "checks",
    "failure_codes",
}


def _read_result(path: Path) -> dict[str, Any] | None:
    if not hasattr(os, "O_NOFOLLOW"):
        return None
    flags = os.O_RDONLY | os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
        with os.fdopen(descriptor, "rb") as stream:
            metadata = os.fstat(stream.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > MAX_RESULT_BYTES:
                return None
            payload = json.loads(stream.read(MAX_RESULT_BYTES + 1).decode("utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return None
    return payload if isinstance(payload, dict) else None


def _matches_identity(
    payload: dict[str, Any] | None,
    *,
    schema: str,
    source_sha: str,
    workflow_sha: str,
    run_id: str,
    target: str,
) -> bool:
    return bool(
        payload
        and payload.get("schema_version") == schema
        and payload.get("source_sha") == source_sha
        and payload.get("workflow_sha") == workflow_sha
        and payload.get("run_id") == run_id
        and payload.get("target") == target
    )


def _valid_package_shape(payload: dict[str, Any] | None) -> bool:
    if payload is None:
        return False
    base = {"schema_version", "source_sha", "workflow_sha", "run_id", "target"}
    success = base | {
        "package_archives_verified",
        "cli_version_command_ok",
        "cli_version_class",
        "progressive_identity_matches",
        "device_auth_help_ok",
        "code_mode_host_help_ok",
        "cli_companion_inventory_ok",
        "app_server_package_manifest_ok",
        "app_server_help_ok",
        "symbols_artifact",
    }
    failure = base | {"package_archives_verified", "failure_code"}
    if set(payload) == failure:
        return (
            payload.get("package_archives_verified") is False
            and isinstance(payload.get("failure_code"), str)
            and re.fullmatch(r"[a-z][a-z0-9_]{0,63}", payload["failure_code"]) is not None
        )
    return (
        set(payload) == success
        and payload.get("schema_version") == "sedna-smoke-consumer-v2"
        and type(payload.get("package_archives_verified")) is bool
        and type(payload.get("cli_version_command_ok")) is bool
        and isinstance(payload.get("cli_version_class"), str)
        and type(payload.get("progressive_identity_matches")) is bool
        and type(payload.get("device_auth_help_ok")) is bool
        and type(payload.get("code_mode_host_help_ok")) is bool
        and type(payload.get("cli_companion_inventory_ok")) is bool
        and type(payload.get("app_server_package_manifest_ok")) is bool
        and type(payload.get("app_server_help_ok")) is bool
        and payload.get("symbols_artifact") == "excluded_not_sanitized_or_qualified"
    )


def _browser_passed(payload: dict[str, Any] | None) -> bool:
    if payload is None:
        return False
    base = {"schema_version", "source_sha", "workflow_sha", "run_id", "target"}
    result_fields = {
        "result",
        "failure_code",
        "observed_test_count",
        "passed",
        "failed",
        "errors",
        "skipped",
    }
    allowed = base | result_fields | {"diagnostic", "diagnostic_status"}
    return (
        set(payload) <= allowed
        and base | result_fields <= set(payload)
        and payload.get("result") == "passed"
        and payload.get("failure_code") is None
        and type(payload.get("observed_test_count")) is int
        and payload.get("observed_test_count") == 1
        and type(payload.get("passed")) is int
        and payload.get("passed") == 1
        and all(type(payload.get(name)) is int and payload[name] == 0 for name in ("failed", "errors", "skipped"))
        and (
            "diagnostic" not in payload
            or isinstance(payload["diagnostic"], dict)
        )
        and (
            "diagnostic_status" not in payload
            or payload["diagnostic_status"] == "suppressed_or_unavailable"
        )
    )


def _write_result_exclusive(path: Path, payload: dict[str, Any]) -> None:
    if not hasattr(os, "O_NOFOLLOW"):
        raise OSError("safe result creation is unavailable")
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        stream.write(json.dumps(payload, sort_keys=True, indent=2) + "\n")


def _identity_from_environment() -> tuple[str, str, str, str]:
    source_sha = os.environ["EXPECTED_SOURCE_SHA"]
    workflow_sha = os.environ["EXPECTED_WORKFLOW_SHA"]
    run_id = os.environ["GITHUB_RUN_ID"]
    target = os.environ["EXPECTED_TARGET"]
    if (
        not SOURCE_SHA_RE.fullmatch(source_sha)
        or not SOURCE_SHA_RE.fullmatch(workflow_sha)
        or not run_id.isascii()
        or not run_id.isdecimal()
        or target not in TARGETS
    ):
        raise ValueError("invalid run identity")
    return source_sha, workflow_sha, run_id, target


def _build_result() -> dict[str, Any]:
    source_sha, workflow_sha, run_id, target = _identity_from_environment()
    package = _read_result(Path(os.environ["PACKAGE_RESULT_PATH"]))
    browser = _read_result(Path(os.environ["BROWSER_RESULT_PATH"]))
    package_identity_ok = (
        _matches_identity(
            package,
            schema="sedna-smoke-consumer-v2",
            source_sha=source_sha,
            workflow_sha=workflow_sha,
            run_id=run_id,
            target=target,
        )
        and _valid_package_shape(package)
    )
    browser_identity_ok = _matches_identity(
        browser,
        schema="sedna-browser-smoke-result-v1",
        source_sha=source_sha,
        workflow_sha=workflow_sha,
        run_id=run_id,
        target=target,
    )
    checks = {
        "package_identity": package_identity_ok,
        "package_archives": bool(package_identity_ok and package.get("package_archives_verified") is True),
        "packaged_cli_version_command": bool(package_identity_ok and package.get("cli_version_command_ok") is True),
        "progressive_cli_identity": bool(package_identity_ok and package.get("progressive_identity_matches") is True),
        "device_auth_help_surface": bool(package_identity_ok and package.get("device_auth_help_ok") is True),
        "code_mode_host_help": bool(package_identity_ok and package.get("code_mode_host_help_ok") is True),
        "packaged_companions": bool(package_identity_ok and package.get("cli_companion_inventory_ok") is True),
        "app_server_manifest": bool(package_identity_ok and package.get("app_server_package_manifest_ok") is True),
        "app_server_executable_invoked": bool(package_identity_ok and package.get("app_server_help_ok") is True),
        "smoke_symbols_omitted": bool(
            package_identity_ok
            and package.get("symbols_artifact") == "excluded_not_sanitized_or_qualified"
        ),
        "browser_consumer": bool(browser_identity_ok and _browser_passed(browser)),
    }
    return {
        "schema_version": "sedna-smoke-consumer-aggregate-v2",
        "source_sha": source_sha,
        "workflow_sha": workflow_sha,
        "run_id": run_id,
        "target": target,
        "checks": checks,
        "failure_codes": [name for name in CHECK_NAMES if not checks[name]],
    }


def _valid_aggregate(path: Path) -> bool:
    result = _read_result(path)
    if result is None or set(result) != RESULT_KEYS:
        return False
    try:
        source_sha, workflow_sha, run_id, target = _identity_from_environment()
    except (KeyError, ValueError):
        return False
    checks = result.get("checks")
    failures = result.get("failure_codes")
    if (
        result.get("schema_version") != "sedna-smoke-consumer-aggregate-v2"
        or result.get("source_sha") != source_sha
        or result.get("workflow_sha") != workflow_sha
        or result.get("run_id") != run_id
        or result.get("target") != target
        or not isinstance(checks, dict)
        or set(checks) != set(CHECK_NAMES)
        or any(type(checks.get(name)) is not bool for name in CHECK_NAMES)
        or not isinstance(failures, list)
        or failures != [name for name in CHECK_NAMES if not checks[name]]
    ):
        return False
    return not failures


def main() -> int:
    arguments = sys.argv[1:]
    result_path = Path(os.environ["FINAL_RESULT_PATH"])
    if arguments == ["--verify"]:
        return 0 if _valid_aggregate(result_path) else 1
    if arguments:
        return 2
    try:
        result = _build_result()
        _write_result_exclusive(result_path, result)
    except (KeyError, OSError, ValueError):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
