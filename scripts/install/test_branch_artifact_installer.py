from __future__ import annotations

import contextlib
import hashlib
import http.server
import importlib.machinery
import importlib.util
import io
import json
import os
import platform
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
import warnings
import zipfile
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


INSTALLER_PATH = Path(__file__).resolve().parents[1] / "install_branch_artifact"
LOADER = importlib.machinery.SourceFileLoader("branch_installer", str(INSTALLER_PATH))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
assert SPEC is not None
installer = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = installer
LOADER.exec_module(installer)


SOURCE_SHA = "a" * 40
WORKFLOW_SHA = "b" * 40
NATIVE_MACHINE = platform.machine().lower()
TARGET = "aarch64-unknown-linux-gnu" if NATIVE_MACHINE in ("aarch64", "arm64") else "x86_64-unknown-linux-gnu"
MACHINE = 183 if TARGET.startswith("aarch64") else 62
BRANCH = "feature/installer-test"


def elf(machine: int = MACHINE) -> bytes:
    return b"\x7fELF\x02\x01" + b"\0" * 12 + struct.pack("<H", machine) + b"\0" * 44


def tar_bytes(files: dict[str, bytes]) -> bytes:
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.mode = 0o755
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
    return output.getvalue()


def raw_tar(entries: list[tuple[str, bytes, str]]) -> bytes:
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as archive:
        for name, data, kind in entries:
            info = tarfile.TarInfo(name)
            info.mode = 0o755
            if kind == "symlink":
                info.type = tarfile.SYMTYPE
                info.linkname = "../../outside"
                archive.addfile(info)
            else:
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
    return output.getvalue()


def zip_bytes(files: dict[str, bytes]) -> bytes:
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in files.items():
            archive.writestr(name, data)
    return output.getvalue()


def rewrite_json_member(archive_bytes: bytes, member_name: str, update) -> bytes:
    with zipfile.ZipFile(io.BytesIO(archive_bytes)) as archive:
        files = {name: archive.read(name) for name in archive.namelist()}
    metadata = json.loads(files[member_name])
    update(metadata)
    files[member_name] = json.dumps(metadata).encode()
    return zip_bytes(files)


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class FixtureHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        fixture_api: FakeApi = self.server.fixture_api  # type: ignore[attr-defined]
        if (
            self.headers.get("Authorization") != "Bearer synthetic-fixture-token"
            or self.headers.get("Proxy-authorization") != "synthetic-proxy-token"
            or self.headers.get("Cookie") != "synthetic-fixture-cookie"
        ):
            self.send_error(401)
            return
        if "/actions/artifacts/" in self.path and self.path.endswith("/zip"):
            redirect_root = getattr(self.server, "redirect_artifact_root", None)
            if redirect_root is not None:
                location = f"{redirect_root}{self.path}"
                self.send_response(302)
                self.send_header("Location", location)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            artifact_id = int(self.path.rsplit("/", 2)[1])
            body = fixture_api.payloads[artifact_id]
            content_type = "application/zip"
            status = 200
        else:
            try:
                body = json.dumps(fixture_api.json(self.path)).encode()
                content_type = "application/json"
                status = 200
            except (AssertionError, KeyError, ValueError):
                body = b"{}"
                content_type = "application/json"
                status = 404
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        return


class RedirectArtifactHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        fixture_api: FakeApi = self.server.fixture_api  # type: ignore[attr-defined]
        self.server.authorization_headers.append(
            (
                self.headers.get("Authorization"),
                self.headers.get("Proxy-authorization"),
                self.headers.get("Cookie"),
            )
        )  # type: ignore[attr-defined]
        artifact_id = int(self.path.rsplit("/", 2)[1])
        body = fixture_api.payloads[artifact_id]
        self.send_response(200)
        self.send_header("Content-Type", "application/zip")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        return


