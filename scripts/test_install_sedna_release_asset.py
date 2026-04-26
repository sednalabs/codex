#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import os
import stat
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("install_sedna_release_asset.py")
SPEC = importlib.util.spec_from_file_location("install_sedna_release_asset", SCRIPT)
installer = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules["install_sedna_release_asset"] = installer
SPEC.loader.exec_module(installer)


class InstallSednaReleaseAssetTest(unittest.TestCase):
    def test_safe_member_path_rejects_unsafe_paths(self) -> None:
        self.assertEqual(installer.safe_member_path("./codex"), Path("codex"))
        for unsafe in ("../codex", "/codex", "nested/../../codex"):
            with self.subTest(unsafe=unsafe):
                with self.assertRaises(installer.InstallError):
                    installer.safe_member_path(unsafe)

    def test_parse_sha256sums_normalizes_names(self) -> None:
        parsed = installer.parse_sha256sums(
            b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  ./asset.tar.gz\n"
        )
        self.assertEqual(
            parsed,
            {"asset.tar.gz": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"},
        )

    def test_verify_assets_requires_metadata_and_archive_digest(self) -> None:
        tag = "v0.124.0-sedna.2"
        archive_name = installer.expected_archive_name(tag)
        archive = b"archive bytes"
        metadata = json.dumps(
            {
                "release_tag": tag,
                "release_version": "0.124.0-sedna.2",
                "repository": "sednalabs/codex",
                "target": installer.TARGET,
            }
        ).encode()
        checksums = f"{hashlib.sha256(archive).hexdigest()}  ./{archive_name}\n".encode()

        installer.verify_assets(
            "sednalabs/codex",
            tag,
            installer.ReleaseAssets(archive_name, archive, checksums, metadata),
        )

    def test_extract_archive_rejects_links(self) -> None:
        payload = io.BytesIO()
        with tarfile.open(fileobj=payload, mode="w:gz") as archive:
            info = tarfile.TarInfo("codex")
            info.type = tarfile.SYMTYPE
            info.linkname = "/bin/sh"
            archive.addfile(info)

        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaises(installer.InstallError):
                installer.extract_archive_safely(payload.getvalue(), Path(temp))

    def test_stage_and_promote_release_from_assets(self) -> None:
        tag = "v0.124.0-sedna.2"
        archive_name = installer.expected_archive_name(tag)
        archive = make_archive({"codex": b"#!/bin/sh\necho codex-cli 0.124.0-sedna.2\n", "codex-responses-api-proxy": b"#!/bin/sh\nexit 0\n"})
        metadata = json.dumps(
            {
                "release_tag": tag,
                "release_version": "0.124.0-sedna.2",
                "repository": "sednalabs/codex",
                "target": installer.TARGET,
            }
        ).encode()
        checksums = f"{hashlib.sha256(archive).hexdigest()}  {archive_name}\n".encode()

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            paths = installer.InstallPaths(root / "standalone", root / "bin")
            release_dir = installer.stage_or_reuse_release(
                paths,
                tag,
                installer.ReleaseAssets(archive_name, archive, checksums, metadata),
                dry_run=False,
            )
            installer.promote_release(paths, release_dir)

            self.assertEqual(paths.current_link.resolve(), release_dir)
            self.assertEqual(paths.visible_codex.resolve(), release_dir / "codex")
            self.assertIn("0.124.0-sedna.2", installer.verify_visible_codex(paths.visible_codex, tag))


def make_archive(files: dict[str, bytes]) -> bytes:
    payload = io.BytesIO()
    with tarfile.open(fileobj=payload, mode="w:gz") as archive:
        for name, contents in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR
            archive.addfile(info, io.BytesIO(contents))
    return payload.getvalue()


if __name__ == "__main__":
    unittest.main()
