"""Artifact-bound model-facing controls for packaged multi-agent execution.

These cases exercise the actual Responses tool surface. They do not attest to
the provider-effective model: the local server is a synthetic transport.
"""

import json
import uuid
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


def _function_call(
    call_id: str,
    name: str,
    args: dict[str, Any],
    *,
    namespace: str | None = "collaboration",
) -> dict[str, Any]:
    item = {
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": json.dumps(args),
        },
    }
    if namespace is not None:
        item["item"]["namespace"] = namespace
    return item


def _tool_output(server: MockResponsesServer, call_id: str) -> dict[str, Any]:
    outputs = [
        item.get("output")
        for request in server.requests()
        if request.path == "/v1/responses"
        for item in request.input()
        if item.get("type") == "function_call_output" and item.get("call_id") == call_id
    ]
    assert len(outputs) == 1, f"missing executed output for {call_id}: {outputs!r}"
    payload = outputs[0]
    if isinstance(payload, list):
        payload = next(
            part["text"] for part in payload if part.get("type") == "output_text"
        )
    assert isinstance(payload, str)
    result = json.loads(payload)
    assert isinstance(result, dict), result
    return result


def _collab_item(thread_read: Any, call_id: str) -> Any:
    items = [
        item.root
        for turn in thread_read.thread.turns
        for item in turn.items
        if getattr(item.root, "id", None) == call_id
    ]
    assert len(items) == 1, (
        f"missing unique collaboration item for {call_id}: {items!r}"
    )
    return items[0]


def _tool_names(tools: list[dict[str, Any]]) -> set[str]:
    names: set[str] = set()
    for tool in tools:
        name = tool.get("name")
        if isinstance(name, str):
            names.add(name)
        children = tool.get("tools")
        if isinstance(children, list):
            names.update(_tool_names(children))
    return names


def _thread_id(request: CapturedResponsesRequest) -> str | None:
    metadata = request.body_json().get("client_metadata")
    return metadata.get("thread_id") if isinstance(metadata, dict) else None


def _is_goal_turn(request: CapturedResponsesRequest) -> bool:
    metadata = request.body_json().get("client_metadata")
    turn_metadata = (
        metadata.get("x-codex-turn-metadata") if isinstance(metadata, dict) else None
    )
    return (
        isinstance(turn_metadata, dict) and turn_metadata.get("turn_trigger") == "goal"
    )


def _contains_input_text(request: CapturedResponsesRequest, text: str) -> bool:
    return text in json.dumps(request.input())


def _has_user_marker(request: CapturedResponsesRequest, marker: str) -> bool:
    recent_input = request.input()[-3:]
    return any(marker in json.dumps(item) for item in recent_input)


def _has_call_output(request: CapturedResponsesRequest, call_id: str) -> bool:
    outputs = [
        item.get("call_id")
        for item in request.input()
        if item.get("type") == "function_call_output"
    ]
    return bool(outputs) and outputs[-1] == call_id


def _has_all_call_outputs(
    request: CapturedResponsesRequest, call_ids: set[str]
) -> bool:
    outputs = {
        item.get("call_id")
        for item in request.input()
        if item.get("type") == "function_call_output"
    }
    return call_ids <= outputs


def _call_output_count(server: MockResponsesServer, call_id: str) -> int:
    return sum(
        1
        for request in server.requests()
        if request.path == "/v1/responses"
        for item in request.input()
        if item.get("type") == "function_call_output" and item.get("call_id") == call_id
    )


def _function_response(
    response_id: str, call_id: str, name: str, args: dict[str, Any]
) -> str:
    return sse(
        [
            ev_response_created(response_id),
            _function_call(call_id, name, args),
            ev_completed(response_id),
        ]
    )


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


def _goal_config(home: Path, server: MockResponsesServer) -> None:
    _rich_config(home, server)
    config_path = home / "config.toml"
    config = config_path.read_text(encoding="utf-8")
    config = config.replace(
        "[features]\n",
        "[features]\ngoals = true\n",
    )
    config_path.write_text(config, encoding="utf-8")


