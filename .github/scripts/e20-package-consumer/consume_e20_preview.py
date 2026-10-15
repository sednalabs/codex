#!/usr/bin/env python3
"""Verify two immutable e20 preview artifacts and exercise their packaged CLI.

This is a hosted-only, data-free compatibility consumer. It constructs a
declarative pre-migration-61 schema from public source, seeds synthetic thread
and migration-ledger rows, then proves first-open and a second-process reopen
of the packaged app-server. It does not replay the duplicate historical SQL
directory and never reads or uploads a user database.
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
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


E20_SOURCE = "e20b2dc9a96c08b9ae7f38298d098ed01af68e87"
# The producer dispatch passed the exact commit SHA as its `ref` input, so its
# preview manifest records that SHA rather than the source branch name.
E20_REF = "e20b2dc9a96c08b9ae7f38298d098ed01af68e87"
WORKFLOW_HOST = "c753adfb07d05567efd386b4b829d2e53b4583c0"
WORKFLOW_HOST_REF = "preview/workflow-host-c753adf-20261015"
BASE_SHA = "703389678a1a6c3a9d131245d03d3f1805b9afa1"
PULL_REQUEST_BRANCH = "validation/w14962-e20-packaged-consumer-20261015"
MIGRATION_38E25 = "38e25e75711d323be7039a81bcf1c31aa1117583"
MIGRATION_61_SHA384 = (
    "4a8337048bb958a21fd5272c1bd670605c84d4063534270d3e41cdc5c5a2e865"
    "f0fe6ae3c336ded873877ee2ea10f2e0"
)
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
EXPECTED_PACKAGE_FILES = {
    "codex",
    "codex-code-mode-host",
    "codex-responses-api-proxy",
}
EXPECTED_RUNNERS = {
    "x86_64": "ubuntu-24.04",
    "aarch64": "ubuntu-24.04-arm",
}
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


def verify_producer(
    *,
    repo: str,
    run_id: str,
    job_id: str,
    artifact_id: str,
    artifact_name: str,
    digest: str,
    architecture: str,
) -> dict[str, Any]:
    run = github_api(f"/repos/{repo}/actions/runs/{run_id}")
    expected_run = {
        "id": int(run_id),
        "path": ".github/workflows/sedna-branch-build.yml",
        "head_branch": WORKFLOW_HOST_REF,
        "head_sha": WORKFLOW_HOST,
        "event": "workflow_dispatch",
        "status": "completed",
        "conclusion": "success",
        "run_attempt": 1,
    }
    for key, value in expected_run.items():
        require(run.get(key) == value, f"Producer run identity mismatch: {key}.")

    jobs = github_api(f"/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100").get("jobs", [])
    job = next((candidate for candidate in jobs if candidate.get("id") == int(job_id)), None)
    require(job is not None, "Exact producer job is absent.")
    require(job.get("run_id") == int(run_id), "Producer job belongs to a different run.")
    require(job.get("status") == "completed" and job.get("conclusion") == "success", "Producer job did not succeed.")
    require(job.get("runner_group_name") == "GitHub Actions", "Producer was not on the standard GitHub-hosted runner group.")
    require(job.get("labels") == [EXPECTED_RUNNERS[architecture]], "Producer runner label does not match the expected free standard runner.")

    artifacts = github_api(f"/repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100").get("artifacts", [])
    artifact = next((candidate for candidate in artifacts if candidate.get("id") == int(artifact_id)), None)
    require(artifact is not None, "Exact producer artifact is absent.")
    require(artifact.get("name") == artifact_name, "Producer artifact name mismatch.")
    require(artifact.get("digest") == digest, "Producer artifact digest mismatch.")
    require(artifact.get("expired") is False, "Producer artifact is expired.")
    return {
        "run_id": run_id,
        "job_id": job_id,
        "producer_head_sha": run["head_sha"],
        "producer_run_number": run["run_number"],
        "source_sha": E20_SOURCE,
        "runner_label": job["labels"][0],
        "artifact_id": artifact_id,
        "artifact_name": artifact_name,
        "artifact_digest": digest,
    }


def read_bundle(artifact_dir: Path, producer_run_number: int) -> tuple[Path, dict[str, Any]]:
    entries = [path for path in artifact_dir.iterdir()]
    require(all(path.is_file() and not path.is_symlink() for path in entries), "Artifact bundle contains an unexpected directory or link.")
    archives = [path for path in entries if path.name.endswith(".tar.gz")]
    manifests = [path for path in entries if path.name.endswith(".json")]
    require(len(entries) == 2 and len(archives) == 1 and len(manifests) == 1, "Artifact bundle shape is not the exact preview package pair.")
    manifest = json.loads(manifests[0].read_text())
    require(manifest.get("repository") == env("GITHUB_REPOSITORY"), "Preview manifest repository mismatch.")
    require(manifest.get("commit") == E20_SOURCE, "Preview manifest source commit mismatch.")
    require(manifest.get("ref") == E20_REF, "Preview manifest source ref mismatch.")
    workflow_url = manifest.get("workflow", "")
    run_id = env("PRODUCER_RUN_ID")
    require(workflow_url.endswith(f"/actions/runs/{run_id}"), "Preview manifest does not bind to its exact producer run.")
    version = manifest.get("previewVersion", "")
    require(
        re.search(rf"-ci\.{producer_run_number}\+g{E20_SOURCE[:8]}$", version) is not None,
        "Preview metadata version does not match the exact producer run and source SHA.",
    )
    if architecture := os.environ.get("ARCHITECTURE"):
        if manifest.get("target") is not None:
            require(manifest["target"] == f"{architecture}-unknown-linux-gnu", "Preview manifest target mismatch.")
    archive_hasher = hashlib.sha256()
    with archives[0].open("rb") as archive_stream:
        for chunk in iter(lambda: archive_stream.read(1024 * 1024), b""):
            archive_hasher.update(chunk)
    return archives[0], {"preview_version": version, "archive_sha256": archive_hasher.hexdigest()}


def extract_codex(archive: Path, destination: Path, architecture: str) -> Path:
    found: set[str] = set()
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
            require(member.isfile() or member.isdir(), "Package contains a link or special file.")
            if member.isdir():
                require(clean_name == "systemd", "Package contains an unrecognized directory.")
                continue
            require(clean_name in EXPECTED_PACKAGE_FILES, "Package contains an unexpected file.")
            found.add(clean_name)
            if clean_name == "codex":
                extracted = package.extractfile(member)
                require(extracted is not None, "Codex executable is unreadable in the package.")
                with extracted, codex_path.open("wb") as output:
                    shutil.copyfileobj(extracted, output, length=1024 * 1024)
                codex_path.chmod(0o755)
    require(found == EXPECTED_PACKAGE_FILES, "Package is missing a required preview binary or service file.")
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
            [str(executable), "app-server", "--listen", "stdio://", "--session-source", "cli"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env_allow,
            bufsize=0,
        )
        require(self.process.stdin is not None and self.process.stdout is not None, "Could not open app-server protocol pipes.")
        self.inp = self.process.stdin
        self.out = self.process.stdout
        self.pending = bytearray()

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
                require("error" not in response, f"Packaged app-server rejected {method}.")
                require("result" in response, f"Packaged app-server omitted {method} result.")
                return response["result"]

    def initialize_and_list(self, expected_thread_id: str) -> None:
        result = self.request(
            "initialize",
            1,
            {
                "clientInfo": {"name": "e20-historical-state-consumer", "version": "1"},
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
        try:
            status = self.process.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=10)
            raise RuntimeError("Packaged app-server did not exit after stdin EOF.")
        require(status == 0, f"Packaged app-server exited with status {status}.")


def source_migrations(source_dir: Path, candidate_dir: Path) -> tuple[list[tuple[int, str, bytes]], bytes]:
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
    migration61_path = candidate_dir / "codex-rs/state/migrations/0061_thread_read_state.sql"
    migration61 = migration61_path.read_bytes()
    require(hashlib.sha384(migration61).hexdigest() == MIGRATION_61_SHA384, "e20 migration-61 bytes do not match the reviewed source identity.")
    return migrations, migration61


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
            ("e20-synthetic-thread", str(rollout_path)),
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


def verify_post_migration(db_path: Path, before: list[tuple[Any, ...]]) -> list[tuple[Any, ...]]:
    mark_sqlite_stage("open migrated database for verification")
    connection = sqlite3.connect(db_path)
    try:
        mark_sqlite_stage("verify migrated ledger")
        after = list(
            connection.execute(
                "SELECT version, description, success, checksum FROM _sqlx_migrations ORDER BY version"
            )
        )
        require(len(after) == 68, "First-open/reopen did not preserve 67 prior rows and add exactly migration 61.")
        previous = {row[0]: row for row in before}
        current = {row[0]: row for row in after}
        require(set(current) == set(previous) | {61}, "Migration ledger versions changed beyond migration 61.")
        for version, row in previous.items():
            require(current[version] == row, f"Historical migration identity changed at version {version}.")
        migration61 = current[61]
        require(migration61[1] == "thread_read_state" and migration61[2] == 1 and migration61[3] == bytes.fromhex(MIGRATION_61_SHA384), "Migration 61 ledger identity is incorrect.")

        mark_sqlite_stage("verify migration-61 schema")
        columns = list(connection.execute("PRAGMA table_info(thread_read_receipts)"))
        expected_columns = [
            ("thread_id", "TEXT", 1, 1),
            ("first_unread_turn", "TEXT", 0, 0),
            ("revision", "TEXT", 1, 0),
            ("last_published_turn", "TEXT", 0, 0),
        ]
        normalized = [(row[1], row[2].upper(), row[3], row[5]) for row in columns]
        require(normalized == expected_columns, "Migration 61 table columns or nullability do not match source.")
        require(list(connection.execute("PRAGMA foreign_key_list(thread_read_receipts)")) == [], "Migration 61 added an unexpected foreign key.")
        return after
    finally:
        connection.close()


def run_app_server(executable: Path, codex_home: Path, sqlite_home: Path) -> None:
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
        server.initialize_and_list("e20-synthetic-thread")
    finally:
        server.close()


def consume() -> None:
    pr_head, workflow_merge_sha = validate_pull_request_checkout()
    architecture = env("ARCHITECTURE")
    require(architecture in EXPECTED_RUNNERS, "Unsupported architecture selector.")
    repo = env("GITHUB_REPOSITORY")
    receipt = verify_producer(
        repo=repo,
        run_id=env("PRODUCER_RUN_ID"),
        job_id=env("PRODUCER_JOB_ID"),
        artifact_id=env("PRODUCER_ARTIFACT_ID"),
        artifact_name=env("PRODUCER_ARTIFACT_NAME"),
        digest=env("PRODUCER_ARTIFACT_DIGEST"),
        architecture=architecture,
    )
    archive, manifest_details = read_bundle(
        Path(env("ARTIFACT_DIR")), int(receipt["producer_run_number"])
    )
    candidate_dir = Path(env("CANDIDATE_SOURCE_DIR"))
    migration_source_dir = Path(env("HISTORICAL_SOURCE_DIR"))
    require(
        subprocess.check_output(["git", "-C", str(candidate_dir), "rev-parse", "HEAD"], text=True).strip()
        == E20_SOURCE,
        "Candidate-source checkout does not match the exact e20 product SHA.",
    )
    require(
        subprocess.check_output(["git", "-C", str(migration_source_dir), "rev-parse", "HEAD"], text=True).strip()
        == MIGRATION_38E25,
        "Historical migration checkout does not match the exact 38e25 source SHA.",
    )
    migrations, migration61 = source_migrations(migration_source_dir, candidate_dir)
    snapshot = schema_snapshot_path()
    snapshot_sha256 = hashlib.sha256(snapshot.read_bytes()).hexdigest()
    executable = extract_codex(archive, Path(env("RUNNER_TEMP")), architecture)

    version = subprocess.run([str(executable), "--version"], check=True, text=True, capture_output=True, timeout=60).stdout.strip()
    require("0.163.0-dev.sedna.1" in version and "-dirty" not in version, "Packaged CLI version does not match the immutable e20 product source.")
    help_text = subprocess.run([str(executable), "mcp", "login", "--help"], check=True, text=True, capture_output=True, timeout=60).stdout
    require("--device-auth" in help_text, "Packaged CLI does not expose MCP device authentication.")

    with tempfile.TemporaryDirectory(prefix="e20-historical-state-consumer-") as temp_dir:
        root = Path(temp_dir)
        codex_home = root / "home"
        sqlite_home = root / "sqlite"
        sqlite_home.mkdir(parents=True, exist_ok=True)
        rollout = root / "synthetic-rollout.jsonl"
        rollout.write_text("\n")
        db_path = sqlite_home / "state_5.sqlite"
        before = create_pre61_database(db_path, snapshot, migrations, rollout)
        run_app_server(executable, codex_home, sqlite_home)
        first_open_rows = verify_post_migration(db_path, before)

        mark_sqlite_stage("open migrated database to seed receipt")
        connection = sqlite3.connect(db_path)
        try:
            mark_sqlite_stage("write synthetic thread receipt")
            connection.execute(
                "INSERT INTO thread_read_receipts (thread_id, first_unread_turn, revision, last_published_turn) VALUES (?, ?, ?, ?)",
                ("e20-synthetic-thread", "synthetic-turn-1", "synthetic-revision-1", "synthetic-turn-0"),
            )
            connection.commit()
        finally:
            connection.close()

        run_app_server(executable, codex_home, sqlite_home)
        reopened_rows = verify_post_migration(db_path, before)
        require(reopened_rows == first_open_rows, "Migration ledger changed between first open and process reopen.")
        mark_sqlite_stage("open migrated database to verify receipt")
        connection = sqlite3.connect(db_path)
        try:
            mark_sqlite_stage("read synthetic receipt after reopen")
            receipt_row = connection.execute(
                "SELECT thread_id, first_unread_turn, revision, last_published_turn FROM thread_read_receipts WHERE thread_id = ?",
                ("e20-synthetic-thread",),
            ).fetchone()
        finally:
            connection.close()
        require(
            receipt_row == ("e20-synthetic-thread", "synthetic-turn-1", "synthetic-revision-1", "synthetic-turn-0"),
            "Migration 61 receipt did not persist across the packaged process reopen.",
        )

    print(
        json.dumps(
            {
                "consumer": "e20-packaged-cli-and-state-migration61",
                "repository": repo,
                "pullRequestHead": pr_head,
                "workflowMergeSha": workflow_merge_sha,
                "productSource": E20_SOURCE,
                "historicalSchemaSource": MIGRATION_38E25,
                "schemaSnapshotSha256": snapshot_sha256,
                "arch": architecture,
                "codexVersion": version,
                "deviceAuthFlag": "present",
                "migrationLedgerBefore": len(migrations),
                "migrationLedgerAfter": len(reopened_rows),
                "migration61Sha384": hashlib.sha384(migration61).hexdigest(),
                "syntheticThreadVisibleOnFirstOpenAndReopen": True,
                "threadReadReceiptPersistedAcrossReopen": True,
                "producer": receipt,
                "previewVersionMetadata": manifest_details["preview_version"],
                "packageArchiveSha256": manifest_details["archive_sha256"],
                "qualificationScope": "data-free source-derived historical schema snapshot; not populated user-database equivalence",
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
        print(f"e20 packaged consumer failed: {detail}", file=sys.stderr)
        raise SystemExit(1)
