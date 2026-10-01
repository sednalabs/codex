"""Real model-facing tool consumers from the single downloaded Linux package.

These are initial artifact-bound cases.  Rich live/replayed TUI and scheduler
joins remain separate mandatory cases; this file makes no acceptance claim for
them merely because a tool is registered or a source fixture exists.
"""

import json
import uuid
from pathlib import Path
from typing import Any

from app_server_harness import (
    MockResponsesServer,
    ev_completed,
    ev_response_created,
    sse,
)
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage
from package_acceptance import _mock_config


def _function_call(call_id: str, name: str, args: dict[str, Any]) -> dict[str, Any]:
    return {
        "type": "response.output_item.done",
        "item": {
            "type": "function_call", "namespace": "collaboration",
            "call_id": call_id, "name": name, "arguments": json.dumps(args),
        },
    }


def _tool_output(server: MockResponsesServer, call_id: str) -> dict[str, Any]:
    outputs = [
        item.get("output")
        for request in server.requests() if request.path == "/v1/responses"
        for item in request.input()
        if item.get("type") == "function_call_output"
        and item.get("call_id") == call_id
    ]
    assert len(outputs) == 1, f"missing executed output for {call_id}: {outputs!r}"
    payload = outputs[0]
    if isinstance(payload, list):
        payload = next(part["text"] for part in payload if part.get("type") == "output_text")
    assert isinstance(payload, str)
    decoded = json.loads(payload)
    assert isinstance(decoded, dict), decoded
    return decoded


def _client(package: SmokePackage, home: Path) -> Codex:
    return Codex(config=CodexConfig(
        codex_bin=str(package.cli), cwd=str(package.directory),
        env={**package.environment, "CODEX_HOME": str(home)},
    ))


def _rich_config(home: Path, server: MockResponsesServer) -> None:
    """Expose configured identity without claiming provider-effective identity."""
    _mock_config(home, server, agent_tools=True)
    config_path = home / "config.toml"
    config = config_path.read_text(encoding="utf-8")
    config = config.replace("multi_agent_v2 = true\n", "")
    config = config.replace(
        "[model_providers.package_smoke]",
        "[features.multi_agent_v2]\n"
        "enabled = true\nhide_spawn_agent_metadata = false\n"
        "non_code_mode_only = true\n\n[model_providers.package_smoke]",
    )
    config_path.write_text('model_reasoning_effort = "medium"\n' + config, encoding="utf-8")


def test_model_receives_and_executes_root_only_agent_list(
    package: SmokePackage, responses_server: MockResponsesServer
) -> None:
    home = Path(package.environment["CODEX_HOME"])
    _mock_config(home, responses_server, agent_tools=True)
    responses_server.enqueue_sse(sse([
        ev_response_created("agents-empty-1"),
        _function_call("agents-empty-list", "list_agents", {}),
        ev_completed("agents-empty-1"),
    ]))
    responses_server.enqueue_assistant_message("list complete", response_id="agents-empty-2")
    with _client(package, home) as client:
        root_thread = client.thread_start(
            ephemeral=False, approval_mode=ApprovalMode.deny_all,
            sandbox=Sandbox.workspace_write,
        )
        turn = root_thread.run("List active agents without starting any child.")
        assert turn.final_response == "list complete"
        root_id = root_thread.id
    output = _tool_output(responses_server, "agents-empty-list")
    agents = output.get("agents")
    assert isinstance(agents, list) and len(agents) == 1, output
    root = agents[0]
    assert isinstance(root, dict), root
    assert root["agent_id"] == root_id
    assert root["agent_name"] == "/root"
    assert root["canonical_path"] == "/root"
    assert root["configured_model"] == "package-smoke"
    assert not any(key in output for key in ("prompt", "instructions", "credentials"))
    assert not any(key in root for key in ("prompt", "instructions", "credentials", "actual_model"))
    first = next(req for req in responses_server.requests() if req.path == "/v1/responses")
    tools = first.body_json().get("tools")
    assert isinstance(tools, list)
    assert any(
        tool.get("type") == "namespace"
        and tool.get("name") == "collaboration"
        and any(child.get("name") == "list_agents" for child in tool.get("tools", []))
        for tool in tools
    ), tools


