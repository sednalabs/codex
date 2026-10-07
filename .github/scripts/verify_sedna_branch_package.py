#!/usr/bin/env python3
"""Consume one exact Linux Sedna preview package without profile activation."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path, PurePosixPath
from typing import Any


EXPECTED_BINARIES = {
    "codex",
    "codex-code-mode-host",
    "codex-responses-api-proxy",
}
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
VERSION_RE = re.compile(
    r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?-dev\.sedna\.[1-9][0-9]*\+g[0-9a-f]{8}$"
)
VERSION_OUTPUT_RE = re.compile(r"^codex (.+) \(git:([0-9a-f]{8})\)$")
TARGET_MACHINES = {
    "x86_64-unknown-linux-gnu": "x86_64",
    "aarch64-unknown-linux-gnu": "aarch64",
}


class ConsumerFailure(Exception):
    def __init__(self, code: str) -> None:
        super().__init__(code)
        self.code = code


def required_env(name: str) -> str:
    value = os.environ.get(name, "")
    if not value:
        raise ConsumerFailure("input_missing")
    return value


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def command_ok(arguments: list[str], *, cwd: Path) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            arguments,
            cwd=cwd,
            text=True,
            capture_output=True,
            check=False,
            timeout=45,
        )
    except subprocess.TimeoutExpired as exc:
        raise ConsumerFailure("packaged_command_timeout") from exc


def verify_sidecar(path: Path, expected_name: str, expected_digest: str) -> None:
    try:
        fields = path.read_text(encoding="ascii").strip().split()
    except (OSError, UnicodeDecodeError) as exc:
        raise ConsumerFailure("checksum_sidecar_invalid") from exc
    if len(fields) != 2 or fields[0] != expected_digest or fields[1] != expected_name:
        raise ConsumerFailure("checksum_sidecar_mismatch")


def read_manifest(artifact_dir: Path, archive_base: str) -> tuple[dict[str, Any], Path]:
    expected_names = {
        f"{archive_base}.json",
        f"{archive_base}.tar.gz",
        f"{archive_base}.tar.gz.sha256",
        *(f"{archive_base}.{name}.sha256" for name in EXPECTED_BINARIES),
    }
    try:
        entries = list(artifact_dir.iterdir())
    except OSError as exc:
        raise ConsumerFailure("artifact_directory_unreadable") from exc
    if any(entry.is_symlink() or not entry.is_file() for entry in entries) or {
        entry.name for entry in entries
    } != expected_names:
        raise ConsumerFailure("artifact_inventory_mismatch")
    try:
        manifest = json.loads((artifact_dir / f"{archive_base}.json").read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ConsumerFailure("manifest_invalid") from exc
    if not isinstance(manifest, dict):
        raise ConsumerFailure("manifest_invalid")
    archive = artifact_dir / f"{archive_base}.tar.gz"
    return manifest, archive


def extract_binaries(archive: Path, destination: Path) -> None:
    try:
        with tarfile.open(archive, mode="r:gz") as bundle:
            members = bundle.getmembers()
            observed: set[str] = set()
            for member in members:
                raw = PurePosixPath(member.name)
                normalized = PurePosixPath(*raw.parts)
                name = str(normalized)
                if name in {".", "./"} and member.isdir():
                    continue
                if (
                    raw.is_absolute()
                    or ".." in raw.parts
                    or not member.isfile()
                    or name not in EXPECTED_BINARIES
                    or name in observed
                ):
                    raise ConsumerFailure("archive_member_rejected")
                source = bundle.extractfile(member)
                if source is None:
                    raise ConsumerFailure("archive_member_unreadable")
                target = destination / name
                with target.open("wb") as output:
                    shutil.copyfileobj(source, output, length=1024 * 1024)
                target.chmod(0o755)
                observed.add(name)
            if observed != EXPECTED_BINARIES:
                raise ConsumerFailure("archive_binary_inventory_mismatch")
    except (OSError, tarfile.TarError) as exc:
        raise ConsumerFailure("archive_invalid") from exc


def verify_and_consume() -> dict[str, Any]:
    artifact_dir = Path(required_env("ARTIFACT_DIR")).resolve()
    runner_temp = Path(required_env("RUNNER_TEMP")).resolve()
    source_sha = required_env("EXPECTED_SOURCE_SHA")
    workflow_sha = required_env("EXPECTED_WORKFLOW_SHA")
    preview_version = required_env("EXPECTED_PREVIEW_VERSION")
    target = required_env("EXPECTED_TARGET")
    display_ref = required_env("EXPECTED_REF")
    run_id = required_env("GITHUB_RUN_ID")
    repository = required_env("GITHUB_REPOSITORY")
    server_url = required_env("GITHUB_SERVER_URL").rstrip("/")
    if not re.fullmatch(r"[0-9a-f]{40}", source_sha) or not re.fullmatch(
        r"[0-9a-f]{40}", workflow_sha
    ):
        raise ConsumerFailure("commit_identity_invalid")
    trusted_root = Path(__file__).resolve().parents[2]
    host = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=trusted_root,
        text=True,
        capture_output=True,
        check=False,
    )
    if host.returncode != 0 or host.stdout.strip() != workflow_sha:
        raise ConsumerFailure("trusted_workflow_identity_mismatch")
    if target not in TARGET_MACHINES or platform.machine() != TARGET_MACHINES[target]:
        raise ConsumerFailure("runner_architecture_mismatch")
    source_short = source_sha[:8]
    if not VERSION_RE.fullmatch(preview_version) or not preview_version.endswith(
        f"+g{source_short}"
    ):
        raise ConsumerFailure("preview_version_invalid")

    archive_base = f"codex-sedna-preview-{preview_version.replace('+', '__')}-{target}"
    manifest, archive = read_manifest(artifact_dir, archive_base)
    expected_workflow = f"{server_url}/{repository}/actions/runs/{run_id}"
    if (
        manifest.get("repository") != repository
        or manifest.get("commit") != source_sha
        or manifest.get("workflowCommit") != workflow_sha
        or manifest.get("ref") != display_ref
        or manifest.get("target") != target
        or manifest.get("previewVersion") != preview_version
        or manifest.get("workflow") != expected_workflow
    ):
        raise ConsumerFailure("package_provenance_mismatch")

    binaries = manifest.get("binaries")
    archive_digest = manifest.get("archiveSha256")
    if (
        not isinstance(binaries, dict)
        or set(binaries) != EXPECTED_BINARIES
        or not isinstance(archive_digest, str)
        or not SHA256_RE.fullmatch(archive_digest)
    ):
        raise ConsumerFailure("manifest_inventory_invalid")
    if sha256_file(archive) != archive_digest:
        raise ConsumerFailure("archive_digest_mismatch")
    verify_sidecar(
        artifact_dir / f"{archive_base}.tar.gz.sha256",
        f"{archive_base}.tar.gz",
        archive_digest,
    )
    for name in EXPECTED_BINARIES:
        digest = binaries.get(name)
        if not isinstance(digest, str) or not SHA256_RE.fullmatch(digest):
            raise ConsumerFailure("binary_digest_invalid")
        verify_sidecar(
            artifact_dir / f"{archive_base}.{name}.sha256", name, digest
        )

    with tempfile.TemporaryDirectory(dir=runner_temp, prefix="sedna-package-consumer-") as temporary:
        package_dir = Path(temporary)
        extract_binaries(archive, package_dir)
        for name in EXPECTED_BINARIES:
            if sha256_file(package_dir / name) != binaries[name]:
                raise ConsumerFailure("binary_digest_mismatch")

        version = command_ok([str(package_dir / "codex"), "--version"], cwd=package_dir)
        if version.returncode != 0:
            raise ConsumerFailure("packaged_version_command_failed")
        version_line = version.stdout.strip()
        match = VERSION_OUTPUT_RE.fullmatch(version_line)
        if (
            match is None
            or match.group(1) != preview_version
            or match.group(2) != source_short
        ):
            raise ConsumerFailure("packaged_version_identity_mismatch")

        device_help = command_ok(
            [str(package_dir / "codex"), "mcp", "login", "--help"], cwd=package_dir
        )
        if (
            device_help.returncode != 0
            or "--device-auth" not in device_help.stdout + device_help.stderr
        ):
            raise ConsumerFailure("packaged_device_auth_help_missing")
        for helper in ("codex-code-mode-host", "codex-responses-api-proxy"):
            result = command_ok([str(package_dir / helper), "--help"], cwd=package_dir)
            if result.returncode != 0:
                raise ConsumerFailure("packaged_helper_help_failed")

    return {
        "status": "success",
        "failure_code": "",
        "source_sha": source_sha,
        "workflow_sha": workflow_sha,
        "run_id": run_id,
        "target": target,
        "archive_sha256": archive_digest,
        "version_identity": "matched",
        "device_auth_help": "present",
        "packaged_helpers": sorted(EXPECTED_BINARIES - {"codex"}),
    }


def main() -> int:
    result_path = Path(required_env("RESULT_PATH"))
    try:
        result = verify_and_consume()
    except ConsumerFailure as exc:
        result = {"status": "failure", "failure_code": exc.code}
    except Exception:
        result = {"status": "failure", "failure_code": "consumer_verifier_error"}
    result_path.write_text(json.dumps(result, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(result, sort_keys=True))
    return 0 if result["status"] == "success" else 1


if __name__ == "__main__":
    raise SystemExit(main())
