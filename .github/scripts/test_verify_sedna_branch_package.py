#!/usr/bin/env python3
"""Focused archive-boundary tests for the hosted package consumer."""

from __future__ import annotations

import importlib.util
import io
import tarfile
import tempfile
from pathlib import Path
from unittest import TestCase, main


SCRIPT = Path(__file__).with_name("verify_sedna_branch_package.py")
SPEC = importlib.util.spec_from_file_location("verify_sedna_branch_package", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class PackageArchiveTests(TestCase):
    def test_exact_package_binary_inventory_extracts(self) -> None:
        payloads = {
            "codex": b"codex-binary",
            "codex-code-mode-host": b"host-binary",
            "codex-responses-api-proxy": b"proxy-binary",
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "package.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                for name, payload in payloads.items():
                    member = tarfile.TarInfo(f"./{name}")
                    member.size = len(payload)
                    bundle.addfile(member, io.BytesIO(payload))
            destination = root / "extract"
            destination.mkdir()

            MODULE.extract_binaries(archive, destination)

            self.assertEqual(
                {path.name for path in destination.iterdir()}, set(payloads)
            )
            for name, payload in payloads.items():
                self.assertEqual((destination / name).read_bytes(), payload)

    def test_archive_parent_traversal_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "package.tar.gz"
            member = tarfile.TarInfo("../escaped")
            member.size = 4
            with tarfile.open(archive, "w:gz") as bundle:
                bundle.addfile(member, io.BytesIO(b"test"))
            destination = root / "extract"
            destination.mkdir()

            with self.assertRaises(MODULE.ConsumerFailure) as failure:
                MODULE.extract_binaries(archive, destination)

            self.assertEqual(failure.exception.code, "archive_member_rejected")
            self.assertEqual(list(root.glob("escaped")), [])


if __name__ == "__main__":
    main()
