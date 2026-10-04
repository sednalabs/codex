"""Prove packaged weekly pacing uses isolated account usage, not sparse headers."""

import base64
import json
import os
import queue
import threading
import time
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

import pytest
from app_server_harness import ev_assistant_message, ev_completed, ev_response_created, sse
from fixtures import SmokePackage
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox
from tui_pty import PackagedTui


_ACCOUNT_ID = "synthetic-pacing-account"
_USER_ID = "synthetic-pacing-user"
_ACCESS_TOKEN = "synthetic-pacing-access-token"
_WEEK_SECONDS = 7 * 24 * 60 * 60


@dataclass(frozen=True)
class _Response:
    response_id: str
    text: str
    headers: dict[str, str]


class _PacingHttpServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, backend: "_PacingBackend") -> None:
        super().__init__(("127.0.0.1", 0), _PacingHandler)
        self.backend = backend


class _PacingHandler(BaseHTTPRequestHandler):
    server: _PacingHttpServer

    def log_message(self, _format: str, *_args: object) -> None:
        return None

    def do_GET(self) -> None:
        backend = self.server.backend
        backend._record_request(self.command, self.path, self.headers)
        if not backend._is_local_host(self.headers.get("host", "")):
            self._reject("non-loopback Host header")
        elif self.path == "/api/codex/accounts/check":
            self._send_json({
                "accounts": [{
                    "id": _ACCOUNT_ID,
                    "workspace_backend_origin": "https://127.0.0.1",
                    "account_routing_override": "NO_CONSTRAINT",
                }],
            })
        elif self.path == "/api/codex/usage":
            self._send_json(backend._usage_payload())
        elif self.path == "/api/codex/rate-limit-reset-credits":
            self._send_json({"credits": [], "available_count": 0, "total_earned_count": 0})
        elif self.path == "/v1/models":
            self._send_json({
                "object": "list",
                "data": [{"id": "mock-model", "object": "model", "created": 0,
                          "owned_by": "openai"}],
            })
        else:
            self._reject(f"unexpected GET {self.path}")

    def do_POST(self) -> None:
        backend = self.server.backend
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length)
        backend._record_response_request(self.path, self.headers, body)
        if not backend._is_local_host(self.headers.get("host", "")):
            self._reject("non-loopback Host header")
            return
        if self.path != "/v1/responses":
            self._reject(f"unexpected POST {self.path}")
            return
        try:
            response = backend._responses.get_nowait()
        except queue.Empty:
            self._reject("no fixture response was queued")
            return
        payload = sse([
            ev_response_created(response.response_id),
            ev_assistant_message(f"message-{response.response_id}", response.text),
            ev_completed(response.response_id),
        ]).encode("utf-8")
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(payload)))
        for name, value in response.headers.items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(payload)
        self.wfile.flush()
        backend._record_response_delivery(response)

    def do_CONNECT(self) -> None:
        self._reject(f"unexpected external proxy tunnel {self.path}")

    def _send_json(self, payload: dict[str, Any]) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _reject(self, reason: str) -> None:
        self.server.backend._record_unexpected(reason)
        body = b"unexpected request to the isolated pacing fixture"
        self.send_response(502)
        self.send_header("content-type", "text/plain")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class _PacingBackend:
    """One test-local loopback producer; every served route is explicit."""

    def __init__(self, *, used_percent: int, time_remaining_percent: int) -> None:
        self.used_percent = used_percent
        self.time_remaining_percent = time_remaining_percent
        self._responses: queue.Queue[_Response] = queue.Queue()
        self._lock = threading.Lock()
        self._requests: list[dict[str, Any]] = []
        self._response_requests: list[dict[str, Any]] = []
        self._delivered_responses: list[dict[str, Any]] = []
        self._unexpected: list[str] = []
        self._server = _PacingHttpServer(self)
        self._thread = threading.Thread(
            target=self._server.serve_forever,
            name="packaged-pacing-fixture",
            daemon=True,
        )

    def __enter__(self) -> "_PacingBackend":
        self._thread.start()
        return self

    def __exit__(self, _kind: object, _error: object, _traceback: object) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=2)

    @property
    def url(self) -> str:
        host, port = self._server.server_address
        return f"http://{host}:{port}"

    def enqueue_assistant_message(
        self,
        text: str,
        *,
        response_id: str,
        headers: dict[str, str] | None = None,
    ) -> None:
        self._responses.put(_Response(response_id, text, headers or {}))

    def requests(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self._requests)

    def response_requests(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self._response_requests)

    def delivered_responses(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self._delivered_responses)

    def assert_no_unexpected_requests(self) -> None:
        with self._lock:
            unexpected = list(self._unexpected)
        assert not unexpected, unexpected

    def _is_local_host(self, host: str) -> bool:
        return host == f"127.0.0.1:{self._server.server_address[1]}"

    def _record_request(self, method: str, path: str, headers: Any) -> None:
        with self._lock:
            self._requests.append({
                "method": method,
                "path": path,
                "authorization_matches": headers.get("authorization")
                == f"Bearer {_ACCESS_TOKEN}",
                "account_id": headers.get("chatgpt-account-id"),
            })

    def _record_response_request(self, path: str, headers: Any, body: bytes) -> None:
        try:
            request_body = json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            request_body = {}
        with self._lock:
            self._response_requests.append({
                "path": path,
                "authorization_matches": headers.get("authorization")
                == f"Bearer {_ACCESS_TOKEN}",
                "body": request_body,
            })

    def _record_unexpected(self, reason: str) -> None:
        with self._lock:
            self._unexpected.append(reason)

    def _record_response_delivery(self, response: _Response) -> None:
        with self._lock:
            self._delivered_responses.append({
                "response_id": response.response_id,
                "headers": dict(response.headers),
            })

    def _usage_payload(self) -> dict[str, Any]:
        reset_after = round(_WEEK_SECONDS * self.time_remaining_percent / 100)
        reset_at = round(time.time()) + reset_after
        return {
            "account_id": _ACCOUNT_ID,
            "user_id": _USER_ID,
            "plan_type": "pro",
            "rate_limit": {
                "allowed": True,
                "limit_reached": False,
                "primary_window": {
                    "used_percent": self.used_percent,
                    "limit_window_seconds": _WEEK_SECONDS,
                    "reset_after_seconds": reset_after,
                    "reset_at": reset_at,
                },
            },
            "rate_limit_reset_credits": {"available_count": 0},
        }


