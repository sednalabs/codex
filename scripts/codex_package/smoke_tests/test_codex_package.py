"""Exercise each assembled package in the order its shipped files appear.

ROOT_OF_EXTRACTED_PACKAGE
├── bin
│   ├── codex[.exe]                       # CLI package only
│   ├── codex-app-server[.exe]            # app-server package only
│   └── codex-code-mode-host[.exe]
├── codex-package.json
├── codex-path
│   └── rg[.exe]
└── codex-resources
    ├── bwrap                             # Linux only
    ├── codex-command-runner.exe          # Windows only
    ├── codex-windows-sandbox-setup.exe   # Windows only
    └── zsh/bin/zsh                       # supported Unix targets only

Debug symbols for all shipped binaries arrive in a separate companion archive.
Each package contains one entrypoint, not both codex and codex-app-server.
"""

import hashlib
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tomllib
from contextlib import closing, contextmanager
from dataclasses import replace
from pathlib import Path
from typing import Iterator
from unittest.mock import patch

import pytest
from app_server_harness import (
    CapturedResponsesRequest,
    MockResponsesServer,
    ev_assistant_message,
    ev_completed,
    ev_completed_with_usage,
    ev_response_created,
    sse,
)
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage


@pytest.mark.parametrize(
    ("arguments", "expected"),
    [
        pytest.param(("--help",), "Usage:", id="help"),
        pytest.param(("features", "list"), "code_mode", id="features"),
        pytest.param(("completion", "bash"), "codex", id="completion"),
    ],
)
def test_cli_public_commands(
    package: SmokePackage,
    arguments: tuple[str, ...],
    expected: str,
) -> None:
    """Packaged CLI remains usable for discovery, features, and completions."""
    output = package.run(*arguments).stdout
    assert expected in output


