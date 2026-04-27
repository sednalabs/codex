#!/usr/bin/env python3
"""Small GitHub webhook receiver with signature verification and routed commands."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import hmac
import http.server
import json
import os
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


JsonObject = dict[str, Any]


class WebhookError(Exception):
    """Expected webhook handling error."""


@dataclass(frozen=True)
class ReceiverConfig:
    secret: bytes
    routes: list[JsonObject]
    lock_path: Path | None
    timeout_seconds: int


def load_secret() -> bytes:
    secret = os.environ.get("GITHUB_WEBHOOK_SECRET")
    secret_file = os.environ.get("GITHUB_WEBHOOK_SECRET_FILE")
    if secret and secret_file:
        raise WebhookError("set only one of GITHUB_WEBHOOK_SECRET or GITHUB_WEBHOOK_SECRET_FILE")
    if secret_file:
        allowed_secret_dir = Path(os.environ.get("GITHUB_WEBHOOK_SECRET_DIR", "/run/secrets")).expanduser().resolve()
        secret_path = Path(secret_file).expanduser().resolve()
        try:
            secret_path.relative_to(allowed_secret_dir)
        except ValueError as exc:
            raise WebhookError("GITHUB_WEBHOOK_SECRET_FILE must be within GITHUB_WEBHOOK_SECRET_DIR") from exc
        secret = secret_path.read_text(encoding="utf-8").strip()
    if not secret:
        raise WebhookError("missing GITHUB_WEBHOOK_SECRET or GITHUB_WEBHOOK_SECRET_FILE")
    return secret.encode("utf-8")


def load_config(path: Path) -> ReceiverConfig:
    payload = json.loads(path.read_text(encoding="utf-8"))
    routes = payload.get("routes")
    if not isinstance(routes, list) or not routes:
        raise WebhookError("config must contain at least one route")
    timeout_seconds = int(payload.get("timeout_seconds", 600))
    lock_value = payload.get("lock_path")
    return ReceiverConfig(
        secret=load_secret(),
        routes=routes,
        lock_path=Path(lock_value) if isinstance(lock_value, str) and lock_value else None,
        timeout_seconds=timeout_seconds,
    )


def constant_time_signature_ok(secret: bytes, body: bytes, header: str | None) -> bool:
    if not header or not header.startswith("sha256="):
        return False
    expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, header)


def dotted_get(payload: JsonObject, path: str) -> Any:
    value: Any = payload
    for part in path.split("."):
        if not isinstance(value, dict) or part not in value:
            raise WebhookError(f"payload field is unavailable: {path}")
        value = value[part]
    return value


def stringify(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if value is None:
        return ""
    return str(value)


def expand_token(token: str, payload: JsonObject, context: JsonObject) -> str:
    replacements = {
        "event": context["event"],
        "action": context["action"],
        "delivery": context["delivery"],
        "repository": context["repository"],
    }
    for name, value in replacements.items():
        token = token.replace("{" + name + "}", stringify(value))
    while "{" in token and "}" in token:
        start = token.index("{")
        end = token.index("}", start)
        field = token[start + 1 : end]
        if not field.startswith("payload."):
            raise WebhookError(f"unsupported command placeholder: {field}")
        value = stringify(dotted_get(payload, field.removeprefix("payload.")))
        token = token[:start] + value + token[end + 1 :]
    return token


def route_matches(route: JsonObject, event: str, action: str, repository: str) -> bool:
    if route.get("event") != event:
        return False
    actions = route.get("actions")
    if isinstance(actions, list) and action not in actions:
        return False
    route_repository = route.get("repository")
    if isinstance(route_repository, str) and route_repository.lower() != repository.lower():
        return False
    return True


def build_command(route: JsonObject, payload: JsonObject, context: JsonObject) -> list[str]:
    command = route.get("command")
    if not isinstance(command, list) or not command or not all(isinstance(part, str) for part in command):
        raise WebhookError("matched route must contain a non-empty string command array")
    return [expand_token(part, payload, context) for part in command]


def run_with_optional_lock(config: ReceiverConfig, command: list[str], cwd: str | None) -> int:
    if config.lock_path is None:
        return subprocess.run(command, cwd=cwd, check=False, timeout=config.timeout_seconds).returncode
    config.lock_path.parent.mkdir(parents=True, exist_ok=True)
    with config.lock_path.open("w", encoding="utf-8") as lock_file:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        return subprocess.run(command, cwd=cwd, check=False, timeout=config.timeout_seconds).returncode


class GitHubWebhookHandler(http.server.BaseHTTPRequestHandler):
    server_version = "github-webhook-receiver"

    def do_GET(self) -> None:
        if self.path != "/healthz":
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"status":"ok"}\n')

    def do_POST(self) -> None:
        if self.path != "/github":
            self.send_error(404)
            return
        try:
            self.handle_github_post()
        except WebhookError as exc:
            self.log_message("rejected webhook: %s", exc)
            self.send_error(400, "bad webhook")
        except Exception as exc:  # noqa: BLE001
            self.log_message("webhook handler error: %s", exc.__class__.__name__)
            self.send_error(500, "webhook handler error")

    def handle_github_post(self) -> None:
        config: ReceiverConfig = self.server.receiver_config  # type: ignore[attr-defined]
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        if not constant_time_signature_ok(config.secret, body, self.headers.get("X-Hub-Signature-256")):
            raise WebhookError("invalid signature")

        payload = json.loads(body)
        if not isinstance(payload, dict):
            raise WebhookError("payload must be a JSON object")

        event = self.headers.get("X-GitHub-Event") or ""
        delivery = self.headers.get("X-GitHub-Delivery") or ""
        action = stringify(payload.get("action"))
        repository = stringify(dotted_get(payload, "repository.full_name"))
        context: JsonObject = {
            "event": event,
            "action": action,
            "delivery": delivery,
            "repository": repository,
        }

        for route in config.routes:
            if not route_matches(route, event, action, repository):
                continue
            command = build_command(route, payload, context)
            cwd = route.get("working_directory")
            if cwd is not None and not isinstance(cwd, str):
                raise WebhookError("working_directory must be a string")
            route_id = stringify(route.get("id") or "unnamed")
            self.log_message(
                "accepted delivery=%s event=%s action=%s repository=%s route=%s",
                delivery,
                event,
                action,
                repository,
                route_id,
            )
            started = time.monotonic()
            returncode = run_with_optional_lock(config, command, cwd)
            elapsed = time.monotonic() - started
            self.log_message(
                "completed delivery=%s route=%s returncode=%s elapsed=%.1fs",
                delivery,
                route_id,
                returncode,
                elapsed,
            )
            if returncode != 0:
                raise WebhookError("route command failed")
            self.send_response(202)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status":"accepted"}\n')
            return

        self.log_message(
            "ignored delivery=%s event=%s action=%s repository=%s",
            delivery,
            event,
            action,
            repository,
        )
        self.send_response(202)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"status":"ignored"}\n')


class ReceiverServer(http.server.ThreadingHTTPServer):
    receiver_config: ReceiverConfig


def serve(config: ReceiverConfig, host: str, port: int) -> None:
    server = ReceiverServer((host, port), GitHubWebhookHandler)
    server.receiver_config = config

    def stop(_signum: int, _frame: Any) -> None:
        server.shutdown()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    server.serve_forever()


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--config",
        type=Path,
        default=Path(os.environ.get("GITHUB_WEBHOOK_RECEIVER_CONFIG", "github-webhook-receiver.json")),
    )
    parser.add_argument("--host", default=os.environ.get("GITHUB_WEBHOOK_RECEIVER_HOST", "127.0.0.1"))
    parser.add_argument("--port", type=int, default=int(os.environ.get("GITHUB_WEBHOOK_RECEIVER_PORT", "8787")))
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        config = load_config(args.config)
        serve(config, args.host, args.port)
    except WebhookError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
