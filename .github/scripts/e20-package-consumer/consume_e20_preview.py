#!/usr/bin/env python3
"""Verify exact first-package artifacts and exercise their packaged CLI.

This is a hosted-only, data-free compatibility consumer. It constructs a
declarative schema snapshot from public source, seeds a synthetic thread and
the 67 source-verified historical migration identities, then proves first-open
and a second-process reopen of the packaged app-server without changing the
historical ledger. It does not replay the duplicate historical SQL directory
and never reads or uploads a user database.
"""

from __future__ import annotations

import hashlib
import json
import os
import posixpath
import re
import select
import shutil
import sqlite3
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


PRODUCT_SOURCE = "d99ff264d5e3b1c7bef1dce4d07324913cfe5c1f"
PRODUCT_TREE = "d03153e8d382c4825e2804a49b43b72811b869e7"
PRODUCT_SOURCE_BRANCH = "integration/be2e-e926-20261009"
PRODUCT_RUN_ID = "38675534571"
PRODUCT_WORKFLOW = ".github/workflows/sedna-first-package.yml"
WORKFLOW_HOST = "66e627b883f0c6cdf691c8b50ff5ac0fee14851e"
WORKFLOW_HOST_TREE = "d03153e8d382c4825e2804a49b43b72811b869e7"
PACKAGE_VERSION_BASE = "0.163.0-dev.sedna.1"
BASE_SHA = "703389678a1a6c3a9d131245d03d3f1805b9afa1"
PULL_REQUEST_BRANCH = "validation/w14962-e20-packaged-consumer-20261015"
MIGRATION_38E25 = "38e25e75711d323be7039a81bcf1c31aa1117583"
EXPECTED_APP_TABLES = {
    "projects",
    "thread_sections",
    "threads",
    "backfill_state",
    "thread_dynamic_tools",
    "thread_spawn_edges",
    "remote_control_enrollments",
    "external_agent_config_imports",
    "rollout_migration_state",
    "rollout_migration_skipped_rollouts",
    "project_roots",
    "project_idempotency_keys",
    "thread_attachments",
    "guardian_review_feedback",
    "phase2_attested_baselines",
    "phase2_attestation_roots",
}
PRODUCER_JOBS = {
    "x86_64": {
        "build_job_id": "116231434617",
        "build_job_name": "Build exact Linux package pair (x86_64-unknown-linux-gnu)",
        "consumer_job_id": "116245283101",
        "consumer_job_name": "Consume exact downloaded packages (x86_64-unknown-linux-gnu)",
        "artifact_id": "11948435472",
        "artifact_name": "sedna-first-package-d99ff264-x86_64-38675534571",
        "artifact_digest": "sha256:9f50a3522a5636cdb6a41f9fca06ca9d27d0a980686c185ab883af9ff795befe",
    },
    "aarch64": {
        "build_job_id": "116231434574",
        "build_job_name": "Build exact Linux package pair (aarch64-unknown-linux-gnu)",
        "consumer_job_id": "116245283179",
        "consumer_job_name": "Consume exact downloaded packages (aarch64-unknown-linux-gnu)",
        "artifact_id": "11948285961",
        "artifact_name": "sedna-first-package-d99ff264-aarch64-38675534571",
        "artifact_digest": "sha256:e3b89a594c0073a4dd2f96c4fd5e227cb8040af46f3217fa3fef15dcb66f2c25",
    },
}
EXPECTED_PACKAGE_RUN_NUMBER = 176
EXPECTED_RUNNERS = {
    "x86_64": "ubuntu-24.04",
    "aarch64": "ubuntu-24.04-arm",
}
# ThreadId parsing requires a UUID; keep this deterministic v7-shaped id test-only.
SYNTHETIC_THREAD_ID = "01900000-0000-7000-8000-000000000061"
MAX_CHILD_STDERR_BYTES = 16 * 1024
MAX_DIAGNOSTIC_CHARS = 4096
SQLITE_STAGE = "consumer startup"


def mark_sqlite_stage(stage: str) -> None:
    global SQLITE_STAGE
    SQLITE_STAGE = stage


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def env(name: str) -> str:
    value = os.environ.get(name, "")
    require(bool(value), f"Required workflow input is missing: {name}.")
    return value


def github_api(path: str) -> dict[str, Any]:
    token = env("GH_TOKEN")
    request = urllib.request.Request(
        f"https://api.github.com{path}",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.loads(response.read())
    except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError) as error:
        raise RuntimeError("GitHub artifact provenance lookup failed.") from error
    require(isinstance(payload, dict), "GitHub API returned an unexpected response.")
    return payload


