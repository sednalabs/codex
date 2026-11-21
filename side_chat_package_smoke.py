"""Isolated ordinary use of one installed Codex package with a loopback model."""

import http.server
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
from types import SimpleNamespace

fixture = pathlib.Path(__file__).parent / "scripts/codex_package/smoke_tests/first_binary"
sys.path.insert(0, str(fixture))
sys.modules["fixtures"] = SimpleNamespace(SmokePackage=SimpleNamespace)
from tui_pty import PackagedTui

binary = pathlib.Path(sys.argv[1]).resolve(strict=True)
original = "Original synthetic Side Chat guidance."
edited = "Edited synthetic Side Chat guidance."
side_opening = "SIDE_GUIDANCE_PROBE"
side_followup = "SIDE_GUIDANCE_FOLLOWUP_PROBE"
parent_followup = "PARENT_AFTER_SIDE_PROBE"
requests = []


class Responses(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.send_error(404)

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        if not self.path.endswith("/responses") or not 0 < length <= 2**21:
            self.send_error(400)
            return
        payload = json.loads(self.rfile.read(length))
        items = payload.get("input", [])
        developer = json.dumps(
            {
                "instructions": payload.get("instructions"),
                "developer_items": [item for item in items if item.get("role") == "developer"],
            }
        )
        serialized = json.dumps(payload)
        last_user_text = next(
            (
                " ".join(
                    part.get("text", "")
                    for part in item.get("content", [])
                    if isinstance(part, dict)
                )
                for item in reversed(items)
                if item.get("role") == "user"
            ),
            "",
        )
        side_context = side_opening in serialized or side_followup in serialized
        side_title_request = last_user_text.startswith(
            "Generate a concise, single-line task title"
        )
        side = not side_title_request and (
            side_opening in last_user_text or side_followup in last_user_text
        )
        if side_title_request and side_context:
            phase = "side-title"
            text = '{"title":"Side discussion"}'
        elif side_followup in last_user_text:
            phase = "side-followup"
            text = "SIDE_POLICY_FOLLOWUP_OK"
        elif side_opening in last_user_text:
            phase = "side-opening"
            text = "SIDE_POLICY_OK"
        elif parent_followup in last_user_text:
            phase = "parent-followup"
            text = "PARENT_FOLLOWUP_OK"
        elif side_context:
            phase = "side-auxiliary"
            text = "SIDE_AUXILIARY_OK"
        else:
            phase = "parent-seed"
            text = "PARENT_SEED_OK"
        requests.append(
            {
                "phase": phase,
                "side_request": side,
                "side_context": side_context,
                "edited_count": developer.count(edited),
                "original_present": original in developer,
                "side_policy": "You are in a side conversation" in developer,
            }
        )
        response_id = "resp-side-smoke-" + phase
        item = {
            "type": "message",
            "id": "msg-side-smoke-" + phase,
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }
        events = [
            {"type": "response.created", "response": {"id": response_id}},
            {"type": "response.output_item.done", "item": item},
            {
                "type": "response.completed",
                "response": {
                    "id": response_id,
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                },
            },
        ]
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for event in events:
            self.wfile.write(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n".encode())


with tempfile.TemporaryDirectory(prefix="side-chat-exact-package-") as directory:
    root = pathlib.Path(directory)
    home = root / "home"
    home.mkdir(mode=0o700)
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Responses) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            (home / "config.toml").write_text(
                'developer_instructions = "' + original + '"\n'
                'model = "package-smoke"\nmodel_provider = "package_smoke"\n'
                'approval_policy = "never"\nsandbox_mode = "read-only"\n'
                'web_search = "disabled"\n'
                f'[projects."{root}"]\ntrust_level = "trusted"\n'
                '[features]\ncode_mode_only = false\nmemories = false\napps = false\nplugins = false\n'
                '[model_providers.package_smoke]\nname = "Package smoke"\n'
                f'base_url = "http://127.0.0.1:{server.server_port}/v1"\n'
                'wire_api = "responses"\nrequest_max_retries = 0\nstream_max_retries = 0\n'
                '[[instruction_overlays.developer_guidance.edits]]\nop = "replace_exact"\n'
                f'from = "{original}"\nto = "{edited}"\nexpected_matches = 1\n'
            )
            env = {
                "HOME": str(home),
                "CODEX_HOME": str(home),
                "CODEX_SQLITE_HOME": str(home),
                "PATH": "/usr/bin:/bin",
                "RUST_LOG": "trace",
                "LANG": "C.UTF-8",
            }
            common = ["--no-daemon", "-c", f"log_dir={root / 'logs'}"]
            result = subprocess.run(
                [str(binary), *common, "exec", "--json", "--skip-git-repo-check", "PARENT_SEED"],
                cwd=root, env=env, capture_output=True, text=True, timeout=45, check=False,
            )
            assert result.returncode == 0, f"synthetic parent exit {result.returncode}"
            started = next(
                item for item in map(json.loads, result.stdout.splitlines())
                if item.get("type") == "thread.started"
            )
            package = SimpleNamespace(cli=binary, directory=root, environment=env)
            with PackagedTui(package, *common, "resume", started["thread_id"]) as tui:
                tui.until("PARENT_SEED_OK", timeout=25)
                tui.submit_line(f"/side {side_opening}", pasted_marker=side_opening)
                try:
                    tui.until("SIDE_POLICY_OK", timeout=35)
                except AssertionError as error:
                    print(json.dumps({"rendered_side_result": False, "requests": requests}))
                    raise AssertionError("Side Chat did not complete the isolated turn") from error
                tui.submit_line(side_followup, pasted_marker=side_followup)
                tui.until("SIDE_POLICY_FOLLOWUP_OK", timeout=35)
                tui.send("\x03")
                tui.until("PARENT_SEED_OK", timeout=15)
                tui.submit_line(parent_followup, pasted_marker=parent_followup)
                tui.until("PARENT_FOLLOWUP_OK", timeout=35)
            expected = [
                {
                    "phase": "parent-seed",
                    "side_request": False,
                    "side_context": False,
                    "edited_count": 1,
                    "original_present": False,
                    "side_policy": False,
                },
                {
                    "phase": "side-opening",
                    "side_request": True,
                    "side_context": True,
                    "edited_count": 1,
                    "original_present": False,
                    "side_policy": True,
                },
                # The TUI's hidden structured title thread is metadata, not a Side turn.
                {
                    "phase": "side-title",
                    "side_request": False,
                    "side_context": True,
                    "edited_count": 1,
                    "original_present": False,
                    "side_policy": False,
                },
                {
                    "phase": "side-followup",
                    "side_request": True,
                    "side_context": True,
                    "edited_count": 1,
                    "original_present": False,
                    "side_policy": True,
                },
                {
                    "phase": "parent-followup",
                    "side_request": False,
                    "side_context": False,
                    "edited_count": 1,
                    "original_present": False,
                    "side_policy": False,
                },
            ]
            print(json.dumps({"rendered_side_result": True, "requests": requests}))
            assert requests == expected
        finally:
            server.shutdown()
            thread.join(timeout=3)
