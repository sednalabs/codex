"""Deterministic regression fixtures for the hosted-runner allowlist."""

from __future__ import annotations

from pathlib import Path
import tempfile
import unittest

from check_standard_runner_graph import RunnerGraphError
from check_standard_runner_graph import check_workflow_graph


class RunnerGraphTests(unittest.TestCase):
    def write_workflow(self, root: Path, relative_path: str, body: str) -> Path:
        path = root / relative_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")
        return path

    def test_literal_x64_and_arm64_reusable_graph_is_allowed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            parent = self.write_workflow(
                root,
                ".github/workflows/parent.yml",
                """name: parent
jobs:
  call-child:
    uses: ./.github/workflows/child.yml
""",
            )
            self.write_workflow(
                root,
                ".github/workflows/child.yml",
                """name: child
jobs:
  x64:
    runs-on: ubuntu-24.04
  arm64:
    runs-on: ubuntu-24.04-arm
""",
            )

            result = check_workflow_graph(root, parent)

            self.assertEqual(
                {(name, runner) for _, name, runner in result},
                {("x64", "ubuntu-24.04"), ("arm64", "ubuntu-24.04-arm")},
            )

    def test_nested_larger_runner_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            parent = self.write_workflow(
                root,
                ".github/workflows/parent.yml",
                """name: parent
jobs:
  call-child:
    uses: ./.github/workflows/child.yml
""",
            )
            self.write_workflow(
                root,
                ".github/workflows/child.yml",
                """name: child
jobs:
  larger:
    runs-on: ubuntu-24.04-16-cores
    steps:
      - run: echo child
""",
            )

            with self.assertRaisesRegex(RunnerGraphError, "ubuntu-24.04-16-cores"):
                check_workflow_graph(root, parent)

    def test_step_only_guard_does_not_make_unsafe_runner_safe(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workflow = self.write_workflow(
                root,
                ".github/workflows/step-guard.yml",
                """name: step-guard
jobs:
  larger:
    runs-on: ubuntu-24.04-16-cores
    steps:
      - if: false
        run: echo this step guard cannot change runner scheduling
""",
            )

            with self.assertRaisesRegex(RunnerGraphError, "ubuntu-24.04-16-cores"):
                check_workflow_graph(root, workflow)


if __name__ == "__main__":
    unittest.main()