def validate_pull_request_checkout() -> tuple[str, str]:
    require(env("GITHUB_EVENT_NAME") == "pull_request", "Only the scoped PR consumer is permitted.")
    event_path = Path(env("GITHUB_EVENT_PATH"))
    event = json.loads(event_path.read_text())
    pull = event.get("pull_request") or {}
    repo = env("GITHUB_REPOSITORY")
    head_repo = ((pull.get("head") or {}).get("repo") or {}).get("full_name")
    require(head_repo == repo, "The consumer requires a same-repository pull request.")
    require((pull.get("base") or {}).get("ref") == "main", "Unexpected pull-request base ref.")
    require((pull.get("base") or {}).get("sha") == BASE_SHA, "Protected main moved from the bound fixture base.")
    require((pull.get("head") or {}).get("ref") == PULL_REQUEST_BRANCH, "Unexpected consumer branch.")
    head_sha = (pull.get("head") or {}).get("sha")
    require(re.fullmatch(r"[0-9a-f]{40}", str(head_sha)) is not None, "Malformed PR source SHA.")
    actual_head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    require(actual_head == head_sha, "Consumer checkout does not match the exact PR source head.")
    workflow_merge_sha = env("GITHUB_SHA")
    return head_sha, workflow_merge_sha


def verify_producer(*, repo: str, run_id: str, architecture: str) -> dict[str, Any]:
    require(run_id == PRODUCT_RUN_ID, "Producer run does not match the exact integrated package run.")
    producer = PRODUCER_JOBS[architecture]
    run = github_api(f"/repos/{repo}/actions/runs/{run_id}")
    expected_run = {
        "id": int(PRODUCT_RUN_ID),
        "path": PRODUCT_WORKFLOW,
        "head_branch": PRODUCT_SOURCE_BRANCH,
        "head_sha": PRODUCT_SOURCE,
        "event": "pull_request",
        "status": "completed",
        # The package builds succeeded. The run is red only because its two
        # same-run rich consumers fail at the separately owned turn-start seam.
        "conclusion": "failure",
        "run_attempt": 1,
        "run_number": EXPECTED_PACKAGE_RUN_NUMBER,
    }
    for key, value in expected_run.items():
        require(run.get(key) == value, f"Producer run identity mismatch: {key}.")
    require(
        (run.get("head_repository") or {}).get("full_name") == repo,
        "Producer run was not created by the expected repository.",
    )
    pull_requests = run.get("pull_requests")
    require(isinstance(pull_requests, list) and len(pull_requests) == 1, "Producer run does not bind to exactly one pull request.")
    pull = pull_requests[0]
    require(pull.get("number") == 937, "Producer run is not associated with PR #937.")
    base = pull.get("base") or {}
    require(base.get("ref") == "main" and base.get("sha") == BASE_SHA, "Producer PR base does not match the selected exact main SHA.")
    head = pull.get("head") or {}
    require(head.get("ref") == PRODUCT_SOURCE_BRANCH, "Producer PR head branch mismatch.")
    require((head.get("repo") or {}).get("full_name") == repo, "Producer PR was not opened from the expected repository.")

    jobs = github_api(f"/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100").get("jobs", [])
    expected_failed_ids = {
        int(PRODUCER_JOBS[arch]["consumer_job_id"]) for arch in EXPECTED_RUNNERS
    }
    failed_ids = {candidate.get("id") for candidate in jobs if candidate.get("conclusion") == "failure"}
    require(failed_ids == expected_failed_ids, "Producer run has an unexpected failed-job set.")

    job_receipts: dict[str, dict[str, Any]] = {}
    for arch, expected in PRODUCER_JOBS.items():
        build_job = next((candidate for candidate in jobs if candidate.get("id") == int(expected["build_job_id"])), None)
        require(build_job is not None, f"Exact {arch} package build job is absent.")
        require(build_job.get("run_id") == int(run_id), f"{arch} package build job belongs to a different run.")
        require(build_job.get("name") == expected["build_job_name"], f"{arch} package build job name mismatch.")
        require(build_job.get("status") == "completed" and build_job.get("conclusion") == "success", f"Exact {arch} package build job did not succeed.")
        require(build_job.get("runner_group_name") == "GitHub Actions", f"{arch} package was not built on the standard GitHub-hosted runner group.")
        require(build_job.get("labels") == [EXPECTED_RUNNERS[arch]], f"{arch} package build runner label mismatch.")

        consumer_job = next((candidate for candidate in jobs if candidate.get("id") == int(expected["consumer_job_id"])), None)
        require(consumer_job is not None, f"Exact same-run {arch} package consumer receipt is absent.")
        require(consumer_job.get("name") == expected["consumer_job_name"], f"Same-run {arch} package consumer job name mismatch.")
        require(consumer_job.get("run_id") == int(run_id), f"Same-run {arch} package consumer belongs to a different run.")
        require(consumer_job.get("status") == "completed" and consumer_job.get("conclusion") == "failure", f"Expected same-run {arch} consumer failure is no longer present.")
        require(consumer_job.get("runner_group_name") == "GitHub Actions", f"Same-run {arch} consumer did not use the standard hosted runner group.")
        require(consumer_job.get("labels") == [EXPECTED_RUNNERS[arch]], f"Same-run {arch} consumer runner label mismatch.")
        job_receipts[arch] = {
            "build_job_id": expected["build_job_id"],
            "build_conclusion": build_job["conclusion"],
            "consumer_job_id": expected["consumer_job_id"],
            "consumer_conclusion": consumer_job["conclusion"],
            "runner_label": build_job["labels"][0],
        }

    artifacts = github_api(f"/repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100").get("artifacts", [])
    artifact_receipts: dict[str, dict[str, Any]] = {}
    for arch, expected in PRODUCER_JOBS.items():
        artifact = next((candidate for candidate in artifacts if candidate.get("id") == int(expected["artifact_id"])), None)
        require(artifact is not None, f"Exact {arch} producer artifact is absent.")
        require(artifact.get("name") == expected["artifact_name"], f"{arch} producer artifact name mismatch.")
        require(artifact.get("digest") == expected["artifact_digest"], f"{arch} producer artifact digest mismatch.")
        require(artifact.get("expired") is False, f"{arch} producer artifact is expired.")
        artifact_receipts[arch] = {
            "artifact_id": expected["artifact_id"],
            "artifact_name": expected["artifact_name"],
            "artifact_digest": expected["artifact_digest"],
        }
    return {
        "run_id": run_id,
        "producer_head_sha": run["head_sha"],
        "producer_event": run["event"],
        "producer_branch": run["head_branch"],
        "producer_run_conclusion": run["conclusion"],
        "producer_run_number": run["run_number"],
        "source_sha": PRODUCT_SOURCE,
        "source_tree": PRODUCT_TREE,
        "workflow_host_sha": WORKFLOW_HOST,
        "package_jobs": job_receipts,
        "package_artifacts": artifact_receipts,
        "artifact_id": producer["artifact_id"],
        "artifact_name": producer["artifact_name"],
        "artifact_digest": producer["artifact_digest"],
        "consumer_job_id": producer["consumer_job_id"],
        "consumer_conclusion": "failure",
    }


