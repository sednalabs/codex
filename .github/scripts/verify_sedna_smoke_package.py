#!/usr/bin/env python3
"""Verify and exercise one source-bound Linux SmokePackage artifact pair."""

from __future__ import annotations

import hashlib
import gzip
import json
import os
import platform
import re
import tarfile
import tempfile
import tomllib
from contextlib import contextmanager
from pathlib import Path, PurePosixPath
from typing import Any

from verify_sedna_branch_package import ConsumerFailure
from verify_sedna_branch_package import command_ok


TARGET_MACHINES = {
    "x86_64-unknown-linux-gnu": "x86_64",
    "aarch64-unknown-linux-gnu": "aarch64",
}
ARCHIVE_NAMES = {
    "cli": "codex-package.tar.gz",
    "app_server": "codex-app-server-package.tar.gz",
}
MAX_RECORD_BYTES = 64 * 1024
MAX_ARCHIVE_BYTES = 2 * 1024 * 1024 * 1024
MAX_MEMBER_BYTES = 1024 * 1024 * 1024
MAX_TOTAL_BYTES = 3 * 1024 * 1024 * 1024
MAX_MEMBERS = 32
MAX_MANIFEST_BYTES = 64 * 1024
TAR_BLOCK_BYTES = 512
MAX_ARCHIVE_EXPANDED_BYTES = MAX_TOTAL_BYTES + (MAX_MEMBERS + 20) * TAR_BLOCK_BYTES
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
SOURCE_SHA_RE = re.compile(r"^[0-9a-f]{40}$")


class SmokePackageFailure(Exception):
    def __init__(self, code: str) -> None:
        super().__init__(code)
        self.code = code


def required_env(name: str) -> str:
    value = os.environ.get(name, "")
    if not value:
        raise SmokePackageFailure("input_missing")
    return value


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _run_git(source_root: Path, *arguments: str) -> str:
    import subprocess

    result = subprocess.run(
        ["git", "rev-parse", *arguments],
        cwd=source_root,
        capture_output=True,
        check=False,
        text=True,
    )
    if result.returncode != 0:
        raise SmokePackageFailure("source_checkout_unreadable")
    return result.stdout.strip()


def _source_is_clean(source_root: Path) -> bool:
    import subprocess

    result = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=normal"],
        cwd=source_root,
        capture_output=True,
        check=False,
        text=True,
    )
    return result.returncode == 0 and not result.stdout.strip()


def _package_version(source_root: Path) -> str:
    try:
        manifest = tomllib.loads(
            (source_root / "codex-rs/Cargo.toml").read_text(encoding="utf-8")
        )
        version = manifest["workspace"]["package"]["version"]
    except (OSError, KeyError, TypeError, tomllib.TOMLDecodeError) as exc:
        raise SmokePackageFailure("source_package_version_unreadable") from exc
    if not isinstance(version, str) or not re.fullmatch(r"[0-9A-Za-z.+-]{1,128}", version):
        raise SmokePackageFailure("source_package_version_invalid")
    return version


def _expected_package_paths(variant: str) -> tuple[set[str], set[str], set[str]]:
    entrypoint = "codex" if variant == "codex" else "codex-app-server"
    directories = {
        "bin",
        "codex-path",
        "codex-resources",
        "codex-resources/zsh",
        "codex-resources/zsh/bin",
    }
    required_files = {
        "codex-package.json",
        f"bin/{entrypoint}",
        "bin/codex-code-mode-host",
        "codex-path/rg",
        "codex-resources/bwrap",
    }
    optional_files = {"codex-resources/zsh/bin/zsh"}
    return directories, required_files, optional_files


def _tar_octal(field: bytes) -> int:
    value = field.strip(b"\x00 ")
    if not value:
        return 0
    if any(byte < ord("0") or byte > ord("7") for byte in value):
        raise SmokePackageFailure("package_archive_header_invalid")
    return int(value, 8)


