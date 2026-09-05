#!/usr/bin/env python3
"""Short-lived GitHub App installation-token broker.

The broker deliberately has a small, stdlib-only surface.  It validates the
installation before minting a token, keeps one token in memory, and can run a
single pre-bound command with the token in ``GH_TOKEN``.  It never persists or
prints credential material.
"""

import argparse
import atexit
import base64
import hashlib
import json
import os
import re
import selectors
import shutil
import stat
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Mapping, Sequence
from urllib.error import HTTPError, URLError
from urllib.parse import urlparse
from urllib.request import HTTPRedirectHandler, Request, build_opener

API_VERSION = "2022-11-28"
DEFAULT_API_BASE = "https://api.github.com"
KEY_BASENAME = "github-app-private-key.pem"
OPENSSL_PATH = "/usr/bin/openssl"
MAX_HTTP_BODY = 1024 * 1024
MAX_CHILD_OUTPUT = 256 * 1024
CHILD_TERMINATION_GRACE_SECONDS = 5
JWT_LIFETIME_SECONDS = 9 * 60
REFRESH_THRESHOLD_SECONDS = 120
ALLOWED_PERMISSIONS = frozenset(
    {"metadata", "contents", "pull_requests", "merge_queues", "checks", "actions", "statuses", "administration"}
)
READ_PERMISSION_NAMES = frozenset(ALLOWED_PERMISSIONS)
TOKEN_ENV_NAMES = frozenset(
    {
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GH_PAT",
        "GITHUB_PAT",
        "GITHUB_APP_PRIVATE_KEY",
        "GITHUB_CLIENT_SECRET",
    }
)
TOKEN_ENV_RE = re.compile(r"^(?:GH|GITHUB)_.+(?:TOKEN|PAT|SECRET|PRIVATE_KEY)$")
APP_SLUG_RE = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
JWT_RE = re.compile(r"(?<![A-Za-z0-9_-])[A-Za-z0-9_-]{12,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{12,}(?![A-Za-z0-9_-])")


class BrokerError(RuntimeError):
    """A safe, non-secret broker failure."""


@dataclass(frozen=True)
class HttpResult:
    status: int
    headers: Mapping[str, str]
    body: bytes
    url: str


@dataclass(frozen=True)
class TokenRecord:
    token: str
    expires_at: str
    expires_epoch: float
    permissions: Mapping[str, str]
    rate_headers: Mapping[str, str]