def read_bundle(artifact_dir: Path, producer_run_number: int, architecture: str) -> tuple[Path, dict[str, Any]]:
    entries = [path for path in artifact_dir.iterdir()]
    require(all(path.is_file() and not path.is_symlink() for path in entries), "Artifact bundle contains an unexpected directory or link.")
    target = f"{architecture}-unknown-linux-gnu"
    cli_archive_name = f"codex-cli-{target}.tar.gz"
    expected_files = {
        f"codex-app-server-{target}.tar.gz",
        cli_archive_name,
        "codex-responses-api-proxy",
        "release-manifest.json",
        "SHA256SUMS.txt",
        "symbols.tar.gz",
    }
    require({path.name for path in entries} == expected_files, "Artifact bundle shape is not the exact first-package payload.")
    manifest_path = artifact_dir / "release-manifest.json"
    manifest = json.loads(manifest_path.read_text())
    require(manifest.get("repository") == env("GITHUB_REPOSITORY"), "Package manifest repository mismatch.")
    require(manifest.get("sourceCommit") == PRODUCT_SOURCE, "Package manifest source commit mismatch.")
    require(manifest.get("sourceTree") == PRODUCT_TREE, "Package manifest source tree mismatch.")
    require(manifest.get("workflowHostCommit") == WORKFLOW_HOST, "Package manifest workflow-host commit mismatch.")
    require(manifest.get("workflowHostTree") == WORKFLOW_HOST_TREE, "Package manifest workflow-host tree mismatch.")
    run_id = PRODUCT_RUN_ID
    require(manifest.get("workflowRunId") == run_id, "Package manifest does not bind to its exact producer run.")
    workflow_url = manifest.get("workflow", "")
    require(workflow_url.endswith(f"/actions/runs/{run_id}"), "Package manifest workflow URL does not bind to its exact producer run.")
    require(manifest.get("target") == target, "Package manifest target mismatch.")
    version = manifest.get("packageVersion", "")
    require(
        version == f"{PACKAGE_VERSION_BASE}-ci.{producer_run_number}+g{PRODUCT_SOURCE[:8]}",
        "Package version does not match the exact producer run and source SHA.",
    )
    artifacts = manifest.get("artifacts")
    payload_names = expected_files - {"release-manifest.json", "SHA256SUMS.txt"}
    require(isinstance(artifacts, dict) and set(artifacts) == payload_names, "Package manifest payload hash list is incomplete or unexpected.")
    computed_hashes: dict[str, str] = {}
    for name in sorted(payload_names):
        hasher = hashlib.sha256()
        with (artifact_dir / name).open("rb") as package_file:
            for chunk in iter(lambda: package_file.read(1024 * 1024), b""):
                hasher.update(chunk)
        computed_hashes[name] = hasher.hexdigest()
    require(artifacts == computed_hashes, "Package manifest payload hashes do not match the downloaded files.")
    checksum_hashes: dict[str, str] = {}
    for line in (artifact_dir / "SHA256SUMS.txt").read_text().splitlines():
        digest, name = line.split("  ", 1)
        require(name not in checksum_hashes, "Package checksum file contains a duplicate path.")
        checksum_hashes[name] = digest
    manifest_digest = hashlib.sha256(manifest_path.read_bytes()).hexdigest()
    expected_checksums = {**computed_hashes, "release-manifest.json": manifest_digest}
    require(checksum_hashes == expected_checksums, "Package checksum file does not bind exactly the manifest and payloads.")

    archive_hasher = hashlib.sha256()
    archive_path = artifact_dir / cli_archive_name
    with archive_path.open("rb") as archive_stream:
        for chunk in iter(lambda: archive_stream.read(1024 * 1024), b""):
            archive_hasher.update(chunk)
    return archive_path, {
        "package_version": version,
        "archive_sha256": archive_hasher.hexdigest(),
        "package_artifact_hashes": computed_hashes,
        "workflow_host_sha": WORKFLOW_HOST,
        "workflow_host_tree": WORKFLOW_HOST_TREE,
    }


