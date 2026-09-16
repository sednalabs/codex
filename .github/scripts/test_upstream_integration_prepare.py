#!/usr/bin/env python3
"""Hosted-only tests for the fixed provisional upstream integration preparer."""
import contextlib
import base64
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


class RemoteLeaseTests(GitFixture):
    def setUp(self) -> None:
        super().setUp()
        self.remote = Path(self.temporary.name) / "remote.git"
        subprocess.run(["git", "init", "--bare", "--quiet", str(self.remote)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
        self.git("remote", "add", "downstream", str(self.remote))
        self.downstream = self.commit("downstream\n")
        self.tree = self.git("rev-parse", f"{self.downstream}^{{tree}}").stdout.decode("ascii").strip()

    def remote_head(self) -> str:
        return subprocess.run(
            ["git", "--git-dir", str(self.remote), "rev-parse", prepare.INTEGRATION_REF],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=True,
        ).stdout.decode("ascii").strip()

    def candidate(self) -> str:
        return prepare.create_provisional_commit(self.repo, self.tree, self.downstream)

    def test_absent_ref_is_created_with_exact_candidate_head(self) -> None:
        candidate = self.candidate()
        with mock.patch.dict(os.environ, {"GITHUB_TOKEN": "test-token"}, clear=False):
            prepare.refuse_existing_ref(self.repo)
            prepare.push_new_ref(self.repo, candidate)
        self.assertEqual(self.remote_head(), candidate)

    def test_preexisting_ref_is_refused_without_changing_remote_head(self) -> None:
        existing = self.candidate()
        self.git("push", "downstream", f"{existing}:{prepare.INTEGRATION_REF}")
        with self.assertRaises(prepare.PrepareError):
            prepare.refuse_existing_ref(self.repo)
        self.assertEqual(self.remote_head(), existing)

    def test_race_after_absence_check_refuses_lease_and_preserves_remote_head(self) -> None:
        candidate = self.candidate()
        prepare.refuse_existing_ref(self.repo)
        competitor = self.git("commit-tree", self.tree, "-p", self.downstream, "-m", "competing fixture").stdout.decode("ascii").strip()
        self.git("push", "downstream", f"{competitor}:{prepare.INTEGRATION_REF}")
        with mock.patch.dict(os.environ, {"GITHUB_TOKEN": "test-token"}, clear=False):
            with self.assertRaises(prepare.PrepareError):
                prepare.push_new_ref(self.repo, candidate)
        self.assertEqual(self.remote_head(), competitor)


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

    def test_push_uses_transient_actions_checkout_basic_header(self) -> None:
        with mock.patch.dict(os.environ, {"GITHUB_TOKEN": "test-token"}, clear=False):
            environment = prepare._push_environment()
        expected = base64.b64encode(b"x-access-token:test-token").decode("ascii")
        self.assertEqual(environment["GIT_CONFIG_VALUE_0"], f"AUTHORIZATION: basic {expected}")


if __name__ == "__main__":
    raise SystemExit(unittest.main(verbosity=2))
