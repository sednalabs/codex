"""Actual packaged /agents rendering and replay for rich agent metadata."""

import json
import re
from threading import Event
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
from package_acceptance import _mock_config
from tui_pty import PackagedTui


def _open_agents(
    tui: PackagedTui, *, required_markers: tuple[str, ...] = ()
) -> str:
    # Establish readiness, then wait for the actual `/agents` popup entry
    # before Enter so paste-burst handling cannot turn the command into text.
    tui.until_screen("Ask Codex to do anything")
    tui.send("/agents")
    popup = tui.until("open the agent command center")
    assert "/agents" in popup, popup
    tui.send("\r")
    return tui.until_screen(
        "Agent command center",
        required_markers=("Group:", *required_markers),
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


def _has_call_output(request: CapturedResponsesRequest, call_id: str) -> bool:
    return any(
        item.get("type") == "function_call_output" and item.get("call_id") == call_id
        for item in request.input()
    )


def _tool_output(server: MockResponsesServer, call_id: str) -> dict[str, Any]:
    outputs_by_request = [
        [
            item.get("output")
            for item in request.input()
            if item.get("type") == "function_call_output" and item.get("call_id") == call_id
        ]
        for request in server.requests() if request.path == "/v1/responses"
    ]
    duplicate_requests = [
        (request_index, len(outputs))
        for request_index, outputs in enumerate(outputs_by_request)
        if len(outputs) > 1
    ]
    assert not duplicate_requests, (
        f"expected at most one output for {call_id} within each request; "
        f"duplicate request occurrences: {duplicate_requests!r}"
    )
    outputs = [outputs[0] for outputs in outputs_by_request if outputs]
    unique_outputs = {json.dumps(output, sort_keys=True): output for output in outputs}
    assert len(unique_outputs) == 1, (
        f"expected one distinct executed output for {call_id}; "
        f"captured {len(outputs)} request-history occurrences and "
        f"{len(unique_outputs)} distinct payloads"
    )
    payload = next(iter(unique_outputs.values()))
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
    lines = [line.strip().strip("│").strip() for line in frame.replace("\r", "").splitlines()]
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
            server.enqueue_sse_for_request(
                lambda request: _thread_id(request) == root_id
                and _latest_user_marker(request, "TUI_RICH_SEED_MARKER"),
                sse([
                    ev_response_created("tui-rich-seed"),
                    ev_assistant_message("tui-rich-seed-message", "persisted root turn"),
                    ev_completed("tui-rich-seed"),
                ]),
            )
            assert (
                root.run("TUI_RICH_SEED_MARKER: persist the root before TUI resume.").final_response
                == "persisted root turn"
            )
            root.set_name("tui-root-task")

        server.enqueue_sse_for_request(
            lambda request: _thread_id(request) == root_id
            and _latest_user_marker(request, "TUI_RICH_ROOT_MARKER"),
            sse([
                ev_response_created("tui-rich-spawn"),
                _function_call(
                    "tui-rich-worker", "spawn_agent",
                    {
                        "task_name": "worker", "message": "PRIVATE_PROMPT_SENTINEL",
                        "model": "gpt-5.6-terra", "reasoning_effort": "medium",
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
                ev_assistant_message("tui-rich-root-message", "TUI_RICH_ROOT_TERMINAL"),
                ev_completed("tui-rich-root-final"),
            ]),
        )
        server.enqueue_sse_for_request(
            lambda request: _thread_id(request) != root_id
            and request.header("x-codex-parent-thread-id") == root_id
            and _latest_user_marker(request, "PRIVATE_PROMPT_SENTINEL"),
            sse([
                ev_response_created("tui-rich-child-started"),
                _function_call(
                    "tui-rich-child-root-wait", "wait_agent",
                    {
                        "targets": ["/root"],
                        "return_when": "all",
                        "timeout_ms": 25000,
                    },
                ),
                ev_completed("tui-rich-child-started"),
            ]),
        )
        child_gate = Event()
        active_child_route = server.enqueue_sse_for_request(
            lambda request: _thread_id(request) != root_id
            and request.header("x-codex-parent-thread-id") == root_id
            and _has_call_output(request, "tui-rich-child-root-wait"),
            sse([
                ev_response_created("tui-rich-child-final"),
                ev_assistant_message("tui-rich-child-message", "TUI_RICH_CHILD_TERMINAL"),
                ev_completed("tui-rich-child-final"),
            ]),
            gate=child_gate,
        )

        # Exercise spawn inside the same packaged process that renders /agents.
        # Advance the child beyond pending_init with its real wait tool, then
        # gate its next model request so the live overview must render it before
        # terminal completion removes it from the thread manager.
        with PackagedTui(isolated, "resume", root_id) as tui:
            tui.until("Ask Codex to do anything")
            tui.send("TUI_RICH_ROOT_MARKER: start one synthetic worker.")
            tui.until("TUI_RICH_ROOT_MARKER: start one synthetic worker.")
            tui.send("\r")
            tui.until("TUI_RICH_ROOT_TERMINAL")
            active_child_route.wait_until_selected(timeout_s=30)
            child_root_wait = _tool_output(server, "tui-rich-child-root-wait")
            assert child_root_wait["reason"] == "target_terminal", child_root_wait
            assert child_root_wait["wake_cause"] == "target_status", child_root_wait
            assert set(child_root_wait["status"]) == {"/root"}, child_root_wait

            metadata = _tool_output(server, "tui-rich-worker")
            child_id = metadata["agent_id"]
            uuid.UUID(child_id)
            assert metadata["configured_model"] == "gpt-5.6-terra", metadata
            assert metadata["configured_reasoning_effort"] == "medium", metadata
            assert "PRIVATE_PROMPT_SENTINEL" not in json.dumps(metadata)

            overview = _open_agents(
                tui, required_markers=("All 2", "tui-root-task", "worker", "Working")
            )
            assert "All 2" in overview, overview
            assert "tui-root-task" in overview and "worker" in overview, overview
            tui.send("f")
            tui.until_screen("Search ›", required_markers=("Agent command center",))
            tui.send("worker")
            details = tui.until_screen(
                child_id,
                value_label="Thread ID:",
                required_markers=(
                    "Configured/resolved model: gpt-5.6-terra",
                    "Configured/resolved effort: medium",
                    "Provider-effective identity: Unknown",
                    root_id,
                    "Thread path:",
                    "/root/worker",
                ),
            )
            _assert_thread_identity_rendered(details, child_id)
            assert "Configured/resolved model: gpt-5.6-terra" in details, details
            assert "Configured/resolved effort: medium" in details, details
            assert "Provider-effective identity: Unknown" in details, details
            parent_id_lines = details.split("Parent thread ID:", maxsplit=1)[1].splitlines()
            assert next(
                line.strip().strip("│").strip()
                for line in parent_id_lines if line.strip()
            ) == root_id, details
            assert "Thread path:" in details and "/root/worker" in details, details
            assert any(
                state in details for state in ("Working", "Needs input", "Ready", "Inactive")
            ), details
            assert "PRIVATE_PROMPT_SENTINEL" not in details, details
            assert "instructions" not in details.lower() and "credentials" not in details.lower()

            tui.send("\x1b")
            tui.until("Ask Codex to do anything")
            replay = _open_agents(tui, required_markers=("worker",))
            assert "worker" in replay, replay
            # Reopen the same live overview after closing the detail pane and
            # require the identity/private-data contract again.
            tui.send("f")
            tui.until_screen("Search ›", required_markers=("Agent command center",))
            tui.send("worker")
            replay_details = tui.until_screen(
                child_id,
                value_label="Thread ID:",
                required_markers=("Provider-effective identity: Unknown",),
            )
            _assert_thread_identity_rendered(replay_details, child_id)
            assert "Provider-effective identity: Unknown" in replay_details
            assert "PRIVATE_PROMPT_SENTINEL" not in replay_details
            child_gate.set()
            child_final = tui.until_screen(
                "TUI_RICH_CHILD_TERMINAL",
                required_markers=("Provider-effective identity: Unknown",),
            )
            assert "TUI_RICH_CHILD_TERMINAL" in child_final, child_final
