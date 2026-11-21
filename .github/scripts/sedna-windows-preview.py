#!/usr/bin/env python3
"""Build, hash, and natively smoke-test a disposable Windows Codex package."""

from __future__ import annotations

from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import queue
import re
import shutil
import struct
import subprocess
import sys
import threading
import time
from typing import Iterator
from zipfile import ZIP_DEFLATED, ZipFile


TARGET = "x86_64-pc-windows-msvc"
BINARIES = {
    "codex.exe": "bin/codex.exe",
    "codex-code-mode-host.exe": "bin/codex-code-mode-host.exe",
    "codex-responses-api-proxy.exe": "bin/codex-responses-api-proxy.exe",
    "codex-app-server.exe": "bin/codex-app-server.exe",
    "codex-command-runner.exe": "codex-resources/codex-command-runner.exe",
    "codex-windows-sandbox-setup.exe": "codex-resources/codex-windows-sandbox-setup.exe",
    "rg.exe": "codex-path/rg.exe",
}
WINDOWS_ENV = (
    "APPDATA",
    "COMSPEC",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "PATH",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "WINDIR",
)


class LocalResponsesHandler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.server.request_count += 1
        if self.path != "/v1/responses":
            self.send_error(404)
            return

        response_id = "sedna-windows-smoke-response"
        events = (
            {
                "type": "response.created",
                "response": {"id": response_id},
            },
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "id": "sedna-windows-smoke-message",
                    "content": [{"type": "output_text", "text": "WINDOWS_SMOKE_OK"}],
                },
            },
            {
                "type": "response.completed",
                "response": {
                    "id": response_id,
                    "usage": {
                        "input_tokens": 0,
                        "input_tokens_details": None,
                        "output_tokens": 0,
                        "output_tokens_details": None,
                        "total_tokens": 0,
                    },
                },
            },
        )
        body = "".join(
            f"event: {event['type']}\n"
            f"data: {json.dumps(event, separators=(',', ':'))}\n\n"
            for event in events
        ).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def log_message(self, format: str, *args: object) -> None:
        del format, args


class LocalResponsesServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), LocalResponsesHandler)
        self.request_count = 0


@contextmanager
def local_responses_server() -> Iterator[LocalResponsesServer]:
    server = LocalResponsesServer()
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def relative_parts(value: str) -> tuple[str, ...]:
    path = PurePosixPath(value)
    if (
        not value
        or "\x00" in value
        or path.is_absolute()
        or path.as_posix() != value
        or any(part in ("", ".", "..") for part in path.parts)
        or "\\" in value
        or ":" in value
    ):
        raise RuntimeError(f"unsafe relative package path: {value}")
    return path.parts


def contained_path(root: Path, relative: str) -> Path:
    parts = relative_parts(relative)
    canonical_root = os.path.realpath(root)
    candidate = os.path.realpath(os.path.join(canonical_root, *parts))
    try:
        common = os.path.commonpath((canonical_root, candidate))
    except ValueError as error:
        raise RuntimeError(f"package path is on another volume: {relative}") from error
    if os.path.normcase(common) != os.path.normcase(canonical_root):
        raise RuntimeError(f"package path escapes its root: {relative}")
    return Path(candidate)


def package_path(root: Path, path: Path) -> str:
    if path.is_symlink():
        raise RuntimeError(f"package contains a symbolic link: {path.name}")
    relative = path.relative_to(root).as_posix()
    checked = contained_path(root, relative)
    if checked != path:
        raise RuntimeError(f"package path is not canonical: {relative}")
    return relative


def is_github_name_component(value: str) -> bool:
    return 0 < len(value) <= 100 and all(
        character.isascii() and (character.isalnum() or character in "._-")
        for character in value
    )


def isolated_env(code_home: Path) -> dict[str, str]:
    env = {key: os.environ[key] for key in WINDOWS_ENV if key in os.environ}
    env.update(CODEX_HOME=str(code_home), RUST_LOG="error")
    return env


