#!/usr/bin/env python3
"""Run one packaged Browser fixture and publish only sanitized test status."""

from __future__ import annotations

import json
import os
import signal
import subprocess
import xml.etree.ElementTree as ET
from pathlib import Path


EXPECTED_TEST = "test_packaged_tui_browser_output_keeps_images_and_manifest_metadata_separate"
DIAGNOSTIC_PROPERTY = "browser_output_diagnostic_json"
MAX_JUNIT_BYTES = 4 * 1024 * 1024
MAX_DIAGNOSTIC_BYTES = 32 * 1024
MAX_RUNTIME_SECONDS = 20 * 60
MAX_DIAGNOSTIC_COUNT = 1024

BROWSER_STAGE_FIELDS = {
    "browser_provider": {
        "call_id_matches_fixture",
        "provider_process_exit_success",
        "provider_json_parse_success",
        "provider_content_item_count",
    },
    "app_server_response": {
        "call_id_matches_fixture",
        "app_server_response_accepted",
        "app_server_response_submitted",
        "app_server_accepted_item_count",
    },
    "core_function_output": {
        "call_id_matches_fixture",
        "core_response_received",
        "core_function_output_constructed",
        "core_output_item_count",
    },
    "core_history_recorded": {
        "call_id_matches_fixture",
        "function_call_count",
        "function_call_output_count",
    },
    "normalized_prompt_input": {
        "call_id_matches_fixture",
        "function_call_count",
        "function_call_output_count",
    },
    "responses_api_request_built": {
        "call_id_matches_fixture",
        "function_call_count",
        "function_call_output_count",
    },
    "pre_transport_request_input": {
        "call_id_matches_fixture",
        "function_call_count",
        "function_call_output_count",
    },
}


def _required_env(name: str) -> str:
    value = os.environ.get(name, "")
    if not value:
        raise ValueError("input_missing")
    return value


def _child_environment(source_root: Path) -> dict[str, str]:
    environment = dict(os.environ)
    environment["PYTHONPATH"] = os.pathsep.join(
        (
            str(source_root / "sdk/python/src"),
            str(source_root / "sdk/python/tests"),
        )
    )
    for key in tuple(environment):
        if key in {
            "RESULT_PATH",
            "JUNIT_PATH",
            "ARTIFACT_DIR",
            "SOURCE_DIR",
            "RUNNER_TEMP",
            "EXPECTED_SOURCE_SHA",
            "EXPECTED_WORKFLOW_SHA",
            "EXPECTED_TARGET",
            "GITHUB_RUN_ID",
            "GITHUB_SHA",
            "GITHUB_WORKSPACE",
            "GITHUB_REPOSITORY",
            "GITHUB_SERVER_URL",
        } or key.startswith("CODEX_BROWSER_"):
            environment.pop(key, None)
    return environment


