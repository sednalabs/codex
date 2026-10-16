"""Exercise Android dynamic tools through the exact packaged Codex CLI.

All provider responses are synthetic and served by this test process over
loopback. This covers the client protocol only; it does not contact a device.
"""

from __future__ import annotations

import json
import threading
from dataclasses import replace
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
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
from fixtures import SmokePackage
from tui_pty import PackagedTui


_PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p6kAAAAASUVORK5CYII="
_TOOLS = (
    "android.input.tap",
    "android.input.text",
    "android.inspect_ui",
    "interactive_session.install_build_from_run",
)
_DROP = object()


def _require(condition: bool, reason: str) -> None:
    if not condition:
        raise AssertionError(reason)


def _thread_id(request: CapturedResponsesRequest) -> str | None:
    metadata = request.body_json().get("client_metadata")
    return metadata.get("thread_id") if isinstance(metadata, dict) else None


def _contains_marker(request: CapturedResponsesRequest, marker: str) -> bool:
    return marker in json.dumps(request.input(), sort_keys=True)


def _has_call_output(request: CapturedResponsesRequest, call_id: str) -> bool:
    return any(
        item.get("type") == "function_call_output" and item.get("call_id") == call_id
        for item in request.input()
    )


def _function_response(response_id: str, call_id: str, name: str, args: dict[str, Any]) -> str:
    item = {
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "namespace": "codex_android",
            "arguments": json.dumps(args, sort_keys=True),
        },
    }
    return sse([ev_response_created(response_id), item, ev_completed(response_id)])


def _queue_tui_turn(
    server: MockResponsesServer,
    marker: str,
    call_id: str,
    name: str,
    args: dict[str, Any],
) -> None:
    server.enqueue_sse_for_request(
        lambda request: (
            request.path == "/v1/responses"
            and _contains_marker(request, marker)
            and not _has_call_output(request, call_id)
        ),
        _function_response(f"response-{call_id}", call_id, name, args),
    )
    server.enqueue_sse_for_request(
        lambda request: (
            request.path == "/v1/responses"
            and _has_call_output(request, call_id)
        ),
        sse(
            [
                ev_response_created(f"final-{call_id}"),
                ev_assistant_message(
                    f"message-{call_id}", f"SYNTHETIC_{call_id}_COMPLETED"
                ),
                ev_completed(f"final-{call_id}"),
            ]
        ),
    )


def _request_with_call_output(
    server: MockResponsesServer, thread_id: str, call_id: str
) -> tuple[CapturedResponsesRequest, dict[str, Any]]:
    request = server.wait_for_request(
        lambda candidate: _thread_id(candidate) == thread_id
        and _has_call_output(candidate, call_id)
    )
    outputs = [
        item
        for item in request.input()
        if item.get("type") == "function_call_output" and item.get("call_id") == call_id
    ]
    _require(len(outputs) == 1, "stage=model-result reason=function-output-cardinality")
    return request, outputs[0]


def _text_parts(value: Any) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, list):
        return [part for item in value for part in _text_parts(item)]
    if isinstance(value, dict):
        parts = [value["text"]] if isinstance(value.get("text"), str) else []
        return parts + [part for item in value.values() for part in _text_parts(item)]
    return []


def _has_native_image(value: Any) -> bool:
    if isinstance(value, list):
        return any(_has_native_image(item) for item in value)
    if isinstance(value, dict):
        if (
            value.get("type") == "input_image"
            and isinstance(value.get("image_url"), str)
            and value["image_url"].startswith("data:image/png;base64,")
        ):
            return True
        return any(_has_native_image(item) for item in value.values())
    if isinstance(value, str):
        try:
            return _has_native_image(json.loads(value))
        except (json.JSONDecodeError, TypeError):
            return False
    return False


