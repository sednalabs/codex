#!/usr/bin/env python3
"""Build, hash, and natively smoke-test a disposable Windows Codex package."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import queue
import shutil
import struct
import subprocess
import sys
import threading
import time
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


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def package_path(root: Path, path: Path) -> str:
    return path.relative_to(root).as_posix()


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
) -> tuple[dict[str, object], str]:
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
        except (OSError, ValueError):
            pass
        finally:
            output.put(None)

    def read_stderr() -> None:
        assert process.stderr is not None
        try:
            errors.append(process.stderr.read())
        except (OSError, ValueError):
            pass

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
            if "method" not in message and message.get("id") == request_id:
                return message

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
        response = receive(int(request["id"]), str(request["method"]))
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
        return jsonrpc_result(response, method), "".join(errors)
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
) -> dict[str, object]:
    with ZipFile(archive_path) as archive:
        members = [member for member in archive.infolist() if not member.is_dir()]
        names = {member.filename for member in members}
        if len(names) != len(members):
            raise RuntimeError("preview ZIP contains duplicate file entries")
        for name in names:
            path = PurePosixPath(name)
            if (
                not name
                or not path.parts
                or path.is_absolute()
                or ".." in path.parts
                or "\\" in name
                or ":" in name
            ):
                raise RuntimeError(f"unsafe ZIP member path: {name}")
        archive.extractall(validation_dir, members=members)

    manifest_path = validation_dir / "MANIFEST.json"
    if not manifest_path.is_file():
        raise RuntimeError("preview ZIP is missing MANIFEST.json")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    for key, value in expected_manifest.items():
        if manifest.get(key) != value:
            raise RuntimeError(f"preview manifest field {key} does not match this run")

    files = manifest.get("files")
    if not isinstance(files, list):
        raise RuntimeError("preview manifest files field is not an array")
    manifest_paths = {str(entry["path"]) for entry in files}
    expected_names = manifest_paths | {"MANIFEST.json"}
    if len(manifest_paths) != len(files) or "MANIFEST.json" in manifest_paths or names != expected_names:
        raise RuntimeError("preview ZIP has an unmanifested, missing, or duplicate file")
    if expected_names != expected_package_files:
        raise RuntimeError("preview ZIP does not have the exact expected package contents")

    for entry in files:
        relative = str(entry["path"])
        path = validation_dir.joinpath(*PurePosixPath(relative).parts)
        if not path.is_file() or sha256(path) != entry["sha256"]:
            raise RuntimeError(f"preview ZIP payload hash mismatch: {relative}")
    for relative in BINARIES.values():
        require_pe_x64(validation_dir.joinpath(*PurePosixPath(relative).parts))
    return manifest


def run_cargo_tests(workspace: Path, target: str, codex: Path) -> None:
    env = os.environ.copy()
    for name in list(env):
        if any(marker in name.lower() for marker in ("token", "secret", "password", "api_key", "apikey", "credential")):
            env.pop(name, None)
    env.pop("CODEX_HOME", None)
    env.pop("OPENAI_API_KEY", None)
    env.pop("CODEX_API_KEY", None)
    env["SEDNA_PREVIEW_CLI"] = str(codex)
    cargo_dir = workspace / "codex-rs"
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
        "GITHUB_WORKSPACE",
        "RUNNER_TEMP",
        "CARGO_TARGET_DIR",
        "CODEX_RELEASE_VERSION",
        "TARGET",
        "DISPLAY_REF",
        "GITHUB_RUN_ID",
        "GITHUB_REPOSITORY",
        "GITHUB_SERVER_URL",
        "GITHUB_STEP_SUMMARY",
    )
    missing = [name for name in required_env if not os.environ.get(name)]
    if missing:
        raise RuntimeError(f"missing workflow environment values: {', '.join(missing)}")
    if os.environ["TARGET"] != TARGET:
        raise RuntimeError(f"only {TARGET} is supported, not {os.environ['TARGET']}")

    workspace = Path(os.environ["GITHUB_WORKSPACE"]).resolve()
    release_dir = Path(os.environ["CARGO_TARGET_DIR"]) / TARGET / "release"
    run_root = Path(os.environ["RUNNER_TEMP"]) / f"sedna-windows-preview-{os.environ['GITHUB_RUN_ID']}"
    artifact_dir = workspace / "dist" / f"sedna-windows-preview-{os.environ['GITHUB_RUN_ID']}"
    if run_root.exists() or artifact_dir.exists():
        raise RuntimeError("refusing to replace an existing preview output")
    package_dir = run_root / "package"
    validation_dir = run_root / "zip-validation"
    consumer_root = run_root / "consumer"
    package_dir.mkdir(parents=True)
    validation_dir.mkdir()
    consumer_root.mkdir()
    artifact_dir.mkdir(parents=True)

    for binary in BINARIES:
        if binary == "rg.exe":
            # The canonical package builder fetches the pinned DotSlash artifact.
            continue
        if not (release_dir / binary).is_file():
            raise RuntimeError(f"required Windows preview binary is missing: {release_dir / binary}")

    subprocess.run(
        [
            sys.executable,
            str(workspace / "scripts" / "build_codex_package.py"),
            "--target",
            TARGET,
            "--variant",
            "codex",
            "--package-dir",
            str(package_dir),
            "--entrypoint-bin",
            str(release_dir / "codex.exe"),
            "--code-mode-host-bin",
            str(release_dir / "codex-code-mode-host.exe"),
            "--codex-command-runner-bin",
            str(release_dir / "codex-command-runner.exe"),
            "--codex-windows-sandbox-setup-bin",
            str(release_dir / "codex-windows-sandbox-setup.exe"),
        ],
        cwd=workspace,
        check=True,
    )
    shutil.copyfile(release_dir / "codex-responses-api-proxy.exe", package_dir / "bin/codex-responses-api-proxy.exe")
    shutil.copyfile(release_dir / "codex-app-server.exe", package_dir / "bin/codex-app-server.exe")

    codex_package = json.loads((package_dir / "codex-package.json").read_text(encoding="utf-8"))
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
    (package_dir / "INSTALL-WINDOWS.md").write_text(install_text, encoding="utf-8", newline="\n")

    source_commit = subprocess.check_output(
        ["git", "-C", str(workspace), "rev-parse", "HEAD"], text=True
    ).strip()
    workflow_root = workspace / ".workflow-src"
    workflow_commit = subprocess.check_output(
        ["git", "-C", str(workflow_root), "rev-parse", "HEAD"], text=True
    ).strip()
    release_version = os.environ["CODEX_RELEASE_VERSION"]
    run_url = (
        f"{os.environ['GITHUB_SERVER_URL']}/{os.environ['GITHUB_REPOSITORY']}"
        f"/actions/runs/{os.environ['GITHUB_RUN_ID']}"
    )
    payload_files = sorted(path for path in package_dir.rglob("*") if path.is_file())
    manifest = {
        "manifestVersion": 1,
        "repository": os.environ["GITHUB_REPOSITORY"],
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
    manifest_path = package_dir / "MANIFEST.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8", newline="\n")

    archive_base = f"codex-sedna-preview-{release_version.replace('+', '__')}-{TARGET}"
    archive_path = artifact_dir / f"{archive_base}.zip"
    with ZipFile(archive_path, "w", ZIP_DEFLATED, compresslevel=9) as archive:
        for path in sorted(package_dir.rglob("*")):
            if path.is_file():
                archive.write(path, package_path(package_dir, path))
    archive_hash = sha256(archive_path)
    sidecar = f"{archive_hash}  {archive_path.name}\n"
    sidecar_path = Path(f"{archive_path}.sha256")
    sidecar_path.write_text(sidecar, encoding="ascii", newline="\n")

    expected_files = {package_path(package_dir, path) for path in payload_files} | {"MANIFEST.json"}
    extracted = validate_archive(
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

    package_metadata = json.loads((validation_dir / "codex-package.json").read_text(encoding="utf-8"))
    if package_metadata.get("target") != TARGET or package_metadata.get("version") != binary_version:
        raise RuntimeError("canonical package metadata does not match the source-built binary")

    codex = validation_dir / "bin/codex.exe"
    app_server = validation_dir / "bin/codex-app-server.exe"
    code_home = consumer_root / "codex-home"
    code_home.mkdir()
    (code_home / "config.toml").write_text('[windows]\nsandbox = "unelevated"\n', encoding="utf-8")
    consumer_workspace = consumer_root / "workspace"
    consumer_workspace.mkdir()
    cli_env = isolated_env(code_home)
    version_output = run_cli(codex, ["--version"], consumer_workspace, cli_env)
    if release_version not in version_output or "-dirty" in version_output:
        raise RuntimeError(f"CLI version does not match this clean preview build: {version_output.strip()}")
    for arguments in (["--help"], ["exec", "--help"], ["app-server", "--help"]):
        run_cli(codex, arguments, consumer_workspace, cli_env)
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

    allowed_file = consumer_workspace / "allowed.txt"
    outside_file = consumer_root / "outside.txt"
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
    rg = validation_dir / "codex-path" / "rg.exe"
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
    with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a", encoding="utf-8") as summary_file:
        summary_file.write("\n".join(summary))


if __name__ == "__main__":
    main()
