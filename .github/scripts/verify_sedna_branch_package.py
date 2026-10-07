#!/usr/bin/env python3
"""Consume one exact Linux Sedna preview package without profile activation."""

from __future__ import annotations

import gzip
import hashlib
import json
import os
import platform
import re
import selectors
import signal
import subprocess
import tarfile
import tempfile
import time
from pathlib import Path, PurePosixPath
from typing import Any


EXPECTED_BINARIES = {
    "codex",
    "codex-code-mode-host",
    "codex-responses-api-proxy",
}
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
MAX_MANIFEST_BYTES = 64 * 1024
MAX_SIDECAR_BYTES = 256
MAX_ARCHIVE_BYTES = 2 * 1024 * 1024 * 1024
MAX_ARCHIVE_MEMBERS = 4  # Three binaries, plus an optional root directory.
MAX_BINARY_BYTES = 1024 * 1024 * 1024
MAX_TOTAL_EXTRACTED_BYTES = 2 * 1024 * 1024 * 1024
TAR_BLOCK_BYTES = 512
MAX_ARCHIVE_EXPANDED_BYTES = MAX_TOTAL_EXTRACTED_BYTES + (
    MAX_ARCHIVE_MEMBERS + 20
) * TAR_BLOCK_BYTES
MAX_COMMAND_OUTPUT_BYTES = 1024 * 1024
MAX_COMMAND_RUNTIME_SECONDS = 45
COMMAND_READ_CHUNK_BYTES = 64 * 1024
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


