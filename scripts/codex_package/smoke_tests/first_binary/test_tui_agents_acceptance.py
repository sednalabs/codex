"""Exercise the actual packaged TUI /agents view through a real Linux PTY."""

import base64
import hashlib
import json
import struct
import sys
import zlib
from dataclasses import replace
from pathlib import Path

from app_server_harness import (
    MockResponsesServer,
    ev_completed,
    ev_response_created,
    sse,
)
import pytest
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage
from package_acceptance import _mock_config
from tui_pty import PackagedTui


def _png(red: int, green: int, blue: int) -> bytes:
    def chunk(kind: bytes, content: bytes) -> bytes:
        return (
            struct.pack(">I", len(content))
            + kind
            + content
            + struct.pack(">I", zlib.crc32(kind + content) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0)
    pixel = zlib.compress(bytes([0, red, green, blue, 255]))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", pixel)
        + chunk(b"IEND", b"")
    )


_SYNTHETIC_BROWSER_PROVIDER = r'''import base64
import json
import os
import sys
from pathlib import Path

home = Path(os.environ["CODEX_HOME"])
provider_observation = home / "browser-provider-invocation.json"
provider_observation.write_text(
    json.dumps({"invoked": True, "tool_name": "<unknown>"}),
    encoding="utf-8",
)
request = json.load(sys.stdin)
tool_name = request.get("tool")
if not isinstance(tool_name, str) or tool_name not in {"browser_observe", "browser_step"}:
    tool_name = "<other>"
provider_observation.write_text(
    json.dumps({"invoked": True, "tool_name": tool_name}),
    encoding="utf-8",
)
fixture_dir = home / "browser-fixture"
manifest_path = fixture_dir / "manifest.json"
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
arguments = request.get("arguments", {})
captures = arguments.get("captures", [])
if (
    request.get("namespace") != "codex_browser"
    or request.get("tool") != "browser_observe"
    or [capture.get("label") for capture in captures] != ["top", "bottom"]
    or arguments.get("save_artifact") is not True
):
    raise SystemExit("unexpected synthetic Browser request")

(fixture_dir / "provider-request.json").write_text(
    json.dumps({
        "namespace": request["namespace"],
        "tool": request["tool"],
        "arguments": arguments,
    }, sort_keys=True),
    encoding="utf-8",
)
capture_lines = [
    "capture: " + json.dumps(
        {"order": capture["order"], "label": capture["label"], "metadata": capture["metadata"]},
        sort_keys=True,
    )
    for capture in manifest["captures"]
]
text = "\n".join([
    *capture_lines,
    "restoration: " + json.dumps(manifest["restoration"], sort_keys=True),
    "artifact_manifest: " + str(manifest_path.relative_to(home)),
])
content = [{"type": "inputText", "text": text}]
for capture in manifest["captures"]:
    image = (fixture_dir / capture["path"]).read_bytes()
    content.append({
        "type": "inputImage",
        "imageUrl": "data:image/png;base64," + base64.b64encode(image).decode("ascii"),
    })
json.dump({"success": True, "contentItems": content}, sys.stdout)
'''


_BROWSER_FIXTURE_CALL_ID = "browser-visual-fixture-call"
_BROWSER_TOOL_NAMES = {"browser_observe", "browser_step"}
_BROWSER_SOURCE_STAGE_FIELDS = {
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
}


