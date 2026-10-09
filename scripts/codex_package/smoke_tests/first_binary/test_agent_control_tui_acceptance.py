"""Actual packaged /agents rendering and replay for rich agent metadata."""

import json
import re
import uuid
from dataclasses import replace
from pathlib import Path
from typing import Any

from app_server_harness import (
    CapturedResponsesRequest,
    MockResponsesServer,
    ev_assistant_message,
    ev_completed,
    ev_response_created,
    sse,
)
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage
from tui_pty import PackagedTui


def _mock_config(
    home: Path, server: MockResponsesServer, *, agent_tools: bool = False
) -> None:
    home.mkdir(parents=True, exist_ok=True)
    (home / "config.toml").write_text(
        'model = "package-smoke"\nmodel_provider = "package_smoke"\n'
        'approval_policy = "never"\nsandbox_mode = "workspace-write"\n'
        "[features]\n"
        f"code_mode_only = {str(not agent_tools).lower()}\n"
        "code_mode_host = true\n"
        "multi_agent_v2 = true\nmemories = false\napps = false\nplugins = false\n"
        '[model_providers.package_smoke]\nname = "package smoke"\n'
        f'base_url = "{server.url}/v1"\nwire_api = "responses"\n'
        "request_max_retries = 0\nstream_max_retries = 0\n",
        encoding="utf-8",
    )


def _function_call(call_id: str, name: str, args: dict[str, Any]) -> dict[str, Any]:
    return {
        "type": "response.output_item.done",
        "item": {
            "type": "function_call", "namespace": "collaboration",
            "call_id": call_id, "name": name, "arguments": json.dumps(args),
        },
    }


def _thread_id(request: CapturedResponsesRequest) -> str | None:
    metadata = request.body_json().get("client_metadata")
    value = metadata.get("thread_id") if isinstance(metadata, dict) else None
    return value if isinstance(value, str) else None


def _latest_user_marker(request: CapturedResponsesRequest, marker: str) -> bool:
    recent_input = request.input()[-3:]
    return any(marker in json.dumps(item) for item in recent_input)


def _tool_output(server: MockResponsesServer, call_id: str) -> dict[str, Any]:
    outputs = [
        item.get("output")
        for request in server.requests() if request.path == "/v1/responses"
        for item in request.input()
        if item.get("type") == "function_call_output" and item.get("call_id") == call_id
    ]
    assert len(outputs) == 1, f"missing executed output for {call_id}: {outputs!r}"
    payload = outputs[0]
    if isinstance(payload, list):
        payload = next(part["text"] for part in payload if part.get("type") == "output_text")
    assert isinstance(payload, str)
    result = json.loads(payload)
    assert isinstance(result, dict), result
    return result


def _isolated(package: SmokePackage, suffix: str) -> tuple[SmokePackage, Path]:
    home = package.directory / suffix
    home.mkdir()
    return replace(package, environment={**package.environment, "CODEX_HOME": str(home)}), home


def _rich_config(home: Path, server: MockResponsesServer) -> None:
    _mock_config(home, server, agent_tools=True)
    config_path = home / "config.toml"
    config = config_path.read_text(encoding="utf-8")
    config = config.replace("multi_agent_v2 = true\n", "")
    config = config.replace(
        "[model_providers.package_smoke]",
        "[features.multi_agent_v2]\nenabled = true\n"
        "hide_spawn_agent_metadata = false\nnon_code_mode_only = true\n\n"
        "[model_providers.package_smoke]",
    )
    config_path.write_text('model_reasoning_effort = "medium"\n' + config, encoding="utf-8")


def _assert_thread_identity_rendered(frame: str, thread_id: str) -> None:
    """The packaged renderer presents the label and immutable ID on adjacent lines."""
    lines = [line.strip() for line in frame.replace("\r", "").splitlines() if line.strip()]
    label_index = lines.index("Thread ID:")
    assert lines[label_index + 1] == thread_id, lines
    assert re.fullmatch(r"[0-9a-f-]{36}", lines[label_index + 1])