def _validate_tar_headers(archive_path: Path) -> None:
    """Bound decompression and reject tar extensions before tarfile parses them."""

    expanded_bytes = 0
    total_size = 0
    member_count = 0
    zero_blocks = 0
    try:
        with gzip.open(archive_path, "rb") as stream:
            while True:
                header = stream.read(TAR_BLOCK_BYTES)
                if not header:
                    break
                if len(header) != TAR_BLOCK_BYTES:
                    raise SmokePackageFailure("package_archive_header_invalid")
                expanded_bytes += TAR_BLOCK_BYTES
                if expanded_bytes > MAX_ARCHIVE_EXPANDED_BYTES:
                    raise SmokePackageFailure("package_archive_expansion_limit")
                if header == bytes(TAR_BLOCK_BYTES):
                    zero_blocks += 1
                    continue
                if zero_blocks:
                    raise SmokePackageFailure("package_archive_header_invalid")

                checksum = _tar_octal(header[148:156])
                actual_checksum = sum(header[:148]) + (8 * ord(" ")) + sum(header[156:])
                if checksum != actual_checksum:
                    raise SmokePackageFailure("package_archive_header_invalid")

                # SmokePackage archives need only short ASCII names, regular
                # files and directories. Reject PAX/GNU extensions before
                # tarfile can materialize their declared payloads.
                typeflag = header[156:157]
                if typeflag not in (b"\x00", b"0", b"5"):
                    raise SmokePackageFailure("package_archive_header_extension_rejected")
                if header[345:500].strip(b"\x00"):
                    raise SmokePackageFailure("package_archive_header_extension_rejected")
                name_bytes = header[:100].split(b"\x00", 1)[0]
                try:
                    name = name_bytes.decode("ascii")
                except UnicodeDecodeError as exc:
                    raise SmokePackageFailure("package_archive_header_invalid") from exc
                if not name:
                    raise SmokePackageFailure("package_archive_header_invalid")

                member_count += 1
                if member_count > MAX_MEMBERS:
                    raise SmokePackageFailure("package_archive_member_limit")
                size = _tar_octal(header[124:136])
                if typeflag == b"5":
                    if size != 0:
                        raise SmokePackageFailure("package_archive_header_invalid")
                else:
                    if name == "codex-package.json" and size > MAX_MANIFEST_BYTES:
                        raise SmokePackageFailure("package_manifest_invalid")
                    if size > MAX_MEMBER_BYTES or total_size + size > MAX_TOTAL_BYTES:
                        raise SmokePackageFailure("package_archive_expansion_limit")
                    total_size += size

                padded_size = ((size + TAR_BLOCK_BYTES - 1) // TAR_BLOCK_BYTES) * TAR_BLOCK_BYTES
                if expanded_bytes + padded_size > MAX_ARCHIVE_EXPANDED_BYTES:
                    raise SmokePackageFailure("package_archive_expansion_limit")
                remaining = padded_size
                while remaining:
                    chunk = stream.read(min(64 * 1024, remaining))
                    if not chunk:
                        raise SmokePackageFailure("package_archive_header_invalid")
                    expanded_bytes += len(chunk)
                    remaining -= len(chunk)
    except SmokePackageFailure:
        raise
    except (OSError, EOFError) as exc:
        raise SmokePackageFailure("package_archive_invalid") from exc
    if zero_blocks < 2:
        raise SmokePackageFailure("package_archive_header_invalid")


def _safe_extract_package(
    archive_path: Path,
    destination: Path,
    *,
    variant: str,
) -> dict[str, Any]:
    if archive_path.is_symlink() or not archive_path.is_file():
        raise SmokePackageFailure("package_archive_missing")
    if archive_path.stat().st_size > MAX_ARCHIVE_BYTES:
        raise SmokePackageFailure("package_archive_size_limit")
    _validate_tar_headers(archive_path)
    directories, required_files, optional_files = _expected_package_paths(variant)
    expected_paths = directories | required_files | optional_files
    seen: set[str] = set()
    observed_files: set[str] = set()
    try:
        destination.mkdir(parents=True, exist_ok=True)
        with tarfile.open(archive_path, mode="r|gz") as bundle:
            for index, member in enumerate(bundle):
                if index >= MAX_MEMBERS:
                    raise SmokePackageFailure("package_archive_member_limit")
                raw = PurePosixPath(member.name)
                name = raw.as_posix()
                if (
                    not name
                    or raw.is_absolute()
                    or ".." in raw.parts
                    or "." in raw.parts
                    or name not in expected_paths
                    or name in seen
                    or member.pax_headers
                    or not (member.isdir() or member.isfile())
                ):
                    raise SmokePackageFailure("package_archive_member_rejected")
                if member.isdir():
                    if name not in directories or member.size != 0:
                        raise SmokePackageFailure("package_archive_member_rejected")
                    seen.add(name)
                    destination.joinpath(*PurePosixPath(name).parts).mkdir(parents=True, exist_ok=True)
                else:
                    if name in directories or member.size < 0 or member.size > MAX_MEMBER_BYTES:
                        raise SmokePackageFailure("package_archive_expansion_limit")
                    if (
                        name in required_files - {"codex-package.json"} | optional_files
                        and not member.mode & 0o111
                    ):
                        raise SmokePackageFailure("package_archive_executable_missing")
                    seen.add(name)
                    observed_files.add(name)
                    target = destination.joinpath(*PurePosixPath(name).parts)
                    target.parent.mkdir(parents=True, exist_ok=True)
                    source = bundle.extractfile(member)
                    if source is None:
                        raise SmokePackageFailure("package_archive_member_unreadable")
                    copied = 0
                    with source, target.open("xb") as output:
                        while copied < member.size:
                            block = source.read(min(1024 * 1024, member.size - copied))
                            if not block:
                                raise SmokePackageFailure("package_archive_member_size_mismatch")
                            output.write(block)
                            copied += len(block)
                    if copied != member.size:
                        raise SmokePackageFailure("package_archive_member_size_mismatch")
                    is_executable = name in required_files - {"codex-package.json"} or name in optional_files
                    target.chmod(0o755 if is_executable else 0o644)
            if not required_files <= observed_files or observed_files - required_files > optional_files:
                raise SmokePackageFailure("package_archive_inventory_mismatch")
    except SmokePackageFailure:
        raise
    except (OSError, EOFError, tarfile.TarError) as exc:
        raise SmokePackageFailure("package_archive_invalid") from exc

    try:
        manifest_path = destination / "codex-package.json"
        if manifest_path.stat().st_size > MAX_MANIFEST_BYTES:
            raise SmokePackageFailure("package_manifest_invalid")
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except SmokePackageFailure:
        raise
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise SmokePackageFailure("package_manifest_invalid") from exc
    if not isinstance(manifest, dict):
        raise SmokePackageFailure("package_manifest_invalid")
    return manifest


def _read_identity(artifact_dir: Path) -> dict[str, Any]:
    expected_names = {
        "smoke-package.json",
        "codex-package.tar.gz",
        "codex-app-server-package.tar.gz",
    }
    try:
        entries = list(artifact_dir.iterdir())
    except OSError as exc:
        raise SmokePackageFailure("artifact_directory_unreadable") from exc
    if (
        {entry.name for entry in entries} != expected_names
        or any(entry.is_symlink() or not entry.is_file() for entry in entries)
    ):
        raise SmokePackageFailure("artifact_inventory_mismatch")
    record_path = artifact_dir / "smoke-package.json"
    try:
        if record_path.stat().st_size > MAX_RECORD_BYTES:
            raise SmokePackageFailure("artifact_identity_invalid")
        record = json.loads(record_path.read_text(encoding="utf-8"))
    except SmokePackageFailure:
        raise
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise SmokePackageFailure("artifact_identity_invalid") from exc
    if not isinstance(record, dict):
        raise SmokePackageFailure("artifact_identity_invalid")
    return record


def _validate_identity(record: dict[str, Any], *, artifact_dir: Path) -> None:
    source_sha = required_env("EXPECTED_SOURCE_SHA")
    source_tree = required_env("EXPECTED_SOURCE_TREE")
    workflow_sha = required_env("EXPECTED_WORKFLOW_SHA")
    target = required_env("EXPECTED_TARGET")
    run_id = required_env("GITHUB_RUN_ID")
    repository = required_env("GITHUB_REPOSITORY")
    server_url = required_env("GITHUB_SERVER_URL").rstrip("/")
    preview_version = required_env("EXPECTED_PREVIEW_VERSION")
    source_root = Path(required_env("SOURCE_DIR")).resolve()
    if target not in TARGET_MACHINES:
        raise SmokePackageFailure("target_invalid")
    if not SOURCE_SHA_RE.fullmatch(source_sha) or not SOURCE_SHA_RE.fullmatch(source_tree) or not SOURCE_SHA_RE.fullmatch(workflow_sha):
        raise SmokePackageFailure("commit_identity_invalid")
    if platform.machine().lower() not in {
        TARGET_MACHINES[target],
        "amd64" if TARGET_MACHINES[target] == "x86_64" else "arm64",
    }:
        raise SmokePackageFailure("runner_architecture_mismatch")
    if _run_git(source_root, "HEAD") != source_sha or _run_git(source_root, "HEAD^{tree}") != source_tree:
        raise SmokePackageFailure("source_checkout_identity_mismatch")
    if not _source_is_clean(source_root):
        raise SmokePackageFailure("source_checkout_not_clean")
    trusted_root = Path(__file__).resolve().parents[2]
    if _run_git(trusted_root, "HEAD") != workflow_sha:
        raise SmokePackageFailure("trusted_workflow_identity_mismatch")
    package_version = _package_version(source_root)
    expected_workflow_url = f"{server_url}/{repository}/actions/runs/{run_id}"
    if set(record) != {
        "schema_version",
        "repository",
        "run_id",
        "workflow_url",
        "source_sha",
        "source_tree",
        "workflow_sha",
        "target",
        "preview_version",
        "package_version",
        "archives",
        "excluded_artifacts",
    }:
        raise SmokePackageFailure("artifact_identity_invalid")
    expected_values = {
        "schema_version": "sedna-smoke-package-v2",
        "repository": repository,
        "run_id": run_id,
        "workflow_url": expected_workflow_url,
        "source_sha": source_sha,
        "source_tree": source_tree,
        "workflow_sha": workflow_sha,
        "target": target,
        "preview_version": preview_version,
        "package_version": package_version,
    }
    if any(record.get(key) != value for key, value in expected_values.items()):
        raise SmokePackageFailure("artifact_identity_mismatch")
    if record.get("excluded_artifacts") != ["symbols"]:
        raise SmokePackageFailure("artifact_identity_invalid")
    archives = record.get("archives")
    if not isinstance(archives, dict) or set(archives) != {"cli", "app_server"}:
        raise SmokePackageFailure("artifact_identity_invalid")
    expected_archive_names = {
        "cli": ARCHIVE_NAMES["cli"],
        "app_server": ARCHIVE_NAMES["app_server"],
    }
    artifact_names = {
        "cli": "codex-package.tar.gz",
        "app_server": "codex-app-server-package.tar.gz",
    }
    for key, expected_name in expected_archive_names.items():
        value = archives.get(key)
        if not isinstance(value, dict) or set(value) != {"name", "size_bytes", "sha256"}:
            raise SmokePackageFailure("artifact_identity_invalid")
        artifact_path = artifact_dir / artifact_names[key]
        artifact_size = artifact_path.stat().st_size
        if artifact_size > MAX_ARCHIVE_BYTES:
            raise SmokePackageFailure("artifact_archive_size_limit")
        if (
            value.get("name") != expected_name
            or type(value.get("size_bytes")) is not int
            or value["size_bytes"] != artifact_size
            or not isinstance(value.get("sha256"), str)
            or not SHA256_RE.fullmatch(value["sha256"])
            or value["sha256"] != sha256_file(artifact_path)
        ):
            raise SmokePackageFailure("artifact_digest_mismatch")


def _classify_cli_version(actual: str, package_version: str, preview_version: str, source_sha: str) -> str:
    short_sha = source_sha[:8]
    known_forms = {
        f"codex {package_version}": "package_version_only",
        f"codex {package_version} (git:{short_sha})": "package_version_with_source_sha",
        f"codex {preview_version}": "preview_version",
        f"codex {preview_version} (git:{short_sha})": "preview_version_with_source_sha",
        f"codex {package_version} (Sedna v{preview_version})": "progressive_display_version",
        f"codex {package_version} (Sedna dev {short_sha})": "progressive_source_identity",
    }
    return known_forms.get(actual, "unrecognized")


def _manifest_for_variant(manifest: dict[str, Any], *, variant: str, target: str, version: str) -> bool:
    entrypoint = "codex" if variant == "codex" else "codex-app-server"
    expected = {
        "version": version,
        "target": target,
        "variant": variant,
        "entrypoint": f"bin/{entrypoint}",
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
    }
    return (
        set(manifest) == set(expected) | {"layoutVersion"}
        and type(manifest.get("layoutVersion")) is int
        and manifest.get("layoutVersion") == 1
        and all(type(manifest.get(key)) is str and manifest[key] == value for key, value in expected.items())
    )


@contextmanager
def _hide_host_control_environment():
    names = (
        "RESULT_PATH",
        "ARTIFACT_DIR",
        "SOURCE_DIR",
        "RUNNER_TEMP",
        "EXPECTED_SOURCE_SHA",
        "EXPECTED_SOURCE_TREE",
        "EXPECTED_WORKFLOW_SHA",
        "EXPECTED_PREVIEW_VERSION",
        "EXPECTED_TARGET",
        "GITHUB_RUN_ID",
        "GITHUB_REPOSITORY",
        "GITHUB_SERVER_URL",
        "GITHUB_SHA",
        "GITHUB_WORKSPACE",
    )
    saved = {name: os.environ.pop(name) for name in names if name in os.environ}
    try:
        yield
    finally:
        os.environ.update(saved)


def _run_packaged_command(arguments: list[str], *, cwd: Path):
    with _hide_host_control_environment():
        return command_ok(arguments, cwd=cwd)


def _invoke_app_server_help(app_server: Path, *, cwd: Path) -> bool:
    try:
        process = _run_packaged_command([str(app_server), "--help"], cwd=cwd)
    except ConsumerFailure:
        return False
    return process.returncode == 0


def _write_result_exclusive(path: Path, result: dict[str, Any]) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if not hasattr(os, "O_NOFOLLOW"):
        raise SmokePackageFailure("result_write_failed")
    flags |= os.O_NOFOLLOW
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        descriptor = os.open(path, flags, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            output.write(json.dumps(result, sort_keys=True, indent=2) + "\n")
    except OSError as exc:
        raise SmokePackageFailure("result_write_failed") from exc


def verify_and_consume() -> dict[str, Any]:
    artifact_dir = Path(required_env("ARTIFACT_DIR")).resolve()
    runner_temp = Path(required_env("RUNNER_TEMP")).resolve()
    source_sha = required_env("EXPECTED_SOURCE_SHA")
    source_root = Path(required_env("SOURCE_DIR")).resolve()
    target = required_env("EXPECTED_TARGET")
    preview_version = required_env("EXPECTED_PREVIEW_VERSION")
    record = _read_identity(artifact_dir)
    _validate_identity(record, artifact_dir=artifact_dir)
    package_version = _package_version(source_root)

    with tempfile.TemporaryDirectory(prefix="sedna-smoke-package-consumer-", dir=runner_temp) as temp_name:
        temp_root = Path(temp_name)
        cli_root = temp_root / "cli"
        app_server_root = temp_root / "app-server"
        cli_manifest = _safe_extract_package(artifact_dir / "codex-package.tar.gz", cli_root, variant="codex")
        app_server_manifest = _safe_extract_package(
            artifact_dir / "codex-app-server-package.tar.gz", app_server_root, variant="codex-app-server"
        )
        if not _manifest_for_variant(cli_manifest, variant="codex", target=target, version=package_version):
            raise SmokePackageFailure("cli_package_manifest_mismatch")
        if not _manifest_for_variant(
            app_server_manifest,
            variant="codex-app-server",
            target=target,
            version=package_version,
        ):
            raise SmokePackageFailure("app_server_package_manifest_mismatch")
        cli = cli_root / "bin/codex"
        code_mode_host = cli_root / "bin/codex-code-mode-host"
        rg = cli_root / "codex-path/rg"
        bwrap = cli_root / "codex-resources/bwrap"
        app_server = app_server_root / "bin/codex-app-server"
        required_binaries = (cli, code_mode_host, rg, bwrap, app_server)
        if any(not path.is_file() or not os.access(path, os.X_OK) for path in required_binaries):
            raise SmokePackageFailure("packaged_companion_missing")

        os.environ["CODEX_HOME"] = str(temp_root / "isolated-codex-home")
        os.environ["PATH"] = f"{cli_root / 'codex-path'}{os.pathsep}{os.environ.get('PATH', '')}"
        for key in list(os.environ):
            if key.startswith("CODEX_BROWSER_"):
                os.environ.pop(key, None)
        os.environ["CODEX_HOME"] = str(temp_root / "isolated-codex-home")
        try:
            version_process = _run_packaged_command([str(cli), "--version"], cwd=temp_root)
            cli_version_exit_ok = version_process.returncode == 0
            version_class = (
                _classify_cli_version(
                    version_process.stdout.strip(),
                    package_version,
                    preview_version,
                    source_sha,
                )
                if cli_version_exit_ok
                else "command_failed"
            )
        except ConsumerFailure as exc:
            cli_version_exit_ok = False
            version_class = exc.code

        try:
            help_process = _run_packaged_command(
                [str(cli), "mcp", "login", "--device-auth", "--help"], cwd=temp_root
            )
            help_text = f"{help_process.stdout}\n{help_process.stderr}"
            device_auth_help_ok = (
                help_process.returncode == 0 and "--device-auth" in help_text
            )
        except ConsumerFailure:
            device_auth_help_ok = False

        try:
            helper_process = _run_packaged_command([str(code_mode_host), "--help"], cwd=temp_root)
            code_mode_host_help_ok = helper_process.returncode == 0
        except ConsumerFailure:
            code_mode_host_help_ok = False

        app_server_help_ok = _invoke_app_server_help(app_server, cwd=temp_root)

    progressive_classes = {
        "progressive_display_version",
        "progressive_source_identity",
    }
    return {
        "schema_version": "sedna-smoke-consumer-v2",
        "source_sha": source_sha,
        "workflow_sha": required_env("EXPECTED_WORKFLOW_SHA"),
        "run_id": required_env("GITHUB_RUN_ID"),
        "target": target,
        "package_archives_verified": True,
        "cli_version_command_ok": cli_version_exit_ok,
        "cli_version_class": version_class,
        "progressive_identity_matches": version_class in progressive_classes,
        "device_auth_help_ok": device_auth_help_ok,
        "code_mode_host_help_ok": code_mode_host_help_ok,
        "cli_companion_inventory_ok": True,
        "app_server_package_manifest_ok": True,
        "app_server_help_ok": app_server_help_ok,
        "symbols_artifact": "excluded_not_sanitized_or_qualified",
    }


def main() -> int:
    result_path = Path(required_env("RESULT_PATH"))
    try:
        result = verify_and_consume()
    except SmokePackageFailure as exc:
        result = {
            "schema_version": "sedna-smoke-consumer-v2",
            "source_sha": os.environ.get("EXPECTED_SOURCE_SHA", ""),
            "workflow_sha": os.environ.get("EXPECTED_WORKFLOW_SHA", ""),
            "run_id": os.environ.get("GITHUB_RUN_ID", ""),
            "target": os.environ.get("EXPECTED_TARGET", ""),
            "package_archives_verified": False,
            "failure_code": exc.code,
        }
        _write_result_exclusive(result_path, result)
        return 1
    _write_result_exclusive(result_path, result)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
