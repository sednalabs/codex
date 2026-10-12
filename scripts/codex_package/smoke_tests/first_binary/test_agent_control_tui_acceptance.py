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
from tui_pty import PackagedTui, shared_agent_server

_SPAWN_MODEL = "gpt-6-luna"


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
        "request_max_retries = 0\nstream_max_retries = 0\n"
        '[tui]\nstatus_line = ["thread-id"]\n',
        encoding="utf-8",
    )


def _function_call(call_id: str, name: str, args: dict[str, Any]) -> dict[str, Any]:
    return {
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "namespace": "collaboration",
            "call_id": call_id,
            "name": name,
            "arguments": json.dumps(args),
        },
    }


def _thread_id(request: CapturedResponsesRequest) -> str | None:
    metadata = request.body_json().get("client_metadata")
    value = metadata.get("thread_id") if isinstance(metadata, dict) else None
    return value if isinstance(value, str) else None


def _latest_user_marker(request: CapturedResponsesRequest, marker: str) -> bool:
    recent_input = request.input()[-3:]
    return any(marker in json.dumps(item) for item in recent_input)


def _tool_output(
    server: MockResponsesServer,
    call_id: str,
    *,
    thread_id: str,
    phase_call_id: str,
) -> dict[str, Any]:
    requests = [
        request
        for request in server.requests()
        if request.path == "/v1/responses" and _thread_id(request) == thread_id
    ]
    for request in requests:
        request_outputs = [
            item.get("output")
            for item in request.input()
            if item.get("type") == "function_call_output"
            and item.get("call_id") == call_id
        ]
        assert len(request_outputs) <= 1, (
            f"duplicate output items for {call_id}: {request_outputs!r}"
        )
    occurrences = [
        item.get("output")
        for request in requests
        for item in request.input()
        if item.get("type") == "function_call_output" and item.get("call_id") == call_id
    ]
    phase_requests = [
        request
        for request in requests
        if any(
            item.get("type") == "function_call_output"
            and item.get("call_id") == phase_call_id
            for item in request.input()
        )
    ]
    assert occurrences, f"missing executed output for {call_id} in {thread_id}"
    assert len(phase_requests) == 1, (
        f"missing or ambiguous {phase_call_id} phase for {thread_id}"
    )
    phase_outputs = [
        item.get("output")
        for item in phase_requests[0].input()
        if item.get("type") == "function_call_output" and item.get("call_id") == call_id
    ]
    assert len(phase_outputs) == 1, (
        f"missing or ambiguous {call_id} in {phase_call_id} phase: {phase_outputs!r}"
    )
    assert all(output == occurrences[0] for output in occurrences), (
        f"conflicting repeated history for {call_id}: {occurrences!r}"
    )
    payload = phase_outputs[0]
    if isinstance(payload, list):
        payload = next(
            part["text"] for part in payload if part.get("type") == "output_text"
        )
    assert isinstance(payload, str)
    result = json.loads(payload)
    assert isinstance(result, dict), result
    return result


def _subagent_activity(thread: Any, call_id: str) -> Any:
    matches = [
        item.root
        for turn in thread.turns
        for item in turn.items
        if getattr(item.root, "type", None) == "subAgentActivity"
        and item.root.id == call_id
    ]
    assert len(matches) == 1, (
        f"expected one SDK sub-agent activity for {call_id}: {matches!r}"
    )
    activity = matches[0]
    assert activity.kind.value == "started", activity
    assert activity.id == call_id
    assert isinstance(activity.agent_thread_id, str)
    uuid.UUID(activity.agent_thread_id)
    assert isinstance(activity.agent_path, str) and activity.agent_path.startswith(
        "/root/"
    )
    return activity