def extract_codex(archive: Path, destination: Path, architecture: str) -> Path:
    found: set[str] = set()
    seen_members: set[str] = set()
    allowed_directories = {"bin", "codex-resources", "codex-path"}
    codex_path = destination / "codex"
    with tarfile.open(archive, "r:gz") as package:
        for member in package.getmembers():
            raw_name = member.name
            require(not raw_name.startswith("/") and "\\" not in raw_name, "Package contains an absolute or platform-ambiguous path.")
            clean_name = posixpath.normpath(raw_name)
            clean_name = clean_name[2:] if clean_name.startswith("./") else clean_name
            require(clean_name != ".." and not clean_name.startswith("../"), "Package contains path traversal.")
            if clean_name in {".", ""}:
                require(member.isdir(), "Package root entry is not a directory.")
                continue
            require(clean_name not in seen_members, "Package contains a duplicate normalized path.")
            seen_members.add(clean_name)
            require(member.isfile() or member.isdir(), "Package contains a link or special file.")
            if member.isdir():
                require(
                    clean_name in allowed_directories
                    or clean_name.startswith("codex-resources/")
                    or clean_name.startswith("codex-path/"),
                    "Package contains an unrecognized directory.",
                )
                continue
            require(
                clean_name == "codex-package.json"
                or clean_name.startswith("bin/")
                or clean_name.startswith("codex-resources/")
                or clean_name.startswith("codex-path/"),
                "Package contains an unrecognized file path.",
            )
            found.add(clean_name)
            if clean_name == "bin/codex":
                require(member.isfile() and bool(member.mode & 0o111), "Packaged Codex entrypoint is not an executable file.")
                extracted = package.extractfile(member)
                require(extracted is not None, "Codex executable is unreadable in the package.")
                with extracted, codex_path.open("wb") as output:
                    shutil.copyfileobj(extracted, output, length=1024 * 1024)
                codex_path.chmod(0o755)
    require("bin/codex" in found, "Package is missing the CLI entrypoint.")
    with codex_path.open("rb") as binary:
        header = binary.read(20)
    require(len(header) == 20 and header[:4] == b"\x7fELF" and header[4] == 2 and header[5] == 1, "Packaged Codex binary is not little-endian ELF64.")
    machine = struct.unpack("<H", header[18:20])[0]
    expected_machine = {"x86_64": 62, "aarch64": 183}[architecture]
    require(machine == expected_machine, "Packaged Codex ELF architecture does not match the consumer runner.")
    return codex_path


