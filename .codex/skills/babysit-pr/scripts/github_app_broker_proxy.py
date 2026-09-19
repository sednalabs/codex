#!/usr/bin/env python3
"""Bounded private client for broker-owned read-only GitHub CLI calls."""
import json, os, socket

MAX_REQUEST = 16 * 1024
MAX_RESPONSE = 256 * 1024
READ_COMMANDS = {"api", "pr", "run"}
FORBIDDEN = {"rerun", "cancel", "dispatch", "merge", "comment", "edit", "close", "reopen"}

class ProxyError(RuntimeError): pass

def validate_gh_argv(argv):
    if not isinstance(argv, list) or not argv: raise ProxyError("unsupported GitHub CLI operation")
    if argv[0] == "-R":
        if len(argv) < 3: raise ProxyError("repository binding is required")
        argv = argv[2:]
    if argv[0] not in READ_COMMANDS:
        raise ProxyError("unsupported GitHub CLI operation")
    if any(not isinstance(x, str) or len(x) > 4096 for x in argv):
        raise ProxyError("invalid GitHub CLI argument")
    lowered = [x.lower() for x in argv]
    if argv[0] == "pr" and len(argv) > 1 and argv[1] not in {"view", "checks", "status"}: raise ProxyError("read-only PR operation rejected")
    if argv[0] == "run" and len(argv) > 1 and argv[1] not in {"view", "list"}: raise ProxyError("read-only workflow operation rejected")
    if any(x in FORBIDDEN for x in lowered) or any(x.startswith("--method=") and x != "--method=get" for x in lowered):
        raise ProxyError("mutating GitHub CLI operation rejected")
    if argv[0] == "api" and any("mutation" in x.lower() for x in argv[1:]):
        raise ProxyError("GraphQL mutation rejected")
    if argv[0] == "api" and any(x in {"-X", "--method"} for x in argv[1:]):
        raise ProxyError("non-GET REST operation rejected")
    if argv[0] == "api" and any("mutation" in x.lower() for x in argv[1:]):
        raise ProxyError("GraphQL mutation rejected")
    if any(x.startswith("--ext") or x == "extension" for x in lowered):
        raise ProxyError("extensions are rejected")
    return True

def request(socket_path, argv):
    validate_gh_argv(argv)
    payload = json.dumps({"argv": argv}, separators=(",", ":")).encode()
    if len(payload) > MAX_REQUEST: raise ProxyError("request exceeds safety bound")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(15); sock.connect(socket_path); sock.sendall(payload + b"\n")
        data = sock.recv(MAX_RESPONSE + 1)
    if len(data) > MAX_RESPONSE: raise ProxyError("response exceeds safety bound")
    try: result = json.loads(data)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc: raise ProxyError("malformed broker response") from exc
    if not isinstance(result, dict) or "stdout" not in result or "stderr" not in result: raise ProxyError("malformed broker response")
    return result
