"""Actual packaged root overview, subagent details, and replay for rich metadata."""

import json
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

from artifact import FirstBinaryEvidence
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
    popup = tui.until_screen("open the agent command center")
    popup_text = " ".join(popup.split())
    assert "/agents open the agent command center" in popup_text, popup
    tui.send("\r")
    return tui.until_screen(
        "Agent command center",
        required_markers=("Group:", *required_markers),
    )


def _open_subagents(tui: PackagedTui) -> str:
    tui.until_screen("Ask Codex to do anything")
    tui.send("/subagents")
    popup = tui.until_screen("session's subagents")
    popup_text = " ".join(popup.split())
    assert "/subagents switch between this session's subagents" in popup_text, popup
    tui.send("\r")
    return tui.until_screen(
        "Subagents",
        required_markers=("Select an agent to watch.",),
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
        "non_code_mode_only = true\n\n"
        "[model_providers.package_smoke]",
    )
    config_path.write_text('model_reasoning_effort = "medium"\n' + config, encoding="utf-8")


def _normalized_screen(frame: str) -> str:
    return " ".join(line.strip().strip("│").strip() for line in frame.splitlines())


def _rendered_field_matches(frame: str, label: str, expected: str) -> bool:
    lines = [line.strip().strip("│").strip() for line in frame.splitlines()]
    for index, line in enumerate(lines):
        if not line.startswith(label):
            continue
        value = line[len(label) :].strip()
        if value == expected:
            return True
        if (
            value.endswith("-")
            and index + 1 < len(lines)
            and value + lines[index + 1].strip() == expected
        ):
            return True
    return False


