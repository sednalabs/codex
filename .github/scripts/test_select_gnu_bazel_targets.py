import json
import re
import subprocess
import sys
import unittest
from pathlib import Path

from select_gnu_bazel_targets import GNU_SUITE_QUERY
from select_gnu_bazel_targets import GNU_TARGET_QUERY
from select_gnu_bazel_targets import SHARD_COUNT
from select_gnu_bazel_targets import assign_targets


class SelectGnuBazelTargetsTest(unittest.TestCase):
    def test_query_uses_exact_manual_tag_element_and_only_existing_exclusions(
        self,
    ) -> None:
        self.assertEqual(
            GNU_TARGET_QUERY,
            '//... except attr("tags", "[\\\\[ ]manual[,\\\\]]", //...) '
            "except //third_party/v8:all "
            "except //codex-rs/v8-poc:v8-poc-unit-tests",
        )
        self.assertEqual(
            GNU_SUITE_QUERY,
            f'kind("test_suite rule", ({GNU_TARGET_QUERY}))',
        )
        pattern_match = re.search(
            r'attr\("tags",\s*("(?:\\.|[^"\\])*")', GNU_TARGET_QUERY
        )
        self.assertIsNotNone(pattern_match)
        serialized_tags_pattern = re.compile(json.loads(pattern_match.group(1)))
        self.assertTrue(serialized_tags_pattern.search("[manual]"))
        self.assertTrue(serialized_tags_pattern.search("[linux, manual, rust]"))
        for tags in (
            "[]",
            "[manual-integration]",
            "[not-manual]",
            "[integration-manual]",
        ):
            with self.subTest(tags=tags):
                self.assertIsNone(serialized_tags_pattern.search(tags))

    def test_assignments_are_complete_unique_and_order_independent(self) -> None:
        targets = [f"//pkg:target-{index}" for index in range(8)]
        assignments = assign_targets(targets, SHARD_COUNT)

        self.assertEqual([len(shard) for shard in assignments], [4, 4])
        self.assertEqual(
            sorted(target for shard in assignments for target in shard), sorted(targets)
        )
        self.assertEqual(
            assign_targets(list(reversed(targets)), SHARD_COUNT), assignments
        )
        self.assertTrue(all(shard == sorted(shard) for shard in assignments))

    def test_rejects_empty_duplicate_or_non_absolute_target_inputs(self) -> None:
        for targets in (
            [],
            [""],
            ["//pkg:target", "//pkg:target"],
            ["@repo//pkg:target"],
        ):
            with self.subTest(targets=targets), self.assertRaises(ValueError):
                assign_targets(targets, SHARD_COUNT)
        with self.assertRaisesRegex(ValueError, "empty shards are forbidden"):
            assign_targets(["//pkg:one"], SHARD_COUNT)

    def test_cli_prints_query_or_selected_targets_without_partial_output(self) -> None:
        script = Path(__file__).with_name("select_gnu_bazel_targets.py")
        query = subprocess.run(
            [sys.executable, str(script), "--query"],
            capture_output=True,
            check=True,
            text=True,
        )
        self.assertEqual(query.stdout.rstrip("\n"), GNU_TARGET_QUERY)
        suite_query = subprocess.run(
            [sys.executable, str(script), "--suite-query"],
            capture_output=True,
            check=True,
            text=True,
        )
        self.assertEqual(suite_query.stdout.rstrip("\n"), GNU_SUITE_QUERY)

        targets = ["//pkg:c", "//pkg:a", "//pkg:b", "//pkg:d"]
        expected = assign_targets(targets, SHARD_COUNT)
        for shard_number, selected in enumerate(expected, 1):
            result = subprocess.run(
                [
                    sys.executable,
                    str(script),
                    "--shard",
                    str(shard_number),
                    "--shard-count",
                    str(SHARD_COUNT),
                ],
                input="\n".join(targets) + "\n",
                capture_output=True,
                check=True,
                text=True,
            )
            self.assertEqual(result.stdout, "\n".join(selected) + "\n")

        invalid = subprocess.run(
            [sys.executable, str(script), "--shard", "1", "--shard-count", "3"],
            input="//pkg:target\n",
            capture_output=True,
            check=False,
            text=True,
        )
        self.assertNotEqual(invalid.returncode, 0)
        self.assertEqual(invalid.stdout, "")


if __name__ == "__main__":
    unittest.main()