@pytest.fixture
def browser_output_diagnostic(record_property):
    """Attach a value-free round-trip snapshot to the always-uploaded JUnit."""
    state = {
        "fixture_function_call_emitted": False,
        "provider_observation_path": None,
        "stage_observation_path": None,
        "responses_server": None,
    }
    yield state

    server = state["responses_server"]
    requests = (
        [request for request in server.requests() if request.path == "/v1/responses"]
        if server is not None else []
    )
    second_request_input_available = False
    call_outputs = []
    if len(requests) >= 2:
        try:
            input_items = requests[1].body_json().get("input")
            if isinstance(input_items, list):
                second_request_input_available = True
                call_outputs = [
                    item for item in input_items
                    if isinstance(item, dict) and item.get("type") == "function_call_output"
                ]
        except (TypeError, ValueError):
            pass

    call_ids = []
    tool_names = []
    for item in call_outputs:
        call_id = item.get("call_id")
        call_ids.append(
            "<fixture>"
            if call_id == _BROWSER_FIXTURE_CALL_ID
            else "<other>" if isinstance(call_id, str) else "<missing>"
        )
        name = item.get("name")
        tool_names.append(
            name if isinstance(name, str) and name in _BROWSER_TOOL_NAMES
            else "<other>" if isinstance(name, str) else "<absent>"
        )

    provider_invoked = False
    provider_tool_name = None
    observation_path = state["provider_observation_path"]
    if observation_path is not None:
        try:
            observation = json.loads(observation_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            observation = {}
        provider_invoked = isinstance(observation, dict) and observation.get("invoked") is True
        observed_name = observation.get("tool_name") if isinstance(observation, dict) else None
        if provider_invoked:
            provider_tool_name = (
                observed_name
                if isinstance(observed_name, str) and observed_name in _BROWSER_TOOL_NAMES
                else "<other>"
            )

    source_stage_observations = []
    stage_observation_path = state["stage_observation_path"]
    try:
        stage_lines = (
            stage_observation_path.read_text(encoding="utf-8").splitlines()
            if isinstance(stage_observation_path, Path) else []
        )
    except OSError:
        stage_lines = []
    for line in stage_lines:
        try:
            observation = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(observation, dict):
            continue
        stage = observation.get("stage")
        if not isinstance(stage, str):
            continue
        expected_fields = _BROWSER_SOURCE_STAGE_FIELDS.get(stage)
        if expected_fields is None or set(observation) != expected_fields | {"stage"}:
            continue
        scalar_values = {
            key: value for key, value in observation.items() if key != "stage"
        }
        if type(scalar_values.get("call_id_matches_fixture")) is not bool:
            continue
        result_keys = expected_fields - {
            "call_id_matches_fixture",
            next(
                key for key in expected_fields
                if key.endswith("_item_count")
            ),
        }
        if any(type(scalar_values.get(key)) is not bool for key in result_keys):
            continue
        count_key = next(key for key in expected_fields if key.endswith("_item_count"))
        if type(scalar_values.get(count_key)) is not int or scalar_values[count_key] < 0:
            continue
        source_stage_observations.append({
            "stage": stage,
            **{key: scalar_values[key] for key in sorted(expected_fields)},
        })
        if len(source_stage_observations) >= 32:
            break

    diagnostic = {
        "fixture_function_call_emitted": state["fixture_function_call_emitted"],
        "responses_request_count": len(requests),
        "second_request_observed": len(requests) >= 2,
        "second_request_input_available": second_request_input_available,
        "function_call_output_count": len(call_outputs),
        "function_call_output_call_ids": call_ids,
        "function_call_output_tool_names": tool_names,
        "synthetic_browser_provider_invoked": provider_invoked,
        "synthetic_browser_provider_tool_name": provider_tool_name,
        "source_stage_observations": source_stage_observations,
    }
    record_property(
        "browser_output_diagnostic_json",
        json.dumps(diagnostic, sort_keys=True, separators=(",", ":")),
    )


def _isolated(package: SmokePackage, suffix: str) -> tuple[SmokePackage, Path]:
    home = package.directory / suffix
    home.mkdir()
    return replace(
        package, environment={**package.environment, "CODEX_HOME": str(home)}
    ), home


def _sdk(package: SmokePackage) -> Codex:
    return Codex(config=CodexConfig(
        codex_bin=str(package.cli), cwd=str(package.directory), env=package.environment,
    ))


def test_actual_tui_agents_entry_has_initial_empty_search(package: SmokePackage) -> None:
    isolated, home = _isolated(package, "tui-empty-search")
    with MockResponsesServer() as server:
        _mock_config(home, server, agent_tools=True)
        with PackagedTui(isolated) as tui:
            tui.send("/agents\n")
            opened = tui.until("Agent command center")
            assert "Group:" in opened
            tui.send("f")
            search = tui.until("Search ›")
            assert "Search ›" in search and "Agent command center" in search


def test_actual_tui_nested_filter_clear_live_rename_and_replay(
    package: SmokePackage,
) -> None:
    isolated, home = _isolated(package, "tui-tree-replay")
    with MockResponsesServer() as server:
        _mock_config(home, server, agent_tools=True)
        for index, message in enumerate(("root done", "child done", "nested done")):
            server.enqueue_assistant_message(message, response_id=f"tui-seed-{index}")
        with _sdk(isolated) as client:
            root = client.thread_start(
                ephemeral=False, approval_mode=ApprovalMode.deny_all,
                sandbox=Sandbox.workspace_write,
            )
            root.set_name("root-package-task")
            assert root.run("Complete root seed.").final_response == "root done"
            child = client.thread_fork(root.id, ephemeral=False)
            child.set_name("child-package-task")
            assert child.run("Complete child seed.").final_response == "child done"
            nested = client.thread_fork(child.id, ephemeral=False)
            nested.set_name("nested-package-task")
            assert nested.run("Complete nested seed.").final_response == "nested done"
            root_id, child_id, nested_id = root.id, child.id, nested.id

        # A second packaged process must reopen the exact child identity.
        with _sdk(isolated) as client:
            assert client.thread_resume(child_id).id == child_id
            assert client.thread_resume(root_id).id == root_id

        with PackagedTui(isolated, "resume", root_id) as tui:
            tui.send("/agents\n")
            tree = tui.until("nested-package-task")
            assert "Agent command center" in tree
            assert "root-package-task" in tree and "child-package-task" in tree
            tui.send("f")
            assert "Search ›" in tui.until("Search ›")
            tui.send("child-package-task")
            child_detail = tui.until(f"Thread ID: {child_id}")
            assert f"Parent thread ID: {root_id}" in child_detail
            tui.send("\x03")
            assert "nested-package-task" in tui.until("nested-package-task")
            tui.send("f")
            assert "Search ›" in tui.until("Search ›")
            tui.send("nested-package-task")
            nested_detail = tui.until(f"Thread ID: {nested_id}")
            assert f"Parent thread ID: {child_id}" in nested_detail
            tui.send("\x03")
            assert "root-package-task" in tui.until("root-package-task")
            tui.send("f")
            assert "Search ›" in tui.until("Search ›")
            tui.send("no-such-synthetic-task")
            assert "No matching tasks" in tui.until("No matching tasks")
            tui.send("\x03")
            assert "nested-package-task" in tui.until("nested-package-task")
            tui.send("r")
            assert "Rename ›" in tui.until("Rename ›")
            tui.send("\x7f" * 80 + "live-renamed-package-task\n")
            assert "live-renamed-package-task" in tui.until("live-renamed-package-task")

        with PackagedTui(isolated, "resume", root_id) as tui:
            tui.send("/agents\n")
            replay = tui.until("live-renamed-package-task")
            assert "Agent command center" in replay
            assert "root-package-task" in replay and "child-package-task" in replay


def test_packaged_tui_browser_output_keeps_images_and_manifest_metadata_separate(
    package: SmokePackage,
    browser_output_diagnostic,
) -> None:
    isolated, home = _isolated(package, "tui-browser-visual")
    browser_output_diagnostic["stage_observation_path"] = (
        home / "browser-output-stage-diagnostic.jsonl"
    )
    # The synthetic home config must be authoritative; inherited provider
    # variables can override it, including selecting a real Browser provider.
    isolated = replace(
        isolated,
        environment={
            key: value
            for key, value in isolated.environment.items()
            if not key.startswith("CODEX_BROWSER_")
        },
    )
    isolated.environment["CODEX_TEST_BROWSER_THREAD_START_OBSERVATION"] = "1"
    isolated.environment["CODEX_TEST_BROWSER_OUTPUT_DIAGNOSTIC"] = "1"
    isolated.environment["CODEX_TEST_BROWSER_OUTPUT_DIAGNOSTIC_CALL_ID"] = (
        _BROWSER_FIXTURE_CALL_ID
    )
    fixture_dir = home / "browser-fixture"
    fixture_dir.mkdir()
    image_bytes = [_png(220, 20, 20), _png(20, 20, 220)]
    captures = []
    for order, (label, filename, width, height, scroll_y, image) in enumerate(
        zip(
            ("top", "bottom"),
            ("top.png", "bottom.png"),
            (800, 640),
            (600, 480),
            (0, 1200),
            image_bytes,
            strict=True,
        ),
        start=1,
    ):
        (fixture_dir / filename).write_bytes(image)
        captures.append({
            "order": order,
            "label": label,
            "path": filename,
            "sha256": hashlib.sha256(image).hexdigest(),
            "metadata": {
                "effectiveViewport": {"width": width, "height": height},
                "devicePixelRatio": 1,
                "scroll": {"x": 0, "y": scroll_y},
            },
        })
    manifest = {
        "schemaVersion": 1,
        "captures": captures,
        "restoration": {
            "success": True,
            "actual": {"effectiveViewport": {"width": 1024, "height": 768}, "scroll": {"x": 0, "y": 0}},
            "expected": {"effectiveViewport": {"width": 1024, "height": 768}, "scroll": {"x": 0, "y": 0}},
        },
    }
    manifest_path = fixture_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, sort_keys=True), encoding="utf-8")
    provider_script = home / "synthetic_browser_provider.py"
    provider_script.write_text(_SYNTHETIC_BROWSER_PROVIDER, encoding="utf-8")
    (home / "browser-computer-use.json").write_text(
        json.dumps({"provider": "command", "command": [sys.executable, str(provider_script)]}),
        encoding="utf-8",
    )

    browser_output_diagnostic["provider_observation_path"] = (
        home / "browser-provider-invocation.json"
    )
    call_id = _BROWSER_FIXTURE_CALL_ID
    with MockResponsesServer() as server:
        browser_output_diagnostic["responses_server"] = server
        _mock_config(home, server, agent_tools=True)
        server.enqueue_sse(sse([
            ev_response_created("browser-visual-fixture-request"),
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "namespace": "codex_browser",
                    "call_id": call_id,
                    "name": "browser_observe",
                    "arguments": json.dumps({
                        "scope": "viewport_and_page",
                        "captures": [
                            {"label": "top", "viewportWidth": 800, "viewportHeight": 600, "scroll": "top"},
                            {"label": "bottom", "viewportWidth": 640, "viewportHeight": 480, "scroll": "bottom"},
                        ],
                        "save_artifact": True,
                    }),
                },
            },
            ev_completed("browser-visual-fixture-request"),
        ]))
        browser_output_diagnostic["fixture_function_call_emitted"] = True
        server.enqueue_assistant_message(
            "browser synthetic consumer complete",
            response_id="browser-visual-fixture-followup",
        )
        with PackagedTui(isolated) as tui:
            tui.until("Ask Codex to do anything")
            prompt = "Inspect the configured synthetic Browser page and report its capture labels."
            tui.send(prompt)
            tui.until(prompt)
            tui.send("\r")
            server.wait_for_requests(2, timeout_s=30)

        observation_path = home / "browser-thread-start-observation.jsonl"
        observations = [
            json.loads(line)
            for line in observation_path.read_text(encoding="utf-8").splitlines()
        ] if observation_path.exists() else []
        distinct_observations = {
            (
                observation.get("browser_namespace_count_before_start"),
                observation.get("fallback_stripped_browser_namespace"),
            )
            for observation in observations
        }
        assert len(distinct_observations) == 1, {
            "observation_count": len(observations),
            "distinct_observation_count": len(distinct_observations),
        }
        namespace_count_before_start, fallback_stripped_browser_namespace = next(
            iter(distinct_observations)
        )

        requests = [request for request in server.requests() if request.path == "/v1/responses"]
        assert len(requests) == 2, {"response_request_count": len(requests)}
        request_body = requests[0].body_json()
        advertised = request_body.get("tools")
        if not isinstance(advertised, list):
            advertised = []
        # Responses Lite places model-visible tools in an input item instead.
        input_items = request_body.get("input", [])
        if isinstance(input_items, list):
            for item in input_items:
                if isinstance(item, dict) and item.get("type") == "additional_tools":
                    additional_tools = item.get("tools", [])
                    if isinstance(additional_tools, list):
                        advertised.extend(additional_tools)
        browser_namespace = next(
            (
                tool for tool in advertised
                if tool.get("type") == "namespace" and tool.get("name") == "codex_browser"
            ),
            None,
        )
        assert browser_namespace is not None, {
            "browser_namespace_count_before_start": namespace_count_before_start,
            "fallback_stripped_browser_namespace": fallback_stripped_browser_namespace,
            "advertised_tool_count": len(advertised),
        }
        assert {tool.get("name") for tool in browser_namespace.get("tools", [])} >= {
            "browser_observe", "browser_step",
        }

        call_outputs = [
            item for request in requests for item in request.input()
            if item.get("type") == "function_call_output" and item.get("call_id") == call_id
        ]
        assert len(call_outputs) == 1, {"browser_call_output_count": len(call_outputs)}
        output = call_outputs[0].get("output")
        assert isinstance(output, list), {"typed_multimodal_output_is_list": isinstance(output, list)}
        output_types = [part.get("type") for part in output]
        assert output_types == ["input_text", "input_image", "input_image"], {
            "typed_output_item_count": len(output_types),
            "typed_input_image_count": output_types.count("input_image"),
        }
        text = output[0].get("text", "")
        recorded_request = json.loads((fixture_dir / "provider-request.json").read_text(encoding="utf-8"))
        assert recorded_request["namespace"] == "codex_browser"
        assert recorded_request["tool"] == "browser_observe"
        assert [capture["label"] for capture in recorded_request["arguments"]["captures"]] == [
            "top", "bottom",
        ]
        assert recorded_request["arguments"]["save_artifact"] is True

        assert Path(text.split("artifact_manifest: ", 1)[1].splitlines()[0]).name == manifest_path.name
        manifest_on_disk = json.loads(manifest_path.read_text(encoding="utf-8"))
        assert [capture["label"] for capture in manifest_on_disk["captures"]] == [
            "top", "bottom",
        ]
        expected_metadata = [
            {
                "effectiveViewport": {"width": 800, "height": 600},
                "devicePixelRatio": 1,
                "scroll": {"x": 0, "y": 0},
            },
            {
                "effectiveViewport": {"width": 640, "height": 480},
                "devicePixelRatio": 1,
                "scroll": {"x": 0, "y": 1200},
            },
        ]
        assert [capture["metadata"] for capture in manifest_on_disk["captures"]] == expected_metadata
        text_captures = [
            json.loads(line.partition("capture: ")[2])
            for line in text.splitlines()
            if line.startswith("capture: ")
        ]
        assert text_captures == [
            {
                "order": capture["order"],
                "label": capture["label"],
                "metadata": capture["metadata"],
            }
            for capture in manifest_on_disk["captures"]
        ]
        assert [capture["label"] for capture in text_captures] == ["top", "bottom"]
        assert [capture["metadata"] for capture in text_captures] == expected_metadata
        restoration = next(line for line in text.splitlines() if line.startswith("restoration: "))
        assert json.loads(restoration.partition("restoration: ")[2]) == manifest_on_disk["restoration"]
        assert "artifact_manifest: browser-fixture/manifest.json" in text
        assert "data:image/png;base64," not in text
        encoded_images = [part.get("image_url", "") for part in output[1:]]
        assert all(image.startswith("data:image/png;base64,") for image in encoded_images)
        model_images = [
            base64.b64decode(image.split(",", 1)[1], validate=True)
            for image in encoded_images
        ]
        assert model_images == image_bytes, {
            "model_image_count": len(model_images),
            "model_image_bytes_match_fixture": model_images == image_bytes,
        }
        for capture, image in zip(manifest_on_disk["captures"], model_images, strict=True):
            saved = (fixture_dir / capture["path"]).read_bytes()
            assert saved == image, {
                "capture_order": capture["order"],
                "saved_png_matches_typed_image": saved == image,
            }
            hash_matches = hashlib.sha256(saved).hexdigest() == capture["sha256"]
            assert hash_matches, {
                "capture_order": capture["order"],
                "saved_png_hash_matches_manifest": hash_matches,
            }
