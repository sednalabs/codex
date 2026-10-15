"""Artifact-bound model-facing controls for packaged multi-agent execution.

These cases exercise the actual Responses tool surface. They do not attest to
the provider-effective model: the local server is a synthetic transport.
"""

import json
import threading
import uuid
from concurrent.futures import ThreadPoolExecutor
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


def _tool_output(
    server: MockResponsesServer, thread_id: str, call_id: str
) -> dict[str, Any]:
    request = server.wait_for_request(
        lambda captured: (
            captured.path == "/v1/responses"
            and _thread_id(captured) == thread_id
            and _has_any_call_output(captured, call_id)
        )
    )
    outputs = [
        item.get("output")
        for item in request.input()
        if item.get("type") == "function_call_output" and item.get("call_id") == call_id
    ]
    assert len(outputs) == 1, f"expected one exact output for {call_id}: {outputs!r}"

    # Retained conversation inputs may repeat an already-sent output. Accept
    # identical context copies, but reject duplicate items in one request or
    # any conflicting payload for the same thread and call ID.
    for captured in server.requests():
        if captured.path != "/v1/responses" or _thread_id(captured) != thread_id:
            continue
        repeated_outputs = [
            item.get("output")
            for item in captured.input()
            if item.get("type") == "function_call_output"
            and item.get("call_id") == call_id
        ]
        if repeated_outputs:
            assert len(repeated_outputs) == 1, (
                f"duplicate output items for {call_id}: {repeated_outputs!r}"
            )
            assert repeated_outputs[0] == outputs[0], (
                f"conflicting output history for {call_id}: {repeated_outputs!r}"
            )

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


def _sub_agent_activity(thread_read: Any, call_id: str) -> Any:
    items = [
        item.root
        for turn in thread_read.thread.turns
        for item in turn.items
        if getattr(item.root, "id", None) == call_id
    ]
    assert len(items) == 1, f"missing unique native activity for {call_id}: {items!r}"
    item = items[0]
    assert item.type == "subAgentActivity", item
    return item


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
    if not isinstance(turn_metadata, str):
        return False
    try:
        turn_metadata = json.loads(turn_metadata)
    except json.JSONDecodeError:
        return False
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


def _has_any_call_output(request: CapturedResponsesRequest, call_id: str) -> bool:
    return any(
        item.get("type") == "function_call_output" and item.get("call_id") == call_id
        for item in request.input()
    )


def _has_all_call_outputs(
    request: CapturedResponsesRequest, call_ids: set[str]
) -> bool:
    outputs = {
        item.get("call_id")
        for item in request.input()
        if item.get("type") == "function_call_output"
    }
    return call_ids <= outputs


