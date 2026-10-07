#!/usr/bin/env python3
"""Join exact package and Browser-consumer statuses without raw test output."""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any


MAX_RESULT_BYTES = 64 * 1024


def _read_result(path: Path) -> dict[str, Any] | None:
    try:
        if path.is_symlink() or not path.is_file() or path.stat().st_size > MAX_RESULT_BYTES:
            return None
        payload = json.loads(path.read_text(encoding="utf-8"))
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


def main() -> int:
    source_sha = os.environ["EXPECTED_SOURCE_SHA"]
    workflow_sha = os.environ["EXPECTED_WORKFLOW_SHA"]
    run_id = os.environ["GITHUB_RUN_ID"]
    target = os.environ["EXPECTED_TARGET"]
    package = _read_result(Path(os.environ["PACKAGE_RESULT_PATH"]))
    browser = _read_result(Path(os.environ["BROWSER_RESULT_PATH"]))
    package_identity_ok = _matches_identity(
        package,
        schema="sedna-smoke-consumer-v1",
        source_sha=source_sha,
        workflow_sha=workflow_sha,
        run_id=run_id,
        target=target,
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
        "browser_consumer": bool(browser_identity_ok and browser.get("result") == "passed"),
    }
    failure_codes = [name for name, passed in checks.items() if not passed]
    result = {
        "schema_version": "sedna-smoke-consumer-aggregate-v1",
        "source_sha": source_sha,
        "workflow_sha": workflow_sha,
        "run_id": run_id,
        "target": target,
        "checks": checks,
        "failure_codes": failure_codes,
    }
    Path(os.environ["FINAL_RESULT_PATH"]).write_text(
        json.dumps(result, sort_keys=True, indent=2) + "\n", encoding="utf-8"
    )
    return 0 if not failure_codes else 1


if __name__ == "__main__":
    raise SystemExit(main())
