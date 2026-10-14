#!/usr/bin/env python3

from __future__ import annotations

from io import BytesIO
import json
import os
from pathlib import Path
from types import SimpleNamespace
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import install_branch_artifact
from install_branch_artifact import (
    BINARIES,
    Refusal,
    extract_binaries,
    expected_archive_names,
    parse_run_title,
    preview_root,
    safe_member_name,
    validate_manifest,
    validate_hosted_jobs,
    validate_run,
    unpack_preview_zip,
)


SOURCE_SHA = "12345678" + "a" * 32
HOST_SHA = "b" * 40
RUN_URL = "https://github.com/sednalabs/codex/actions/runs/42"


def elf_header(machine: int) -> bytes:
    header = bytearray(20)
    header[:6] = b"\x7fELF\x02\x01"
    header[18:20] = machine.to_bytes(2, "little")
    return bytes(header)


def preview_tar(machine: int, names: tuple[str, ...] = BINARIES) -> BytesIO:
    payload = BytesIO()
    with tarfile.open(fileobj=payload, mode="w:gz") as archive:
        for name in names:
            item = tarfile.TarInfo(f"./{name}")
            item.mode = 0o755
            item.size = len(elf_header(machine))
            archive.addfile(item, BytesIO(elf_header(machine)))
    payload.seek(0)
    return payload


