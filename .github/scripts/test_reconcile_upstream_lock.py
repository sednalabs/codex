#!/usr/bin/env python3
"""Hosted unit tests for the bounded lock reconciliation helper."""
from __future__ import annotations

import importlib.util
import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock
import subprocess


SCRIPT = Path(__file__).with_name("reconcile_upstream_lock.py")
SPEC = importlib.util.spec_from_file_location("reconcile_upstream_lock", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ReconcileLockTests(unittest.TestCase):
    def test_sha_requires_exact_40_hex(self) -> None:
        self.assertEqual(MODULE.sha("A" * 40, "candidate"), "a" * 40)
        with self.assertRaises(MODULE.ReconcileError):
            MODULE.sha("not-a-sha", "candidate")

    def test_package_entries_preserves_duplicate_versions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            lock = Path(directory) / "Cargo.lock"
            lock.write_text(
                'version = 4\n\n[[package]]\nname = "demo"\nversion = "1.2.3"\nsource = "registry+https://example.invalid"\n',
                encoding="utf-8",
            )
            self.assertEqual(
                MODULE.package_entries(lock),
                [("demo", "1.2.3", "registry+https://example.invalid")],
            )

    def test_duplicate_versions_are_distinct_entries(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            lock = Path(directory) / "Cargo.lock"
            lock.write_text(
                'version = 4\n\n[[package]]\nname = "demo"\nversion = "1.2.3"\nsource = "registry+https://example.invalid"\n\n[[package]]\nname = "demo"\nversion = "2.0.0"\nsource = "registry+https://example.invalid"\n',
                encoding="utf-8",
            )
            self.assertEqual(len(MODULE.package_entries(lock)), 2)

    def test_metadata_command_does_not_update_all(self) -> None:
        command = MODULE.cargo_metadata_command(Path("codex-rs/Cargo.toml"))
        self.assertEqual(command[:3], ["cargo", "metadata", "--format-version"])
        self.assertNotIn("generate-lockfile", command)
        self.assertNotIn("update", command)

    def test_mocked_git_failure_is_not_hidden(self) -> None:
        completed = mock.Mock(returncode=1, stdout="", stderr="fatal: missing Cargo.lock")
        with mock.patch.object(MODULE.subprocess, "run", return_value=completed):
            with self.assertRaises(MODULE.ReconcileError):
                MODULE.run(Path("/tmp/workspace"), "show", "deadbeef")

    def test_main_fixture_covers_exact_heads_seed_metadata_and_report(self) -> None:
        candidate = "a" * 40
        upstream = "b" * 40
        lock = 'version = 4\n\n[[package]]\nname = "demo"\nversion = "1.2.3"\nsource = "registry+https://example.invalid"\n'
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            (workspace / ".git").mkdir()
            (workspace / "codex-rs").mkdir()
            (workspace / "codex-rs/Cargo.toml").write_text("[workspace]\nmembers=[]\n", encoding="utf-8")
            report = workspace / "report.json"
            git_calls: list[tuple[str, ...]] = []

            def fake_git(repo: Path, *args: str, check: bool = True):
                git_calls.append(args)
                if args[:3] == ("rev-parse", "--verify", "HEAD"):
                    return subprocess.CompletedProcess(args, 0, candidate + "\n", "")
                if args[:3] == ("rev-parse", "--verify", "FETCH_HEAD"):
                    return subprocess.CompletedProcess(args, 0, upstream + "\n", "")
                if args[:1] == ("fetch",):
                    return subprocess.CompletedProcess(args, 0, "", "")
                if args[:1] == ("show",):
                    return subprocess.CompletedProcess(args, 0, lock, "")
                raise AssertionError(args)

            cargo = mock.Mock(returncode=0, stdout='{"packages":[]}', stderr="")
            argv = ["prog", "--candidate", candidate, "--upstream", upstream, "--workspace", str(workspace), "--report", str(report)]
            with mock.patch.object(MODULE, "run", side_effect=fake_git), mock.patch.object(MODULE.subprocess, "run", return_value=cargo) as cargo_run, mock.patch.object(MODULE.sys, "argv", argv):
                self.assertEqual(MODULE.main(), 0)
            target = workspace / "codex-rs/Cargo.lock"
            self.assertEqual(target.read_text(encoding="utf-8"), lock)
            result = json.loads(report.read_text(encoding="utf-8"))
            self.assertEqual(result["candidate"], candidate)
            self.assertEqual(result["upstream_source"]["commit"], upstream)
            self.assertEqual(result["candidate_lock_sha256"], hashlib.sha256(lock.encode()).hexdigest())
            self.assertEqual(cargo_run.call_args.kwargs["cwd"], workspace.resolve() / "codex-rs")
            self.assertEqual(cargo_run.call_args.args[0][:3], ["cargo", "metadata", "--format-version"])
            self.assertIn(("rev-parse", "--verify", "HEAD"), git_calls)
            self.assertIn(("rev-parse", "--verify", "FETCH_HEAD"), git_calls)

    def test_main_reports_upstream_mismatch(self) -> None:
        candidate = "a" * 40
        upstream = "b" * 40
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            (workspace / ".git").mkdir()
            (workspace / "codex-rs").mkdir()
            (workspace / "codex-rs/Cargo.toml").write_text("[workspace]\nmembers=[]\n", encoding="utf-8")
            report = workspace / "report.json"
            def fake_git(repo: Path, *args: str, check: bool = True):
                if args[:3] == ("rev-parse", "--verify", "HEAD"):
                    return subprocess.CompletedProcess(args, 0, candidate + "\n", "")
                if args[:3] == ("rev-parse", "--verify", "FETCH_HEAD"):
                    return subprocess.CompletedProcess(args, 0, "c" * 40 + "\n", "")
                return subprocess.CompletedProcess(args, 0, "", "")
            argv = ["prog", "--candidate", candidate, "--upstream", upstream, "--workspace", str(workspace), "--report", str(report)]
            with mock.patch.object(MODULE, "run", side_effect=fake_git), mock.patch.object(MODULE.sys, "argv", argv):
                self.assertEqual(MODULE.main(), 1)
            self.assertIn("upstream source mismatch", report.read_text(encoding="utf-8"))

    def test_main_reports_cargo_diagnostic(self) -> None:
        candidate = "a" * 40
        upstream = "b" * 40
        lock = 'version = 4\n'
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            (workspace / ".git").mkdir()
            (workspace / "codex-rs").mkdir()
            (workspace / "codex-rs/Cargo.toml").write_text("[workspace]\nmembers=[]\n", encoding="utf-8")
            report = workspace / "report.json"
            def fake_git(repo: Path, *args: str, check: bool = True):
                if args[:3] == ("rev-parse", "--verify", "HEAD"):
                    return subprocess.CompletedProcess(args, 0, candidate + "\n", "")
                if args[:3] == ("rev-parse", "--verify", "FETCH_HEAD"):
                    return subprocess.CompletedProcess(args, 0, upstream + "\n", "")
                if args[:1] == ("show",):
                    return subprocess.CompletedProcess(args, 0, lock, "")
                return subprocess.CompletedProcess(args, 0, "", "")
            argv = ["prog", "--candidate", candidate, "--upstream", upstream, "--workspace", str(workspace), "--report", str(report)]
            cargo = mock.Mock(returncode=1, stdout="", stderr="error: manifest path codex-rs/Cargo.toml is invalid")
            with mock.patch.object(MODULE, "run", side_effect=fake_git), mock.patch.object(MODULE.subprocess, "run", return_value=cargo), mock.patch.object(MODULE.sys, "argv", argv):
                self.assertEqual(MODULE.main(), 1)
            self.assertIn("manifest path codex-rs/Cargo.toml is invalid", report.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