def app_server_request(
    executable: Path,
    arguments: list[str],
    request: dict[str, object],
    code_home: Path,
    cwd: Path,
    complete_thread_turn: bool = False,
) -> tuple[dict[str, object], str]:
    request_method = str(request["method"])
    process = subprocess.Popen(
        [str(executable), *arguments],
        cwd=cwd,
        env=isolated_env(code_home),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
        creationflags=subprocess.CREATE_NO_WINDOW,
    )
    output: queue.Queue[dict[str, object] | str | None] = queue.Queue()
    errors: list[str] = []
    pending_notifications: list[dict[str, object]] = []

    def read_stdout() -> None:
        assert process.stdout is not None
        try:
            for line in process.stdout:
                if not line.strip():
                    continue
                try:
                    output.put(json.loads(line))
                except json.JSONDecodeError:
                    output.put(f"non-JSON app-server stdout: {line.rstrip()}")
        except (OSError, ValueError) as error:
            output.put(f"app-server stdout read failed: {error}")
        finally:
            output.put(None)

    def read_stderr() -> None:
        assert process.stderr is not None
        try:
            errors.append(process.stderr.read())
        except (OSError, ValueError) as error:
            errors.append(f"app-server stderr read failed: {error}")

    stdout_reader = threading.Thread(target=read_stdout, daemon=True)
    stderr_reader = threading.Thread(target=read_stderr, daemon=True)
    stdout_reader.start()
    stderr_reader.start()

    def send(message: dict[str, object]) -> None:
        assert process.stdin is not None
        process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def receive(request_id: int, method: str) -> dict[str, object]:
        deadline = time.monotonic() + 45
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"app-server {method} request timed out")
            try:
                message = output.get(timeout=remaining)
            except queue.Empty as error:
                raise TimeoutError(f"app-server {method} request timed out") from error
            if message is None:
                raise RuntimeError(f"app-server exited before answering {method}")
            if isinstance(message, str):
                raise RuntimeError(message)
            if "method" in message:
                pending_notifications.append(message)
            elif message.get("id") == request_id:
                return message

    def receive_notification(method: str, expected_thread_id: str) -> dict[str, object]:
        deadline = time.monotonic() + 45
        while True:
            for index, message in enumerate(pending_notifications):
                params = message.get("params")
                if not isinstance(params, dict):
                    continue
                thread = params.get("thread")
                thread_id = params.get("threadId")
                if isinstance(thread, dict):
                    thread_id = thread.get("id")
                if message.get("method") == method and thread_id == expected_thread_id:
                    return pending_notifications.pop(index)

            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"app-server {method} notification timed out")
            try:
                message = output.get(timeout=remaining)
            except queue.Empty as error:
                raise TimeoutError(f"app-server {method} notification timed out") from error
            if message is None:
                raise RuntimeError(f"app-server exited before emitting {method}")
            if isinstance(message, str):
                raise RuntimeError(message)
            pending_notifications.append(message)

    try:
        send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "sedna-windows-preview-smoke",
                        "title": "Sedna Windows preview smoke",
                        "version": "1",
                    }
                },
            }
        )
        initialized = receive(1, "initialize")
        jsonrpc_result(initialized, "initialize")
        send({"jsonrpc": "2.0", "method": "initialized"})
        send(request)
        response = receive(int(request["id"]), request_method)
        result = jsonrpc_result(response, request_method)
        if complete_thread_turn:
            if request_method != "thread/start":
                raise RuntimeError("only thread/start can request a completed smoke turn")
            thread = result.get("thread")
            thread_id = thread.get("id") if isinstance(thread, dict) else None
            if not isinstance(thread_id, str) or not thread_id:
                raise RuntimeError("thread/start returned no thread ID")
            started = receive_notification("thread/started", thread_id)
            started_params = started.get("params")
            started_thread = (
                started_params.get("thread") if isinstance(started_params, dict) else None
            )
            if not isinstance(started_thread, dict) or started_thread.get("id") != thread_id:
                raise RuntimeError("thread/started notification did not match the created thread")

            send(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "turn/start",
                    "params": {
                        "threadId": thread_id,
                        "cwd": str(cwd),
                        "input": [
                            {"type": "text", "text": "Reply exactly: WINDOWS_SMOKE_OK"}
                        ],
                        "sandboxPolicy": {
                            "type": "workspaceWrite",
                            "writableRoots": [str(cwd)],
                            "networkAccess": False,
                            "excludeTmpdirEnvVar": True,
                            "excludeSlashTmp": True,
                        },
                    },
                }
            )
            turn_response = receive(3, "turn/start")
            jsonrpc_result(turn_response, "turn/start")
            completed = receive_notification("turn/completed", thread_id)
            completed_params = completed.get("params")
            turn = completed_params.get("turn") if isinstance(completed_params, dict) else None
            if not isinstance(turn, dict) or turn.get("status") != "completed":
                raise RuntimeError("local mock-provider smoke turn did not complete successfully")

        assert process.stdin is not None
        process.stdin.close()
        try:
            process.wait(timeout=45)
        except subprocess.TimeoutExpired as error:
            raise TimeoutError("app-server did not exit after stdin closed") from error
        stdout_reader.join(timeout=5)
        stderr_reader.join(timeout=5)
        if stdout_reader.is_alive() or stderr_reader.is_alive():
            raise TimeoutError("app-server output pipes did not close")
        return result, "".join(errors)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        if process.stdin is not None and not process.stdin.closed:
            process.stdin.close()
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()


