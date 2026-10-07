#!/usr/bin/env python3
"""Focused archive-boundary tests for the hosted package consumer."""

from __future__ import annotations

import importlib.util
import io
import sys
import tarfile
import tempfile
import time
from pathlib import Path
from unittest import TestCase, main, mock


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

    def test_archive_member_count_is_bounded(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "package.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                for name in ("codex", "codex-code-mode-host", "codex-responses-api-proxy"):
                    member = tarfile.TarInfo(name)
                    member.size = 0
                    bundle.addfile(member, io.BytesIO())
            destination = root / "extract"
            destination.mkdir()

            with mock.patch.object(MODULE, "MAX_ARCHIVE_MEMBERS", 2):
                with self.assertRaises(MODULE.ConsumerFailure) as failure:
                    MODULE.extract_binaries(archive, destination)

            self.assertEqual(failure.exception.code, "archive_member_limit")

    def test_archive_expanded_bytes_are_bounded_before_copy(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "package.tar.gz"
            member = tarfile.TarInfo("codex")
            member.size = 5
            with tarfile.open(archive, "w:gz") as bundle:
                bundle.addfile(member, io.BytesIO(b"12345"))
            destination = root / "extract"
            destination.mkdir()

            with mock.patch.object(MODULE, "MAX_BINARY_BYTES", 4):
                with self.assertRaises(MODULE.ConsumerFailure) as failure:
                    MODULE.extract_binaries(archive, destination)

            self.assertEqual(failure.exception.code, "archive_expansion_limit")
            self.assertEqual(list(destination.iterdir()), [])

    def test_pax_and_gnu_extension_headers_are_rejected_before_extraction(self) -> None:
        for archive_format, name in (
            (tarfile.PAX_FORMAT, "p" * 120),
            (tarfile.GNU_FORMAT, "g" * 120),
        ):
            with self.subTest(archive_format=archive_format):
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    archive = root / "package.tar.gz"
                    member = tarfile.TarInfo(name)
                    member.size = 1
                    with tarfile.open(
                        archive, "w:gz", format=archive_format
                    ) as bundle:
                        bundle.addfile(member, io.BytesIO(b"x"))
                    destination = root / "extract"
                    destination.mkdir()

                    with self.assertRaises(MODULE.ConsumerFailure) as failure:
                        MODULE.extract_binaries(archive, destination)

                    self.assertEqual(
                        failure.exception.code, "archive_header_extension_rejected"
                    )
                    self.assertEqual(list(destination.iterdir()), [])

    def test_packaged_command_output_is_bounded(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(MODULE.ConsumerFailure) as failure:
                MODULE.command_ok(
                    [
                        sys.executable,
                        "-c",
                        "import sys; sys.stdout.write('x' * 1200000)",
                    ],
                    cwd=Path(directory),
                )

        self.assertEqual(failure.exception.code, "packaged_command_output_limit")

    def test_packaged_command_cleans_descendants_after_leader_exit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "surviving-child"
            child_code = (
                "import time; from pathlib import Path; time.sleep(0.4); "
                f"Path({str(marker)!r}).write_text('alive')"
            )
            parent_code = (
                "import subprocess, sys; "
                f"subprocess.Popen([sys.executable, '-c', {child_code!r}], "
                "stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, "
                "stderr=subprocess.DEVNULL); print('parent-exited')"
            )
            completed = MODULE.command_ok(
                [sys.executable, "-c", parent_code], cwd=Path(directory)
            )
            self.assertEqual(completed.stdout.strip(), "parent-exited")
            time.sleep(0.5)
            self.assertFalse(marker.exists())


if __name__ == "__main__":
    main()
