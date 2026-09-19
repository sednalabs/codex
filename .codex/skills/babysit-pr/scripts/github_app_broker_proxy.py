#!/usr/bin/env python3
"""Bounded private client for broker-owned read-only GitHub CLI calls."""
import base64
import json
import os
import re
import socket
import stat
import struct
from pathlib import Path
from urllib.parse import urlparse

MAX_REQUEST = 16 * 1024
MAX_RESPONSE = 512 * 1024
MAX_ARGUMENT = 4096
IO_TIMEOUT = 15
_GRAPHQL_WRITE = re.compile(r"\b(?:mutation|subscription)\b", re.IGNORECASE)
_REST_PATH = re.compile(r"^(?:actions|commits|issues|pulls)(?:/|\?|$)")

class ProxyError(RuntimeError): pass

def _pairs(values):
    if len(values) % 2:
        raise ProxyError("option value is missing")
    return list(zip(values[::2], values[1::2]))


def _validate_graphql(values, repository=None):
    query = None
    variables = {}
    for option, value in _pairs(values):
        if option in {"-X", "--method"}:
            if value.upper() != "GET":
                raise ProxyError("non-GET GraphQL operation rejected")
            continue
        if option not in {"-f", "--raw-field", "-F", "--field"} or "=" not in value:
            raise ProxyError("unsupported GraphQL argument")
        name, field_value = value.split("=", 1)
        if name == "query":
            if option not in {"-f", "--raw-field"} or query is not None:
                raise ProxyError("GraphQL query field is invalid")
            query = field_value
        elif option not in {"-F", "--field"} or name not in {
            "owner", "name", "number", "cursor"
        }:
            raise ProxyError("unsupported GraphQL variable")
        else:
            if name in variables:
                raise ProxyError("duplicate GraphQL variable")
            variables[name] = field_value
    if query is None or not query.lstrip().lower().startswith("query"):
        raise ProxyError("GraphQL read query is required")
    # Be deliberately conservative: reject write-operation words even in
    # comments or string literals rather than trying to implement a parser.
    if _GRAPHQL_WRITE.search(query):
        raise ProxyError("GraphQL mutation rejected")
    if not {"owner", "name", "number"}.issubset(variables):
        raise ProxyError("GraphQL repository binding is incomplete")
    if repository:
        owner, name = repository.split("/", 1)
        if variables["owner"].lower() != owner.lower() or variables["name"].lower() != name.lower():
            raise ProxyError("GraphQL repository binding rejected")
    if not variables["number"].isdigit():
        raise ProxyError("GraphQL pull request number is invalid")


def _validate_rest(endpoint, values, repository):
    normalized = endpoint.lstrip("/")
    if repository:
        prefix = f"repos/{repository}/"
    else:
        match = re.match(r"^repos/[^/]+/[^/]+/", normalized)
        if match is None:
            raise ProxyError("REST repository binding is required")
        prefix = match.group(0)
    if not normalized.startswith(prefix) or not _REST_PATH.match(normalized[len(prefix):]):
        raise ProxyError("REST repository binding rejected")
    explicit_get = False
    has_fields = False
    for option, value in _pairs(values):
        if option in {"-X", "--method"}:
            if value.upper() != "GET":
                raise ProxyError("non-GET REST operation rejected")
            explicit_get = True
        elif option in {"-f", "--raw-field", "-F", "--field"}:
            has_fields = True
            if "=" not in value:
                raise ProxyError("REST field is invalid")
        else:
            raise ProxyError("unsupported REST argument")
    if has_fields and not explicit_get:
        raise ProxyError("REST fields require an explicit GET")


def _validate_download_directory(value):
    if not isinstance(value, str):
        raise ProxyError("download directory must be a string")
    match = re.fullmatch(r"/tmp/(gh-run-download-[A-Za-z0-9._-]+)", value)
    if match is None:
        raise ProxyError("download directory must be absolute")
    # Reconstruct from the allowlisted basename so the filesystem operation
    # never consumes an unchecked client-supplied path expression.
    path = Path("/tmp") / match.group(1)
    try:
        info = path.lstat()
    except OSError as exc:
        raise ProxyError("download directory is unavailable") from exc
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise ProxyError("download directory must be a real directory")
    if info.st_uid != os.geteuid() or info.st_mode & 0o077:
        raise ProxyError("download directory is not private")
    temp_root = Path(os.path.realpath("/tmp"))
    resolved = Path(os.path.realpath(path))
    if resolved.parent != temp_root or not resolved.name.startswith("gh-run-download-"):
        raise ProxyError("download directory escaped the watcher temporary root")