class AppServer:
    def __init__(self, executable: Path, codex_home: Path, sqlite_home: Path):
        env_allow = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(codex_home),
            "CODEX_HOME": str(codex_home),
            "CODEX_SQLITE_HOME": str(sqlite_home),
            "CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG": "1",
        }
        self.process = subprocess.Popen(
            [str(executable), "app-server", "--listen", "stdio://"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env_allow,
            bufsize=0,
        )
        require(
            self.process.stdin is not None
            and self.process.stdout is not None
            and self.process.stderr is not None,
            "Could not open app-server protocol pipes.",
        )
        self.inp = self.process.stdin
        self.out = self.process.stdout
        self._stderr = self.process.stderr
        self._stderr_tail = bytearray()
        self._stderr_lock = threading.Lock()
        self._stderr_reader = threading.Thread(
            target=self._drain_stderr,
            args=(self._stderr.fileno(),),
            daemon=True,
        )
        self._stderr_reader.start()
        self._diagnostic_roots = self._collect_diagnostic_roots(codex_home)
        self.exit_status: int | None = None
        self.pending = bytearray()

    @staticmethod
    def _collect_diagnostic_roots(codex_home: Path) -> list[tuple[str, str]]:
        roots = [
            (str(codex_home.parent), "<temporary>"),
            (os.environ.get("GITHUB_WORKSPACE", ""), "<workspace>"),
            (os.environ.get("RUNNER_TEMP", ""), "<runner-temp>"),
        ]
        normalized = [(root.rstrip("/"), replacement) for root, replacement in roots if root.rstrip("/")]
        return sorted(
            normalized,
            key=lambda item: len(item[0]),
            reverse=True,
        )

    def _drain_stderr(self, fd: int) -> None:
        while True:
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                return
            if not chunk:
                return
            with self._stderr_lock:
                self._stderr_tail.extend(chunk)
                overflow = len(self._stderr_tail) - MAX_CHILD_STDERR_BYTES
                if overflow > 0:
                    del self._stderr_tail[:overflow]

    def _sanitize_diagnostic(self, value: str, limit: int = MAX_DIAGNOSTIC_CHARS) -> str:
        text = value
        for root, replacement in self._diagnostic_roots:
            text = text.replace(root, replacement)
        text = re.sub(
            r"(?i)\b(authorization|proxy-authorization)(\s*[:=]\s*)[^\r\n]*",
            r"\1\2<redacted>",
            text,
        )
        text = re.sub(
            r"(?i)\b([a-z0-9_]*token|[a-z0-9_]*secret|[a-z0-9_]*api[_-]?key|"
            r"[a-z0-9_]*password|[a-z0-9_]*cookie)"
            r"(\s*(?:[:=]\s*|\s+))(?:\"[^\"]*\"|'[^']*'|[^\s,;]+)",
            r"\1\2<redacted>",
            text,
        )
        text = re.sub(r"(?i)\bbearer\s+[^\s,;]+", "Bearer <redacted>", text)
        text = re.sub(r"https?://[^/\s:@]+:[^/\s@]+@", "https://<redacted>@", text)
        text = re.sub(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]", "?", text)
        return text[:limit].strip()

    def _stderr_text(self) -> str:
        with self._stderr_lock:
            raw = bytes(self._stderr_tail)
        return self._sanitize_diagnostic(raw.decode("utf-8", errors="replace"))

    def _rpc_failure_detail(self, error: "AppServerRpcError", historical_check: str) -> str:
        code = str(error.code) if isinstance(error.code, int) else "nonstandard"
        message = self._sanitize_diagnostic(error.message, 1024) or "<empty>"
        details = [
            f"Packaged app-server rejected {error.method} (rpc_{code}).",
            f"rpc_message={message}",
            historical_check,
        ]
        if self.exit_status is not None:
            details.append(f"child_exit_status={self.exit_status}")
        stderr = self._stderr_text()
        if stderr:
            details.append(f"child_stderr_tail={stderr}")
        return self._sanitize_diagnostic(" ".join(details))

    def send(self, value: dict[str, Any]) -> None:
        self.inp.write((json.dumps(value, separators=(",", ":")) + "\n").encode())
        self.inp.flush()

    def read_line(self, timeout: float) -> dict[str, Any]:
        deadline = time.monotonic() + timeout
        while True:
            newline = self.pending.find(b"\n")
            if newline >= 0:
                raw = bytes(self.pending[:newline])
                del self.pending[: newline + 1]
                if raw:
                    return json.loads(raw)
            remaining = deadline - time.monotonic()
            require(remaining > 0, "Packaged app-server protocol response timed out.")
            readable, _, _ = select.select([self.out], [], [], remaining)
            require(bool(readable), "Packaged app-server protocol response timed out.")
            chunk = os.read(self.out.fileno(), 65536)
            require(bool(chunk), "Packaged app-server closed stdout unexpectedly.")
            self.pending.extend(chunk)

    def request(self, method: str, request_id: int, params: dict[str, Any]) -> dict[str, Any]:
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            response = self.read_line(60)
            if response.get("id") == request_id:
                if "error" in response:
                    error = response["error"]
                    code = error.get("code") if isinstance(error, dict) else None
                    message = error.get("message", "") if isinstance(error, dict) else ""
                    raise AppServerRpcError(method, code, message)
                require("result" in response, f"Packaged app-server omitted {method} result.")
                return response["result"]

    def initialize_and_list(self, expected_thread_id: str) -> None:
        result = self.request(
            "initialize",
            1,
            {
                "clientInfo": {"name": "integrated-historical-state-consumer", "version": "1"},
                "capabilities": {"experimentalApi": True},
            },
        )
        require(isinstance(result, dict), "Packaged app-server initialize result has an unexpected shape.")
        self.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        result = self.request("thread/list", 2, {"limit": 10, "useStateDbOnly": True})
        data = result.get("data") if isinstance(result, dict) else None
        require(isinstance(data, list), "Packaged thread/list returned no data array.")
        require(any(isinstance(thread, dict) and thread.get("id") == expected_thread_id for thread in data), "Packaged thread/list did not read the synthetic historical thread.")

    def close(self) -> None:
        if not self.inp.closed:
            self.inp.close()
        timed_out = False
        status: int | None = None
        try:
            status = self.process.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.process.kill()
            status = self.process.wait(timeout=10)
            timed_out = True
        finally:
            self._stderr_reader.join(timeout=5)
            if not self._stderr.closed:
                self._stderr.close()
            if not self.out.closed:
                self.out.close()
        self.exit_status = status
        if timed_out:
            raise RuntimeError("Packaged app-server did not exit after stdin EOF.")
        require(status is not None, "Packaged app-server exit status was unavailable.")
        require(status == 0, f"Packaged app-server exited with status {status}.")


class AppServerRpcError(RuntimeError):
    def __init__(self, method: str, code: Any, message: Any):
        super().__init__(method)
        self.method = method
        self.code = code
        self.message = message if isinstance(message, str) else ""


def source_migrations(source_dir: Path) -> list[tuple[int, str, bytes]]:
    migration_dir = source_dir / "codex-rs/state/migrations"
    migrations: list[tuple[int, str, bytes]] = []
    for path in sorted(migration_dir.glob("*.sql")):
        match = re.fullmatch(r"([0-9]+)_([A-Za-z0-9_]+)\.sql", path.name)
        require(match is not None, "Historical migration filename has an unsupported form.")
        version = int(match.group(1))
        if version > 0:
            # SQLx stores the filename suffix with underscores normalized to
            # spaces in `_sqlx_migrations.description`.
            migrations.append((version, match.group(2).replace("_", " "), path.read_bytes()))
    require(len(migrations) == 67, "Historical public source does not contain exactly the 67 recorded migrations.")
    require(len({version for version, _, _ in migrations}) == 67, "Historical migration versions are not unique.")
    require(not any(version == 61 for version, _, _ in migrations), "Historical pre-61 source unexpectedly includes migration 61.")
    return migrations


def schema_snapshot_path() -> Path:
    path = Path(__file__).with_name("38e25-pre61-snapshot.sql")
    require(path.is_file(), "Public pre-61 schema snapshot is missing.")
    return path


def create_pre61_database(db_path: Path, snapshot_path: Path, migrations: list[tuple[int, str, bytes]], rollout_path: Path) -> list[tuple[Any, ...]]:
    mark_sqlite_stage("open synthetic pre-61 database")
    connection = sqlite3.connect(db_path)
    try:
        mark_sqlite_stage("create source-derived pre-61 schema")
        connection.executescript(snapshot_path.read_text())
        mark_sqlite_stage("verify source-derived schema table set")
        table_names = {
            row[0]
            for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        }
        require(table_names == EXPECTED_APP_TABLES, "Data-free historical schema snapshot has an unexpected table set.")
        mark_sqlite_stage("create migration ledger")
        connection.execute(
            """CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            )"""
        )
        mark_sqlite_stage("seed historical migration identities")
        for version, description, raw_sql in migrations:
            connection.execute(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 0)",
                (version, description, hashlib.sha384(raw_sql).digest()),
            )
        mark_sqlite_stage("seed synthetic backfill state")
        connection.execute(
            "INSERT INTO backfill_state (id, status, last_watermark, last_success_at, updated_at) VALUES (1, 'complete', NULL, 1, 1)"
        )
        mark_sqlite_stage("seed synthetic thread row")
        connection.execute(
            """INSERT INTO threads (
                id, rollout_path, created_at, updated_at, source, model_provider, cwd,
                title, sandbox_policy, approval_mode, has_user_event, preview
            ) VALUES (?, ?, 1, 1, 'cli', 'openai', '/', 'Synthetic migration consumer thread',
                      'workspaceWrite', 'on-request', 1, 'Synthetic migration snapshot row')""",
            (SYNTHETIC_THREAD_ID, str(rollout_path)),
        )
        mark_sqlite_stage("read pre-migration ledger snapshot")
        before = list(
            connection.execute(
                "SELECT version, description, success, checksum FROM _sqlx_migrations ORDER BY version"
            )
        )
        require(len(before) == 67, "Pre-migration fixture does not have all 67 historical ledger entries.")
        mark_sqlite_stage("commit pre-migration synthetic database")
        connection.commit()
        return before
    finally:
        connection.close()


