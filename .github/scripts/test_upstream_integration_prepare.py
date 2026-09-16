#!/usr/bin/env python3
"""Hosted-only tests for the fixed provisional upstream integration preparer."""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT_DIR = Path(__file__).parent
PREVIEW_SPEC = importlib.util.spec_from_file_location("upstream_merge_preview", SCRIPT_DIR / "upstream_merge_preview.py")
assert PREVIEW_SPEC and PREVIEW_SPEC.loader
preview = importlib.util.module_from_spec(PREVIEW_SPEC)
sys.modules[PREVIEW_SPEC.name] = preview
PREVIEW_SPEC.loader.exec_module(preview)

PREPARE_SPEC = importlib.util.spec_from_file_location("upstream_integration_prepare", SCRIPT_DIR / "upstream_integration_prepare.py")
assert PREPARE_SPEC and PREPARE_SPEC.loader
prepare = importlib.util.module_from_spec(PREPARE_SPEC)
sys.modules[PREPARE_SPEC.name] = prepare
PREPARE_SPEC.loader.exec_module(prepare)


class GitFixture(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="upstream-integration-prepare-fixture-")
        self.repo = Path(self.temporary.name) / "repo"
        subprocess.run(["git", "init", "--quiet", str(self.repo)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
        self.git("config", "user.email", "fixtures@example.invalid")
        self.git("config", "user.name", "Hosted Fixture")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def git(self, *arguments: str) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run(["git", "-C", str(self.repo), *arguments], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)

    def commit(self, content: str) -> str:
        (self.repo / "source.txt").write_text(content, encoding="utf-8")
        self.git("add", "source.txt")
        self.git("commit", "--quiet", "-m", "fixture")
        return self.git("rev-parse", "HEAD").stdout.decode("ascii").strip()


class ProvisionalCommitTests(GitFixture):
    def test_candidate_has_exactly_downstream_parent_and_requested_result_tree(self) -> None:
        downstream = self.commit("downstream\n")
        tree = self.git("rev-parse", f"{downstream}^{{tree}}").stdout.decode("ascii").strip()
        candidate = prepare.create_provisional_commit(self.repo, tree, downstream)
        raw = self.git("cat-file", "commit", candidate).stdout.decode("utf-8")
        headers, message = raw.split("\n\n", 1)
        self.assertEqual([line for line in headers.splitlines() if line.startswith("tree ")], [f"tree {tree}"])
        self.assertEqual([line for line in headers.splitlines() if line.startswith("parent ")], [f"parent {downstream}"])
        self.assertEqual(message, prepare.PREPARATION_MESSAGE + "\n")


class PreparationControlTests(unittest.TestCase):
    def test_default_is_no_write_and_cli_prepare_is_explicit(self) -> None:
        with mock.patch.object(prepare.tempfile, "TemporaryDirectory") as temporary_directory:
            result = prepare.prepare_integration(publish=False)
        self.assertEqual(result["status"], "not-requested")
        self.assertFalse(result["preparation_requested"])
        temporary_directory.assert_not_called()
        prepared = {"status": "provisional-prepared"}
        with mock.patch.object(prepare, "prepare_integration", return_value=prepared) as mocked:
            with mock.patch.object(sys, "argv", ["upstream_integration_prepare.py", "--prepare"]):
                with contextlib.redirect_stdout(io.StringIO()) as captured:
                    self.assertEqual(prepare.main(), 0)
        self.assertTrue(mocked.call_args.kwargs["publish"])
        self.assertEqual(json.loads(captured.getvalue()), prepared)

    def test_malformed_input_and_failed_mapped_base_are_rejected(self) -> None:
        with self.assertRaises(prepare.PrepareError):
            prepare.validate_inputs(prepare.FrozenInputs(downstream="not-a-sha"))
        inputs = prepare.FrozenInputs()
        with mock.patch.object(preview, "preview_repository", return_value={"status": "diagnostic-incomplete"}):
            with self.assertRaises(prepare.PrepareError):
                prepare.recompute_mapped_preview(Path("/unused"), inputs)

    def test_existing_ref_and_creation_race_are_refused(self) -> None:
        completed = subprocess.CompletedProcess
        with mock.patch.object(preview, "run_git", return_value=completed([], 0, b"present\n", b"")):
            with self.assertRaises(prepare.PrepareError):
                prepare.refuse_existing_ref(Path("/unused"))
        with mock.patch.object(preview, "run_git", return_value=completed([], 2, b"", b"")):
            prepare.refuse_existing_ref(Path("/unused"))
        with mock.patch.dict(os.environ, {"GITHUB_TOKEN": "test-token"}, clear=False):
            with mock.patch.object(prepare, "_run", return_value=completed([], 1, b"", b"")) as mocked:
                with self.assertRaises(prepare.PrepareError):
                    prepare.push_new_ref(Path("/unused"), "a" * 40)
        arguments = mocked.call_args.args[1:]
        self.assertIn(f"--force-with-lease={prepare.INTEGRATION_REF}:", arguments)
        self.assertIn(f"{'a' * 40}:{prepare.INTEGRATION_REF}", arguments)


if __name__ == "__main__":
    raise SystemExit(unittest.main(verbosity=2))
