#!/usr/bin/env python3
"""Hosted-only real-Git fixtures for the upstream merge preview helper."""
from __future__ import annotations

import argparse
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SPEC = importlib.util.spec_from_file_location(
    "upstream_merge_preview", Path(__file__).with_name("upstream_merge_preview.py")
)
assert SPEC and SPEC.loader
preview = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = preview
SPEC.loader.exec_module(preview)


class GitFixture(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="upstream-merge-preview-fixture-")
        self.repo = Path(self.temporary.name) / "repo"
        subprocess.run(["git", "init", "--quiet", str(self.repo)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
        self.git("config", "user.email", "fixtures@example.invalid")
        self.git("config", "user.name", "Hosted Fixture")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def git(self, *arguments: object, check: bool = True) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run(
            ["git", "-C", str(self.repo), *(str(argument) for argument in arguments)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=check,
        )

    def head(self) -> str:
        return self.git("rev-parse", "HEAD").stdout.decode("ascii").strip()

    def commit(self, message: str, files: dict[str, str], remove: tuple[str, ...] = ()) -> str:
        for name, content in files.items():
            path = self.repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        for name in remove:
            self.git("rm", "--quiet", "--ignore-unmatch", "--", name)
        self.git("add", "-A")
        self.git("commit", "--quiet", "-m", message)
        return self.head()

    def branch_commit(self, branch: str, base: str, message: str, files: dict[str, str], remove: tuple[str, ...] = ()) -> str:
        self.git("checkout", "--quiet", "-B", branch, base)
        return self.commit(message, files, remove)

    def assert_preview_status(self, result: dict[str, object], expected: str) -> None:
        # The helper's reason values are fixed categories and intentionally do
        # not contain subprocess output, paths, content, or history.  Include
        # it as a concise common-seam failure context for hosted fixtures.
        self.assertEqual(result.get("status"), expected, {"status": result.get("status"), "reason": result.get("reason")})


class RealMergeTreeFixtures(GitFixture):
    def test_clean_merge_is_real_and_metadata_only(self) -> None:
        base = self.commit("base", {"common.txt": "base\n"})
        downstream = self.branch_commit("downstream", base, "downstream", {"downstream.txt": "d\n"})
        upstream = self.branch_commit("upstream", base, "upstream", {"upstream.txt": "u\n"})
        result = preview.preview_repository(self.repo, downstream, upstream, base)
        self.assert_preview_status(result, "clean")
        self.assertEqual(result["requested"]["downstream"], result["actual"]["downstream"])
        self.assertEqual(result["base_contract"]["mode"], "direct-unique-common-base")
        self.assertEqual(result["merge"]["staged_paths"], [])
        self.assertNotIn("common.txt", json.dumps(result))

    def test_text_conflict_uses_real_nul_records(self) -> None:
        base = self.commit("base", {"shared.txt": "base\n"})
        downstream = self.branch_commit("downstream", base, "downstream", {"shared.txt": "downstream\n"})
        upstream = self.branch_commit("upstream", base, "upstream", {"shared.txt": "upstream\n"})
        result = preview.preview_repository(self.repo, downstream, upstream, base)
        self.assert_preview_status(result, "conflicts")
        self.assertGreater(result["merge"]["conflict_record_count"], 0)
        self.assertGreater(result["merge"]["staged_path_count"], 0)
        self.assertTrue(all(path["encoding"] == "utf-8" for path in result["merge"]["staged_paths"]))
        self.assertTrue(all("paths" in record for record in result["merge"]["informational_records"]))

    def test_clean_overlapping_content_merge_preserves_auto_merging_metadata(self) -> None:
        base = self.commit("base", {"shared.txt": "one\ntwo\nthree\nfour\nfive\nsix\nseven\n"})
        downstream = self.branch_commit("downstream", base, "downstream hunk", {"shared.txt": "one\ndownstream-two\nthree\nfour\nfive\nsix\nseven\n"})
        upstream = self.branch_commit("upstream", base, "upstream hunk", {"shared.txt": "one\ntwo\nthree\nfour\nfive\nupstream-six\nseven\n"})
        result = preview.preview_repository(self.repo, downstream, upstream, base)
        self.assert_preview_status(result, "clean")
        self.assertEqual(result["merge"]["conflict_record_count"], 0)
        self.assertIn("Auto-merging", [record["type"] for record in result["merge"]["informational_records"]])

    def test_rename_delete_conflict_is_not_reduced_to_a_message_parser(self) -> None:
        base = self.commit("base", {"old.txt": "base\n"})
        self.git("checkout", "--quiet", "-B", "downstream", base)
        self.git("mv", "old.txt", "new.txt")
        downstream = self.commit("rename", {})
        upstream = self.branch_commit("upstream", base, "delete", {}, ("old.txt",))
        result = preview.preview_repository(self.repo, downstream, upstream, base)
        self.assert_preview_status(result, "conflicts")
        self.assertTrue(any(entry["type"].startswith("CONFLICT (") for entry in result["merge"]["conflict_types"]))

    def test_non_content_directory_file_conflict_is_not_clean_when_paths_are_sparse(self) -> None:
        base = self.commit("base", {"base.txt": "base\n"})
        downstream = self.branch_commit("downstream", base, "file", {"node": "file\n"})
        upstream = self.branch_commit("upstream", base, "directory", {"node/child.txt": "child\n"})
        result = preview.preview_repository(self.repo, downstream, upstream, base)
        self.assert_preview_status(result, "conflicts")
        self.assertGreater(result["merge"]["informational_record_count"], 0)
        self.assertNotEqual(result["merge"]["conflict_types"], [])

    def test_real_directory_rename_collision_uses_no_space_stable_type(self) -> None:
        base = self.commit(
            "base",
            {
                "dir1/a": "a\n",
                "dir1/b": "b\n",
                "dir2/d": "d\n",
                "dir2/e": "e\n",
            },
        )
        downstream = self.branch_commit(
            "downstream",
            base,
            "add colliding files",
            {"dir1/c": "c\n", "dir1/yo": "one\n", "dir2/f": "f\n", "dir2/yo": "two\n"},
        )
        self.git("checkout", "--quiet", "-B", "upstream", base)
        self.git("mv", "dir1", "combined")
        self.git("mv", "dir2/d", "combined/d")
        self.git("mv", "dir2/e", "combined/e")
        (self.repo / "dir2").rmdir()
        upstream = self.commit("combine directories", {})
        self.git("config", "merge.directoryRenames", "true")
        result = preview.preview_repository(self.repo, downstream, upstream, base)
        self.assert_preview_status(result, "conflicts")
        self.assertIn(
            "CONFLICT(directory rename collision)",
            [record["type"] for record in result["merge"]["informational_records"]],
        )
        self.assertIn(
            "CONFLICT(directory rename collision)",
            [entry["type"] for entry in result["merge"]["conflict_types"]],
        )

    def test_rewritten_base_mapping_requires_equal_tree_and_separate_ancestry(self) -> None:
        base = self.commit("base", {"base.txt": "base\n"})
        base_tree = self.git("rev-parse", f"{base}^{{tree}}").stdout.decode("ascii").strip()
        rewritten = self.git("commit-tree", base_tree, "-m", "rewritten fixture base").stdout.decode("ascii").strip()
        self.git("branch", "-f", "rewritten", rewritten)
        downstream = self.branch_commit("downstream", rewritten, "downstream", {"downstream.txt": "d\n"})
        upstream = self.branch_commit("upstream", base, "upstream", {"upstream.txt": "u\n"})
        result = preview.preview_repository(self.repo, downstream, upstream, base, rewritten)
        self.assert_preview_status(result, "clean")
        self.assertEqual(result["base_contract"]["mode"], "mapped-rewrite-explicit-tree")
        self.assertFalse(result["base_contract"]["logical_base_is_ancestor_of_downstream"])
        self.assertTrue(result["base_contract"]["logical_base_is_ancestor_of_upstream"])
        self.assertTrue(result["base_contract"]["rewritten_base_is_ancestor_of_downstream"])
        self.assertTrue(result["base_contract"]["rewrite_tree_equal"])
        self.assertFalse(result["base_contract"]["natural_common_base_used"])

    def test_wrong_direct_base_and_absent_object_are_incomplete(self) -> None:
        root = self.commit("root", {"root.txt": "root\n"})
        base = self.branch_commit("base", root, "base", {"base.txt": "base\n"})
        downstream = self.branch_commit("downstream", base, "downstream", {"d.txt": "d\n"})
        upstream = self.branch_commit("upstream", base, "upstream", {"u.txt": "u\n"})
        wrong = preview.preview_repository(self.repo, downstream, upstream, root)
        absent = preview.preview_repository(self.repo, downstream, upstream, "f" * 40)
        self.assert_preview_status(wrong, "diagnostic-incomplete")
        self.assertEqual(wrong["reason"], "logical base is not the unique direct merge base")
        self.assert_preview_status(absent, "diagnostic-incomplete")
        self.assertEqual(absent["reason"], "required object unavailable or not a commit")

    def test_multiple_real_merge_bases_are_ambiguous(self) -> None:
        root = self.commit("root", {"root.txt": "root\n"})
        first = self.branch_commit("first", root, "first", {"first.txt": "first\n"})
        second = self.branch_commit("second", root, "second", {"second.txt": "second\n"})
        merge_tree = self.git("merge-tree", "--write-tree", first, second).stdout.decode("ascii").splitlines()[0]
        left_merge = self.git("commit-tree", merge_tree, "-p", first, "-p", second, "-m", "left merge").stdout.decode("ascii").strip()
        right_merge = self.git("commit-tree", merge_tree, "-p", second, "-p", first, "-m", "right merge").stdout.decode("ascii").strip()
        downstream = self.branch_commit("downstream", left_merge, "downstream", {"d.txt": "d\n"})
        upstream = self.branch_commit("upstream", right_merge, "upstream", {"u.txt": "u\n"})
        result = preview.preview_repository(self.repo, downstream, upstream, first)
        self.assert_preview_status(result, "diagnostic-incomplete")
        self.assertEqual(result["reason"], "direct merge base is ambiguous")


class ParserAndCapabilityTests(unittest.TestCase):
    TREE = b"0123456789abcdef0123456789abcdef01234567"

    def test_documented_nul_records_allow_many_to_many_and_ignore_messages(self) -> None:
        raw = (
            self.TREE + b"\0stage-a\0stage-b\0\0"
            b"2\0alpha\0beta\0CONFLICT (rename/delete)\0raw message must not be emitted\0"
            b"1\0alpha\0Auto-merging\0another raw message\0"
            b"2\0beta\0gamma\0CONFLICT (directory/file)\0raw message two\0"
            b"0\0CONFLICT(directory rename unclear split)\0raw message three\0"
        )
        parsed = preview.interpret_merge_tree(1, raw)
        self.assertEqual(parsed.status, "conflicts")
        self.assertEqual(parsed.staged_paths, (b"stage-a", b"stage-b"))
        self.assertEqual([record.stable_type for record in parsed.records], ["CONFLICT (rename/delete)", "Auto-merging", "CONFLICT (directory/file)", "CONFLICT(directory rename unclear split)"])
        self.assertEqual(parsed.records[0].paths, (b"alpha", b"beta"))
        self.assertTrue(preview.is_conflict_stable_type("CONFLICT(directory rename collision)"))
        self.assertTrue(preview.is_conflict_stable_type("CONFLICT(directory rename unclear split)"))
        self.assertFalse(preview.is_conflict_stable_type("Auto-merging"))

    def test_empty_staged_section_is_never_clean(self) -> None:
        raw = self.TREE + b"\0\0" + b"0\0CONFLICT (directory rename)\0ignored\0"
        parsed = preview.interpret_merge_tree(1, raw)
        self.assertEqual(parsed.status, "conflicts")
        self.assertEqual(parsed.staged_paths, ())

    def test_path_serialisation_is_reversible_and_rejects_host_like_paths(self) -> None:
        self.assertEqual(
            preview.serialise_repo_path(b"src/file.txt"),
            {"encoding": "utf-8", "value": "src/file.txt"},
        )
        self.assertEqual(
            preview.serialise_repo_path(b"src/\xff-name"),
            {"encoding": "hex", "value": "7372632fff2d6e616d65"},
        )
        for unsafe in (b"/private/host-path", b"../traversal", b"src/../traversal", b"C:\\host-path"):
            with self.assertRaises(preview.PreviewError):
                preview.serialise_repo_path(unsafe)

    def test_malformed_output_and_error_return_codes_fail_closed(self) -> None:
        malformed = (b"", self.TREE + b"\0stage\0", self.TREE + b"\0\0x\0missing fields")
        for raw in malformed:
            with self.assertRaises(preview.PreviewError):
                preview.interpret_merge_tree(1, raw)
        with self.assertRaises(preview.PreviewError):
            preview.interpret_merge_tree(2, self.TREE + b"\0")

    @staticmethod
    def valid_merge_tree_help(merge_base: bytes = b"--merge-base=<tree-ish>") -> bytes:
        return b"\n".join(
            (
                b"usage: git merge-tree [--write-tree] [<options>] <branch1> <branch2>",
                b"    --write-tree                  real merge",
                b"    --[no-]messages               messages",
                b"    -z                            NUL output",
                b"    --name-only                   names only",
                b"    " + merge_base + b"             merge base",
            )
        )

    def test_boolean_help_spelling_and_metavar_variants_are_accepted(self) -> None:
        preview.validate_merge_tree_help(self.valid_merge_tree_help())
        preview.validate_merge_tree_help(self.valid_merge_tree_help(b"--merge-base <tree-ish>"))

    def test_missing_or_malformed_help_option_rows_fail_closed(self) -> None:
        missing = self.valid_merge_tree_help().replace(b"    --name-only                   names only\n", b"")
        malformed = self.valid_merge_tree_help().replace(b"--[no-]messages", b"--[no-messages")
        prose_and_lookalike = self.valid_merge_tree_help().replace(
            b"    --[no-]messages               messages",
            b"    prose mentions --messages\n    --messages-fake              not the option",
        )
        for help_output in (missing, malformed, prose_and_lookalike):
            with self.assertRaises(preview.PreviewError):
                preview.validate_merge_tree_help(help_output)

    def test_unsupported_capability_is_not_accepted(self) -> None:
        with self.assertRaises(preview.PreviewError):
            preview.validate_merge_tree_help(b"--write-tree --merge-base -z")

    def test_exact_sha_inputs_are_mandatory(self) -> None:
        self.assertEqual(preview.exact_sha("A" * 40, "input"), "a" * 40)
        with self.assertRaises(preview.PreviewError):
            preview.exact_sha("main", "input")

    def test_incomplete_diagnostic_has_a_nonzero_process_status(self) -> None:
        self.assertEqual(preview.exit_status({"status": "clean"}), 0)
        self.assertEqual(preview.exit_status({"status": "conflicts"}), 1)
        self.assertEqual(preview.exit_status({"status": "diagnostic-incomplete"}), 2)


def run_suite(report_path: Path | None) -> int:
    suite = unittest.defaultTestLoader.loadTestsFromModule(sys.modules[__name__])
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if report_path is not None:
        report = {
            "format": 1,
            "suite": "upstream-merge-preview-fixtures",
            "status": "passed" if result.wasSuccessful() else "failed",
            "tests_run": result.testsRun,
            "failures": len(result.failures),
            "errors": len(result.errors),
            "git_version": "unavailable",
        }
        try:
            report["git_version"] = preview.git_version(Path.cwd())
        except preview.PreviewError:
            pass
        report_path.write_text(json.dumps(report, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8")
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--report", type=Path)
    arguments = parser.parse_args()
    raise SystemExit(run_suite(arguments.report))
