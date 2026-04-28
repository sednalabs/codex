#!/usr/bin/env python3
"""Small GitHub webhook receiver with signature verification and routed commands."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import hmac
import http.client
import http.server
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


JsonObject = dict[str, Any]
MAX_BODY_BYTES = 25 * 1024 * 1024


class WebhookError(Exception):
    """Expected webhook handling error."""


class CommandExecutionError(Exception):
    """Route command failed after a webhook was accepted."""


@dataclass(frozen=True)
class ReceiverConfig:
    secret: bytes
    routes: list[JsonObject]
    lock_path: Path | None
    timeout_seconds: int


def load_secret() -> bytes:
    secret = os.environ.get("GITHUB_WEBHOOK_SECRET")
    if not secret:
        raise WebhookError("missing GITHUB_WEBHOOK_SECRET")
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


_SAFE_ARG_PATTERN = re.compile(r"^[A-Za-z0-9._/@:+,=-]+$")


def stringify(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if value is None:
        return ""
    return str(value)


def validate_env_value(value: str, field: str) -> str:
    if not value:
        raise WebhookError(f"payload field expands to empty environment value: {field}")
    if not _SAFE_ARG_PATTERN.fullmatch(value):
        raise WebhookError(f"payload field contains unsafe environment characters: {field}")
    return value


def expand_token(token: str, payload: JsonObject, context: JsonObject) -> str:
    result = []
    last_end = 0
    while "{" in token[last_end:] and "}" in token[last_end:]:
        start = token.index("{", last_end)
        end = token.index("}", start)
        result.append(token[last_end:start])
        field = token[start + 1 : end]
        if field in context:
            result.append(stringify(context[field]))
        elif field.startswith("payload."):
            result.append(stringify(dotted_get(payload, field.removeprefix("payload."))))
        else:
            raise WebhookError(f"unsupported command placeholder: {field}")
        last_end = end + 1
    result.append(token[last_end:])
    return "".join(result)


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
    _ = payload, context
    command = route.get("command")
    if not isinstance(command, list) or not command or not all(isinstance(part, str) for part in command):
        raise WebhookError("matched route must contain a non-empty string command array")
    if any("{" in part or "}" in part for part in command):
        raise WebhookError("command arrays must be static; use environment mappings for webhook values")
    return command


def build_environment(route: JsonObject, payload: JsonObject, context: JsonObject) -> dict[str, str]:
    env = {
        "GITHUB_WEBHOOK_EVENT": context["event"],
        "GITHUB_WEBHOOK_ACTION": context["action"],
        "GITHUB_WEBHOOK_DELIVERY": context["delivery"],
        "GITHUB_WEBHOOK_REPOSITORY": context["repository"],
    }
    configured = route.get("environment")
    if configured is None:
        return {key: stringify(value) for key, value in env.items()}
    if not isinstance(configured, dict) or not all(
        isinstance(key, str) and isinstance(value, str) for key, value in configured.items()
    ):
        raise WebhookError("environment must be an object with string keys and values")
    for key, template in configured.items():
        if not re.fullmatch(r"[A-Z_][A-Z0-9_]*", key):
            raise WebhookError(f"invalid environment variable name: {key}")
        env[key] = validate_env_value(expand_token(template, payload, context), f"environment.{key}")
    return {key: stringify(value) for key, value in env.items()}


def run_with_optional_lock(
    config: ReceiverConfig,
    command: list[str],
    cwd: str | None,
    env: dict[str, str],
) -> int:
    process_env = os.environ.copy()
    process_env.update(env)
    if config.lock_path is None:
        return subprocess.run(
            command,
            cwd=cwd,
            env=process_env,
            check=False,
            timeout=config.timeout_seconds,
        ).returncode
    config.lock_path.parent.mkdir(parents=True, exist_ok=True)
    with config.lock_path.open("w", encoding="utf-8") as lock_file:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        return subprocess.run(
            command,
            cwd=cwd,
            env=process_env,
            check=False,
            timeout=config.timeout_seconds,
        ).returncode


def content_length(headers: http.client.HTTPMessage) -> int:
    raw_value = headers.get("Content-Length", "0")
    try:
        length = int(raw_value)
    except ValueError as exc:
        raise WebhookError("invalid Content-Length") from exc
    if length < 0 or length > MAX_BODY_BYTES:
        raise WebhookError("invalid or excessive Content-Length")
    return length


def decode_payload(body: bytes) -> JsonObject:
    try:
        payload = json.loads(body)
    except json.JSONDecodeError as exc:
        raise WebhookError(f"invalid JSON payload: {exc}") from exc
    if not isinstance(payload, dict):
        raise WebhookError("payload must be a JSON object")
    return payload


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
        except CommandExecutionError as exc:
            self.log_message("webhook command error: %s", exc)
            self.send_error(500, "webhook command error")
        except Exception as exc:  # noqa: BLE001
            self.log_message("webhook handler error: %s: %s", exc.__class__.__name__, exc)
            self.send_error(500, "webhook handler error")

    def handle_github_post(self) -> None:
        config: ReceiverConfig = self.server.receiver_config  # type: ignore[attr-defined]
        length = content_length(self.headers)
        body = self.rfile.read(length)
        if not constant_time_signature_ok(config.secret, body, self.headers.get("X-Hub-Signature-256")):
            raise WebhookError("invalid signature")

        payload = decode_payload(body)

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
            env = build_environment(route, payload, context)
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
            returncode = run_with_optional_lock(config, command, cwd, env)
            elapsed = time.monotonic() - started
            self.log_message(
                "completed delivery=%s route=%s returncode=%s elapsed=%.1fs",
                delivery,
                route_id,
                returncode,
                elapsed,
            )
            if returncode != 0:
                raise CommandExecutionError(f"route command failed with exit code {returncode}")
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


def make_shutdown_handler(server: http.server.HTTPServer) -> Any:
    shutdown_started = threading.Event()

    def stop(_signum: int, _frame: Any) -> None:
        if shutdown_started.is_set():
            return
        shutdown_started.set()
        threading.Thread(
            target=server.shutdown,
            name="github-webhook-receiver-shutdown",
            daemon=True,
        ).start()

    return stop


def serve(config: ReceiverConfig, host: str, port: int) -> None:
    server = ReceiverServer((host, port), GitHubWebhookHandler)
    server.receiver_config = config

    stop = make_shutdown_handler(server)
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        server.serve_forever()
    finally:
        server.server_close()


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