def verify_historical_ledger(db_path: Path, before: list[tuple[Any, ...]]) -> list[tuple[Any, ...]]:
    mark_sqlite_stage("open database for historical ledger verification")
    connection = sqlite3.connect(f"{db_path.resolve().as_uri()}?mode=ro", uri=True)
    try:
        mark_sqlite_stage("verify unchanged historical ledger")
        after = list(
            connection.execute(
                "SELECT version, description, success, checksum FROM _sqlx_migrations ORDER BY version"
            )
        )
        require(len(after) == 67, "Packaged first-open/reopen changed the 67-entry historical migration ledger size.")
        previous = {row[0]: row for row in before}
        current = {row[0]: row for row in after}
        require(set(current) == set(previous), "Packaged first-open/reopen changed the historical migration versions.")
        require(after == before, "Packaged first-open/reopen changed a historical migration identity.")
        table_names = {
            row[0]
            for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        }
        require(
            table_names == EXPECTED_APP_TABLES | {"_sqlx_migrations"},
            "Packaged first-open/reopen changed the source-derived schema table set or migration ledger table.",
        )
        return after
    finally:
        connection.close()


def historical_state_check_after_failure(db_path: Path, before: list[tuple[Any, ...]]) -> str:
    try:
        rows = verify_historical_ledger(db_path, before)
        return f"historical_state_check=passed ledger_rows={len(rows)}"
    except sqlite3.Error as error:
        sqlite_name = getattr(error, "sqlite_errorname", None) or "UNKNOWN"
        return f"historical_state_check=failed sqlite={sqlite_name} stage={SQLITE_STAGE}"
    except RuntimeError as error:
        return f"historical_state_check=failed validation={str(error)[:512]}"