class _FakeAndroidProvider:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._phase = ""
        self._calls: list[dict[str, Any]] = []
        self.install_started = threading.Event()
        self.release_install = threading.Event()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, _format: str, *_args: Any) -> None:
                return

            def do_POST(self) -> None:
                length = int(self.headers.get("Content-Length", "0"))
                if length <= 0 or length > 64 * 1024:
                    self.send_error(413)
                    return
                try:
                    request = json.loads(self.rfile.read(length))
                    if request.get("method") == "notifications/initialized":
                        self.send_response(202)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                    response = owner._respond(request)
                except Exception:
                    response = {"jsonrpc": "2.0", "id": None, "error": {"code": -32603, "message": "synthetic fixture error"}}
                if response is _DROP:
                    self.close_connection = True
                    return
                body = json.dumps(response, separators=(",", ":")).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/mcp"
        self._thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self._thread.start()

    def close(self) -> None:
        self.release_install.set()
        self.server.shutdown()
        self.server.server_close()
        self._thread.join(timeout=5)

    def __enter__(self) -> _FakeAndroidProvider:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def set_phase(self, phase: str) -> None:
        with self._lock:
            self._phase = phase

    def calls_for(self, phase: str) -> list[dict[str, Any]]:
        with self._lock:
            return [dict(call) for call in self._calls if call["phase"] == phase]

    def _respond(self, request: dict[str, Any]) -> Any:
        method = request.get("method")
        request_id = request.get("id")
        if method == "initialize":
            return {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "synthetic-android-provider", "version": "1"},
                },
            }
        if method == "tools/list":
            return {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "tools": [
                        {"name": name, "description": "synthetic test tool", "inputSchema": {"type": "object"}}
                        for name in _TOOLS
                    ]
                },
            }
        if method != "tools/call":
            return {"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "unknown synthetic method"}}

        params = request.get("params") or {}
        name = params.get("name")
        arguments = params.get("arguments") or {}
        with self._lock:
            phase = self._phase
            self._calls.append({"phase": phase, "name": name, "arguments": arguments})

        if phase == "step-transport-loss" and name == "android.input.tap":
            return _DROP
        if phase == "step-failure" and name == "android.input.text":
            return {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "isError": True,
                    "content": [{"type": "text", "text": "SYNTHETIC_ACTION_FAILURE"}],
                    "structuredContent": {},
                },
            }
        if name == "android.inspect_ui":
            return {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "structuredContent": {
                        "serial": arguments.get("serial"),
                        "node_count": 1,
                        "visible_ui": {"nodes": [{"label": f"SCREEN_{phase.upper().replace('-', '_')}"}]},
                    },
                    "content": [
                        {"type": "text", "text": "synthetic current screen"},
                        {"type": "image", "mimeType": "image/png", "data": _PNG},
                    ],
                },
            }
        if name == "interactive_session.install_build_from_run":
            self.install_started.set()
            if not self.release_install.wait(timeout=15):
                return {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {"isError": True, "content": [{"type": "text", "text": "synthetic install gate timed out"}]},
                }
            return {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "structuredContent": {"status": "completed", "artifact_name": arguments.get("artifact_name")},
                    "content": [{"type": "text", "text": "synthetic install completed"}],
                },
            }
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "structuredContent": {"status": "completed"},
                "content": [{"type": "text", "text": "synthetic action completed"}],
            },
        }


def _safe_environment(package: SmokePackage, home: Path) -> dict[str, str]:
    environment = {**package.environment, "CODEX_HOME": str(home)}
    for key in list(environment):
        upper = key.upper()
        if any(marker in upper for marker in ("API_KEY", "TOKEN", "CLIENT_SECRET", "CREDENTIAL", "OPENAI_BASE_URL", "CODEX_BASE_URL")):
            environment.pop(key, None)
    return environment


def _run_tool_turn(
    tui: PackagedTui,
    server: MockResponsesServer,
    marker: str,
    call_id: str,
    arguments: dict[str, Any],
    provider: _FakeAndroidProvider,
    phase: str,
) -> tuple[CapturedResponsesRequest, dict[str, Any]]:
    provider.set_phase(phase)
    _queue_tui_turn(server, marker, call_id, "android_step", arguments)
    tui.submit_line(marker, pasted_marker=marker)
    request = server.wait_for_request(
        lambda candidate: candidate.path == "/v1/responses"
        and _contains_marker(candidate, marker),
        timeout_s=30,
    )
    thread_id = _thread_id(request)
    _require(
        isinstance(thread_id, str) and bool(thread_id),
        f"stage={phase} reason=thread-id-not-present",
    )
    tool_names = {
        name
        for item in request.body_json().get("tools", [])
        if isinstance(item, dict)
        for name in [item.get("name"), *[child.get("name") for child in item.get("tools", []) if isinstance(child, dict)]]
        if isinstance(name, str)
    }
    _require(
        "android_step" in tool_names and "android_install_build_from_run" in tool_names,
        f"stage={phase} reason=android-tools-not-advertised",
    )
    tui.until(f"SYNTHETIC_{call_id}_COMPLETED")
    output_request, output = _request_with_call_output(server, thread_id, call_id)
    _require(
        _thread_id(output_request) == thread_id,
        f"stage={phase} reason=thread-id-changed",
    )
    _require(
        _has_native_image(output.get("output")),
        f"stage={phase} reason=native-image-not-forwarded",
    )
    return output_request, output


