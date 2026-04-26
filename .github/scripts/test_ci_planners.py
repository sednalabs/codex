#!/usr/bin/env python3
"""Fixture tests for CI planner scripts and follow-up route selection."""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

import yaml


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
AGGREGATE_VALIDATION_SUMMARY = load_module(
    "aggregate_validation_summary_module", SCRIPTS_DIR / "aggregate_validation_summary.py"
)
REPORT_ACTIONS_CACHE_OCCUPANCY = load_module(
    "report_actions_cache_occupancy_module", SCRIPTS_DIR / "report_actions_cache_occupancy.py"
)
CHECK_MARKDOWN_LINKS = load_module(
    "check_markdown_links_module", SCRIPTS_DIR / "check_markdown_links.py"
)
CHECK_WORKFLOW_POLICY = load_module(
    "check_workflow_policy_module", SCRIPTS_DIR / "check_workflow_policy.py"
)
SUMMARIZE_RUST_CI_FULL = load_module(
    "summarize_rust_ci_full_module", SCRIPTS_DIR / "summarize_rust_ci_full.py"
)
SYNC_UPSTREAM_MIRROR = load_module(
    "sync_upstream_mirror_module", SCRIPTS_DIR / "sync_upstream_mirror.py"
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
    payload = yaml.load(workflow_path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)
    return (
        (((payload.get("on") or {}).get("workflow_dispatch") or {}).get("inputs") or {})
        .get("lane", {})
        .get("options", [])
    )


def parse_pull_request_types(workflow_path: Path) -> list[str]:
    payload = yaml.load(workflow_path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)
    return (((payload.get("on") or {}).get("pull_request") or {}).get("types") or [])


def load_workflow_payload(workflow_path: Path) -> dict:
    payload = yaml.load(workflow_path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)
    return payload if isinstance(payload, dict) else {}


def just_recipe_names(header: str) -> list[str]:
    names: list[str] = []
    for recipe_part in header.split(","):
        tokens = recipe_part.strip().split()
        if tokens:
            names.append(tokens[0])
    return names


def just_recipe_bodies(justfile_path: Path) -> dict[str, list[str]]:
    recipes: dict[str, list[str]] = {}
    current_names: list[str] = []
    current_body: list[str] = []
    for line in justfile_path.read_text(encoding="utf-8").splitlines():
        if line and not line.startswith((" ", "\t", "#")) and ":" in line:
            for name in current_names:
                recipes[name] = current_body
            current_names = just_recipe_names(line.split(":", 1)[0].strip())
            current_body = []
        elif current_names:
            current_body.append(line)
    for name in current_names:
        recipes[name] = current_body
    return recipes


def just_recipes_with_nextest(justfile_path: Path) -> set[str]:
    recipes = just_recipe_bodies(justfile_path)
    return {name for name, body in recipes.items() if any("cargo nextest" in line for line in body)}


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


class SyncUpstreamMirrorTests(unittest.TestCase):
    def test_read_only_fallback_uses_live_upstream_when_mirror_is_stale(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            repo, _origin_bare, upstream_bare, _old_sha, new_sha = self.create_fixture(
                Path(tmpdir), mirror_state="stale"
            )

            result = SYNC_UPSTREAM_MIRROR.sync_upstream_mirror(
                repo=repo,
                mode="read-only-fallback",
                upstream_url=str(upstream_bare),
            )

        self.assertEqual(
            {
                "audit_baseline": result["audit_baseline"],
                "expected_mirror_sha": result["expected_mirror_sha"],
                "mirror_audit_args": result["mirror_audit_args"],
                "mirror_state": result["mirror_state"],
                "wrote_mirror": result["wrote_mirror"],
            },
            {
                "audit_baseline": "upstream-ref",
                "expected_mirror_sha": new_sha,
                "mirror_audit_args": ["--mirror-ref", "refs/remotes/upstream/main"],
                "mirror_state": "stale_ff_only",
                "wrote_mirror": False,
            },
        )

    def test_required_write_requires_a_token_even_when_mirror_is_exact(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            repo, _origin_bare, upstream_bare, _old_sha, _new_sha = self.create_fixture(
                Path(tmpdir), mirror_state="exact"
            )

            with self.assertRaisesRegex(
                SYNC_UPSTREAM_MIRROR.MirrorSyncError,
                "missing upstream sync token",
            ):
                SYNC_UPSTREAM_MIRROR.sync_upstream_mirror(
                    repo=repo,
                    mode="required-write",
                    upstream_url=str(upstream_bare),
                )

    def test_required_write_fast_forwards_stale_mirror(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            repo, origin_bare, upstream_bare, _old_sha, new_sha = self.create_fixture(
                Path(tmpdir), mirror_state="stale"
            )

            result = SYNC_UPSTREAM_MIRROR.sync_upstream_mirror(
                repo=repo,
                mode="required-write",
                upstream_url=str(upstream_bare),
                token="dummy-token",
                mirror_push_url=str(origin_bare),
            )
            mirror_sha = self.git(
                origin_bare,
                "--git-dir",
                str(origin_bare),
                "rev-parse",
                "refs/heads/upstream-main",
            )

        self.assertEqual(
            {
                "audit_baseline": result["audit_baseline"],
                "expected_mirror_sha": result["expected_mirror_sha"],
                "mirror_audit_args": result["mirror_audit_args"],
                "mirror_sha": mirror_sha,
                "mirror_state": result["mirror_state"],
                "wrote_mirror": result["wrote_mirror"],
            },
            {
                "audit_baseline": "origin-mirror",
                "expected_mirror_sha": new_sha,
                "mirror_audit_args": [
                    "--mirror-remote",
                    "origin",
                    "--mirror-branch",
                    "upstream-main",
                ],
                "mirror_sha": new_sha,
                "mirror_state": "exact",
                "wrote_mirror": True,
            },
        )

    def create_fixture(
        self, root: Path, *, mirror_state: str
    ) -> tuple[Path, Path, Path, str, str]:
        origin_bare = root / "origin.git"
        upstream_bare = root / "upstream.git"
        source = root / "source"
        repo = root / "repo"

        self.git(root, "init", "--bare", str(origin_bare))
        self.git(root, "init", "--bare", str(upstream_bare))
        self.git(root, "init", "--initial-branch=main", str(source))
        self.git(source, "config", "user.name", "CI Planner Tests")
        self.git(source, "config", "user.email", "ci-planner-tests@example.invalid")

        (source / "payload.txt").write_text("old\n", encoding="utf-8")
        self.git(source, "add", "payload.txt")
        self.git(source, "commit", "-m", "old")
        old_sha = self.git(source, "rev-parse", "HEAD")

        (source / "payload.txt").write_text("new\n", encoding="utf-8")
        self.git(source, "commit", "-am", "new")
        new_sha = self.git(source, "rev-parse", "HEAD")

        self.git(source, "push", str(upstream_bare), "main:refs/heads/main")
        mirror_sha = new_sha if mirror_state == "exact" else old_sha
        self.git(source, "push", str(origin_bare), f"{mirror_sha}:refs/heads/upstream-main")

        self.git(root, "init", "--initial-branch=main", str(repo))
        self.git(repo, "remote", "add", "origin", str(origin_bare))
        self.git(repo, "remote", "add", "upstream", str(upstream_bare))
        return repo, origin_bare, upstream_bare, old_sha, new_sha

    def git(self, cwd: Path, *args: str) -> str:
        proc = subprocess.run(
            ["git", *args],
            cwd=cwd,
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
                "codex.spawn-agent-tool-model-surface-targeted",
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
            [
                "codex.spawn-agent-tool-model-surface-targeted",
                "codex.spawn-agent-description-model-surface-targeted",
            ],
        )

    def test_picker_model_tui_path_reuses_shared_non_tui_routes(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/tui/src/chatwidget.rs"],
            self.routes,
        )
        self.assertEqual(
            lanes,
            [
                "codex.spawn-agent-tool-model-surface-targeted",
                "codex.spawn-agent-description-model-surface-targeted",
            ],
        )

    def test_workflow_ci_route_stays_lightweight(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            [
                ".github/workflows/validation-lab.yml",
                ".github/scripts/resolve_validation_plan.py",
                "docs/validation_workflow.md",
                "justfile",
            ],
            self.routes,
        )
        self.assertEqual(
            lanes,
            [
                "codex.workflow-ci-sanity",
                "codex.downstream-docs-check",
            ],
        )

    def test_workflow_ci_route_accepts_lane_reusable_workflows_and_catalog(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            [
                ".github/workflows/_validation-lane-rust-minimal.yml",
                ".github/workflows/_validation-lane-rust-integration.yml",
                ".github/validation-lanes.json",
                ".github/scripts/test_ci_planners.py",
            ],
            self.routes,
        )
        self.assertEqual(
            lanes,
            [
                "codex.workflow-ci-sanity",
                "codex.downstream-docs-check",
            ],
        )

    def test_workflow_ci_route_accepts_downstream_audit_plumbing(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            [
                ".github/scripts/validation-lanes/downstream-docs-check.sh",
                "scripts/downstream-divergence-audit.py",
            ],
            self.routes,
        )
        self.assertEqual(
            lanes,
            [
                "codex.workflow-ci-sanity",
                "codex.downstream-docs-check",
            ],
        )

    def test_downstream_docs_route_includes_registry_and_tracking_docs(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            [
                "docs/divergences/index.yaml",
                "docs/downstream-divergence-tracking.md",
            ],
            self.routes,
        )
        self.assertEqual(lanes, ["codex.downstream-docs-check"])

    def test_downstream_docs_lane_runs_full_history_audit(self) -> None:
        lane = next(
            lane
            for lane in self.catalog["lanes"]
            if lane["lane_id"] == "codex.downstream-docs-check"
        )
        self.assertEqual(
            lane["script_path"],
            ".github/scripts/validation-lanes/downstream-docs-check.sh",
        )
        self.assertEqual(lane.get("checkout_fetch_depth"), 0)
        self.assertFalse(lane["needs_just"])

    def test_app_server_followup_route_picks_full_carry_bundle(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/app-server/src/router.rs"],
            self.routes,
        )
        self.assertEqual(
            lanes,
            [
                "codex.app-server-protocol-test",
                "codex.app-server-thread-cwd-targeted",
                "codex.blocking-waits-targeted",
            ],
        )

    def test_brokered_tool_replay_route_stays_tight(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/tui/src/app/app_server_adapter.rs"],
            self.routes,
        )
        self.assertEqual(
            lanes,
            [
                "codex.app-server-protocol-test",
                "codex.tui-brokered-tool-replay-targeted",
            ],
        )

    def test_custom_prompt_review_prompt_core_path_stays_targeted(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/core/src/review_prompts.rs"],
            self.routes,
        )
        self.assertEqual(
            lanes,
            ["codex.custom-prompts-targeted"],
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

    def test_workflow_ci_sanity_lane_uses_direct_script_contract(self) -> None:
        lane = next(
            lane
            for lane in self.catalog["lanes"]
            if lane["lane_id"] == "codex.workflow-ci-sanity"
        )
        self.assertEqual(
            lane["script_path"],
            ".github/scripts/validation-lanes/workflow-ci-sanity.sh",
        )
        self.assertEqual(lane["script_args"], [])
        self.assertFalse(lane["needs_just"])

    def test_argument_comment_lint_lane_uses_bazel_setup_contract(self) -> None:
        lane = next(
            lane
            for lane in self.catalog["lanes"]
            if lane["lane_id"] == "codex.argument-comment-lint"
        )
        self.assertEqual(lane["setup_class"], "workflow")
        self.assertTrue(lane["explicit_only"])
        self.assertEqual(
            lane["script_path"],
            ".github/scripts/validation-lanes/argument-comment-lint.sh",
        )
        self.assertEqual(lane["script_args"], [])
        self.assertTrue(lane["needs_bazel"])
        self.assertTrue(lane["needs_linux_build_deps"])
        self.assertTrue(lane["needs_dotslash"])
        self.assertFalse(lane["needs_sccache"])


class ValidationPlanScriptTests(unittest.TestCase):
    maxDiff = None

    def test_lab_targeted_ui_protocol_lane_set_returns_selected_matrix(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "lab",
            "--profile",
            "targeted",
            "--lane-set",
            "ui-protocol",
            "--catalog-path",
            str(REPO_ROOT / ".github/validation-lanes.json"),
        )

        self.assertEqual(payload["run_selected_lanes"], "true")
        self.assertEqual(payload["run_smoke_gate"], "false")
        self.assertEqual(payload["selected_workflow_lane_count"], 0)
        self.assertEqual(payload["selected_node_lane_count"], 0)
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 14)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 4)
        self.assertEqual(payload["selected_release_lane_count"], 0)
        self.assertTrue(
            all(
                lane.get("checkout_fetch_depth") == 1
                for lane in payload["selected_matrix"]["include"]
            )
        )
        self.assertIn("codex.app-server-protocol-test", payload["selected_lane_ids"])
        self.assertIn("codex.blocking-waits-targeted", payload["selected_lane_ids"])

    def test_lab_smoke_profile_uses_wider_rust_integration_parallelism(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "lab",
            "--profile",
            "smoke",
            "--lane-set",
            "all",
            "--catalog-path",
            str(REPO_ROOT / ".github/validation-lanes.json"),
        )

        self.assertEqual(payload["run_smoke_gate"], "true")
        self.assertEqual(payload["smoke_rust_integration_lane_count"], 5)
        self.assertEqual(payload["rust_integration_max_parallel"], "5")

    def test_lab_full_all_tolerates_null_groups_entries(self) -> None:
        catalog_path = REPO_ROOT / ".github/validation-lanes.json"
        catalog = json.loads(catalog_path.read_text(encoding="utf-8"))

        # Reproduce production failure mode where one lane has groups=null.
        catalog["lanes"][0]["groups"] = None

        with tempfile.NamedTemporaryFile("w", encoding="utf-8", suffix=".json") as handle:
            json.dump(catalog, handle)
            handle.flush()

            payload = run_script(
                SCRIPTS_DIR / "resolve_validation_plan.py",
                "lab",
                "--profile",
                "full",
                "--lane-set",
                "all",
                "--catalog-path",
                handle.name,
            )

        self.assertEqual(payload["run_selected_lanes"], "true")
        self.assertIn("planned_matrix", payload)
        self.assertIn("selected_matrix", payload)
        self.assertIn("selected_workflow_matrix", payload)
        self.assertIn("smoke_workflow_matrix", payload)

    def test_lab_targeted_rejects_boolean_checkout_fetch_depth_metadata(self) -> None:
        catalog_path = REPO_ROOT / ".github/validation-lanes.json"
        catalog = json.loads(catalog_path.read_text(encoding="utf-8"))
        catalog["lanes"][0]["checkout_fetch_depth"] = False

        with tempfile.NamedTemporaryFile("w", encoding="utf-8", suffix=".json") as handle:
            json.dump(catalog, handle)
            handle.flush()

            proc = subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "resolve_validation_plan.py"),
                    "lab",
                    "--profile",
                    "targeted",
                    "--lane-set",
                    "all",
                    "--catalog-path",
                    handle.name,
                ],
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(
            "must set checkout_fetch_depth to a non-negative integer",
            proc.stderr,
        )

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

        self.assertEqual(payload["run_smoke_gate"], "true")
        self.assertEqual(payload["smoke_gate_kind"], "runtime")
        self.assertEqual(payload["selected_workflow_lane_count"], 1)
        self.assertEqual(payload["selected_node_lane_count"], 0)
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 2)
        self.assertEqual(payload["selected_rust_minimal_batch_count"], 5)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 1)
        self.assertEqual(payload["selected_rust_integration_batch_count"], 5)
        self.assertEqual(payload["selected_release_lane_count"], 1)
        self.assertEqual(payload["smoke_rust_integration_lane_count"], 5)
        self.assertEqual(payload["workflow_max_parallel"], "8")
        self.assertEqual(payload["node_max_parallel"], "4")
        self.assertEqual(payload["rust_minimal_max_parallel"], "6")
        self.assertEqual(payload["rust_integration_max_parallel"], "2")
        self.assertEqual(payload["release_max_parallel"], "1")
        self.assertEqual(payload["rust_batching_mode"], "auto")

    def test_heavy_plan_can_disable_rust_batching(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "heavy",
            "--event-name",
            "workflow_dispatch",
            "--requested-lane",
            "all",
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
            "--rust-batching",
            "off",
        )

        self.assertEqual(payload["selected_rust_minimal_batch_count"], 0)
        self.assertEqual(payload["selected_rust_integration_batch_count"], 0)
        self.assertGreater(payload["selected_rust_minimal_lane_count"], 0)
        self.assertGreater(payload["selected_rust_integration_lane_count"], 0)

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
                "codex.spawn-agent-tool-model-surface-targeted",
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
        self.assertEqual(payload["smoke_rust_integration_lane_count"], 0)
        self.assertEqual(payload["selected_workflow_lane_count"], 0)
        self.assertEqual(payload["selected_node_lane_count"], 0)
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 1)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 0)
        self.assertEqual(
            [lane["lane_id"] for lane in payload["selected_matrix"]["include"]],
            ["codex.tui-agent-picker-model-surface-targeted"],
        )

    def test_heavy_plan_route_keeps_workflow_ci_changes_on_workflow_lanes(self) -> None:
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
            "true",
            "--run-ui-protocol-family",
            "false",
            "--run-docs-family",
            "true",
            "--changed-files-json",
            json.dumps(
                [
                    ".github/workflows/validation-lab.yml",
                    ".github/scripts/resolve_validation_plan.py",
                    "docs/validation_workflow.md",
                    "justfile",
                ]
            ),
        )

        self.assertEqual(payload["run_smoke_gate"], "false")
        self.assertEqual(payload["selected_workflow_lane_count"], 2)
        self.assertEqual(payload["selected_node_lane_count"], 0)
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 0)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 0)
        self.assertEqual(payload["selected_release_lane_count"], 0)
        self.assertEqual(
            [lane["lane_id"] for lane in payload["selected_matrix"]["include"]],
            [
                "codex.workflow-ci-sanity",
                "codex.downstream-docs-check",
            ],
        )

    def test_validation_lab_selected_lanes_do_not_block_on_smoke_gate(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        jobs = payload.get("jobs") or {}

        self.assertEqual((jobs.get("workflow_lanes") or {}).get("needs"), ["metadata"])
        self.assertEqual((jobs.get("node_lanes") or {}).get("needs"), ["metadata"])
        self.assertEqual((jobs.get("rust_minimal_lanes") or {}).get("needs"), ["metadata"])
        self.assertEqual((jobs.get("rust_integration_lanes") or {}).get("needs"), ["metadata"])
        self.assertEqual((jobs.get("release_lanes") or {}).get("needs"), ["metadata"])

    def test_validation_lab_summary_waits_for_smoke_and_selected_fanout(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        jobs = payload.get("jobs") or {}
        summary = jobs.get("summary") or {}

        self.assertEqual(
            summary.get("needs"),
            [
                "metadata",
                "smoke_workflow_lanes",
                "smoke_node_lanes",
                "smoke_rust_minimal_lanes",
                "smoke_rust_integration_lanes",
                "smoke_release_lanes",
                "workflow_lanes",
                "node_lanes",
                "rust_minimal_lanes",
                "rust_integration_lanes",
                "release_lanes",
                "artifact",
            ],
        )

    def test_validation_lab_only_fetches_target_history_for_artifact_versioning(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        metadata_steps = (((payload.get("jobs") or {}).get("metadata") or {}).get("steps") or [])
        target_checkout = next(
            step for step in metadata_steps if step.get("name") == "Check out validation target"
        )

        self.assertEqual(
            (target_checkout.get("with") or {}).get("fetch-depth"),
            "${{ (inputs.profile == 'artifact' || inputs.artifact_build) && '0' || '1' }}",
        )

        compute_plan = next(
            step for step in metadata_steps if step.get("name") == "Compute validation-lab plan"
        )
        run_script = compute_plan.get("run") or ""
        self.assertIn('if [[ "${LAB_PROFILE}" == "artifact"', run_script)
        self.assertIn("git -C \"${target_checkout}\" tag --merged HEAD", run_script)

    def test_just_recipe_bodies_handles_comma_separated_recipe_names(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            justfile = Path(tmpdir) / "justfile"
            justfile.write_text(
                "\n".join(
                    [
                        "foo, bar:",
                        "    cargo nextest run -p codex-core",
                        "",
                        "with-param target='default':",
                        "    cargo test -p codex-tui",
                        "",
                    ]
                ),
                encoding="utf-8",
            )

            recipes = just_recipe_bodies(justfile)

        self.assertEqual(recipes["foo"], ["    cargo nextest run -p codex-core", ""])
        self.assertEqual(recipes["bar"], ["    cargo nextest run -p codex-core", ""])
        self.assertEqual(recipes["with-param"], ["    cargo test -p codex-tui"])
        self.assertNotIn("foo,", recipes)

    def test_run_just_recipe_lanes_declare_nextest_when_recipe_uses_nextest(self) -> None:
        catalog = RESOLVE_VALIDATION_PLAN.load_catalog()
        nextest_recipes = just_recipes_with_nextest(REPO_ROOT / "justfile")
        missing: list[str] = []
        for lane in catalog["lanes"]:
            if lane.get("script_path") != ".github/scripts/validation-lanes/run-just-recipe.sh":
                continue
            script_args = lane.get("script_args") or []
            recipe = script_args[0] if script_args else ""
            if recipe in nextest_recipes and not lane.get("needs_nextest"):
                missing.append(str(lane.get("lane_id")))

        self.assertEqual(missing, [])

    def test_run_just_recipe_lanes_declare_linux_build_deps_when_recipe_compiles_linux_sandbox(
        self,
    ) -> None:
        catalog = RESOLVE_VALIDATION_PLAN.load_catalog()
        recipe_bodies = just_recipe_bodies(REPO_ROOT / "justfile")
        direct_linux_build_deps_recipes = {
            name
            for name, body in recipe_bodies.items()
            if any(
                command in line
                for line in body
                for command in ("cargo test", "cargo nextest", "cargo check", "cargo build")
            )
            and any("codex-core" in line or "codex-tui" in line for line in body)
        }
        nested_linux_build_deps_recipes = {
            name
            for name, body in recipe_bodies.items()
            if any("just --justfile ../justfile" in line for line in body)
            and any(
                any(subrecipe in line for subrecipe in direct_linux_build_deps_recipes)
                for line in body
            )
        }
        linux_build_deps_recipes = direct_linux_build_deps_recipes | nested_linux_build_deps_recipes
        missing: list[str] = []
        for lane in catalog["lanes"]:
            if lane.get("script_path") != ".github/scripts/validation-lanes/run-just-recipe.sh":
                continue
            script_args = lane.get("script_args") or []
            recipe = script_args[0] if script_args else ""
            if recipe in linux_build_deps_recipes and not lane.get("needs_linux_build_deps"):
                missing.append(str(lane.get("lane_id")))

        self.assertEqual(missing, [])

    def test_expensive_rust_minimal_lanes_enable_sccache(self) -> None:
        catalog = RESOLVE_VALIDATION_PLAN.load_catalog()
        enabled = {
            lane["lane_id"]
            for lane in catalog["lanes"]
            if lane.get("setup_class") == "rust_minimal" and lane.get("needs_sccache")
        }
        self.assertEqual(
            enabled,
            {
                "codex.app-server-protocol-test",
                "codex.native-computer-use-tool-registry-targeted",
                "codex.core-subagent-notification-visibility-targeted",
                "codex.spawn-agent-description-model-surface-targeted",
                "codex.spawn-agent-tool-model-surface-targeted",
                "codex.tui-agent-picker-model-surface-targeted",
                "codex.tui-agent-picker-targeted",
                "codex.tui-agent-picker-tree-targeted",
                "codex.tui-agent-picker-usage-targeted",
                "codex.tui-agent-usage-totals-targeted",
                "codex.tui-brokered-tool-replay-targeted",
                "codex.tui-config-refresh-session-targeted",
                "codex.tui-esc-interrupt-targeted",
                "codex.tui-front-queue-submit-targeted",
                "codex.tui-thread-session-policy-targeted",
                "codex.tui-transcript-viewport-targeted",
                "codex.tui-weekly-pacing-status-line-targeted",
            },
        )

    def test_tui_weekly_pacing_lane_pins_live_status_line_contract(self) -> None:
        catalog = RESOLVE_VALIDATION_PLAN.load_catalog()
        lane = next(
            lane
            for lane in catalog["lanes"]
            if lane["lane_id"] == "codex.tui-weekly-pacing-status-line-targeted"
        )
        self.assertEqual(
            lane["script_path"], ".github/scripts/validation-lanes/run-just-recipe.sh"
        )
        self.assertEqual(lane["script_args"], ["tui-weekly-pacing-status-line-targeted"])

        recipe = "\n".join(
            just_recipe_bodies(REPO_ROOT / "justfile")[
                "tui-weekly-pacing-status-line-targeted"
            ]
        )
        self.assertIn("--exact", recipe)
        for test_name in [
            "status_line_weekly_limit_renders_pacing_suffixes_from_live_status_line",
            "status_line_weekly_limit_renders_stale_suffix_over_pace_details",
            "status_line_weekly_limit_omits_pacing_when_inputs_are_missing",
        ]:
            self.assertIn(test_name, recipe)

    def test_validation_lab_passes_sccache_policy_only_to_sccache_lanes(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        jobs = payload.get("jobs") or {}
        expected_policy = (
            "${{ inputs.supersession_mode != 'auto' && "
            "'write-fallback' || 'restore-only' }}"
        )

        sccache_jobs = [
            "smoke_rust_minimal_lanes",
            "smoke_rust_integration_lanes",
            "smoke_release_lanes",
            "rust_minimal_lanes",
            "rust_integration_lanes",
            "release_lanes",
            "artifact",
        ]
        for job_name in sccache_jobs:
            with self.subTest(job=job_name):
                self.assertEqual(
                    ((jobs.get(job_name) or {}).get("with") or {}).get("cache_policy"),
                    expected_policy,
                )

        non_sccache_jobs = [
            "smoke_workflow_lanes",
            "smoke_node_lanes",
            "workflow_lanes",
            "node_lanes",
        ]
        for job_name in non_sccache_jobs:
            with self.subTest(job=job_name):
                self.assertNotIn("cache_policy", (jobs.get(job_name) or {}).get("with") or {})

    def test_validation_lab_passes_bazel_setup_to_workflow_lanes(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        jobs = payload.get("jobs") or {}

        for job_name in ["smoke_workflow_lanes", "workflow_lanes"]:
            with self.subTest(job=job_name):
                self.assertEqual(
                    ((jobs.get(job_name) or {}).get("with") or {}).get("needs_bazel"),
                    "${{ matrix.needs_bazel }}",
                )

    def test_validation_lab_workflow_lanes_do_not_inherit_secrets_from_operator_refs(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        jobs = payload.get("jobs") or {}

        for job_name in ["smoke_workflow_lanes", "workflow_lanes"]:
            with self.subTest(job=job_name):
                self.assertNotIn("secrets", jobs.get(job_name) or {})

    def test_sedna_heavy_writes_fallback_cache_only_for_manual_dispatch(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml")
        jobs = payload.get("jobs") or {}
        expected_policy = "${{ github.event_name == 'workflow_dispatch' && 'write-fallback' || 'restore-only' }}"

        for job_name in [
            "smoke_rust_minimal_lanes",
            "smoke_rust_integration_lanes",
            "smoke_release_lanes",
            "rust_minimal_lanes",
            "rust_minimal_batches",
            "rust_integration_lanes",
            "rust_integration_batches",
            "release_lanes",
        ]:
            with self.subTest(job=job_name):
                self.assertEqual(
                    ((jobs.get(job_name) or {}).get("with") or {}).get("cache_policy"),
                    expected_policy,
                )

    def test_sedna_heavy_passes_bazel_setup_to_workflow_lanes(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml")
        jobs = payload.get("jobs") or {}

        for job_name in ["smoke_workflow_lanes", "workflow_lanes"]:
            with self.subTest(job=job_name):
                self.assertEqual(
                    ((jobs.get(job_name) or {}).get("with") or {}).get("needs_bazel"),
                    "${{ matrix.needs_bazel }}",
                )

    def test_reusable_sccache_workflows_require_explicit_fallback_writes(self) -> None:
        for workflow_name in [
            "_validation-lane-rust-minimal.yml",
            "_validation-lane-rust-integration.yml",
            "_validation-lane-rust-batch.yml",
            "_validation-lane-release.yml",
            "_sedna-linux-rust.yml",
        ]:
            with self.subTest(workflow=workflow_name):
                workflow_path = REPO_ROOT / ".github/workflows" / workflow_name
                workflow_text = workflow_path.read_text(encoding="utf-8")
                payload = load_workflow_payload(workflow_path)
                inputs = (((payload.get("on") or {}).get("workflow_call") or {}).get("inputs") or {})
                self.assertEqual((inputs.get("checkout_fetch_depth") or {}).get("default"), "1")
                self.assertEqual((inputs.get("cache_policy") or {}).get("default"), "restore-only")
                self.assertNotIn("ACTIONS_RUNTIME_TOKEN", workflow_text)
                self.assertNotIn("SCCACHE_GHA_ENABLED=true", workflow_text)

                run_job = (payload.get("jobs") or {}).get("run") or {}
                checkout_step = next(
                    step
                    for step in run_job.get("steps") or []
                    if step.get("uses") == "actions/checkout@v6"
                )
                self.assertEqual(
                    (checkout_step.get("with") or {}).get("fetch-depth"),
                    "${{ inputs.checkout_fetch_depth }}",
                )
                self.assertEqual((run_job.get("env") or {}).get("SCCACHE_CACHE_SIZE"), "2G")
                self.assertFalse(
                    any(
                        step.get("name") == "Expose GitHub cache-service env for sccache"
                        for step in run_job.get("steps") or []
                    )
                )
                configure_step = next(
                    step
                    for step in run_job.get("steps") or []
                    if step.get("name") == "Configure sccache backend"
                )
                self.assertIn("configure_sccache_backend.sh", configure_step.get("run") or "")

                save_step = next(
                    step
                    for step in run_job.get("steps") or []
                    if step.get("name") == "Save sccache cache (fallback)"
                )
                self.assertIn(
                    "steps.sccache_backend.outputs.policy == 'write-fallback'",
                    save_step.get("if") or "",
                )

    def test_reusable_validation_lane_workflows_source_helpers_from_workflow_ref(self) -> None:
        for workflow_name in [
            "_validation-lane-workflow.yml",
            "_validation-lane-node.yml",
            "_validation-lane-rust-minimal.yml",
            "_validation-lane-rust-integration.yml",
            "_validation-lane-release.yml",
        ]:
            with self.subTest(workflow=workflow_name):
                payload = load_workflow_payload(REPO_ROOT / ".github/workflows" / workflow_name)
                run_job = (payload.get("jobs") or {}).get("run") or {}
                checkout_steps = [
                    step
                    for step in run_job.get("steps") or []
                    if step.get("uses") == "actions/checkout@v6"
                ]
                self.assertGreaterEqual(len(checkout_steps), 2)
                self.assertEqual(
                    (checkout_steps[1].get("with") or {}).get("ref"),
                    "${{ github.sha }}",
                )
                self.assertEqual(
                    (checkout_steps[1].get("with") or {}).get("path"),
                    ".workflow-src",
                )

                run_lane_step = next(
                    step
                    for step in run_job.get("steps") or []
                    if step.get("name") == "Run requested lane script"
                )
                self.assertIn(
                    ".workflow-src/.github/scripts/run_validation_lane.py",
                    run_lane_step.get("run") or "",
                )

                summary_step = next(
                    step
                    for step in run_job.get("steps") or []
                    if step.get("name") == "Prepare lane summary artifact"
                )
                self.assertIn(
                    ".workflow-src/.github/scripts/write_lane_summary.py",
                    summary_step.get("run") or "",
                )

    def test_validation_lane_workflow_keeps_secrets_out_of_target_controlled_scripts(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/_validation-lane-workflow.yml")
        workflow_call = ((payload.get("on") or {}).get("workflow_call") or {})
        self.assertNotIn("secrets", workflow_call)

        run_job = (payload.get("jobs") or {}).get("run") or {}
        run_lane_step = next(
            step
            for step in run_job.get("steps") or []
            if step.get("name") == "Run requested lane script"
        )
        run_lane_env = run_lane_step.get("env") or {}
        for env_name, env_value in run_lane_env.items():
            with self.subTest(env=env_name):
                self.assertNotRegex(env_name, r"(API_KEY|PRIVATE_KEY|SECRET|TOKEN)")
                self.assertNotIn("secrets.", str(env_value))

    def test_sedna_sync_upstream_uses_github_app_token_and_shared_helper(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-sync-upstream.yml")
        sync_job = ((payload.get("jobs") or {}).get("sync") or {})
        steps = sync_job.get("steps") or []

        credential_step = next(
            step
            for step in steps
            if step.get("name") == "Resolve upstream sync credential mode"
        )
        self.assertIn("SEDNA_SYNC_UPSTREAM_APP_CLIENT_ID", (credential_step.get("env") or {}).get("APP_CLIENT_ID", ""))
        self.assertIn("SEDNA_SYNC_UPSTREAM_APP_PRIVATE_KEY", (credential_step.get("env") or {}).get("APP_PRIVATE_KEY", ""))

        token_step = next(
            step
            for step in steps
            if step.get("name") == "Generate upstream sync app token"
        )
        self.assertEqual(
            token_step.get("if"),
            "${{ steps.credential-mode.outputs.use_app_token == 'true' }}",
        )
        self.assertEqual(token_step.get("uses"), "actions/create-github-app-token@v3")
        self.assertEqual(
            token_step.get("with") or {},
            {
                "client-id": "${{ vars.SEDNA_SYNC_UPSTREAM_APP_CLIENT_ID }}",
                "private-key": "${{ secrets.SEDNA_SYNC_UPSTREAM_APP_PRIVATE_KEY }}",
                "permission-contents": "write",
                "permission-workflows": "write",
            },
        )

        sync_step = next(
            step for step in steps if step.get("name") == "Fast-forward upstream mirror"
        )
        self.assertIn(".github/scripts/sync_upstream_mirror.py", sync_step.get("run") or "")
        self.assertIn("--mode required-write", sync_step.get("run") or "")
        self.assertEqual(
            (sync_job.get("outputs") or {}).get("synced_upstream_main_sha"),
            "${{ steps.sync-mirror.outputs.synced_upstream_main_sha }}",
        )

    def test_rust_ci_full_fallback_sccache_writes_are_disabled_by_default(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci-full.yml")
        jobs = payload.get("jobs") or {}

        for job_name in ["lint_build", "nextest_archive"]:
            with self.subTest(job=job_name):
                job = jobs.get(job_name) or {}
                workflow_text = (REPO_ROOT / ".github/workflows/rust-ci-full.yml").read_text(
                    encoding="utf-8"
                )
                env = job.get("env") or {}
                self.assertEqual(env.get("SCCACHE_CACHE_SIZE"), "2G")
                self.assertEqual(env.get("SCCACHE_FALLBACK_CACHE_POLICY"), "restore-only")
                self.assertNotIn("ACTIONS_RUNTIME_TOKEN", workflow_text)
                self.assertNotIn("SCCACHE_GHA_ENABLED=true", workflow_text)

                save_step = next(
                    step
                    for step in job.get("steps") or []
                    if step.get("name") == "Save sccache cache (fallback)"
                )
                self.assertIn(
                    "steps.sccache_backend.outputs.policy == 'write-fallback'",
                    save_step.get("if") or "",
                )
                install_step = next(
                    step for step in job.get("steps") or [] if step.get("name") == "Install sccache"
                )
                self.assertNotIn("version", install_step.get("with") or {})

    def test_rust_ci_full_runs_after_successful_scheduled_rust_ci_only(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci-full.yml")
        trigger = payload.get("on") or {}
        jobs = payload.get("jobs") or {}

        self.assertEqual(
            ((trigger.get("workflow_run") or {}).get("workflows") or []),
            ["rust-ci"],
        )
        self.assertNotIn("schedule", trigger)
        self.assertEqual(payload.get("permissions"), {"actions": "read", "contents": "read"})

        gate = (jobs.get("matrix_plan") or {}).get("if") or ""
        self.assertIn("github.event.workflow_run.event == 'schedule'", gate)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", gate)
        self.assertIn("github.event.workflow_run.head_branch == 'main'", gate)

        result_gate = (jobs.get("results") or {}).get("if") or ""
        self.assertIn("always()", result_gate)
        self.assertIn("github.event.workflow_run.event == 'schedule'", result_gate)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", result_gate)

    def test_rust_ci_full_results_understands_archive_and_remote_test_jobs(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci-full.yml")
        jobs = payload.get("jobs") or {}
        results = jobs.get("results") or {}
        steps = results.get("steps") or []

        self.assertEqual(
            results.get("needs"),
            [
                "general",
                "cargo_shear",
                "matrix_plan",
                "argument_comment_lint_package",
                "argument_comment_lint_prebuilt",
                "lint_build",
                "nextest_archive",
                "tests",
                "remote_tests",
            ],
        )
        self.assertIn("remote_tests_matrix", (jobs.get("matrix_plan") or {}).get("outputs") or {})
        self.assertEqual((jobs.get("tests") or {}).get("needs"), ["matrix_plan", "nextest_archive"])
        self.assertEqual(
            (jobs.get("remote_tests") or {}).get("needs"), ["matrix_plan", "nextest_archive"]
        )

        download_step = next(
            step for step in steps if step.get("name") == "Download failure summaries"
        )
        self.assertEqual(
            download_step.get("uses"),
            "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
        )
        self.assertEqual((download_step.get("with") or {}).get("pattern"), "rust-ci-full-*-summary-*")
        self.assertEqual((download_step.get("with") or {}).get("merge-multiple"), "true")

        aggregate_step = next(
            step for step in steps if step.get("name") == "Build structured summary"
        )
        self.assertIn("summarize_rust_ci_full.py aggregate", aggregate_step.get("run") or "")
        verify_step = next(step for step in steps if step.get("name") == "Verify full CI result")
        verify_run = verify_step.get("run") or ""
        self.assertIn("require_success \"nextest_archive\"", verify_run)
        self.assertIn("require_success \"remote_tests\"", verify_run)

    def test_rust_ci_full_archive_test_and_results_jobs_do_not_receive_secrets(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci-full.yml")
        jobs = payload.get("jobs") or {}

        for job_name in ["lint_build", "nextest_archive", "tests", "remote_tests", "results"]:
            with self.subTest(job=job_name):
                job = jobs.get(job_name) or {}
                self.assertNotIn("secrets", job)
                self.assertNotIn("secrets.", json.dumps(job, sort_keys=True))
                self.assertNotIn("ACTIONS_RUNTIME_TOKEN", json.dumps(job, sort_keys=True))
                self.assertNotIn("SCCACHE_GHA_ENABLED=true", json.dumps(job, sort_keys=True))

    def test_rust_ci_full_nextest_archive_is_reused_by_test_families(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci-full.yml")
        jobs = payload.get("jobs") or {}

        archive_steps = (jobs.get("nextest_archive") or {}).get("steps") or []
        archive_run = next(
            step for step in archive_steps if step.get("name") == "Build nextest archive"
        ).get("run") or ""
        self.assertIn("cargo nextest archive", archive_run)
        self.assertIn("--archive-file", archive_run)

        for job_name in ["tests", "remote_tests"]:
            with self.subTest(job=job_name):
                steps = (jobs.get(job_name) or {}).get("steps") or []
                download_step = next(
                    step for step in steps if step.get("name") == "Download nextest archive"
                )
                self.assertEqual(
                    (download_step.get("with") or {}).get("name"),
                    "rust-ci-full-nextest-archive-${{ matrix.target }}-${{ matrix.profile }}",
                )
                run_step = next(
                    step
                    for step in steps
                    if step.get("name") in {"tests", "remote tests"}
                )
                self.assertIn("cargo nextest run", run_step.get("run") or "")
                self.assertIn("--archive-file", run_step.get("run") or "")

        remote_matrix = (
            (jobs.get("matrix_plan") or {})
            .get("outputs", {})
            .get("remote_tests_matrix", "")
        )
        self.assertEqual(
            remote_matrix,
            "${{ steps.plan.outputs.remote_tests_matrix }}",
        )
        plan_run = (
            ((jobs.get("matrix_plan") or {}).get("steps") or [])[0].get("run") or ""
        )
        self.assertNotIn('"filter"', plan_run)
        remote_run = next(
            step
            for step in (jobs.get("remote_tests") or {}).get("steps") or []
            if step.get("name") == "remote tests"
        ).get("run") or ""
        self.assertNotIn(" -E ", remote_run)

    def test_rust_ci_full_summary_parser_extracts_compact_blockers(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            nextest_log = root / "nextest.log"
            nextest_log.write_text(
                "\n".join(
                    [
                        "Starting 3 tests across 2 binaries (1 tests skipped)",
                        "        FAIL [   0.042s] (1/3) codex_core::remote_env::fails_cleanly",
                        "     TIMEOUT [  60.000s] (2/3) codex_core::remote_exec_server::hangs",
                    ]
                ),
                encoding="utf-8",
            )
            clippy_log = root / "clippy.log"
            clippy_log.write_text(
                "\n".join(
                    [
                        "error: this expression creates a reference",
                        "  --> codex-rs/core/src/lib.rs:12:34",
                        "error: could not compile `codex-core` due to 1 previous error",
                    ]
                ),
                encoding="utf-8",
            )

            nextest = SUMMARIZE_RUST_CI_FULL.nextest_summary(nextest_log, "nextest-linux")
            clippy = SUMMARIZE_RUST_CI_FULL.clippy_summary(clippy_log, "clippy-linux")

        self.assertEqual(
            nextest,
            {
                "type": "nextest",
                "suite": "nextest-linux",
                "log_missing": False,
                "started": {"tests": 3, "binaries": 2, "skipped": 1},
                "failure_signal_count": 2,
                "unique_failure_count": 2,
                "status_counts": {"FAIL": 1, "TIMEOUT": 1},
                "failures": [
                    {
                        "status": "fail",
                        "duration": "0.042s",
                        "test": "codex_core::remote_env::fails_cleanly",
                    },
                    {
                        "status": "timeout",
                        "duration": "60.000s",
                        "test": "codex_core::remote_exec_server::hangs",
                    },
                ],
                "truncated": False,
            },
        )
        self.assertEqual(
            clippy,
            {
                "type": "clippy",
                "suite": "clippy-linux",
                "log_missing": False,
                "error_count": 1,
                "errors": [
                    {
                        "message": "this expression creates a reference",
                        "location": "codex-rs/core/src/lib.rs:12:34",
                    }
                ],
                "truncated": False,
            },
        )

    def test_rust_ci_full_summary_aggregate_keeps_skips_non_blocking(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            summary_dir = root / "summaries"
            summary_dir.mkdir()
            (summary_dir / "nextest.json").write_text(
                json.dumps(
                    {
                        "type": "nextest",
                        "suite": "nextest-linux",
                        "failures": [{"status": "fail", "test": "a::test"}],
                        "unique_failure_count": 1,
                    }
                ),
                encoding="utf-8",
            )
            output = root / "summary.json"
            SUMMARIZE_RUST_CI_FULL.aggregate_summary(
                needs_json=json.dumps(
                    {
                        "general": {"result": "skipped"},
                        "tests": {"result": "failure"},
                        "remote_tests": {"result": "success"},
                    }
                ),
                summary_dir=summary_dir,
                checkout_ref="abc123",
                source_event="schedule",
                output=output,
            )
            payload = json.loads(output.read_text(encoding="utf-8"))

        self.assertEqual(payload["checkout_ref"], "abc123")
        self.assertEqual(payload["source_event"], "schedule")
        self.assertEqual(
            payload["jobs"],
            {"general": "skipped", "remote_tests": "success", "tests": "failure"},
        )
        self.assertEqual(
            payload["primary_blockers"],
            [
                {"type": "job", "job": "tests", "result": "failure"},
                {
                    "type": "nextest",
                    "suite": "nextest-linux",
                    "status": "fail",
                    "test": "a::test",
                    "unique_failure_count": 1,
                },
            ],
        )

    def test_lane_summary_records_cache_telemetry_without_raw_command(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "summary.json"
            subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "write_lane_summary.py"),
                    "--lane-id",
                    "codex.example",
                    "--summary-title",
                    "example",
                    "--run-command",
                    "cargo test --locked",
                    "--cache-policy",
                    "restore-only",
                    "--cache-backend",
                    "fallback",
                    "--sccache-restore-mode",
                    "restore-key-or-miss",
                    "--output",
                    str(output),
                ],
                check=True,
            )

            summary = json.loads(output.read_text(encoding="utf-8"))

        self.assertEqual(summary["script_path"], "legacy-run-command")
        self.assertEqual(summary["cache_policy"], "restore-only")
        self.assertEqual(summary["cache_backend"], "fallback")
        self.assertEqual(summary["sccache_restore_mode"], "restore-key-or-miss")
        self.assertNotIn("run_command", summary)

    def test_lane_summary_records_script_metadata_and_cache_telemetry(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "summary.json"
            subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "write_lane_summary.py"),
                    "--lane-id",
                    "codex.example",
                    "--summary-title",
                    "example",
                    "--script-path",
                    ".github/scripts/validation-lanes/run-just-recipe.sh",
                    "--script-args-json",
                    '["blocking-waits-targeted"]',
                    "--cache-policy",
                    "restore-only",
                    "--cache-backend",
                    "gha",
                    "--sccache-restore-mode",
                    "not-applicable",
                    "--output",
                    str(output),
                ],
                check=True,
            )

            summary = json.loads(output.read_text(encoding="utf-8"))

        self.assertEqual(
            summary["script_path"], ".github/scripts/validation-lanes/run-just-recipe.sh"
        )
        self.assertEqual(summary["script_args"], ["blocking-waits-targeted"])
        self.assertEqual(summary["cache_policy"], "restore-only")
        self.assertEqual(summary["cache_backend"], "gha")
        self.assertEqual(summary["sccache_restore_mode"], "not-applicable")
        self.assertNotIn("run_command", summary)

    def test_validation_lab_frontier_all_widens_to_all_active_non_explicit_lanes(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "lab",
            "--profile",
            "frontier",
            "--lane-set",
            "all",
            "--lanes",
            "",
            "--artifact-build",
            "false",
        )

        selected_lane_ids = [lane["lane_id"] for lane in payload["selected_matrix"]["include"]]
        self.assertIn("codex.downstream-docs-check", selected_lane_ids)
        self.assertIn("codex.workflow-ci-sanity", selected_lane_ids)
        self.assertIn("sedna.release-linux-smoke", selected_lane_ids)
        self.assertIn("codex.tui-config-refresh-session-targeted", selected_lane_ids)
        self.assertIn("codex.spawn-agent-description-model-surface-targeted", selected_lane_ids)
        self.assertNotIn("codex.tui-agent-picker-model-surface-targeted", selected_lane_ids)
        self.assertEqual(payload["selected_workflow_lane_count"], 4)
        self.assertEqual(payload["selected_node_lane_count"], 1)
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 17)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 15)
        self.assertEqual(payload["selected_release_lane_count"], 1)
        self.assertEqual(payload["workflow_max_parallel"], "4")
        self.assertEqual(payload["node_max_parallel"], "1")
        self.assertEqual(payload["rust_minimal_max_parallel"], "17")
        self.assertEqual(payload["rust_integration_max_parallel"], "8")
        self.assertEqual(payload["release_max_parallel"], "1")

    def test_validation_lab_frontier_all_can_include_explicit_only_lanes(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "lab",
            "--profile",
            "frontier",
            "--lane-set",
            "all",
            "--lanes",
            "",
            "--artifact-build",
            "false",
            "--include-explicit-lanes",
            "true",
        )

        selected_lane_ids = [lane["lane_id"] for lane in payload["selected_matrix"]["include"]]
        self.assertIn("codex.tui-agent-picker-model-surface-targeted", selected_lane_ids)
        self.assertIn("codex.argument-comment-lint", selected_lane_ids)
        self.assertIn("downstream-ledger-seam", selected_lane_ids)
        self.assertEqual(payload["selected_workflow_lane_count"], 5)
        self.assertEqual(payload["selected_node_lane_count"], 1)
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 19)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 16)
        self.assertEqual(payload["selected_release_lane_count"], 1)
        self.assertEqual(payload["rust_minimal_max_parallel"], "19")
        self.assertEqual(payload["rust_integration_max_parallel"], "8")

    def test_validation_lab_frontier_all_excludes_smoke_gate_lanes_by_metadata(self) -> None:
        catalog = {
            "lanes": [
                {
                    "lane_id": "codex.synthetic-runtime-gate",
                    "groups": ["core"],
                    "status_class": "active",
                    "setup_class": "rust_integration",
                    "frontier_role": "sentinel",
                    "summary_family": "synthetic-gate",
                    "cost_class": "high",
                    "working_directory": ".",
                    "script_path": ".github/scripts/validation-lanes/run-just-recipe.sh",
                    "script_args": ["synthetic-runtime-gate"],
                    "needs_just": True,
                    "needs_node": False,
                    "needs_nextest": False,
                    "needs_linux_build_deps": True,
                    "needs_dotslash": True,
                    "needs_sccache": True,
                    "smoke_gate_only": True,
                    "smoke_gate_kinds": ["runtime"],
                },
                {
                    "lane_id": "codex.synthetic-real-lane",
                    "groups": ["core"],
                    "status_class": "active",
                    "setup_class": "rust_minimal",
                    "frontier_role": "sentinel",
                    "summary_family": "synthetic-real-lane",
                    "cost_class": "medium",
                    "working_directory": ".",
                    "script_path": ".github/scripts/validation-lanes/run-just-recipe.sh",
                    "script_args": ["synthetic-real-lane"],
                    "needs_just": True,
                    "needs_node": False,
                    "needs_nextest": False,
                    "needs_linux_build_deps": False,
                    "needs_dotslash": False,
                    "needs_sccache": False,
                },
            ]
        }

        selected = RESOLVE_VALIDATION_PLAN.select_frontier_all(catalog)

        self.assertEqual(
            [lane["lane_id"] for lane in selected],
            ["codex.synthetic-real-lane"],
        )

    def test_frontier_helper_rejects_boolean_checkout_fetch_depth(self) -> None:
        catalog = {
            "lanes": [
                {
                    "lane_id": "codex.synthetic-real-lane",
                    "groups": ["core"],
                    "status_class": "active",
                    "setup_class": "rust_minimal",
                    "frontier_role": "sentinel",
                    "summary_family": "synthetic-real-lane",
                    "cost_class": "medium",
                    "checkout_fetch_depth": True,
                    "working_directory": ".",
                    "script_path": ".github/scripts/validation-lanes/run-just-recipe.sh",
                    "script_args": ["synthetic-real-lane"],
                    "needs_just": True,
                    "needs_node": False,
                    "needs_nextest": False,
                    "needs_linux_build_deps": False,
                    "needs_dotslash": False,
                    "needs_sccache": False,
                }
            ]
        }

        with self.assertRaisesRegex(
            SystemExit,
            "must set checkout_fetch_depth to a non-negative integer",
        ):
            RESOLVE_VALIDATION_PLAN.select_frontier_all(catalog)

    def test_heavy_plan_workflow_dispatch_all_uses_frontier_harvest_policy(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "heavy",
            "--event-name",
            "workflow_dispatch",
            "--requested-lane",
            "all",
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

        self.assertEqual(payload["matrix_fail_fast"], "false")
        self.assertEqual(payload["continue_after_smoke_failure"], "true")
        self.assertEqual(payload["eager_release_lanes"], "true")
        self.assertEqual(payload["workflow_max_parallel"], "5")
        self.assertEqual(payload["node_max_parallel"], "1")
        self.assertEqual(payload["rust_minimal_max_parallel"], "19")
        self.assertEqual(payload["rust_integration_max_parallel"], "8")
        self.assertEqual(payload["release_max_parallel"], "1")
        planned_lane_ids = [lane["lane_id"] for lane in payload["planned_matrix"]["include"]]
        selected_lane_ids = payload["selected_lane_ids"]
        self.assertEqual(
            planned_lane_ids[:5],
            [
                "core-compile-smoke",
                "core-carry-core-smoke",
                "core-carry-ui-smoke",
                "core-ledger-smoke",
                "core-runtime-surface-smoke",
            ],
        )
        self.assertEqual(planned_lane_ids[5:], selected_lane_ids)
        self.assertIn("codex.core-startup-sync-targeted", selected_lane_ids)
        self.assertIn("codex.downstream-docs-check", selected_lane_ids)
        self.assertNotIn("codex.nextest-archive-core-carry-pilot", selected_lane_ids)

    def test_heavy_plan_ci_heavy_pr_uses_frontier_harvest_policy(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "heavy",
            "--event-name",
            "pull_request",
            "--requested-lane",
            "",
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

        self.assertEqual(payload["matrix_fail_fast"], "false")
        self.assertEqual(payload["continue_after_smoke_failure"], "true")
        self.assertEqual(payload["eager_release_lanes"], "true")
        self.assertEqual(payload["rust_integration_max_parallel"], "8")
        self.assertEqual(payload["release_max_parallel"], "1")
        self.assertNotIn(
            "codex.nextest-archive-core-carry-pilot",
            payload["selected_lane_ids"],
        )

    def test_nextest_archive_pilot_is_explicit_only(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "heavy",
            "--event-name",
            "workflow_dispatch",
            "--requested-lane",
            "codex.nextest-archive-core-carry-pilot",
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
        self.assertEqual(payload["selected_lane_ids"], ["codex.nextest-archive-core-carry-pilot"])
        self.assertEqual(payload["matrix_fail_fast"], "true")
        self.assertEqual(payload["eager_release_lanes"], "false")

    def test_sedna_heavy_manual_harvest_jobs_follow_metadata_fail_fast(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml")
        jobs = payload.get("jobs") or {}

        metadata_outputs = (jobs.get("metadata") or {}).get("outputs") or {}
        self.assertEqual(metadata_outputs.get("display_ref"), "${{ steps.meta.outputs.display_ref }}")
        self.assertEqual(metadata_outputs.get("checkout_sha"), "${{ steps.meta.outputs.checkout_sha }}")
        self.assertEqual(
            metadata_outputs.get("planned_matrix"),
            "${{ steps.meta.outputs.planned_matrix }}",
        )
        self.assertEqual(
            metadata_outputs.get("selected_lane_ids"),
            "${{ steps.meta.outputs.selected_lane_ids }}",
        )
        self.assertEqual(
            metadata_outputs.get("eager_release_lanes"),
            "${{ steps.meta.outputs.eager_release_lanes }}",
        )
        self.assertEqual(
            ((jobs.get("smoke_rust_integration_lanes") or {}).get("strategy") or {}).get(
                "fail-fast"
            ),
            "${{ fromJson(needs.metadata.outputs.matrix_fail_fast) }}",
        )
        self.assertEqual(
            ((jobs.get("rust_integration_lanes") or {}).get("strategy") or {}).get(
                "fail-fast"
            ),
            "${{ fromJson(needs.metadata.outputs.matrix_fail_fast) }}",
        )
        rust_if = (jobs.get("rust_integration_lanes") or {}).get("if") or ""
        self.assertIn("needs.metadata.outputs.continue_after_smoke_failure == 'true'", rust_if)
        release_eager = jobs.get("release_lanes_eager") or {}
        self.assertEqual(release_eager.get("needs"), ["metadata"])
        self.assertIn(
            "needs.metadata.outputs.eager_release_lanes == 'true'",
            release_eager.get("if") or "",
        )
        release_if = (jobs.get("release_lanes") or {}).get("if") or ""
        self.assertIn("needs.metadata.outputs.eager_release_lanes != 'true'", release_if)

    def test_sedna_heavy_pr_triggers_keep_ready_for_review(self) -> None:
        trigger_types = parse_pull_request_types(
            REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml"
        )
        self.assertEqual(
            trigger_types,
            ["opened", "synchronize", "reopened", "ready_for_review", "labeled"],
        )

    def test_sedna_heavy_metadata_skips_draft_pr_churn_without_ci_heavy(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml")
        metadata_if = ((payload.get("jobs") or {}).get("metadata") or {}).get("if") or ""

        self.assertIn("github.event.pull_request.draft == false", metadata_if)
        self.assertIn("github.event.label.name == 'ci:heavy'", metadata_if)

    def test_sedna_heavy_workflow_dispatch_concurrency_keys_on_lane(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml")
        concurrency = payload.get("concurrency") or {}
        group = concurrency.get("group") or ""

        self.assertIn("inputs.lane || 'all'", group)
        self.assertIn("github.event.pull_request.number", group)

    def test_sedna_heavy_metadata_exposes_planner_fingerprint_and_dedupe_reason(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml")
        metadata_outputs = (((payload.get("jobs") or {}).get("metadata") or {}).get("outputs") or {})

        self.assertEqual(
            metadata_outputs.get("planner_fingerprint"),
            "${{ steps.meta.outputs.planner_fingerprint }}",
        )
        self.assertEqual(
            metadata_outputs.get("dedupe_reason"),
            "${{ steps.meta.outputs.dedupe_reason }}",
        )
    def test_sedna_heavy_summary_job_aggregates_lane_artifacts(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml")
        jobs = payload.get("jobs") or {}
        summary = jobs.get("summary") or {}

        self.assertEqual(
            summary.get("needs"),
            [
                "metadata",
                "smoke_workflow_lanes",
                "smoke_node_lanes",
                "smoke_rust_minimal_lanes",
                "smoke_rust_integration_lanes",
                "smoke_release_lanes",
                "workflow_lanes",
                "node_lanes",
                "rust_minimal_lanes",
                "rust_minimal_batches",
                "rust_integration_lanes",
                "rust_integration_batches",
                "release_lanes_eager",
                "release_lanes",
            ],
        )
        summary_if = summary.get("if") or ""
        self.assertIn("always()", summary_if)
        self.assertIn("needs.metadata.result == 'success'", summary_if)
        self.assertEqual(summary.get("runs-on"), "ubuntu-24.04")

        steps = summary.get("steps") or []
        self.assertEqual((summary.get("permissions") or {}).get("actions"), "read")
        self.assertEqual((steps[0] or {}).get("uses"), "actions/checkout@v6")
        self.assertEqual((steps[1] or {}).get("uses"), "actions/download-artifact@v8")
        self.assertEqual((steps[2] or {}).get("name"), "Record Actions cache occupancy")
        self.assertIn(
            "aggregate_validation_summary.py",
            (steps[3] or {}).get("run") or "",
        )
        self.assertIn(
            '--planned-matrix-json \'${{ needs.metadata.outputs.planned_matrix }}\'',
            (steps[3] or {}).get("run") or "",
        )
        self.assertIn(
            "--cache-occupancy-json",
            (steps[3] or {}).get("run") or "",
        )
        self.assertIn(
            '--head-sha "${{ needs.metadata.outputs.checkout_sha }}"',
            (steps[3] or {}).get("run") or "",
        )
        self.assertIn(
            '--workflow-result "${WORKFLOW_RESULT}"',
            (steps[3] or {}).get("run") or "",
        )
        self.assertIn(
            '--rust-minimal-result "${rust_minimal_result}"',
            (steps[3] or {}).get("run") or "",
        )
        self.assertIn(
            '--rust-integration-result "${rust_integration_result}"',
            (steps[3] or {}).get("run") or "",
        )
        self.assertEqual((steps[4] or {}).get("uses"), "actions/upload-artifact@v7")

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
        extra_args: list[str] | None = None,
    ) -> dict:
        head_sha = self.repo.commit("head", head_files)
        args = [
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
        ]
        if extra_args:
            args.extend(extra_args)
        return run_script(SCRIPTS_DIR / "resolve_rust_ci_mode.py", *args)

    def test_rust_ci_changed_job_uses_pr_metadata_fast_path_with_git_fallback(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci.yml")
        changed = ((payload.get("jobs") or {}).get("changed") or {})
        steps = changed.get("steps") or []
        checkout = next(
            step
            for step in steps
            if step.get("uses") == "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd"
        )
        self.assertEqual((checkout.get("with") or {}).get("fetch-depth"), "1")

        metadata_step = next(
            step for step in steps if step.get("name") == "Resolve PR changed files via API"
        )
        self.assertEqual(metadata_step.get("uses"), "actions/github-script@v9")
        metadata_script = ((metadata_step.get("with") or {}).get("script") or "")
        self.assertIn("github.paginate(github.rest.pulls.listFiles", metadata_script)
        self.assertIn("github.rest.repos.compareCommitsWithBasehead", metadata_script)

        fallback_step = next(
            step for step in steps if step.get("name") == "Fetch history for git diff fallback"
        )
        self.assertIn(
            "steps.pr_diff.outputs.needs_git_fallback == 'true'",
            fallback_step.get("if") or "",
        )

        detect_step = next(
            step for step in steps if step.get("name") == "Detect changed paths and rust-ci mode"
        )
        detect_run = detect_step.get("run") or ""
        self.assertIn("--primary-files-json", detect_run)
        self.assertIn("--primary-line-count", detect_run)
        self.assertIn("--latest-delta-files-json", detect_run)
        self.assertIn("--latest-delta-line-count", detect_run)

    def test_explicit_primary_diff_inputs_route_without_git_history(self) -> None:
        outputs = run_script(
            SCRIPTS_DIR / "resolve_rust_ci_mode.py",
            "--repo-root",
            str(self.repo.root),
            "--event-name",
            "pull_request",
            "--event-action",
            "opened",
            "--base-sha",
            "0" * 40,
            "--head-sha",
            "1" * 40,
            "--primary-files-json",
            json.dumps(["codex-rs/protocol/src/openai_models.rs"]),
            "--primary-line-count",
            "2",
        )

        self.assertEqual(outputs["validation_mode"], "light_initial")
        self.assertEqual(outputs["run_incremental_validation"], "true")
        self.assertEqual(
            outputs["incremental_lanes"],
            ",".join(
                [
                    "codex.spawn-agent-tool-model-surface-targeted",
                    "codex.spawn-agent-description-model-surface-targeted",
                ]
            ),
        )

    def test_explicit_latest_delta_inputs_route_green_followup_without_git_history(self) -> None:
        outputs = run_script(
            SCRIPTS_DIR / "resolve_rust_ci_mode.py",
            "--repo-root",
            str(self.repo.root),
            "--event-name",
            "pull_request",
            "--event-action",
            "synchronize",
            "--base-sha",
            "0" * 40,
            "--head-sha",
            "1" * 40,
            "--before-sha",
            "2" * 40,
            "--previous-green-required",
            "true",
            "--primary-files-json",
            json.dumps(["codex-rs/tools/src/agent_tool.rs"]),
            "--primary-line-count",
            "20",
            "--latest-delta-files-json",
            json.dumps(["codex-rs/tools/src/agent_tool.rs"]),
            "--latest-delta-line-count",
            "1",
        )

        self.assertEqual(outputs["validation_mode"], "light_followup")
        self.assertEqual(outputs["run_incremental_validation"], "true")
        self.assertEqual(
            outputs["incremental_lanes"],
            ",".join(
                [
                    "codex.spawn-agent-tool-model-surface-targeted",
                    "codex.core-subagent-spawn-approval-targeted",
                ]
            ),
        )

    def test_explicit_workflow_catalog_diff_stays_on_workflow_lanes(self) -> None:
        outputs = run_script(
            SCRIPTS_DIR / "resolve_rust_ci_mode.py",
            "--repo-root",
            str(self.repo.root),
            "--event-name",
            "pull_request",
            "--event-action",
            "opened",
            "--base-sha",
            "0" * 40,
            "--head-sha",
            "1" * 40,
            "--primary-files-json",
            json.dumps(
                [
                    ".github/workflows/_validation-lane-rust-minimal.yml",
                    ".github/workflows/validation-lab.yml",
                    ".github/validation-lanes.json",
                    ".github/scripts/test_ci_planners.py",
                ]
            ),
            "--primary-line-count",
            "40",
        )

        self.assertEqual(outputs["validation_mode"], "light_initial")
        self.assertEqual(outputs["workflows"], "true")
        self.assertEqual(outputs["run_general"], "false")
        self.assertEqual(outputs["run_cargo_shear"], "false")
        self.assertEqual(
            outputs["incremental_lanes"],
            ",".join(
                [
                    "codex.workflow-ci-sanity",
                    "codex.downstream-docs-check",
                ]
            ),
        )

    def test_explicit_large_primary_diff_does_not_enter_light_route(self) -> None:
        outputs = run_script(
            SCRIPTS_DIR / "resolve_rust_ci_mode.py",
            "--repo-root",
            str(self.repo.root),
            "--event-name",
            "pull_request",
            "--event-action",
            "opened",
            "--base-sha",
            "0" * 40,
            "--head-sha",
            "1" * 40,
            "--primary-files-json",
            json.dumps(["codex-rs/core/src/review_prompts.rs"]),
            "--primary-line-count",
            "401",
        )

        self.assertEqual(outputs["validation_mode"], "full")
        self.assertEqual(outputs["run_incremental_validation"], "false")

    def test_explicit_changed_files_rejects_malformed_json_cleanly(self) -> None:
        proc = subprocess.run(
            [
                "python3",
                str(SCRIPTS_DIR / "resolve_rust_ci_mode.py"),
                "--repo-root",
                str(self.repo.root),
                "--event-name",
                "pull_request",
                "--event-action",
                "opened",
                "--base-sha",
                "0" * 40,
                "--head-sha",
                "1" * 40,
                "--primary-files-json",
                "not-json",
            ],
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("invalid JSON input for changed-files", proc.stderr)
        self.assertNotIn("Traceback", proc.stderr)

    def test_light_initial_routes_small_openai_models_pr_to_exact_lane(self) -> None:
        outputs = self.run_rust_ci_mode(
            event_action="opened",
            head_files={"codex-rs/protocol/src/openai_models.rs": "first\nsecond\n"},
        )

        self.assertEqual(outputs["validation_mode"], "light_initial")
        self.assertEqual(outputs["run_incremental_validation"], "true")
        self.assertEqual(
            outputs["incremental_lanes"],
            ",".join(
                [
                    "codex.spawn-agent-tool-model-surface-targeted",
                    "codex.spawn-agent-description-model-surface-targeted",
                ]
            ),
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
                    "codex.spawn-agent-tool-model-surface-targeted",
                    "codex.core-subagent-spawn-approval-targeted",
                ]
            ),
        )
        self.assertEqual(outputs["run_argument_comment_lint_prebuilt"], "false")

    def test_light_followup_accepts_small_workflow_catalog_delta_after_green_head(self) -> None:
        green_sha = self.repo.commit(
            "green",
            {".github/workflows/validation-lab.yml": "base\n"},
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
                "workflow-followup",
                {
                    ".github/workflows/_validation-lane-rust-minimal.yml": "one\n",
                    ".github/workflows/validation-lab.yml": "two\n",
                    ".github/validation-lanes.json": "three\n",
                    ".github/scripts/test_ci_planners.py": "four\n",
                },
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
                    "codex.workflow-ci-sanity",
                    "codex.downstream-docs-check",
                ]
            ),
        )

    def test_workflow_only_pr_skips_rust_compile_gates(self) -> None:
        outputs = self.run_rust_ci_mode(
            event_action="opened",
            head_files={
                ".github/workflows/rust-ci.yml": "workflow\n",
                ".github/scripts/resolve_rust_ci_mode.py": "planner\n",
                "justfile": "validation:\n",
            },
        )

        self.assertEqual(outputs["validation_mode"], "light_initial")
        self.assertEqual(outputs["workflows"], "true")
        self.assertEqual(outputs["has_relevant_changes"], "true")
        self.assertEqual(outputs["run_general"], "false")
        self.assertEqual(outputs["run_cargo_shear"], "false")
        self.assertEqual(outputs["run_argument_comment_lint_prebuilt"], "false")
        self.assertEqual(outputs["run_argument_comment_lint_package"], "false")
        self.assertEqual(outputs["run_incremental_validation"], "true")
        self.assertEqual(
            outputs["incremental_lanes"],
            ",".join(
                [
                    "codex.workflow-ci-sanity",
                    "codex.downstream-docs-check",
                ]
            ),
        )

    def test_skill_only_pr_is_irrelevant_to_rust_ci(self) -> None:
        outputs = self.run_rust_ci_mode(
            event_action="opened",
            head_files={".codex/skills/example/SKILL.md": "hello\n"},
        )

        self.assertEqual(outputs["has_relevant_changes"], "false")
        self.assertEqual(outputs["run_general"], "false")
        self.assertEqual(outputs["run_cargo_shear"], "false")
        self.assertEqual(outputs["run_argument_comment_lint_package"], "false")
        self.assertEqual(outputs["run_argument_comment_lint_prebuilt"], "false")

    def test_non_rust_codex_rs_asset_pr_is_irrelevant_to_rust_ci(self) -> None:
        outputs = self.run_rust_ci_mode(
            event_action="opened",
            head_files={
                "codex-rs/skills/src/assets/samples/skill-creator/scripts/init_skill.py": "print('hi')\n",
            },
        )

        self.assertEqual(outputs["has_relevant_changes"], "false")
        self.assertEqual(outputs["codex"], "false")
        self.assertEqual(outputs["argument_comment_lint"], "false")
        self.assertEqual(outputs["run_general"], "false")
        self.assertEqual(outputs["run_cargo_shear"], "false")
        self.assertEqual(outputs["run_argument_comment_lint_package"], "false")
        self.assertEqual(outputs["run_argument_comment_lint_prebuilt"], "false")

    def test_rust_build_script_pr_still_triggers_rust_ci(self) -> None:
        outputs = self.run_rust_ci_mode(
            event_action="opened",
            head_files={"codex-rs/cli/build.rs": "fn main() {}\n"},
        )

        self.assertEqual(outputs["has_relevant_changes"], "true")
        self.assertEqual(outputs["codex"], "true")
        self.assertEqual(outputs["argument_comment_lint"], "true")
        self.assertEqual(outputs["run_general"], "true")
        self.assertEqual(outputs["run_cargo_shear"], "true")
        self.assertEqual(outputs["run_argument_comment_lint_prebuilt"], "true")

    def test_review_prompts_pr_routes_to_custom_prompt_targeted_validation(self) -> None:
        outputs = self.run_rust_ci_mode(
            event_action="opened",
            head_files={"codex-rs/core/src/review_prompts.rs": "fn review_prompt() {}\n"},
        )

        self.assertEqual(outputs["validation_mode"], "light_initial")
        self.assertEqual(outputs["codex"], "true")
        self.assertEqual(outputs["run_general"], "false")
        self.assertEqual(outputs["run_cargo_shear"], "false")
        self.assertEqual(outputs["run_incremental_validation"], "true")
        self.assertEqual(outputs["incremental_lanes"], "codex.custom-prompts-targeted")


class HelperScriptTests(unittest.TestCase):
    def test_repository_workflows_follow_static_policy(self) -> None:
        self.assertEqual(CHECK_WORKFLOW_POLICY.collect_violations(REPO_ROOT), [])

    def test_workflow_policy_rejects_missing_node_version_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/ci.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: ci
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-node@v6
        with:
          node-version-file: codex-rs/node-version.txt
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(
            violations,
            [
                ".github/workflows/ci.yml: actions/setup-node references missing "
                "node-version-file 'codex-rs/node-version.txt'; use node-version "
                "when the version is repository policy."
            ],
        )

    def test_workflow_policy_rejects_install_action_version_input(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/ci.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: ci
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: taiki-e/install-action@v2
        with:
          tool: nextest
          version: 0.9.103
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(
            violations,
            [
                ".github/workflows/ci.yml: taiki-e/install-action does not support "
                "with.version; use tool: nextest@0.9.103 instead."
            ],
        )

    def test_configure_sccache_restore_only_uses_read_only_fallback(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            github_output = root / "github-output"
            github_env = root / "github-env"
            workspace = root / "workspace"
            workspace.mkdir()

            subprocess.run(
                [
                    "bash",
                    str(SCRIPTS_DIR / "configure_sccache_backend.sh"),
                    "restore-only",
                ],
                check=True,
                env={
                    **os.environ,
                    "GITHUB_OUTPUT": str(github_output),
                    "GITHUB_ENV": str(github_env),
                    "GITHUB_WORKSPACE": str(workspace),
                },
            )

            output = github_output.read_text(encoding="utf-8")
            env = github_env.read_text(encoding="utf-8")

        self.assertIn("policy=restore-only", output)
        self.assertIn("backend=fallback", output)
        self.assertIn("SCCACHE_GHA_ENABLED=false", env)
        self.assertIn(f"SCCACHE_DIR={workspace}/.sccache", env)
        self.assertNotIn("SCCACHE_GHA_ENABLED=true", env)

    def test_actions_cache_occupancy_summary_groups_refs_and_prefixes(self) -> None:
        summary = REPORT_ACTIONS_CACHE_OCCUPANCY.summarize_caches(
            [
                {
                    "key": "sccache/a/b/c",
                    "ref": "refs/pull/164/merge",
                    "size_in_bytes": 1024,
                },
                {
                    "key": "cargo-home-linux-rust-hash",
                    "ref": "refs/heads/main",
                    "size_in_bytes": 2048,
                },
                {
                    "key": "sccache/d/e/f",
                    "ref": "refs/pull/164/merge",
                    "size_in_bytes": 4096,
                },
            ]
        )

        self.assertEqual(summary["total_entries"], 3)
        self.assertEqual(summary["total_size_bytes"], 7168)
        self.assertEqual(
            summary["by_prefix"][0],
            {"name": "sccache", "entries": 2, "size_bytes": 5120},
        )
        self.assertEqual(summary["by_ref"][0]["name"], "refs/pull/164/merge")
        self.assertEqual(summary["by_ref"][0]["entries"], 2)

    def test_build_results_tolerates_selected_lane_missing_from_matrix(self) -> None:
        results = AGGREGATE_VALIDATION_SUMMARY.build_results(
            planned_matrix=[],
            selected_lane_ids=["lane.only.in.selection"],
            actual_by_lane={},
            smoke_gate_result="skipped",
            setup_class_results={},
            matrix_fail_fast=False,
        )

        self.assertEqual(len(results), 1)
        self.assertEqual(results[0]["lane_id"], "lane.only.in.selection")
        self.assertEqual(results[0]["outcome"], "missing")
        self.assertEqual(results[0]["summary_family"], "lane.only.in.selection")

    def test_markdown_link_regex_excludes_optional_title(self) -> None:
        match = CHECK_MARKDOWN_LINKS.INLINE_LINK_RE.search(
            '[Spec](docs/example.md "Optional title")'
        )
        self.assertIsNotNone(match)
        self.assertEqual(match.group(1), "docs/example.md")

    def test_resolve_target_treats_root_relative_paths_as_repo_relative(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            docs_dir = root / "docs"
            docs_dir.mkdir(parents=True, exist_ok=True)
            source = docs_dir / "guide.md"
            source.write_text("guide\n", encoding="utf-8")
            readme = root / "README.md"
            readme.write_text("root\n", encoding="utf-8")

            original_root = CHECK_MARKDOWN_LINKS.ROOT
            CHECK_MARKDOWN_LINKS.ROOT = root
            try:
                resolved = CHECK_MARKDOWN_LINKS.resolve_target(source, "/README.md")
            finally:
                CHECK_MARKDOWN_LINKS.ROOT = original_root

        self.assertEqual(resolved, readme.resolve())


if __name__ == "__main__":
    unittest.main()
