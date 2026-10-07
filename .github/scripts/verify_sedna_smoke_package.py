#!/usr/bin/env python3
"""Verify and exercise one source-bound Linux SmokePackage artifact pair."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import tarfile
import tempfile
import tomllib
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
    directories, required_files, optional_files = _expected_package_paths(variant)
    expected_paths = directories | required_files | optional_files
    seen: dict[str, tarfile.TarInfo] = {}
    total_size = 0
    try:
        with tarfile.open(archive_path, mode="r:gz") as bundle:
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
                else:
                    if name in directories or member.size < 0 or member.size > MAX_MEMBER_BYTES:
                        raise SmokePackageFailure("package_archive_expansion_limit")
                    if (
                        name in required_files - {"codex-package.json"} | optional_files
                        and not member.mode & 0o111
                    ):
                        raise SmokePackageFailure("package_archive_executable_missing")
                    total_size += member.size
                    if total_size > MAX_TOTAL_BYTES:
                        raise SmokePackageFailure("package_archive_expansion_limit")
                seen[name] = member
            observed_files = {name for name, member in seen.items() if member.isfile()}
            if not required_files <= observed_files or observed_files - required_files > optional_files:
                raise SmokePackageFailure("package_archive_inventory_mismatch")

            destination.mkdir(parents=True, exist_ok=True)
            for name, member in seen.items():
                target = destination.joinpath(*PurePosixPath(name).parts)
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                    continue
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


def _validate_symbols_archive(path: Path, *, run_id: str, target: str) -> None:
    root = f"codex-symbols-sedna-{run_id}-{target}"
    expected_files = {
        f"{root}/codex.debug",
        f"{root}/codex-app-server.debug",
        f"{root}/codex-code-mode-host.debug",
    }
    observed_files: set[str] = set()
    observed_dirs: set[str] = set()
    total_size = 0
    try:
        if path.is_symlink() or not path.is_file() or path.stat().st_size > MAX_ARCHIVE_BYTES:
            raise SmokePackageFailure("symbols_archive_invalid")
        with tarfile.open(path, mode="r:gz") as bundle:
            for index, member in enumerate(bundle):
                if index >= MAX_MEMBERS:
                    raise SmokePackageFailure("symbols_archive_member_limit")
                raw = PurePosixPath(member.name)
                name = raw.as_posix()
                if raw.is_absolute() or ".." in raw.parts or member.pax_headers:
                    raise SmokePackageFailure("symbols_archive_member_rejected")
                if member.isdir():
                    if name != root or member.size != 0 or name in observed_dirs:
                        raise SmokePackageFailure("symbols_archive_member_rejected")
                    observed_dirs.add(name)
                elif member.isfile():
                    if name not in expected_files or name in observed_files:
                        raise SmokePackageFailure("symbols_archive_member_rejected")
                    if member.size < 0 or member.size > MAX_MEMBER_BYTES:
                        raise SmokePackageFailure("symbols_archive_expansion_limit")
                    total_size += member.size
                    if total_size > MAX_TOTAL_BYTES:
                        raise SmokePackageFailure("symbols_archive_expansion_limit")
                    observed_files.add(name)
                else:
                    raise SmokePackageFailure("symbols_archive_member_rejected")
    except SmokePackageFailure:
        raise
    except (OSError, EOFError, tarfile.TarError) as exc:
        raise SmokePackageFailure("symbols_archive_invalid") from exc
    if observed_files != expected_files or observed_dirs != {root}:
        raise SmokePackageFailure("symbols_archive_inventory_mismatch")


def _read_identity(artifact_dir: Path) -> dict[str, Any]:
    expected_names = {
        "smoke-package.json",
        "codex-package.tar.gz",
        "codex-app-server-package.tar.gz",
        "codex-symbols.tar.gz",
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
    expected_symbol_name = "codex-symbols.tar.gz"
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
    }:
        raise SmokePackageFailure("artifact_identity_invalid")
    expected_values = {
        "schema_version": "sedna-smoke-package-v1",
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
    archives = record.get("archives")
    if not isinstance(archives, dict) or set(archives) != {"cli", "app_server", "symbols"}:
        raise SmokePackageFailure("artifact_identity_invalid")
    expected_archive_names = {
        "cli": ARCHIVE_NAMES["cli"],
        "app_server": ARCHIVE_NAMES["app_server"],
        "symbols": expected_symbol_name,
    }
    artifact_names = {
        "cli": "codex-package.tar.gz",
        "app_server": "codex-app-server-package.tar.gz",
        "symbols": "codex-symbols.tar.gz",
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
        f"codex {package_version} (Sedna {preview_version})": "progressive_display_version",
        f"codex {package_version} (Sedna dev g{short_sha})": "progressive_source_identity",
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
        _validate_symbols_archive(
            artifact_dir / "codex-symbols.tar.gz",
            run_id=required_env("GITHUB_RUN_ID"),
            target=target,
        )

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
            version_process = command_ok([str(cli), "--version"], cwd=temp_root)
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
            help_process = command_ok(
                [str(cli), "mcp", "login", "--device-auth", "--help"], cwd=temp_root
            )
            help_text = f"{help_process.stdout}\n{help_process.stderr}"
            device_auth_help_ok = (
                help_process.returncode == 0 and "--device-auth" in help_text
            )
        except ConsumerFailure:
            device_auth_help_ok = False

        try:
            helper_process = command_ok([str(code_mode_host), "--help"], cwd=temp_root)
            code_mode_host_help_ok = helper_process.returncode == 0
        except ConsumerFailure:
            code_mode_host_help_ok = False

    progressive_classes = {
        "preview_version",
        "preview_version_with_source_sha",
        "progressive_display_version",
        "progressive_source_identity",
    }
    return {
        "schema_version": "sedna-smoke-consumer-v1",
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
    }


def main() -> int:
    result_path = Path(required_env("RESULT_PATH"))
    try:
        result = verify_and_consume()
    except SmokePackageFailure as exc:
        result = {
            "schema_version": "sedna-smoke-consumer-v1",
            "source_sha": os.environ.get("EXPECTED_SOURCE_SHA", ""),
            "workflow_sha": os.environ.get("EXPECTED_WORKFLOW_SHA", ""),
            "run_id": os.environ.get("GITHUB_RUN_ID", ""),
            "target": os.environ.get("EXPECTED_TARGET", ""),
            "package_archives_verified": False,
            "failure_code": exc.code,
        }
        result_path.write_text(json.dumps(result, sort_keys=True, indent=2) + "\n", encoding="utf-8")
        return 1
    result_path.write_text(json.dumps(result, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