def _b64url(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode("ascii")


def build_jwt_claims(app_id: str | int, now: int) -> tuple[str, str]:
    """Build the unsigned JWT header and claims segments for inspection/tests."""
    if not str(app_id).isdigit():
        raise BrokerError("App ID must be numeric")
    header = _b64url(_json_bytes({"alg": "RS256", "typ": "JWT"}))
    claims = _b64url(_json_bytes({"iat": int(now) - 60, "exp": int(now) + JWT_LIFETIME_SECONDS, "iss": int(app_id)}))
    return header, claims


def _redact(value: Any, secrets: Sequence[str] = ()) -> str:
    text = str(value)
    for secret in secrets:
        if secret:
            text = text.replace(secret, "[REDACTED]")
    text = re.sub(r"(?i)(authorization\s*:\s*(?:bearer|token)\s+)[^\s,;]+", r"\1[REDACTED]", text)
    text = JWT_RE.sub("[REDACTED]", text)
    return text


def _without_ambient_tokens(environment: Mapping[str, str]) -> dict[str, str]:
    clean = dict(environment)
    for name in list(clean):
        if name in TOKEN_ENV_NAMES or TOKEN_ENV_RE.fullmatch(name):
            clean.pop(name, None)
    return clean


def _json_bytes(value: Any) -> bytes:
    try:
        return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")
    except (TypeError, ValueError) as exc:
        raise BrokerError("request body is not JSON-serializable") from exc


def _normalise_repo(repository: str) -> tuple[str, str, str]:
    if not isinstance(repository, str) or repository.count("/") != 1:
        raise BrokerError("repository must be OWNER/REPOSITORY")
    owner, name = repository.split("/", 1)
    if not owner or not name or any(c.isspace() for c in repository):
        raise BrokerError("repository must be OWNER/REPOSITORY")
    return owner, name, f"{owner}/{name}"


def _normalise_permissions(value: Mapping[str, Any] | Sequence[str]) -> dict[str, str]:
    if isinstance(value, Mapping):
        items = value.items()
    elif isinstance(value, Sequence) and not isinstance(value, (str, bytes, bytearray)):
        items = ((name, "read") for name in value)
    else:
        raise BrokerError("permissions must be an object or list")
    result: dict[str, str] = {}
    for raw_name, raw_level in items:
        if not isinstance(raw_name, str):
            raise BrokerError("permission name must be a string")
        name = raw_name.lower()
        if name not in ALLOWED_PERMISSIONS:
            raise BrokerError(f"unsupported permission: {name}")
        if raw_level != "read":
            raise BrokerError(f"write permission is not allowed: {name}")
        if name in result:
            raise BrokerError(f"duplicate permission: {name}")
        result[name] = "read"
    if not result:
        raise BrokerError("at least one read permission is required")
    return dict(sorted(result.items()))


def _normalise_phase_a_grant(value: Any, *, source: str) -> dict[str, str]:
    if not isinstance(value, dict):
        raise BrokerError(f"{source} permissions are missing")
    result: dict[str, str] = {}
    for raw_name, raw_level in value.items():
        if not isinstance(raw_name, str):
            raise BrokerError(f"{source} permission name is invalid")
        name = raw_name.lower()
        if name not in ALLOWED_PERMISSIONS or raw_level != "read" or name in result:
            raise BrokerError(f"{source} permissions exceed the Phase A read-only ceiling")
        result[name] = "read"
    expected = {name: "read" for name in ALLOWED_PERMISSIONS}
    if result != expected:
        raise BrokerError(f"{source} permissions do not match the Phase A read-only ceiling")
    return dict(sorted(result.items()))


def _parse_expiry(value: Any) -> tuple[str, float]:
    if not isinstance(value, str):
        raise BrokerError("token expiry is missing or invalid")
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as exc:
        raise BrokerError("token expiry is invalid") from exc
    if parsed.tzinfo is None:
        raise BrokerError("token expiry must include a timezone")
    return value, parsed.timestamp()


def _response_json(response: HttpResult) -> Any:
    if len(response.body) > MAX_HTTP_BODY:
        raise BrokerError("GitHub response body exceeds the safety bound")
    try:
        return json.loads(response.body.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise BrokerError("GitHub response was not valid JSON") from exc


def _header(headers: Mapping[str, str], name: str) -> str | None:
    wanted = name.lower()
    for key, value in headers.items():
        if str(key).lower() == wanted:
            return str(value)
    return None


class _NoRedirectHandler(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: D401
        raise BrokerError("GitHub redirect rejected")


def fingerprint_command(repository: str, permissions: Mapping[str, Any] | Sequence[str], argv: Sequence[str]) -> str:
    """Return a deterministic fingerprint for one command and its source files."""
    if not argv or any(not isinstance(arg, str) for arg in argv):
        raise BrokerError("command argv must be non-empty strings")
    _, _, repo = _normalise_repo(repository)
    perms = _normalise_permissions(permissions)
    executable = shutil.which(argv[0]) if "/" not in argv[0] else argv[0]
    if executable is None:
        raise BrokerError("command executable was not found")
    source_entries = []
    seen: set[str] = set()
    script_args = [
        arg
        for arg in argv[1:]
        if _looks_like_path(arg)
        and (Path(arg).expanduser().exists() or arg.endswith((".py", ".sh", ".js", ".mjs", ".rb", ".pl")))
    ]
    for candidate in (executable, *script_args):
        path = Path(candidate).expanduser()
        if not path.is_absolute():
            path = Path.cwd() / path
        # Inspect the spelling supplied to the command before resolving it;
        # otherwise a symlink would disappear from the safety check.
        _validate_source(path)
        path = path.resolve(strict=True)
        key = str(path)
        if key in seen:
            continue
        seen.add(key)
        _validate_source(path)
        try:
            data = path.read_bytes()
        except OSError as exc:
            raise BrokerError("command source could not be read") from exc
        source_entries.append(
            {
                "path": key,
                "sha256": hashlib.sha256(data).hexdigest(),
                "mode": stat.S_IMODE(path.stat().st_mode),
                "uid": path.stat().st_uid,
                "dev": path.stat().st_dev,
                "ino": path.stat().st_ino,
            }
        )
    payload = {"argv": list(argv), "executable": str(Path(executable).resolve()), "repository": repo, "permissions": perms, "sources": source_entries}
    return hashlib.sha256(_json_bytes(payload)).hexdigest()


# Descriptive alias for callers that prefer noun-first naming.
command_fingerprint = fingerprint_command


def _looks_like_path(value: str) -> bool:
    if value.startswith("-"):
        return False
    return "/" in value or value.endswith((".py", ".sh", ".js", ".mjs", ".rb", ".pl"))


def _validate_source(path: Path) -> None:
    try:
        info = path.lstat()
    except OSError as exc:
        raise BrokerError("command source could not be inspected") from exc
    if stat.S_ISLNK(info.st_mode):
        raise BrokerError("symlink command sources are rejected")
    if not stat.S_ISREG(info.st_mode):
        raise BrokerError("command source is not a regular file")
    if info.st_mode & 0o022:
        raise BrokerError("group/world-writable command source is rejected")


def _validate_private_key(path: Path) -> os.stat_result:
    """Validate the exact credential inode without reading its key material."""
    try:
        info = path.lstat()
    except OSError as exc:
        raise BrokerError("private key could not be inspected") from exc
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise BrokerError("private key must be a regular non-symlink credential")
    if info.st_mode & 0o077:
        raise BrokerError("private key must not be accessible to group or other users")
    if info.st_uid not in {0, os.geteuid()}:
        raise BrokerError("private key owner is not trusted")
    return info


def _validate_credentials_directory(path: Path) -> None:
    if not path.is_absolute():
        raise BrokerError("CREDENTIALS_DIRECTORY must be absolute")
    try:
        info = path.lstat()
    except OSError as exc:
        raise BrokerError("CREDENTIALS_DIRECTORY could not be inspected") from exc
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise BrokerError("CREDENTIALS_DIRECTORY must be a non-symlink directory")
    if info.st_mode & 0o077:
        raise BrokerError("CREDENTIALS_DIRECTORY must not be accessible to group or other users")
    if info.st_uid not in {0, os.geteuid()}:
        raise BrokerError("CREDENTIALS_DIRECTORY owner is not trusted")


def _production_credentials_directory(value: str | None) -> Path:
    if not isinstance(value, str) or not value:
        raise BrokerError("CREDENTIALS_DIRECTORY is required")
    root = os.path.realpath("/run/credentials")
    canonical = os.path.realpath(value)
    if not canonical.startswith(root + os.sep):
        raise BrokerError("production CREDENTIALS_DIRECTORY escaped /run/credentials")
    return Path(canonical)


def _run_child_bounded(argv: Sequence[str], environment: Mapping[str, str]) -> tuple[int, bytes, bytes]:
    """Run one child while enforcing an in-memory output ceiling."""
    try:
        process = subprocess.Popen(
            list(argv),
            env=dict(environment),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
    except OSError as exc:
        raise BrokerError("child command failed to start") from exc
    if process.stdout is None or process.stderr is None:
        process.kill()
        process.wait()
        raise BrokerError("child output pipes were not created")

    streams = {process.stdout.fileno(): ("stdout", process.stdout), process.stderr.fileno(): ("stderr", process.stderr)}
    buffers = {"stdout": bytearray(), "stderr": bytearray()}
    selector = selectors.DefaultSelector()
    for descriptor, (label, stream) in streams.items():
        os.set_blocking(descriptor, False)
        selector.register(stream, selectors.EVENT_READ, data=label)

    output_exceeded = False
    try:
        while selector.get_map():
            events = selector.select(timeout=1)
            if not events and process.poll() is not None:
                for key in list(selector.get_map().values()):
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
                break
            for key, _ in events:
                try:
                    chunk = os.read(key.fileobj.fileno(), 64 * 1024)
                except BlockingIOError:
                    continue
                if not chunk:
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
                    continue
                target = buffers[key.data]
                remaining = MAX_CHILD_OUTPUT - len(target)
                if remaining > 0:
                    target.extend(chunk[:remaining])
                if len(chunk) > remaining:
                    output_exceeded = True
                    break
            if output_exceeded:
                break
    finally:
        selector.close()

    if output_exceeded:
        for _, stream in streams.values():
            stream.close()
        process.terminate()
        try:
            process.wait(timeout=CHILD_TERMINATION_GRACE_SECONDS)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        raise BrokerError("child output exceeded the safety bound")

    return process.wait(), bytes(buffers["stdout"]), bytes(buffers["stderr"])


class GitHubAppBroker:
    """Validate, mint, cache, and revoke one GitHub App installation token."""

    def __init__(
        self,
        *,
        app_id: str | int,
        app_slug: str,
        installation_id: str | int,
        account: str,
        repository: str,
        permissions: Mapping[str, Any] | Sequence[str],
        key_basename: str = KEY_BASENAME,
        api_base_url: str = DEFAULT_API_BASE,
        requester: Callable[..., Any] | None = None,
        child_runner: Callable[[Sequence[str], Mapping[str, str]], tuple[int, bytes, bytes]] = _run_child_bounded,
        clock: Callable[[], float] = time.time,
    ) -> None:
        self.app_id = str(app_id)
        self.installation_id = str(installation_id)
        if not self.app_id.isdigit() or not self.installation_id.isdigit():
            raise BrokerError("App and installation IDs must be numeric")
        if not isinstance(app_slug, str) or not APP_SLUG_RE.fullmatch(app_slug):
            raise BrokerError("App slug is invalid")
        self.app_slug = app_slug
        if not isinstance(account, str) or not account or "/" in account:
            raise BrokerError("installation account is invalid")
        self.account = account
        owner, _, self.repository = _normalise_repo(repository)
        if owner.lower() != self.account.lower():
            raise BrokerError("selected repository is outside the installation account")
        self.permissions = _normalise_permissions(permissions)
        self.credentials_directory = _production_credentials_directory(os.environ.get("CREDENTIALS_DIRECTORY"))
        _validate_credentials_directory(self.credentials_directory)
        if key_basename != KEY_BASENAME:
            raise BrokerError("private-key basename must use the fixed credential name")
        self.key_basename = key_basename
        parsed = urlparse(api_base_url)
        if (
            parsed.scheme != "https"
            or not parsed.netloc
            or parsed.username
            or parsed.password
            or parsed.path not in {"", "/"}
            or parsed.params
            or parsed.query
            or parsed.fragment
        ):
            raise BrokerError("API base must be an HTTPS origin")
        if requester is None and api_base_url.rstrip("/") != DEFAULT_API_BASE:
            raise BrokerError("production API origin must be api.github.com")
        self.api_base_url = api_base_url.rstrip("/")
        self._origin = (parsed.scheme, parsed.hostname, parsed.port or 443)
        self._requester = requester or self._urllib_request
        self._child_runner = child_runner
        self._clock = clock
        self._record: TokenRecord | None = None
        self._pending_cleanup_token: str | None = None
        self._closed = False
        atexit.register(self._atexit_revoke)

    @property
    def key_path(self) -> Path:
        path = self.credentials_directory / self.key_basename
        if path.parent != self.credentials_directory:
            raise BrokerError("private-key path escaped credentials directory")
        return path

    def _jwt(self) -> str:
        path = self.key_path
        expected_key = _validate_private_key(path)
        _validate_source(Path(OPENSSL_PATH))
        now = int(self._clock())
        header, claims = build_jwt_claims(self.app_id, now)
        signing_input = f"{header}.{claims}".encode("ascii")
        try:
            key_fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        except OSError as exc:
            raise BrokerError("private key could not be opened") from exc
        try:
            opened_key = os.fstat(key_fd)
            if (opened_key.st_dev, opened_key.st_ino) != (expected_key.st_dev, expected_key.st_ino):
                raise BrokerError("private key changed while it was being opened")
            if opened_key.st_mode & 0o077 or opened_key.st_uid not in {0, os.geteuid()}:
                raise BrokerError("opened private key no longer satisfies the custody boundary")
            os.set_inheritable(key_fd, True)
            completed = subprocess.run(
                [OPENSSL_PATH, "dgst", "-sha256", "-sign", f"/dev/fd/{key_fd}"],
                input=signing_input,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                env=_without_ambient_tokens(os.environ),
                pass_fds=(key_fd,),
            )
        except (OSError, ValueError) as exc:
            raise BrokerError("OpenSSL signing failed") from exc
        finally:
            os.close(key_fd)
        if completed.returncode != 0 or not completed.stdout:
            raise BrokerError("OpenSSL signing failed")
        return f"{header}.{claims}.{_b64url(completed.stdout)}"

    def _url(self, path: str) -> str:
        if not path.startswith("/") or ".." in path.split("/"):
            raise BrokerError("unsafe GitHub API path")
        url = f"{self.api_base_url}{path}"
        parsed = urlparse(url)
        if (parsed.scheme, parsed.hostname, parsed.port or 443) != self._origin:
            raise BrokerError("cross-host GitHub target rejected")
        return url

    def _urllib_request(self, method: str, url: str, headers: Mapping[str, str], data: bytes | None) -> HttpResult:
        request = Request(url, method=method, headers=dict(headers), data=data)
        try:
            with build_opener(_NoRedirectHandler).open(request, timeout=15) as response:
                body = response.read(MAX_HTTP_BODY + 1)
                return HttpResult(response.status, dict(response.headers.items()), body, response.geturl())
        except HTTPError as exc:
            body = exc.read(MAX_HTTP_BODY + 1)
            return HttpResult(exc.code, dict(exc.headers.items()), body, exc.geturl())
        except (URLError, TimeoutError, OSError) as exc:
            raise BrokerError(f"GitHub request failed: {_redact(exc)}") from exc

    def _request_json(self, method: str, path: str, token: str | None, body: Any = None) -> tuple[HttpResult, Any]:
        url = self._url(path)
        headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": API_VERSION}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        data = None if body is None else _json_bytes(body)
        if data is not None:
            headers["Content-Type"] = "application/json"
        try:
            raw = self._requester(method, url, headers, data)
        except BrokerError:
            raise
        except (TypeError, ValueError) as exc:
            raise BrokerError("GitHub requester failed before returning a response") from exc
        response = self._coerce_response(raw, url)
        if response.status < 200 or response.status >= 300:
            detail = ""
            if response.body and len(response.body) <= MAX_HTTP_BODY:
                try:
                    parsed = json.loads(response.body.decode("utf-8"))
                    detail = parsed.get("message", "") if isinstance(parsed, dict) else ""
                except (UnicodeDecodeError, json.JSONDecodeError):
                    detail = ""
            raise BrokerError(_redact(f"GitHub request returned HTTP {response.status}: {detail}", (token,) if token else ()))
        if method == "DELETE" and not response.body:
            return response, None
        return response, _response_json(response)

    def _coerce_response(self, raw: Any, expected_url: str) -> HttpResult:
        if isinstance(raw, HttpResult):
            response = raw
        elif isinstance(raw, tuple) and len(raw) == 4:
            response = HttpResult(int(raw[0]), dict(raw[1]), bytes(raw[2]), str(raw[3]))
        elif isinstance(raw, Mapping):
            body = raw.get("body", b"")
            if isinstance(body, str):
                body = body.encode("utf-8")
            response = HttpResult(int(raw.get("status", 200)), dict(raw.get("headers", {})), bytes(body), str(raw.get("url", expected_url)))
        else:
            raise BrokerError("requester returned an invalid response")
        parsed_expected = urlparse(expected_url)
        parsed_actual = urlparse(response.url)
        if (parsed_actual.scheme, parsed_actual.hostname, parsed_actual.port or 443) != (parsed_expected.scheme, parsed_expected.hostname, parsed_expected.port or 443):
            raise BrokerError("cross-host redirect rejected")
        if response.url != expected_url:
            raise BrokerError("GitHub redirect rejected")
        if len(response.body) > MAX_HTTP_BODY:
            raise BrokerError("GitHub response body exceeds the safety bound")
        return response

    def _validate_app(self) -> Mapping[str, Any]:
        response, payload = self._request_json("GET", "/app", self._jwt())
        if not isinstance(payload, dict):
            raise BrokerError("App response is not an object")
        if str(payload.get("id")) != self.app_id or payload.get("slug") != self.app_slug:
            raise BrokerError("App identity mismatch")
        owner = payload.get("owner")
        if (
            not isinstance(owner, dict)
            or str(owner.get("login", "")).lower() != self.account.lower()
            or owner.get("type") != "Organization"
        ):
            raise BrokerError("App is not owned by the expected organization")
        permissions = _normalise_phase_a_grant(payload.get("permissions"), source="App")
        if payload.get("events") != []:
            raise BrokerError("App has webhook event subscriptions outside Phase A")
        return {"permissions": permissions, "headers": response.headers}

    def _validate_installation(self) -> Mapping[str, Any]:
        response, payload = self._request_json("GET", f"/app/installations/{self.installation_id}", self._jwt())
        if not isinstance(payload, dict):
            raise BrokerError("installation response is not an object")
        if str(payload.get("id")) != self.installation_id:
            raise BrokerError("installation identity mismatch")
        account = payload.get("account")
        if not isinstance(account, dict) or str(account.get("login", "")).lower() != self.account.lower():
            raise BrokerError("installation account mismatch")
        if str(payload.get("app_id", "")) != self.app_id:
            raise BrokerError("App identity mismatch")
        if payload.get("app_slug") != self.app_slug:
            raise BrokerError("installation App slug mismatch")
        if payload.get("target_type") != "Organization":
            raise BrokerError("installation target is not an organization")
        if payload.get("repository_selection") != "selected":
            raise BrokerError("installation is not selected-repository scoped")
        if payload.get("suspended_at") is not None:
            raise BrokerError("installation is suspended")
        if payload.get("events") != []:
            raise BrokerError("installation has webhook event subscriptions outside Phase A")
        normalised_granted = _normalise_phase_a_grant(payload.get("permissions"), source="installation")
        return {"permissions": normalised_granted, "headers": response.headers}

    def _revoke_token_once(self, token: str) -> dict[str, Any]:
        try:
            response, _ = self._request_json("DELETE", "/installation/token", token)
            if response.status not in {204, 200}:
                raise BrokerError("installation token revocation returned an unexpected status")
            return {"attempted": True, "revoked": True, "status": response.status}
        except BrokerError as exc:
            return {"attempted": True, "revoked": False, "error": _redact(exc, (token,))}

    def get_installation_token(self) -> TokenRecord:
        if self._closed:
            raise BrokerError("broker is closed")
        if self._pending_cleanup_token is not None:
            raise BrokerError("installation token cleanup is pending")
        if self._record and self._clock() < self._record.expires_epoch - REFRESH_THRESHOLD_SECONDS:
            return self._record
        if self._record is not None:
            old_token = self._record.token
            revocation = self._revoke_token_once(old_token)
            if not revocation["revoked"]:
                self._pending_cleanup_token = old_token
                self._record = None
                self._closed = True
                raise BrokerError("cached installation token could not be revoked before refresh")
            self._record = None
        self._validate_app()
        self._validate_installation()
        response, payload = self._request_json(
            "POST",
            f"/app/installations/{self.installation_id}/access_tokens",
            self._jwt(),
            {"repositories": [self.repository.split("/", 1)[1]], "permissions": self.permissions},
        )
        issued_token = payload.get("token") if isinstance(payload, dict) else None
        try:
            if not isinstance(issued_token, str) or not issued_token:
                raise BrokerError("installation token response is invalid")
            if payload.get("repository_selection") != "selected":
                raise BrokerError("installation token is not selected-repository scoped")
            expires_at, expires_epoch = _parse_expiry(payload.get("expires_at"))
            if expires_epoch <= self._clock():
                raise BrokerError("installation token is already expired")
            returned_permissions = payload.get("permissions")
            if not isinstance(returned_permissions, dict):
                raise BrokerError("installation token permissions are missing")
            normalised_returned = _normalise_permissions(returned_permissions)
            if normalised_returned != self.permissions:
                raise BrokerError("installation token permissions do not match the requested subset")
            repositories = payload.get("repositories")
            if not isinstance(repositories, list) or len(repositories) != 1 or not isinstance(repositories[0], dict):
                raise BrokerError("installation token repository binding is invalid")
            full_name = repositories[0].get("full_name")
            if str(full_name).lower() != self.repository.lower():
                raise BrokerError("installation token repository mismatch")
        except BrokerError as exc:
            if isinstance(issued_token, str) and issued_token:
                cleanup = self._revoke_token_once(issued_token)
                if not cleanup["revoked"]:
                    self._pending_cleanup_token = issued_token
                    self._closed = True
                    raise BrokerError("installation token response was rejected and revocation was not proven") from exc
            raise
        rate_headers = {key: value for key, value in response.headers.items() if key.lower().startswith("x-ratelimit-")}
        self._record = TokenRecord(issued_token, expires_at, expires_epoch, normalised_returned, rate_headers)
        return self._record

    def public_identity(self, record: TokenRecord | None = None) -> dict[str, Any]:
        record = record or self.get_installation_token()
        return {"app_id": self.app_id, "app_slug": self.app_slug, "installation_id": self.installation_id, "account": self.account, "repository": self.repository, "permissions": dict(record.permissions), "expires_at": record.expires_at, "mint_rate_limit_headers": dict(record.rate_headers)}

    def execute(self, argv: Sequence[str], expected_fingerprint: str) -> dict[str, Any]:
        actual = fingerprint_command(self.repository, self.permissions, argv)
        if not isinstance(expected_fingerprint, str) or actual != expected_fingerprint:
            raise BrokerError("command fingerprint mismatch")
        record = self.get_installation_token()
        if fingerprint_command(self.repository, self.permissions, argv) != expected_fingerprint:
            raise BrokerError("command fingerprint changed before child execution")
        child_env = _without_ambient_tokens(os.environ)
        child_env["GH_TOKEN"] = record.token
        try:
            returncode, raw_stdout, raw_stderr = self._child_runner(argv, child_env)
        except BrokerError:
            raise
        except (OSError, ValueError) as exc:
            raise BrokerError(f"child command failed to start: {_redact(exc, (record.token,))}") from exc
        stdout = _redact(raw_stdout.decode("utf-8", "replace"), (record.token,))
        stderr = _redact(raw_stderr.decode("utf-8", "replace"), (record.token,))
        return {"identity": self.public_identity(record), "returncode": returncode, "stdout": stdout, "stderr": stderr}

    def revoke(self) -> dict[str, Any]:
        token = self._pending_cleanup_token or (self._record.token if self._record is not None else None)
        if token is None:
            self._closed = True
            return {"attempted": False, "revoked": False}
        result = self._revoke_token_once(token)
        if result["revoked"]:
            self._pending_cleanup_token = None
            self._record = None
            self._closed = True
        else:
            self._pending_cleanup_token = token
            self._record = None
            self._closed = True
        return result

    def _atexit_revoke(self) -> None:
        if self._record is not None or self._pending_cleanup_token is not None:
            self.revoke()

    def close(self) -> dict[str, Any]:
        return self.revoke()

    def __enter__(self) -> "GitHubAppBroker":
        return self

    def __exit__(self, exc_type, exc_value, traceback) -> bool:
        self.close()
        return False


def _parse_permissions_arg(value: str) -> dict[str, str]:
    try:
        parsed = json.loads(value)
    except json.JSONDecodeError as exc:
        raise argparse.ArgumentTypeError("permissions must be JSON") from exc
    try:
        return _normalise_permissions(parsed)
    except BrokerError as exc:
        raise argparse.ArgumentTypeError(str(exc)) from exc


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--app-id", required=True)
    common.add_argument("--app-slug", required=True)
    common.add_argument("--installation-id", required=True)
    common.add_argument("--account", required=True)
    common.add_argument("--repo", required=True)
    common.add_argument("--permissions", required=True, type=_parse_permissions_arg)
    fp = sub.add_parser("fingerprint", parents=[common], help="print a non-minting command fingerprint")
    fp.add_argument("argv", nargs=argparse.REMAINDER)
    run = sub.add_parser("exec", parents=[common], help="run one fingerprint-bound command")
    run.add_argument("--fingerprint", required=True)
    run.add_argument("argv", nargs=argparse.REMAINDER)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    command_argv = list(args.argv)
    if command_argv and command_argv[0] == "--":
        command_argv = command_argv[1:]
    broker: GitHubAppBroker | None = None
    try:
        if not command_argv:
            raise BrokerError("a command is required")
        if args.command == "fingerprint":
            print(fingerprint_command(args.repo, args.permissions, command_argv))
            return 0
        broker = GitHubAppBroker(app_id=args.app_id, app_slug=args.app_slug, installation_id=args.installation_id, account=args.account, repository=args.repo, permissions=args.permissions)
        result = broker.execute(command_argv, args.fingerprint)
        revocation = broker.close()
        result["revocation"] = revocation
        print(json.dumps(result, sort_keys=True))
        if revocation.get("attempted") and not revocation.get("revoked"):
            return 3
        return int(result["returncode"])
    except BrokerError as exc:
        revocation = broker.close() if broker is not None else {"attempted": False, "revoked": False}
        message = _redact(exc)
        if revocation.get("attempted") and not revocation.get("revoked"):
            message = f"{message}; installation token revocation was not proven"
        print(message, file=sys.stderr)
        return 2
    except Exception:
        revocation = broker.close() if broker is not None else {"attempted": False, "revoked": False}
        message = "broker failed closed on an unexpected internal error"
        if revocation.get("attempted") and not revocation.get("revoked"):
            message = f"{message}; installation token revocation was not proven"
        print(message, file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
