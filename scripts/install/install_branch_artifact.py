#!/usr/bin/env python3
"""Install one exact Sedna Linux preview artifact beside, not over, Codex.

This intentionally does not modify ~/.local/bin, CODEX_HOME, profiles, auth, or
state. The resulting binary must be invoked by its printed absolute path.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from typing import Any, BinaryIO
from urllib.parse import quote


WORKFLOW_PATH = ".github/workflows/sedna-branch-build.yml"
BINARIES = ("codex", "codex-code-mode-host", "codex-responses-api-proxy")
MAX_ARTIFACT_BYTES = 2 * 1024**3
MAX_MANIFEST_BYTES = 64 * 1024
MAX_COMPRESSED_TAR_BYTES = 1024**3
MAX_BINARY_BYTES = 1024**3
MAX_TOTAL_BINARY_BYTES = 2 * 1024**3
MIN_INSTALL_FREE_BYTES = 256 * 1024**2
MIN_TEMP_FREE_BYTES = 128 * 1024**2
PLATFORMS = {
    "linux-x86_64": ("x86_64-unknown-linux-gnu", 62),
    "linux-aarch64": ("aarch64-unknown-linux-gnu", 183),
}
DISPLAY_TITLE = re.compile(
    r"^sedna-branch-build \[target=(?P<ref>.+?) "
    r"platform=(?P<platform>linux-x86_64|linux-aarch64) "
    r"host_only=(?P<host_only>true|false)\]$"
)
SHA256 = re.compile(r"^[0-9a-f]{64}$")
GIT_SHA = re.compile(r"^[0-9a-f]{40}$")


class Refusal(RuntimeError):
    """The selected run or payload did not satisfy the preview contract."""


def gh_json(endpoint: str) -> dict[str, Any]:
    try:
        result = subprocess.run(
            ["gh", "api", endpoint], check=False, capture_output=True, text=True
        )
    except OSError as exc:
        raise Refusal(
            "The GitHub CLI (gh) is required to verify preview artifacts"
        ) from exc
    if result.returncode != 0:
        detail = result.stderr.strip() or f"gh api exited {result.returncode}"
        raise Refusal(f"GitHub API request failed: {detail}")
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise Refusal(f"GitHub API returned invalid JSON for {endpoint}") from exc
    if not isinstance(value, dict):
        raise Refusal(f"GitHub API returned an unexpected response for {endpoint}")
    return value


def repository_name() -> str:
    try:
        result = subprocess.run(
            [
                "gh",
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "--jq",
                ".nameWithOwner",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as exc:
        raise Refusal(
            "The GitHub CLI (gh) is required to verify preview artifacts"
        ) from exc
    if result.returncode != 0:
        detail = result.stderr.strip() or "gh repo view failed"
        raise Refusal(f"Cannot identify the current GitHub repository: {detail}")
    name = result.stdout.strip()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", name):
        raise Refusal(
            "The current checkout has no valid GitHub owner/repository identity"
        )
    return name


def parse_run_title(run: dict[str, Any]) -> tuple[str, str, bool]:
    match = DISPLAY_TITLE.fullmatch(str(run.get("display_title", "")))
    if not match:
        raise Refusal("The workflow run is not a recognized Linux preview build")
    return (
        match.group("ref"),
        match.group("platform"),
        match.group("host_only") == "true",
    )


def validate_run(
    run: dict[str, Any],
    *,
    repository: dict[str, Any],
    workflow_id: int,
    expected_ref: str | None,
    expected_target: str | None,
) -> tuple[str, str, str]:
    if run.get("path") != WORKFLOW_PATH or run.get("workflow_id") != workflow_id:
        raise Refusal(
            "The run was not produced by the protected branch-preview workflow"
        )
    if run.get("event") != "workflow_dispatch":
        raise Refusal("Only an explicitly dispatched branch-preview run is accepted")
    if run.get("status") != "completed" or run.get("conclusion") != "success":
        raise Refusal(
            "The selected branch-preview workflow run did not complete successfully"
        )
    if run.get("head_branch") != repository.get("default_branch"):
        raise Refusal(
            "The workflow host is not the repository's protected default branch"
        )
    host_sha = str(run.get("head_sha", ""))
    if not GIT_SHA.fullmatch(host_sha):
        raise Refusal("The workflow-host commit identity is missing or invalid")

    source_ref, platform_name, host_only = parse_run_title(run)
    if host_only:
        raise Refusal(
            "Host-only artifacts are incomplete and cannot be installed as Codex"
        )
    if expected_ref is not None and source_ref != expected_ref:
        raise Refusal("The workflow run selected a different source ref")
    if expected_target is not None and platform_name != expected_target:
        raise Refusal("The workflow run targets a different platform")
    target, _machine = PLATFORMS[platform_name]
    return source_ref, platform_name, target


def get_workflow_id(repository_name_value: str) -> int:
    workflow = gh_json(
        f"repos/{repository_name_value}/actions/workflows/sedna-branch-build.yml"
    )
    if workflow.get("path") != WORKFLOW_PATH or not isinstance(workflow.get("id"), int):
        raise Refusal("The expected branch-preview workflow is unavailable")
    return int(workflow["id"])


def validate_hosted_jobs(
    repository_name_value: str, run: dict[str, Any], platform_name: str
) -> None:
    run_id = run.get("id")
    attempt = run.get("run_attempt")
    if not isinstance(run_id, int) or not isinstance(attempt, int):
        raise Refusal("The selected workflow run has no valid attempt identity")
    jobs = gh_json(
        f"repos/{repository_name_value}/actions/runs/{run_id}/jobs?per_page=100"
    ).get("jobs", [])
    if not isinstance(jobs, list):
        raise Refusal("The workflow run has no readable job records")
    runner_label = {
        "linux-x86_64": "ubuntu-24.04",
        "linux-aarch64": "ubuntu-24.04-arm",
    }[platform_name]
    expected_build_name = {
        "linux-x86_64": "build-linux-x86_64 / Branch build",
        "linux-aarch64": "build-linux-aarch64 / Branch build",
    }[platform_name]
    for job_name, required_label in (
        ("Resolve preview metadata", "ubuntu-24.04"),
        (expected_build_name, runner_label),
    ):
        matches = [
            job
            for job in jobs
            if isinstance(job, dict) and job.get("name") == job_name
        ]
        if len(matches) != 1:
            raise Refusal(f"The successful run is missing its exact {job_name} job")
        job = matches[0]
        labels = job.get("labels", [])
        if (
            job.get("run_attempt") != attempt
            or job.get("status") != "completed"
            or job.get("conclusion") != "success"
            or job.get("runner_group_name") != "GitHub Actions"
            or not str(job.get("runner_name", "")).startswith("GitHub Actions ")
            or not isinstance(labels, list)
            or required_label not in labels
        ):
            raise Refusal(
                f"The {job_name} job did not pass on the required "
                "standard hosted runner"
            )


def select_run_by_id(
    repository_name_value: str, run_id: int
) -> dict[str, Any]:
    return gh_json(f"repos/{repository_name_value}/actions/runs/{run_id}")


def resolve_branch_sha(repository_name_value: str, branch: str) -> str:
    if not branch or branch.startswith("-") or "\0" in branch:
        raise Refusal("A valid branch name is required")
    encoded_branch = quote(branch, safe="/")
    ref = gh_json(
        f"repos/{repository_name_value}/git/ref/heads/{encoded_branch}"
    )
    sha = str(ref.get("object", {}).get("sha", ""))
    if not GIT_SHA.fullmatch(sha):
        raise Refusal(
            "GitHub did not resolve the requested branch to a full commit SHA"
        )
    return sha


def select_run_by_branch(
    repository_name_value: str,
    branch: str,
    source_sha: str,
    workflow_id: int,
    repository: dict[str, Any],
    platform_name: str,
) -> dict[str, Any]:
    short_sha = source_sha[:8]
    endpoint = (
        f"repos/{repository_name_value}/actions/workflows/sedna-branch-build.yml/runs"
    )
    for page in range(1, 11):
        listing = gh_json(
            f"{endpoint}?event=workflow_dispatch&per_page=100&page={page}"
        )
        runs = listing.get("workflow_runs", [])
        if not isinstance(runs, list) or not runs:
            break
        for candidate in runs:
            if not isinstance(candidate, dict):
                continue
            try:
                source_ref, candidate_platform, host_only = parse_run_title(candidate)
            except Refusal:
                continue
            if (
                source_ref != branch
                or candidate_platform != platform_name
                or host_only
                or candidate.get("path") != WORKFLOW_PATH
                or candidate.get("workflow_id") != workflow_id
                or candidate.get("event") != "workflow_dispatch"
                or candidate.get("status") != "completed"
                or candidate.get("conclusion") != "success"
                or candidate.get("head_branch") != repository.get("default_branch")
            ):
                continue
            run_id = candidate.get("id")
            if not isinstance(run_id, int):
                continue
            artifacts = gh_json(
                f"repos/{repository_name_value}/actions/runs/{run_id}/artifacts"
            ).get("artifacts", [])
            if not isinstance(artifacts, list):
                continue
            suffix = f"-{short_sha}-{PLATFORMS[platform_name][0]}"
            if any(
                isinstance(item, dict)
                and str(item.get("name", "")).startswith("sedna-branch-")
                and str(item.get("name", "")).endswith(suffix)
                and not item.get("expired", True)
                for item in artifacts
            ):
                return select_run_by_id(repository_name_value, run_id)
        if len(runs) < 100:
            break
    raise Refusal(
        f"No successful non-host-only preview exists for {branch} at {source_sha} "
        f"on {platform_name}; build one with the protected sedna-branch-build workflow"
    )


def select_artifact(
    repository_name_value: str,
    run: dict[str, Any],
    source_sha: str | None,
    target: str,
    repository: dict[str, Any],
) -> dict[str, Any]:
    run_id = run.get("id")
    if not isinstance(run_id, int):
        raise Refusal("The selected workflow run has no valid run ID")
    artifacts = gh_json(
        f"repos/{repository_name_value}/actions/runs/{run_id}/artifacts"
    ).get("artifacts", [])
    suffix = f"-{source_sha[:8]}-{target}" if source_sha else f"-{target}"
    matches = [
        item
        for item in artifacts
        if isinstance(item, dict)
        and str(item.get("name", "")).startswith("sedna-branch-")
        and str(item.get("name", "")).endswith(suffix)
    ]
    if len(matches) != 1:
        raise Refusal(
            "The run does not contain exactly one matching full Codex artifact"
        )
    artifact = matches[0]
    workflow_run = artifact.get("workflow_run", {})
    if not isinstance(workflow_run, dict):
        raise Refusal("The artifact has no workflow-run identity")
    if (
        artifact.get("expired") is not False
        or artifact.get("size_in_bytes", 0) <= 0
        or workflow_run.get("id") != run_id
        or workflow_run.get("repository_id") != repository.get("id")
        or workflow_run.get("head_sha") != run.get("head_sha")
    ):
        raise Refusal(
            "The artifact metadata does not bind to the selected run and repository"
        )
    digest = str(artifact.get("digest", ""))
    if not digest.startswith("sha256:") or not SHA256.fullmatch(
        digest.removeprefix("sha256:")
    ):
        raise Refusal("The artifact has no verifiable SHA-256 digest")
    return artifact


def get_source_identity(repository_name_value: str, source_sha: str) -> tuple[str, str]:
    commit = gh_json(f"repos/{repository_name_value}/commits/{source_sha}")
    if commit.get("sha") != source_sha:
        raise Refusal("GitHub resolved the preview source to a different commit")
    tree_sha = str(commit.get("commit", {}).get("tree", {}).get("sha", ""))
    if not GIT_SHA.fullmatch(tree_sha):
        raise Refusal("The preview source tree identity is missing or invalid")
    return source_sha, tree_sha


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def expected_archive_names(preview_version: str, target: str) -> tuple[str, str]:
    file_version = preview_version.replace("+", "__")
    base = f"codex-sedna-preview-{file_version}-{target}"
    return f"{base}.tar.gz", f"{base}.json"


def validate_manifest(
    manifest: Any,
    *,
    repository_name_value: str,
    source_sha: str,
    source_ref: str,
    target: str,
    run: dict[str, Any],
) -> dict[str, Any]:
    if not isinstance(manifest, dict):
        raise Refusal("Preview metadata is not a JSON object")
    expected = {
        "repository": repository_name_value,
        "commit": source_sha,
        "ref": source_ref,
        "workflow": run.get("html_url"),
    }
    for field, value in expected.items():
        if not value or manifest.get(field) != value:
            raise Refusal(f"Preview metadata does not match the selected run ({field})")
    if manifest.get("target") not in (None, target):
        raise Refusal("Preview metadata targets a different architecture")
    preview_version = manifest.get("previewVersion")
    if not isinstance(preview_version, str):
        raise Refusal("Preview metadata has no version")
    run_number = run.get("run_number")
    expected_suffix = f"-ci.{run_number}+g{source_sha[:8]}"
    if not preview_version.endswith(expected_suffix):
        raise Refusal("Preview version does not bind to this run number and source SHA")
    if not GIT_SHA.fullmatch(str(run.get("head_sha", ""))):
        raise Refusal("Workflow-host SHA is missing or invalid")
    return manifest


def elf_machine(path: Path) -> int:
    with path.open("rb") as source:
        header = source.read(20)
    if len(header) < 20 or header[:4] != b"\x7fELF" or header[4] != 2 or header[5] != 1:
        raise Refusal(f"{path.name} is not a little-endian 64-bit ELF executable")
    return int.from_bytes(header[18:20], byteorder="little")


def safe_member_name(name: str) -> str:
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts:
        raise Refusal("Preview archive contains an unsafe path")
    parts = [part for part in path.parts if part not in ("", ".")]
    if not parts:
        return "."
    if len(parts) != 1:
        raise Refusal("Preview archive contains nested or unexpected paths")
    return parts[0]


def extract_binaries(
    source: BinaryIO,
    destination: Path,
    expected_machine: int,
    expected_sizes: dict[str, int] | None = None,
) -> None:
    seen: set[str] = set()
    total_size = 0
    try:
        archive = tarfile.open(fileobj=source, mode="r|gz")
    except (tarfile.TarError, OSError) as exc:
        raise Refusal("The preview payload is not a valid gzip tar archive") from exc
    try:
        with archive:
            for member in archive:
                name = safe_member_name(member.name)
                if name == "." and member.isdir():
                    continue
                if name not in BINARIES or not member.isfile() or name in seen:
                    raise Refusal(
                        "Preview archive is partial or contains unexpected entries"
                    )
                if member.size <= 0 or member.mode & 0o111 == 0:
                    raise Refusal(f"Preview binary {name} is empty or not executable")
                if member.size > MAX_BINARY_BYTES:
                    raise Refusal(f"Preview binary {name} exceeds the size limit")
                total_size += member.size
                if total_size > MAX_TOTAL_BINARY_BYTES:
                    raise Refusal("Preview binaries exceed the total size limit")
                if (
                    expected_sizes is not None
                    and expected_sizes.get(name) != member.size
                ):
                    raise Refusal("Preview archive changed after size preflight")
                input_file = archive.extractfile(member)
                if input_file is None:
                    raise Refusal(f"Preview binary {name} could not be read")
                output = destination / name
                with input_file, output.open("xb") as target_file:
                    copied = 0
                    while chunk := input_file.read(1024 * 1024):
                        copied += len(chunk)
                        if copied > member.size:
                            raise Refusal(
                                f"Preview binary {name} exceeds its header size"
                            )
                        target_file.write(chunk)
                    if copied != member.size:
                        raise Refusal(f"Preview binary {name} is truncated")
                output.chmod(0o755)
                if elf_machine(output) != expected_machine:
                    raise Refusal(
                        f"Preview binary {name} has the wrong target architecture"
                    )
                seen.add(name)
    except tarfile.TarError as exc:
        raise Refusal("Preview tar archive is corrupt or truncated") from exc
    if seen != set(BINARIES):
        missing = ", ".join(sorted(set(BINARIES) - seen))
        raise Refusal(f"Preview archive is incomplete; missing: {missing}")
    if expected_sizes is not None and seen != set(expected_sizes):
        raise Refusal("Preview archive changed after size preflight")


def inspect_preview_tar(source: BinaryIO) -> tuple[dict[str, int], int]:
    """Validate all tar headers and payload lengths before writing any binary."""
    seen: dict[str, int] = {}
    total_size = 0
    try:
        archive = tarfile.open(fileobj=source, mode="r|gz")
    except (tarfile.TarError, OSError) as exc:
        raise Refusal("The preview payload is not a valid gzip tar archive") from exc
    try:
        with archive:
            for member in archive:
                name = safe_member_name(member.name)
                if name == "." and member.isdir():
                    continue
                if name not in BINARIES or not member.isfile() or name in seen:
                    raise Refusal(
                        "Preview archive is partial or contains unexpected entries"
                    )
                if member.size <= 0 or member.mode & 0o111 == 0:
                    raise Refusal(f"Preview binary {name} is empty or not executable")
                if member.size > MAX_BINARY_BYTES:
                    raise Refusal(f"Preview binary {name} exceeds the size limit")
                total_size += member.size
                if total_size > MAX_TOTAL_BINARY_BYTES:
                    raise Refusal("Preview binaries exceed the total size limit")
                input_file = archive.extractfile(member)
                if input_file is None:
                    raise Refusal(f"Preview binary {name} could not be read")
                with input_file:
                    actual_size = 0
                    while chunk := input_file.read(1024 * 1024):
                        actual_size += len(chunk)
                        if actual_size > member.size:
                            raise Refusal(
                                f"Preview binary {name} exceeds its header size"
                            )
                    if actual_size != member.size:
                        raise Refusal(f"Preview binary {name} is truncated")
                seen[name] = member.size
    except tarfile.TarError as exc:
        raise Refusal("Preview tar archive is corrupt or truncated") from exc
    if set(seen) != set(BINARIES):
        missing = ", ".join(sorted(set(BINARIES) - set(seen)))
        raise Refusal(f"Preview archive is incomplete; missing: {missing}")
    return seen, total_size


def download_artifact_zip(
    repository: str, artifact: dict[str, Any], path: Path
) -> None:
    endpoint = f"repos/{repository}/actions/artifacts/{artifact['id']}/zip"
    try:
        with path.open("wb") as output:
            result = subprocess.run(
                ["gh", "api", endpoint],
                check=False,
                stdout=output,
                stderr=subprocess.PIPE,
            )
    except OSError as exc:
        raise Refusal(
            "The GitHub CLI (gh) is required to download the preview"
        ) from exc
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise Refusal(f"Could not download the exact preview artifact: {detail}")
    if path.stat().st_size != artifact.get("size_in_bytes"):
        raise Refusal("Downloaded artifact size differs from GitHub artifact metadata")
    if sha256_file(path) != str(artifact["digest"]).removeprefix("sha256:"):
        raise Refusal(
            "Downloaded artifact digest differs from GitHub artifact metadata"
        )


def unpack_preview_zip(
    zip_path: Path,
    bin_dir: Path,
    *,
    preview_version: str,
    target: str,
    repository: str,
    source_sha: str,
    source_ref: str,
    run: dict[str, Any],
) -> None:
    archive_name, manifest_name = expected_archive_names(preview_version, target)
    try:
        with zipfile.ZipFile(zip_path) as bundle:
            infos = bundle.infolist()
            names = [info.filename for info in infos]
            if sorted(names) != sorted((archive_name, manifest_name)):
                raise Refusal(
                    "The downloaded artifact is partial or has unexpected files"
                )
            if any(
                info.file_size < 0
                or info.is_dir()
                or (info.external_attr >> 16) & 0o170000 not in (0, stat.S_IFREG)
                for info in bundle.infolist()
            ):
                raise Refusal("The downloaded artifact has an invalid archive shape")
            archive_info = bundle.getinfo(archive_name)
            manifest_info = bundle.getinfo(manifest_name)
            if archive_info.file_size > MAX_COMPRESSED_TAR_BYTES:
                raise Refusal("The compressed preview payload exceeds the size limit")
            if manifest_info.file_size > MAX_MANIFEST_BYTES:
                raise Refusal("Preview metadata exceeds the size limit")
            manifest_bytes = bundle.read(manifest_name)
            try:
                manifest = json.loads(manifest_bytes)
            except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                raise Refusal("Preview metadata is malformed JSON") from exc
            validate_manifest(
                manifest,
                repository_name_value=repository,
                source_sha=source_sha,
                source_ref=source_ref,
                target=target,
                run=run,
            )
            machine = next(
                value[1] for value in PLATFORMS.values() if value[0] == target
            )
            with bundle.open(archive_name) as payload:
                expected_sizes, total_size = inspect_preview_tar(payload)
            free = shutil.disk_usage(bin_dir).free
            if free < total_size + MIN_INSTALL_FREE_BYTES:
                raise Refusal(
                    "Insufficient destination disk space for the complete preview"
                )
            with bundle.open(archive_name) as payload:
                extract_binaries(payload, bin_dir, machine, expected_sizes)
    except (zipfile.BadZipFile, KeyError, OSError) as exc:
        raise Refusal(
            "The downloaded preview artifact is not a valid ZIP bundle"
        ) from exc


def preview_root() -> Path:
    data_home = Path(os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local/share")))
    if not data_home.is_absolute():
        raise Refusal("XDG_DATA_HOME must be an absolute path")
    root = data_home / "codex" / "branch-previews"
    codex_home = Path(
        os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))
    ).expanduser()
    root_resolved = root.resolve(strict=False)
    codex_resolved = codex_home.resolve(strict=False)
    if (
        root_resolved == codex_resolved
        or codex_resolved in root_resolved.parents
        or root_resolved in codex_resolved.parents
    ):
        raise Refusal(
            "Preview binaries must not be installed inside CODEX_HOME or its state"
        )
    if not codex_home.is_absolute():
        raise Refusal("CODEX_HOME must be absolute before preview installation")
    return root


def install_preview(
    zip_path: Path,
    artifact: dict[str, Any],
    run: dict[str, Any],
    repository_name_value: str,
    source_ref: str,
    source_sha: str,
    source_tree: str,
    target: str,
    preview_version: str,
) -> Path:
    root = preview_root()
    source_parent = root / source_sha
    target_parent = root / source_sha / target
    destination = target_parent / f"run-{run['id']}"
    if destination.exists() or destination.is_symlink():
        raise Refusal(f"The exact preview destination already exists: {destination}")
    for ancestor in (
        root,
        root.parent,
        root.parent.parent,
        source_parent,
        target_parent,
    ):
        if ancestor.is_symlink():
            raise Refusal("Refusing to install through a symlinked preview path")
    target_parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    if root.is_symlink() or source_parent.is_symlink() or target_parent.is_symlink():
        raise Refusal("Refusing to install through a symlinked preview path")
    temp_path = Path(tempfile.mkdtemp(prefix=f".run-{run['id']}-", dir=target_parent))
    try:
        bin_dir = temp_path / "bin"
        bin_dir.mkdir(mode=0o700)
        unpack_preview_zip(
            zip_path,
            bin_dir,
            preview_version=preview_version,
            target=target,
            repository=repository_name_value,
            source_sha=source_sha,
            source_ref=source_ref,
            run=run,
        )
        receipt = {
            "repository": repository_name_value,
            "sourceRef": source_ref,
            "sourceCommit": source_sha,
            "sourceTree": source_tree,
            "target": target,
            "previewVersion": preview_version,
            "workflowPath": WORKFLOW_PATH,
            "workflowHostCommit": run["head_sha"],
            "workflowRunId": run["id"],
            "workflowRunUrl": run["html_url"],
            "artifactId": artifact["id"],
            "artifactName": artifact["name"],
            "artifactSha256": artifact["digest"].removeprefix("sha256:"),
            "previewOnly": True,
            "canonicalPackageAcceptance": False,
            "installedAt": datetime.now(timezone.utc).isoformat(),
        }
        (temp_path / "preview-install.json").write_text(
            json.dumps(receipt, indent=2) + "\n", encoding="utf-8"
        )
        os.rename(temp_path, destination)
    except Exception:
        if temp_path.exists():
            shutil.rmtree(temp_path)
        raise
    return destination


def default_platform() -> str:
    machine = platform.machine().lower()
    if machine in ("x86_64", "amd64"):
        return "linux-x86_64"
    if machine in ("aarch64", "arm64"):
        return "linux-aarch64"
    raise Refusal(f"Unsupported local architecture for preview selection: {machine}")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Stage an exact Linux preview beside, but do not activate it over, Codex."
        )
    )
    selection = parser.add_mutually_exclusive_group(required=True)
    selection.add_argument(
        "--run-id", type=int, help="exact successful preview workflow run ID"
    )
    selection.add_argument(
        "--branch",
        help="select a successful artifact for this branch's current SHA",
    )
    parser.add_argument(
        "--platform",
        choices=tuple(PLATFORMS),
        help=(
            "Linux target (defaults to this machine for --branch; "
            "inferred for --run-id)"
        ),
    )
    args = parser.parse_args(argv)
    if args.run_id is not None and args.run_id < 1:
        parser.error("--run-id must be a positive integer")
    if args.branch is not None and args.platform is None:
        args.platform = default_platform()
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        repository_name_value = repository_name()
        repository = gh_json(f"repos/{repository_name_value}")
        workflow_id = get_workflow_id(repository_name_value)

        if args.run_id is not None:
            run = select_run_by_id(repository_name_value, args.run_id)
            source_ref, platform_name, _target = validate_run(
                run,
                repository=repository,
                workflow_id=workflow_id,
                expected_ref=None,
                expected_target=args.platform,
            )
            if args.platform is not None and platform_name != args.platform:
                raise Refusal("The selected run does not target the requested platform")
        else:
            source_ref = args.branch
            platform_name = args.platform
            source_sha = resolve_branch_sha(repository_name_value, source_ref)
            run = select_run_by_branch(
                repository_name_value,
                source_ref,
                source_sha,
                workflow_id,
                repository,
                platform_name,
            )
            validate_run(
                run,
                repository=repository,
                workflow_id=workflow_id,
                expected_ref=source_ref,
                expected_target=platform_name,
            )

        source_ref_from_run, platform_from_run, target = validate_run(
            run,
            repository=repository,
            workflow_id=workflow_id,
            expected_ref=source_ref,
            expected_target=platform_name,
        )
        if source_ref_from_run != source_ref or platform_from_run != platform_name:
            raise Refusal("The selected run identity changed during validation")
        validate_hosted_jobs(repository_name_value, run, platform_name)
        artifact = select_artifact(
            repository_name_value,
            run,
            None if args.run_id is not None else source_sha,
            target,
            repository,
        )
        # Source SHA is taken from the artifact name only after the exact run has
        # been selected, then independently verified against its signed manifest.
        artifact_suffix = f"-{target}"
        artifact_stem = str(artifact["name"])
        if not artifact_stem.endswith(artifact_suffix):
            raise Refusal("Artifact target name does not match the selected run")
        source_short = artifact_stem[: -len(artifact_suffix)].rsplit("-", 1)[-1]
        if not re.fullmatch(r"[0-9a-f]{8}", source_short):
            raise Refusal("Artifact name has no valid source-SHA suffix")

        # --branch has a current full SHA; --run-id obtains it from the payload
        # manifest after downloading and then verifies the exact source commit.
        if args.branch is not None:
            source_sha = resolve_branch_sha(repository_name_value, source_ref)
            if source_sha[:8] != source_short:
                raise Refusal(
                    "The selected branch artifact is not for the branch's current SHA"
                )

        source_identity = None
        if args.branch is not None:
            source_identity = get_source_identity(repository_name_value, source_sha)

        artifact_size = int(artifact["size_in_bytes"])
        if artifact_size <= 0 or artifact_size > MAX_ARTIFACT_BYTES:
            raise Refusal("The selected preview artifact exceeds the size limit")
        with tempfile.TemporaryDirectory(prefix="codex-branch-preview-") as temp:
            temp_dir = Path(temp)
            free = shutil.disk_usage(temp_dir).free
            if free < artifact_size + MIN_TEMP_FREE_BYTES:
                raise Refusal(
                    "Insufficient temporary disk space to verify the artifact safely"
                )
            zip_path = temp_dir / "artifact.zip"
            download_artifact_zip(repository_name_value, artifact, zip_path)
            # Read only the manifest first to discover the full SHA for --run-id.
            with zipfile.ZipFile(zip_path) as bundle:
                manifest_names = [
                    info.filename
                    for info in bundle.infolist()
                    if info.filename.endswith(".json")
                ]
                if len(manifest_names) != 1:
                    raise Refusal(
                        "The preview artifact does not contain exactly one manifest"
                    )
                manifest_info = bundle.getinfo(manifest_names[0])
                if manifest_info.file_size > MAX_MANIFEST_BYTES:
                    raise Refusal("Preview metadata exceeds the size limit")
                try:
                    manifest = json.loads(bundle.read(manifest_names[0]))
                except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                    raise Refusal("Preview metadata is malformed JSON") from exc
            source_sha = (
                str(manifest.get("commit", ""))
                if isinstance(manifest, dict)
                else ""
            )
            if not GIT_SHA.fullmatch(source_sha) or source_sha[:8] != source_short:
                raise Refusal("Artifact name and manifest source SHA do not match")
            if args.branch is not None and source_sha != source_identity[0]:
                raise Refusal(
                    "The artifact source SHA differs from the selected branch head"
                )
            source_sha, source_tree = get_source_identity(
                repository_name_value, source_sha
            )
            validate_manifest(
                manifest,
                repository_name_value=repository_name_value,
                source_sha=source_sha,
                source_ref=source_ref,
                target=target,
                run=run,
            )
            if args.branch is not None:
                latest_branch_sha = resolve_branch_sha(
                    repository_name_value, source_ref
                )
                if latest_branch_sha != source_sha:
                    raise Refusal(
                        "The selected branch moved while the preview was being verified"
                    )
            destination = install_preview(
                zip_path,
                artifact,
                run,
                repository_name_value,
                source_ref,
                source_sha,
                source_tree,
                target,
                str(manifest["previewVersion"]),
            )

        print(
            "Staged verified preview only; the active Codex command and user state "
            "were not changed."
        )
        print(f"Source: {source_sha} (tree {source_tree}), ref {source_ref}")
        print(f"Target: {target}; workflow run: {run['id']} ({run['html_url']})")
        print(
            f"Artifact: {artifact['id']} "
            f"sha256:{artifact['digest'].removeprefix('sha256:')}"
        )
        print(f"Run explicitly with: {destination / 'bin' / 'codex'}")
        print(f"Rollback by removing only: {destination}")
        print("This is a branch preview, not canonical package or consumer acceptance.")
        return 0
    except Refusal as exc:
        print(f"Refusing branch preview: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