def jsonrpc_result(response: dict[str, object], method: str) -> dict[str, object]:
    if "error" in response:
        raise RuntimeError(f"app-server {method} failed: {response['error']}")
    result = response.get("result")
    if not isinstance(result, dict):
        raise RuntimeError(f"app-server {method} returned no object result")
    return result


def require_pe_x64(path: Path) -> None:
    with path.open("rb") as executable:
        dos_header = executable.read(64)
        if len(dos_header) != 64 or dos_header[:2] != b"MZ":
            raise RuntimeError(f"missing PE/DOS header: {path}")
        pe_offset = struct.unpack_from("<I", dos_header, 0x3C)[0]
        executable.seek(pe_offset)
        pe_header = executable.read(6)
    if len(pe_header) != 6 or pe_header[:4] != b"PE\0\0":
        raise RuntimeError(f"invalid PE header: {path}")
    machine = struct.unpack_from("<H", pe_header, 4)[0]
    if machine != 0x8664:
        raise RuntimeError(f"expected x64 PE (0x8664) for {path}, got 0x{machine:04x}")


def run_cli(executable: Path, arguments: list[str], cwd: Path, env: dict[str, str]) -> str:
    result = subprocess.run(
        [str(executable), *arguments],
        cwd=cwd,
        env=env,
        check=True,
        capture_output=True,
        text=True,
        timeout=45,
    )
    return result.stdout + result.stderr


def validate_archive(
    archive_path: Path,
    validation_dir: Path,
    expected_manifest: dict[str, object],
    expected_package_files: set[str],
) -> None:
    with ZipFile(archive_path) as archive:
        members = archive.infolist()
        names = [member.filename for member in members]
        if len(names) != len(members):
            raise RuntimeError("preview ZIP contains duplicate file entries")
        for member in members:
            if member.is_dir():
                raise RuntimeError("preview ZIP contains an unmanifested directory entry")
            relative_parts(member.filename)
        for member in members:
            destination = contained_path(validation_dir, member.filename)
            destination.parent.mkdir(parents=True, exist_ok=True)
            with archive.open(member) as source, destination.open("xb") as target:
                shutil.copyfileobj(source, target)

    manifest_path = contained_path(validation_dir, "MANIFEST.json")
    if not manifest_path.is_file():
        raise RuntimeError("preview ZIP is missing MANIFEST.json")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    for key, value in expected_manifest.items():
        if manifest.get(key) != value:
            raise RuntimeError(f"preview manifest field {key} does not match this run")

    files = manifest.get("files")
    if not isinstance(files, list):
        raise RuntimeError("preview manifest files field is not an array")
    manifest_paths: set[str] = set()
    for entry in files:
        if not isinstance(entry, dict) or not isinstance(entry.get("path"), str):
            raise RuntimeError("preview manifest contains an invalid file entry")
        relative = entry["path"]
        relative_parts(relative)
        digest = entry.get("sha256")
        if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None:
            raise RuntimeError(f"preview manifest contains an invalid SHA-256 for {relative}")
        manifest_paths.add(relative)
    expected_names = manifest_paths | {"MANIFEST.json"}
    if (
        len(manifest_paths) != len(files)
        or "MANIFEST.json" in manifest_paths
        or set(names) != expected_names
    ):
        raise RuntimeError("preview ZIP has an unmanifested, missing, or duplicate file")
    if expected_names != expected_package_files:
        raise RuntimeError("preview ZIP does not have the exact expected package contents")

    for entry in files:
        relative = entry["path"]
        path = contained_path(validation_dir, relative)
        if not path.is_file() or sha256(path) != entry["sha256"]:
            raise RuntimeError(f"preview ZIP payload hash mismatch: {relative}")
    for relative in BINARIES.values():
        require_pe_x64(contained_path(validation_dir, relative))


