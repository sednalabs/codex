#!/usr/bin/env python3
"""Bounded private client for broker-owned read-only GitHub CLI calls."""
import base64, json, os, socket, struct

MAX_REQUEST = 16 * 1024
MAX_RESPONSE = 512 * 1024
MAX_ARGUMENT = 4096
IO_TIMEOUT = 15
READ_COMMANDS = {"api", "pr", "run", "repo"}
FORBIDDEN = {"rerun", "cancel", "dispatch", "merge", "comment", "edit", "close", "reopen"}

class ProxyError(RuntimeError): pass

def validate_gh_argv(argv, repository=None):
    if not isinstance(argv, list) or not argv: raise ProxyError("unsupported GitHub CLI operation")
    if argv[0] == "-R":
        if len(argv) < 3: raise ProxyError("repository binding is required")
        argv = argv[2:]
    if any(not isinstance(x, str) or not x or len(x) > MAX_ARGUMENT for x in argv):
        raise ProxyError("invalid GitHub CLI argument")
    if argv[0] == "api":
        if len(argv) < 2: raise ProxyError("API endpoint is required")
        if argv[1] == "graphql":
            for idx, value in enumerate(argv[2:]):
                if value in {"-X", "--method"} and (idx + 3 > len(argv) or argv[idx + 3].upper() != "GET"):
                    raise ProxyError("non-GET GraphQL operation rejected")
            if not any(x.startswith(("-f", "--raw-field", "-F")) or x.startswith(("query=", "query:")) for x in argv[2:]):
                raise ProxyError("GraphQL query is required")
            query = " ".join(x for x in argv[2:] if "query" in x.lower()).lstrip()
            query_text = query.split("=", 1)[1].lstrip() if "=" in query else query
            if query_text.lower().startswith(("mutation", "subscription")) or " mutation" in query_text.lower():
                raise ProxyError("GraphQL mutation rejected")
        else:
            if repository:
                prefix = "repos/" + repository + "/"
                endpoint = argv[1].lstrip("/")
                if not endpoint.startswith(prefix): raise ProxyError("REST repository binding rejected")
            for idx, value in enumerate(argv[2:]):
                if value in {"-X", "--method"}:
                    if idx + 3 > len(argv) or argv[idx + 3].upper() != "GET": raise ProxyError("non-GET REST operation rejected")
            if any(x.startswith(("-X=", "--method=")) and x.lower() not in {"-x=get", "--method=get"} for x in argv[2:]):
                raise ProxyError("non-GET REST operation rejected")
    elif argv[0] == "pr":
        if len(argv) < 2 or argv[1] not in {"view", "checks", "status"}: raise ProxyError("read-only PR operation rejected")
    elif argv[0] == "run":
        if len(argv) < 2 or argv[1] not in {"view", "list", "download"}: raise ProxyError("read-only workflow operation rejected")
    elif argv[0] == "repo":
        if argv[1:] != ["view", "--json", "nameWithOwner"]: raise ProxyError("repository autodetection shape rejected")
    else:
        raise ProxyError("unsupported GitHub CLI operation")
    lowered = [x.lower() for x in argv]
    if any(x in {"--web", "--jq", "--template"} or x.startswith(("--jq=", "--template=")) for x in lowered):
        raise ProxyError("unsafe output option rejected")
    if any(x in FORBIDDEN for x in lowered) or any(x.startswith("--ext") or x == "extension" for x in lowered):
        raise ProxyError("mutating or extension operation rejected")
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