def test_packaged_model_wait_agent_times_out_without_activity(
    package: SmokePackage,
    responses_server: MockResponsesServer,
) -> None:
    """No mailbox event is not evidence that an agent was delivered or completed."""
    home = Path(package.environment["CODEX_HOME"])
    _rich_config(home, responses_server)

    responses_server.enqueue_sse(
        sse(
            [
                ev_response_created("quiet-wait"),
                _function_call("quiet-wait-call", "wait_agent", {"timeout_ms": 1500}),
                ev_completed("quiet-wait"),
            ]
        )
    )
    responses_server.enqueue_assistant_message(
        "no actionable work", response_id="quiet-final"
    )

    with Codex(
        config=CodexConfig(
            codex_bin=str(package.cli),
            cwd=str(package.directory),
            env={**package.environment, "CODEX_HOME": str(home)},
        )
    ) as client:
        thread = client.thread_start(
            ephemeral=False,
            approval_mode=ApprovalMode.deny_all,
            sandbox=Sandbox.workspace_write,
        )
        turn = thread.run(
            "Wait briefly for agent activity; continue if nothing arrives."
        )
        assert turn.final_response == "no actionable work"
        thread_read = thread.read(include_turns=True)

    result = _tool_output(responses_server, "quiet-wait-call")
    assert result["timed_out"] is True, result
    assert result["outcome"] == "timeout", result
    wait_item = _collab_item(thread_read, "quiet-wait-call")
    assert wait_item.wait_outcome.value == "timeout", wait_item
    assert wait_item.receiver_thread_ids == [], wait_item
    assert wait_item.queued_update_count == 0, wait_item
    assert wait_item.agents_states == {}, wait_item
    request = next(
        req for req in responses_server.requests() if req.path == "/v1/responses"
    )
    tools = request.body_json().get("tools")
    assert isinstance(tools, list)
    assert "wait_agent" in _tool_names(tools), tools