def run_cargo_tests(workspace: Path, target: str, codex: Path) -> None:
    env = os.environ.copy()
    for name in list(env):
        if any(marker in name.lower() for marker in ("token", "secret", "password", "api_key", "apikey", "credential")):
            env.pop(name, None)
    env.pop("CODEX_HOME", None)
    env.pop("OPENAI_API_KEY", None)
    env.pop("CODEX_API_KEY", None)
    env["SEDNA_PREVIEW_CLI"] = str(codex)
    cargo_dir = contained_path(workspace, "codex-rs")
    for test_name in (
        "multi_agent_v2_wait_agent_keeps_queue_only_mailbox_activity_durable",
        "multi_agent_v2_wait_agent_returns_for_already_queued_mail",
    ):
        subprocess.run(
            [
                "cargo",
                "test",
                "--locked",
                "--target",
                target,
                "--release",
                "-p",
                "codex-core",
                "--lib",
                test_name,
                "--",
                "--nocapture",
                "--test-threads=1",
            ],
            cwd=cargo_dir,
            env=env,
            check=True,
        )
    subprocess.run(
        [
            "cargo",
            "test",
            "--locked",
            "--target",
            target,
            "--release",
            "-p",
            "codex-utils-pty",
            "--test",
            "windows_preview_tui",
            "--",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ],
        cwd=cargo_dir,
        env=env,
        check=True,
    )