class InstallBranchArtifactTest(unittest.TestCase):
    def test_run_title_binds_ref_platform_and_rejects_host_only(self) -> None:
        normal_run = {
            "id": 42,
            "run_number": 42,
            "path": ".github/workflows/sedna-branch-build.yml",
            "workflow_id": 8,
            "event": "workflow_dispatch",
            "status": "completed",
            "conclusion": "success",
            "head_branch": "main",
            "head_sha": HOST_SHA,
            "display_title": "sedna-branch-build [target=feature/demo "
            "platform=linux-aarch64 host_only=false]",
        }
        self.assertEqual(
            parse_run_title(normal_run), ("feature/demo", "linux-aarch64", False)
        )
        self.assertEqual(
            validate_run(
                normal_run,
                repository={"default_branch": "main"},
                workflow_id=8,
                expected_ref="feature/demo",
                expected_target="linux-aarch64",
            ),
            ("feature/demo", "linux-aarch64", "aarch64-unknown-linux-gnu"),
        )
        host_only = {
            **normal_run,
            "display_title": "sedna-branch-build [target=feature/demo "
            "platform=linux-x86_64 host_only=true]",
        }
        with self.assertRaises(Refusal):
            validate_run(
                host_only,
                repository={"default_branch": "main"},
                workflow_id=8,
                expected_ref="feature/demo",
                expected_target="linux-x86_64",
            )

    def test_archive_names_preserve_preview_version_and_target(self) -> None:
        self.assertEqual(
            expected_archive_names(
                "1.2.3-ci.42+g12345678", "x86_64-unknown-linux-gnu"
            ),
            (
                "codex-sedna-preview-1.2.3-ci.42__g12345678-"
                "x86_64-unknown-linux-gnu.tar.gz",
                "codex-sedna-preview-1.2.3-ci.42__g12345678-"
                "x86_64-unknown-linux-gnu.json",
            ),
        )

    def test_safe_member_name_rejects_traversal_and_nested_paths(self) -> None:
        self.assertEqual(safe_member_name("./codex"), "codex")
        with self.assertRaises(Refusal):
            safe_member_name("../codex")
        with self.assertRaises(Refusal):
            safe_member_name("bin/codex")

    def test_preview_root_is_separate_from_codex_home(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            data_home = root / "data"
            codex_home = root / "codex-home"
            with patch.dict(
                os.environ,
                {
                    "XDG_DATA_HOME": str(data_home),
                    "CODEX_HOME": str(codex_home),
                },
            ):
                self.assertEqual(
                    preview_root(), data_home / "codex" / "branch-previews"
                )
            with patch.dict(
                os.environ,
                {
                    "XDG_DATA_HOME": str(data_home),
                    "CODEX_HOME": str(data_home / "codex"),
                },
            ):
                with self.assertRaises(Refusal):
                    preview_root()

    def test_selected_build_must_use_the_exact_standard_hosted_runner(self) -> None:
        run = {"id": 42, "run_attempt": 1}
        jobs = {
            "jobs": [
                {
                    "name": "Resolve preview metadata",
                    "run_attempt": 1,
                    "status": "completed",
                    "conclusion": "success",
                    "runner_group_name": "GitHub Actions",
                    "runner_name": "GitHub Actions 100",
                    "labels": ["ubuntu-24.04"],
                },
                {
                    "name": "build-linux-aarch64 / Branch build",
                    "run_attempt": 1,
                    "status": "completed",
                    "conclusion": "success",
                    "runner_group_name": "GitHub Actions",
                    "runner_name": "GitHub Actions 101",
                    "labels": ["ubuntu-24.04-arm"],
                },
            ]
        }
        with patch.object(install_branch_artifact, "gh_json", return_value=jobs):
            validate_hosted_jobs("sednalabs/codex", run, "linux-aarch64")

        jobs["jobs"][1]["runner_group_name"] = "Self-hosted"
        with patch.object(install_branch_artifact, "gh_json", return_value=jobs):
            with self.assertRaises(Refusal):
                validate_hosted_jobs("sednalabs/codex", run, "linux-aarch64")

    def test_extract_accepts_all_three_binaries_for_requested_architecture(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            destination = Path(temp_dir)
            with preview_tar(62) as payload:
                extract_binaries(payload, destination, expected_machine=62)
            self.assertEqual(
                {path.name for path in destination.iterdir()}, set(BINARIES)
            )
            self.assertTrue(
                all(path.stat().st_mode & 0o111 for path in destination.iterdir())
            )

    def test_extract_rejects_partial_or_wrong_architecture_archives(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            destination = Path(temp_dir)
            with preview_tar(62, BINARIES[:-1]) as payload:
                with self.assertRaises(Refusal):
                    extract_binaries(payload, destination, expected_machine=62)

    def test_extract_rejects_binary_and_total_size_limits_before_overflow(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            destination = Path(temp_dir)
            with patch("install_branch_artifact.MAX_BINARY_BYTES", 19):
                with preview_tar(62) as payload:
                    with self.assertRaises(Refusal):
                        extract_binaries(payload, destination, expected_machine=62)
            self.assertEqual(list(destination.iterdir()), [])

        with tempfile.TemporaryDirectory() as temp_dir:
            destination = Path(temp_dir)
            with patch("install_branch_artifact.MAX_TOTAL_BINARY_BYTES", 39):
                with preview_tar(62) as payload:
                    with self.assertRaises(Refusal):
                        extract_binaries(payload, destination, expected_machine=62)
            self.assertLessEqual(
                sum(path.stat().st_size for path in destination.iterdir()), 39
            )
        with tempfile.TemporaryDirectory() as temp_dir:
            destination = Path(temp_dir)
            with preview_tar(183) as payload:
                with self.assertRaises(Refusal):
                    extract_binaries(payload, destination, expected_machine=62)

    def test_manifest_binds_repository_source_ref_workflow_and_run_version(
        self,
    ) -> None:
        run = {
            "id": 42,
            "run_number": 42,
            "html_url": RUN_URL,
            "head_sha": HOST_SHA,
        }
        manifest = {
            "repository": "sednalabs/codex",
            "commit": SOURCE_SHA,
            "ref": "feature/demo",
            "previewVersion": "1.2.3-ci.42+g12345678",
            "workflow": RUN_URL,
        }
        self.assertEqual(
            validate_manifest(
                manifest,
                repository_name_value="sednalabs/codex",
                source_sha=SOURCE_SHA,
                source_ref="feature/demo",
                target="x86_64-unknown-linux-gnu",
                run=run,
            ),
            manifest,
        )
        mismatched = json.loads(json.dumps(manifest))
        mismatched["ref"] = "other-branch"
        with self.assertRaises(Refusal):
            validate_manifest(
                mismatched,
                repository_name_value="sednalabs/codex",
                source_sha=SOURCE_SHA,
                source_ref="feature/demo",
                target="x86_64-unknown-linux-gnu",
                run=run,
            )

    def test_outer_artifact_must_have_exact_manifest_and_complete_payload(self) -> None:
        version = "1.2.3-ci.42+g12345678"
        target = "x86_64-unknown-linux-gnu"
        archive_name, manifest_name = expected_archive_names(version, target)
        run = {
            "id": 42,
            "run_number": 42,
            "html_url": RUN_URL,
            "head_sha": HOST_SHA,
        }
        manifest = {
            "repository": "sednalabs/codex",
            "commit": SOURCE_SHA,
            "ref": "feature/demo",
            "previewVersion": version,
            "workflow": RUN_URL,
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            archive_path = root / "artifact.zip"
            with preview_tar(62) as tar_payload, zipfile.ZipFile(
                archive_path, "w", compression=zipfile.ZIP_DEFLATED
            ) as bundle:
                bundle.writestr(archive_name, tar_payload.getvalue())
                bundle.writestr(manifest_name, json.dumps(manifest))
            destination = root / "bin"
            destination.mkdir()
            unpack_preview_zip(
                archive_path,
                destination,
                preview_version=version,
                target=target,
                repository="sednalabs/codex",
                source_sha=SOURCE_SHA,
                source_ref="feature/demo",
                run=run,
            )
            self.assertEqual(
                {path.name for path in destination.iterdir()}, set(BINARIES)
            )

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            archive_path = root / "artifact.zip"
            with preview_tar(62) as tar_payload, zipfile.ZipFile(
                archive_path, "w", compression=zipfile.ZIP_DEFLATED
            ) as bundle:
                bundle.writestr(archive_name, tar_payload.getvalue())
                bundle.writestr(manifest_name, json.dumps(manifest))
                bundle.writestr("unexpected.txt", "partial or extra content")
            destination = root / "bin"
            destination.mkdir()
            with self.assertRaises(Refusal):
                unpack_preview_zip(
                    archive_path,
                    destination,
                    preview_version=version,
                    target=target,
                    repository="sednalabs/codex",
                    source_sha=SOURCE_SHA,
                    source_ref="feature/demo",
                    run=run,
                )

    def test_unpack_preflights_archive_limits_and_destination_space(self) -> None:
        version = "1.2.3-ci.42+g12345678"
        target = "x86_64-unknown-linux-gnu"
        archive_name, manifest_name = expected_archive_names(version, target)
        run = {
            "id": 42,
            "run_number": 42,
            "html_url": RUN_URL,
            "head_sha": HOST_SHA,
        }
        manifest = {
            "repository": "sednalabs/codex",
            "commit": SOURCE_SHA,
            "ref": "feature/demo",
            "previewVersion": version,
            "workflow": RUN_URL,
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            archive_path = root / "artifact.zip"
            with preview_tar(62) as tar_payload, zipfile.ZipFile(
                archive_path, "w", compression=zipfile.ZIP_DEFLATED
            ) as bundle:
                bundle.writestr(archive_name, tar_payload.getvalue())
                bundle.writestr(manifest_name, json.dumps(manifest))
            destination = root / "bin"
            destination.mkdir()

            with patch("install_branch_artifact.MAX_COMPRESSED_TAR_BYTES", 1):
                with self.assertRaises(Refusal):
                    unpack_preview_zip(
                        archive_path,
                        destination,
                        preview_version=version,
                        target=target,
                        repository="sednalabs/codex",
                        source_sha=SOURCE_SHA,
                        source_ref="feature/demo",
                        run=run,
                    )

            with patch(
                "install_branch_artifact.shutil.disk_usage",
                return_value=SimpleNamespace(free=0),
            ):
                with self.assertRaises(Refusal):
                    unpack_preview_zip(
                        archive_path,
                        destination,
                        preview_version=version,
                        target=target,
                        repository="sednalabs/codex",
                        source_sha=SOURCE_SHA,
                        source_ref="feature/demo",
                        run=run,
                    )
            self.assertEqual(list(destination.iterdir()), [])

            with patch("install_branch_artifact.MAX_MANIFEST_BYTES", 1):
                with self.assertRaises(Refusal):
                    unpack_preview_zip(
                        archive_path,
                        destination,
                        preview_version=version,
                        target=target,
                        repository="sednalabs/codex",
                        source_sha=SOURCE_SHA,
                        source_ref="feature/demo",
                        run=run,
                    )


if __name__ == "__main__":
    unittest.main()