def test_packaged_model_nested_spawn_recovery_and_list_after_resume(
    package: SmokePackage,
) -> None:
    """Actual V2 tools create nested children, reload by messaging, and list stable identities."""
    home = package.directory / "nested-agent-recovery-home"
    home.mkdir()
    environment = {**package.environment, "CODEX_HOME": str(home)}

    with MockResponsesServer() as server:
        _rich_config(home, server)
        with Codex(
            config=CodexConfig(
                codex_bin=str(package.cli),
                cwd=str(package.directory),
                env=environment,
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
                    and _has_user_marker(request, "ROOT_NESTED_SPAWN_MARKER")
                    and not _has_call_output(request, "spawn-worker")
                ),
                _function_response(
                    "root-spawn-worker",
                    "spawn-worker",
                    "spawn_agent",
                    {
                        "task_name": "worker",
                        "message": "WORKER_INITIAL_MARKER",
                        "model": "package-smoke",
                        "reasoning_effort": "medium",
                    },
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_call_output(request, "spawn-worker")
                    and not _has_user_marker(request, "ROOT_RECOVERY_LIST_MARKER")
                    and not _has_call_output(request, "list-after-reload")
                ),
                sse(
                    [
                        ev_response_created("root-after-spawn"),
                        ev_assistant_message(
                            "root-first-message", "root first turn complete"
                        ),
                        ev_completed("root-after-spawn"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) != root_id
                    and request.header("x-codex-parent-thread-id") == root_id
                    and _has_user_marker(request, "WORKER_INITIAL_MARKER")
                    and not _has_call_output(request, "spawn-grandchild")
                ),
                _function_response(
                    "worker-spawn-grandchild",
                    "spawn-grandchild",
                    "spawn_agent",
                    {
                        "task_name": "grandchild",
                        "message": "GRANDCHILD_INITIAL_MARKER",
                        "model": "package-smoke",
                        "reasoning_effort": "medium",
                    },
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _has_call_output(request, "spawn-grandchild")
                    and not _has_user_marker(request, "WORKER_AFTER_RELOAD_MARKER")
                    and not _has_call_output(request, "followup-worker-after-reload")
                ),
                sse(
                    [
                        ev_response_created("worker-after-grandchild"),
                        ev_assistant_message(
                            "worker-first-message", "worker first turn complete"
                        ),
                        ev_completed("worker-after-grandchild"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _has_user_marker(request, "GRANDCHILD_INITIAL_MARKER")
                    and not _has_user_marker(request, "GRANDCHILD_AFTER_RELOAD_MARKER")
                ),
                sse(
                    [
                        ev_response_created("grandchild-initial"),
                        ev_assistant_message(
                            "grandchild-first-message", "grandchild first turn complete"
                        ),
                        ev_completed("grandchild-initial"),
                    ]
                ),
            )

            initial = root.run("ROOT_NESTED_SPAWN_MARKER: start the worker.")
            assert initial.final_response == "root first turn complete"

        worker = _tool_output(server, "spawn-worker")
        grandchild = _tool_output(server, "spawn-grandchild")
        worker_id = worker["agent_id"]
        grandchild_id = grandchild["agent_id"]
        uuid.UUID(worker_id)
        uuid.UUID(grandchild_id)
        assert worker["configured_model"] == "package-smoke", worker
        assert worker["configured_reasoning_effort"] == "medium", worker
        assert grandchild["configured_model"] == "package-smoke", grandchild
        assert grandchild["configured_reasoning_effort"] == "medium", grandchild

        # A second packaged client reloads the root; follow-ups lazily reload
        # the nested V2 records, and list_agents returns the actual persisted tree.
        with Codex(
            config=CodexConfig(
                codex_bin=str(package.cli),
                cwd=str(package.directory),
                env=environment,
            )
        ) as resumed_client:
            resumed_root = resumed_client.thread_resume(root_id)
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_user_marker(request, "ROOT_RECOVERY_LIST_MARKER")
                    and not _has_call_output(request, "list-after-reload")
                ),
                sse(
                    [
                        ev_response_created("root-recovery-actions"),
                        _function_call(
                            "followup-worker-after-reload",
                            "followup_task",
                            {
                                "target": "/root/worker",
                                "message": "WORKER_AFTER_RELOAD_MARKER",
                            },
                        ),
                        _function_call(
                            "followup-grandchild-after-reload",
                            "followup_task",
                            {
                                "target": "/root/worker/grandchild",
                                "message": "GRANDCHILD_AFTER_RELOAD_MARKER",
                            },
                        ),
                        _function_call("list-after-reload", "list_agents", {}),
                        ev_completed("root-recovery-actions"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_call_output(request, "list-after-reload")
                ),
                sse(
                    [
                        ev_response_created("root-recovery-finished"),
                        ev_assistant_message(
                            "root-recovery-message", "recovery listing complete"
                        ),
                        ev_completed("root-recovery-finished"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == worker_id
                    and _has_user_marker(request, "WORKER_AFTER_RELOAD_MARKER")
                ),
                sse(
                    [
                        ev_response_created("worker-after-reload"),
                        ev_assistant_message(
                            "worker-recovery-message", "worker recovered"
                        ),
                        ev_completed("worker-after-reload"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == grandchild_id
                    and _has_user_marker(request, "GRANDCHILD_AFTER_RELOAD_MARKER")
                ),
                sse(
                    [
                        ev_response_created("grandchild-after-reload"),
                        ev_assistant_message(
                            "grandchild-recovery-message", "grandchild recovered"
                        ),
                        ev_completed("grandchild-after-reload"),
                    ]
                ),
            )
            resumed = resumed_root.run(
                "ROOT_RECOVERY_LIST_MARKER: wake descendants and list them."
            )
            assert resumed.final_response == "recovery listing complete"

        agents = _tool_output(server, "list-after-reload")["agents"]
        by_id = {agent["agent_id"]: agent for agent in agents}
        assert worker_id in by_id and grandchild_id in by_id, agents
        assert by_id[worker_id]["canonical_path"] == "/root/worker", by_id[worker_id]
        assert by_id[grandchild_id]["canonical_path"] == "/root/worker/grandchild", (
            by_id[grandchild_id]
        )
        assert by_id[worker_id]["agent_status"] is not None, by_id[worker_id]
        assert by_id[grandchild_id]["agent_status"] is not None, by_id[grandchild_id]
        for agent in (by_id[worker_id], by_id[grandchild_id]):
            assert not any(
                key in agent
                for key in ("prompt", "instructions", "credentials", "actual_model")
            )


def test_packaged_model_queue_only_message_does_not_wake_until_followup_and_exact_join(
    package: SmokePackage,
) -> None:
    """Queue-only mail is counted but does not wake; a target follow-up does."""
    home = package.directory / "queue-versus-followup-home"
    home.mkdir()
    environment = {**package.environment, "CODEX_HOME": str(home)}

    with MockResponsesServer() as server:
        _rich_config(home, server)
        with Codex(
            config=CodexConfig(
                codex_bin=str(package.cli),
                cwd=str(package.directory),
                env=environment,
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
                    and _has_user_marker(request, "QUEUE_WAKE_ROOT_MARKER")
                    and not _has_all_call_outputs(
                        request, {"spawn-waiter", "spawn-sender"}
                    )
                ),
                sse(
                    [
                        ev_response_created("queue-root-spawns"),
                        _function_call(
                            "spawn-waiter",
                            "spawn_agent",
                            {
                                "task_name": "waiter",
                                "message": "WAITER_INITIAL_MARKER",
                                "model": "package-smoke",
                                "reasoning_effort": "medium",
                            },
                        ),
                        _function_call(
                            "spawn-sender",
                            "spawn_agent",
                            {
                                "task_name": "sender",
                                "message": "SENDER_INITIAL_MARKER",
                                "model": "package-smoke",
                                "reasoning_effort": "medium",
                            },
                        ),
                        ev_completed("queue-root-spawns"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_all_call_outputs(request, {"spawn-waiter", "spawn-sender"})
                    and not _has_call_output(request, "root-exact-wait-call")
                ),
                sse(
                    [
                        ev_response_created("root-exact-target-wait"),
                        _function_call(
                            "root-exact-wait-call",
                            "wait_agent",
                            {
                                "targets": ["/root/waiter"],
                                "return_when": "all",
                                "timeout_ms": 25000,
                            },
                        ),
                        ev_completed("root-exact-target-wait"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    request.header("x-codex-parent-thread-id") == root_id
                    and _has_user_marker(request, "WAITER_INITIAL_MARKER")
                    and not _has_call_output(request, "waiter-mailbox-wait-call")
                ),
                sse(
                    [
                        ev_response_created("waiter-pending"),
                        _function_call(
                            "waiter-mailbox-wait-call",
                            "wait_agent",
                            {"timeout_ms": 25000},
                        ),
                        ev_completed("waiter-pending"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    request.header("x-codex-parent-thread-id") == root_id
                    and _has_user_marker(request, "SENDER_INITIAL_MARKER")
                    and not _has_call_output(request, "queue-only-call")
                ),
                _function_response(
                    "sender-queue-turn",
                    "queue-only-call",
                    "send_message",
                    {
                        "target": "/root/waiter",
                        "message": "QUEUE_ONLY_SENTINEL",
                    },
                ),
            )
            followup_route = server.enqueue_sse_for_request(
                lambda request: (
                    request.header("x-codex-parent-thread-id") == root_id
                    and _has_call_output(request, "queue-only-call")
                    and not _has_call_output(request, "actionable-followup-call")
                ),
                _function_response(
                    "sender-followup-turn",
                    "actionable-followup-call",
                    "followup_task",
                    {
                        "target": "/root/waiter",
                        "message": "FOLLOWUP_TURN_SENTINEL",
                    },
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    request.header("x-codex-parent-thread-id") == root_id
                    and _has_call_output(request, "actionable-followup-call")
                ),
                sse(
                    [
                        ev_response_created("sender-finished"),
                        ev_assistant_message("sender-final", "sender done"),
                        ev_completed("sender-finished"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    request.header("x-codex-parent-thread-id") == root_id
                    and _has_call_output(request, "waiter-mailbox-wait-call")
                ),
                sse(
                    [
                        ev_response_created("waiter-finished"),
                        ev_assistant_message("waiter-final", "waiter done"),
                        ev_completed("waiter-finished"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_call_output(request, "root-exact-wait-call")
                ),
                sse(
                    [
                        ev_response_created("root-join-finished"),
                        ev_assistant_message("root-final", "selected target finished"),
                        ev_completed("root-join-finished"),
                    ]
                ),
            )
            turn = root.run(
                "QUEUE_WAKE_ROOT_MARKER: spawn, wait only for waiter, then finish."
            )
            assert turn.final_response == "selected target finished"
            waiter_id = _tool_output(server, "spawn-waiter")["agent_id"]
            waiter_read = client.thread_resume(waiter_id).read(include_turns=True)
            root_read = root.read(include_turns=True)

        waiter_result = _tool_output(server, "waiter-mailbox-wait-call")
        assert waiter_result["outcome"] == "unattributed_mailbox_activity", (
            waiter_result
        )
        waiter_item = _collab_item(waiter_read, "waiter-mailbox-wait-call")
        assert waiter_item.wait_outcome.value == "unattributedMailboxActivity", (
            waiter_item
        )
        assert waiter_item.receiver_thread_ids == [], waiter_item
        assert waiter_item.queued_update_count >= 1, waiter_item
        assert waiter_item.agents_states == {}, waiter_item
        assert _call_output_count(server, "queue-only-call") == 1
        assert _call_output_count(server, "actionable-followup-call") == 1
        followup_route.wait_until_selected()
        requests = [
            request
            for request in server.requests()
            if request.path == "/v1/responses"
        ]
        followup_emission_index = next(
            index
            for index, request in enumerate(requests)
            if request.header("x-codex-parent-thread-id") == root_id
            and _has_call_output(request, "queue-only-call")
            and not _has_call_output(request, "actionable-followup-call")
        )
        waiter_resumed_index = next(
            index
            for index, request in enumerate(requests)
            if _thread_id(request) == waiter_id
            and _has_call_output(request, "waiter-mailbox-wait-call")
        )
        assert followup_emission_index < waiter_resumed_index, requests

        root_result = _tool_output(server, "root-exact-wait-call")
        assert root_result["outcome"] == "target_terminal_all", root_result
        root_wait_item = _collab_item(root_read, "root-exact-wait-call")
        assert root_wait_item.wait_outcome.value == "targetTerminalAll", root_wait_item
        assert root_wait_item.receiver_thread_ids == [waiter_id], root_wait_item
        assert root_wait_item.receiver_agents[0].thread_id == waiter_id, root_wait_item
        assert root_wait_item.agents_states[waiter_id].status.value == "completed", (
            root_wait_item
        )


def test_packaged_model_goal_continuation_and_terminal_transition(
    package: SmokePackage,
) -> None:
    """Drive goal-triggered model turns through intermediate success to completion.

    The separate Rust lifecycle witness, not this HTTP fixture, establishes the
    exact idle interval before automatic goal continuation.
    """
    home = package.directory / "goal-continuation-home"
    home.mkdir()
    environment = {**package.environment, "CODEX_HOME": str(home)}

    with MockResponsesServer() as server:
        _goal_config(home, server)
        with Codex(
            config=CodexConfig(
                codex_bin=str(package.cli),
                cwd=str(package.directory),
                env=environment,
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
                    and _has_user_marker(request, "GOAL_ROOT_MARKER")
                    and not _has_call_output(request, "goal-worker-spawn")
                ),
                _function_response(
                    "root-spawn-goal-worker",
                    "goal-worker-spawn",
                    "spawn_agent",
                    {
                        "task_name": "worker",
                        "message": "GOAL_WORKER_INITIAL_MARKER",
                        "model": "package-smoke",
                        "reasoning_effort": "medium",
                    },
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_call_output(request, "goal-worker-spawn")
                    and not _has_call_output(request, "root-goal-wait-call")
                ),
                sse(
                    [
                        ev_response_created("root-goal-wait"),
                        _function_call(
                            "root-goal-wait-call",
                            "wait_agent",
                            {
                                "targets": ["/root/worker"],
                                "return_when": "all",
                                "timeout_ms": 20000,
                            },
                        ),
                        ev_completed("root-goal-wait"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    request.header("x-codex-parent-thread-id") == root_id
                    and _has_user_marker(request, "GOAL_WORKER_INITIAL_MARKER")
                    and not _has_call_output(request, "worker-create-goal-call")
                ),
                _function_response(
                    "worker-create-goal",
                    "worker-create-goal-call",
                    "create_goal",
                    {
                        "objective": "Complete the synthetic goal lifecycle",
                    },
                    namespace=None,
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    request.header("x-codex-parent-thread-id") == root_id
                    and _has_call_output(request, "worker-create-goal-call")
                    and not _is_goal_turn(request)
                ),
                sse(
                    [
                        ev_response_created("worker-goal-established"),
                        ev_assistant_message("worker-goal-created", "goal established"),
                        ev_completed("worker-goal-established"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _is_goal_turn(request)
                    and not _contains_input_text(request, "GOAL_PROGRESS_ONE")
                    and not _contains_input_text(request, "goal-queue-only-call")
                ),
                sse(
                    [
                        ev_response_created("goal-progress-one"),
                        ev_assistant_message(
                            "goal-progress-one-message", "GOAL_PROGRESS_ONE"
                        ),
                        ev_completed("goal-progress-one"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _is_goal_turn(request)
                    and _contains_input_text(request, "GOAL_PROGRESS_ONE")
                    and not _contains_input_text(request, "goal-queue-only-call")
                ),
                _function_response(
                    "goal-queue-only-turn",
                    "goal-queue-only-call",
                    "send_message",
                    {
                        "target": "/root",
                        "message": "GOAL_QUEUE_ONLY_SENTINEL",
                    },
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _is_goal_turn(request)
                    and _has_call_output(request, "goal-queue-only-call")
                    and not _contains_input_text(request, "GOAL_PROGRESS_TWO")
                ),
                sse(
                    [
                        ev_response_created("goal-progress-two"),
                        ev_assistant_message(
                            "goal-progress-two-message", "GOAL_PROGRESS_TWO"
                        ),
                        ev_completed("goal-progress-two"),
                    ]
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _is_goal_turn(request)
                    and _contains_input_text(request, "GOAL_PROGRESS_TWO")
                    and not _has_call_output(request, "goal-update-complete-call")
                ),
                _function_response(
                    "goal-complete-turn",
                    "goal-update-complete-call",
                    "update_goal",
                    {
                        "status": "complete",
                    },
                    namespace=None,
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _is_goal_turn(request)
                    and _has_call_output(request, "goal-update-complete-call")
                ),
                sse(
                    [
                        ev_response_created("goal-terminal-response"),
                        ev_assistant_message("goal-terminal-message", "goal complete"),
                        ev_completed("goal-terminal-response"),
                    ]
                ),
            )
            turn = root.run("GOAL_ROOT_MARKER: start the worker and wait for its goal.")
            assert turn.final_response == "goal complete"
            root_read = root.read(include_turns=True)

        goal_requests = [
            request for request in server.requests() if _is_goal_turn(request)
        ]
        assert len(goal_requests) == 5, [
            request.body_json() for request in goal_requests
        ]
        result = _tool_output(server, "root-goal-wait-call")
        assert result["outcome"] == "target_terminal_all", result
        goal_worker_id = _tool_output(server, "goal-worker-spawn")["agent_id"]
        goal_wait_item = _collab_item(root_read, "root-goal-wait-call")
        assert goal_wait_item.wait_outcome.value == "targetTerminalAll", goal_wait_item
        assert goal_wait_item.receiver_thread_ids == [goal_worker_id], goal_wait_item
        assert goal_wait_item.receiver_agents[0].thread_id == goal_worker_id, (
            goal_wait_item
        )
        assert (
            goal_wait_item.agents_states[goal_worker_id].status.value == "completed"
        ), goal_wait_item
        assert goal_wait_item.queued_update_count >= 1, goal_wait_item
        assert _call_output_count(server, "worker-create-goal-call") == 1
        assert _call_output_count(server, "goal-update-complete-call") == 1