def main() -> None:
    required_env = (
        "CARGO_TARGET_DIR",
        "CODEX_RELEASE_VERSION",
        "TARGET",
        "DISPLAY_REF",
        "GITHUB_SHA",
        "GITHUB_RUN_ID",
        "GITHUB_REPOSITORY",
        "GITHUB_SERVER_URL",
    )
    missing = [name for name in required_env if not os.environ.get(name)]
    if missing:
        raise RuntimeError(f"missing workflow environment values: {', '.join(missing)}")
    if os.environ["TARGET"] != TARGET:
        raise RuntimeError(f"only {TARGET} is supported, not {os.environ['TARGET']}")

    checkout_root = Path.cwd().resolve(strict=True)
    if checkout_root.name != "codex-rs":
        raise RuntimeError("preview validation must run from the codex-rs checkout directory")
    workspace = checkout_root.parent
    if not (workspace / "scripts" / "build_codex_package.py").is_file():
        raise RuntimeError("the canonical Codex package builder is missing from the checkout")

    target_root_by_drive = {
        "C:": Path("C:/cargo-target"),
        "D:": Path("D:/cargo-target"),
    }
    target_dir = next(
        (
            candidate
            for drive, candidate in target_root_by_drive.items()
            if PureWindowsPath(os.environ["CARGO_TARGET_DIR"]) == PureWindowsPath(candidate)
        ),
        None,
    )
    if target_dir is None or not target_dir.is_dir():
        raise RuntimeError("CARGO_TARGET_DIR is not one of the standard runner cargo-target roots")
    if os.path.normcase(os.path.realpath(target_dir)) != os.path.normcase(str(target_dir)):
        raise RuntimeError("CARGO_TARGET_DIR resolves through an unexpected path alias")
    release_dir = contained_path(target_dir, f"{TARGET}/release")

    release_version = os.environ["CODEX_RELEASE_VERSION"]
    file_version = release_version.replace("+", "__")
    if re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,159}", file_version) is None:
        raise RuntimeError("preview version contains characters that are unsafe in an artifact filename")
    run_id = os.environ["GITHUB_RUN_ID"]
    repository = os.environ["GITHUB_REPOSITORY"]
    owner, separator, repository_name = repository.partition("/")
    server_url = os.environ["GITHUB_SERVER_URL"]
    workflow_commit = os.environ["GITHUB_SHA"]
    if not run_id.isdecimal():
        raise RuntimeError("workflow run ID must be decimal")
    if (
        not separator
        or "/" in repository_name
        or not is_github_name_component(owner)
        or not is_github_name_component(repository_name)
    ):
        raise RuntimeError("workflow repository value is malformed")
    if server_url != "https://github.com":
        raise RuntimeError("native Windows preview artifacts may only be published from github.com")
    if re.fullmatch(r"[0-9a-f]{40,64}", workflow_commit) is None:
        raise RuntimeError("workflow commit must be a full hexadecimal Git object ID")

    dist_root = contained_path(workspace, "dist")
    if os.path.normcase(str(dist_root)) != os.path.normcase(str(workspace / "dist")):
        raise RuntimeError("refusing to write preview artifacts through a dist symlink")
    work_root = contained_path(dist_root, ".sedna-windows-preview-work")
    artifact_dir = contained_path(dist_root, "sedna-windows-preview")
    if work_root.exists() or artifact_dir.exists():
        raise RuntimeError("refusing to replace an existing preview output")
    dist_root.mkdir(parents=True, exist_ok=True)
    work_root.mkdir()
    artifact_dir.mkdir()
    package_dir = contained_path(work_root, "package")
    validation_dir = contained_path(work_root, "zip-validation")
    consumer_root = contained_path(work_root, "consumer")
    package_dir.mkdir()
    validation_dir.mkdir()
    consumer_root.mkdir()

    for binary in BINARIES:
        if binary == "rg.exe":
            # The canonical package builder fetches the pinned DotSlash artifact.
            continue
        binary_path = contained_path(release_dir, binary)
        if not binary_path.is_file():
            raise RuntimeError(f"required Windows preview binary is missing: {binary}")

    package_builder = contained_path(workspace, "scripts/build_codex_package.py")
    subprocess.run(
        [
            sys.executable,
            str(package_builder),
            "--target",
            TARGET,
            "--variant",
            "codex",
            "--package-dir",
            str(package_dir),
            "--entrypoint-bin",
            str(contained_path(release_dir, "codex.exe")),
            "--code-mode-host-bin",
            str(contained_path(release_dir, "codex-code-mode-host.exe")),
            "--codex-command-runner-bin",
            str(contained_path(release_dir, "codex-command-runner.exe")),
            "--codex-windows-sandbox-setup-bin",
            str(contained_path(release_dir, "codex-windows-sandbox-setup.exe")),
        ],
        cwd=workspace,
        check=True,
    )
    shutil.copyfile(
        contained_path(release_dir, "codex-responses-api-proxy.exe"),
        contained_path(package_dir, "bin/codex-responses-api-proxy.exe"),
    )
    shutil.copyfile(
        contained_path(release_dir, "codex-app-server.exe"),
        contained_path(package_dir, "bin/codex-app-server.exe"),
    )

    codex_package_path = contained_path(package_dir, "codex-package.json")
    codex_package = json.loads(codex_package_path.read_text(encoding="utf-8"))
    binary_version = codex_package["version"]
    install_text = """# Sedna native Windows x64 CLI preview

This disposable, unsigned package is not an official Sedna release and does not install automatic updates.

1. Verify the ZIP against its adjacent .sha256 file before extracting it. In PowerShell:
   $zip = Get-Item .\\codex-sedna-preview-*.zip
   $expected = ((Get-Content -Raw "$($zip.FullName).sha256").Trim() -split '\\s+')[0]
   $actual = (Get-FileHash -LiteralPath $zip.FullName -Algorithm SHA256).Hash
   if ($actual -ne $expected) { throw 'Preview ZIP SHA-256 mismatch' }
2. Extract the ZIP into a new per-user directory. Keep bin, codex-resources, and codex-path together; the CLI needs the companion host, ripgrep, command runner, and Windows sandbox setup helper.
3. Run bin\\codex.exe --version or bin\\codex.exe --help. For a TUI trial, open a terminal in the project directory and run the full path to bin\\codex.exe.
4. Use a fresh, isolated CODEX_HOME for a preview trial. Do not replace an existing installation or copy only codex.exe.

The package includes the matching app-server and Responses API proxy executables. MANIFEST.json binds each payload file to the source commit and hosted preview run.
"""
    contained_path(package_dir, "INSTALL-WINDOWS.md").write_text(
        install_text, encoding="utf-8", newline="\n"
    )

    source_commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=workspace, text=True).strip()
    if re.fullmatch(r"[0-9a-f]{40,64}", source_commit) is None:
        raise RuntimeError("source commit must be a full hexadecimal Git object ID")
    run_url = f"{server_url}/{repository}/actions/runs/{run_id}"
    payload_files: list[Path] = []
    for path in sorted(package_dir.rglob("*")):
        if path.is_symlink():
            raise RuntimeError(f"package builder produced a symbolic link: {path.name}")
        if path.is_file():
            payload_files.append(contained_path(package_dir, package_path(package_dir, path)))
    manifest = {
        "manifestVersion": 1,
        "repository": repository,
        "ref": os.environ["DISPLAY_REF"],
        "sourceCommit": source_commit,
        "workflowCommit": workflow_commit,
        "previewVersion": release_version,
        "binaryVersion": binary_version,
        "target": TARGET,
        "packageVariant": "codex",
        "signing": "unsigned",
        "workflowRun": run_url,
        "binaries": list(BINARIES.values()),
        "files": [
            {"path": package_path(package_dir, path), "sha256": sha256(path)}
            for path in payload_files
        ],
    }
    manifest_path = contained_path(package_dir, "MANIFEST.json")
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8", newline="\n")

    archive_name = f"codex-sedna-preview-{file_version}-{TARGET}.zip"
    archive_path = contained_path(artifact_dir, archive_name)
    with ZipFile(archive_path, "w", ZIP_DEFLATED, compresslevel=9) as archive:
        for path in sorted([*payload_files, manifest_path]):
            archive.write(path, package_path(package_dir, path))
    archive_hash = sha256(archive_path)
    sidecar = f"{archive_hash}  {archive_path.name}\n"
    sidecar_path = contained_path(artifact_dir, f"{archive_path.name}.sha256")
    sidecar_path.write_text(sidecar, encoding="ascii", newline="\n")

    expected_files = {package_path(package_dir, path) for path in payload_files} | {"MANIFEST.json"}
    validate_archive(
        archive_path,
        validation_dir,
        {
            "sourceCommit": source_commit,
            "workflowCommit": workflow_commit,
            "previewVersion": release_version,
            "binaryVersion": binary_version,
            "target": TARGET,
            "signing": "unsigned",
        },
        expected_files,
    )
    if sha256(archive_path) != archive_hash or sidecar_path.read_text(encoding="ascii") != sidecar:
        raise RuntimeError("preview ZIP or external SHA-256 sidecar changed after creation")

    package_metadata = json.loads(
        contained_path(validation_dir, "codex-package.json").read_text(encoding="utf-8")
    )
    if package_metadata.get("target") != TARGET or package_metadata.get("version") != binary_version:
        raise RuntimeError("canonical package metadata does not match the source-built binary")

    codex = contained_path(validation_dir, BINARIES["codex.exe"])
    app_server = contained_path(validation_dir, BINARIES["codex-app-server.exe"])
    code_home = contained_path(consumer_root, "codex-home")
    code_home.mkdir()
    contained_path(code_home, "config.toml").write_text(
        '[windows]\nsandbox = "unelevated"\n', encoding="utf-8"
    )
    consumer_workspace = contained_path(consumer_root, "workspace")
    consumer_workspace.mkdir()
    cli_env = isolated_env(code_home)
    version_output = run_cli(codex, ["--version"], consumer_workspace, cli_env)
    if release_version not in version_output or "-dirty" in version_output:
        raise RuntimeError(f"CLI version does not match this clean preview build: {version_output.strip()}")
    for arguments in (["--help"], ["exec", "--help"], ["app-server", "--help"]):
        run_cli(codex, arguments, consumer_workspace, cli_env)
    config_path = contained_path(code_home, "config.toml")
    with local_responses_server() as mock_server:
        mock_host, mock_port = mock_server.server_address
        config_path.write_text(
            f'''model = "mock-model"
model_provider = "local_smoke"
approval_policy = "never"
sandbox_mode = "read-only"

[windows]
sandbox = "unelevated"

[model_providers.local_smoke]
name = "Local Windows package smoke fixture"
base_url = "http://{mock_host}:{mock_port}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
supports_websockets = false
requires_openai_auth = false
''',
            encoding="utf-8",
        )
        start_result, _ = app_server_request(
            codex,
            ["app-server", "--listen", "stdio://"],
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "thread/start",
                "params": {"cwd": str(consumer_workspace), "sandbox": "workspace-write"},
            },
            code_home,
            consumer_workspace,
            complete_thread_turn=True,
        )
        if mock_server.request_count != 1:
            raise RuntimeError(
                "persisted thread smoke did not use exactly one local mock Responses request"
            )
    thread = start_result.get("thread")
    thread_id = thread.get("id") if isinstance(thread, dict) else None
    if not isinstance(thread_id, str) or not thread_id:
        raise RuntimeError("thread/start returned no thread ID")
    resume_result, _ = app_server_request(
        app_server,
        ["--listen", "stdio://"],
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/resume",
            "params": {"threadId": thread_id, "cwd": str(consumer_workspace)},
        },
        code_home,
        consumer_workspace,
    )
    resumed_thread = resume_result.get("thread")
    if not isinstance(resumed_thread, dict) or resumed_thread.get("id") != thread_id:
        raise RuntimeError(f"packaged standalone app-server could not resume thread {thread_id}")

    allowed_file = contained_path(consumer_workspace, "allowed.txt")
    outside_file = contained_path(consumer_root, "outside.txt")
    if allowed_file.exists() or outside_file.exists():
        raise RuntimeError("refusing to overwrite an existing sandbox smoke target")
    workspace_policy = {
        "type": "workspaceWrite",
        "writableRoots": [str(consumer_workspace)],
        "networkAccess": False,
        "excludeTmpdirEnvVar": True,
        "excludeSlashTmp": True,
    }

    def run_sandbox_command(command: str) -> dict[str, object]:
        result, _ = app_server_request(
            app_server,
            ["--listen", "stdio://"],
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "command/exec",
                "params": {
                    "command": ["cmd.exe", "/d", "/c", command],
                    "cwd": str(consumer_workspace),
                    "timeoutMs": 15000,
                    "sandboxPolicy": workspace_policy,
                },
            },
            code_home,
            consumer_workspace,
        )
        return result

    allowed_result = run_sandbox_command("echo ALLOWED> allowed.txt")
    if allowed_result.get("exitCode") != 0 or not allowed_file.is_file():
        raise RuntimeError("Windows restricted-token workspace-write sandbox rejected its allowed command")
    if allowed_file.read_text(encoding="utf-8").strip() != "ALLOWED":
        raise RuntimeError("allowed sandbox command wrote unexpected content")
    rg = contained_path(validation_dir, BINARIES["rg.exe"])
    rg_version = run_cli(rg, ["--version"], consumer_workspace, cli_env)
    if "ripgrep 15.2.0" not in rg_version:
        raise RuntimeError(f"packaged ripgrep version is unexpected: {rg_version.strip()}")
    rg_matches = run_cli(rg, ["ALLOWED", str(allowed_file)], consumer_workspace, cli_env)
    if "ALLOWED" not in rg_matches:
        raise RuntimeError("packaged ripgrep did not find the expected workspace fixture")
    outside_result = run_sandbox_command(r"echo BLOCKED> ..\outside.txt")
    if outside_result.get("exitCode") == 0 or outside_file.exists():
        outside_file.unlink(missing_ok=True)
        raise RuntimeError("Windows restricted-token sandbox did not deny the out-of-scope write")

    run_cargo_tests(workspace, TARGET, codex)
    summary = [
        "### Native Windows x64 Sedna CLI preview",
        "",
        f"- Source commit: `{source_commit}`",
        f"- Workflow commit: `{workflow_commit}`",
        f"- Preview version: `{release_version}`",
        f"- Binary version: `{binary_version}`",
        "- Signing: unsigned preview; not published as a GitHub Release.",
        f"- Target: `{TARGET}`",
        f"- Package: `{archive_path.name}`",
        f"- SHA-256: `{archive_hash}`",
        "- Consumer proof: exact ZIP verified; CLI/app-server startup; persisted thread create/resume; Windows restricted-token workspace-write allow/deny; exact-ZIP ConPTY TUI startup; quiet/actionable wait fixtures.",
        f"- Workflow: {run_url}",
        "",
    ]
    contained_path(artifact_dir, "PREVIEW-SUMMARY.md").write_text(
        "\n".join(summary), encoding="utf-8", newline="\n"
    )


if __name__ == "__main__":
    main()
