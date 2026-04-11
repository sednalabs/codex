#!/usr/bin/env python3
"""Fixture tests for CI planner scripts and follow-up route selection."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPTS_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPTS_DIR.parent.parent


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"unable to load module from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


RESOLVE_VALIDATION_PLAN = load_module(
    "resolve_validation_plan_module", SCRIPTS_DIR / "resolve_validation_plan.py"
)
RESOLVE_RUST_CI_MODE = load_module(
    "resolve_rust_ci_mode_module", SCRIPTS_DIR / "resolve_rust_ci_mode.py"
)


def run_script(script: Path, *args: str) -> dict:
    proc = subprocess.run(
        ["python3", str(script), *args],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(proc.stdout)


def parse_workflow_dispatch_lane_options(workflow_path: Path) -> list[str]:
    lines = workflow_path.read_text(encoding="utf-8").splitlines()
    in_lane_block = False
    in_options_block = False
    options: list[str] = []

    for line in lines:
        stripped = line.strip()
        indent = len(line) - len(line.lstrip(" "))

        if not in_lane_block:
            if stripped == "lane:" and indent >= 6:
                in_lane_block = True
            continue

        if in_lane_block and not in_options_block:
            if indent <= 6 and stripped and stripped != "lane:":
                break
            if stripped == "options:" and indent >= 8:
                in_options_block = True
            continue

        if in_options_block:
            if indent <= 8 and stripped and not stripped.startswith("- "):
                break
            if stripped.startswith("- "):
                options.append(stripped[2:].strip())

    return options


class TempGitRepo:
    def __init__(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.root = Path(self._tmpdir.name)
        self._git("init", "--initial-branch=main")
        self._git("config", "user.name", "CI Planner Tests")
        self._git("config", "user.email", "ci-planner-tests@example.invalid")

    def cleanup(self) -> None:
        self._tmpdir.cleanup()

    def write_files(self, files: dict[str, str]) -> None:
        for relative_path, content in files.items():
            path = self.root / relative_path
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")

    def commit(self, message: str, files: dict[str, str]) -> str:
        self.write_files(files)
        self._git("add", "--all")
        self._git("commit", "-m", message)
        return self.rev_parse("HEAD")

    def rev_parse(self, ref: str) -> str:
        return self._git("rev-parse", ref)

    def _git(self, *args: str) -> str:
        proc = subprocess.run(
            ["git", "-C", str(self.root), *args],
            check=True,
            capture_output=True,
            text=True,
        )
        return proc.stdout.strip()


class RouteSelectionTests(unittest.TestCase):
    maxDiff = None

    @classmethod
    def setUpClass(cls) -> None:
        cls.catalog = RESOLVE_VALIDATION_PLAN.load_catalog()
        cls.routes = cls.catalog["followup_routes"]

    def test_picker_shared_surface_routes_to_both_picker_lanes(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/tui/src/app.rs"],
            self.routes,
        )
        self.assertEqual(
            lanes,
            [
                "codex.tui-agent-picker-targeted",
                "codex.tui-agent-picker-tree-targeted",
            ],
        )

    def test_picker_tree_unique_files_keep_tree_route_exact(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            [
                "codex-rs/tui/src/app.rs",
                "codex-rs/tui/src/app/agent_navigation.rs",
            ],
            self.routes,
        )
        self.assertEqual(lanes, ["codex.tui-agent-picker-tree-targeted"])

    def test_spawn_tool_surface_routes_to_both_related_lanes(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/tools/src/agent_tool.rs"],
            self.routes,
        )
        self.assertEqual(
            lanes,
            [
                "codex.tui-agent-picker-model-surface-targeted",
                "codex.core-subagent-spawn-approval-targeted",
            ],
        )

    def test_openai_models_route_stays_out_of_app_server_lane(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/protocol/src/openai_models.rs"],
            self.routes,
        )
        self.assertEqual(
            lanes,
            ["codex.tui-agent-picker-model-surface-targeted"],
        )

    def test_heavy_workflow_dispatch_options_cover_catalog_lanes(self) -> None:
        workflow_options = parse_workflow_dispatch_lane_options(
            REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml"
        )
        expected_lane_ids = [
            lane["lane_id"]
            for lane in self.catalog["lanes"]
            if lane.get("lane_id")
        ]
        self.assertEqual(
            workflow_options,
            ["all", *expected_lane_ids],
        )


class ValidationPlanScriptTests(unittest.TestCase):
    maxDiff = None

    def test_heavy_plan_splits_selected_lanes_by_setup_class(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "heavy",
            "--event-name",
            "pull_request",
            "--requested-lane",
            "",
            "--run-all-lanes",
            "false",
            "--run-core-family",
            "true",
            "--run-attestation-family",
            "false",
            "--run-workflow-family",
            "false",
            "--run-ui-protocol-family",
            "true",
            "--run-docs-family",
            "true",
            "--changed-files-json",
            json.dumps(
                [
                    "codex-rs/core/src/tools/handlers/multi_agents_common.rs",
                    "codex-rs/tui/src/app.rs",
                    "docs/downstream.md",
                ]
            ),
        )

        self.assertGreater(payload["selected_light_lane_count"], 0)
        self.assertGreater(payload["selected_rust_lane_count"], 0)
        self.assertGreater(payload["selected_heavy_lane_count"], 0)
        self.assertEqual(payload["light_max_parallel"], "4")
        self.assertEqual(payload["rust_max_parallel"], "2")
        self.assertEqual(payload["heavy_max_parallel"], "1")

    def test_heavy_plan_route_uses_bounded_shared_spawn_surface(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "heavy",
            "--event-name",
            "pull_request",
            "--requested-lane",
            "",
            "--run-all-lanes",
            "false",
            "--run-core-family",
            "false",
            "--run-attestation-family",
            "false",
            "--run-workflow-family",
            "false",
            "--run-ui-protocol-family",
            "false",
            "--run-docs-family",
            "false",
            "--changed-files-json",
            json.dumps(
                [
                    "codex-rs/tools/src/agent_tool.rs",
                    "codex-rs/tools/src/agent_tool_tests.rs",
                ]
            ),
        )

        self.assertEqual(
            [lane["lane_id"] for lane in payload["selected_matrix"]["include"]],
            [
                "codex.tui-agent-picker-model-surface-targeted",
                "codex.core-subagent-spawn-approval-targeted",
            ],
        )

    def test_heavy_plan_exact_workflow_dispatch_lane_skips_smoke_gate(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "heavy",
            "--event-name",
            "workflow_dispatch",
            "--requested-lane",
            "codex.tui-agent-picker-model-surface-targeted",
            "--run-all-lanes",
            "true",
            "--run-core-family",
            "false",
            "--run-attestation-family",
            "false",
            "--run-workflow-family",
            "false",
            "--run-ui-protocol-family",
            "false",
            "--run-docs-family",
            "false",
            "--changed-files-json",
            "[]",
        )

        self.assertEqual(payload["run_smoke_gate"], "false")
        self.assertEqual(payload["smoke_gate_kind"], "")
        self.assertEqual(payload["smoke_heavy_lane_count"], 0)
        self.assertEqual(
            [lane["lane_id"] for lane in payload["selected_matrix"]["include"]],
            ["codex.tui-agent-picker-model-surface-targeted"],
        )


class RustCiModeScriptTests(unittest.TestCase):
    maxDiff = None

    def setUp(self) -> None:
        self.repo = TempGitRepo()
        self.base_sha = self.repo.commit("base", {"README.md": "base\n"})

    def tearDown(self) -> None:
        self.repo.cleanup()

    def run_rust_ci_mode(
        self,
        *,
        event_action: str,
        head_files: dict[str, str],
        previous_green_required: str = "false",
        before_sha: str = "",
    ) -> dict:
        head_sha = self.repo.commit("head", head_files)
        return run_script(
            SCRIPTS_DIR / "resolve_rust_ci_mode.py",
            "--repo-root",
            str(self.repo.root),
            "--event-name",
            "pull_request",
            "--event-action",
            event_action,
            "--base-sha",
            self.base_sha,
            "--head-sha",
            head_sha,
            "--before-sha",
            before_sha,
            "--previous-green-required",
            previous_green_required,
        )

    def test_light_initial_routes_small_openai_models_pr_to_exact_lane(self) -> None:
        outputs = self.run_rust_ci_mode(
            event_action="opened",
            head_files={"codex-rs/protocol/src/openai_models.rs": "first\nsecond\n"},
        )

        self.assertEqual(outputs["validation_mode"], "light_initial")
        self.assertEqual(outputs["run_incremental_validation"], "true")
        self.assertEqual(
            outputs["incremental_lanes"],
            "codex.tui-agent-picker-model-surface-targeted",
        )
        self.assertEqual(outputs["run_general"], "false")
        self.assertEqual(outputs["run_cargo_shear"], "false")

    def test_light_followup_routes_small_spawn_tool_delta_to_shared_lanes(self) -> None:
        green_sha = self.repo.commit(
            "green",
            {"codex-rs/tools/src/agent_tool.rs": "base\n"},
        )
        outputs = run_script(
            SCRIPTS_DIR / "resolve_rust_ci_mode.py",
            "--repo-root",
            str(self.repo.root),
            "--event-name",
            "pull_request",
            "--event-action",
            "synchronize",
            "--base-sha",
            self.base_sha,
            "--head-sha",
            self.repo.commit(
                "followup",
                {"codex-rs/tools/src/agent_tool.rs": "base\nfollowup\n"},
            ),
            "--before-sha",
            green_sha,
            "--previous-green-required",
            "true",
        )

        self.assertEqual(outputs["validation_mode"], "light_followup")
        self.assertEqual(outputs["run_incremental_validation"], "true")
        self.assertEqual(
            outputs["incremental_lanes"],
            ",".join(
                [
                    "codex.tui-agent-picker-model-surface-targeted",
                    "codex.core-subagent-spawn-approval-targeted",
                ]
            ),
        )
        self.assertEqual(outputs["run_argument_comment_lint_prebuilt"], "false")


if __name__ == "__main__":
    unittest.main()