def test_packaged_tui_agents_details_render_configured_identity_and_unknown_effective_identity(
    package: SmokePackage,
) -> None:
    """Render model/effort truthfully and keep private tool payloads out of /agents."""
    isolated, home = _isolated(package, "tui-rich-agent-metadata")
    with MockResponsesServer() as server:
        _rich_config(home, server)
        with Codex(config=CodexConfig(
            codex_bin=str(package.cli), cwd=str(package.directory), env=isolated.environment,
        )) as client:
            root = client.thread_start(
                ephemeral=False, approval_mode=ApprovalMode.deny_all,
                sandbox=Sandbox.workspace_write,
            )
            root_id = root.id
            with PackagedTui(isolated, "resume", root_id) as empty_tui:
                empty_tui.send("/agents")
                empty_tui.send("\r")
                empty_frame = empty_tui.until("Agent command center")
                assert "All 0" in empty_frame, empty_frame
            server.enqueue_sse_for_request(
                lambda request: _thread_id(request) == root_id
                and _latest_user_marker(request, "TUI_RICH_ROOT_MARKER"),
                sse([
                    ev_response_created("tui-rich-spawn"),
                    _function_call(
                        "tui-rich-worker", "spawn_agent",
                        {
                            "task_name": "worker", "message": "PRIVATE_PROMPT_SENTINEL",
                            "model": "package-smoke", "reasoning_effort": "medium",
                        },
                    ),
                    ev_completed("tui-rich-spawn"),
                ]),
            )
            server.enqueue_sse_for_request(
                lambda request: _thread_id(request) == root_id
                and any(
                    item.get("type") == "function_call_output"
                    and item.get("call_id") == "tui-rich-worker"
                    for item in request.input()
                ),
                sse([
                    ev_response_created("tui-rich-root-final"),
                    ev_assistant_message("tui-rich-root-message", "synthetic completion"),
                    ev_completed("tui-rich-root-final"),
                ]),
            )
            server.enqueue_sse_for_request(
                lambda request: _thread_id(request) != root_id
                and request.header("x-codex-parent-thread-id") == root_id
                and _latest_user_marker(request, "PRIVATE_PROMPT_SENTINEL"),
                sse([
                    ev_response_created("tui-rich-child-final"),
                    ev_assistant_message("tui-rich-child-message", "synthetic completion"),
                    ev_completed("tui-rich-child-final"),
                ]),
            )
            assert (
                root.run("TUI_RICH_ROOT_MARKER: start one synthetic worker.").final_response
                == "synthetic completion"
            )
        metadata = _tool_output(server, "tui-rich-worker")

        child_id = metadata["agent_id"]
        uuid.UUID(child_id)
        assert metadata["configured_model"] == "package-smoke", metadata
        assert metadata["configured_reasoning_effort"] == "medium", metadata
        assert "PRIVATE_PROMPT_SENTINEL" not in json.dumps(metadata)

    # Reopen the exact persisted tree in a second packaged process. The view is
    # driven by the binary's /agents implementation, not an in-process renderer.
    with PackagedTui(isolated, "resume", root.id) as tui:
        tui.send("/agents")
        tui.send("\r")
        tui.until("Agent command center")
        tui.send("f")
        tui.until("Search ›")
        tui.send("worker")
        details = tui.until(child_id)
        _assert_thread_identity_rendered(details, child_id)
        assert "Configured/resolved model: package-smoke" in details, details
        assert "Configured/resolved effort: medium" in details, details
        assert "Provider-effective identity: Unknown" in details, details
        parent_id_lines = details.split("Parent thread ID:", maxsplit=1)[1].splitlines()
        assert next(line.strip() for line in parent_id_lines if line.strip()) == root.id, details
        assert "Thread path:" in details and "/root/worker" in details, details
        assert any(
            state in details for state in ("Working", "Needs input", "Ready", "Inactive")
        ), details
        assert "PRIVATE_PROMPT_SENTINEL" not in details, details
        assert "instructions" not in details.lower() and "credentials" not in details.lower()

    with PackagedTui(isolated, "resume", root.id) as tui:
        tui.send("/agents")
        tui.send("\r")
        replay = tui.until("worker")
        # Select the same task after replay so the second frame contains its
        # detail view and the same exact label/ID adjacency.
        tui.send("f")
        tui.until("Search ›")
        tui.send("worker")
        replay_details = tui.until(child_id)
        _assert_thread_identity_rendered(replay_details, child_id)
        assert "Provider-effective identity: Unknown" in replay_details
        assert "PRIVATE_PROMPT_SENTINEL" not in replay_details
