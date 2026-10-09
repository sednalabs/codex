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
import urllib.parse
import urllib.request
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


def action_artifact(
    run_id: int,
    *,
    source_sha: str = SOURCE_SHA,
    ref: str = BRANCH,
    target: str = TARGET,
    package_files: dict[str, bytes] | None = None,
    host_only: bool = False,
) -> bytes:
    preview_version = f"1.2.3-ci.{run_id}+g{source_sha[:8]}"
    version = preview_version.replace("+", "__")
    archive_base = "codex-sedna-preview-host" if host_only else "codex-sedna-preview"
    archive_name = f"{archive_base}-{version}-{target}.tar.gz"
    if host_only:
        binaries = {"codex-code-mode-host": elf()}
    else:
        binaries = package_files or {name: elf() for name in installer.EXECUTABLES}
    metadata = {
        "repository": installer.REPOSITORY,
        "commit": source_sha,
        "ref": ref,
        "previewVersion": preview_version,
        "workflow": f"https://github.com/{installer.REPOSITORY}/actions/runs/{run_id}",
    }
    if target == "aarch64-unknown-linux-gnu":
        metadata["target"] = target
    if host_only:
        metadata["artifact"] = "codex-code-mode-host"
        metadata["workflowCommit"] = WORKFLOW_SHA
        metadata["binarySha256"] = sha(binaries["codex-code-mode-host"])
    files = {
        archive_name: tar_bytes({f"./{name}": data for name, data in binaries.items()}),
        f"{archive_base}-{version}-{target}.json": json.dumps(metadata).encode(),
    }
    return zip_bytes(files)


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
                self.send_response(302)
                self.send_header("Location", f"{redirect_root}{self.path}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            artifact_id = int(self.path.rsplit("/", 2)[1])
            body = fixture_api.payloads[artifact_id]
            content_type = "application/zip"
        else:
            try:
                body = json.dumps(fixture_api.json(self.path)).encode()
                content_type = "application/json"
            except (AssertionError, KeyError, ValueError):
                body = b"{}"
                content_type = "application/json"
                self.send_response(404)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        return


class RedirectArtifactHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        fixture_api: FakeApi = self.server.fixture_api  # type: ignore[attr-defined]
        self.server.authorization_headers.append(  # type: ignore[attr-defined]
            (
                self.headers.get("Authorization"),
                self.headers.get("Proxy-authorization"),
                self.headers.get("Cookie"),
            )
        )
        artifact_id = int(self.path.rsplit("/", 2)[1])
        body = fixture_api.payloads[artifact_id]
        self.send_response(200)
        self.send_header("Content-Type", "application/zip")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        return


class FakeApi:
    def __init__(self, artifacts: dict[int, tuple[int, bytes]], created: dict[int, str] | None = None):
        self.payloads = {artifact_id: data for artifact_id, (_, data) in artifacts.items()}
        self.records: dict[int, dict] = {}
        self.runs: dict[int, dict] = {}
        self.created = created or {}
        self.artifacts_by_run: dict[int, list[dict]] = {}
        for index, (artifact_id, (run_id, data)) in enumerate(artifacts.items()):
            with zipfile.ZipFile(io.BytesIO(data)) as archive:
                metadata_name = next(name for name in archive.namelist() if name.endswith(".json"))
                metadata = json.loads(archive.read(metadata_name))
            source_sha = metadata["commit"]
            ref = metadata["ref"]
            target = metadata.get("target", TARGET)
            self.records[artifact_id] = {
                "id": artifact_id,
                "name": f"sedna-branch-{installer.branch_slug(ref)}-{source_sha[:8]}-{target}",
                "digest": f"sha256:{sha(data)}",
                "expired": False,
                "workflow_run": {"id": run_id},
            }
            self.runs[run_id] = {
                "id": run_id,
                "event": "workflow_dispatch",
                "status": "completed",
                "conclusion": "success",
                "repository": {"full_name": installer.REPOSITORY},
                "path": f"{installer.WORKFLOW_PATH}@refs/heads/main",
                "head_branch": ref,
                "head_sha": WORKFLOW_SHA,
                "created_at": self.created.get(run_id, f"2026-10-09T00:00:{index:02d}Z"),
            }
            self.artifacts_by_run.setdefault(run_id, []).append(self.records[artifact_id])
        self.runs_payload = list(self.runs.values())

    def json(self, path: str):
        parsed = urllib.parse.urlsplit(path)
        if "/actions/workflows/" in parsed.path and "/runs" in parsed.path:
            query = urllib.parse.parse_qs(parsed.query)
            page = query.get("page", ["1"])[0]
            branch = query.get("branch", [None])[0]
            if page != "1":
                return {"workflow_runs": []}
            runs = self.runs_payload
            if branch is not None:
                runs = [run for run in runs if run.get("head_branch") == branch]
            return {"workflow_runs": runs}
        if "/actions/runs/" in parsed.path and "/artifacts" in parsed.path:
            run_id = int(parsed.path.split("/actions/runs/")[1].split("/")[0])
            return {"artifacts": self.artifacts_by_run.get(run_id, [])}
        if "/actions/runs/" in parsed.path:
            run_id = int(parsed.path.rsplit("/", 1)[-1])
            return self.runs[run_id]
        raise AssertionError(f"unexpected API path: {path}")

    def download(self, artifact_id: int, destination: Path) -> str:
        data = self.payloads[artifact_id]
        destination.write_bytes(data)
        return sha(data)


def make_api(artifacts: dict[int, tuple[int, bytes]], created: dict[int, str] | None = None) -> FakeApi:
    return FakeApi(artifacts, created)


class BranchArtifactInstallerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.package_zip = action_artifact(1001)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def resolve_exact(self, api: FakeApi | None = None):
        api = api or make_api({41: (1001, self.package_zip)})
        args = SimpleNamespace(run_id=1001, branch=None)
        return installer.resolve_artifact(api, args, TARGET, self.root / "download")

    def test_exact_run_id_resolves_one_verified_full_package(self) -> None:
        artifact = self.resolve_exact()
        files, manifest = installer.package_files(artifact, self.root / "package")
        self.assertEqual(set(files), set(installer.EXECUTABLES))
        self.assertEqual(manifest["commit"], SOURCE_SHA)
        self.assertEqual(manifest["workflowRun"], 1001)
        self.assertEqual(manifest["workflowHeadSha"], WORKFLOW_SHA)
        self.assertEqual(manifest["target"], TARGET)
        self.assertEqual(manifest["artifact"]["id"], 41)
        self.assertEqual(manifest["artifact"]["archiveSha256"], sha(next(p for p in artifact.files.values() if p.name.endswith(".tar.gz")).read_bytes()))

    def test_exact_run_id_is_sufficient_and_host_run_option_is_not_supported(self) -> None:
        args = installer.parse_args(["--run-id", "1001"])
        self.assertEqual(args.run_id, 1001)
        with self.assertRaises(SystemExit):
            installer.parse_args(["--run-id", "1001", "--host-run-id", "1002"])

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
                request, None, 302, "Found", {}, "http://downloads.example.invalid/artifact.zip"
            )

    def test_artifact_source_must_match_filename_and_preview_version(self) -> None:
        bad_package = rewrite_json_member(
            self.package_zip,
            "codex-sedna-preview-1.2.3-ci.1001__gaaaaaaaa-" + TARGET + ".json",
            lambda metadata: metadata.update(commit="c" * 40),
        )
        api = make_api({41: (1001, bad_package)})
        with self.assertRaisesRegex(installer.InstallError, "previewVersion does not match"):
            self.resolve_exact(api)

    def test_host_only_artifact_is_rejected(self) -> None:
        api = make_api({41: (1001, action_artifact(1001, host_only=True))})
        with self.assertRaisesRegex(installer.InstallError, "complete branch package"):
            self.resolve_exact(api)

    def test_branch_selection_chooses_newest_matching_package_source(self) -> None:
        old_sha = "c" * 40
        api = make_api(
            {
                41: (1001, action_artifact(1001, source_sha=old_sha)),
                42: (1002, action_artifact(1002, source_sha=SOURCE_SHA)),
            },
            {1001: "2026-10-08T00:00:00Z", 1002: "2026-10-09T00:00:00Z"},
        )
        args = SimpleNamespace(run_id=None, branch=BRANCH)
        artifact = installer.resolve_artifact(api, args, TARGET, self.root / "branch-download")
        self.assertEqual(artifact.source_sha, SOURCE_SHA)
        self.assertEqual(artifact.run["id"], 1002)

    def test_branch_selection_skips_expired_latest_artifact(self) -> None:
        api = make_api(
            {
                41: (1001, action_artifact(1001)),
                42: (1002, action_artifact(1002)),
            },
            {1001: "2026-10-08T00:00:00Z", 1002: "2026-10-09T00:00:00Z"},
        )
        api.records[42]["expired"] = True
        args = SimpleNamespace(run_id=None, branch=BRANCH)
        artifact = installer.resolve_artifact(api, args, TARGET, self.root / "branch-expired")
        self.assertEqual(artifact.run["id"], 1001)

    def test_branch_lookup_rejects_multiple_artifacts_in_selected_run(self) -> None:
        api = make_api({41: (1001, action_artifact(1001)), 42: (1001, action_artifact(1001))})
        args = SimpleNamespace(run_id=None, branch=BRANCH)
        with self.assertRaisesRegex(installer.InstallError, "2 usable artifacts"):
            installer.resolve_artifact(api, args, TARGET, self.root / "branch-ambiguous")

    def test_branch_selection_fails_closed_if_newest_match_is_host_only(self) -> None:
        api = make_api(
            {
                41: (1001, action_artifact(1001)),
                42: (1002, action_artifact(1002, host_only=True)),
            },
            {1001: "2026-10-08T00:00:00Z", 1002: "2026-10-09T00:00:00Z"},
        )
        args = SimpleNamespace(run_id=None, branch=BRANCH)
        with self.assertRaisesRegex(installer.InstallError, "complete branch package"):
            installer.resolve_artifact(api, args, TARGET, self.root / "branch-host-only")

    def test_wrong_target_has_no_eligible_artifact(self) -> None:
        api = make_api({41: (1001, self.package_zip)})
        with self.assertRaisesRegex(installer.InstallError, "0 usable artifacts"):
            installer.resolve_artifact(api, SimpleNamespace(run_id=1001, branch=None), "aarch64-unknown-linux-gnu", self.root / "wrong-target")

    def test_artifact_must_be_attached_to_exact_run(self) -> None:
        api = make_api({41: (1001, self.package_zip)})
        api.records[41]["workflow_run"]["id"] = 1002
        with self.assertRaisesRegex(installer.InstallError, "not attached"):
            self.resolve_exact(api)

    def test_bad_actions_digest_is_rejected(self) -> None:
        api = make_api({41: (1001, self.package_zip)})
        api.records[41]["digest"] = "sha256:" + "0" * 64
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
        with self.assertRaisesRegex(installer.InstallError, "unsafe or duplicate"):
            installer.safe_zip_extract(zip_path, sha(archive_bytes.getvalue()), self.root / "symlink-out")

    def test_unsafe_package_member_is_rejected(self) -> None:
        unsafe_files = {"../codex": elf(), "codex-code-mode-host": elf(), "codex-responses-api-proxy": elf()}
        bad_package = action_artifact(1001, package_files=unsafe_files)
        artifact = self.resolve_exact(make_api({41: (1001, bad_package)}))
        with self.assertRaisesRegex(installer.InstallError, "unsafe or unexpected"):
            installer.package_files(artifact, self.root / "unsafe-package")

    def test_wrong_elf_architecture_is_rejected(self) -> None:
        wrong_machine = 183 if MACHINE == 62 else 62
        binaries = {name: elf(wrong_machine) for name in installer.EXECUTABLES}
        artifact = self.resolve_exact(make_api({41: (1001, action_artifact(1001, package_files=binaries))}))
        with self.assertRaisesRegex(installer.InstallError, "expected"):
            installer.package_files(artifact, self.root / "wrong-arch")

    def test_dry_run_does_not_read_or_write_home(self) -> None:
        artifact = self.resolve_exact()
        files, manifest = installer.package_files(artifact, self.root / "dry-package")
        fake_home = self.root / "unopened-home"
        output = io.StringIO()
        with patch.dict(os.environ, {"HOME": str(fake_home)}), contextlib.redirect_stdout(output):
            installer.install_package(files, manifest, dry_run=True)
        self.assertFalse(fake_home.exists())
        self.assertIn("dry-run: verified", output.getvalue())
        self.assertIn("workflow run 1001", output.getvalue())

    def test_public_just_recipe_dry_run_uses_loopback_fixture_api(self) -> None:
        api = make_api({41: (1001, self.package_zip)})
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
                ["just", "install-branch-artifact", "--run-id", "1001", "--dry-run"],
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
        self.assertIn("workflow run 1001", result.stdout)
        self.assertEqual(artifact_server.authorization_headers, [(None, None, None)])
        self.assertFalse(fake_home.exists())

    def test_activation_preserves_profile_files_and_previous_package(self) -> None:
        artifact = self.resolve_exact()
        files, manifest = installer.package_files(artifact, self.root / "activate-package")
        home = self.root / "home"
        home.mkdir()
        codex_home = home / "custom-codex-home"
        codex_home.mkdir()
        protected = {
            "config.toml": b"profile=preserved\n",
            "auth.json": b"synthetic-auth-fixture\n",
            "state.sqlite": b"synthetic-state-fixture\n",
        }
        for name, value in protected.items():
            (codex_home / name).write_bytes(value)
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
            {"HOME": str(home), "CODEX_HOME": str(codex_home), "CODEX_INSTALL_DIR": str(visible)},
            clear=False,
        ):
            installer.install_package(files, manifest, dry_run=False)
        new_release = install_root / "releases" / f"branch-{SOURCE_SHA}-{TARGET}-r1001"
        self.assertTrue((new_release / "bin" / "codex").is_file())
        self.assertTrue((new_release / "bin" / "codex-code-mode-host").is_file())
        self.assertTrue((new_release / "bin" / "codex-responses-api-proxy").is_file())
        self.assertEqual(os.readlink(new_release / "codex"), "bin/codex")
        self.assertEqual(os.readlink(install_root / "current"), str(new_release))
        self.assertEqual(os.readlink(visible / "codex"), str(install_root / "current" / "bin" / "codex"))
        self.assertTrue((old / "codex").exists())
        self.assertEqual({name: (codex_home / name).read_bytes() for name in protected}, protected)

    def test_activation_failure_rolls_back_both_symlinks(self) -> None:
        artifact = self.resolve_exact()
        files, manifest = installer.package_files(artifact, self.root / "rollback-package")
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
