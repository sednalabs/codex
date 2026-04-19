#!/usr/bin/env python3
"""Fixture tests for CI planner scripts and follow-up route selection."""

from __future__ import annotations

import importlib.util
import json
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
CHECK_MARKDOWN_LINKS = load_module(
    "check_markdown_links_module", SCRIPTS_DIR / "check_markdown_links.py"
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

    def test_review_prompt_core_path_stays_out_of_custom_prompt_shortcut(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/core/src/review_prompts.rs"],
            self.routes,
        )
        self.assertEqual(lanes, [])

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

    def test_workflow_ci_sanity_lane_uses_host_checkout_justfile(self) -> None:
        lane = next(
            lane
            for lane in self.catalog["lanes"]
            if lane["lane_id"] == "codex.workflow-ci-sanity"
        )
        self.assertEqual(
            lane["run_command"],
            "just --justfile ../.workflow-src/justfile workflow-ci-sanity",
        )


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
        self.assertEqual(payload["selected_light_lane_count"], 0)
        self.assertGreater(payload["selected_rust_lane_count"], 0)
        self.assertGreater(payload["selected_heavy_lane_count"], 0)

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
        self.assertEqual(payload["light_max_parallel"], "8")
        self.assertEqual(payload["rust_max_parallel"], "4")
        self.assertEqual(payload["heavy_max_parallel"], "2")

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
        self.assertEqual(payload["smoke_heavy_lane_count"], 0)
        self.assertEqual(
            [lane["lane_id"] for lane in payload["selected_matrix"]["include"]],
            ["codex.tui-agent-picker-model-surface-targeted"],
        )

    def test_heavy_plan_route_keeps_workflow_ci_changes_on_light_lanes(self) -> None:
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
        self.assertEqual(payload["selected_light_lane_count"], 2)
        self.assertEqual(payload["selected_rust_lane_count"], 0)
        self.assertEqual(payload["selected_heavy_lane_count"], 0)
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

        self.assertEqual((jobs.get("light_lanes") or {}).get("needs"), ["metadata"])
        self.assertEqual((jobs.get("rust_lanes") or {}).get("needs"), ["metadata"])
        self.assertEqual((jobs.get("heavy_lanes") or {}).get("needs"), ["metadata"])

    def test_heavy_plan_route_keeps_workflow_ci_changes_on_light_lanes(self) -> None:
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
        self.assertEqual(payload["selected_light_lane_count"], 2)
        self.assertEqual(payload["selected_rust_lane_count"], 0)
        self.assertEqual(payload["selected_heavy_lane_count"], 0)
        self.assertEqual(
            [lane["lane_id"] for lane in payload["selected_matrix"]["include"]],
            [
                "codex.workflow-ci-sanity",
                "codex.downstream-docs-check",
            ],
        )

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
        self.assertIn("codex.release-linux-build-smoke", selected_lane_ids)
        self.assertIn("codex.tui-config-refresh-session-targeted", selected_lane_ids)
        self.assertIn("codex.spawn-agent-description-model-surface-targeted", selected_lane_ids)
        self.assertNotIn("codex.tui-agent-picker-model-surface-targeted", selected_lane_ids)
        self.assertEqual(payload["selected_light_lane_count"], 5)
        self.assertEqual(payload["selected_rust_lane_count"], 22)
        self.assertEqual(payload["selected_heavy_lane_count"], 7)
        self.assertEqual(payload["rust_max_parallel"], "22")
        self.assertEqual(payload["heavy_max_parallel"], "7")

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
        self.assertIn("downstream-ledger-seam", selected_lane_ids)
        self.assertEqual(payload["selected_light_lane_count"], 5)
        self.assertEqual(payload["selected_rust_lane_count"], 24)
        self.assertEqual(payload["selected_heavy_lane_count"], 7)
        self.assertEqual(payload["rust_max_parallel"], "24")
        self.assertEqual(payload["heavy_max_parallel"], "7")

    def test_validation_lab_frontier_all_excludes_smoke_gate_lanes_by_metadata(self) -> None:
        catalog = {
            "lanes": [
                {
                    "lane_id": "codex.synthetic-runtime-gate",
                    "run_command": "echo synthetic-gate",
                    "groups": ["core"],
                    "status_class": "active",
                    "setup_class": "heavy",
                    "frontier_role": "sentinel",
                    "summary_family": "synthetic-gate",
                    "cost_class": "high",
                    "smoke_gate_only": True,
                    "smoke_gate_kinds": ["runtime"],
                },
                {
                    "lane_id": "codex.synthetic-real-lane",
                    "run_command": "echo synthetic-real-lane",
                    "groups": ["core"],
                    "status_class": "active",
                    "setup_class": "rust",
                    "frontier_role": "sentinel",
                    "summary_family": "synthetic-real-lane",
                    "cost_class": "medium",
                },
            ]
        }

        selected = RESOLVE_VALIDATION_PLAN.select_frontier_all(catalog)

        self.assertEqual(
            [lane["lane_id"] for lane in selected],
            ["codex.synthetic-real-lane"],
        )

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
        self.assertEqual(payload["light_max_parallel"], "5")
        self.assertEqual(payload["rust_max_parallel"], "24")
        self.assertEqual(payload["heavy_max_parallel"], "12")
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
            ((jobs.get("smoke_light_lanes") or {}).get("strategy") or {}).get("fail-fast"),
            "${{ fromJson(needs.metadata.outputs.matrix_fail_fast) }}",
        )
        self.assertEqual(
            ((jobs.get("rust_lanes") or {}).get("strategy") or {}).get("fail-fast"),
            "${{ fromJson(needs.metadata.outputs.matrix_fail_fast) }}",
        )
        rust_if = (jobs.get("rust_lanes") or {}).get("if") or ""
        self.assertIn("needs.metadata.outputs.continue_after_smoke_failure == 'true'", rust_if)

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
                "smoke_light_lanes",
                "smoke_rust_lanes",
                "smoke_heavy_lanes",
                "light_lanes",
                "rust_lanes",
                "heavy_lanes",
            ],
        )
        summary_if = summary.get("if") or ""
        self.assertIn("always()", summary_if)
        self.assertIn("needs.metadata.result == 'success'", summary_if)
        self.assertEqual(summary.get("runs-on"), "ubuntu-24.04")

        steps = summary.get("steps") or []
        self.assertEqual((steps[0] or {}).get("uses"), "actions/checkout@v6")
        self.assertEqual((steps[1] or {}).get("uses"), "actions/download-artifact@v8")
        self.assertIn(
            "aggregate_validation_summary.py",
            (steps[2] or {}).get("run") or "",
        )
        self.assertIn(
            '--planned-matrix-json \'${{ needs.metadata.outputs.planned_matrix }}\'',
            (steps[2] or {}).get("run") or "",
        )
        self.assertIn(
            '--head-sha "${{ needs.metadata.outputs.checkout_sha }}"',
            (steps[2] or {}).get("run") or "",
        )
        self.assertEqual((steps[3] or {}).get("uses"), "actions/upload-artifact@v7")

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

    def test_review_prompts_pr_falls_back_to_full_validation(self) -> None:
        outputs = self.run_rust_ci_mode(
            event_action="opened",
            head_files={"codex-rs/core/src/review_prompts.rs": "fn review_prompt() {}\n"},
        )

        self.assertEqual(outputs["validation_mode"], "full")
        self.assertEqual(outputs["codex"], "true")
        self.assertEqual(outputs["run_general"], "true")
        self.assertEqual(outputs["run_cargo_shear"], "true")
        self.assertEqual(outputs["run_incremental_validation"], "false")
        self.assertEqual(outputs["incremental_lanes"], "")


class HelperScriptTests(unittest.TestCase):
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