@pytest.mark.skipif(
    sys.platform != "linux",
    reason="Instruction consumer is scoped to the disposable Linux first-package runner",
)
def test_cli_instruction_inspection_with_synthetic_linux_configuration(
    package: SmokePackage,
    tmp_path: Path,
) -> None:
    """Exercise the shipped CLI in the disposable Linux first-package consumer."""
    home = tmp_path / "home"
    config_home = home / "codex"
    workspace = tmp_path / "workspace"
    temporary = tmp_path / "temporary"
    for directory in (config_home, workspace, temporary):
        directory.mkdir(parents=True)
    # The resolver recognizes .git/HEAD as a checkout boundary. Keep project
    # discovery inside this fresh workspace without invoking an external git.
    (workspace / ".git").mkdir()
    (workspace / ".git" / "HEAD").write_text("ref: refs/heads/main\n", encoding="utf-8")

    # Keep the exact package entrypoint, but do not inherit credentials, provider
    # settings, remote-control selectors, or runtime-proof configuration.
    environment = {
        key: package.environment[key]
        for key in ("PATH", "SystemRoot", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT")
        if key in package.environment
    }
    if "PATH" not in environment:
        pytest.fail("Packaged CLI PATH is unavailable", pytrace=False)
    environment.update(
        CODEX_HOME=str(config_home),
        HOME=str(home),
        USERPROFILE=str(home),
        XDG_CONFIG_HOME=str(home / "xdg-config"),
        APPDATA=str(home / "app-data"),
        LOCALAPPDATA=str(home / "local-app-data"),
        PROGRAMDATA=str(home / "program-data"),
        TMPDIR=str(temporary),
        TMP=str(temporary),
        TEMP=str(temporary),
        NO_PROXY="127.0.0.1,localhost",
        no_proxy="127.0.0.1,localhost",
    )
    isolated = replace(package, directory=workspace, environment=environment)
    config_path = config_home / "config.toml"
    unopened_path = workspace / "missing-instruction-target.md"
    configuration = (
        'instructions = "PACKAGE_BASE_MARKER"\n'
        'developer_instructions = "PACKAGE_DEVELOPER_MARKER"\n'
        f"model_instructions_file = {json.dumps(str(unopened_path))}\n"
        "[features.multi_agent_v2]\n"
        'subagent_developer_instructions = "PACKAGE_CHILD_MARKER"\n'
    )
    config_path.write_text(configuration, encoding="utf-8")

    def run_report(*arguments: str, stdin: str | None = None) -> tuple[dict, str]:
        # Synthetic HOME does not redirect Unix system configuration. This
        # canonical Linux consumer must fail closed before either invocation;
        # lstat also rejects dangling symlinks without reading their targets.
        for name in ("config.toml", "requirements.toml", "managed_config.toml"):
            try:
                (Path("/etc/codex") / name).lstat()
            except FileNotFoundError:
                continue
            except OSError:
                pytest.fail("System configuration absence is unverified", pytrace=False)
            pytest.fail(
                "Instruction consumer requires absent system configuration",
                pytrace=False,
            )
        try:
            completed = isolated.run("debug", "instructions", *arguments, stdin=stdin)
            report = json.loads(completed.stdout)
        except (OSError, subprocess.SubprocessError, ValueError, UnicodeError):
            pytest.fail("Packaged instruction inspection failed", pytrace=False)
        if not isinstance(report, dict):
            pytest.fail(
                "Packaged instruction report has an invalid shape", pytrace=False
            )
        return report, completed.stdout

    def inline_fields(report: dict) -> dict:
        values = report.get("inline_configuration")
        if not isinstance(values, list):
            return {}
        return {
            value["key"]: value
            for value in values
            if isinstance(value, dict) and isinstance(value.get("key"), str)
        }

    def section(report: dict, key: str) -> dict:
        value = report.get(key)
        return value if isinstance(value, dict) else {}

    default, default_output = run_report()
    default_fields = inline_fields(default)
    base = default_fields.get("instructions", {})
    origin = base.get("origin")
    model_file = default_fields.get("model_instructions_file", {})
    default_checks = (
        isinstance(origin, dict)
        and origin.get("kind") == "user"
        and origin.get("file") == str(config_path),
        base.get("present") is True,
        default_fields.get("developer_instructions", {}).get("present") is True,
        default_fields.get(
            "features.multi_agent_v2.subagent_developer_instructions", {}
        ).get("present")
        is True,
        len(default_fields) == 8,
        all("content" not in field for field in default_fields.values()),
        not any(
            marker in default_output
            for marker in (
                "PACKAGE_BASE_MARKER",
                "PACKAGE_DEVELOPER_MARKER",
                "PACKAGE_CHILD_MARKER",
            )
        ),
        model_file.get("configured_path") == str(unopened_path),
        model_file.get("content_status") == "not_opened; configured path only",
        section(default, "caller_supplied_text").get("origin") == "not read",
        default.get("scope")
        == "selected local merged configuration; not a complete effective session prompt",
        default.get("unknown_or_not_inspected")
        == [
            "cloud-managed additions (cloud callback is no-op)",
            "stored thread history, current model template, runtime conversation and provider-hidden instructions",
            "AGENTS.md and user/provider instruction discovery",
            "model_instructions_file target contents and final baseline fallback",
        ],
    )
    if not all(default_checks):
        pytest.fail("Default packaged instruction projection failed", pytrace=False)

    explicit, explicit_output = run_report(
        "--show-content",
        "--client-instructions-stdin",
        stdin="PACKAGE_CALLER_MARKER\npassword=PACKAGE_PRIVATE_MARKER\n",
    )
    explicit_fields = inline_fields(explicit)
    caller = section(explicit, "caller_supplied_text")
    managed = section(explicit, "local_managed_additional_developer_instructions")
    warning = explicit.get("publication_warning")
    explicit_checks = (
        explicit_fields.get("instructions", {}).get("content") == "PACKAGE_BASE_MARKER",
        explicit_fields.get("developer_instructions", {}).get("content")
        == "PACKAGE_DEVELOPER_MARKER",
        caller.get("origin") == "caller-supplied; unverified",
        caller.get("content") == "PACKAGE_CALLER_MARKER\n[REDACTED]\n",
        caller.get("redacted_lines") == 1,
        "PACKAGE_PRIVATE_MARKER" not in explicit_output,
        managed.get("content")
        == "excluded; local-managed instruction content is never displayed",
        isinstance(warning, str)
        and "heuristic" in warning
        and "not guaranteed" in warning,
    )
    if not all(explicit_checks):
        pytest.fail("Explicit packaged instruction projection failed", pytrace=False)
    state_created = any(
        path.name in {"auth.json", "history.jsonl", "sessions"}
        or ".sqlite" in path.name
        or path.name.endswith(".db")
        for path in config_home.rglob("*")
    )
    config_unchanged = config_path.read_text(encoding="utf-8") == configuration
    target_unopened = not unopened_path.exists()
    assert not state_created, "Instruction inspection created auth or session state"
    assert config_unchanged, "Instruction inspection changed its synthetic config"
    assert target_unopened, "Instruction inspection created its configured target"


@pytest.mark.skipif(
    sys.platform != "linux",
    reason="Usage lineage consumer is scoped to the disposable Linux package runner",
)
def test_packaged_usage_report_covers_recorded_synthetic_spawn_tree(
    package: SmokePackage,
    tmp_path: Path,
    record_property,
) -> None:
    """A packaged root/child spawn is visible to the adjacent bounded reporter."""
    home = tmp_path / "home"
    codex_home = home / "codex"
    workspace = tmp_path / "workspace"
    temporary = tmp_path / "temporary"
    for directory in (codex_home, workspace, temporary):
        directory.mkdir(parents=True)
    (workspace / ".git").mkdir()
    (workspace / ".git" / "HEAD").write_text("ref: refs/heads/main\n", encoding="utf-8")

    environment = {
        key: package.environment[key]
        for key in ("PATH", "SystemRoot", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT")
        if key in package.environment
    }
    environment.update(
        CODEX_HOME=str(codex_home),
        CODEX_SQLITE_HOME=str(codex_home),
        HOME=str(home),
        USERPROFILE=str(home),
        XDG_CONFIG_HOME=str(home / "xdg-config"),
        APPDATA=str(home / "app-data"),
        LOCALAPPDATA=str(home / "local-app-data"),
        PROGRAMDATA=str(home / "program-data"),
        TMPDIR=str(temporary),
        TMP=str(temporary),
        TEMP=str(temporary),
    )
    config_path = codex_home / "config.toml"

    def require_system_configuration_absent() -> None:
        for name in ("config.toml", "requirements.toml", "managed_config.toml"):
            try:
                (Path("/etc/codex") / name).lstat()
            except FileNotFoundError:
                continue
            except OSError:
                pytest.fail("System configuration absence is unverified", pytrace=False)
            pytest.fail(
                "Packaged usage consumer requires absent system configuration",
                pytrace=False,
            )

    @contextmanager
    def isolated_client() -> Iterator[Codex]:
        with patch.dict(os.environ, environment, clear=True):
            with Codex(
                config=CodexConfig(
                    codex_bin=str(package.cli),
                    cwd=str(workspace),
                    env=environment,
                )
            ) as client:
                yield client

    def request_thread_id(request: CapturedResponsesRequest) -> str | None:
        metadata = request.body_json().get("client_metadata")
        return metadata.get("thread_id") if isinstance(metadata, dict) else None

    def has_call_output(request: CapturedResponsesRequest, call_id: str) -> bool:
        return any(
            item.get("type") == "function_call_output"
            and item.get("call_id") == call_id
            for item in request.input()
        )

    def function_call(
        call_id: str, name: str, arguments: dict[str, object]
    ) -> dict[str, object]:
        return {
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": json.dumps(arguments, sort_keys=True),
                "namespace": "collaboration",
            },
        }

    def completed_with_usage_and_model(
        response_id: str,
        *,
        model: str,
        input_tokens: int,
        cached_input_tokens: int,
        output_tokens: int,
        reasoning_output_tokens: int,
        total_tokens: int,
    ) -> dict[str, object]:
        event = ev_completed_with_usage(
            response_id,
            input_tokens=input_tokens,
            cached_input_tokens=cached_input_tokens,
            output_tokens=output_tokens,
            reasoning_output_tokens=reasoning_output_tokens,
            total_tokens=total_tokens,
        )
        event["response"]["model"] = model
        return event

    config_path.write_text(
        'model = "package-smoke"\n'
        'model_provider = "package_smoke"\n'
        'model_reasoning_effort = "medium"\n'
        'approval_policy = "never"\n'
        'sandbox_mode = "workspace-write"\n'
        "[features]\n"
        "code_mode_only = false\n"
        "code_mode_host = true\n"
        "memories = false\n"
        "apps = false\n"
        "plugins = false\n"
        "[features.multi_agent_v2]\n"
        "enabled = true\n"
        "hide_spawn_agent_metadata = false\n"
        "non_code_mode_only = true\n"
        "[model_providers.package_smoke]\n"
        'name = "package smoke"\n'
        'wire_api = "responses"\n'
        "request_max_retries = 0\n"
        "stream_max_retries = 0\n",
        encoding="utf-8",
    )

    with MockResponsesServer() as server:
        with config_path.open("a", encoding="utf-8") as config_file:
            config_file.write(f'base_url = "{server.url}/v1"\n')
        require_system_configuration_absent()
        with isolated_client() as client:
            root = client.thread_start(
                ephemeral=False,
                approval_mode=ApprovalMode.deny_all,
                sandbox=Sandbox.workspace_write,
            )
            root_id = str(root.id)
            spawn_call_id = "usage-spawn-call"
            wait_call_id = "usage-wait-call"
            spawn_marker = "SYNTHETIC_USAGE_ROOT_MARKER"
            child_marker = "SYNTHETIC_USAGE_CHILD_MARKER"

            server.enqueue_sse_for_request(
                lambda request: (
                    request_thread_id(request) == root_id
                    and spawn_marker in json.dumps(request.input())
                    and not has_call_output(request, spawn_call_id)
                ),
                sse(
                    [
                        ev_response_created("usage-root-spawn"),
                        function_call(
                            spawn_call_id,
                            "spawn_agent",
                            {
                                "task_name": "usage-worker",
                                "message": child_marker,
                                "agent_type": "default",
                                "model": "gpt-6-luna",
                                "reasoning_effort": "medium",
                                "fork_turns": "none",
                            },
                        ),
                        completed_with_usage_and_model(
                            "usage-root-spawn",
                            model="synthetic-root-provider-model",
                            input_tokens=2,
                            cached_input_tokens=0,
                            output_tokens=3,
                            reasoning_output_tokens=0,
                            total_tokens=5,
                        ),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    request_thread_id(request) == root_id
                    and has_call_output(request, spawn_call_id)
                    and not has_call_output(request, wait_call_id)
                ),
                sse(
                    [
                        ev_response_created("usage-root-wait"),
                        function_call(
                            wait_call_id,
                            "wait_agent",
                            {
                                "targets": ["/root/usage-worker"],
                                "return_when": "all",
                                "timeout_ms": 20_000,
                            },
                        ),
                        completed_with_usage_and_model(
                            "usage-root-wait",
                            model="synthetic-root-provider-model",
                            input_tokens=2,
                            cached_input_tokens=0,
                            output_tokens=3,
                            reasoning_output_tokens=0,
                            total_tokens=5,
                        ),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    request_thread_id(request) == root_id
                    and has_call_output(request, wait_call_id)
                ),
                sse(
                    [
                        ev_response_created("usage-root-final"),
                        ev_assistant_message("usage-root-answer", "root complete"),
                        completed_with_usage_and_model(
                            "usage-root-final",
                            model="synthetic-root-provider-model",
                            input_tokens=2,
                            cached_input_tokens=0,
                            output_tokens=3,
                            reasoning_output_tokens=0,
                            total_tokens=5,
                        ),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    request.header("x-codex-parent-thread-id") == root_id
                    and child_marker in json.dumps(request.input())
                ),
                sse(
                    [
                        ev_response_created("usage-child-final"),
                        ev_assistant_message("usage-child-answer", "child complete"),
                        completed_with_usage_and_model(
                            "usage-child-final",
                            model="synthetic-child-provider-model",
                            input_tokens=2,
                            cached_input_tokens=0,
                            output_tokens=3,
                            reasoning_output_tokens=0,
                            total_tokens=5,
                        ),
                    ]
                ),
            )

            turn = root.run(f"{spawn_marker}: start a child and wait for it.")

            selected_root_requests = [
                request
                for request in server.requests()
                if request.path == "/v1/responses"
                and request_thread_id(request) == root_id
                and has_call_output(request, spawn_call_id)
                and not has_call_output(request, wait_call_id)
            ]
            if len(selected_root_requests) != 1:
                pytest.fail(
                    "Synthetic spawn result was not uniquely observed", pytrace=False
                )
            spawn_outputs = [
                item
                for item in selected_root_requests[0].input()
                if isinstance(item, dict)
                and item.get("type") == "function_call_output"
                and item.get("call_id") == spawn_call_id
            ]
            if len(spawn_outputs) != 1:
                pytest.fail("Synthetic spawn result was not unique", pytrace=False)
            output = spawn_outputs[0].get("output")
            if not isinstance(output, str):
                pytest.fail(
                    "Synthetic spawn result was missing or non-text", pytrace=False
                )
            json_parsed = True
            try:
                result = json.loads(output)
            except (TypeError, ValueError):
                json_parsed = False
                result = None
            valid_keys = isinstance(result, dict) and set(result) in (
                {"task_name"},
                {"task_name", "nickname"},
            )
            valid_nickname = isinstance(result, dict) and (
                "nickname" not in result
                or result["nickname"] is None
                or isinstance(result["nickname"], str)
            )
            if not (
                valid_keys
                and valid_nickname
                and result.get("task_name") == "/root/usage-worker"
            ):

                def json_type(value: object) -> str:
                    if value is None:
                        return "null"
                    if type(value) is bool:
                        return "boolean"
                    if type(value) is int:
                        return "integer"
                    if type(value) is float:
                        return "number"
                    if isinstance(value, str):
                        return "string"
                    if isinstance(value, list):
                        return "array"
                    if isinstance(value, dict):
                        return "object"
                    return "other"

                is_object = isinstance(result, dict)
                known_keys = {"task_name", "nickname"}
                diagnostic = {
                    "json_parsed": json_parsed,
                    "top_level_type": json_type(result)
                    if json_parsed
                    else "invalid_json",
                    "task_name_present": is_object and "task_name" in result,
                    "task_name_type": (
                        json_type(result["task_name"])
                        if is_object and "task_name" in result
                        else "absent"
                    ),
                    "nickname_present": is_object and "nickname" in result,
                    "nickname_type": (
                        json_type(result["nickname"])
                        if is_object and "nickname" in result
                        else "absent"
                    ),
                    "known_key_count": (
                        sum(key in result for key in known_keys) if is_object else 0
                    ),
                    "unknown_key_count": (
                        sum(key not in known_keys for key in result) if is_object else 0
                    ),
                    "expected_task_name_matches": (
                        is_object and result.get("task_name") == "/root/usage-worker"
                    ),
                    "nickname_type_valid": valid_nickname,
                    "closed_key_set_valid": valid_keys,
                }
                record_property(
                    "usage_spawn_diagnostic_json",
                    json.dumps(diagnostic, sort_keys=True, separators=(",", ":")),
                )
                allowed_keys = (
                    "task_name",
                    "nickname",
                    "error",
                    "code",
                    "type",
                    "message",
                )
                allowed_error_codes = {
                    "invalid_arguments",
                    "invalid_request_error",
                    "model_not_found",
                    "permission_denied",
                    "rate_limit_exceeded",
                    "context_length_exceeded",
                }
                error = result.get("error") if is_object else None
                code = (
                    error.get("code")
                    if isinstance(error, dict)
                    else result.get("code")
                    if is_object
                    else None
                )
                error_code = (
                    code
                    if isinstance(code, str) and code in allowed_error_codes
                    else "absent"
                    if code is None
                    else "unrecognized"
                )
                error_category = (
                    "unclassified_json" if json_parsed else "unclassified_non_json"
                )
                if not json_parsed:
                    if output.startswith(
                        ("unsupported call: ", "unsupported custom tool call: ")
                    ):
                        error_category = "tool_unavailable"
                    elif (
                        output.startswith("Unknown model `")
                        and " for spawn_agent. Available models: " in output
                    ):
                        error_category = "requested_model_unavailable"
                    elif (
                        output.startswith("Reasoning effort `")
                        and " is not supported for model `" in output
                    ):
                        error_category = "unsupported_reasoning_effort"
                    elif output == "agent type is currently not available":
                        error_category = "requested_role_unavailable"
                # Keep failure diagnostics closed: never emit response values or arbitrary keys.
                classification = {
                    "json_parsed": json_parsed,
                    "top_level_type": diagnostic["top_level_type"],
                    "allowed_keys_present": [
                        key for key in allowed_keys if is_object and key in result
                    ],
                    "allowed_value_types": {
                        key: json_type(result[key])
                        for key in allowed_keys
                        if is_object and key in result
                    },
                    "error_category": error_category,
                    "error_code": error_code,
                }
                print(
                    "usage_spawn_failure_classification="
                    + json.dumps(classification, sort_keys=True, separators=(",", ":"))
                )
                pytest.fail(
                    "Synthetic spawn result did not match the closed V2 result shape",
                    pytrace=False,
                )

            child_requests = [
                request
                for request in server.requests()
                if request.path == "/v1/responses"
                and request.header("x-codex-parent-thread-id") == root_id
                and child_marker in json.dumps(request.input())
                and isinstance(request_thread_id(request), str)
                and bool(request_thread_id(request).strip())
            ]
            if len(child_requests) != 1:
                pytest.fail(
                    "Synthetic child Responses request was not uniquely observed",
                    pytrace=False,
                )
            child_id = request_thread_id(child_requests[0])
            if not isinstance(child_id, str) or not child_id.strip():
                pytest.fail(
                    "Synthetic child identity was not uniquely observed", pytrace=False
                )
            if turn.final_response != "root complete":
                pytest.fail("Packaged root/child smoke did not complete", pytrace=False)

    database_path = codex_home / "usage_1.sqlite"
    if not database_path.is_file():
        pytest.fail("Packaged usage database was not created", pytrace=False)
    try:
        with closing(
            sqlite3.connect(f"file:{database_path.as_posix()}?mode=ro", uri=True)
        ) as database:
            database.row_factory = sqlite3.Row
            edges = database.execute(
                "SELECT spawn_request_id, parent_thread_id, child_thread_id, "
                "requested_model, requested_role, requested_reasoning_effort, "
                "status, created_at, completed_at FROM usage_spawn_requests "
                "WHERE parent_thread_id = ?",
                (root_id,),
            ).fetchall()
            call_rows = database.execute(
                "SELECT thread_id, COUNT(*) AS call_count, "
                "SUM(total_tokens) AS total_tokens FROM usage_provider_calls "
                "WHERE thread_id IN (?, ?) GROUP BY thread_id",
                (root_id, child_id),
            ).fetchall()
    except sqlite3.Error:
        pytest.fail(
            "Synthetic packaged usage database could not be read", pytrace=False
        )
    if len(edges) != 1:
        pytest.fail(
            "Successful spawn edge was not recorded exactly once", pytrace=False
        )
    edge = edges[0]
    if not (
        isinstance(edge["spawn_request_id"], str)
        and edge["spawn_request_id"].startswith("spawn-v1:")
        and len(edge["spawn_request_id"]) <= 128
        and edge["parent_thread_id"] == root_id
        and edge["child_thread_id"] == child_id
        and edge["requested_model"] == "gpt-6-luna"
        and edge["requested_role"] == "default"
        and edge["requested_reasoning_effort"] == "medium"
        and edge["status"] == "spawned"
        and isinstance(edge["created_at"], str)
        and edge["completed_at"] is None
    ):
        pytest.fail(
            "Recorded spawn edge did not match the successful call", pytrace=False
        )
    calls_by_thread = {
        row["thread_id"]: (row["call_count"], row["total_tokens"]) for row in call_rows
    }
    if calls_by_thread != {root_id: (3, 15), child_id: (1, 5)}:
        pytest.fail("Synthetic provider-call totals were not preserved", pytrace=False)

    script = next(
        parent / "scripts/codex_usage_report.py"
        for parent in Path(__file__).resolve().parents
        if (parent / "scripts/codex_usage_report.py").is_file()
    )
    require_system_configuration_absent()
    try:
        completed = subprocess.run(
            [
                sys.executable,
                str(script),
                "--database",
                str(database_path),
                "--all-time",
                "--timezone",
                "UTC",
                "--scope",
                "subtree",
                "--thread-id",
                root_id,
                "--credit-mode",
                "none",
            ],
            cwd=workspace,
            env=environment,
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
        report = json.loads(completed.stdout)
    except (OSError, subprocess.SubprocessError, UnicodeError, ValueError):
        pytest.fail("Bounded packaged usage report failed", pytrace=False)
    if not isinstance(report, dict):
        pytest.fail("Bounded packaged usage report had an invalid shape", pytrace=False)
    contributions = report.get("scope_contributions")
    coverage = report.get("lineage_coverage")
    summary = report.get("summary")
    root_descendant_models = (
        contributions.get("root_vs_descendants_by_actual_model")
        if isinstance(contributions, dict)
        else None
    )
    root_models = (
        root_descendant_models.get("requested_root")
        if isinstance(root_descendant_models, dict)
        else None
    )
    descendant_models = (
        root_descendant_models.get("all_descendants")
        if isinstance(root_descendant_models, dict)
        else None
    )
    own_contribution = (
        contributions.get("own") if isinstance(contributions, dict) else None
    )
    descendant_contribution = (
        contributions.get("descendants_excluding_anchor")
        if isinstance(contributions, dict)
        else None
    )
    token_subtotals = (
        summary.get("token_sums_known_subtotal") if isinstance(summary, dict) else None
    )

    def model_usage(rows: object) -> dict[str, tuple[int, int]] | None:
        if not isinstance(rows, list):
            return None
        observed: dict[str, tuple[int, int]] = {}
        for row in rows:
            if not isinstance(row, dict):
                return None
            model = row.get("actual_model_used")
            call_count = row.get("provider_call_count")
            subtotals = row.get("token_sums_known_subtotal")
            missing = row.get("token_missing_call_count")
            if (
                not isinstance(model, str)
                or not isinstance(call_count, int)
                or not isinstance(subtotals, dict)
                or not isinstance(subtotals.get("total_tokens"), int)
                or not isinstance(missing, dict)
                or missing.get("total_tokens") != 0
                or model in observed
            ):
                return None
            observed[model] = (call_count, subtotals["total_tokens"])
        return observed

    report_checks = (
        completed.returncode == 0,
        report.get("status") == "complete",
        report.get("recorder_completeness")
        == "unknown; report covers recorded ledger rows only",
        report.get("scope") == {"kind": "subtree", "thread_id": root_id},
        isinstance(coverage, dict)
        and coverage.get("participating_thread_count") == 2
        and coverage.get("descendant_thread_count") == 1
        and coverage.get("unresolved_thread_count") == 0,
        isinstance(summary, dict)
        and summary.get("provider_call_count") == 4
        and summary.get("configured_model_unrecorded_call_count") == 4
        and isinstance(token_subtotals, dict)
        and token_subtotals.get("total_tokens") == 20,
        isinstance(contributions, dict)
        and contributions.get("anchor_thread_id") == root_id
        and isinstance(own_contribution, dict)
        and own_contribution.get("provider_call_count") == 3
        and isinstance(descendant_contribution, dict)
        and descendant_contribution.get("provider_call_count") == 1
        and contributions.get("disjoint_call_counts_reconcile") is True,
        model_usage(root_models) == {"synthetic-root-provider-model": (3, 15)},
        model_usage(descendant_models) == {"synthetic-child-provider-model": (1, 5)},
    )
    if not all(report_checks):
        pytest.fail("Packaged root/child usage report did not reconcile", pytrace=False)


@pytest.mark.skipif(
    sys.platform != "linux",
    reason="Package instruction overlay consumer is scoped to the Linux first-package runner",
)
def test_cli_instruction_overlays_in_synthetic_linux_package(
    package: SmokePackage,
    tmp_path: Path,
) -> None:
    """Exercise editable instruction overlays through the exact packaged CLI."""
    home = tmp_path / "home"
    config_home = home / "codex"
    workspace = tmp_path / "workspace"
    temporary = tmp_path / "temporary"
    for directory in (config_home, workspace, temporary):
        directory.mkdir(parents=True)
    (workspace / ".git").mkdir()
    (workspace / ".git" / "HEAD").write_text("ref: refs/heads/main\n", encoding="utf-8")

    environment = {
        key: package.environment[key]
        for key in ("PATH", "SystemRoot", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT")
        if key in package.environment
    }
    if "PATH" not in environment:
        pytest.fail("Packaged CLI PATH is unavailable", pytrace=False)
    environment.update(
        CODEX_HOME=str(config_home),
        HOME=str(home),
        USERPROFILE=str(home),
        XDG_CONFIG_HOME=str(home / "xdg-config"),
        APPDATA=str(home / "app-data"),
        LOCALAPPDATA=str(home / "local-app-data"),
        PROGRAMDATA=str(home / "program-data"),
        TMPDIR=str(temporary),
        TMP=str(temporary),
        TEMP=str(temporary),
        NO_PROXY="127.0.0.1,localhost",
        no_proxy="127.0.0.1,localhost",
    )
    isolated = replace(package, directory=workspace, environment=environment)
    config_path = config_home / "config.toml"

    def common_configuration(server_url: str) -> str:
        return (
            'model = "package-overlay-smoke"\n'
            'model_provider = "package_overlay_smoke"\n'
            'instructions = "PACKAGE_BASE_ORIGINAL"\n'
            'developer_instructions = "PACKAGE_DEVELOPER_ORIGINAL"\n'
            'approval_policy = "never"\n'
            'sandbox_mode = "read-only"\n'
            "suppress_unstable_features_warning = true\n"
            "\n[features]\n"
            "apps = false\n"
            "plugins = false\n"
            "memories = false\n"
            "\n[analytics]\n"
            "enabled = false\n"
            "\n[otel]\n"
            'exporter = "none"\n'
            'trace_exporter = "none"\n'
            'metrics_exporter = "none"\n'
            "\n[model_providers.package_overlay_smoke]\n"
            'name = "package overlay smoke"\n'
            f'base_url = "{server_url}/v1"\n'
            'wire_api = "responses"\n'
            "requires_openai_auth = false\n"
            "request_max_retries = 0\n"
            "stream_max_retries = 0\n"
        )

    overlay_tables = (
        "\n[instruction_overlays.model_baseline]\n"
        'edits = [{ op = "replace_exact", from = "PACKAGE_BASE_ORIGINAL", '
        'to = "PACKAGE_BASE_EDITED", expected_matches = 1 }]\n'
        "\n[instruction_overlays.developer_guidance]\n"
        "edits = [\n"
        '  { op = "replace", text = "old remove" },\n'
        '  { op = "prepend", text = "start " },\n'
        '  { op = "append", text = " end" },\n'
        '  { op = "replace_exact", from = "old", to = "new", expected_matches = 1 },\n'
        '  { op = "remove_exact", text = " remove", expected_matches = 1 },\n'
        "]\n"
    )
    arguments = (
        "--ask-for-approval",
        "never",
        "--sandbox",
        "read-only",
        "--model",
        "package-overlay-smoke",
        "exec",
        "--ephemeral",
        "--ignore-rules",
        "--skip-git-repo-check",
        "--color",
        "never",
        "PACKAGE_SYNTHETIC_PROMPT",
    )

    def require_system_configuration_absent() -> None:
        for name in ("config.toml", "requirements.toml", "managed_config.toml"):
            try:
                (Path("/etc/codex") / name).lstat()
            except FileNotFoundError:
                continue
            except OSError:
                pytest.fail("System configuration absence is unverified", pytrace=False)
            pytest.fail(
                "Package overlay consumer requires absent system configuration",
                pytrace=False,
            )

    def response_requests(
        server: MockResponsesServer,
    ) -> list[CapturedResponsesRequest]:
        return [
            request for request in server.requests() if request.path == "/v1/responses"
        ]

    def request_projection(request: CapturedResponsesRequest) -> list[str]:
        try:
            body = request.body_json()
        except (ValueError, UnicodeError):
            pytest.fail("Mock Responses request body is not valid JSON", pytrace=False)
        if (
            request.method != "POST"
            or request.path != "/v1/responses"
            or not isinstance(body, dict)
            or body.get("model") != "package-overlay-smoke"
        ):
            pytest.fail("Mock Responses request model is invalid", pytrace=False)
        if "instructions" in body:
            pytest.fail("Unexpected top-level instructions field", pytrace=False)
        items = body.get("input")
        if not isinstance(items, list):
            pytest.fail("Mock Responses request input is invalid", pytrace=False)
        developer_texts: list[str] = []
        prompt_seen = False
        for item in items:
            if not isinstance(item, dict):
                pytest.fail("Mock Responses request item is invalid", pytrace=False)
            role = item.get("role")
            content = item.get("content")
            if not isinstance(content, (str, list)):
                continue
            texts: list[str] = []
            if isinstance(content, str):
                texts.append(content)
            else:
                for part in content:
                    if not isinstance(part, dict):
                        continue
                    text = part.get("text")
                    if part.get("type") == "input_text" and isinstance(text, str):
                        texts.append(text)
            if role == "developer":
                developer_texts.extend(texts)
            elif role == "user" and "PACKAGE_SYNTHETIC_PROMPT" in texts:
                prompt_seen = True
        if not prompt_seen:
            pytest.fail(
                "Synthetic package prompt is absent from the request", pytrace=False
            )
        return developer_texts

    def run_success(
        server: MockResponsesServer,
        response_id: str,
        expected_base: str,
        expected_developer: str,
        expected_permission: str | None,
        forbidden_texts: tuple[str, ...] = (),
    ) -> str:
        require_system_configuration_absent()
        before = len(response_requests(server))
        server.enqueue_assistant_message("PACKAGE_DONE", response_id=response_id)
        try:
            isolated.run(*arguments, stdin="")
        except (OSError, subprocess.SubprocessError, UnicodeError):
            pytest.fail("Packaged overlay invocation failed", pytrace=False)
        requests = response_requests(server)
        if len(requests) != before + 1:
            pytest.fail(
                "Successful overlay invocation did not send exactly one request",
                pytrace=False,
            )
        developer_texts = request_projection(requests[-1])
        all_developer_text = "\n".join(developer_texts)
        if developer_texts.count(expected_base) != 1:
            pytest.fail("Expected synthetic baseline text is absent", pytrace=False)
        if developer_texts.count(expected_developer) != 1:
            pytest.fail("Expected synthetic developer text is absent", pytrace=False)
        if any(marker in all_developer_text for marker in forbidden_texts):
            pytest.fail(
                "Original overlay source text remains in the request", pytrace=False
            )
        if (
            expected_permission is not None
            and expected_permission not in all_developer_text
        ):
            pytest.fail("Mandatory permissions guidance is absent", pytrace=False)
        permission_fragments = [
            text for text in developer_texts if "<permissions instructions>" in text
        ]
        if len(permission_fragments) != 1:
            pytest.fail(
                "Mandatory permissions fragment shape is invalid", pytrace=False
            )
        return permission_fragments[0]

    with MockResponsesServer() as server:
        common = common_configuration(server.url)
        config_path.write_text(common, encoding="utf-8")
        baseline_permission = run_success(
            server,
            "PACKAGE_BASELINE",
            "PACKAGE_BASE_ORIGINAL",
            "PACKAGE_DEVELOPER_ORIGINAL",
            "<permissions instructions>",
        )

        config_path.write_text(common + overlay_tables, encoding="utf-8")
        overlay_permission = run_success(
            server,
            "PACKAGE_OVERLAY",
            "PACKAGE_BASE_EDITED",
            "start new end",
            baseline_permission,
            ("PACKAGE_BASE_ORIGINAL", "PACKAGE_DEVELOPER_ORIGINAL"),
        )
        if overlay_permission != baseline_permission:
            pytest.fail("Overlay changed mandatory permissions guidance", pytrace=False)

        mismatch_configuration = overlay_tables.replace(
            'to = "PACKAGE_BASE_EDITED", expected_matches = 1',
            'to = "PACKAGE_BASE_EDITED", expected_matches = 2',
            1,
        )
        config_path.write_text(common + mismatch_configuration, encoding="utf-8")
        require_system_configuration_absent()
        before_mismatch = len(response_requests(server))
        try:
            isolated.run(*arguments, stdin="")
        except subprocess.CalledProcessError as error:
            if error.returncode == 0:
                pytest.fail("Count-mismatch invocation returned success", pytrace=False)
        except (OSError, subprocess.SubprocessError, UnicodeError):
            pytest.fail(
                "Count-mismatch invocation could not be completed", pytrace=False
            )
        else:
            pytest.fail("Count-mismatch overlay unexpectedly succeeded", pytrace=False)
        if len(response_requests(server)) != before_mismatch:
            pytest.fail("Count mismatch sent a Responses request", pytrace=False)

        config_path.write_text(common, encoding="utf-8")
        rollback_permission = run_success(
            server,
            "PACKAGE_ROLLBACK",
            "PACKAGE_BASE_ORIGINAL",
            "PACKAGE_DEVELOPER_ORIGINAL",
            baseline_permission,
        )
        if rollback_permission != baseline_permission:
            pytest.fail(
                "Rollback changed mandatory permissions guidance", pytrace=False
            )


def test_cli_version_matches_release_sources(package: SmokePackage) -> None:
    """Verify the compiled banner against the sources used to package the release."""
    root = next(
        parent
        for parent in Path(__file__).resolve().parents
        if (parent / "codex-rs/Cargo.toml").is_file()
    )
    manifest = tomllib.loads((root / "codex-rs/Cargo.toml").read_text())
    version = manifest["workspace"]["package"]["version"]
    assert package.run("--version").stdout == f"codex-cli {version}\n"


def test_package_manifests_bind_all_extracted_payloads(package: SmokePackage) -> None:
    """Each downloaded package manifest hashes its exact extracted payload."""
    for package_root in (package.cli_root, package.app_server_root):
        manifest = json.loads(
            (package_root / "codex-package.json").read_text(encoding="utf-8")
        )
        payload_hashes: dict[str, str] = {}
        for path in sorted(package_root.rglob("*")):
            relative_path = path.relative_to(package_root).as_posix()
            if not path.is_file() or relative_path == "codex-package.json":
                continue
            digest = hashlib.sha256()
            with path.open("rb") as payload:
                for chunk in iter(lambda: payload.read(1024 * 1024), b""):
                    digest.update(chunk)
            payload_hashes[relative_path] = digest.hexdigest()

        assert manifest.get("payloadSha256") == payload_hashes, package_root


@pytest.mark.parametrize("entrypoint", ["codex", "codex-app-server"])
def test_app_server_runs_code_mode_through_python_sdk(
    package: SmokePackage,
    responses_server: MockResponsesServer,
    entrypoint: str,
) -> None:
    """Both packages run their own ripgrep through sandboxed SDK code mode."""
    windows = "windows" in package.target
    sandbox_config = ("--config", 'windows.sandbox="unelevated"') if windows else ()
    if entrypoint == "codex":
        executable = package.cli
        package_root = package.cli_root
        package_path_dir = package.cli_path_dir
        config = CodexConfig(
            codex_bin=str(executable),
            config_overrides=sandbox_config[1:],
            cwd=str(package.directory),
            env=package.environment,
        )
    else:
        executable = package.app_server
        package_root = package.app_server_root
        package_path_dir = package.app_server_path_dir
        config = CodexConfig(
            launch_args_override=(str(executable), *sandbox_config),
            cwd=str(package.directory),
            env=package.environment,
        )
    if windows:
        # TODO(anp): Assert resolved rg paths once Windows sandbox PATH finds them.
        ripgrep = str(package_path_dir / "rg.exe").replace("'", "''")
        command = (
            f"$rg = '{ripgrep}'; Write-Output $rg; & $rg --version; "
            "if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }"
        )
    else:
        command = "command -v rg && rg --version"
    exec_arguments = {"cmd": command, "login": False, "yield_time_ms": 10_000}
    responses_server.enqueue_sse(
        sse(
            [
                ev_response_created("code-mode"),
                {
                    "type": "response.output_item.done",
                    "item": {
                        "type": "custom_tool_call",
                        "call_id": "package-smoke",
                        "name": "exec",
                        "input": (
                            "text(JSON.stringify(await tools.exec_command("
                            f"{json.dumps(exec_arguments)})))"
                        ),
                    },
                },
                ev_completed("code-mode"),
            ]
        )
    )
    responses_server.enqueue_assistant_message("Done", response_id="code-mode-done")
    with Codex(config=config) as client:
        turn = client.thread_start(
            ephemeral=True,
            approval_mode=ApprovalMode.deny_all,
            sandbox=Sandbox.workspace_write,
        ).run("run package smoke")
        assert turn.final_response == "Done", turn
    # The mock also records analytics POSTs, which have no Responses input list.
    output = next(
        item
        for request in responses_server.requests()
        if request.path == "/v1/responses"
        for item in request.input()
        if item.get("type") == "custom_tool_call_output"
        and item.get("call_id") == "package-smoke"
    )["output"]
    if not isinstance(output, str):
        output = next(
            item["text"] for item in output if item.get("text", "").startswith("{")
        )
    execution = json.loads(output)
    execution_output = execution["output"]
    assert execution["exit_code"] == 0, execution_output
    assert "ripgrep" in execution_output, execution_output

    # Each entrypoint must execute the ripgrep bundled in its own package.
    reported_ripgrep_path = Path(execution_output.splitlines()[0].strip())
    assert reported_ripgrep_path.is_relative_to(package_root), execution_output


def _code_mode_host_symbol_address(
    package: SmokePackage,
    symbols: Path,
    *arguments: str,
) -> str:
    symbol_output = subprocess.run(
        ["nm", *arguments, str(symbols)],
        cwd=package.directory,
        env=package.environment,
        capture_output=True,
        text=True,
        check=True,
        timeout=45,
    ).stdout
    return next(
        parts[0]
        for line in symbol_output.splitlines()
        if len(parts := line.split()) == 3
        and parts[1].lower() == "t"
        and "codex_code_mode_host" in parts[2]
    )


@pytest.mark.skipif(sys.platform != "linux", reason="requires Linux debug symbols")
def test_linux_debug_symbols_resolve_packaged_code(
    package: SmokePackage,
    code_mode_host_debug_symbols: Path,
) -> None:
    """Linux host symbols resolve a packaged function and its source location."""
    address = _code_mode_host_symbol_address(
        package, code_mode_host_debug_symbols, "--defined-only", "--extern-only"
    )
    resolved = subprocess.run(
        [
            "addr2line",
            "-e",
            str(code_mode_host_debug_symbols),
            "-f",
            "-C",
            f"0x{address}",
        ],
        cwd=package.directory,
        env=package.environment,
        capture_output=True,
        text=True,
        check=True,
        timeout=45,
    ).stdout
    assert "codex_code_mode_host" in resolved, resolved
    source = resolved.splitlines()[-1]
    assert source != "??:?" and source.rsplit(":", 1)[-1].isdigit(), source


@pytest.mark.skipif(sys.platform != "darwin", reason="requires macOS debug symbols")
def test_macos_debug_symbols_resolve_packaged_code(
    package: SmokePackage,
    code_mode_host_debug_symbols: Path,
) -> None:
    """macOS host symbols resolve a packaged function."""
    address = _code_mode_host_symbol_address(package, code_mode_host_debug_symbols)
    resolved = subprocess.run(
        ["atos", "-o", str(code_mode_host_debug_symbols), f"0x{address}"],
        cwd=package.directory,
        env=package.environment,
        capture_output=True,
        text=True,
        check=True,
        timeout=45,
    ).stdout
    assert "codex_code_mode_host" in resolved, resolved


@pytest.mark.skipif(sys.platform != "win32", reason="requires Windows debug symbols")
def test_windows_debug_symbols_resolve_packaged_code(
    package: SmokePackage,
    code_mode_host_debug_symbols: Path,
) -> None:
    """Windows host symbols match the packaged executable's debug signature."""
    # Cargo embeds an underscored PDB name; Bazel uses the binary's name.
    # Offer the archived PDB under both names so dumpbin still verifies that
    # its signature matches the packaged executable, regardless of builder.
    cargo_symbols = code_mode_host_debug_symbols.with_name("codex_code_mode_host.pdb")
    shutil.copy2(code_mode_host_debug_symbols, cargo_symbols)
    symbols = (code_mode_host_debug_symbols, cargo_symbols)
    host = code_mode_host_debug_symbols.with_suffix(".exe")
    shutil.copy2(package.cli.with_name("codex-code-mode-host.exe"), host)
    result = subprocess.run(
        ["dumpbin", "/PDBPATH", str(host)],
        cwd=package.directory,
        env=package.environment,
        capture_output=True,
        text=True,
        check=True,
        timeout=45,
    )
    assert any(str(path).casefold() in result.stdout.casefold() for path in symbols), (
        result.stdout
    )