def action_artifact(kind: str, run_id: int, source_sha: str = SOURCE_SHA, ref: str = BRANCH, target: str = TARGET):
    core_version = f"1.2.3-ci.1+g{source_sha[:8]}"
    host_version = f"1.2.3-ci.2+g{source_sha[:8]}"
    core_archive_name = f"codex-sedna-preview-{core_version.replace('+', '__')}-{target}.tar.gz"
    host_archive_name = f"codex-sedna-preview-host-{host_version.replace('+', '__')}-{target}.tar.gz"
    if kind == "core":
        archive_name = core_archive_name
        payload = tar_bytes({"codex": elf(), "codex-responses-api-proxy": elf()})
        metadata = {
            "repository": installer.REPOSITORY,
            "commit": source_sha,
            "ref": ref,
            "previewVersion": core_version,
            "workflow": f"https://github.com/{installer.REPOSITORY}/actions/runs/{run_id}",
            "target": target,
        }
        return zip_bytes({archive_name: payload, "core.json": json.dumps(metadata).encode()})
    host_binary = elf(MACHINE if target == TARGET else 183)
    host_archive = tar_bytes({"codex-code-mode-host": host_binary})
    metadata = {
        "repository": installer.REPOSITORY,
        "commit": source_sha,
        "workflowCommit": WORKFLOW_SHA,
        "ref": ref,
        "previewVersion": host_version,
        "artifact": "codex-code-mode-host",
        "binarySha256": sha(host_binary),
        "workflow": f"https://github.com/{installer.REPOSITORY}/actions/runs/{run_id}",
    }
    base = host_archive_name.removesuffix(".tar.gz")
    return zip_bytes(
        {
            host_archive_name: host_archive,
            f"{base}.binary.sha256": f"{sha(host_binary)}  codex-code-mode-host\n".encode(),
            f"{host_archive_name}.sha256": f"{sha(host_archive)}  {host_archive_name}\n".encode(),
            "host.json": json.dumps(metadata).encode(),
        }
    )


class FakeApi:
    def __init__(self, artifacts: dict[int, tuple[str, bytes]], created: dict[int, str] | None = None):
        self.payloads = {artifact_id: data for artifact_id, (_, data) in artifacts.items()}
        self.records: dict[int, dict] = {}
        self.runs: dict[int, dict] = {}
        self.created = created or {}
        self.artifacts_by_run: dict[int, list[dict]] = {}
        for index, (artifact_id, (kind, data)) in enumerate(artifacts.items()):
            run_id = artifact_id + 1000
            with zipfile.ZipFile(io.BytesIO(data)) as archive:
                metadata_name = next(name for name in archive.namelist() if name.endswith(".json"))
                metadata = json.loads(archive.read(metadata_name))
            source_sha = metadata["commit"]
            ref = metadata["ref"]
            self.records[artifact_id] = {
                "id": artifact_id,
                "name": f"sedna-branch-{installer.branch_slug(ref)}-{source_sha[:8]}-{TARGET}",
                "digest": f"sha256:{sha(data)}",
                "expired": False,
                "workflow_run": {"id": run_id},
            }
            run = {
                "id": run_id,
                "event": "workflow_dispatch",
                "status": "completed",
                "conclusion": "success",
                "repository": {"full_name": installer.REPOSITORY},
                "path": f"{installer.WORKFLOW_PATH}@refs/heads/main",
                "head_sha": WORKFLOW_SHA,
                "created_at": self.created.get(run_id, f"2026-09-30T00:00:{index:02d}Z"),
            }
            self.runs[run_id] = run
            self.artifacts_by_run.setdefault(run_id, []).append(self.records[artifact_id])
        self.runs_payload = list(self.runs.values())

    def json(self, path: str):
        if "/actions/workflows/" in path and "/runs?" in path and "page=1" in path:
            return {"workflow_runs": self.runs_payload}
        if "/actions/workflows/" in path and "page=2" in path:
            return {"workflow_runs": []}
        if "/actions/runs/" in path and "/artifacts?" in path:
            run_id = int(path.split("/actions/runs/")[1].split("/")[0])
            return {"artifacts": self.artifacts_by_run.get(run_id, [])}
        if "/actions/runs/" in path:
            run_id = int(path.rsplit("/", 1)[-1])
            return self.runs[run_id]
        raise AssertionError(f"unexpected API path: {path}")

    def download(self, artifact_id: int, destination: Path) -> str:
        data = self.payloads[artifact_id]
        destination.write_bytes(data)
        return sha(data)


class FakeZstdProcess:
    def __init__(self, payload: bytes):
        self.stdout = io.BytesIO(payload)
        self.killed = False

    def wait(self) -> int:
        return 0

    def kill(self) -> None:
        self.killed = True


def make_api(pair: tuple[bytes, bytes]) -> FakeApi:
    return FakeApi({1: ("core", pair[0]), 2: ("host", pair[1])})


class BranchArtifactInstallerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.core_zip = action_artifact("core", 1001)
        self.host_zip = action_artifact("host", 1002)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def qualified_record(self, *, profile: str = "full", disposition: str = "accepted", eligible: bool = True):
        return {
            "record_id": "synthetic-qualified-q3-s1",
            "disposition": disposition,
            "w14780_eligible": eligible,
            "identity": {
                "product_sha": SOURCE_SHA,
                "comparison_base_ref": "main",
                "comparison_base_sha": WORKFLOW_SHA,
                "fixture_sha": installer.Q3_SHA,
                "sdk_sha": installer.S1_SHA,
                "profile": profile,
            },
            "producer": {
                "repository": installer.REPOSITORY,
                "workflow_id": installer.QUALIFICATION_WORKFLOW_ID,
                "workflow_path": installer.QUALIFICATION_WORKFLOW_PATH,
                "run_id": 4321,
            },
            "jobs": [],
            "artifacts": {
                "x86_64": {"id": 501, "name": "sedna-first-binary-example-x86_64", "digest": "sha256:" + "a" * 64, "size_in_bytes": 1024},
                "aarch64": {"id": 502, "name": "sedna-first-binary-example-aarch64", "digest": "sha256:" + "b" * 64, "size_in_bytes": 1024},
            },
        }

    def producer_contract_fixture(self):
        run = {
            "id": 4321,
            "run_attempt": 2,
            "event": "workflow_dispatch",
            "status": "completed",
            "conclusion": "failure",
            "repository": {"id": 1152496647, "full_name": installer.REPOSITORY},
            "head_repository": {"id": 1152496647, "full_name": installer.REPOSITORY},
            "workflow_id": installer.QUALIFICATION_WORKFLOW_ID,
            "path": f"{installer.QUALIFICATION_WORKFLOW_PATH}@refs/heads/main",
            "head_sha": WORKFLOW_SHA,
            "head_branch": "candidate/full-record",
            "ref": "refs/heads/candidate/full-record",
        }
        contracts = []
        actual_jobs = []
        for name, runner in installer.QUALIFICATION_JOB_RUNNERS.items():
            conclusion = "skipped" if name.startswith("Prepare exact") else "failure" if name.startswith("Consume native") else "success"
            contract = {
                "name": name,
                "status": "completed",
                "conclusion": conclusion,
                "runner": runner,
                "ran": conclusion != "skipped",
            }
            contracts.append(contract)
            job = {**contract, "run_id": run["id"], "run_attempt": run["run_attempt"], "labels": [runner]}
            if contract["ran"]:
                job.update({"runner_group_id": 0, "runner_group_name": "GitHub Actions", "runner_name": "GitHub Actions 42"})
            else:
                job.update({"runner_group_id": None, "runner_group_name": None, "runner_name": None})
            actual_jobs.append(job)

        artifacts = {}
        api_artifacts = []
        for arch, artifact_id in (("x86_64", 501), ("aarch64", 502)):
            name = f"sedna-first-binary-{SOURCE_SHA}-{arch}"
            digest = "sha256:" + ("a" if arch == "x86_64" else "b") * 64
            artifacts[arch] = {"id": artifact_id, "name": name, "digest": digest, "size_in_bytes": 1024}
            api_artifacts.append({
                **artifacts[arch],
                "expired": False,
                "workflow_run": {
                    "id": run["id"],
                    "head_sha": run["head_sha"],
                    "head_branch": run["head_branch"],
                },
            })
        record = {
            "identity": {"product_sha": SOURCE_SHA},
            "producer": {
                "repository": installer.REPOSITORY,
                "workflow_id": installer.QUALIFICATION_WORKFLOW_ID,
                "workflow_path": installer.QUALIFICATION_WORKFLOW_PATH,
                "run_id": run["id"],
                "run_attempt": run["run_attempt"],
                "workflow_host_sha": run["head_sha"],
                "branch": run["head_branch"],
                "status": run["status"],
                "conclusion": run["conclusion"],
            },
            "jobs": contracts,
            "artifacts": artifacts,
        }

        class ProducerApi:
            def __init__(self):
                self.jobs = actual_jobs
                self.artifacts = api_artifacts

            def json(self, path):
                if path.endswith(f"/actions/runs/{run['id']}"):
                    return run
                if path.endswith(f"/actions/workflows/{installer.QUALIFICATION_WORKFLOW_ID}"):
                    return {"id": installer.QUALIFICATION_WORKFLOW_ID, "path": installer.QUALIFICATION_WORKFLOW_PATH, "state": "active"}
                if "/jobs?" in path:
                    return {"total_count": len(self.jobs), "jobs": self.jobs}
                if "/artifacts?" in path:
                    return {"total_count": len(self.artifacts), "artifacts": self.artifacts}
                raise AssertionError(f"unexpected producer API path: {path}")

        return ProducerApi(), record, run

    def qualified_stage_fixture(self):
        package_names = (
            "codex-package.json", "manifest.json", "bin/codex", "bin/codex-code-mode-host",
            "codex-path/rg", "codex-resources/bwrap", "codex-responses-api-proxy",
            f"codex-package-{TARGET}.tar.zst",
        )
        files = {}
        for name in package_names:
            path = self.root / "verified" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(
                b"{}" if name in ("codex-package.json", "manifest.json")
                else b"synthetic-zstd-package" if name.endswith(".tar.zst")
                else elf()
            )
            files[name] = path
        manifest = {
            "binarySha256": {name: sha(path.read_bytes()) for name, path in files.items()},
            "mode": "qualified-complete-package",
            "target": TARGET,
        }
        temporary = self.root / "temporary"
        temporary.mkdir()
        return files, manifest, temporary, package_names

    def test_qualified_mode_selects_only_exact_accepted_full_record(self) -> None:
        row = self.qualified_record()
        selected = installer._select_qualified_record({"records": [row]}, row["record_id"])
        self.assertIs(selected, row)
        self.assertEqual(
            installer.QUALIFICATION_TESTS_BY_HOST[installer.QUALIFICATION_HOST_SHA][
                (selected["identity"]["fixture_sha"], selected["identity"]["sdk_sha"])
            ]["plain"],
            installer.QUALIFICATION_TESTS_BY_HOST[installer.QUALIFICATION_HOST_SHA][
                (installer.Q3_SHA, installer.S1_SHA)
            ]["plain"],
        )

    def test_qualified_mode_rejects_non_full_or_unaccepted_records(self) -> None:
        for row in (
            self.qualified_record(profile="pair"),
            self.qualified_record(disposition="diagnostic", eligible=False),
            self.qualified_record(disposition="accepted", eligible=False),
        ):
            with self.subTest(profile=row["identity"]["profile"], disposition=row["disposition"]):
                with self.assertRaises(installer.InstallError):
                    installer._select_qualified_record({"records": [row]}, row["record_id"])

    def test_qualified_mode_rejects_unsupported_fixture_sdk_generation(self) -> None:
        row = self.qualified_record()
        row["identity"]["sdk_sha"] = "d" * 40
        with self.assertRaisesRegex(installer.InstallError, "does not support"):
            installer._select_qualified_record({"records": [row]}, row["record_id"])

    def test_qualified_mode_requires_exact_run_and_record_selector(self) -> None:
        with self.assertRaises(SystemExit):
            installer.parse_args(["--qualified-run-id", "4321"])
        with self.assertRaises(SystemExit):
            installer.parse_args(["--record-id", "synthetic-qualified-q3-s1"])
        args = installer.parse_args([
            "--qualified-run-id", "4321", "--record-id", "synthetic-qualified-q3-s1",
            "--stage-dir", str(self.root / "qualified-stage"),
        ])
        self.assertEqual(args.qualified_run_id, 4321)

    def test_producer_job_contract_binds_success_to_exact_run_attempt(self) -> None:
        api, record, _ = self.producer_contract_fixture()
        self.assertEqual(
            set(installer._verify_producer_for_record(api, record)),
            {"x86_64", "aarch64"},
        )
        api.jobs[0]["run_id"] += 1
        with self.assertRaisesRegex(installer.InstallError, "exact producer run attempt"):
            installer._verify_producer_for_record(api, record)

    def test_producer_job_contract_rejects_mismatched_attempt(self) -> None:
        api, record, run = self.producer_contract_fixture()
        api.jobs[0]["run_attempt"] = run["run_attempt"] + 1
        with self.assertRaisesRegex(installer.InstallError, "exact producer run attempt"):
            installer._verify_producer_for_record(api, record)

    def test_producer_artifact_association_and_digest_are_exact(self) -> None:
        api, record, _ = self.producer_contract_fixture()
        api.artifacts[0]["workflow_run"]["id"] += 1
        with self.assertRaisesRegex(installer.InstallError, "artifact identity"):
            installer._verify_producer_for_record(api, record)

        api, record, _ = self.producer_contract_fixture()
        api.artifacts[0]["digest"] = "sha256:" + "0" * 64
        with self.assertRaisesRegex(installer.InstallError, "artifact identity"):
            installer._verify_producer_for_record(api, record)

    def test_qualified_stage_persists_complete_layout_without_install_paths(self) -> None:
        files, manifest, temporary, package_names = self.qualified_stage_fixture()
        destination = self.root / "persisted-qualified-package"
        fake_home = self.root / "untouched-home"
        with patch.dict(os.environ, {"HOME": str(fake_home), "CODEX_HOME": str(fake_home / ".codex")}):
            result = installer.stage_qualified_package(files, manifest, str(destination), temporary)
        self.assertEqual(result, destination)
        self.assertTrue((destination / "qualified-complete-package.json").is_file())
        self.assertEqual(json.loads((destination / "qualified-complete-package.json").read_text()), manifest)
        self.assertEqual({name for name in package_names if (destination / name).is_file()}, set(package_names))
        self.assertFalse(fake_home.exists())
        self.assertFalse((fake_home / ".codex" / "current").exists())

    def test_qualified_public_command_stages_and_never_activates(self) -> None:
        files, manifest, _, package_names = self.qualified_stage_fixture()
        destination = self.root / "public-command-stage"
        fake_home = self.root / "untouched-public-command-home"
        output = io.StringIO()
        with (
            patch.object(installer, "native_target", return_value=TARGET),
            patch.object(installer, "github_token", return_value="synthetic-token"),
            patch.object(installer, "GitHubApi", return_value=object()),
            patch.object(installer, "resolve_qualified_package", return_value=(files, manifest)),
            patch.object(installer, "install_package", side_effect=AssertionError("qualified mode activated")),
            patch.dict(os.environ, {"HOME": str(fake_home), "CODEX_HOME": str(fake_home / ".codex")}),
            contextlib.redirect_stdout(output),
        ):
            result = installer.main([
                "--qualified-run-id", "4321",
                "--record-id", "synthetic-qualified-q3-s1",
                "--stage-dir", str(destination),
            ])
        self.assertEqual(result, 0)
        self.assertIn("(not installed)", output.getvalue())
        self.assertTrue((destination / "qualified-complete-package.json").is_file())
        self.assertTrue(all((destination / name).is_file() for name in package_names))
        self.assertFalse(fake_home.exists())

    def test_qualified_stage_copy_or_provenance_failure_leaves_destination_absent(self) -> None:
        files, manifest, temporary, _ = self.qualified_stage_fixture()
        destination = self.root / "copy-failure-stage"
        with patch.object(installer.shutil, "copyfile", side_effect=OSError("synthetic copy failure")):
            with self.assertRaisesRegex(OSError, "synthetic copy failure"):
                installer.stage_qualified_package(files, manifest, str(destination), temporary)
        self.assertFalse(destination.exists())
        self.assertEqual(list(self.root.glob(".copy-failure-stage.staging-*")), [])

        destination = self.root / "provenance-failure-stage"
        with patch.object(installer.json, "dump", side_effect=OSError("synthetic provenance failure")):
            with self.assertRaisesRegex(OSError, "synthetic provenance failure"):
                installer.stage_qualified_package(files, manifest, str(destination), temporary)
        self.assertFalse(destination.exists())
        self.assertEqual(list(self.root.glob(".provenance-failure-stage.staging-*")), [])

    def test_qualified_stage_no_replace_preserves_racing_destination(self) -> None:
        files, manifest, temporary, _ = self.qualified_stage_fixture()
        destination = self.root / "racing-stage"
        original_publish = installer._publish_directory_noreplace

        def create_racing_destination(source: Path, target: Path) -> None:
            target.mkdir()
            (target / "sentinel").write_text("pre-existing owner data")
            original_publish(source, target)

        with patch.object(installer, "_publish_directory_noreplace", side_effect=create_racing_destination):
            with self.assertRaisesRegex(installer.InstallError, "appeared before atomic publication"):
                installer.stage_qualified_package(files, manifest, str(destination), temporary)
        self.assertEqual((destination / "sentinel").read_text(), "pre-existing owner data")
        self.assertEqual(list(self.root.glob(".racing-stage.staging-*")), [])

    def test_qualified_stage_rejects_existing_traversal_and_unsafe_parent(self) -> None:
        temporary = self.root / "temporary"
        temporary.mkdir()
        existing = self.root / "existing-stage"
        existing.mkdir()
        with self.assertRaisesRegex(installer.InstallError, "already exists"):
            installer.stage_qualified_package({}, {}, str(existing), temporary)
        with self.assertRaisesRegex(installer.InstallError, "path traversal"):
            installer.stage_qualified_package({}, {}, str(self.root / ".." / "outside"), temporary)
        parent_link = self.root / "stage-parent-link"
        parent_link.symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(installer.InstallError, "unsafe qualified stage parent"):
            installer.stage_qualified_package({}, {}, str(parent_link / "stage"), temporary)

    def test_qualified_evidence_zip_rejects_wrong_digest_traversal_link_and_duplicate(self) -> None:
        cases = []
        cases.append((zip_bytes({"evidence.json": b"{}"}), "wrong-digest", "digest mismatch"))
        cases.append((zip_bytes({"../outside": b"escape"}), "traversal", "unsafe qualification evidence"))

        link_buffer = io.BytesIO()
        with zipfile.ZipFile(link_buffer, "w") as archive:
            link = zipfile.ZipInfo("evidence-link")
            link.create_system = 3
            link.external_attr = (stat.S_IFLNK | 0o777) << 16
            archive.writestr(link, "../../outside")
        cases.append((link_buffer.getvalue(), "link", "unsafe qualification evidence"))

        duplicate_buffer = io.BytesIO()
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            with zipfile.ZipFile(duplicate_buffer, "w") as archive:
                archive.writestr("duplicate.json", b"one")
                archive.writestr("duplicate.json", b"two")
        cases.append((duplicate_buffer.getvalue(), "duplicate", "unsafe qualification evidence"))

        for payload, label, message in cases:
            with self.subTest(case=label):
                path = self.root / f"evidence-{label}.zip"
                path.write_bytes(payload)
                digest = "0" * 64 if label == "wrong-digest" else sha(payload)
                with self.assertRaisesRegex(installer.InstallError, message):
                    installer._safe_evidence_zip(path, digest, self.root / f"evidence-{label}-out")

    def test_qualified_package_tar_rejects_traversal_link_and_duplicate(self) -> None:
        required = {
            "codex-package.json": b"{}",
            "bin/codex": elf(),
            "bin/codex-code-mode-host": elf(),
            "codex-path/rg": elf(),
            "codex-resources/bwrap": elf(),
        }
        cases = {
            "traversal": [*((name, data, "file") for name, data in required.items()), ("../outside", b"escape", "file")],
            "link": [*((name, data, "file") for name, data in required.items() if name != "bin/codex"), ("bin/codex", b"", "symlink")],
            "duplicate": [*((name, data, "file") for name, data in required.items()), ("bin/codex", elf(), "file")],
        }
        for label, entries in cases.items():
            with self.subTest(case=label):
                payload = raw_tar(entries)
                path = self.root / f"package-{label}.tar.zst"
                path.write_bytes(payload)
                process = FakeZstdProcess(payload)
                with patch.object(installer.subprocess, "Popen", return_value=process):
                    with self.assertRaises(installer.InstallError):
                        installer._safe_zstd_package(path, self.root / f"package-{label}-out", TARGET)

    def test_hc_full_junit_accepts_q3_s1_inventory_and_rejects_missing_case(self) -> None:
        plan = installer.QUALIFICATION_TESTS_BY_HOST[installer.QUALIFICATION_HOST_SHA][
            (installer.Q3_SHA, installer.S1_SHA)
        ]
        cases = [
            installer.ET.Element("testcase", name=f"test_packaged_historical_upgrade_and_reopen[{name}]")
            for name in sorted(installer.STATE_POSITIVE)
        ]
        cases.extend(
            installer.ET.Element("testcase", name=f"test_packaged_historical_rejection_preserves_preimage[{name}]")
            for name in sorted(installer.STATE_NEGATIVE)
        )
        cases.extend(installer.ET.Element("testcase", name=name) for name in sorted(plan["plain"]))
        installer._verify_full_junit(cases, plan["plain"])
        with self.assertRaisesRegex(installer.InstallError, "inventory"):
            installer._verify_full_junit(cases[:-1], plan["plain"])

    def resolve_exact(self, api: FakeApi | None = None):
        api = api or make_api((self.core_zip, self.host_zip))
        args = SimpleNamespace(run_id=1001, host_run_id=1002, branch=None)
        return installer.resolve_artifacts(api, args, TARGET, self.root / "download")

    def test_exact_run_pair_builds_verified_three_binary_package(self) -> None:
        core, host = self.resolve_exact()
        files, manifest = installer.package_files(core, host, self.root / "package")
        self.assertEqual(set(files), set(installer.EXECUTABLES))
        self.assertEqual(manifest["commit"], SOURCE_SHA)
        self.assertEqual(manifest["workflowRuns"], {"core": 1001, "host": 1002})
        self.assertNotEqual(core.metadata["previewVersion"], host.metadata["previewVersion"])

    def test_exact_run_requires_explicit_companion(self) -> None:
        with self.assertRaises(SystemExit):
            installer.parse_args(["--run-id", "1001"])

    def test_run_with_workflow_path_suffix_is_rejected(self) -> None:
        run = {
            "id": 1001,
            "event": "workflow_dispatch",
            "status": "completed",
            "conclusion": "success",
            "repository": {"full_name": installer.REPOSITORY},
            "path": f"{installer.WORKFLOW_PATH}.unexpected@refs/heads/main",
            "head_sha": WORKFLOW_SHA,
        }
        with self.assertRaisesRegex(installer.InstallError, "not bound"):
            installer.validate_run(run)

    def test_authenticated_redirect_rejects_https_downgrade(self) -> None:
        request = urllib.request.Request("https://api.github.com/repos/example/actions/artifact")
        with self.assertRaises(urllib.error.HTTPError):
            installer.SafeRedirectHandler().redirect_request(
                request,
                None,
                302,
                "Found",
                {},
                "http://downloads.example.invalid/artifact.zip",
            )

    def test_source_mismatch_is_rejected(self) -> None:
        api = make_api((self.core_zip, action_artifact("host", 1002, source_sha="c" * 40)))
        with self.assertRaisesRegex(installer.InstallError, "do not match"):
            self.resolve_exact(api)

    def test_host_checksum_mismatch_is_rejected(self) -> None:
        bad_host = rewrite_json_member(
            action_artifact("host", 1002),
            "host.json",
            lambda metadata: metadata.update(binarySha256="0" * 64),
        )
        api = make_api((self.core_zip, bad_host))
        with self.assertRaisesRegex(installer.InstallError, "binary SHA-256"):
            self.resolve_exact(api)

    def test_branch_selection_uses_latest_core_source_not_later_old_host(self) -> None:
        old_source = "c" * 40
        new_core = action_artifact("core", 1001, source_sha=SOURCE_SHA)
        new_host = action_artifact("host", 1002, source_sha=SOURCE_SHA)
        old_host = action_artifact("host", 1003, source_sha=old_source)
        api = FakeApi(
            {
                1: ("core", new_core),
                2: ("host", new_host),
                3: ("host", old_host),
            },
            {1001: "2026-09-29T00:00:00Z", 1002: "2026-09-29T00:00:01Z", 1003: "2026-09-30T00:00:00Z"},
        )
        args = SimpleNamespace(run_id=None, host_run_id=None, branch=BRANCH)
        core, host = installer.resolve_artifacts(api, args, TARGET, self.root / "branch-download")
        self.assertEqual(core.source_sha, SOURCE_SHA)
        self.assertEqual(host.source_sha, SOURCE_SHA)
        self.assertEqual(core.run["id"], 1001)
        self.assertEqual(host.run["id"], 1002)

    def test_branch_mode_fails_closed_when_companion_is_missing(self) -> None:
        api = FakeApi({21: ("core", action_artifact("core", 1021))})
        args = SimpleNamespace(run_id=None, host_run_id=None, branch=BRANCH)
        with self.assertRaisesRegex(installer.InstallError, "provide --host-run-id"):
            installer.resolve_artifacts(api, args, TARGET, self.root / "branch-no-host")

    def test_unsafe_tar_member_is_rejected(self) -> None:
        unsafe_core_tar = tar_bytes({"../codex": elf(), "codex-responses-api-proxy": elf()})
        core_version = f"1.2.3-ci.1+g{SOURCE_SHA[:8]}"
        core_zip = zip_bytes(
            {
                f"codex-sedna-preview-{core_version.replace('+', '__')}-{TARGET}.tar.gz": unsafe_core_tar,
                "core.json": json.dumps(
                    {
                        "repository": installer.REPOSITORY,
                        "commit": SOURCE_SHA,
                        "ref": BRANCH,
                        "previewVersion": core_version,
                        "workflow": "https://github.com/sednalabs/codex/actions/runs/1001",
                    }
                ).encode(),
            }
        )
        core, host = self.resolve_exact(make_api((core_zip, self.host_zip)))
        with self.assertRaisesRegex(installer.InstallError, "unsafe or unexpected"):
            installer.package_files(core, host, self.root / "unsafe-package")

    def test_bad_actions_digest_is_rejected(self) -> None:
        api = make_api((self.core_zip, self.host_zip))
        api.records[1]["digest"] = "sha256:" + "0" * 64
        with self.assertRaisesRegex(installer.InstallError, "digest mismatch"):
            self.resolve_exact(api)

    def test_actions_zip_symlink_is_rejected(self) -> None:
        archive_bytes = io.BytesIO()
        with zipfile.ZipFile(archive_bytes, "w") as archive:
            info = zipfile.ZipInfo("payload-link")
            info.create_system = 3
            info.external_attr = (stat.S_IFLNK | 0o777) << 16
            archive.writestr(info, "../../outside")
        zip_path = self.root / "symlink.zip"
        zip_path.write_bytes(archive_bytes.getvalue())
        digest = sha(archive_bytes.getvalue())
        with self.assertRaisesRegex(installer.InstallError, "unsafe or duplicate"):
            installer.safe_zip_extract(zip_path, digest, self.root / "symlink-out")

    def test_dry_run_does_not_read_or_write_home(self) -> None:
        core, host = self.resolve_exact()
        files, manifest = installer.package_files(core, host, self.root / "dry-package")
        fake_home = self.root / "unopened-home"
        output = io.StringIO()
        with patch.dict(os.environ, {"HOME": str(fake_home)}), contextlib.redirect_stdout(output):
            installer.install_package(files, manifest, dry_run=True)
        self.assertFalse(fake_home.exists())
        self.assertIn("dry-run: verified", output.getvalue())

    def test_public_just_recipe_dry_run_uses_loopback_fixture_api(self) -> None:
        api = make_api((self.core_zip, self.host_zip))
        artifact_server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), RedirectArtifactHandler)
        artifact_server.fixture_api = api  # type: ignore[attr-defined]
        artifact_server.authorization_headers = []  # type: ignore[attr-defined]
        artifact_thread = threading.Thread(target=artifact_server.serve_forever, daemon=True)
        artifact_thread.start()
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FixtureHandler)
        server.fixture_api = api  # type: ignore[attr-defined]
        server.redirect_artifact_root = f"http://127.0.0.1:{artifact_server.server_port}"
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        fake_home = self.root / "recipe-home"
        fixture_api_root = f"http://127.0.0.1:{server.server_port}"
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(fake_home),
                "GH_TOKEN": "synthetic-fixture-token",
                "NO_PROXY": "127.0.0.1,localhost,::1",
                "no_proxy": "127.0.0.1,localhost,::1",
                "SEDNA_BRANCH_INSTALLER_TESTING": "1",
                "SEDNA_BRANCH_INSTALLER_TEST_API_ROOT": fixture_api_root,
            }
        )
        try:
            result = subprocess.run(
                [
                    "just",
                    "install-branch-artifact",
                    "--run-id",
                    "1001",
                    "--host-run-id",
                    "1002",
                    "--dry-run",
                ],
                cwd=INSTALLER_PATH.parents[1],
                env=env,
                check=True,
                capture_output=True,
                text=True,
                timeout=60,
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            artifact_server.shutdown()
            artifact_server.server_close()
            artifact_thread.join(timeout=5)
        self.assertIn("dry-run: verified", result.stdout)
        self.assertIn("core run 1001 and host run 1002", result.stdout)
        self.assertEqual(artifact_server.authorization_headers, [(None, None, None), (None, None, None)])
        self.assertFalse(fake_home.exists())

    def test_activation_preserves_previous_package_and_switches_launcher(self) -> None:
        core, host = self.resolve_exact()
        files, manifest = installer.package_files(core, host, self.root / "activate-package")
        home = self.root / "home"
        home.mkdir()
        codex_home = home / "custom-codex-home"
        install_root = codex_home / "packages" / "standalone"
        old = install_root / "releases" / "old"
        (old / "bin").mkdir(parents=True)
        (old / "bin" / "codex").write_text("old binary", encoding="utf-8")
        (old / "codex").symlink_to("bin/codex")
        (install_root / "current").symlink_to(old)
        visible = home / "custom-install-bin"
        visible.mkdir(parents=True)
        (visible / "codex").symlink_to(old / "bin" / "codex")
        with patch.dict(
            os.environ,
            {
                "HOME": str(home),
                "CODEX_HOME": str(codex_home),
                "CODEX_INSTALL_DIR": str(visible),
            },
            clear=False,
        ):
            installer.install_package(files, manifest, dry_run=False)
        new_release = install_root / "releases" / f"branch-{SOURCE_SHA}-{TARGET}-r1001-h1002"
        self.assertTrue((new_release / "bin" / "codex").is_file())
        self.assertTrue((new_release / "bin" / "codex-code-mode-host").is_file())
        self.assertEqual(os.readlink(new_release / "codex"), "bin/codex")
        self.assertEqual(os.readlink(install_root / "current"), str(new_release))
        self.assertEqual(os.readlink(visible / "codex"), str(install_root / "current" / "bin" / "codex"))
        self.assertTrue((old / "codex").exists())

    def test_activation_failure_rolls_back_both_symlinks(self) -> None:
        core, host = self.resolve_exact()
        files, manifest = installer.package_files(core, host, self.root / "rollback-package")
        home = self.root / "rollback-home"
        home.mkdir()
        codex_home = home / "custom-codex-home"
        install_root = codex_home / "packages" / "standalone"
        old = install_root / "releases" / "old"
        (old / "bin").mkdir(parents=True)
        (old / "bin" / "codex").write_text("old binary", encoding="utf-8")
        (old / "codex").symlink_to("bin/codex")
        (install_root / "current").symlink_to(old)
        visible = home / "custom-install-bin"
        visible.mkdir(parents=True)
        (visible / "codex").symlink_to(old / "bin" / "codex")
        env = {
            "HOME": str(home),
            "CODEX_HOME": str(codex_home),
            "CODEX_INSTALL_DIR": str(visible),
            "SEDNA_BRANCH_INSTALLER_TESTING": "1",
            "SEDNA_BRANCH_INSTALLER_TEST_FAIL_AT": "after-visible",
        }
        with patch.dict(os.environ, env, clear=False):
            with self.assertRaisesRegex(installer.InstallError, "injected activation failure"):
                installer.install_package(files, manifest, dry_run=False)
        self.assertEqual(os.readlink(install_root / "current"), str(old))
        self.assertEqual(os.readlink(visible / "codex"), str(old / "bin" / "codex"))
        self.assertTrue((old / "codex").exists())

    def test_aarch64_elf_machine_is_supported(self) -> None:
        path = self.root / "codex-arm"
        path.write_bytes(elf(183))
        installer.elf_machine(path, "aarch64-unknown-linux-gnu")


if __name__ == "__main__":
    unittest.main()