def validate_gh_argv(argv, repository=None):
    if not isinstance(argv, list) or not argv:
        raise ProxyError("unsupported GitHub CLI operation")
    if any(not isinstance(x, str) or not x or len(x) > MAX_ARGUMENT for x in argv):
        raise ProxyError("invalid GitHub CLI argument")
    bound_repo = None
    if argv[0] == "-R":
        if len(argv) < 4 or (repository and argv[1].lower() != repository.lower()):
            raise ProxyError("repository binding mismatch")
        bound_repo = argv[1]
        repository = repository or bound_repo
        argv = argv[2:]

    command = argv[0]
    if command == "api":
        if len(argv) < 2:
            raise ProxyError("API endpoint is required")
        if argv[1] == "graphql":
            _validate_graphql(argv[2:], repository)
        else:
            _validate_rest(argv[1], argv[2:], repository)
    elif command == "repo":
        if argv[1:] != ["view", "--json", "nameWithOwner"]:
            raise ProxyError("repository autodetection shape rejected")
    elif command == "pr":
        if bound_repo is None or argv[1] not in {"view", "checks"}:
            raise ProxyError("read-only PR operation rejected")
        if len(argv) == 4:
            target = None
            option_index = 2
        elif len(argv) == 5:
            target = argv[2]
            option_index = 3
        else:
            raise ProxyError("PR output shape rejected")
        if target is not None and not (target.isdigit() or (
            urlparse(target).netloc == "github.com"
            and urlparse(target).path.startswith(f"/{repository}/pull/")
        )):
            raise ProxyError("PR target is outside the bound repository")
        if argv[option_index] != "--json" or not argv[option_index + 1]:
            raise ProxyError("PR output shape rejected")
    elif command == "run":
        if bound_repo is None or len(argv) < 3:
            raise ProxyError("read-only workflow operation rejected")
        subcommand = argv[1]
        if subcommand == "list":
            pairs = _pairs(argv[2:])
            allowed = {"--workflow", "--limit", "--json", "--branch"}
            if any(option not in allowed for option, _ in pairs):
                raise ProxyError("workflow list shape rejected")
            names = [option for option, _ in pairs]
            if any(names.count(name) != 1 for name in {"--workflow", "--limit", "--json"}):
                raise ProxyError("workflow list shape rejected")
            values = dict(pairs)
            if values["--limit"] != "30" or not values["--workflow"] or not values["--json"]:
                raise ProxyError("workflow list shape rejected")
        elif subcommand == "view":
            tail = argv[2:]
            valid = (
                len(tail) == 3 and tail[0].isdigit() and tail[1] == "--json" and bool(tail[2])
            ) or (
                len(tail) == 2 and tail[0].isdigit() and tail[1] == "--log-failed"
            ) or (
                len(tail) == 3 and tail[0] == "--job" and tail[1].isdigit() and tail[2] == "--log"
            )
            if not valid:
                raise ProxyError("workflow view shape rejected")
        elif subcommand == "download":
            if (
                len(argv) != 7
                or not argv[2].isdigit()
                or argv[3:5] != ["--name", "validation-summary"]
                or argv[5] != "--dir"
            ):
                raise ProxyError("workflow download shape rejected")
            _validate_download_directory(argv[6])
        else:
            raise ProxyError("read-only workflow operation rejected")
    else:
        raise ProxyError("unsupported GitHub CLI operation")
    return True

def _read_frame(sock):
    header = _read_exact(sock, 4)
    size = struct.unpack("!I", header)[0]
    if size > MAX_REQUEST: raise ProxyError("request exceeds safety bound")
    return _read_exact(sock, size)

def _read_exact(sock, size):
    out = bytearray()
    while len(out) < size:
        chunk = sock.recv(size - len(out))
        if not chunk: raise ProxyError("truncated broker frame")
        out.extend(chunk)
    return bytes(out)

def request(socket_path, argv):
    validate_gh_argv(argv)
    payload = json.dumps({"argv": argv}, separators=(",", ":")).encode()
    if len(payload) > MAX_REQUEST: raise ProxyError("request exceeds safety bound")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(IO_TIMEOUT); sock.connect(socket_path)
        sock.sendall(struct.pack("!I", len(payload)) + payload)
        header = _read_exact(sock, 4)
        size = struct.unpack("!I", header)[0]
        if size > MAX_RESPONSE: raise ProxyError("response exceeds safety bound")
        data = _read_exact(sock, size)
        if sock.recv(1): raise ProxyError("multiple broker responses")
    try: result = json.loads(data)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc: raise ProxyError("malformed broker response") from exc
    if not isinstance(result, dict) or "stdout" not in result or "stderr" not in result: raise ProxyError("malformed broker response")
    if "stdout_b64" in result:
        try: result["stdout_bytes"] = base64.b64decode(result["stdout_b64"], validate=True)
        except (ValueError, TypeError): raise ProxyError("malformed binary broker response")
    return result