def _function_response(
    response_id: str,
    call_id: str,
    name: str,
    args: dict[str, Any],
    *,
    namespace: str | None = "collaboration",
) -> str:
    return sse(
        [
            ev_response_created(response_id),
            _function_call(call_id, name, args, namespace=namespace),
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

    result = _tool_output(responses_server, thread.id, "quiet-wait-call")
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
            initial_descendant_ids: dict[str, str] = {}

            def initial_descendant(
                request: CapturedResponsesRequest,
                role: str,
                parent_id: str | None,
                marker: str,
            ) -> bool:
                thread_id = _thread_id(request)
                if (
                    not isinstance(thread_id, str)
                    or parent_id is None
                    or thread_id == parent_id
                    or request.header("x-codex-parent-thread-id") != parent_id
                    or not _has_user_marker(request, marker)
                ):
                    return False
                # Bind only the first exact lineage/phase request. The actual
                # persisted spawn records below must attest these same UUIDs.
                return thread_id == initial_descendant_ids.setdefault(role, thread_id)

            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_user_marker(request, "ROOT_NESTED_SPAWN_MARKER")
                    and not _contains_input_text(request, "ROOT_RECOVERY_LIST_MARKER")
                    and not _has_call_output(request, "spawn-worker")
                ),
                _function_response(
                    "root-spawn-worker",
                    "spawn-worker",
                    "spawn_agent",
                    {
                        "task_name": "worker",
                        "message": "WORKER_INITIAL_MARKER",
                        "model": _SPAWN_MODEL,
                        "reasoning_effort": "medium",
                    },
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_call_output(request, "spawn-worker")
                    and not _contains_input_text(request, "ROOT_RECOVERY_LIST_MARKER")
                    and not _has_call_output(request, "root-wait-worker")
                ),
                _function_response(
                    "root-waits-worker",
                    "root-wait-worker",
                    "wait_agent",
                    {
                        "targets": ["/root/worker"],
                        "return_when": "all",
                        "timeout_ms": 25000,
                    },
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_call_output(request, "root-wait-worker")
                    and not _contains_input_text(request, "ROOT_RECOVERY_LIST_MARKER")
                    and not _has_call_output(request, "root-wait-grandchild")
                ),
                _function_response(
                    "root-waits-grandchild",
                    "root-wait-grandchild",
                    "wait_agent",
                    {
                        "targets": ["/root/worker/grandchild"],
                        "return_when": "all",
                        "timeout_ms": 25000,
                    },
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_call_output(request, "root-wait-grandchild")
                    and not _contains_input_text(request, "ROOT_RECOVERY_LIST_MARKER")
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
                    not _contains_input_text(request, "WORKER_AFTER_RELOAD_MARKER")
                    and initial_descendant(
                        request, "worker", root_id, "WORKER_INITIAL_MARKER"
                    )
                    and not _has_call_output(request, "spawn-grandchild")
                ),
                _function_response(
                    "worker-spawn-grandchild",
                    "spawn-grandchild",
                    "spawn_agent",
                    {
                        "task_name": "grandchild",
                        "message": "GRANDCHILD_INITIAL_MARKER",
                        "model": _SPAWN_MODEL,
                        "reasoning_effort": "medium",
                    },
                ),
            )
            server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == initial_descendant_ids.get("worker")
                    and request.header("x-codex-parent-thread-id") == root_id
                    and _has_call_output(request, "spawn-grandchild")
                    and not _contains_input_text(request, "WORKER_AFTER_RELOAD_MARKER")
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
                    not _contains_input_text(request, "GRANDCHILD_AFTER_RELOAD_MARKER")
                    and initial_descendant(
                        request,
                        "grandchild",
                        initial_descendant_ids.get("worker"),
                        "GRANDCHILD_INITIAL_MARKER",
                    )
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
            worker_body = _tool_output(server, root_id, "spawn-worker")
            assert worker_body["task_name"] == "/root/worker", worker_body
            root_read = root.read(include_turns=True)
            worker_spawn = _sub_agent_activity(root_read, "spawn-worker")
            worker_id = worker_spawn.agent_thread_id
            assert worker_id == initial_descendant_ids["worker"], worker_spawn
            assert worker_spawn.agent_path == "/root/worker", worker_spawn
            assert worker_spawn.kind.value == "started", worker_spawn
            assert worker_spawn.model == _SPAWN_MODEL, worker_spawn
            assert worker_spawn.reasoning_effort.value == "medium", worker_spawn
            uuid.UUID(worker_id)
            worker_read = client._client.thread_read(worker_id, include_turns=True)
            grandchild_body = _tool_output(server, worker_id, "spawn-grandchild")
            assert grandchild_body["task_name"] == "/root/worker/grandchild", (
                grandchild_body
            )
            grandchild_spawn = _sub_agent_activity(worker_read, "spawn-grandchild")
            grandchild_id = grandchild_spawn.agent_thread_id
            assert grandchild_id == initial_descendant_ids["grandchild"], (
                grandchild_spawn
            )
            assert grandchild_spawn.agent_path == "/root/worker/grandchild", (
                grandchild_spawn
            )
            assert grandchild_spawn.kind.value == "started", grandchild_spawn
            assert grandchild_spawn.model == _SPAWN_MODEL, grandchild_spawn
            assert grandchild_spawn.reasoning_effort.value == "medium", grandchild_spawn
            uuid.UUID(grandchild_id)
            worker_wait = _collab_item(root_read, "root-wait-worker")
            assert worker_wait.receiver_thread_ids == [worker_id], worker_wait
            assert worker_wait.wait_outcome.value == "targetTerminalAll", worker_wait
            grandchild_wait = _collab_item(root_read, "root-wait-grandchild")
            assert grandchild_wait.receiver_thread_ids == [grandchild_id], (
                grandchild_wait
            )
            assert grandchild_wait.wait_outcome.value == "targetTerminalAll", (
                grandchild_wait
            )

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
                    and _contains_input_text(request, "ROOT_RECOVERY_LIST_MARKER")
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
                    and _contains_input_text(request, "ROOT_RECOVERY_LIST_MARKER")
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
            recovery_response_gate = threading.Event()
            worker_recovery_route = server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == worker_id
                    and _contains_input_text(request, "WORKER_AFTER_RELOAD_MARKER")
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
                gate=recovery_response_gate,
            )
            grandchild_recovery_route = server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == grandchild_id
                    and _contains_input_text(request, "GRANDCHILD_AFTER_RELOAD_MARKER")
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
                gate=recovery_response_gate,
            )
            resumed = resumed_root.run(
                "ROOT_RECOVERY_LIST_MARKER: wake descendants and list them."
            )
            assert resumed.final_response == "recovery listing complete"
            resumed_root_read = resumed_root.read(include_turns=True)
            worker_followup = _sub_agent_activity(
                resumed_root_read, "followup-worker-after-reload"
            )
            grandchild_followup = _sub_agent_activity(
                resumed_root_read, "followup-grandchild-after-reload"
            )
            assert worker_followup.agent_thread_id == worker_id, worker_followup
            assert worker_followup.agent_path == "/root/worker", worker_followup
            assert worker_followup.kind.value == "interacted", worker_followup
            assert grandchild_followup.agent_thread_id == grandchild_id, (
                grandchild_followup
            )
            assert grandchild_followup.agent_path == "/root/worker/grandchild", (
                grandchild_followup
            )
            assert grandchild_followup.kind.value == "interacted", grandchild_followup

            # Follow-up receipts acknowledge submission, not completion. Keep
            # this client alive until both cold-restored descendants finish.
            # Hold their responses until the exact current turns are subscribed,
            # so neither an old terminal turn nor a fast completion can win.
            recovery_turn_ids: list[str] = []
            try:
                for descendant_id, route in (
                    (worker_id, worker_recovery_route),
                    (grandchild_id, grandchild_recovery_route),
                ):
                    route.wait_until_selected(timeout_s=10)
                    descendant = resumed_client._client.thread_read(
                        descendant_id, include_turns=True
                    )
                    active_turns = [
                        turn
                        for turn in descendant.thread.turns
                        if turn.status.value == "inProgress"
                    ]
                    assert len(active_turns) == 1, active_turns
                    turn_id = active_turns[0].id
                    resumed_client._client.register_turn_notifications(turn_id)
                    recovery_turn_ids.append(turn_id)
                recovery_response_gate.set()
                for turn_id in recovery_turn_ids:
                    completed = resumed_client._client.wait_for_turn_completed(turn_id)
                    assert completed.turn.id == turn_id, completed
                    assert completed.turn.status.value == "completed", completed
            finally:
                recovery_response_gate.set()
                for turn_id in recovery_turn_ids:
                    resumed_client._client.unregister_turn_notifications(turn_id)

        listed_agents = _tool_output(server, root_id, "list-after-reload")["agents"]
        by_name = {agent["agent_name"]: agent for agent in listed_agents}
        assert "/root/worker" in by_name, listed_agents
        assert "/root/worker/grandchild" in by_name, listed_agents
        assert by_name["/root/worker"]["agent_status"] is not None, by_name
        assert by_name["/root/worker/grandchild"]["agent_status"] is not None, by_name
        for agent in (by_name["/root/worker"], by_name["/root/worker/grandchild"]):
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
            waiter_response_gate = threading.Event()
            queue_only_response_gate = threading.Event()
            followup_response_gate = threading.Event()
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
                                "model": _SPAWN_MODEL,
                                "reasoning_effort": "medium",
                            },
                        ),
                        _function_call(
                            "spawn-sender",
                            "spawn_agent",
                            {
                                "task_name": "sender",
                                "message": "SENDER_INITIAL_MARKER",
                                "model": _SPAWN_MODEL,
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
            waiter_wait_route = server.enqueue_sse_for_request(
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
                gate=waiter_response_gate,
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
                gate=queue_only_response_gate,
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
                gate=followup_response_gate,
            )
            sender_finished_route = server.enqueue_sse_for_request(
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
            waiter_resumed_route = server.enqueue_sse_for_request(
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

            def run_root() -> Any:
                return root.run(
                    "QUEUE_WAKE_ROOT_MARKER: spawn, wait only for waiter, then finish."
                )

            def wait_for_wait_item_start(
                waiter_thread_id: str, waiter_turn_id: str
            ) -> Any:
                try:
                    while True:
                        notification = waiter_client.next_turn_notification(
                            waiter_turn_id
                        )
                        payload = notification.payload
                        item = getattr(getattr(payload, "item", None), "root", None)
                        if (
                            notification.method == "item/started"
                            and getattr(payload, "thread_id", None) == waiter_thread_id
                            and getattr(payload, "turn_id", None) == waiter_turn_id
                            and getattr(item, "id", None) == "waiter-mailbox-wait-call"
                            and getattr(getattr(item, "tool", None), "value", None)
                            == "wait"
                            and getattr(getattr(item, "status", None), "value", None)
                            == "inProgress"
                        ):
                            return payload
                finally:
                    waiter_client.unregister_turn_notifications(waiter_turn_id)

            waiter_client: Any = None
            waiter_turn_id: str | None = None
            executor = ThreadPoolExecutor(max_workers=2)
            try:
                root_future = executor.submit(run_root)
                waiter_wait_route.wait_until_selected(timeout_s=10)
                server.wait_for_request(
                    lambda request: (
                        request.path == "/v1/responses"
                        and _thread_id(request) == root_id
                        and _has_all_call_outputs(
                            request, {"spawn-waiter", "spawn-sender"}
                        )
                    ),
                    timeout_s=10,
                )
                root_read = client._client.thread_read(root_id, include_turns=True)
                waiter_body = _tool_output(server, root_id, "spawn-waiter")
                sender_body = _tool_output(server, root_id, "spawn-sender")
                assert waiter_body["task_name"] == "/root/waiter", waiter_body
                assert sender_body["task_name"] == "/root/sender", sender_body
                waiter_spawn = _sub_agent_activity(root_read, "spawn-waiter")
                sender_spawn = _sub_agent_activity(root_read, "spawn-sender")
                waiter_id = waiter_spawn.agent_thread_id
                sender_id = sender_spawn.agent_thread_id
                assert waiter_spawn.agent_path == "/root/waiter", waiter_spawn
                assert sender_spawn.agent_path == "/root/sender", sender_spawn
                assert waiter_spawn.kind.value == "started", waiter_spawn
                assert sender_spawn.kind.value == "started", sender_spawn
                uuid.UUID(waiter_id)
                uuid.UUID(sender_id)
                waiter_snapshot = client._client.thread_read(
                    waiter_id, include_turns=True
                )
                active_turns = [
                    turn
                    for turn in waiter_snapshot.thread.turns
                    if getattr(getattr(turn, "status", None), "value", None)
                    == "inProgress"
                ]
                assert len(active_turns) == 1, active_turns
                waiter_turn_id = active_turns[0].id
                waiter_client = client._client
                waiter_client.register_turn_notifications(waiter_turn_id)
                wait_started_future = executor.submit(
                    wait_for_wait_item_start, waiter_id, waiter_turn_id
                )
                waiter_response_gate.set()
                wait_started = wait_started_future.result(timeout=10)
                assert wait_started.thread_id == waiter_id, wait_started
                assert wait_started.turn_id == waiter_turn_id, wait_started
                assert wait_started.item.root.id == "waiter-mailbox-wait-call", (
                    wait_started
                )
                assert wait_started.item.root.status.value == "inProgress", wait_started

                # Only release queue-only delivery after the real wait item has
                # started; the actionable response remains held for the quiet phase.
                queue_only_response_gate.set()
                followup_route.wait_until_selected(timeout_s=10)
                sender_read = client._client.thread_read(sender_id, include_turns=True)
                send_item = _collab_item(sender_read, "queue-only-call")
                assert send_item.type == "subAgentActivity", send_item
                assert send_item.kind.value == "interacted", send_item
                assert send_item.agent_thread_id == waiter_id, send_item
                assert send_item.agent_path == "/root/waiter", send_item

                assert not waiter_resumed_route.selected.wait(0.5), (
                    "queue-only delivery woke the registered waiter"
                )
                quiet_read = client._client.thread_read(waiter_id, include_turns=True)
                # Started items are live notifications, not durable history.
                # The bound start above and unchanged active turn witness the
                # quiet phase while the actionable response remains held.
                quiet_turns = [
                    turn
                    for turn in quiet_read.thread.turns
                    if turn.id == waiter_turn_id
                ]
                assert len(quiet_turns) == 1, quiet_read
                assert quiet_turns[0].status.value == "inProgress", quiet_turns
                assert not followup_response_gate.is_set()
                followup_response_gate.set()
                waiter_resumed_route.wait_until_selected()
                sender_finished_route.wait_until_selected(timeout_s=10)
                turn = root_future.result(timeout=30)
            except BaseException:
                waiter_response_gate.set()
                queue_only_response_gate.set()
                followup_response_gate.set()
                if waiter_client is not None and waiter_turn_id is not None:
                    waiter_client.unregister_turn_notifications(waiter_turn_id)
                client.close()
                raise
            finally:
                waiter_response_gate.set()
                queue_only_response_gate.set()
                followup_response_gate.set()
                executor.shutdown(wait=True, cancel_futures=True)
            assert turn.final_response == "selected target finished"
            waiter_read = client._client.thread_read(waiter_id, include_turns=True)
            root_read = root.read(include_turns=True)
            sender_read = client._client.thread_read(sender_id, include_turns=True)

        waiter_result = _tool_output(server, waiter_id, "waiter-mailbox-wait-call")
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
        followup_item = _sub_agent_activity(sender_read, "actionable-followup-call")
        assert followup_item.kind.value == "interacted", followup_item
        assert followup_item.agent_thread_id == waiter_id, followup_item
        assert followup_item.agent_path == "/root/waiter", followup_item

        root_result = _tool_output(server, root_id, "root-exact-wait-call")
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
                        "model": _SPAWN_MODEL,
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
            worker_goal_established_route = server.enqueue_sse_for_request(
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
            goal_progress_one_route = server.enqueue_sse_for_request(
                lambda request: (
                    _is_goal_turn(request)
                    and not _contains_input_text(request, "GOAL_PROGRESS_ONE")
                    and not _contains_input_text(request, "goal-queue-only-call")
                    and not _has_call_output(request, "goal-update-complete-call")
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
            goal_queue_only_route = server.enqueue_sse_for_request(
                lambda request: (
                    _is_goal_turn(request)
                    and _contains_input_text(request, "GOAL_PROGRESS_ONE")
                    and not _contains_input_text(request, "goal-queue-only-call")
                    and not _has_call_output(request, "goal-update-complete-call")
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
            goal_progress_two_route = server.enqueue_sse_for_request(
                lambda request: (
                    _is_goal_turn(request)
                    and _has_call_output(request, "goal-queue-only-call")
                    and not _contains_input_text(request, "GOAL_PROGRESS_TWO")
                    and not _has_call_output(request, "goal-update-complete-call")
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
            goal_update_route = server.enqueue_sse_for_request(
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
            goal_terminal_route = server.enqueue_sse_for_request(
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
            root_goal_terminal_route = server.enqueue_sse_for_request(
                lambda request: (
                    _thread_id(request) == root_id
                    and _has_call_output(request, "root-goal-wait-call")
                ),
                sse(
                    [
                        ev_response_created("root-goal-joined"),
                        ev_assistant_message("root-goal-finished", "goal complete"),
                        ev_completed("root-goal-joined"),
                    ]
                ),
            )
            turn = root.run("GOAL_ROOT_MARKER: start the worker and wait for its goal.")
            assert turn.final_response == "goal complete"
            goal_worker_body = _tool_output(server, root_id, "goal-worker-spawn")
            assert goal_worker_body["task_name"] == "/root/worker", goal_worker_body
            root_read = root.read(include_turns=True)
            goal_worker_spawn = _sub_agent_activity(root_read, "goal-worker-spawn")
            goal_worker_id = goal_worker_spawn.agent_thread_id
            assert goal_worker_spawn.agent_path == "/root/worker", goal_worker_spawn
            assert goal_worker_spawn.kind.value == "started", goal_worker_spawn
            uuid.UUID(goal_worker_id)
            worker_read = client._client.thread_read(goal_worker_id, include_turns=True)

        assert worker_goal_established_route.selected.is_set()
        assert goal_progress_one_route.selected.is_set()
        assert goal_queue_only_route.selected.is_set()
        assert goal_progress_two_route.selected.is_set()
        assert goal_update_route.selected.is_set()
        assert goal_terminal_route.selected.is_set()
        assert root_goal_terminal_route.selected.is_set()
        result = _tool_output(server, root_id, "root-goal-wait-call")
        assert result["outcome"] == "target_terminal_all", result
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
        created = _tool_output(server, goal_worker_id, "worker-create-goal-call")
        assert created["goal"]["threadId"] == goal_worker_id, created
        assert (
            created["goal"]["objective"] == "Complete the synthetic goal lifecycle"
        ), created
        assert created["goal"]["status"] == "active", created
        assert created["remainingTokens"] is None, created
        assert created["completionBudgetReport"] is None, created
        completed = _tool_output(server, goal_worker_id, "goal-update-complete-call")
        assert completed["goal"]["threadId"] == goal_worker_id, completed
        assert (
            completed["goal"]["objective"] == "Complete the synthetic goal lifecycle"
        ), completed
        assert completed["goal"]["status"] == "complete", completed
        assert completed["remainingTokens"] is None, completed
        assert (completed["completionBudgetReport"] is not None) == (
            completed["goal"]["timeUsedSeconds"] > 0
            or completed["goal"].get("tokenBudget") is not None
        ), completed
        queued_message = _sub_agent_activity(worker_read, "goal-queue-only-call")
        assert queued_message.kind.value == "interacted", queued_message
        assert queued_message.agent_thread_id == root_id, queued_message
        assert queued_message.agent_path == "/root", queued_message