def _selected_subagent_details(
    tui: PackagedTui,
    expected: dict[str, tuple[str, str]],
    initial_frame: str,
) -> dict[str, str]:
    """Wait for rows, then bind the latest selected detail to its identity."""
    # UUIDs appear in each populated row; path/parent/status are selected-only
    # fields and are validated inside that row's detail block below.
    population = tui.until_all(tuple(expected), initial=initial_frame)
    assert all(thread_id in population for thread_id in expected), population

    selected: dict[str, str] = {}
    # The real picker includes the primary row first. Reset to that row, then
    # require each descendant's selected-only identity fields after moving.
    # Any redraw alone is not evidence that the preceding Down was applied.
    tui.send("\x1b[H")
    for index, (expected_id, (expected_path, expected_parent)) in enumerate(
        expected.items()
    ):
        tui.send("\x1b[B")
        tui.until_output()
        tui.resize(111 + index)
        frame = tui.until_all(
            (
                expected_id,
                f"Path: {expected_path}",
                f"Parent: {expected_parent}",
                "Status: ",
            )
        )
        # PTY reads are incremental terminal deltas, not reconstructed frames.
        # A resize can therefore leave the previous selection's detail in this
        # stream before the latest redraw. Bind to the final selected-only
        # Path/Parent block and the row identity immediately preceding it.
        latest_details = [
            (frame.rfind(f"Path: {path}"), thread_id, path, parent_id)
            for thread_id, (path, parent_id) in expected.items()
            if f"Path: {path}" in frame
        ]
        assert latest_details, frame
        latest_path_start, thread_id, path, parent_id = max(latest_details)
        assert thread_id == expected_id, frame
        path_marker = f"Path: {path}"
        parent_marker = f"Parent: {parent_id}"
        path_start = latest_path_start
        preceding_ids = [
            (position, candidate_id)
            for candidate_id in expected
            if (position := frame.rfind(candidate_id, 0, path_start)) >= 0
        ]
        assert preceding_ids and max(preceding_ids)[1] == expected_id, frame
        parent_start = frame.find(parent_marker, path_start)
        assert parent_start > path_start, frame
        status_start = frame.find("Status: ", parent_start)
        detail_start = frame.rfind(thread_id, 0, path_start)
        assert detail_start >= 0, frame
        assert status_start > parent_start, frame
        status_match = re.match(
            r"Status:\s+(running|not running|not loaded)(?=\s|$)",
            frame[status_start:],
        )
        status_marker = status_match.group(0) if status_match is not None else None
        assert detail_start < path_start < parent_start < status_start, frame
        assert status_marker is not None, frame
        detail_end = status_start + len(status_marker)
        # Configured model/effort and provider identity precede Path in the same
        # rendered row description. Include the row's UUID prefix so the checks
        # below inspect that metadata without loosening any expected values.
        detail = frame[detail_start:detail_end]
        assert all(
            other_id not in detail
            for other_id in set(expected) - {thread_id, parent_id}
        ), detail
        assert path_marker in detail and parent_marker in detail, detail
        assert detail.count(parent_id) == 1, detail
        assert detail.index(parent_id) == detail.index(parent_marker) + len(
            "Parent: "
        ), detail
        assert f"configured model: {_SPAWN_MODEL}" in detail, detail
        assert "configured effort: medium" in detail, detail
        assert "provider-effective identity: unverified" in detail, detail
        assert "PRIVATE_PROMPT_SENTINEL" not in frame, frame
        assert "PRIVATE_NESTED_PROMPT_SENTINEL" not in frame, frame
        assert "instructions" not in frame.lower(), frame
        assert "credentials" not in frame.lower(), frame
        selected[thread_id] = detail

    assert set(selected) == set(expected), selected
    return selected


def _isolated(package: SmokePackage, suffix: str) -> tuple[SmokePackage, Path]:
    home = package.directory / suffix
    home.mkdir()
    return replace(
        package, environment={**package.environment, "CODEX_HOME": str(home)}
    ), home


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
    config_path.write_text(
        'model_reasoning_effort = "medium"\n' + config, encoding="utf-8"
    )