def valid_preview_version(preview_version: str, source_sha: str) -> bool:
    if not re.fullmatch(r"[0-9a-f]{40}", source_sha):
        return False
    source_short = source_sha[:8]
    if preview_version == f"0.0.0-dev.sedna.g{source_short}":
        return True
    return bool(
        VERSION_RE.fullmatch(preview_version)
        and preview_version.endswith(f"+g{source_short}")
    )


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
    process: subprocess.Popen[bytes] | None = None
    selector = selectors.DefaultSelector()
    output = {"stdout": bytearray(), "stderr": bytearray()}
    output_bytes = 0
    deadline = time.monotonic() + MAX_COMMAND_RUNTIME_SECONDS
    try:
        process = subprocess.Popen(
            arguments,
            cwd=cwd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        assert process.stdout is not None and process.stderr is not None
        selector.register(process.stdout, selectors.EVENT_READ, "stdout")
        selector.register(process.stderr, selectors.EVENT_READ, "stderr")
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ConsumerFailure("packaged_command_timeout")
            for key, _ in selector.select(min(remaining, 0.25)):
                chunk = os.read(key.fileobj.fileno(), COMMAND_READ_CHUNK_BYTES)
                if not chunk:
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
                    continue
                output_bytes += len(chunk)
                if output_bytes > MAX_COMMAND_OUTPUT_BYTES:
                    raise ConsumerFailure("packaged_command_output_limit")
                output[key.data].extend(chunk)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ConsumerFailure("packaged_command_timeout")
        try:
            return_code = process.wait(timeout=remaining)
        except subprocess.TimeoutExpired as exc:
            raise ConsumerFailure("packaged_command_timeout") from exc
        return subprocess.CompletedProcess(
            arguments,
            return_code,
            output["stdout"].decode("utf-8", errors="replace"),
            output["stderr"].decode("utf-8", errors="replace"),
        )
    finally:
        selector.close()
        if process is not None:
            try:
                # The leader may exit while a descendant remains in its
                # process group, including after closing inherited pipes.
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except OSError:
                if process.poll() is None:
                    process.kill()
            if process.poll() is None:
                process.wait()
            for stream in (process.stdout, process.stderr):
                if stream is not None and not stream.closed:
                    stream.close()


def verify_sidecar(path: Path, expected_name: str, expected_digest: str) -> None:
    try:
        if path.stat().st_size > MAX_SIDECAR_BYTES:
            raise ConsumerFailure("checksum_sidecar_invalid")
        fields = path.read_text(encoding="ascii").strip().split()
    except ConsumerFailure:
        raise
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
        entries: dict[str, Path] = {}
        for entry in artifact_dir.iterdir():
            if len(entries) >= len(expected_names):
                raise ConsumerFailure("artifact_inventory_mismatch")
            entries[entry.name] = entry
    except OSError as exc:
        raise ConsumerFailure("artifact_directory_unreadable") from exc
    if (
        set(entries) != expected_names
        or any(entry.is_symlink() or not entry.is_file() for entry in entries.values())
    ):
        raise ConsumerFailure("artifact_inventory_mismatch")
    try:
        manifest_path = artifact_dir / f"{archive_base}.json"
        if manifest_path.stat().st_size > MAX_MANIFEST_BYTES:
            raise ConsumerFailure("manifest_invalid")
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except ConsumerFailure:
        raise
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ConsumerFailure("manifest_invalid") from exc
    if not isinstance(manifest, dict):
        raise ConsumerFailure("manifest_invalid")
    archive = artifact_dir / f"{archive_base}.tar.gz"
    return manifest, archive


def _tar_octal(field: bytes) -> int:
    value = field.strip(b"\x00 ")
    if not value:
        return 0
    if any(byte < ord("0") or byte > ord("7") for byte in value):
        raise ConsumerFailure("archive_header_invalid")
    return int(value, 8)


def validate_tar_headers(archive: Path) -> None:
    """Reject extensions before tarfile can materialize their declared payloads."""

    expanded_bytes = 0
    member_count = 0
    end_marker_seen = False
    end_marker_blocks = 0
    try:
        with gzip.open(archive, "rb") as stream:
            while True:
                header = stream.read(TAR_BLOCK_BYTES)
                if not header:
                    break
                if len(header) != TAR_BLOCK_BYTES:
                    raise ConsumerFailure("archive_header_invalid")
                expanded_bytes += TAR_BLOCK_BYTES
                if expanded_bytes > MAX_ARCHIVE_EXPANDED_BYTES:
                    raise ConsumerFailure("archive_expansion_limit")
                if header == bytes(TAR_BLOCK_BYTES):
                    end_marker_seen = True
                    end_marker_blocks += 1
                    continue
                if end_marker_seen:
                    raise ConsumerFailure("archive_header_invalid")

                checksum = _tar_octal(header[148:156])
                actual_checksum = (
                    sum(header[:148])
                    + (8 * ord(" "))
                    + sum(header[156:])
                )
                if checksum != actual_checksum:
                    raise ConsumerFailure("archive_header_invalid")

                # This package format needs only regular files and the root
                # directory. Reject PAX/GNU extensions before tarfile parses
                # their potentially huge declared header payloads.
                typeflag = header[156:157]
                if typeflag not in (b"\x00", b"0", b"5"):
                    raise ConsumerFailure("archive_header_extension_rejected")
                member_count += 1
                if member_count > MAX_ARCHIVE_MEMBERS:
                    raise ConsumerFailure("archive_member_limit")

                size = _tar_octal(header[124:136])
                if typeflag == b"5" and size != 0:
                    raise ConsumerFailure("archive_header_invalid")
                if size > MAX_BINARY_BYTES or size > MAX_TOTAL_EXTRACTED_BYTES:
                    raise ConsumerFailure("archive_expansion_limit")
                padded_size = (
                    (size + TAR_BLOCK_BYTES - 1) // TAR_BLOCK_BYTES
                ) * TAR_BLOCK_BYTES
                if expanded_bytes + padded_size > MAX_ARCHIVE_EXPANDED_BYTES:
                    raise ConsumerFailure("archive_expansion_limit")
                remaining = padded_size
                while remaining:
                    block = stream.read(min(64 * 1024, remaining))
                    if not block:
                        raise ConsumerFailure("archive_header_invalid")
                    expanded_bytes += len(block)
                    remaining -= len(block)
    except ConsumerFailure:
        raise
    except (OSError, EOFError) as exc:
        raise ConsumerFailure("archive_invalid") from exc
    if end_marker_blocks < 2:
        raise ConsumerFailure("archive_header_invalid")


def extract_binaries(archive: Path, destination: Path) -> None:
    try:
        if archive.stat().st_size > MAX_ARCHIVE_BYTES:
            raise ConsumerFailure("archive_size_limit")
        validate_tar_headers(archive)
        with tarfile.open(archive, mode="r|gz") as bundle:
            observed: set[str] = set()
            member_count = 0
            total_bytes = 0
            root_directory_seen = False
            for member in bundle:
                member_count += 1
                if member_count > MAX_ARCHIVE_MEMBERS:
                    raise ConsumerFailure("archive_member_limit")
                raw = PurePosixPath(member.name)
                normalized = PurePosixPath(*raw.parts)
                name = str(normalized)
                if name == "." and member.isdir() and not root_directory_seen:
                    root_directory_seen = True
                    continue
                if (
                    raw.is_absolute()
                    or ".." in raw.parts
                    or not member.isfile()
                    or name not in EXPECTED_BINARIES
                    or name in observed
                ):
                    raise ConsumerFailure("archive_member_rejected")
                if (
                    member.size < 0
                    or member.size > MAX_BINARY_BYTES
                    or total_bytes + member.size > MAX_TOTAL_EXTRACTED_BYTES
                ):
                    raise ConsumerFailure("archive_expansion_limit")
                source = bundle.extractfile(member)
                if source is None:
                    raise ConsumerFailure("archive_member_unreadable")
                target = destination / name
                copied = 0
                with target.open("wb") as output:
                    while copied < member.size:
                        block = source.read(min(1024 * 1024, member.size - copied))
                        if not block:
                            raise ConsumerFailure("archive_member_size_mismatch")
                        output.write(block)
                        copied += len(block)
                if copied != member.size:
                    raise ConsumerFailure("archive_member_size_mismatch")
                target.chmod(0o755)
                observed.add(name)
                total_bytes += copied
            if observed != EXPECTED_BINARIES:
                raise ConsumerFailure("archive_binary_inventory_mismatch")
    except ConsumerFailure:
        raise
    except (OSError, tarfile.TarError) as exc:
        raise ConsumerFailure("archive_invalid") from exc


def verify_and_consume() -> dict[str, Any]:
    artifact_dir = Path(required_env("ARTIFACT_DIR")).resolve()
    runner_temp = Path(required_env("RUNNER_TEMP")).resolve()
    source_sha = required_env("EXPECTED_SOURCE_SHA")
    workflow_sha = required_env("EXPECTED_WORKFLOW_SHA")
    preview_version = required_env("EXPECTED_PREVIEW_VERSION")
    target = required_env("EXPECTED_TARGET")
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
    if not valid_preview_version(preview_version, source_sha):
        raise ConsumerFailure("preview_version_invalid")

    archive_base = f"codex-sedna-preview-{preview_version.replace('+', '__')}-{target}"
    manifest, archive = read_manifest(artifact_dir, archive_base)
    expected_workflow = f"{server_url}/{repository}/actions/runs/{run_id}"
    if (
        manifest.get("repository") != repository
        or manifest.get("commit") != source_sha
        or manifest.get("workflowCommit") != workflow_sha
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
    try:
        if archive.stat().st_size > MAX_ARCHIVE_BYTES:
            raise ConsumerFailure("archive_size_limit")
    except OSError as exc:
        raise ConsumerFailure("archive_invalid") from exc
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