def test_packaged_android_step_and_install_protocol(package: SmokePackage) -> None:
    home = package.directory / "android-protocol-home"
    home.mkdir()
    isolated = replace(
        package,
        environment=_safe_environment(package, home),
    )
    with _FakeAndroidProvider() as android, MockResponsesServer() as responses:
        config_path = home / "config.toml"
        config_path.write_text(
            'model = "package-smoke"\nmodel_provider = "package_smoke"\n'
            'approval_policy = "never"\nsandbox_mode = "workspace-write"\n'
            "[sandbox_workspace_write]\nnetwork_access = true\n"
            "[features]\ncode_mode_only = false\ncode_mode_host = true\n"
            "multi_agent_v2 = true\nmemories = false\napps = false\nplugins = false\n"
            "[model_providers.package_smoke]\nname = \"synthetic package test\"\n"
            f'base_url = "{responses.url}/v1"\nwire_api = "responses"\n'
            "request_max_retries = 0\nstream_max_retries = 0\n",
            encoding="utf-8",
        )
        (home / "android-computer-use.json").write_text(
            json.dumps({"mcp_url": android.url, "default_serial": "synthetic-serial-17"}),
            encoding="utf-8",
        )
        # Native provider specs are injected by the packaged TUI's normal
        # ThreadStart path; the Python SDK's convenience start method omits
        # dynamic_tools and is not a valid consumer for this contract.
        with PackagedTui(isolated) as tui:
            tui.until("Ask Codex to do anything")

            _, failure_output = _run_tool_turn(
                tui,
                responses,
                "ANDROID_STEP_FIRST_FAILURE_MARKER",
                "android-step-first-failure",
                {
                    "actions": [
                        {"type": "tap", "x": 8, "y": 13},
                        {"type": "type", "text": "SYNTHETIC_FAIL_TOKEN"},
                        {"type": "tap", "x": 90, "y": 91},
                    ]
                },
                android,
                "step-failure",
            )
            failure_calls = android.calls_for("step-failure")
            _require(
                [call["name"] for call in failure_calls]
                == ["android.input.tap", "android.input.text", "android.inspect_ui"],
                "stage=step-failure reason=ordered-first-failure-stop-or-fresh-observe",
            )
            _require(
                failure_calls[0]["arguments"]
                == {"serial": "synthetic-serial-17", "x": 8, "y": 13},
                "stage=step-failure reason=tap-argument-mapping",
            )
            _require(
                failure_calls[1]["arguments"].get("text") == "SYNTHETIC_FAIL_TOKEN"
                and failure_calls[1]["arguments"].get("serial") == "synthetic-serial-17",
                "stage=step-failure reason=text-argument-mapping",
            )
            _require(
                failure_calls[2]["arguments"].get("include_screenshot") is True
                and failure_calls[2]["arguments"].get("serial") == "synthetic-serial-17",
                "stage=step-failure reason=fresh-observation-arguments",
            )
            failure_text = "\n".join(_text_parts(failure_output.get("output")))
            _require(
                "SYNTHETIC_ACTION_FAILURE" in failure_text
                and "Actions completed before failure:" in failure_text
                and "tapped at 8,13" in failure_text,
                "stage=step-failure reason=completed-action-summary",
            )
            _require("SCREEN_STEP_FAILURE" in failure_text, "stage=step-failure reason=post-failure-screen-not-returned")

            _, transport_output = _run_tool_turn(
                tui,
                responses,
                "ANDROID_STEP_TRANSPORT_LOSS_MARKER",
                "android-step-transport-loss",
                {"type": "tap", "x": 21, "y": 34},
                android,
                "step-transport-loss",
            )
            transport_calls = android.calls_for("step-transport-loss")
            _require(
                [call["name"] for call in transport_calls]
                == ["android.input.tap", "android.inspect_ui"],
                "stage=step-transport-loss reason=mutation-replayed-or-observation-missing",
            )
            _require(
                "SCREEN_STEP_TRANSPORT_LOSS" in "\n".join(_text_parts(transport_output.get("output"))),
                "stage=step-transport-loss reason=fresh-observation-not-returned",
            )

            _, success_output = _run_tool_turn(
                tui,
                responses,
                "ANDROID_STEP_SUCCESS_MARKER",
                "android-step-success",
                {"type": "tap", "x": 5, "y": 6},
                android,
                "step-success",
            )
            success_calls = android.calls_for("step-success")
            _require(
                [call["name"] for call in success_calls]
                == ["android.input.tap", "android.inspect_ui"],
                "stage=step-success reason=post-action-observation-order",
            )
            _require(
                "SCREEN_STEP_SUCCESS" in "\n".join(_text_parts(success_output.get("output"))),
                "stage=step-success reason=fresh-observation-not-returned",
            )

            android.set_phase("install")
            install_args = {
                "workflow_run_id": 38812124082,
                "artifact_name": "synthetic-android-build",
                "launch_after_install": False,
            }
            _queue_tui_turn(
                responses,
                "ANDROID_INSTALL_COMPLETION_MARKER",
                "android-install-completion",
                "android_install_build_from_run",
                install_args,
            )
            tui.submit_line(
                "ANDROID_INSTALL_COMPLETION_MARKER",
                pasted_marker="ANDROID_INSTALL_COMPLETION_MARKER",
            )
            _require(
                android.install_started.wait(timeout=8),
                "stage=install reason=provider-call-not-started",
            )
            _require(
                not any(
                    _has_call_output(request, "android-install-completion")
                    for request in responses.requests()
                ),
                "stage=install reason=returned-before-provider-completion",
            )
            android.release_install.set()
            tui.until("SYNTHETIC_android-install-completion_COMPLETED")
            install_request = responses.wait_for_request(
                lambda candidate: candidate.path == "/v1/responses"
                and _thread_id(candidate) is not None
                and _contains_marker(candidate, "ANDROID_INSTALL_COMPLETION_MARKER")
            )
            install_tool_names = {
                name
                for item in install_request.body_json().get("tools", [])
                if isinstance(item, dict)
                for name in [item.get("name"), *[child.get("name") for child in item.get("tools", []) if isinstance(child, dict)]]
                if isinstance(name, str)
            }
            _require("android_install_build_from_run" in install_tool_names, "stage=install reason=install-tool-not-advertised")
            install_thread_id = _thread_id(install_request)
            _require(
                isinstance(install_thread_id, str) and bool(install_thread_id),
                "stage=install reason=thread-id-not-present",
            )
            install_output_request, install_output = _request_with_call_output(
                responses, install_thread_id, "android-install-completion"
            )
            _require(
                _thread_id(install_output_request) == install_thread_id,
                "stage=install reason=thread-id-changed",
            )
            _require(_has_native_image(install_output.get("output")), "stage=install reason=native-image-not-forwarded")
            install_calls = android.calls_for("install")
            _require(
                [call["name"] for call in install_calls]
                == ["interactive_session.install_build_from_run", "android.inspect_ui"],
                "stage=install reason=completion-or-post-install-observation-order",
            )
            _require(
                install_calls[0]["arguments"]
                == {
                    "workflow_run_id": 38812124082,
                    "artifact_name": "synthetic-android-build",
                    "launch_after_install": False,
                    "serial": "synthetic-serial-17",
                },
                "stage=install reason=provider-argument-mapping",
            )
            _require(
                install_calls[1]["arguments"].get("serial") == "synthetic-serial-17"
                and install_calls[1]["arguments"].get("include_screenshot") is True,
                "stage=install reason=post-install-observation-arguments",
            )
            _require(
                "SCREEN_INSTALL" in "\n".join(_text_parts(install_output.get("output"))),
                "stage=install reason=fresh-post-install-screen-not-returned",
            )
            _require(not responses.routing_errors(), "stage=responses reason=mock-route-selection-error")
