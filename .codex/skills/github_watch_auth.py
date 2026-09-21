"""Small, side-effect bounded GitHub CLI auth/rate-limit helpers for watchers."""

import json
import os
import shlex
import subprocess
import time

TOKEN_COMMAND_ENV = "CODEX_GITHUB_APP_TOKEN_COMMAND"
REPOSITORY_ENV = "CODEX_GITHUB_REPOSITORY"
MAX_RESET_SLEEP_SECONDS = 15 * 60
SLEEP_SLICE_SECONDS = 5


def redact(value, secrets=()):
    text = str(value or "")
    for secret in secrets:
        if secret:
            text = text.replace(str(secret), "<redacted>")
    return text


def _helper_token(command, repo=None):
    try:
        argv = shlex.split(command)
    except ValueError:
        return None
    if not argv:
        return None
    env = os.environ.copy()
    env.pop("GH_TOKEN", None)
    env.pop("GITHUB_TOKEN", None)
    if repo:
        env[REPOSITORY_ENV] = str(repo)
    try:
        result = subprocess.run(
            argv, check=False, capture_output=True, text=True, timeout=15, env=env
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    token = (result.stdout or "").strip()
    if not token or "\n" in token or "\r" in token or any(ch.isspace() for ch in token):
        return None
    return token


class AuthState:
    """Prefer a configured App-token helper, with one safe refresh attempt."""

    def __init__(self):
        self._token = None
        self._helper_attempted = False
        self._refresh_attempted = False
        self._slept_reset = None
        self.source = "interactive"
        self.provider_failed = False
        self.deadline = None

    def apply(self, env, repo=None, force_refresh=False):
        command = os.environ.get(TOKEN_COMMAND_ENV, "").strip()
        if command and (not self._helper_attempted or force_refresh):
            if force_refresh:
                self._refresh_attempted = True
            self._helper_attempted = True
            token = _helper_token(command, repo=repo)
            if token:
                self._token = token
                self.source = "app_helper"
            else:
                self.provider_failed = True
                raise RuntimeError(f"{TOKEN_COMMAND_ENV} was configured but did not return a token")
        if self._token:
            env["GH_TOKEN"] = self._token
            env.pop("GITHUB_TOKEN", None)
            return True
        if env.get("GH_TOKEN"):
            self.source = "GH_TOKEN"
        elif env.get("GITHUB_TOKEN"):
            self.source = "GITHUB_TOKEN"
        return False

    def refresh(self, env, repo=None):
        if self._refresh_attempted:
            return False
        self._refresh_attempted = True
        return self.apply(env, repo=repo, force_refresh=True)

    def already_slept(self, reset):
        return reset is not None and reset == self._slept_reset

    def mark_slept(self, reset):
        self._slept_reset = reset


def is_rate_limited(message):
    normalized = str(message or "").casefold()
    return any(
        marker in normalized
        for marker in (
            "rate limit exceeded",
            "api rate limit",
            "secondary rate limit",
            "too many requests",
            "http 429",
        )
    )


def is_secondary_limit(message):
    normalized = str(message or "").casefold()
    return "secondary rate limit" in normalized or "abuse detection" in normalized


def is_retry_safe(args):
    """Allow retries only for known read-only gh command forms."""
    args = list(args or [])
    if not args:
        return False
    if args[0] == "api":
        endpoint = str(args[1] if len(args) > 1 else "")
        for flag in ("--method", "-X"):
            if flag in args:
                try:
                    method = str(args[args.index(flag) + 1]).upper()
                except (IndexError, ValueError):
                    return False
                if method != "GET":
                    return False
        if any(str(value).startswith(("--input=", "--field=", "--raw-field=", "--method=", "-X")) for value in args[2:]):
            return False
        if "--input" in args:
            return False
        fields = [str(args[i + 1]) for i, value in enumerate(args[:-1]) if value in {"-f", "-F", "--raw-field", "--field"}]
        if fields:
            if endpoint != "graphql" or not any(field.startswith("query=") for field in fields):
                return False
            query = next(field.split("=", 1)[1] for field in fields if field.startswith("query="))
            if "mutation" in query.casefold() or "subscription" in query.casefold():
                return False
        return len(args) > 1
    if args[0] in {"pr", "run", "repo", "check"}:
        return len(args) > 1 and args[1] in {"view", "list", "status"}
    return False


def rate_resource(args):
    args = list(args or [])
    if args and args[0] == "api":
        endpoint = str(args[1] if len(args) > 1 else "")
        if endpoint == "graphql":
            return "graphql"
        if "/search/" in f"/{endpoint.lstrip('/')}" :
            return "search"
    return "core"


def is_auth_failure(message):
    normalized = str(message or "").casefold()
    return any(marker in normalized for marker in ("http 401", "bad credentials", "authentication failed"))


def reset_from_message(message):
    import re

    match = re.search(r"(?:x[- ]?ratelimit[- ]?reset|reset(?:s)?)[^0-9]{0,20}(\d{9,})", str(message), re.I)
    return int(match.group(1)) if match else None


def retry_after_reset(message):
    import re

    match = re.search(r"retry-after\s*[:=]\s*(\d+)", str(message), re.I)
    if not match:
        return None
    return int(time.time()) + int(match.group(1))


def read_rate_limit_reset(env, resource="core", gh_binary="gh"):
    try:
        result = subprocess.run(
            [gh_binary, "api", "rate_limit"],
            check=False,
            capture_output=True,
            text=True,
            timeout=15,
            env=env,
        )
        payload = json.loads(result.stdout or "")
    except (OSError, ValueError, subprocess.SubprocessError):
        return None
    try:
        return int(payload["resources"][resource]["reset"])
    except (KeyError, TypeError, ValueError):
        return None


def wait_for_reset(state, env, error_text, resource="core", sleep=time.sleep):
    secondary = is_secondary_limit(error_text)
    reset = reset_from_message(error_text) or retry_after_reset(error_text)
    if reset is None and secondary:
        return False
    reset = reset or read_rate_limit_reset(env, resource=resource)
    if reset is None or state.already_slept(reset):
        return False
    reset_delay = max(0, min(reset - int(time.time()) + 1, MAX_RESET_SLEEP_SECONDS))
    delay = reset_delay
    if state.deadline is not None:
        remaining_budget = state.deadline - time.monotonic()
        if remaining_budget <= 0:
            return False
        if reset_delay >= remaining_budget:
            end = time.monotonic() + remaining_budget
            while time.monotonic() < end:
                sleep(min(SLEEP_SLICE_SECONDS, end - time.monotonic()))
            return False
        delay = reset_delay
    state.mark_slept(reset)
    end = time.monotonic() + delay
    while time.monotonic() < end:
        sleep(min(SLEEP_SLICE_SECONDS, end - time.monotonic()))
    return True