def test_packaged_tui_agents_details_render_configured_identity_and_unknown_effective_identity(
    package: SmokePackage, artifact_evidence: FirstBinaryEvidence,
) -> None:
    """Render model/effort truthfully and keep private prompt data out of agent views."""
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
        # gate its next model request so the live picker can inspect it before
        # terminal completion removes it from the thread manager.
        with PackagedTui(isolated, "resume", root_id, columns=40) as tui:
            tui.until("Ask Codex to do anything")
            root_command = "TUI_RICH_ROOT_MARKER: start one synthetic worker."
            tui.send(root_command)
            composer_screen = tui.until_screen("synthetic worker.")
            normalized_composer = _normalized_screen(composer_screen)
            assert root_command in normalized_composer, composer_screen
            # The narrow startup header prioritizes the upstream version and
            # progressive iteration; the trailing source-hash metadata may be
            # ellipsized at 40 columns.
            visible_version_prefix = f"v{artifact_evidence.version}".split("+", 1)[0]
            assert (
                visible_version_prefix.replace(" ", "")
                in "".join(normalized_composer.split())
            ), composer_screen
            tui.send("\r")
            tui.until("TUI_RICH_ROOT_TERMINAL")
            active_child_route.wait_until_selected(timeout_s=30)
            child_root_wait = _tool_output(server, "tui-rich-child-root-wait")
            assert child_root_wait["reason"] == "target_terminal", child_root_wait
            assert child_root_wait["wake_cause"] == "target_status", child_root_wait
            assert set(child_root_wait["status"]) == {"/root"}, child_root_wait

            spawn_output = _tool_output(server, "tui-rich-worker")
            assert "PRIVATE_PROMPT_SENTINEL" not in json.dumps(spawn_output)
            child_requests = [
                request for request in server.requests()
                if request.path == "/v1/responses"
                and request.header("x-codex-parent-thread-id") == root_id
                and _latest_user_marker(request, "PRIVATE_PROMPT_SENTINEL")
            ]
            child_ids = {_thread_id(request) for request in child_requests}
            assert len(child_ids) == 1 and None not in child_ids, (
                f"expected one exact child request under root {root_id}; "
                f"observed child IDs {child_ids}"
            )
            child_id = next(iter(child_ids))
            uuid.UUID(child_id)
            started = tui.until_screen(
                "Started `/root/worker`",
                required_markers=(
                    "Configured model: gpt-5.6-terra",
                    "Configured reasoning effort: medium",
                ),
            )
            assert "PRIVATE_PROMPT_SENTINEL" not in started, started

            overview = _open_agents(
                tui, required_markers=("All 1", "tui-root-task")
            )
            assert "All 1" in overview, overview
            assert "tui-root-task" in overview, overview
            assert child_id not in overview and "/root/worker" not in overview, overview
            assert "PRIVATE_PROMPT_SENTINEL" not in overview, overview
            tui.send("\x1b")
            tui.until("Ask Codex to do anything")
            picker = _open_subagents(tui)
            assert "PRIVATE_PROMPT_SENTINEL" not in picker, picker
            picker_rows = tui.until_screen(
                "2. • /root/worker",
                required_markers=("1. • Main [default] (current)",),
            )
            assert "PRIVATE_PROMPT_SENTINEL" not in picker_rows, picker_rows
            tui.send("\x1b[B")
            narrow_picker = tui.until_screen("› 2. • /root/worker")
            selected_child_rows = [
                line.strip().strip("│").strip()
                for line in narrow_picker.splitlines()
                if line.strip().strip("│").strip().startswith("›")
                and "/root/worker" in line
            ]
            assert len(selected_child_rows) == 1, narrow_picker
            assert "PRIVATE_PROMPT_SENTINEL" not in narrow_picker, narrow_picker
            tui.send("\x1b")
            tui.until("Ask Codex to do anything")

        # The production picker intentionally hides selected descriptions when
        # the remaining details column is too narrow. Keep the 40-column row-
        # selection witness above, then verify rich child details in a fresh
        # default-width TUI while the child is still held behind its route gate.
        with PackagedTui(isolated, "resume", root_id) as details_tui:
            details_tui.until("Ask Codex to do anything")
            picker = _open_subagents(details_tui)
            assert "PRIVATE_PROMPT_SENTINEL" not in picker, picker
            picker_rows = details_tui.until_screen(
                "2. • /root/worker",
                required_markers=("1. • Main [default] (current)",),
            )
            assert "PRIVATE_PROMPT_SENTINEL" not in picker_rows, picker_rows
            details_tui.send("\x1b[B")
            child_details = details_tui.until_screen(
                f"Thread ID: {child_id}",
                required_markers=("Status: Active",),
            )
            child_details_text = _normalized_screen(child_details)
            assert f"Thread ID: {child_id}" in child_details_text, child_details
            assert "Path: /root/worker" in child_details_text, child_details
            assert f"Parent: {root_id}" in child_details_text, child_details
            assert "Configured model: gpt-5.6-terra" in child_details_text, child_details
            assert (
                "Configured reasoning effort: medium" in child_details_text
            ), child_details
            assert "Status: Active" in child_details_text, child_details
            assert "Provider-effective identity:" not in child_details_text, child_details
            assert "PRIVATE_PROMPT_SENTINEL" not in child_details, child_details
            assert "instructions" not in child_details.lower()
            assert "credentials" not in child_details.lower()

            details_tui.send("\x1b")
            details_tui.until("Ask Codex to do anything")
            replay_picker = _open_subagents(details_tui)
            assert "PRIVATE_PROMPT_SENTINEL" not in replay_picker, replay_picker
            replay_picker_rows = details_tui.until_screen(
                "2. • /root/worker",
                required_markers=("1. • Main [default] (current)",),
            )
            assert "PRIVATE_PROMPT_SENTINEL" not in replay_picker_rows, replay_picker_rows
            details_tui.send("\x1b[B")
            replay_details = details_tui.until_screen(
                f"Thread ID: {child_id}",
                required_markers=("Status: Active",),
            )
            replay_details_text = _normalized_screen(replay_details)
            assert f"Thread ID: {child_id}" in replay_details_text, replay_details
            assert f"Parent: {root_id}" in replay_details_text, replay_details
            assert "Configured model: gpt-5.6-terra" in replay_details_text
            assert "Configured reasoning effort: medium" in replay_details_text
            assert "Status: Active" in replay_details_text, replay_details
            assert "PRIVATE_PROMPT_SENTINEL" not in replay_details, replay_details
            child_gate.set()
            details_tui.send("\x1b")
            details_tui.until("Ask Codex to do anything")
            parent_completion = details_tui.until_screen(
                "Completed `/root/worker`",
                required_markers=("Ask Codex to do anything",),
            )
            assert "Completed `/root/worker`" in parent_completion, parent_completion
            # Keep the parent loaded in this app-server: parent-owned V2 child
            # threads are selected through the parent's Subagents picker, not
            # resumed by child ID in a fresh app-server.
            completed_picker = _open_subagents(details_tui)
            assert "PRIVATE_PROMPT_SENTINEL" not in completed_picker, completed_picker
            completed_picker_rows = details_tui.until_screen(
                "2. • /root/worker",
                required_markers=("1. • Main [default] (current)",),
            )
            assert "PRIVATE_PROMPT_SENTINEL" not in completed_picker_rows
            details_tui.send("\x1b[B")
            completed_child_row = details_tui.until_screen(
                f"Thread ID: {child_id}",
            )
            assert f"Thread ID: {child_id}" in _normalized_screen(completed_child_row)
            assert "Path: /root/worker" in _normalized_screen(completed_child_row)
            assert "PRIVATE_PROMPT_SENTINEL" not in completed_child_row
            details_tui.send("\r")
            child_final = details_tui.until_screen("TUI_RICH_CHILD_TERMINAL")
            assert "TUI_RICH_CHILD_TERMINAL" in child_final, child_final

        # At width 40 the command center intentionally renders only its task
        # list. Reopen at the default width to verify the root details pane.
        with PackagedTui(isolated, "resume", root_id) as root_details_tui:
            root_details_tui.until("Ask Codex to do anything")
            resumed_root_screen = root_details_tui.screen.text()
            if (
                "Agent command center" in resumed_root_screen
                and "Group: Project" in resumed_root_screen
            ):
                # A resumed session can already be on its persisted overview.
                # Leave it before invoking /agents so this assertion observes
                # the command transition rather than an unchanged screen.
                root_details_tui.send("\x1b")
                root_details_tui.until_screen("Ask Codex to do anything")
            root_overview = _open_agents(
                root_details_tui,
                required_markers=(
                    "All 2",
                    "tui-root-task",
                ),
            )
            root_overview = root_details_tui.until_screen(
                root_id,
                value_label="Thread ID:",
                required_markers=(
                    "Configured/resolved model:",
                    "Configured/resolved effort: medium",
                    "Provider-effective identity: Unknown",
                ),
            )
            root_details_text = _normalized_screen(root_overview)
            assert f"Thread ID: {root_id}" in root_details_text, root_overview
            assert _rendered_field_matches(
                root_overview, "Configured/resolved model:", "package-smoke"
            ), root_overview
            assert "Configured/resolved effort: medium" in root_details_text
            assert "Provider-effective identity: Unknown" in root_details_text
            assert child_id not in root_overview and "/root/worker" not in root_overview
            assert "PRIVATE_PROMPT_SENTINEL" not in root_overview, root_overview

        with PackagedTui(isolated, "resume", root_id, columns=40) as replay_tui:
            replay_started = replay_tui.until_screen(
                "Started `/root/worker`",
                required_markers=(
                    "Configured model: gpt-5.6-terra",
                    "Configured reasoning effort: medium",
                ),
            )
            assert "PRIVATE_PROMPT_SENTINEL" not in replay_started, replay_started
