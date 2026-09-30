from __future__ import annotations

import contextlib
import hashlib
import importlib.machinery
import importlib.util
import io
import json
import os
import stat
import struct
import sys
import tarfile
import tempfile
import unittest
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
TARGET = "x86_64-unknown-linux-gnu"
MACHINE = 62
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
        api = FakeApi({21: ("core", self.core_zip)})
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

    def test_activation_preserves_previous_package_and_switches_launcher(self) -> None:
        core, host = self.resolve_exact()
        files, manifest = installer.package_files(core, host, self.root / "activate-package")
        home = self.root / "home"
        home.mkdir()
        old = home / ".codex" / "packages" / "standalone" / "releases" / "old"
        old.mkdir(parents=True)
        (old / "codex").write_text("old binary", encoding="utf-8")
        install_root = home / ".codex" / "packages" / "standalone"
        (install_root / "current").symlink_to(old)
        visible = home / ".local" / "bin"
        visible.mkdir(parents=True)
        (visible / "codex").symlink_to(old / "codex")
        with patch.dict(os.environ, {"HOME": str(home)}, clear=False):
            installer.install_package(files, manifest, dry_run=False)
        new_release = install_root / "releases" / f"branch-{SOURCE_SHA}-{TARGET}-r1001-h1002"
        self.assertTrue((new_release / "bin" / "codex").is_file())
        self.assertTrue((new_release / "bin" / "codex-code-mode-host").is_file())
        self.assertEqual(os.readlink(new_release / "codex"), "bin/codex")
        self.assertEqual(os.readlink(install_root / "current"), str(new_release))
        self.assertEqual(os.readlink(visible / "codex"), str(install_root / "current" / "codex"))
        self.assertTrue((old / "codex").exists())

    def test_activation_failure_rolls_back_both_symlinks(self) -> None:
        core, host = self.resolve_exact()
        files, manifest = installer.package_files(core, host, self.root / "rollback-package")
        home = self.root / "rollback-home"
        home.mkdir()
        old = home / ".codex" / "packages" / "standalone" / "releases" / "old"
        old.mkdir(parents=True)
        (old / "codex").write_text("old binary", encoding="utf-8")
        install_root = home / ".codex" / "packages" / "standalone"
        (install_root / "current").symlink_to(old)
        visible = home / ".local" / "bin"
        visible.mkdir(parents=True)
        (visible / "codex").symlink_to(old / "codex")
        env = {
            "HOME": str(home),
            "SEDNA_BRANCH_INSTALLER_TESTING": "1",
            "SEDNA_BRANCH_INSTALLER_TEST_FAIL_AT": "after-visible",
        }
        with patch.dict(os.environ, env, clear=False):
            with self.assertRaisesRegex(installer.InstallError, "injected activation failure"):
                installer.install_package(files, manifest, dry_run=False)
        self.assertEqual(os.readlink(install_root / "current"), str(old))
        self.assertEqual(os.readlink(visible / "codex"), str(old / "codex"))
        self.assertTrue((old / "codex").exists())

    def test_aarch64_elf_machine_is_supported(self) -> None:
        path = self.root / "codex-arm"
        path.write_bytes(elf(183))
        installer.elf_machine(path, "aarch64-unknown-linux-gnu")


if __name__ == "__main__":
    unittest.main()