def _jwt(claims: dict[str, Any]) -> str:
    def encode(value: dict[str, Any]) -> str:
        raw = json.dumps(value, separators=(",", ":")).encode("utf-8")
        return base64.urlsafe_b64encode(raw).decode("ascii").rstrip("=")

    return f"{encode({'alg': 'none', 'typ': 'JWT'})}.{encode(claims)}.synthetic-signature"


def _isolated_package(
    package: SmokePackage,
    suffix: str,
    backend: _PacingBackend,
    *,
    pacing_style: str | None,
) -> tuple[SmokePackage, Path]:
    home = package.directory / suffix
    home.mkdir(mode=0o700)
    no_proxy = "127.0.0.1,localhost"
    # Deliberately allow only runtime essentials. If an accidental external
    # HTTP(S) request is made, route it to this fail-closed fixture as a proxy.
    environment = {
        "PATH": package.environment["PATH"],
        "HOME": str(home),
        "CODEX_HOME": str(home),
        "TMPDIR": str(package.directory),
        "ZDOTDIR": str(home),
        "NO_PROXY": no_proxy,
        "no_proxy": no_proxy,
        "HTTP_PROXY": backend.url,
        "http_proxy": backend.url,
        "HTTPS_PROXY": backend.url,
        "https_proxy": backend.url,
        "ALL_PROXY": backend.url,
        "all_proxy": backend.url,
    }
    isolated = replace(package, environment=environment)
    pacing = (
        f'weekly_limit_pacing_style = "{pacing_style}"\n'
        if pacing_style is not None else ""
    )
    # The real TUI prefetch gate needs both ChatGPT auth and this custom-provider
    # requirement; otherwise startup reports no account usage to the status line.
    (home / "config.toml").write_text(
        'model = "mock-model"\n'
        'model_provider = "pacing_smoke"\n'
        f'chatgpt_base_url = "{backend.url}"\n'
        'cli_auth_credentials_store = "file"\n'
        'approval_policy = "never"\n'
        'sandbox_mode = "workspace-write"\n'
        'suppress_unstable_features_warning = true\n'
        '[analytics]\nenabled = false\n'
        '[otel]\nexporter = "none"\ntrace_exporter = "none"\nmetrics_exporter = "none"\n'
        '[sandbox_workspace_write]\nnetwork_access = true\n'
        '[features]\ncode_mode_only = true\ncode_mode_host = true\n'
        'multi_agent_v2 = false\nmemories = false\napps = false\nplugins = false\n'
        '[tui]\nstatus_line = ["weekly-limit"]\n'
        f"{pacing}"
        '[model_providers.pacing_smoke]\n'
        'name = "synthetic pacing provider"\n'
        f'base_url = "{backend.url}/v1"\n'
        'wire_api = "responses"\n'
        'requires_openai_auth = true\n'
        'request_max_retries = 0\nstream_max_retries = 0\n',
        encoding="utf-8",
    )
    id_token = _jwt({
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_user_id": _USER_ID,
            "chatgpt_account_id": _ACCOUNT_ID,
        },
    })
    auth = {
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": _ACCESS_TOKEN,
            "refresh_token": "synthetic-pacing-refresh-token",
            "account_id": _ACCOUNT_ID,
        },
        "last_refresh": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    }
    auth_path = home / "auth.json"
    fd = os.open(auth_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as auth_file:
        json.dump(auth, auth_file)
    return isolated, home


def _sdk(package: SmokePackage) -> Codex:
    return Codex(config=CodexConfig(
        codex_bin=str(package.cli),
        cwd=str(package.directory),
        env=package.environment,
    ))


@pytest.mark.parametrize(
    ("pacing_style", "used_percent", "time_remaining_percent", "expected", "sparse"),
    [
        (None, 56, 50, "weekly 44% left (over 6%)", "weekly 5% left (over 51%)"),
        ("ratio", 40, 56, "weekly 60%/56%", "weekly 5%/56%"),
    ],
)
def test_packaged_weekly_pacing_uses_account_usage_across_sparse_update_and_resume(
    package: SmokePackage,
    pacing_style: str | None,
    used_percent: int,
    time_remaining_percent: int,
    expected: str,
    sparse: str,
) -> None:
    suffix = f"tui-pacing-{pacing_style or 'qualitative-default'}"
    with _PacingBackend(
        used_percent=used_percent,
        time_remaining_percent=time_remaining_percent,
    ) as backend:
        isolated, home = _isolated_package(
            package, suffix, backend, pacing_style=pacing_style,
        )
        backend.enqueue_assistant_message(
            "pacing seed complete", response_id="pacing-seed",
        )
        with _sdk(isolated) as client:
            thread = client.thread_start(
                ephemeral=False,
                approval_mode=ApprovalMode.deny_all,
                sandbox=Sandbox.workspace_write,
            )
            result = thread.run("Create a synthetic pacing thread.")
            assert result.final_response == "pacing seed complete"
            thread_id = thread.id

        sparse_reset_at = round(time.time() + _WEEK_SECONDS * 56 / 100)
        backend.enqueue_assistant_message(
            "sparse response complete",
            response_id="pacing-sparse-update",
            headers={
                "x-codex-primary-used-percent": "95",
                "x-codex-primary-window-minutes": str(_WEEK_SECONDS // 60),
                "x-codex-primary-reset-at": str(sparse_reset_at),
            },
        )
        with PackagedTui(isolated, "resume", thread_id) as tui:
            tui.until("Ask Codex to do anything")
            initial = tui.until_screen(
                expected,
                required_markers=("Ask Codex to do anything",),
            )
            assert expected in initial, initial
            tui.send("Verify the sparse weekly-limit update remains separate.")
            tui.send("\r")
            after_response = tui.until_screen(
                "sparse response complete",
                required_markers=(expected,),
            )
            assert expected in after_response, after_response
            assert sparse not in after_response, after_response

        with PackagedTui(isolated, "resume", thread_id) as replay:
            replayed = replay.until_screen(
                "sparse response complete",
                required_markers=(expected,),
            )
            assert expected in replayed, replayed
            assert sparse not in replayed, replayed

        requests = backend.requests()
        usage = [request for request in requests if request["path"] == "/api/codex/usage"]
        credits = [
            request for request in requests
            if request["path"] == "/api/codex/rate-limit-reset-credits"
        ]
        account_checks = [
            request for request in requests
            if request["path"] == "/api/codex/accounts/check"
        ]
        assert usage and all(request["authorization_matches"] for request in usage), requests
        assert all(request["account_id"] == _ACCOUNT_ID for request in usage), requests
        assert credits and account_checks, requests
        assert all(request["authorization_matches"] for request in account_checks), requests
        responses = backend.response_requests()
        assert len(responses) == 2, responses
        assert all(request["path"] == "/v1/responses" for request in responses), responses
        assert all(request["authorization_matches"] for request in responses), responses
        assert any(
            "Verify the sparse weekly-limit update remains separate."
            in json.dumps(request["body"])
            for request in responses
        ), responses
        assert backend.delivered_responses() == [
            {"response_id": "pacing-seed", "headers": {}},
            {
                "response_id": "pacing-sparse-update",
                "headers": {
                    "x-codex-primary-used-percent": "95",
                    "x-codex-primary-window-minutes": str(_WEEK_SECONDS // 60),
                    "x-codex-primary-reset-at": str(sparse_reset_at),
                },
            },
        ]
        backend.assert_no_unexpected_requests()