def test_packaged_tui_subagents_picker_renders_spawned_descendant_identity_and_replay(
    package: SmokePackage,
) -> None:
    """Render real spawned descendants with configured identity and privacy truth."""
    isolated, home = _isolated(package, "tui-rich-agent-metadata")
    with MockResponsesServer() as server:
        _rich_config(home, server)
        with Codex(
            config=CodexConfig(
                codex_bin=str(package.cli),
                cwd=str(package.directory),
                env=isolated.environment,
            )
        ) as client:
            root = client.thread_start(
                ephemeral=False,
                approval_mode=ApprovalMode.deny_all,
                sandbox=Sandbox.workspace_write,
            )
            root_id = root.id
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and request.message_input_texts("user")[-1:]
                    == ["TUI_RICH_SEED_MARKER"]
                ),
                sse(
                    [
                        ev_response_created("tui-rich-seed"),
                        ev_assistant_message("tui-rich-seed-message", "root seeded"),
                        ev_completed("tui-rich-seed"),
                    ]
                ),
            )
            # An empty thread has no rollout to cold-resume after SDK shutdown.
            assert root.run("TUI_RICH_SEED_MARKER").final_response == "root seeded"
        server.enqueue_sse_for_request(
            lambda request: (
                _thread_id(request) == root_id
                and _latest_user_marker(request, "TUI_RICH_ROOT_MARKER")
                and not any(
                    item.get("type") == "function_call_output"
                    and item.get("call_id") == "tui-rich-worker"
                    for item in request.input()
                )
            ),
            sse(
                [
                    ev_response_created("tui-rich-spawn"),
                    _function_call(
                        "tui-rich-worker",
                        "spawn_agent",
                        {
                            "task_name": "worker",
                            "message": "PRIVATE_PROMPT_SENTINEL",
                            "model": _SPAWN_MODEL,
                            "reasoning_effort": "medium",
                        },
                    ),
                    ev_completed("tui-rich-spawn"),
                ]
            ),
        )
        server.enqueue_sse_for_request(
            lambda request: (
                _thread_id(request) == root_id
                and any(
                    item.get("type") == "function_call_output"
                    and item.get("call_id") == "tui-rich-worker"
                    for item in request.input()
                )
                and not any(
                    item.get("type") == "function_call_output"
                    and item.get("call_id") == "tui-rich-root-wait"
                    for item in request.input()
                )
            ),
            sse(
                [
                    ev_response_created("tui-rich-root-wait"),
                    _function_call(
                        "tui-rich-root-wait",
                        "wait_agent",
                        {"targets": ["/root/worker"]},
                    ),
                    ev_completed("tui-rich-root-wait"),
                ]
            ),
        )
        server.enqueue_sse_for_request(
            lambda request: (
                _thread_id(request) == root_id
                and any(
                    item.get("type") == "function_call_output"
                    and item.get("call_id") == "tui-rich-root-wait"
                    for item in request.input()
                )
            ),
            sse(
                [
                    ev_response_created("tui-rich-root-final"),
                    ev_assistant_message(
                        "tui-rich-root-message", "synthetic completion"
                    ),
                    ev_completed("tui-rich-root-final"),
                ]
            ),
        )
        server.enqueue_sse_for_request(
            lambda request: (
                _thread_id(request) != root_id
                and request.header("x-codex-parent-thread-id") == root_id
                and _latest_user_marker(request, "PRIVATE_PROMPT_SENTINEL")
                and not any(
                    item.get("type") == "function_call_output"
                    and item.get("call_id") == "tui-rich-nested"
                    for item in request.input()
                )
            ),
            sse(
                [
                    ev_response_created("tui-rich-nested-spawn"),
                    _function_call(
                        "tui-rich-nested",
                        "spawn_agent",
                        {
                            "task_name": "nested",
                            "message": "PRIVATE_NESTED_PROMPT_SENTINEL",
                            "model": _SPAWN_MODEL,
                            "reasoning_effort": "medium",
                        },
                    ),
                    ev_completed("tui-rich-nested-spawn"),
                ]
            ),
        )
        server.enqueue_sse_for_request(
            lambda request: (
                _thread_id(request) != root_id
                and request.header("x-codex-parent-thread-id") == root_id
                and any(
                    item.get("type") == "function_call_output"
                    and item.get("call_id") == "tui-rich-nested"
                    for item in request.input()
                )
                and not any(
                    item.get("type") == "function_call_output"
                    and item.get("call_id") == "tui-rich-worker-wait"
                    for item in request.input()
                )
            ),
            sse(
                [
                    ev_response_created("tui-rich-worker-wait"),
                    _function_call(
                        "tui-rich-worker-wait",
                        "wait_agent",
                        {"targets": ["/root/worker/nested"]},
                    ),
                    ev_completed("tui-rich-worker-wait"),
                ]
            ),
        )
        server.enqueue_sse_for_request(
            lambda request: (
                _thread_id(request) != root_id
                and request.header("x-codex-parent-thread-id") == root_id
                and any(
                    item.get("type") == "function_call_output"
                    and item.get("call_id") == "tui-rich-worker-wait"
                    for item in request.input()
                )
            ),
            sse(
                [
                    ev_response_created("tui-rich-worker-final"),
                    ev_assistant_message(
                        "tui-rich-worker-message", "synthetic worker completion"
                    ),
                    ev_completed("tui-rich-worker-final"),
                ]
            ),
        )
        server.enqueue_sse_for_request(
            lambda request: (
                _thread_id(request) != root_id
                and request.header("x-codex-parent-thread-id") not in (None, root_id)
                and _latest_user_marker(request, "PRIVATE_NESTED_PROMPT_SENTINEL")
            ),
            sse(
                [
                    ev_response_created("tui-rich-nested-final"),
                    ev_assistant_message(
                        "tui-rich-nested-message", "synthetic nested completion"
                    ),
                    ev_completed("tui-rich-nested-final"),
                ]
            ),
        )
        with Codex(
            config=CodexConfig(
                codex_bin=str(package.cli),
                cwd=str(package.directory),
                env=isolated.environment,
            )
        ) as client:
            root = client.thread_resume(root_id)
            assert (
                root.run(
                    "TUI_RICH_ROOT_MARKER: start one synthetic worker."
                ).final_response
                == "synthetic completion"
            )
            server.wait_for_requests(8, timeout_s=30)
            root_history = root.read(include_turns=True).thread
            worker_activity = _subagent_activity(root_history, "tui-rich-worker")
            child_id = worker_activity.agent_thread_id
            assert worker_activity.agent_path == "/root/worker", worker_activity
            assert worker_activity.model is not None, worker_activity
            assert worker_activity.model == _SPAWN_MODEL, worker_activity
            assert worker_activity.reasoning_effort is not None, worker_activity
            assert worker_activity.reasoning_effort.value == "medium", worker_activity

            worker = client.thread_resume(child_id)
            worker_history = worker.read(include_turns=True).thread
            nested_activity = _subagent_activity(worker_history, "tui-rich-nested")
            nested_id = nested_activity.agent_thread_id
            assert nested_activity.agent_path == "/root/worker/nested", nested_activity
            assert nested_activity.model is not None, nested_activity
            assert nested_activity.model == _SPAWN_MODEL, nested_activity
            assert nested_activity.reasoning_effort is not None, nested_activity
            assert nested_activity.reasoning_effort.value == "medium", nested_activity

        worker_tool_body = _tool_output(
            server,
            "tui-rich-worker",
            thread_id=root_id,
            phase_call_id="tui-rich-root-wait",
        )
        assert set(worker_tool_body) == {"task_name", "nickname"}, worker_tool_body
        assert worker_tool_body["task_name"] == worker_activity.agent_path
        assert worker_tool_body["nickname"] is None or isinstance(
            worker_tool_body["nickname"], str
        ), worker_tool_body
        assert "PRIVATE_PROMPT_SENTINEL" not in json.dumps(worker_tool_body)

        nested_tool_body = _tool_output(
            server,
            "tui-rich-nested",
            thread_id=child_id,
            phase_call_id="tui-rich-worker-wait",
        )
        assert set(nested_tool_body) == {"task_name", "nickname"}, nested_tool_body
        assert nested_tool_body["task_name"] == nested_activity.agent_path
        assert nested_tool_body["nickname"] is None or isinstance(
            nested_tool_body["nickname"], str
        ), nested_tool_body
        assert "PRIVATE_NESTED_PROMPT_SENTINEL" not in json.dumps(nested_tool_body)

    expected_descendants = {
        child_id: ("/root/worker", root_id),
        nested_id: ("/root/worker/nested", child_id),
    }

    # Reopen the exact persisted tree in a second packaged process. The native
    # picker is driven by actual spawned-agent activity, not forked thread rows.
    with shared_agent_server(isolated):
        with PackagedTui(isolated, "resume", root.id) as tui:
            tui.until_all(
                (
                    "/review - review any changes and find issues",
                    root_id,
                    "Ask Codex",
                )
            )
            tui.submit_line("/subagents", pasted_marker="/subagents")
            picker_frame = tui.until("Subagents")
            selected = _selected_subagent_details(
                tui, expected_descendants, picker_frame
            )
            assert set(selected) == {child_id, nested_id}

    with shared_agent_server(isolated):
        with PackagedTui(isolated, "resume", root.id) as tui:
            tui.until_all(
                (
                    "/review - review any changes and find issues",
                    root_id,
                    "Ask Codex",
                )
            )
            tui.submit_line("/subagents", pasted_marker="/subagents")
            picker_frame = tui.until("Subagents")
            selected = _selected_subagent_details(
                tui, expected_descendants, picker_frame
            )
            assert set(selected) == {child_id, nested_id}