def test_model_spawn_hidden_metadata_has_no_private_fields(
    package: SmokePackage, responses_server: MockResponsesServer
) -> None:
    """Default-hidden output is intentionally narrower than the rich visible mode."""
    home = Path(package.environment["CODEX_HOME"])
    _mock_config(home, responses_server, agent_tools=True)
    responses_server.enqueue_sse(sse([
        ev_response_created("agents-spawn-1"),
        _function_call(
            "agents-spawn-worker", "spawn_agent",
            {"task_name": "worker", "message": "Return a short harmless result."},
        ),
        ev_completed("agents-spawn-1"),
    ]))
    # Parent and child may request the mock concurrently after spawn; both
    # responses are harmless and do not carry provenance or account claims.
    for number in range(4):
        responses_server.enqueue_assistant_message(
            "synthetic completion", response_id=f"agents-spawn-followup-{number}"
        )
    with _client(package, home) as client:
        turn = client.thread_start(
            ephemeral=False, approval_mode=ApprovalMode.deny_all,
            sandbox=Sandbox.workspace_write,
        ).run("Spawn one harmless worker.")
        assert turn.final_response == "synthetic completion"
    output = _tool_output(responses_server, "agents-spawn-worker")
    assert output == {"task_name": "/root/worker"}, output
    assert not any(key in output for key in ("prompt", "instructions", "credentials"))


def test_visible_model_spawn_and_resumed_list_keep_stable_identity(
    package: SmokePackage, responses_server: MockResponsesServer
) -> None:
    home = package.directory / "rich-model-consumer-home"
    _rich_config(home, responses_server)
    responses_server.enqueue_sse(sse([
        ev_response_created("agents-visible-spawn"),
        _function_call(
            "agents-visible-worker", "spawn_agent",
            {"task_name": "worker", "message": "Return a short harmless result."},
        ),
        ev_completed("agents-visible-spawn"),
    ]))
    for number in range(4):
        responses_server.enqueue_assistant_message(
            "synthetic completion", response_id=f"agents-visible-followup-{number}"
        )
    with _client(package, home) as client:
        root = client.thread_start(
            ephemeral=False, approval_mode=ApprovalMode.deny_all,
            sandbox=Sandbox.workspace_write,
        )
        assert root.run("Spawn one visible worker.").final_response == "synthetic completion"
        root_id = root.id
    spawned = _tool_output(responses_server, "agents-visible-worker")
    assert spawned["task_name"] == "/root/worker"
    child_id = spawned["agent_id"]
    uuid.UUID(child_id)
    assert spawned["agent_status"] is not None
    assert spawned["configured_model"] == "package-smoke"
    assert spawned["configured_reasoning_effort"] == "medium"
    assert not any(key in spawned for key in ("prompt", "instructions", "credentials", "actual_model"))

    # A second packaged process resumes the owner before reloading the same
    # child, then asks the model-facing list handler for its current identity.
    with MockResponsesServer() as listing_server:
        _rich_config(home, listing_server)
        listing_server.enqueue_sse(sse([
            ev_response_created("agents-visible-list"),
            _function_call("agents-list-after-resume", "list_agents", {}),
            ev_completed("agents-visible-list"),
        ]))
        listing_server.enqueue_assistant_message("list complete", response_id="agents-visible-list-end")
        with _client(package, home) as client:
            root = client.thread_resume(root_id)
            assert client.thread_resume(child_id).id == child_id
            assert root.run("List the exact resumed worker.").final_response == "list complete"
        listed = _tool_output(listing_server, "agents-list-after-resume")
    matches = [agent for agent in listed["agents"] if agent["agent_id"] == child_id]
    assert len(matches) == 1, listed
    worker = matches[0]
    assert worker["agent_name"] == "/root/worker"
    assert worker["canonical_path"] == "/root/worker"
    assert worker["configured_model"] in ("package-smoke", None)
    if worker["configured_model"] is None:
        assert worker["configured_reasoning_effort"] is None
    else:
        assert worker["configured_reasoning_effort"] == "medium"
    assert not any(key in worker for key in ("prompt", "instructions", "credentials", "actual_model"))