def run_app_server(
    executable: Path,
    codex_home: Path,
    sqlite_home: Path,
    db_path: Path,
    before: list[tuple[Any, ...]],
) -> None:
    codex_home.mkdir(parents=True, exist_ok=True)
    sqlite_home.mkdir(parents=True, exist_ok=True)
    config = codex_home / "config.toml"
    if not config.exists():
        config.write_text(
            "[features]\nplugins = false\nrecommended_plugins = false\n\n"
            "[analytics]\nenabled = false\n"
        )
    server = AppServer(executable, codex_home, sqlite_home)
    try:
        server.initialize_and_list(SYNTHETIC_THREAD_ID)
    except Exception as error:
        # Preserve the RPC/startup failure if shutdown also reports an error;
        # otherwise the cleanup exception masks the useful fixed diagnostic.
        try:
            server.close()
        except Exception:
            pass
        if isinstance(error, AppServerRpcError):
            historical_check = historical_state_check_after_failure(db_path, before)
            raise RuntimeError(server._rpc_failure_detail(error, historical_check)) from None
        raise
    else:
        server.close()


def consume() -> None:
    pr_head, workflow_merge_sha = validate_pull_request_checkout()
    architecture = env("ARCHITECTURE")
    require(architecture in EXPECTED_RUNNERS, "Unsupported architecture selector.")
    repo = env("GITHUB_REPOSITORY")
    receipt = verify_producer(
        repo=repo,
        run_id=env("PRODUCER_RUN_ID"),
        architecture=architecture,
    )
    archive, manifest_details = read_bundle(
        Path(env("ARTIFACT_DIR")), int(receipt["producer_run_number"]), architecture
    )
    candidate_dir = Path(env("CANDIDATE_SOURCE_DIR"))
    migration_source_dir = Path(env("HISTORICAL_SOURCE_DIR"))
    require(
        subprocess.check_output(["git", "-C", str(candidate_dir), "rev-parse", "HEAD"], text=True).strip()
        == PRODUCT_SOURCE,
        "Candidate-source checkout does not match the exact product SHA.",
    )
    require(
        subprocess.check_output(["git", "-C", str(candidate_dir), "rev-parse", "HEAD^{tree}"], text=True).strip()
        == PRODUCT_TREE,
        "Candidate-source checkout does not match the exact product tree.",
    )
    require(
        subprocess.check_output(["git", "-C", str(migration_source_dir), "rev-parse", "HEAD"], text=True).strip()
        == MIGRATION_38E25,
        "Historical migration checkout does not match the exact 38e25 source SHA.",
    )
    migrations = source_migrations(migration_source_dir)
    product_migrations = source_migrations(candidate_dir)
    require(product_migrations == migrations, "Product migrations differ from the pinned historical source.")
    snapshot = schema_snapshot_path()
    snapshot_sha256 = hashlib.sha256(snapshot.read_bytes()).hexdigest()
    executable = extract_codex(archive, Path(env("RUNNER_TEMP")), architecture)

    version = subprocess.run([str(executable), "--version"], check=True, text=True, capture_output=True, timeout=60).stdout.strip()
    require("0.163.0-dev.sedna.1" in version and "-dirty" not in version, "Packaged CLI version does not match the immutable product source.")
    help_text = subprocess.run([str(executable), "mcp", "login", "--help"], check=True, text=True, capture_output=True, timeout=60).stdout
    require("--device-auth" in help_text, "Packaged CLI does not expose MCP device authentication.")

    with tempfile.TemporaryDirectory(prefix="integrated-historical-state-consumer-") as temp_dir:
        root = Path(temp_dir)
        codex_home = root / "home"
        sqlite_home = root / "sqlite"
        sqlite_home.mkdir(parents=True, exist_ok=True)
        rollout = root / "synthetic-rollout.jsonl"
        rollout.write_text("\n")
        db_path = sqlite_home / "state_5.sqlite"
        before = create_pre61_database(db_path, snapshot, migrations, rollout)
        run_app_server(executable, codex_home, sqlite_home, db_path, before)
        first_open_rows = verify_historical_ledger(db_path, before)

        run_app_server(executable, codex_home, sqlite_home, db_path, before)
        reopened_rows = verify_historical_ledger(db_path, before)
        require(reopened_rows == first_open_rows, "Historical migration ledger changed between first open and process reopen.")

    print(
        json.dumps(
            {
                "consumer": "exact-package-cli-and-historical-state-reopen",
                "repository": repo,
                "pullRequestHead": pr_head,
                "workflowMergeSha": workflow_merge_sha,
                "productSource": PRODUCT_SOURCE,
                "historicalSchemaSource": MIGRATION_38E25,
                "schemaSnapshotSha256": snapshot_sha256,
                "productMigrationEntriesMatchHistoricalSource": True,
                "arch": architecture,
                "codexVersion": version,
                "deviceAuthHelpFlag": "present (help output only; not login/provider acceptance)",
                "migrationLedgerBefore": len(migrations),
                "migrationLedgerAfter": len(reopened_rows),
                "historicalMigrationLedgerEntriesUnchanged": True,
                "syntheticThreadVisibleOnFirstOpenAndReopen": True,
                "historicalLedgerUnchangedAcrossFirstOpenAndReopen": True,
                "producer": receipt,
                "producerWorkflowConclusion": receipt["producer_run_conclusion"],
                "packageVersion": manifest_details["package_version"],
                "packageArchiveSha256": manifest_details["archive_sha256"],
                "packageArtifactHashes": manifest_details["package_artifact_hashes"],
                "workflowHostSha": manifest_details["workflow_host_sha"],
                "workflowHostTree": manifest_details["workflow_host_tree"],
                "qualificationScope": "data-free source-derived historical schema snapshot with 67 unchanged migration identities through first open and reopen; not populated user-database equivalence or full TUI/provider acceptance",
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    try:
        consume()
    except Exception as error:
        # Keep logs value-free: exception text here is restricted to fixed
        # validation messages and never includes environment values/tokens.
        if isinstance(error, RuntimeError):
            detail = str(error)
        elif isinstance(error, sqlite3.Error):
            sqlite_name = getattr(error, "sqlite_errorname", None) or "UNKNOWN"
            detail = f"SQLite {sqlite_name} during {SQLITE_STAGE}"
        else:
            detail = type(error).__name__
        print(f"packaged consumer failed: {detail}", file=sys.stderr)
        raise SystemExit(1)