def _write_result_exclusive(path: Path, result: dict[str, object]) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if not hasattr(os, "O_NOFOLLOW"):
        raise OSError("safe result creation is unavailable")
    flags |= os.O_NOFOLLOW
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(path, flags, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        output.write(json.dumps(result, sort_keys=True, indent=2) + "\n")


def _bounded_count(value: object) -> bool:
    return type(value) is int and 0 <= value <= MAX_DIAGNOSTIC_COUNT


def _sanitize_browser_diagnostic(case: ET.Element) -> dict[str, object] | None:
    properties = [
        prop for prop in case.iter("property")
        if prop.get("name") == DIAGNOSTIC_PROPERTY
    ]
    if len(properties) != 1:
        return None
    encoded = properties[0].get("value")
    if not isinstance(encoded, str) or len(encoded.encode("utf-8")) > MAX_DIAGNOSTIC_BYTES:
        return None
    try:
        payload = json.loads(encoded)
    except json.JSONDecodeError:
        return None
    expected_keys = {
        "fixture_function_call_emitted",
        "responses_request_count",
        "second_request_observed",
        "second_request_input_available",
        "function_call_output_count",
        "function_call_output_call_ids",
        "function_call_output_tool_names",
        "synthetic_browser_provider_invoked",
        "synthetic_browser_provider_tool_name",
        "source_stage_observations",
    }
    if not isinstance(payload, dict) or set(payload) != expected_keys:
        return None
    bool_fields = (
        "fixture_function_call_emitted",
        "second_request_observed",
        "second_request_input_available",
        "synthetic_browser_provider_invoked",
    )
    if any(type(payload.get(field)) is not bool for field in bool_fields):
        return None
    count_fields = ("responses_request_count", "function_call_output_count")
    if any(not _bounded_count(payload.get(field)) for field in count_fields):
        return None
    stage_observations = payload.get("source_stage_observations")
    if not isinstance(stage_observations, list) or len(stage_observations) > 32:
        return None

    safe_stages: list[dict[str, object]] = []
    for observation in stage_observations:
        if not isinstance(observation, dict):
            return None
        stage = observation.get("stage")
        fields = BROWSER_STAGE_FIELDS.get(stage) if isinstance(stage, str) else None
        if fields is None or set(observation) != fields | {"stage"}:
            return None
        count_keys = {
            key for key in fields if key.endswith("_item_count") or key.endswith("_count")
        }
        bool_keys = fields - count_keys
        if any(type(observation.get(key)) is not bool for key in bool_keys):
            return None
        if any(not _bounded_count(observation.get(key)) for key in count_keys):
            return None
        safe_stages.append(
            {
                "stage": stage,
                **{key: observation[key] for key in sorted(fields)},
            }
        )

    # Call IDs, tool names, paths, and all other producer-supplied strings are
    # deliberately excluded from the public result.
    return {
        "fixture_function_call_emitted": payload["fixture_function_call_emitted"],
        "responses_request_count": payload["responses_request_count"],
        "second_request_observed": payload["second_request_observed"],
        "second_request_input_available": payload["second_request_input_available"],
        "function_call_output_count": payload["function_call_output_count"],
        "synthetic_browser_provider_invoked": payload["synthetic_browser_provider_invoked"],
        "source_stage_observations": safe_stages,
    }


def _junit_result(path: Path, *, return_code: int | None) -> dict[str, object]:
    if not path.is_file() or path.is_symlink():
        return {
            "result": "incomplete",
            "failure_code": "junit_missing",
            "observed_test_count": 0,
            "passed": 0,
            "failed": 0,
            "errors": 0,
            "skipped": 0,
        }
    if path.stat().st_size > MAX_JUNIT_BYTES:
        return {
            "result": "incomplete",
            "failure_code": "junit_size_limit",
            "observed_test_count": 0,
            "passed": 0,
            "failed": 0,
            "errors": 0,
            "skipped": 0,
        }
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError):
        return {
            "result": "incomplete",
            "failure_code": "junit_invalid",
            "observed_test_count": 0,
            "passed": 0,
            "failed": 0,
            "errors": 0,
            "skipped": 0,
        }
    cases = [case for case in root.iter("testcase") if case.get("name") == EXPECTED_TEST]
    passed = failed = errors = skipped = 0
    for case in cases:
        child_tags = {child.tag for child in case}
        if "error" in child_tags:
            errors += 1
        elif "failure" in child_tags:
            failed += 1
        elif "skipped" in child_tags:
            skipped += 1
        else:
            passed += 1
    result = "passed" if return_code == 0 and len(cases) == 1 and passed == 1 else "failed"
    failure_code = None if result == "passed" else (
        "pytest_timeout" if return_code is None else
        "expected_test_not_unique" if len(cases) != 1 else
        "pytest_nonzero_exit" if return_code != 0 and failed + errors + skipped == 0 else
        "browser_consumer_assertion_failed"
    )
    result_payload: dict[str, object] = {
        "result": result,
        "failure_code": failure_code,
        "observed_test_count": len(cases),
        "passed": passed,
        "failed": failed,
        "errors": errors,
        "skipped": skipped,
    }
    if len(cases) == 1:
        safe_diagnostic = _sanitize_browser_diagnostic(cases[0])
        if safe_diagnostic is not None:
            result_payload["diagnostic"] = safe_diagnostic
        else:
            result_payload["diagnostic_status"] = "suppressed_or_unavailable"
    return result_payload


def _terminate_group(process: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except OSError:
        if process.poll() is None:
            process.kill()
    if process.poll() is None:
        process.wait()


def main() -> int:
    result_path = Path(_required_env("RESULT_PATH"))
    junit_path = Path(_required_env("JUNIT_PATH"))
    source_root = Path(_required_env("SOURCE_DIR")).resolve()
    target = _required_env("EXPECTED_TARGET")
    source_sha = _required_env("EXPECTED_SOURCE_SHA")
    workflow_sha = _required_env("EXPECTED_WORKFLOW_SHA")
    run_id = _required_env("GITHUB_RUN_ID")
    pytest_project = source_root / "scripts/codex_package/smoke_tests"
    selector = (
        "first_binary/test_tui_agents_acceptance.py::"
        f"{EXPECTED_TEST}"
    )
    command = [
        "uv",
        "run",
        "--frozen",
        "pytest",
        "-q",
        selector,
        "--junitxml",
        str(junit_path),
        "--compression",
        "gzip",
        "--package-target",
        target,
        "--cli-archive",
        str(Path(_required_env("ARTIFACT_DIR")) / "codex-package.tar.gz"),
        "--app-server-archive",
        str(Path(_required_env("ARTIFACT_DIR")) / "codex-app-server-package.tar.gz"),
        "--symbols-archive",
        # The upstream conftest requires this option, but this exact Browser
        # selector does not request the symbol fixture; no symbol archive is
        # produced, bound, extracted, or qualified by this route.
        str(Path(_required_env("RUNNER_TEMP")) / "symbols-excluded-not-qualified.tar.gz"),
    ]
    environment = _child_environment(source_root)
    process: subprocess.Popen[bytes] | None = None
    return_code: int | None = None
    try:
        junit_path.unlink(missing_ok=True)
        process = subprocess.Popen(
            command,
            cwd=pytest_project,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            return_code = process.wait(timeout=MAX_RUNTIME_SECONDS)
        except subprocess.TimeoutExpired:
            _terminate_group(process)
            return_code = None
    except OSError:
        return_code = 127
    finally:
        if process is not None and process.poll() is None:
            _terminate_group(process)

    result = {
        "schema_version": "sedna-browser-smoke-result-v1",
        "source_sha": source_sha,
        "workflow_sha": workflow_sha,
        "run_id": run_id,
        "target": target,
        **_junit_result(junit_path, return_code=return_code),
    }
    _write_result_exclusive(result_path, result)
    junit_path.unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
