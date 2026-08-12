#!/usr/bin/env python3
"""Fixture tests for CI planner scripts and follow-up route selection."""

from __future__ import annotations

import contextlib
import io
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
import unittest.mock as mock
from pathlib import Path

import yaml


SCRIPTS_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPTS_DIR.parent.parent


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"unable to load module from {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


RESOLVE_VALIDATION_PLAN = load_module(
    "resolve_validation_plan_module", SCRIPTS_DIR / "resolve_validation_plan.py"
)
RESOLVE_RUST_CI_MODE = load_module(
    "resolve_rust_ci_mode_module", SCRIPTS_DIR / "resolve_rust_ci_mode.py"
)
RESOLVE_BAZEL_CI_MODE = load_module(
    "resolve_bazel_ci_mode_module", SCRIPTS_DIR / "resolve_bazel_ci_mode.py"
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
SKIP_DUPLICATE_WORKFLOW_RUN = load_module(
    "skip_duplicate_workflow_run_module", SCRIPTS_DIR / "skip_duplicate_workflow_run.py"
)
VALIDATION_PLAN_FINGERPRINT = load_module(
    "validation_plan_fingerprint_module", SCRIPTS_DIR / "validation_plan_fingerprint.py"
)
PREPARE_CODEQL_DIFF_RANGES = load_module(
    "prepare_codeql_diff_ranges_module", SCRIPTS_DIR / "prepare_codeql_diff_ranges.py"
)
SYNC_UPSTREAM_MIRROR = load_module(
    "sync_upstream_mirror_module", SCRIPTS_DIR / "sync_upstream_mirror.py"
)
DISPATCH_SEDNA_RELEASE = load_module(
    "dispatch_sedna_release_module", SCRIPTS_DIR / "dispatch_sedna_release.py"
)
RESOLVE_SEDNA_RELEASE_VERSION = load_module(
    "resolve_sedna_release_version_module",
    SCRIPTS_DIR / "resolve_sedna_release_version.py",
)
DOWNSTREAM_DIVERGENCE_AUDIT = load_module(
    "downstream_divergence_audit_module",
    REPO_ROOT / "scripts" / "downstream-divergence-audit.py",
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


def parse_github_output_file(output_path: Path) -> dict[str, str]:
    outputs: dict[str, str] = {}
    lines = output_path.read_text(encoding="utf-8").splitlines()
    index = 0
    while index < len(lines):
        line = lines[index]
        index += 1
        if not line:
            continue
        if "<<" in line:
            key, delimiter = line.split("<<", 1)
            if not key or not delimiter:
                continue
            value_lines: list[str] = []
            while index < len(lines) and lines[index] != delimiter:
                value_lines.append(lines[index])
                index += 1
            if index < len(lines):
                index += 1
            outputs[key] = "\n".join(value_lines)
            continue
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        if key:
            outputs[key] = value
    return outputs


def workflow_step_by_name(workflow_path: Path, job_name: str, step_name: str) -> dict:
    payload = load_workflow_payload(workflow_path)
    steps = (((payload.get("jobs") or {}).get(job_name) or {}).get("steps") or [])
    for step in steps:
        if step.get("name") == step_name:
            return step
    raise AssertionError(f"missing step {step_name!r} in {workflow_path}")


def run_workflow_step_script(
    script: str, event: dict, *, event_name: str = "push"
) -> tuple[subprocess.CompletedProcess, dict]:
    with tempfile.TemporaryDirectory() as tmpdir:
        root = Path(tmpdir)
        event_path = root / "event.json"
        output_path = root / "github-output.txt"
        event_path.write_text(json.dumps(event), encoding="utf-8")
        output_path.write_text("", encoding="utf-8")
        env = {
            **os.environ,
            "EVENT_AFTER": str(event.get("after") or ""),
            "GITHUB_EVENT_NAME": event_name,
            "GITHUB_EVENT_PATH": str(event_path),
            "GITHUB_OUTPUT": str(output_path),
            "GITHUB_SHA": str(event.get("after") or "abc123"),
            "EVENT_NAME": event_name,
            "EVENT_REF": str(event.get("ref") or ""),
            "EVENT_SHA": str(event.get("after") or "abc123"),
            "HEAD_MESSAGE": str((event.get("head_commit") or {}).get("message") or "")
            if isinstance(event.get("head_commit"), dict)
            else "",
        }
        proc = subprocess.run(
            ["bash", "-c", f"set -euo pipefail\n{script}"],
            check=False,
            capture_output=True,
            text=True,
            env=env,
        )
        return proc, parse_github_output_file(output_path)


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
        self._git("config", "commit.gpgSign", "false")
        self._git("config", "tag.gpgSign", "false")

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

    def _git(self, *args: str, env: dict[str, str] | None = None) -> str:
        git_env = os.environ.copy()
        if env is not None:
            git_env.update(env)
        proc = subprocess.run(
            [
                "git",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "tag.gpgSign=false",
                "-C",
                str(self.root),
                *args,
            ],
            check=True,
            capture_output=True,
            env=git_env,
            text=True,
        )
        return proc.stdout.strip()


class CodeqlDiffRangeTests(unittest.TestCase):
    def test_collect_uses_direct_tree_diff_for_shallow_pr_checkout(self) -> None:
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="+++ b/changed.py\n@@ -0,0 +1 @@\n+changed\n",
            stderr="",
        )
        with mock.patch.object(
            PREPARE_CODEQL_DIFF_RANGES.subprocess,
            "run",
            return_value=completed,
        ) as run:
            ranges = PREPARE_CODEQL_DIFF_RANGES.collect_diff_ranges("base", "head")

        self.assertEqual(
            ranges,
            [{"path": "changed.py", "startLine": 1, "endLine": 1}],
        )
        command = run.call_args.args[0]
        self.assertIn("base..head", command)
        self.assertNotIn("base...head", command)

    def test_parses_added_and_modified_ranges_and_merges_adjacent_hunks(self) -> None:
        diff = """\
diff --git a/alpha.py b/alpha.py
--- a/alpha.py
+++ b/alpha.py
@@ -1 +1,2 @@
+one
+two
@@ -4 +5 @@
+five
diff --git a/old.py b/new.py
similarity index 80%
rename from old.py
rename to new.py
--- a/old.py
+++ b/new.py
@@ -8,0 +9,3 @@
+nine
+ten
+eleven
"""

        self.assertEqual(
            PREPARE_CODEQL_DIFF_RANGES.parse_diff_ranges(diff),
            [
                {"path": "alpha.py", "startLine": 1, "endLine": 2},
                {"path": "alpha.py", "startLine": 5, "endLine": 5},
                {"path": "new.py", "startLine": 9, "endLine": 11},
            ],
        )

    def test_skips_deletions_and_decodes_quoted_git_paths(self) -> None:
        diff = """\
diff --git a/deleted.py b/deleted.py
--- a/deleted.py
+++ /dev/null
@@ -1 +0,0 @@
-deleted
diff --git \"a/dir name.py\" \"b/dir name.py\"
--- \"a/dir name.py\"
+++ \"b/dir name.py\"
@@ -3,0 +4 @@
+added
"""

        self.assertEqual(
            PREPARE_CODEQL_DIFF_RANGES.parse_diff_ranges(diff),
            [{"path": "dir name.py", "startLine": 4, "endLine": 4}],
        )

    def test_rejects_non_repository_new_path(self) -> None:
        with self.assertRaisesRegex(ValueError, "unexpected Git new-path header"):
            PREPARE_CODEQL_DIFF_RANGES.parse_diff_ranges(
                "+++ /tmp/outside.py\n@@ -0,0 +1 @@\n+bad\n"
            )


class SyncUpstreamMirrorTests(unittest.TestCase):
    def test_read_only_fallback_uses_stale_mirror_for_pr_audit(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            repo, _origin_bare, upstream_bare, old_sha, _new_sha = self.create_fixture(
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
                "audit_baseline": "origin-mirror",
                "expected_mirror_sha": old_sha,
                "mirror_audit_args": [
                    "--upstream-remote",
                    "origin",
                    "--upstream-branch",
                    "upstream-main",
                    "--mirror-remote",
                    "origin",
                    "--mirror-branch",
                    "upstream-main",
                ],
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


class DispatchSednaReleaseTests(unittest.TestCase):
    def test_refresh_upstream_rust_tags_fetches_only_rust_release_tags(self) -> None:
        with mock.patch.object(DISPATCH_SEDNA_RELEASE, "run_command") as run_command:
            DISPATCH_SEDNA_RELEASE.refresh_upstream_rust_tags(
                repo=Path("/repo"),
                upstream_remote="upstream",
                dry_run=False,
            )

        run_command.assert_called_once_with(
            [
                "git",
                "-C",
                "/repo",
                "fetch",
                "--no-tags",
                "upstream",
                "+refs/tags/rust-v*:refs/tags/rust-v*",
            ],
            dry_run=False,
        )

    def test_dispatch_release_uses_computed_release_metadata(self) -> None:
        args = mock.Mock(
            workflow="sedna-release.yml",
            repo_slug="sednalabs/codex",
            dispatch_ref="main",
            channel="prerelease",
            draft=False,
            repo=Path("/repo"),
            dry_run=True,
        )
        metadata = {
            "release_tag": "v0.133.0-sedna.1+upstream.31",
            "target_commit": "d4b356a4c23ff606556dac7232353c80d2ce8deb",
        }

        with mock.patch.object(DISPATCH_SEDNA_RELEASE, "run_command") as run_command:
            DISPATCH_SEDNA_RELEASE.dispatch_release(args, metadata)

        run_command.assert_called_once_with(
            [
                "gh",
                "workflow",
                "run",
                "sedna-release.yml",
                "--repo",
                "sednalabs/codex",
                "--ref",
                "main",
                "-f",
                "target_sha=d4b356a4c23ff606556dac7232353c80d2ce8deb",
                "-f",
                "channel=prerelease",
                "-f",
                "release_tag=v0.133.0-sedna.1+upstream.31",
                "-f",
                "draft=false",
            ],
            cwd=Path("/repo"),
            dry_run=True,
        )

    def test_main_refreshes_tags_before_resolving_release_metadata(self) -> None:
        events: list[str] = []
        metadata = {
            "release_tag": "v0.133.0-sedna.1+upstream.31",
            "target_commit": "d4b356a4c23ff606556dac7232353c80d2ce8deb",
        }

        def refresh_tags(**kwargs: object) -> None:
            self.assertIs(kwargs["dry_run"], False)
            events.append("refresh")

        def resolve_metadata(_args: object) -> dict[str, object]:
            self.assertEqual(events, ["refresh"])
            events.append("resolve")
            return metadata

        def dispatch(_args: object, dispatch_metadata: dict[str, object]) -> None:
            self.assertEqual(events, ["refresh", "resolve"])
            self.assertEqual(dispatch_metadata, metadata)
            events.append("dispatch")

        with tempfile.TemporaryDirectory() as tmpdir:
            with (
                mock.patch.object(
                    DISPATCH_SEDNA_RELEASE,
                    "refresh_upstream_rust_tags",
                    side_effect=refresh_tags,
                ),
                mock.patch.object(
                    DISPATCH_SEDNA_RELEASE,
                    "resolve_release_metadata",
                    side_effect=resolve_metadata,
                ),
                mock.patch.object(
                    DISPATCH_SEDNA_RELEASE,
                    "dispatch_release",
                    side_effect=dispatch,
                ),
            ):
                with contextlib.redirect_stdout(io.StringIO()):
                    result = DISPATCH_SEDNA_RELEASE.main(
                        [
                            "--repo",
                            tmpdir,
                            "--target-sha",
                            "d4b356a4c23ff606556dac7232353c80d2ce8deb",
                            "--github-releases",
                            "off",
                        ]
                    )

        self.assertEqual(result, 0)
        self.assertEqual(events, ["refresh", "resolve", "dispatch"])


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

    def test_picker_lifecycle_surface_routes_to_both_picker_lanes(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            [
                "codex-rs/tui/src/app/loaded_threads.rs",
                "codex-rs/tui/src/app/session_lifecycle.rs",
                "codex-rs/tui/src/app/tests/session_lifecycle_requests.rs",
            ],
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
            ],
        )

    def test_v2_residency_route_stays_on_multi_agent_orchestration_lane(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            [
                "codex-rs/core/src/agent/control.rs",
                "codex-rs/core/src/agent/control/residency.rs",
                "codex-rs/core/src/agent/control/residency_tests.rs",
                "codex-rs/core/src/agent/control/spawn.rs",
                "codex-rs/core/src/agent/control_tests.rs",
                "codex-rs/core/src/agent/registry.rs",
                "codex-rs/core/src/agent/registry_tests.rs",
                "codex-rs/core/src/thread_manager.rs",
                "codex-rs/core/tests/suite/agent_execution.rs",
                ".github/scripts/test_ci_planners.py",
                ".github/validation-lanes.json",
                "docs/carry-divergence-ledger.md",
                "docs/divergences/index.yaml",
                "docs/downstream-regression-matrix.md",
                "docs/downstream.md",
                "justfile",
            ],
            self.routes,
        )
        self.assertEqual(lanes, ["codex.core-multi-agent-orchestration-targeted"])

        recipe = "\n".join(
            just_recipe_bodies(REPO_ROOT / "justfile")[
                "core-multi-agent-orchestration-targeted"
            ]
        )
        self.assertEqual(recipe.count("cargo nextest run"), 4)
        self.assertEqual(recipe.count("RUST_MIN_STACK="), 4)
        self.assertEqual(recipe.count("--no-tests=fail"), 4)
        for test_name in (
            "agent::control::residency::tests::"
            "residency_slot_reservation_unloads_oldest_idle_v2_agent",
            "agent::control::residency::tests::"
            "interrupted_v2_agent_remains_known_and_reloads_after_residency_eviction",
            "agent::control::residency::tests::"
            "ephemeral_v2_agent_is_not_evicted_without_reloadable_history",
            "agent::registry::tests::cold_status_text_stays_compact_when_json_escaped",
            "agent::control::tests::ensure_v2_agent_loaded_reloads_registered_unloaded_agent",
            "suite::agent_execution::v2_evicted_completed_agent_keeps_final_status",
        ):
            self.assertIn(test_name, recipe)

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

    def test_workflow_ci_route_accepts_plan_fingerprint_helper_alone(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            [".github/scripts/validation_plan_fingerprint.py"],
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
                ".github/scripts/validation-lanes/downstream-divergence-audit.sh",
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

    def test_skill_loader_fixture_route_stays_exact(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/core-skills/src/loader_tests.rs"],
            self.routes,
        )
        self.assertEqual(
            lanes,
            ["codex.skill-loader-fixture-hermeticity-targeted"],
        )

    def test_skill_loader_fixture_lane_pins_both_hermeticity_tests(self) -> None:
        lane = next(
            lane
            for lane in self.catalog["lanes"]
            if lane["lane_id"] == "codex.skill-loader-fixture-hermeticity-targeted"
        )
        self.assertEqual(lane["setup_class"], "rust_minimal")
        self.assertEqual(
            lane["script_args"],
            ["skill-loader-fixture-hermeticity-targeted"],
        )
        self.assertTrue(lane["needs_nextest"])

        recipe = "\n".join(
            just_recipe_bodies(REPO_ROOT / "justfile")[
                "skill-loader-fixture-hermeticity-targeted"
            ]
        )
        self.assertIn("RUST_MIN_STACK=", recipe)
        self.assertIn("cargo nextest run -p codex-core-skills --lib", recipe)
        self.assertIn("--no-tests=fail", recipe)
        self.assertIn(
            "loader::tests::non_git_repo_skills_search_does_not_walk_parents",
            recipe,
        )
        self.assertIn(
            "loader::tests::skill_roots_include_admin_with_lowest_priority",
            recipe,
        )
        self.assertEqual(recipe.count("loader::tests::"), 2)
        self.assertIn("--exact", recipe)

    def test_downstream_docs_route_includes_registry_and_tracking_docs(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            [
                "docs/divergences/index.yaml",
                "docs/downstream-divergence-tracking.md",
            ],
            self.routes,
        )
        self.assertEqual(
            lanes,
            [
                "codex.downstream-docs-check",
                "codex.downstream-divergence-audit",
            ],
        )

    def test_downstream_docs_lane_is_pr_local_sanity(self) -> None:
        lane = next(
            lane
            for lane in self.catalog["lanes"]
            if lane["lane_id"] == "codex.downstream-docs-check"
        )
        self.assertEqual(
            lane["script_path"],
            ".github/scripts/validation-lanes/downstream-docs-check.sh",
        )
        self.assertEqual(lane.get("checkout_fetch_depth"), 1)
        self.assertFalse(lane["needs_just"])

    def test_downstream_divergence_audit_lane_is_explicit_global_audit(self) -> None:
        lane = next(
            lane
            for lane in self.catalog["lanes"]
            if lane["lane_id"] == "codex.downstream-divergence-audit"
        )
        self.assertTrue(lane["explicit_only"])
        self.assertTrue(lane["pilot_only"])
        self.assertEqual(
            lane["script_path"],
            ".github/scripts/validation-lanes/downstream-divergence-audit.sh",
        )
        self.assertEqual(lane.get("checkout_fetch_depth"), 0)
        self.assertFalse(lane["needs_just"])

    def test_nextest_archive_pilot_declares_archive_contract(self) -> None:
        lane = next(
            lane
            for lane in self.catalog["lanes"]
            if lane["lane_id"] == "codex.nextest-archive-core-carry-pilot"
        )
        archive = lane.get("nextest_archive") or {}

        self.assertTrue(lane["explicit_only"])
        self.assertTrue(lane["pilot_only"])
        self.assertEqual(lane["setup_class"], "rust_integration")
        self.assertEqual(archive.get("cohort"), "core-carry-pilot")
        self.assertEqual(
            archive.get("artifact_name"),
            "validation-lab-nextest-core-carry-pilot",
        )
        self.assertEqual(archive.get("archive_file_name"), "codex-core-carry-nextest.tar.zst")
        self.assertEqual(
            archive.get("build_script_path"),
            ".github/scripts/validation-lanes/build-nextest-archive-core-carry-pilot.sh",
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
                "codex.blocking-waits-app-server-targeted",
            ],
        )

    def test_native_computer_use_code_mode_route_covers_wrapper_and_provider_lanes(
        self,
    ) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            [
                ".github/scripts/test_ci_planners.py",
                ".github/validation-lanes.json",
                "codex-rs/code-mode-protocol/src/description.rs",
                "codex-rs/core/src/tools/handlers/computer_use.rs",
                "codex-rs/core/src/tools/handlers/computer_use_code_mode.rs",
                "codex-rs/core/src/tools/handlers/mod.rs",
                "codex-rs/core/tests/suite/code_mode.rs",
                "docs/carry-divergence-ledger.md",
                "docs/divergences/index.yaml",
                "docs/downstream-regression-matrix.md",
                "docs/downstream-tool-surface-matrix.md",
                "docs/native-computer-use.md",
                "justfile",
            ],
            self.routes,
        )
        self.assertEqual(
            lanes,
            [
                "codex.app-server-protocol-test",
                "codex.app-server-computer-use-targeted",
                "codex.tui-native-computer-use-targeted",
                "codex.exec-native-computer-use-targeted",
                "codex.native-computer-use-tool-registry-targeted",
                "codex.code-mode-declaration-targeted",
                "codex.native-computer-use-doctor-targeted",
            ],
        )

    def test_app_server_schema_fixture_route_stays_on_schema_contract_lane(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/app-server-protocol/schema/json/ServerNotification.json"],
            self.routes,
        )
        self.assertEqual(lanes, ["codex.app-server-protocol-test"])

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

    def test_custom_prompt_review_prompt_crate_path_stays_targeted(self) -> None:
        lanes = RESOLVE_VALIDATION_PLAN.select_followup_lanes(
            ["codex-rs/prompts/src/review_request.rs"],
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

    def test_coverage_workflow_does_not_use_code_quality_product(self) -> None:
        workflow_path = REPO_ROOT / ".github/workflows/code-coverage.yml"
        workflow_text = workflow_path.read_text(encoding="utf-8")
        payload = load_workflow_payload(workflow_path)

        self.assertEqual(payload.get("name"), "Coverage Tests")
        self.assertNotIn("actions/upload-code-coverage@", workflow_text)
        self.assertNotIn("code-quality:", workflow_text)
        self.assertEqual(
            set((payload.get("jobs") or {}).keys()),
            {"python-tools", "python-sdk", "typescript-sdk"},
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

    def test_argument_comment_lint_lane_uses_prebuilt_setup_contract(self) -> None:
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
        self.assertFalse(lane["needs_bazel"])
        self.assertTrue(lane["needs_linux_build_deps"])
        self.assertTrue(lane["needs_dotslash"])
        self.assertFalse(lane["needs_sccache"])
        self.assertEqual(lane["timeout_minutes"], 30)

    def test_bazel_macos_clippy_caps_hosted_runner_fanout(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/bazel.yml")
        clippy_steps = ((payload.get("jobs") or {}).get("clippy") or {}).get("steps") or []
        clippy_run = next(
            step
            for step in clippy_steps
            if step.get("name") == "bazel build --config=clippy lint targets"
        ).get("run") or ""
        self.assertIn('[[ "${RUNNER_OS}" == "macOS" ]]', clippy_run)
        self.assertIn("--jobs=96", clippy_run)
        self.assertIn("--loading_phase_threads=8", clippy_run)

    def test_bazel_windows_tests_serialize_host_global_policy_state(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/bazel.yml")
        windows_steps = (
            ((payload.get("jobs") or {}).get("test-windows-shard") or {}).get("steps")
            or []
        )
        windows_test_run = next(
            step for step in windows_steps if step.get("name") == "bazel test shard"
        ).get("run") or ""
        self.assertIn("--local_test_jobs=1", windows_test_run)

    def test_bazel_windows_native_main_avoids_remote_rust_cache(self) -> None:
        bazel = load_workflow_payload(REPO_ROOT / ".github/workflows/bazel.yml")
        self.assertNotIn("test-windows-native-main", bazel.get("jobs") or {})

        payload = load_workflow_payload(
            REPO_ROOT / ".github/workflows/native-windows-bazel-health.yml"
        )
        self.assertEqual(payload.get("name"), "Native Windows Bazel health")
        triggers = payload.get("on") or {}
        self.assertEqual((triggers.get("push") or {}).get("branches"), ["main"])
        dispatch_inputs = (triggers.get("workflow_dispatch") or {}).get("inputs") or {}
        self.assertEqual(
            (dispatch_inputs.get("collect_diagnostics") or {}).get("type"),
            "boolean",
        )
        self.assertEqual(
            payload.get("concurrency"),
            {
                "group": "native-windows-bazel-health-${{ github.ref }}",
                "cancel-in-progress": "false",
                "queue": "max",
            },
        )

        native_job = (payload.get("jobs") or {}).get("test-windows-native-main") or {}
        native_steps = native_job.get("steps") or []
        native_step = next(
            (step for step in native_steps if step.get("name") == "bazel test //..."),
            None,
        )
        self.assertIsNotNone(native_step, "Step 'bazel test //...' not found")
        self.assertNotIn("continue-on-error", native_step)
        native_test_run = native_step.get("run") or ""
        self.assertIn(
            "--modify_execution_info=Rustc=+no-remote-cache",
            native_test_run,
        )

        diagnostics_step = next(
            step
            for step in native_steps
            if step.get("name") == "Collect native Windows Bazel diagnostics"
        )
        self.assertIn("failure()", diagnostics_step.get("if") or "")
        self.assertIn("inputs.collect_diagnostics", diagnostics_step.get("if") or "")
        self.assertIn(
            "collect-native-windows-bazel-diagnostics.ps1",
            diagnostics_step.get("run") or "",
        )
        diagnostics_upload = next(
            step
            for step in native_steps
            if step.get("name") == "Upload native Windows Bazel diagnostics"
        )
        self.assertEqual((diagnostics_upload.get("with") or {}).get("retention-days"), "3")

    def test_bazel_ci_docs_only_plan_is_fail_closed_and_preserves_required_signal(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/bazel.yml")
        jobs = payload.get("jobs") or {}
        plan_job = jobs.get("plan") or {}
        self.assertEqual(
            plan_job.get("outputs"),
            {
                "mode": "${{ steps.resolve.outputs.mode }}",
                "run_bazel": "${{ steps.resolve.outputs.run_bazel }}",
            },
        )
        plan_steps = plan_job.get("steps") or []
        compare_step = next(
            step for step in plan_steps if step.get("name") == "Compare exact changed files"
        )
        self.assertEqual(compare_step.get("uses"), "actions/github-script@v9.0.0")
        self.assertEqual(
            compare_step.get("if"),
            "${{ github.event_name == 'pull_request' || github.event_name == 'merge_group' }}",
        )
        compare_script = (compare_step.get("with") or {}).get("script") or ""
        self.assertIn("github.rest.repos.compareCommitsWithBasehead", compare_script)
        self.assertIn("context.payload.pull_request?.base?.sha", compare_script)
        self.assertIn("context.payload.pull_request?.head?.sha", compare_script)
        self.assertIn("context.payload.merge_group?.base_sha", compare_script)
        self.assertIn("context.sha", compare_script)
        self.assertIn("data.files", compare_script)
        self.assertIn("files.length >= 300", compare_script)
        self.assertIn("file.previous_filename", compare_script)
        self.assertIn("comparison_complete", compare_script)

        resolve_step = next(
            step for step in plan_steps if step.get("name") == "Resolve Bazel CI mode"
        )
        self.assertIn(
            "resolve_bazel_ci_mode.py",
            resolve_step.get("run") or "",
        )
        self.assertIn("--github-output", resolve_step.get("run") or "")

        docs_job = jobs.get("docs-only") or {}
        self.assertEqual(docs_job.get("needs"), "plan")
        self.assertEqual(docs_job.get("if"), "${{ needs.plan.outputs.mode == 'docs_only' }}")
        docs_step = next(
            step
            for step in docs_job.get("steps") or []
            if step.get("name") == "Check markdown links"
        )
        self.assertEqual(docs_step.get("run"), "python3 .github/scripts/check_markdown_links.py")

        for job_name in ["test", "test-windows-shard", "clippy", "verify-release-build"]:
            with self.subTest(job=job_name):
                job = jobs.get(job_name) or {}
                self.assertEqual(job.get("needs"), "plan")
                self.assertEqual(job.get("if"), "${{ needs.plan.outputs.run_bazel == 'true' }}")

        windows_gate = jobs.get("test-windows") or {}
        self.assertEqual(windows_gate.get("needs"), ["plan", "test-windows-shard"])
        self.assertEqual(
            windows_gate.get("if"),
            "${{ always() && needs.plan.outputs.run_bazel == 'true' }}",
        )
        results_job = jobs.get("results") or {}
        self.assertEqual(results_job.get("name"), "Bazel required gate")
        self.assertEqual(
            results_job.get("needs"),
            [
                "plan",
                "docs-only",
                "test",
                "test-windows-shard",
                "test-windows",
                "clippy",
                "verify-release-build",
            ],
        )
        results_run = (
            next(
                step
                for step in results_job.get("steps") or []
                if step.get("name") == "Require the selected Bazel CI mode"
            ).get("run")
            or ""
        )
        self.assertIn('if mode == "docs_only"', results_run)
        self.assertIn('elif mode == "full"', results_run)
        self.assertIn('require("docs-only", "success")', results_run)
        self.assertIn('require(job, "skipped")', results_run)
        self.assertIn('require(job, "success")', results_run)

        heredoc_start = "python3 - <<'PY'\n"
        self.assertIn(heredoc_start, results_run)
        gate_script = results_run.split(heredoc_start, 1)[1].rsplit("\nPY", 1)[0]
        for mode, docs_result, normal_result in [
            ("docs_only", "success", "skipped"),
            ("full", "skipped", "success"),
        ]:
            with self.subTest(mode=mode):
                needs = {
                    "plan": {"result": "success", "outputs": {"mode": mode}},
                    "docs-only": {"result": docs_result},
                    "test": {"result": normal_result},
                    "test-windows-shard": {"result": normal_result},
                    "test-windows": {"result": normal_result},
                    "clippy": {"result": normal_result},
                    "verify-release-build": {"result": normal_result},
                }
                proc = subprocess.run(
                    ["python3", "-c", gate_script],
                    check=False,
                    capture_output=True,
                    text=True,
                    env={**os.environ, "NEEDS_JSON": json.dumps(needs)},
                )
                self.assertEqual(proc.returncode, 0, proc.stderr)

    def test_bazel_cache_writes_are_limited_to_trusted_post_merge_pushes(self) -> None:
        setup_action = load_workflow_payload(REPO_ROOT / ".github/actions/setup-bazel-ci/action.yml")
        setup_bazel_step = next(
            step
            for step in ((setup_action.get("runs") or {}).get("steps") or [])
            if step.get("name") == "Set up Bazel"
        )
        self.assertEqual(
            (setup_bazel_step.get("with") or {}).get("bazelisk-cache"),
            "true",
        )
        self.assertEqual(
            (setup_bazel_step.get("with") or {}).get("disk-cache"),
            "${{ github.workflow }}",
        )
        self.assertEqual(
            (setup_bazel_step.get("with") or {}).get("cache-save"),
            "${{ github.event_name == 'push' && (github.ref == 'refs/heads/main' || github.ref == 'refs/heads/upstream-main') }}",
        )

        prepare_action = load_workflow_payload(REPO_ROOT / ".github/actions/prepare-bazel-ci/action.yml")
        self.assertEqual(
            (prepare_action.get("outputs") or {}).get("repository-cache-write-enabled", {}).get(
                "value"
            ),
            "${{ steps.repository_cache_write_policy.outputs.repository-cache-write-enabled }}",
        )
        policy_step = next(
            step
            for step in ((prepare_action.get("runs") or {}).get("steps") or [])
            if step.get("name") == "Determine Bazel repository cache write eligibility"
        )
        policy_run = policy_step.get("run") or ""
        self.assertIn('"${EVENT_NAME}" == "push"', policy_run)
        self.assertIn("refs/heads/main", policy_run)
        self.assertIn("refs/heads/upstream-main", policy_run)

        cache_key_step = next(
            step
            for step in ((prepare_action.get("runs") or {}).get("steps") or [])
            if step.get("name") == "Compute bazel repository cache key"
        )
        self.assertIn(
            "bazel-cache-${CACHE_SCOPE}-${TARGET}-${CACHE_HASH}",
            cache_key_step.get("run") or "",
        )
        cache_summary_step = next(
            step
            for step in ((prepare_action.get("runs") or {}).get("steps") or [])
            if step.get("name") == "Summarize Bazel repository cache"
        )
        cache_summary_env = cache_summary_step.get("env") or {}
        self.assertEqual(
            cache_summary_env.get("CACHE_KEY"),
            "${{ steps.cache_bazel_repository_key.outputs.repository-cache-key }}",
        )
        self.assertEqual(
            cache_summary_env.get("CACHE_HIT"),
            "${{ steps.cache_bazel_repository_restore.outputs.cache-hit }}",
        )
        self.assertEqual(
            cache_summary_env.get("WRITE_ENABLED"),
            "${{ steps.repository_cache_write_policy.outputs.repository-cache-write-enabled }}",
        )
        self.assertEqual(
            cache_summary_env.get("SETUP_BAZEL_DISK_CACHE_SCOPE"),
            "${{ github.workflow }}",
        )
        cache_summary_run = cache_summary_step.get("run") or ""
        self.assertIn("setup-bazel Bazelisk download cache", cache_summary_run)
        self.assertIn("setup-bazel disk-cache scope", cache_summary_run)
        self.assertIn("Repository cache primary key", cache_summary_run)

        bazel = load_workflow_payload(REPO_ROOT / ".github/workflows/bazel.yml")
        cache_save_steps = [
            step
            for job in (bazel.get("jobs") or {}).values()
            for step in (job.get("steps") or [])
            if step.get("name") == "Save bazel repository cache"
        ]
        self.assertEqual(len(cache_save_steps), 3)
        for save_step in cache_save_steps:
            with self.subTest(cache_key=(save_step.get("with") or {}).get("key")):
                self.assertEqual(save_step.get("continue-on-error"), "true")
                self.assertIn(
                    "steps.prepare_bazel.outputs.repository-cache-write-enabled == 'true'",
                    save_step.get("if") or "",
                )

        native_health = load_workflow_payload(
            REPO_ROOT / ".github/workflows/native-windows-bazel-health.yml"
        )
        native_steps = (
            ((native_health.get("jobs") or {}).get("test-windows-native-main") or {}).get(
                "steps"
            )
            or []
        )
        native_cache_save = next(
            step for step in native_steps if step.get("name") == "Save bazel repository cache"
        )
        self.assertEqual(native_cache_save.get("continue-on-error"), "true")
        self.assertIn(
            "steps.prepare_bazel.outputs.repository-cache-write-enabled == 'true'",
            native_cache_save.get("if") or "",
        )

    def test_bazel_ci_applies_caller_flags_after_remote_config(self) -> None:
        script = (REPO_ROOT / ".github/scripts/run-bazel-ci.sh").read_text()
        config_append = 'bazel_run_args+=("--config=${ci_config}")'
        caller_append = 'bazel_run_args+=("${bazel_args[@]:1}")'

        self.assertIn('bazel_run_args=("${bazel_args[0]}")', script)
        self.assertLess(script.index(config_append), script.index(caller_append))


class DownstreamDivergenceAuditTests(unittest.TestCase):
    def run_git(self, repo: Path, *args: str) -> str:
        result = subprocess.run(
            ["git", "-C", str(repo), *args],
            check=True,
            capture_output=True,
            text=True,
        )
        return result.stdout.strip()

    def commit_all(self, repo: Path, message: str) -> str:
        self.run_git(repo, "add", ".")
        self.run_git(repo, "commit", "-m", message)
        return self.run_git(repo, "rev-parse", "HEAD")

    def valid_registry_entry(self, **overrides: object) -> dict[str, object]:
        entry: dict[str, object] = {
            "id": "guarded-carry",
            "status": "live",
            "files": ["carry.py"],
            "upstreamability_tier": "upstream-pr",
            "boundary_type": "runtime-contract",
            "hotspot_files": [],
            "extraction_target": "upstream carry",
            "owner": "downstream",
            "guardrail_lane": "hosted-test",
        }
        entry.update(overrides)
        return entry

    def test_required_markers_verify_exact_downstream_commit_tree(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            self.run_git(repo, "init", "-b", "main")
            self.run_git(repo, "config", "user.email", "ci@example.invalid")
            self.run_git(repo, "config", "user.name", "CI")
            (repo / "carry.py").write_text(
                "def bounded_complete_snapshot():\n    return True\n",
                encoding="utf-8",
            )
            downstream_sha = self.commit_all(repo, "guarded carry")
            (repo / "carry.py").write_text(
                "def working_tree_only():\n    return False\n",
                encoding="utf-8",
            )
            registry = {
                "divergences": [
                    self.valid_registry_entry(
                        required_markers={
                            "carry.py": ["bounded_complete_snapshot", "return True"]
                        }
                    )
                ]
            }

            DOWNSTREAM_DIVERGENCE_AUDIT.validate_registry(registry)
            checks = DOWNSTREAM_DIVERGENCE_AUDIT.verify_required_markers(
                repo,
                downstream_sha,
                registry,
                {"guarded-carry"},
            )

            self.assertEqual(
                checks,
                [
                    {
                        "entry_id": "guarded-carry",
                        "path": "carry.py",
                        "markers": ["bounded_complete_snapshot", "return True"],
                    }
                ],
            )

    def test_required_markers_reject_missing_file_and_marker(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            self.run_git(repo, "init", "-b", "main")
            self.run_git(repo, "config", "user.email", "ci@example.invalid")
            self.run_git(repo, "config", "user.name", "CI")
            (repo / "carry.py").write_text("present = True\n", encoding="utf-8")
            downstream_sha = self.commit_all(repo, "guarded carry")

            cases = {
                "missing marker": (
                    {"carry.py": ["bounded_complete_snapshot"]},
                    "carry.py is missing required markers: 'bounded_complete_snapshot'",
                ),
                "missing file": (
                    {"missing.py": ["bounded_complete_snapshot"]},
                    "required marker file is missing: missing.py",
                ),
            }
            for label, (required_markers, message) in cases.items():
                with self.subTest(label=label):
                    registry = {
                        "divergences": [
                            self.valid_registry_entry(
                                files=["*.py"],
                                required_markers=required_markers,
                            )
                        ]
                    }
                    DOWNSTREAM_DIVERGENCE_AUDIT.validate_registry(registry)
                    with self.assertRaisesRegex(ValueError, message):
                        DOWNSTREAM_DIVERGENCE_AUDIT.verify_required_markers(
                            repo,
                            downstream_sha,
                            registry,
                            {"guarded-carry"},
                        )

    def test_required_markers_report_failures_without_enforcement(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            self.run_git(repo, "init", "-b", "main")
            self.run_git(repo, "config", "user.email", "ci@example.invalid")
            self.run_git(repo, "config", "user.name", "CI")
            (repo / "carry.py").write_text("present = True\n", encoding="utf-8")
            downstream_sha = self.commit_all(repo, "guarded carry")
            registry = {
                "divergences": [
                    self.valid_registry_entry(
                        required_markers={
                            "carry.py": ["missing_marker"],
                        }
                    )
                ]
            }

            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                checks = DOWNSTREAM_DIVERGENCE_AUDIT.verify_required_markers(
                    repo,
                    downstream_sha,
                    registry,
                    {"guarded-carry"},
                    enforce=False,
                )

            self.assertEqual(checks, [])
            self.assertEqual(
                stderr.getvalue(),
                "warning: guarded-carry: carry.py is missing required markers: "
                "'missing_marker'\n",
            )

    def test_required_markers_report_invalid_contract_without_enforcement(self) -> None:
        registry = {
            "divergences": [
                self.valid_registry_entry(
                    required_markers={
                        "carry.py": "not-a-list",
                    }
                )
            ]
        }

        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            checks = DOWNSTREAM_DIVERGENCE_AUDIT.verify_required_markers(
                Path("."),
                "unused",
                registry,
                {"guarded-carry"},
                enforce=False,
            )

        self.assertEqual(checks, [])
        self.assertEqual(
            stderr.getvalue(),
            "warning: guarded-carry: required_markers['carry.py'] must be a "
            "non-empty list\n",
        )

    def test_required_markers_reject_unsafe_or_uncovered_paths(self) -> None:
        cases = {
            "parent traversal": (
                {"../carry.py": ["marker"]},
                "path must be a safe repo-relative POSIX path",
            ),
            "absolute": (
                {"/carry.py": ["marker"]},
                "path must be a safe repo-relative POSIX path",
            ),
            "uncovered": (
                {"other.py": ["marker"]},
                "path is not covered by files: other.py",
            ),
        }
        for label, (required_markers, message) in cases.items():
            with self.subTest(label=label):
                registry = {
                    "divergences": [
                        self.valid_registry_entry(required_markers=required_markers)
                    ]
                }
                with self.assertRaisesRegex(ValueError, message):
                    DOWNSTREAM_DIVERGENCE_AUDIT.validate_registry(registry)

    def test_required_markers_reject_empty_marker_contracts(self) -> None:
        cases = {
            "empty object": ({}, "must be a non-empty object"),
            "empty marker list": ({"carry.py": []}, "must be a non-empty list"),
            "empty marker": (
                {"carry.py": [""]},
                "entries must be non-empty strings",
            ),
        }
        for label, (required_markers, message) in cases.items():
            with self.subTest(label=label):
                registry = {
                    "divergences": [
                        self.valid_registry_entry(required_markers=required_markers)
                    ]
                }
                with self.assertRaisesRegex(ValueError, message):
                    DOWNSTREAM_DIVERGENCE_AUDIT.validate_registry(registry)

    def test_registry_gate_uses_downstream_carry_not_upstream_ahead_paths(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            self.run_git(repo, "init", "-b", "main")
            self.run_git(repo, "config", "user.email", "ci@example.invalid")
            self.run_git(repo, "config", "user.name", "CI")

            (repo / "shared.py").write_text("print('base')\n", encoding="utf-8")
            base_sha = self.commit_all(repo, "base")

            self.run_git(repo, "checkout", "-b", "upstream")
            (repo / "upstream_only.py").write_text("print('upstream')\n", encoding="utf-8")
            upstream_sha = self.commit_all(repo, "upstream")

            self.run_git(repo, "checkout", "-b", "downstream", base_sha)
            (repo / "downstream_only.py").write_text("print('downstream')\n", encoding="utf-8")
            downstream_sha = self.commit_all(repo, "downstream")

            all_items = DOWNSTREAM_DIVERGENCE_AUDIT.diff_items_between(
                repo,
                upstream_sha,
                downstream_sha,
            )
            all_code_items = [item for item in all_items if item.is_code]
            all_paths = sorted({path for item in all_code_items for path in item.paths})
            self.assertEqual(all_paths, ["downstream_only.py", "upstream_only.py"])

            merge_base = DOWNSTREAM_DIVERGENCE_AUDIT.merge_base_sha(
                repo,
                upstream_sha,
                downstream_sha,
            )
            carry_items = DOWNSTREAM_DIVERGENCE_AUDIT.diff_items_between(
                repo,
                merge_base,
                downstream_sha,
            )
            carry_code_items = [item for item in carry_items if item.is_code]
            registry = {
                "_path": "docs/divergences/index.yaml",
                "divergences": [
                    {
                        "id": "downstream-only",
                        "status": "live",
                        "files": ["downstream_only.py"],
                    }
                ],
            }

            registry_state = DOWNSTREAM_DIVERGENCE_AUDIT.reconcile_registry(
                registry,
                carry_code_items,
            )

            self.assertEqual(registry_state["uncovered_code_paths"], [])
            self.assertEqual(
                registry_state["path_registry_ids"],
                {"downstream_only.py": ["downstream-only"]},
            )


class ValidationPlanScriptTests(unittest.TestCase):
    maxDiff = None

    def validation_lab_fingerprint(
        self,
        *,
        selection_meta: dict | None = None,
        artifact_build: bool = False,
        include_explicit_lanes: bool = False,
    ) -> str:
        selection = {
            "fanout_tier": "enterprise",
            "run_selected_lanes": True,
            "run_smoke_gate": False,
            "smoke_gate_kind": "none",
            "run_artifact": artifact_build,
            "matrix_fail_fast": False,
            "matrix_max_parallel": 4,
            "workflow_max_parallel": 4,
            "node_max_parallel": 4,
            "rust_minimal_max_parallel": 4,
            "rust_integration_max_parallel": 4,
            "release_max_parallel": 4,
            "rust_batching_mode": "auto",
            "selected_setup_classes": ["workflow"],
            "selected_lane_ids": ["codex.workflow-ci-sanity"],
            "planned_matrix": {
                "include": [
                    {
                        "lane_id": "codex.workflow-ci-sanity",
                        "setup_class": "workflow",
                    }
                ]
            },
            "smoke_matrix": {"include": []},
            "selected_matrix": {
                "include": [
                    {
                        "lane_id": "codex.workflow-ci-sanity",
                        "setup_class": "workflow",
                    }
                ]
            },
            "selected_rust_minimal_batch_matrix": {"include": []},
            "selected_rust_integration_batch_matrix": {"include": []},
        }
        if selection_meta:
            selection.update(selection_meta)
        payload = VALIDATION_PLAN_FINGERPRINT.plan_fingerprint_payload(
            selection_meta=selection,
            workflow="validation-lab.yml",
            workflow_ref="sednalabs/codex/.github/workflows/validation-lab.yml@refs/heads/main",
            workflow_sha="feedface",
            target_head_sha="abc123",
            profile="targeted",
            lane_set="docs",
            fanout_tier="enterprise",
            lanes="codex.workflow-ci-sanity",
            rust_batching="auto",
            artifact_build=artifact_build,
            include_explicit_lanes=include_explicit_lanes,
        )
        return VALIDATION_PLAN_FINGERPRINT.fingerprint_payload(payload)

    def test_validation_lab_plan_fingerprint_is_stable_for_exact_plan(self) -> None:
        first = self.validation_lab_fingerprint()
        second = self.validation_lab_fingerprint()

        self.assertEqual(first, second)

    def test_validation_lab_plan_fingerprint_changes_for_lane_list(self) -> None:
        baseline = self.validation_lab_fingerprint()
        changed = self.validation_lab_fingerprint(
            selection_meta={
                "selected_lane_ids": [
                    "codex.workflow-ci-sanity",
                    "codex.downstream-docs-check",
                ],
                "planned_matrix": {
                    "include": [
                        {
                            "lane_id": "codex.workflow-ci-sanity",
                            "setup_class": "workflow",
                        },
                        {
                            "lane_id": "codex.downstream-docs-check",
                            "setup_class": "workflow",
                        },
                    ]
                },
            }
        )

        self.assertNotEqual(baseline, changed)

    def test_validation_lab_plan_fingerprint_changes_for_artifact_flag(self) -> None:
        baseline = self.validation_lab_fingerprint(artifact_build=False)
        artifact = self.validation_lab_fingerprint(artifact_build=True)

        self.assertNotEqual(baseline, artifact)

    def test_validation_lab_plan_fingerprint_reports_missing_selection_env(self) -> None:
        env = dict(os.environ)
        env.pop("SELECTION_META", None)
        proc = subprocess.run(
            [
                "python3",
                str(SCRIPTS_DIR / "validation_plan_fingerprint.py"),
                "--workflow",
                "validation-lab.yml",
                "--workflow-ref",
                "sednalabs/codex/.github/workflows/validation-lab.yml@refs/heads/main",
                "--workflow-sha",
                "feedface",
                "--target-head-sha",
                "abc123",
                "--profile",
                "targeted",
                "--lane-set",
                "docs",
                "--fanout-tier",
                "enterprise",
            ],
            check=False,
            capture_output=True,
            text=True,
            env=env,
        )

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("missing selection metadata env: SELECTION_META", proc.stderr)

    def test_validation_lab_plan_fingerprint_reads_selection_stdin(self) -> None:
        selection = {
            "selected_lane_ids": ["codex.workflow-ci-sanity"],
            "planned_matrix": {"include": []},
        }
        env = dict(os.environ)
        env.pop("SELECTION_META", None)
        proc = subprocess.run(
            [
                "python3",
                str(SCRIPTS_DIR / "validation_plan_fingerprint.py"),
                "--selection-meta-stdin",
                "--workflow",
                "validation-lab.yml",
                "--workflow-ref",
                "sednalabs/codex/.github/workflows/validation-lab.yml@refs/heads/main",
                "--workflow-sha",
                "feedface",
                "--target-head-sha",
                "abc123",
                "--profile",
                "targeted",
                "--lane-set",
                "docs",
                "--fanout-tier",
                "enterprise",
            ],
            check=False,
            capture_output=True,
            text=True,
            env=env,
            input=json.dumps(selection),
        )

        self.assertEqual(proc.returncode, 0, proc.stderr)
        expected_payload = VALIDATION_PLAN_FINGERPRINT.plan_fingerprint_payload(
            selection_meta=selection,
            workflow="validation-lab.yml",
            workflow_ref=(
                "sednalabs/codex/.github/workflows/validation-lab.yml@refs/heads/main"
            ),
            workflow_sha="feedface",
            target_head_sha="abc123",
            profile="targeted",
            lane_set="docs",
            fanout_tier="enterprise",
            lanes="",
            rust_batching="auto",
            artifact_build=False,
            include_explicit_lanes=False,
        )
        self.assertEqual(
            proc.stdout.strip(),
            VALIDATION_PLAN_FINGERPRINT.fingerprint_payload(expected_payload),
        )

    def test_validation_lab_plan_fingerprint_reports_missing_selection_stdin(self) -> None:
        proc = subprocess.run(
            [
                "python3",
                str(SCRIPTS_DIR / "validation_plan_fingerprint.py"),
                "--selection-meta-stdin",
                "--workflow",
                "validation-lab.yml",
                "--workflow-ref",
                "sednalabs/codex/.github/workflows/validation-lab.yml@refs/heads/main",
                "--workflow-sha",
                "feedface",
                "--target-head-sha",
                "abc123",
                "--profile",
                "targeted",
                "--lane-set",
                "docs",
                "--fanout-tier",
                "enterprise",
            ],
            check=False,
            capture_output=True,
            text=True,
            input="",
        )

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("missing selection metadata on stdin", proc.stderr)

    def recommend_lab_for_files(self, files: list[str]) -> dict:
        return run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "recommend-lab",
            "--changed-files-json",
            json.dumps(files),
            "--catalog-path",
            str(REPO_ROOT / ".github/validation-lanes.json"),
        )

    def test_recommend_lab_workflow_only_uses_workflow_route(self) -> None:
        payload = self.recommend_lab_for_files([".github/workflows/validation-lab.yml"])

        self.assertTrue(payload["advisory"])
        self.assertEqual(payload["profile"], "targeted")
        self.assertEqual(payload["lane_set"], "docs")
        self.assertEqual(payload["source"], "followup_route")
        self.assertEqual(
            payload["lane_ids"],
            [
                "codex.workflow-ci-sanity",
                "codex.downstream-docs-check",
            ],
        )
        self.assertEqual(
            payload["dispatch_inputs"]["lanes"],
            "codex.workflow-ci-sanity,codex.downstream-docs-check",
        )

    def test_recommend_lab_rust_core_path_keeps_core_lane_set(self) -> None:
        payload = self.recommend_lab_for_files(["codex-rs/core/src/lib.rs"])

        self.assertEqual(payload["profile"], "targeted")
        self.assertEqual(payload["lane_set"], "core-carry")
        self.assertEqual(payload["source"], "followup_route")
        self.assertEqual(
            payload["lane_ids"],
            [
                "codex.blocking-waits-core-targeted",
                "codex.blocking-waits-unified-exec-targeted",
                "codex.blocking-waits-app-server-targeted",
                "codex.blocking-waits-mcp-targeted",
            ],
        )

    def test_recommend_lab_ui_protocol_path_uses_exact_route(self) -> None:
        payload = self.recommend_lab_for_files(
            ["codex-rs/app-server-protocol/src/protocol/v2/thread.rs"]
        )

        self.assertEqual(payload["profile"], "targeted")
        self.assertEqual(payload["lane_set"], "ui-protocol")
        self.assertEqual(payload["source"], "followup_route")
        self.assertEqual(
            payload["lane_ids"],
            [
                "codex.app-server-protocol-test",
                "codex.app-server-thread-cwd-targeted",
                "codex.blocking-waits-app-server-targeted",
            ],
        )

    def test_recommend_lab_external_agent_containment_route_is_fail_closed(self) -> None:
        payload = self.recommend_lab_for_files(
            [
                "codex-rs/external-agent-migration/src/service.rs",
                "codex-rs/external-agent-migration/Cargo.toml",
                "codex-rs/Cargo.lock",
                "MODULE.bazel.lock",
                "codex-rs/app-server/tests/suite/v2/external_agent_config.rs",
                ".github/scripts/test_ci_planners.py",
                ".github/workflows/sedna-heavy-tests.yml",
                "docs/carry-divergence-ledger.md",
            ]
        )

        self.assertEqual(payload["profile"], "targeted")
        self.assertEqual(payload["source"], "followup_route")
        self.assertEqual(
            payload["lane_ids"],
            ["codex.external-agent-migration-containment-targeted"],
        )

        lane = next(
            lane
            for lane in RESOLVE_VALIDATION_PLAN.load_catalog()["lanes"]
            if lane["lane_id"]
            == "codex.external-agent-migration-containment-targeted"
        )
        self.assertTrue(lane["needs_nextest"])
        recipe = "\n".join(
            just_recipe_bodies(REPO_ROOT / "justfile")[
                "external-agent-migration-containment-targeted"
            ]
        )
        self.assertEqual(recipe.count("cargo nextest run --locked"), 2)
        self.assertEqual(recipe.count("--no-tests=fail"), 2)
        self.assertIn(
            "suite::v2::external_agent_config::"
            "external_agent_memory_import_rejects_stale_symlink_before_workspace_mutation",
            recipe,
        )
        self.assertIn("--exact", recipe)

    def test_recommend_lab_subagent_model_pinning_route_is_fail_closed(self) -> None:
        payload = self.recommend_lab_for_files(
            [
                "codex-rs/core/src/agent/control.rs",
                "codex-rs/core/src/agent/control/spawn.rs",
                "codex-rs/core/src/agent/control_tests.rs",
                "codex-rs/core/src/agent/builtins/terminal-babysitter.toml",
                "codex-rs/core/src/agent/role_tests.rs",
                "codex-rs/core/src/tools/handlers/multi_agents_common.rs",
                "codex-rs/core/src/tools/handlers/multi_agents_spec_tests.rs",
                "codex-rs/core/src/tools/handlers/multi_agents_tests.rs",
                "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
                "codex-rs/core/tests/suite/subagent_notifications.rs",
                "codex-rs/core/tests/suite/multi_agent_resume.rs",
                "codex-rs/state/src/extract.rs",
                "codex-rs/thread-store/src/local/read_thread.rs",
                "codex-rs/thread-store/src/thread_metadata_sync.rs",
                "codex-rs/thread-store/src/types.rs",
                ".github/scripts/test_ci_planners.py",
                ".github/validation-lanes.json",
                "docs/carry-divergence-ledger.md",
                "docs/divergences/index.yaml",
                "docs/downstream-regression-matrix.md",
                "docs/downstream.md",
                "justfile",
            ]
        )

        self.assertEqual(payload["profile"], "targeted")
        self.assertEqual(payload["source"], "followup_route")
        self.assertEqual(
            payload["lane_ids"],
            ["codex.core-subagent-model-pinning-targeted"],
        )

        lane = next(
            lane
            for lane in RESOLVE_VALIDATION_PLAN.load_catalog()["lanes"]
            if lane["lane_id"] == "codex.core-subagent-model-pinning-targeted"
        )
        self.assertTrue(lane["needs_nextest"])
        recipe = "\n".join(
            just_recipe_bodies(REPO_ROOT / "justfile")[
                "core-subagent-model-pinning-targeted"
            ]
        )
        self.assertEqual(recipe.count("cargo nextest run"), 6)
        self.assertEqual(recipe.count("RUST_MIN_STACK="), 6)
        self.assertEqual(recipe.count("--no-tests=fail"), 6)
        self.assertIn(
            "tools::handlers::multi_agents_spec::tests::"
            "spawn_agent_tool_v2_requires_task_name_and_lists_visible_models",
            recipe,
        )
        self.assertIn(
            "agent::role::tests::apply_role_preserves_unspecified_keys",
            recipe,
        )
        self.assertIn(
            "agent::role::tests::"
            "spawn_tool_spec_marks_terminal_babysitter_locked_model_and_reasoning_effort",
            recipe,
        )
        self.assertIn(
            "tools::handlers::multi_agents::tests::"
            "spawn_agent_reasoning_effort_accepts_empty_support_metadata",
            recipe,
        )
        self.assertIn(
            "tools::handlers::multi_agents::tests::"
            "multi_agent_v2_spawn_accepts_child_model_without_backend_assignment",
            recipe,
        )
        self.assertIn(
            "tools::handlers::multi_agents::tests::"
            "multi_agent_v2_spawn_accepts_luna_compatibility_override",
            recipe,
        )
        self.assertIn(
            "tools::handlers::multi_agents::tests::"
            "multi_agent_v2_spawn_rejects_child_model_from_different_backend",
            recipe,
        )
        self.assertIn(
            "tools::handlers::multi_agents::tests::"
            "multi_agent_v2_spawn_fork_turns_all_rejects_agent_type_override",
            recipe,
        )
        self.assertIn(
            "tools::handlers::multi_agents::tests::"
            "multi_agent_v2_spawn_partial_fork_turns_allows_agent_type_override",
            recipe,
        )
        self.assertIn(
            "suite::subagent_notifications::"
            "spawn_agent_uses_configured_subagent_defaults",
            recipe,
        )
        self.assertIn(
            "suite::subagent_notifications::"
            "spawn_agent_preserves_configured_defaults_through_unrelated_role",
            recipe,
        )
        self.assertIn(
            "suite::subagent_notifications::"
            "spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role",
            recipe,
        )
        self.assertIn(
            "suite::subagent_notifications::"
            "spawn_agent_role_overrides_requested_model_and_reasoning_settings",
            recipe,
        )
        self.assertIn(
            "suite::subagent_notifications::"
            "spawn_agent_rejects_reasoning_effort_unsupported_by_role_model",
            recipe,
        )
        self.assertIn(
            "suite::subagent_notifications::"
            "spawned_full_history_v2_child_uses_model_precedence_without_dropping_context",
            recipe,
        )
        self.assertIn(
            "local::read_thread::tests::"
            "read_thread_keeps_complete_indexed_identity_during_rollout_overlay",
            recipe,
        )
        self.assertIn(
            "suite::multi_agent_resume::"
            "cold_root_resume_restores_agent_identity_and_reloads_target_on_followup",
            recipe,
        )
        self.assertIn(
            "suite::multi_agent_resume::"
            "cold_root_resume_restores_agent_identity_and_role_on_followup",
            recipe,
        )
        self.assertEqual(recipe.count("--exact"), 5)

    def test_recommend_lab_docs_path_uses_docs_domain_fallback(self) -> None:
        payload = self.recommend_lab_for_files(["docs/validation_workflow.md"])

        self.assertEqual(payload["profile"], "targeted")
        self.assertEqual(payload["lane_set"], "docs")
        self.assertEqual(payload["source"], "domain_rules")
        self.assertEqual(payload["lane_ids"], ["codex.downstream-docs-check"])

    def test_recommend_lab_release_path_uses_release_domain_fallback(self) -> None:
        payload = self.recommend_lab_for_files(
            [".github/workflows/sedna-branch-build.yml"]
        )

        self.assertEqual(payload["profile"], "targeted")
        self.assertEqual(payload["lane_set"], "release")
        self.assertEqual(payload["source"], "domain_rules")
        self.assertEqual(payload["lane_ids"], [])

    def test_recommend_lab_unknown_path_uses_frontier_fallback(self) -> None:
        payload = self.recommend_lab_for_files(["unknown/place/example.txt"])

        self.assertEqual(payload["profile"], "frontier")
        self.assertEqual(payload["lane_set"], "all")
        self.assertEqual(payload["source"], "conservative_fallback")
        self.assertEqual(payload["lane_ids"], [])
        self.assertEqual(payload["domains"], ["unknown"])

    def test_recommend_lab_missing_metadata_uses_frontier_fallback(self) -> None:
        payload = self.recommend_lab_for_files([])

        self.assertEqual(payload["profile"], "frontier")
        self.assertEqual(payload["lane_set"], "all")
        self.assertEqual(payload["source"], "conservative_fallback")
        self.assertIn("metadata was empty", payload["reason"])

    def test_recommend_lab_rejects_route_with_unknown_lane(self) -> None:
        catalog = json.loads((REPO_ROOT / ".github/validation-lanes.json").read_text())
        catalog["followup_routes"].append(
            {
                "route_id": "synthetic-missing-lane",
                "lane_ids": ["codex.synthetic-missing-lane"],
                "allowed_paths": ["synthetic/missing-lane.txt"],
            }
        )

        with tempfile.NamedTemporaryFile("w", encoding="utf-8", suffix=".json") as handle:
            json.dump(catalog, handle)
            handle.flush()

            proc = subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "resolve_validation_plan.py"),
                    "recommend-lab",
                    "--changed-files-json",
                    json.dumps(["synthetic/missing-lane.txt"]),
                    "--catalog-path",
                    handle.name,
                ],
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(
            "matched follow-up route contains unknown lane IDs: "
            "codex.synthetic-missing-lane",
            proc.stderr,
        )

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
        self.assertEqual(len(payload["selected_matrix"]["include"]), 26)
        self.assertEqual(payload["planned_job_count"], 16)
        self.assertEqual(payload["rust_batching_mode"], "auto")
        self.assertEqual(payload["selected_workflow_lane_count"], 0)
        self.assertEqual(payload["selected_node_lane_count"], 0)
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 0)
        self.assertEqual(payload["selected_rust_minimal_batch_count"], 9)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 0)
        self.assertEqual(payload["selected_rust_integration_batch_count"], 7)
        self.assertEqual(payload["selected_release_lane_count"], 0)
        for batch in (
            payload["selected_rust_minimal_batch_matrix"]["include"]
            + payload["selected_rust_integration_batch_matrix"]["include"]
        ):
            self.assertLessEqual(batch["batch_lane_count"], 2)
            self.assertLessEqual(batch["estimated_weight_seconds"], 720)
        self.assertTrue(
            all(
                lane.get("checkout_fetch_depth") == 1
                for lane in payload["selected_matrix"]["include"]
            )
        )
        self.assertIn("codex.app-server-protocol-test", payload["selected_lane_ids"])
        blocking_wait_lanes = {
            "codex.blocking-waits-app-server-targeted",
            "codex.blocking-waits-core-targeted",
            "codex.blocking-waits-mcp-targeted",
            "codex.blocking-waits-unified-exec-targeted",
        }
        selected_lane_ids = set(payload["selected_lane_ids"])
        self.assertTrue(
            blocking_wait_lanes.issubset(selected_lane_ids),
            f"missing blocking wait lanes: {blocking_wait_lanes - selected_lane_ids}",
        )
        blocking_wait_batches = {}
        for batch in payload["selected_rust_integration_batch_matrix"]["include"]:
            lane_ids = batch["lane_ids"]
            if len(lane_ids) == 1 and lane_ids[0] in blocking_wait_lanes:
                blocking_wait_batches[lane_ids[0]] = batch
        self.assertEqual(set(blocking_wait_batches), blocking_wait_lanes)
        for batch in blocking_wait_batches.values():
            self.assertEqual(batch["batch_lane_count"], 1)
            self.assertEqual(batch["estimated_weight_seconds"], 720)

        native_registry_batches = {}
        for batch in payload["selected_rust_minimal_batch_matrix"]["include"]:
            lane_ids = batch["lane_ids"]
            if lane_ids == ["codex.native-computer-use-tool-registry-targeted"]:
                native_registry_batches[lane_ids[0]] = batch
        self.assertEqual(
            set(native_registry_batches),
            {"codex.native-computer-use-tool-registry-targeted"},
        )
        for batch in native_registry_batches.values():
            self.assertEqual(batch["batch_lane_count"], 1)
            self.assertEqual(batch["estimated_weight_seconds"], 720)

    def test_lab_targeted_ui_protocol_can_disable_rust_batching(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "lab",
            "--profile",
            "targeted",
            "--lane-set",
            "ui-protocol",
            "--rust-batching",
            "off",
            "--catalog-path",
            str(REPO_ROOT / ".github/validation-lanes.json"),
        )

        self.assertEqual(payload["planned_job_count"], 26)
        self.assertEqual(payload["rust_batching_mode"], "off")
        self.assertEqual(payload["rust_batching_reason"], "disabled by workflow input")
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 17)
        self.assertEqual(payload["selected_rust_minimal_batch_count"], 0)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 9)
        self.assertEqual(payload["selected_rust_integration_batch_count"], 0)

    def test_lab_product_surface_lane_set_returns_first_wave_lanes(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "lab",
            "--profile",
            "targeted",
            "--lane-set",
            "product-surfaces",
            "--catalog-path",
            str(REPO_ROOT / ".github/validation-lanes.json"),
        )

        self.assertEqual(payload["planned_job_count"], 5)
        self.assertEqual(payload["selected_workflow_lane_count"], 1)
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 1)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 3)
        self.assertEqual(
            payload["selected_lane_ids"],
            [
                "codex.app-server-v2-contract-targeted",
                "codex.mcp-server-contract-targeted",
                "codex.exec-server-targeted",
                "codex.cli-surface-targeted",
                "codex.workflow-security-targeted",
            ],
        )

    def test_lab_sdk_lane_set_returns_python_and_typescript_lanes(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "lab",
            "--profile",
            "targeted",
            "--lane-set",
            "sdk",
            "--catalog-path",
            str(REPO_ROOT / ".github/validation-lanes.json"),
        )

        self.assertEqual(payload["planned_job_count"], 2)
        self.assertEqual(payload["selected_workflow_lane_count"], 1)
        self.assertEqual(payload["selected_node_lane_count"], 1)
        self.assertEqual(
            payload["selected_lane_ids"],
            [
                "codex.sdk-python-targeted",
                "codex.sdk-typescript-targeted",
            ],
        )

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

    def test_lab_rejects_matrix_plans_above_job_limit(self) -> None:
        def workflow_lane(index: int) -> dict:
            return {
                "lane_id": f"codex.synthetic-workflow-{index:03d}",
                "groups": ["workflow"],
                "lane_sets": ["all"],
                "status_class": "active",
                "setup_class": "workflow",
                "frontier_role": "depth",
                "summary_family": f"synthetic-workflow-{index:03d}",
                "cost_class": "low",
                "checkout_fetch_depth": 1,
                "timeout_minutes": 30,
                "working_directory": ".",
                "script_path": ".github/scripts/validation-lanes/workflow-ci-sanity.sh",
                "script_args": [],
                "needs_just": False,
                "needs_node": False,
                "needs_nextest": False,
                "needs_linux_build_deps": False,
                "needs_dotslash": False,
                "needs_sccache": False,
                "needs_bazel": False,
            }

        catalog = {"lanes": [workflow_lane(index) for index in range(257)]}
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", suffix=".json") as handle:
            json.dump(catalog, handle)
            handle.flush()

            proc = subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "resolve_validation_plan.py"),
                    "lab",
                    "--profile",
                    "frontier",
                    "--lane-set",
                    "all",
                    "--artifact-build",
                    "false",
                    "--catalog-path",
                    handle.name,
                ],
                capture_output=True,
                text=True,
                check=False,
            )

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("would create 257 matrix/artifact jobs", proc.stderr)
        self.assertIn("above the 256 job cap", proc.stderr)

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

    def test_validation_catalog_rejects_absolute_and_traversal_paths(self) -> None:
        catalog = RESOLVE_VALIDATION_PLAN.normalize_catalog(RESOLVE_VALIDATION_PLAN.load_catalog())

        absolute_catalog = json.loads(json.dumps(catalog))
        absolute_catalog["lanes"][0]["working_directory"] = "/tmp"
        with self.assertRaisesRegex(
            SystemExit,
            "must be a relative path within the repository root",
        ):
            RESOLVE_VALIDATION_PLAN.validate_catalog(absolute_catalog, repo_root=REPO_ROOT)

        traversal_catalog = json.loads(json.dumps(catalog))
        traversal_catalog["lanes"][0]["script_path"] = "../escape.sh"
        with self.assertRaisesRegex(
            SystemExit,
            "must not contain '..' path segments",
        ):
            RESOLVE_VALIDATION_PLAN.validate_catalog(traversal_catalog, repo_root=REPO_ROOT)

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
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 0)
        self.assertEqual(payload["selected_rust_minimal_batch_count"], 13)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 1)
        self.assertEqual(payload["selected_rust_integration_batch_count"], 12)
        self.assertEqual(payload["selected_release_lane_count"], 0)
        self.assertEqual(payload["smoke_rust_integration_lane_count"], 5)
        self.assertEqual(payload["smoke_release_lane_count"], 1)
        self.assertEqual(payload["workflow_max_parallel"], "8")
        self.assertEqual(payload["node_max_parallel"], "4")
        self.assertEqual(payload["rust_minimal_max_parallel"], "6")
        self.assertEqual(payload["rust_integration_max_parallel"], "2")
        self.assertEqual(payload["release_max_parallel"], "1")
        self.assertEqual(payload["rust_batching_mode"], "auto")
        self.assertIn(
            "codex.core-multi-agent-orchestration-targeted",
            payload["selected_lane_ids"],
        )

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
        self.assertEqual(
            (jobs.get("rust_minimal_lanes") or {}).get("needs"), ["metadata"]
        )
        self.assertEqual(
            (jobs.get("rust_minimal_batches") or {}).get("needs"), ["metadata"]
        )
        self.assertEqual((jobs.get("nextest_archives") or {}).get("needs"), ["metadata"])
        self.assertEqual(
            (jobs.get("rust_integration_archive_lanes") or {}).get("needs"),
            ["metadata", "nextest_archives"],
        )
        self.assertEqual(
            (jobs.get("rust_integration_lanes") or {}).get("needs"), ["metadata"]
        )
        self.assertEqual(
            (jobs.get("rust_integration_batches") or {}).get("needs"), ["metadata"]
        )
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
                "rust_minimal_batches",
                "nextest_archives",
                "rust_integration_archive_lanes",
                "rust_integration_lanes",
                "rust_integration_batches",
                "release_lanes",
                "artifact",
            ],
        )

    def test_validation_lab_summary_records_cache_occupancy(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        summary = ((payload.get("jobs") or {}).get("summary") or {})
        steps = summary.get("steps") or []

        self.assertEqual((summary.get("permissions") or {}).get("actions"), "read")
        record_step = next(
            (
                step
                for step in steps
                if step.get("name") == "Record Actions cache occupancy"
            ),
            {},
        )
        report_step = next(
            (
                step
                for step in steps
                if "--cache-occupancy-json" in (step.get("run") or "")
            ),
            {},
        )
        self.assertIn(
            "report_actions_cache_occupancy.py",
            record_step.get("run") or "",
        )
        self.assertIn(
            "--cache-occupancy-json",
            report_step.get("run") or "",
        )
        self.assertIn(
            '--rust-batching-mode "${{ needs.metadata.outputs.rust_batching_mode }}"',
            report_step.get("run") or "",
        )
        self.assertIn(
            '--rust-batching-reason "${{ needs.metadata.outputs.rust_batching_reason }}"',
            report_step.get("run") or "",
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

    def test_sedna_branch_build_uses_safe_ref_env_and_macos_preview_matrix(
        self,
    ) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-branch-build.yml")
        workflow_dispatch_inputs = (
            (payload.get("on") or {}).get("workflow_dispatch") or {}
        ).get("inputs") or {}
        self.assertEqual(
            (workflow_dispatch_inputs.get("platform") or {}).get("options"),
            ["linux-x86_64", "macos"],
        )
        self.assertEqual(
            (workflow_dispatch_inputs.get("platform") or {}).get("default"),
            "linux-x86_64",
        )

        metadata_step = workflow_step_by_name(
            REPO_ROOT / ".github/workflows/sedna-branch-build.yml",
            "metadata",
            "Compute preview version",
        )
        env = metadata_step.get("env") or {}
        self.assertEqual(
            env.get("CHECKOUT_REF"),
            "${{ github.event_name == 'workflow_dispatch' && inputs.ref || github.sha }}",
        )
        self.assertEqual(
            env.get("DISPLAY_REF"),
            "${{ github.event_name == 'workflow_dispatch' && inputs.ref || github.ref_name }}",
        )
        run_script = metadata_step.get("run") or ""
        self.assertIn('checkout_ref="${CHECKOUT_REF}"', run_script)
        self.assertIn('branch_name="${DISPLAY_REF}"', run_script)
        self.assertNotIn("checkout_ref='${{", run_script)

        build_job = (payload.get("jobs") or {}).get("build") or {}
        self.assertEqual(
            (build_job.get("with") or {}).get("display_ref"),
            "${{ needs.metadata.outputs.display_ref }}",
        )
        run_command = (build_job.get("with") or {}).get("run_command") or ""
        self.assertIn('os.environ["DISPLAY_REF"]', run_command)
        self.assertIn("json.dump(payload, sys.stdout, indent=2)", run_command)
        self.assertNotIn("${{ needs.metadata.outputs.display_ref }}", run_command)

        macos_job = (payload.get("jobs") or {}).get("build-macos") or {}
        self.assertEqual(macos_job.get("if"), "${{ inputs.platform == 'macos' }}")
        self.assertEqual(
            ((macos_job.get("strategy") or {}).get("matrix") or {}).get("include"),
            [
                {
                    "runner": "macos-15",
                    "target": "aarch64-apple-darwin",
                },
                {
                    "runner": "macos-15-intel",
                    "target": "x86_64-apple-darwin",
                },
            ],
        )
        macos_build_step = workflow_step_by_name(
            REPO_ROOT / ".github/workflows/sedna-branch-build.yml",
            "Build macOS preview binaries",
        )
        macos_build_script = macos_build_step.get("run") or ""
        self.assertIn("--locked", macos_build_script)
        self.assertIn("--bin codex-code-mode-host", macos_build_script)

        macos_stage_step = workflow_step_by_name(
            REPO_ROOT / ".github/workflows/sedna-branch-build.yml",
            "Stage ad hoc signed macOS preview artifact",
        )
        macos_stage_script = macos_stage_step.get("run") or ""
        self.assertIn('"signing": "ad-hoc"', macos_stage_script)
        self.assertIn('"notarized": False', macos_stage_script)
        self.assertNotIn("${{ needs.metadata.outputs.display_ref }}", macos_stage_script)

    def test_validation_lab_uses_safe_ref_env_for_checkout_and_display_refs(self) -> None:
        metadata_step = workflow_step_by_name(
            REPO_ROOT / ".github/workflows/validation-lab.yml",
            "metadata",
            "Compute validation-lab plan",
        )
        env = metadata_step.get("env") or {}
        self.assertEqual(env.get("LAB_HOST_REF"), "${{ github.ref_name }}")
        self.assertEqual(env.get("LAB_CHECKOUT_REF"), "${{ inputs.ref || github.sha }}")
        self.assertEqual(env.get("LAB_DISPLAY_REF"), "${{ inputs.ref || github.ref_name }}")
        run_script = metadata_step.get("run") or ""
        self.assertIn('host_ref="${LAB_HOST_REF}"', run_script)
        self.assertIn('checkout_ref="${LAB_CHECKOUT_REF}"', run_script)
        self.assertIn('display_ref="${LAB_DISPLAY_REF}"', run_script)
        self.assertNotIn("checkout_ref='${{", run_script)
        self.assertNotIn("display_ref='${{", run_script)

    def test_validation_lab_exposes_fanout_and_batching_controls(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        workflow_dispatch_inputs = (
            (((payload.get("on") or {}).get("workflow_dispatch") or {}).get("inputs") or {})
        )
        workflow_call_inputs = (
            (((payload.get("on") or {}).get("workflow_call") or {}).get("inputs") or {})
        )
        lane_set_options = (workflow_dispatch_inputs.get("lane_set") or {}).get("options") or []

        self.assertIn("product-surfaces", lane_set_options)
        self.assertIn("sdk", lane_set_options)
        self.assertEqual(
            (workflow_dispatch_inputs.get("fanout_tier") or {}).get("options"),
            ["balanced", "enterprise", "soak"],
        )
        self.assertEqual(
            (workflow_dispatch_inputs.get("rust_batching") or {}).get("options"),
            ["auto", "off", "force"],
        )
        self.assertEqual(
            (workflow_call_inputs.get("fanout_tier") or {}).get("default"),
            "enterprise",
        )
        self.assertEqual(
            (workflow_call_inputs.get("rust_batching") or {}).get("default"), "auto"
        )
        for workflow_name in ("validation-lab.yml", "sedna-heavy-tests.yml"):
            workflow_text = (
                REPO_ROOT / ".github/workflows" / workflow_name
            ).read_text()
            self.assertIn('          - "off"\n', workflow_text)
            self.assertNotIn("          - off\n", workflow_text)

        metadata_job = ((payload.get("jobs") or {}).get("metadata") or {})
        self.assertEqual(
            (metadata_job.get("outputs") or {}).get("fanout_tier"),
            "${{ steps.meta.outputs.fanout_tier }}",
        )
        self.assertEqual(
            (metadata_job.get("outputs") or {}).get("planned_job_count"),
            "${{ steps.meta.outputs.planned_job_count }}",
        )
        metadata_step = workflow_step_by_name(
            REPO_ROOT / ".github/workflows/validation-lab.yml",
            "metadata",
            "Compute validation-lab plan",
        )
        env = metadata_step.get("env") or {}
        self.assertEqual(
            env.get("LAB_FANOUT_TIER"),
            "${{ inputs.fanout_tier || 'enterprise' }}",
        )
        self.assertEqual(env.get("LAB_RUST_BATCHING"), "${{ inputs.rust_batching || 'auto' }}")
        self.assertEqual(
            env.get("LAB_RUST_BATCHING_OVERRIDE"),
            "${{ vars.VALIDATION_LAB_RUST_BATCHING }}",
        )
        run_script = metadata_step.get("run") or ""
        self.assertIn('--fanout-tier "${LAB_FANOUT_TIER}"', run_script)
        self.assertIn('--rust-batching "${LAB_RUST_BATCHING}"', run_script)
        self.assertIn('--rust-batching-override "${LAB_RUST_BATCHING_OVERRIDE}"', run_script)

    def test_validation_lab_exposes_exact_plan_dedupe_metadata(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        jobs = payload.get("jobs") or {}
        metadata_job = jobs.get("metadata") or {}
        outputs = metadata_job.get("outputs") or {}
        steps = metadata_job.get("steps") or []

        self.assertEqual((metadata_job.get("permissions") or {}).get("actions"), "read")
        self.assertEqual(
            outputs.get("planner_fingerprint"),
            "${{ steps.meta.outputs.planner_fingerprint }}",
        )
        self.assertEqual(
            outputs.get("dedupe_should_skip"),
            "${{ steps.dedupe.outputs.should_skip || 'false' }}",
        )
        compute_step = next(
            step for step in steps if step.get("name") == "Compute validation-lab plan"
        )
        compute_env = compute_step.get("env") or {}
        compute_run = compute_step.get("run") or ""
        self.assertEqual(compute_env.get("LAB_WORKFLOW_REF"), "${{ github.workflow_ref }}")
        self.assertEqual(compute_env.get("LAB_WORKFLOW_SHA"), "${{ github.sha }}")
        self.assertIn("validation_plan_fingerprint.py", compute_run)
        self.assertIn("--selection-meta-stdin", compute_run)
        self.assertIn('< "${selection_meta_path}"', compute_run)
        self.assertNotIn("--selection-meta-path", compute_run)
        self.assertIn("planner_fingerprint=${planner_fingerprint}", compute_run)

        dedupe_step = next(
            step for step in steps if step.get("name") == "Check exact-plan evidence reuse"
        )
        dedupe_run = dedupe_step.get("run") or ""
        self.assertIn("skip_duplicate_workflow_run.py", dedupe_run)
        self.assertIn("--summary-artifact-name validation-summary", dedupe_run)
        self.assertIn(
            '--required-planner-fingerprint "${LAB_PLANNER_FINGERPRINT}"',
            dedupe_run,
        )
        self.assertIn('if [[ "${LAB_SUPERSESSION_MODE}" != "auto"', dedupe_run)
        self.assertIn("exact_plan_success_available_retained_by_", dedupe_run)

    def test_validation_lab_exact_plan_skip_gates_fanout_jobs_only(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        jobs = payload.get("jobs") or {}
        fanout_jobs = [
            "smoke_workflow_lanes",
            "smoke_node_lanes",
            "smoke_rust_minimal_lanes",
            "smoke_rust_integration_lanes",
            "smoke_release_lanes",
            "workflow_lanes",
            "node_lanes",
            "rust_minimal_lanes",
            "rust_minimal_batches",
            "nextest_archives",
            "rust_integration_archive_lanes",
            "rust_integration_lanes",
            "rust_integration_batches",
            "release_lanes",
            "artifact",
        ]

        for job_name in fanout_jobs:
            with self.subTest(job=job_name):
                self.assertIn(
                    "needs.metadata.outputs.dedupe_should_skip != 'true'",
                    (jobs.get(job_name) or {}).get("if") or "",
                )
        self.assertNotIn(
            "dedupe_should_skip != 'true'",
            (jobs.get("summary") or {}).get("if") or "",
        )

    def test_validation_lab_nextest_archive_jobs_build_and_download_artifacts(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        jobs = payload.get("jobs") or {}
        metadata_outputs = (jobs.get("metadata") or {}).get("outputs") or {}

        self.assertEqual(
            metadata_outputs.get("selected_nextest_archive_matrix"),
            "${{ steps.meta.outputs.selected_nextest_archive_matrix }}",
        )
        self.assertEqual(
            metadata_outputs.get("selected_rust_integration_archive_matrix"),
            "${{ steps.meta.outputs.selected_rust_integration_archive_matrix }}",
        )

        archive_job = jobs.get("nextest_archives") or {}
        self.assertEqual(archive_job.get("needs"), ["metadata"])
        self.assertIn(
            "needs.metadata.outputs.selected_nextest_archive_count",
            archive_job.get("if") or "",
        )
        archive_steps = archive_job.get("steps") or []
        build_step = next(
            step for step in archive_steps if step.get("name") == "Build nextest archive"
        )
        self.assertIn("run_validation_lane.py", build_step.get("run") or "")
        self.assertEqual(
            (build_step.get("env") or {}).get("VALIDATION_LAB_NEXTEST_ARCHIVE_FILE"),
            "${{ runner.temp }}/validation-lab-nextest-archives/${{ matrix.archive_file_name }}",
        )
        upload_step = next(
            step for step in archive_steps if step.get("name") == "Upload nextest archive"
        )
        self.assertEqual(upload_step.get("uses"), "actions/upload-artifact@v7")
        self.assertEqual((upload_step.get("with") or {}).get("name"), "${{ matrix.artifact_name }}")

        archive_lanes = jobs.get("rust_integration_archive_lanes") or {}
        self.assertEqual(archive_lanes.get("needs"), ["metadata", "nextest_archives"])
        self.assertIn("needs.nextest_archives.result == 'success'", archive_lanes.get("if") or "")
        self.assertEqual(
            ((archive_lanes.get("with") or {}).get("nextest_archive_artifact_name")),
            "${{ matrix.nextest_archive_artifact_name }}",
        )
        self.assertEqual(
            ((archive_lanes.get("with") or {}).get("nextest_archive_file_name")),
            "${{ matrix.nextest_archive_file_name }}",
        )
        self.assertNotIn(
            "nextest_archive_artifact_name",
            (jobs.get("rust_integration_lanes") or {}).get("with") or {},
        )

    def test_validation_lab_rust_integration_workflow_downloads_nextest_archive(self) -> None:
        payload = load_workflow_payload(
            REPO_ROOT / ".github/workflows/_validation-lane-rust-integration.yml"
        )
        workflow_call = ((payload.get("on") or {}).get("workflow_call") or {})
        inputs = workflow_call.get("inputs") or {}
        run_job = (payload.get("jobs") or {}).get("run") or {}
        steps = run_job.get("steps") or []

        self.assertIn("nextest_archive_artifact_name", inputs)
        self.assertIn("nextest_archive_file_name", inputs)
        download_step = next(
            step for step in steps if step.get("name") == "Download nextest archive"
        )
        self.assertEqual(download_step.get("uses"), "actions/download-artifact@v8")
        self.assertEqual(
            (download_step.get("with") or {}).get("name"),
            "${{ inputs.nextest_archive_artifact_name }}",
        )
        export_step = next(
            step for step in steps if step.get("name") == "Export nextest archive path"
        )
        self.assertIn("VALIDATION_LAB_NEXTEST_ARCHIVE_FILE", export_step.get("run") or "")

        summary_step = next(
            step for step in steps if step.get("name") == "Prepare lane summary artifact"
        )
        summary_run = summary_step.get("run") or ""
        self.assertIn("--nextest-archive-artifact-name", summary_run)
        self.assertIn("--nextest-archive-file-name", summary_run)
        self.assertIn("--nextest-archive-mode", summary_run)

    def test_validation_lab_summary_records_plan_dedupe_fields(self) -> None:
        summary_step = workflow_step_by_name(
            REPO_ROOT / ".github/workflows/validation-lab.yml",
            "summary",
            "Build validation summary artifact",
        )
        run_script = summary_step.get("run") or ""

        self.assertIn(
            '--planner-fingerprint "${{ needs.metadata.outputs.planner_fingerprint }}"',
            run_script,
        )
        self.assertIn(
            '--dedupe-should-skip "${{ needs.metadata.outputs.dedupe_should_skip }}"',
            run_script,
        )
        self.assertIn(
            '--dedupe-matched-run-url "${{ needs.metadata.outputs.dedupe_matched_run_url }}"',
            run_script,
        )
        self.assertIn(
            '--latest-head-sha "${{ needs.metadata.outputs.head_sha }}"',
            run_script,
        )

    def test_sedna_heavy_tests_uses_safe_ref_env_and_requested_lane_inputs(self) -> None:
        metadata_step = workflow_step_by_name(
            REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml",
            "metadata",
            "Compute checkout ref",
        )
        env = metadata_step.get("env") or {}
        self.assertEqual(env.get("CHECKOUT_REF"), "${{ github.sha }}")
        self.assertEqual(env.get("DISPLAY_REF"), "${{ github.ref_name }}")
        self.assertEqual(env.get("INPUT_REF"), "${{ inputs.ref }}")
        self.assertEqual(env.get("PR_HEAD_SHA"), "${{ github.event.pull_request.head.sha }}")
        self.assertEqual(env.get("PR_HEAD_REF"), "${{ github.event.pull_request.head.ref }}")
        self.assertEqual(env.get("REQUESTED_LANE"), "${{ inputs.lane }}")
        self.assertEqual(env.get("INPUT_RUST_BATCHING"), "${{ inputs.rust_batching || 'auto' }}")
        self.assertEqual(
            env.get("INPUT_RUST_BATCHING_OVERRIDE"),
            "${{ vars.SEDNA_HEAVY_RUST_BATCHING }}",
        )
        run_script = metadata_step.get("run") or ""
        self.assertIn('checkout_ref="${CHECKOUT_REF}"', run_script)
        self.assertIn('checkout_ref="${PR_HEAD_SHA}"', run_script)
        self.assertIn('display_ref="${DISPLAY_REF}"', run_script)
        self.assertIn('--requested-lane "${REQUESTED_LANE}"', run_script)
        self.assertIn('--rust-batching "${INPUT_RUST_BATCHING}"', run_script)
        self.assertIn('--rust-batching-override "${INPUT_RUST_BATCHING_OVERRIDE}"', run_script)
        self.assertIn('os.environ["REQUESTED_LANE"]', run_script)
        self.assertNotIn('"requested_lane": "${{ inputs.lane }}"', run_script)

    def test_rust_ci_full_nextest_platform_uses_versioned_tool_syntax(self) -> None:
        payload = load_workflow_payload(
            REPO_ROOT / ".github/workflows/rust-ci-full-nextest-platform.yml"
        )
        archive_env = ((payload.get("jobs") or {}).get("archive") or {}).get("env") or {}
        self.assertEqual(archive_env.get("CARGO_PROFILE_CI_TEST_DEBUG"), "0")
        self.assertEqual(archive_env.get("CARGO_PROFILE_CI_TEST_STRIP"), "symbols")

        tool_values: list[str] = []
        for job in (payload.get("jobs") or {}).values():
            for step in (job or {}).get("steps") or []:
                if step.get("uses") != "taiki-e/install-action@065d6a08a14e61e89fb0a4c10eecdbdef39c7d8e":
                    continue
                with_section = step.get("with") or {}
                self.assertNotIn("version", with_section)
                tool_values.append(with_section.get("tool"))

        self.assertEqual(
            len(tool_values),
            3,
        )
        self.assertCountEqual(
            tool_values,
            ["sccache@0.7.5", "nextest@0.9.103", "nextest@0.9.103"],
        )
        workflow_text = (
            REPO_ROOT / ".github/workflows/rust-ci-full-nextest-platform.yml"
        ).read_text(encoding="utf-8")
        self.assertNotIn("run_id }}-${{ matrix.shard", workflow_text)
        self.assertNotIn("remote-env-target-${{ matrix.shard", workflow_text)
        self.assertNotIn('hash:${{ matrix.shard }}/4', workflow_text)

    def test_rust_ci_full_nextest_platform_keeps_inputs_out_of_shell_source(self) -> None:
        payload = load_workflow_payload(
            REPO_ROOT / ".github/workflows/rust-ci-full-nextest-platform.yml"
        )
        jobs = payload.get("jobs") or {}

        self.assertEqual(
            (jobs["archive"].get("env") or {}).get("INPUT_TARGET"),
            "${{ inputs.target }}",
        )
        self.assertEqual(
            (jobs["archive"].get("env") or {}).get("INPUT_PROFILE"),
            "${{ inputs.profile }}",
        )
        self.assertEqual(
            (jobs["shard"].get("env") or {}).get("INPUT_TEST_THREADS"),
            "${{ inputs.test_threads }}",
        )

        unsafe_expressions = (
            "${{ inputs.target }}",
            "${{ inputs.profile }}",
            "${{ inputs.test_threads }}",
        )
        for job_name, job in jobs.items():
            for step in (job or {}).get("steps") or []:
                run_script = step.get("run") or ""
                step_name = step.get("name") or step.get("id")
                for expression in unsafe_expressions:
                    message = (
                        f"{job_name}/{step_name} interpolates {expression} into shell source"
                    )
                    self.assertNotIn(
                        expression,
                        run_script,
                        message,
                    )

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

    def test_stack_sensitive_targeted_recipes_set_rust_min_stack(self) -> None:
        recipes = just_recipe_bodies(REPO_ROOT / "justfile")

        multi_agent_recipe = "\n".join(
            recipes["core-multi-agent-orchestration-targeted"]
        )
        self.assertEqual(multi_agent_recipe.count("RUST_MIN_STACK="), 4)

        blocking_waits_core_recipe = "\n".join(
            recipes["blocking-waits-core-targeted"]
        )
        self.assertEqual(blocking_waits_core_recipe.count("RUST_MIN_STACK="), 1)

        unified_exec_lines = recipes["blocking-waits-unified-exec-targeted"]
        unified_exec_cargo_lines = [
            line for line in unified_exec_lines if "cargo " in line
        ]
        self.assertTrue(unified_exec_cargo_lines)
        self.assertEqual(
            [
                line
                for line in unified_exec_cargo_lines
                if "RUST_MIN_STACK=" not in line
            ],
            [],
        )

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
                "codex.cli-surface-targeted",
                "codex.exec-native-computer-use-targeted",
                "codex.external-agent-session-migration-targeted",
                "codex.inference-observation-contract-targeted",
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
                "codex.tui-native-computer-use-targeted",
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
            "rust_minimal_batches",
            "rust_integration_lanes",
            "rust_integration_batches",
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

    def test_validation_lab_passes_timeout_to_workflow_lanes(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/validation-lab.yml")
        jobs = payload.get("jobs") or {}

        for job_name in ["smoke_workflow_lanes", "workflow_lanes"]:
            with self.subTest(job=job_name):
                self.assertEqual(
                    ((jobs.get(job_name) or {}).get("with") or {}).get("timeout_minutes"),
                    "${{ matrix.timeout_minutes }}",
                )

        for job_name in ["smoke_node_lanes", "node_lanes"]:
            with self.subTest(job=job_name):
                self.assertNotIn("timeout_minutes", (jobs.get(job_name) or {}).get("with") or {})

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

    def test_sedna_heavy_passes_timeout_to_workflow_lanes(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml")
        jobs = payload.get("jobs") or {}

        for job_name in ["smoke_workflow_lanes", "workflow_lanes"]:
            with self.subTest(job=job_name):
                self.assertEqual(
                    ((jobs.get(job_name) or {}).get("with") or {}).get("timeout_minutes"),
                    "${{ matrix.timeout_minutes }}",
                )

        for job_name in ["smoke_node_lanes", "node_lanes"]:
            with self.subTest(job=job_name):
                self.assertNotIn("timeout_minutes", (jobs.get(job_name) or {}).get("with") or {})

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
                    if step.get("uses") == "actions/checkout@v7"
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
                workflow_src_prefix = (
                    "../.workflow-src"
                    if workflow_name == "_sedna-linux-rust.yml"
                    else ".workflow-src"
                )
                self.assertEqual(
                    configure_step.get("run"),
                    f"bash {workflow_src_prefix}/.github/scripts/configure_sccache_backend.sh '${{{{ inputs.cache_policy }}}}'",
                )

                save_step = next(
                    step
                    for step in run_job.get("steps") or []
                    if step.get("name") == "Save sccache cache (fallback)"
                )
                self.assertIn(
                    "steps.sccache_backend.outputs.policy == 'write-fallback'",
                    save_step.get("if") or "",
                )

    def test_rust_batch_workflow_reclaims_disk_and_uses_small_debug_profiles(self) -> None:
        payload = load_workflow_payload(
            REPO_ROOT / ".github/workflows/_validation-lane-rust-batch.yml"
        )
        run_job = (payload.get("jobs") or {}).get("run") or {}
        env = run_job.get("env") or {}

        self.assertEqual(env.get("CARGO_PROFILE_DEV_DEBUG"), "0")
        self.assertEqual(env.get("CARGO_PROFILE_DEV_STRIP"), "symbols")
        self.assertEqual(env.get("CARGO_PROFILE_TEST_DEBUG"), "0")
        self.assertEqual(env.get("CARGO_PROFILE_TEST_STRIP"), "symbols")

        cleanup_step = next(
            step
            for step in run_job.get("steps") or []
            if step.get("name") == "Reclaim runner disk headroom"
        )
        cleanup_script = cleanup_step.get("run") or ""
        self.assertIn("/usr/local/lib/android", cleanup_script)
        self.assertIn("/usr/share/dotnet", cleanup_script)
        self.assertIn("/opt/ghc", cleanup_script)
        self.assertIn("/usr/local/share/boost", cleanup_script)
        self.assertIn("/opt/hostedtoolcache/CodeQL", cleanup_script)
        self.assertIn("12 GiB safety floor", cleanup_script)

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
                    if step.get("uses") == "actions/checkout@v7"
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

    def test_sync_models_json_splits_read_check_from_write_pr_creation(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sync-models-json.yml")
        jobs = payload.get("jobs") or {}
        check_job = jobs.get("check") or {}
        create_pr_job = jobs.get("create_pr") or {}

        self.assertEqual(payload.get("permissions"), {"contents": "read"})
        self.assertEqual(check_job.get("permissions"), {"contents": "read"})
        self.assertEqual(
            create_pr_job.get("permissions"),
            {"contents": "write", "pull-requests": "write"},
        )
        self.assertEqual(create_pr_job.get("needs"), "check")
        self.assertEqual(create_pr_job.get("if"), "needs.check.outputs.changed == 'true'")
        self.assertEqual(
            (check_job.get("outputs") or {}).get("changed"),
            "${{ steps.diff.outputs.changed }}",
        )
        self.assertEqual(
            (check_job.get("outputs") or {}).get("upstream_short_sha"),
            "${{ steps.upstream.outputs.upstream_short_sha }}",
        )

        check_steps = check_job.get("steps") or []
        upload_step = next(
            step for step in check_steps if step.get("name") == "Upload sync payload"
        )
        self.assertEqual(upload_step.get("if"), "steps.diff.outputs.changed == 'true'")
        self.assertEqual(upload_step.get("uses"), "actions/upload-artifact@v7")

        create_steps = create_pr_job.get("steps") or []
        download_step = next(
            step for step in create_steps if step.get("name") == "Download sync payload"
        )
        self.assertEqual(download_step.get("uses"), "actions/download-artifact@v8")
        create_step = next(step for step in create_steps if step.get("name") == "Create PR")
        self.assertIn(
            "needs.check.outputs.upstream_short_sha",
            (create_step.get("with") or {}).get("branch", ""),
        )
        self.assertEqual(
            (create_step.get("with") or {}).get("body-path"),
            "sync-models-json-update/summary.md",
        )

    def test_codeql_advanced_workflow_is_authoritative_hardened_setup(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/codeql.yml")
        trigger = payload.get("on") or {}
        jobs = payload.get("jobs") or {}
        analyze_job = jobs.get("analyze") or {}
        results_job = jobs.get("results") or {}
        steps = analyze_job.get("steps") or []

        self.assertIn("workflow_dispatch", trigger)
        self.assertEqual(trigger.get("merge_group"), {"types": ["checks_requested"]})
        self.assertEqual(payload.get("permissions"), {"contents": "read"})
        concurrency = payload.get("concurrency") or {}
        concurrency_group = str(concurrency.get("group") or "")
        self.assertIn("concurrency-group::${{ github.workflow }}::", concurrency_group)
        self.assertIn("format('merge-group-{0}', github.sha)", concurrency_group)
        self.assertIn("format('pr-{0}', github.event.pull_request.number)", concurrency_group)
        self.assertIn("format('push-{0}', github.sha)", concurrency_group)
        self.assertIn("format('{0}-{1}', github.event_name, github.run_id)", concurrency_group)
        self.assertEqual(
            concurrency.get("cancel-in-progress"),
            "${{ github.event_name == 'pull_request' }}",
        )
        self.assertNotIn("plan", jobs)
        self.assertNotIn("needs", analyze_job)
        self.assertNotIn("if", analyze_job)
        self.assertEqual(analyze_job.get("runs-on"), "ubuntu-24.04")
        self.assertEqual(
            analyze_job.get("permissions") or {},
            {
                "actions": "read",
                "contents": "read",
                "packages": "read",
                "security-events": "write",
            },
        )

        self.assertEqual(
            ((analyze_job.get("strategy") or {}).get("matrix")),
            {
                "include": [
                    {
                        "language": "actions",
                        "build-mode": "none",
                        "config_file": "./.codeql-runtime/codeql-actions.yml",
                    },
                    {
                        "language": "c-cpp",
                        "build-mode": "none",
                        "config_file": "./.github/codeql/codeql-config.yml",
                    },
                    {
                        "language": "javascript-typescript",
                        "build-mode": "none",
                        "config_file": "./.github/codeql/codeql-config.yml",
                    },
                    {
                        "language": "python",
                        "build-mode": "none",
                        "config_file": "./.github/codeql/codeql-config.yml",
                    },
                    {
                        "language": "rust",
                        "build-mode": "none",
                        "config_file": "./.github/codeql/codeql-rust.yml",
                    },
                ]
            },
        )

        workflow_json = json.dumps(payload, sort_keys=True)
        self.assertNotIn("autobuild", workflow_json)
        self.assertNotIn("manual", workflow_json)
        self.assertNotIn("resolve_codeql_plan", workflow_json)
        self.assertNotIn("has_codeql_relevant_changes", workflow_json)
        self.assertNotIn("run_all_languages", workflow_json)
        self.assertNotIn("fromJSON(needs.plan.outputs.matrix)", workflow_json)

        checkout_step = next(step for step in steps if step.get("name") == "Checkout repository")
        self.assertEqual(checkout_step.get("uses"), "actions/checkout@v7")
        self.assertEqual((checkout_step.get("with") or {}).get("persist-credentials"), "false")

        install_rust_step = next(
            step for step in steps if step.get("name") == "Install Rust toolchains for CodeQL"
        )
        self.assertEqual(install_rust_step.get("if"), "${{ matrix.language == 'rust' }}")
        install_rust_run = install_rust_step.get("run") or ""
        self.assertIn("rust-toolchain*", install_rust_run)
        self.assertIn('"--component"', install_rust_run)
        self.assertIn('"rust-src"', install_rust_run)
        self.assertNotIn("toolchain.get(\"components\"", install_rust_run)
        self.assertNotIn('"clippy"', install_rust_run)
        self.assertNotIn('"rustfmt"', install_rust_run)
        self.assertNotIn('"rustc-dev"', install_rust_run)
        self.assertNotIn('"llvm-tools-preview"', install_rust_run)
        self.assertIn("subprocess.run(command, check=True)", install_rust_run)

        restore_rust_cache_step = next(
            step for step in steps if step.get("name") == "Restore Rust dependency cache for CodeQL"
        )
        self.assertEqual(restore_rust_cache_step.get("if"), "${{ matrix.language == 'rust' }}")
        self.assertEqual(restore_rust_cache_step.get("uses"), "actions/cache/restore@v6")
        restore_cache_with = restore_rust_cache_step.get("with") or {}
        self.assertIn("~/.cargo/registry/cache/", restore_cache_with.get("path") or "")
        self.assertIn("~/.cargo/git/db/", restore_cache_with.get("path") or "")
        self.assertIn("codeql-rust-cargo-home-v1-", restore_cache_with.get("key") or "")
        workflow_json = json.dumps(payload, sort_keys=True)
        self.assertNotIn("~/.rustup/toolchains", workflow_json)

        telemetry_step = next(
            step for step in steps if step.get("name") == "Record Rust cache telemetry for CodeQL"
        )
        self.assertEqual(telemetry_step.get("if"), "${{ matrix.language == 'rust' }}")
        telemetry_run = telemetry_step.get("run") or ""
        self.assertIn("CodeQL Rust cache telemetry", telemetry_run)
        self.assertIn("cache_codeql_rust_cargo_home_restore.outputs.cache-hit", telemetry_run)

        prefetch_rust_step = next(
            step for step in steps if step.get("name") == "Prefetch Rust dependencies for CodeQL"
        )
        self.assertEqual(prefetch_rust_step.get("if"), "${{ matrix.language == 'rust' }}")
        self.assertEqual(prefetch_rust_step.get("continue-on-error"), "true")
        prefetch_run = prefetch_rust_step.get("run") or ""
        self.assertIn("cargo fetch --locked --manifest-path codex-rs/Cargo.toml", prefetch_run)
        self.assertIn(
            "cargo fetch --locked --manifest-path tools/argument-comment-lint/Cargo.toml",
            prefetch_run,
        )

        actions_config_step = next(
            step for step in steps if step.get("name") == "Prepare Actions CodeQL query pack config"
        )
        self.assertEqual(actions_config_step.get("if"), "${{ matrix.language == 'actions' }}")
        actions_config_run = actions_config_step.get("run") or ""
        self.assertIn(".github/codeql/actions-workflow-security", actions_config_run)
        self.assertIn("github.event.pull_request.head.repo.full_name", actions_config_run)
        self.assertIn("github.repository", actions_config_run)
        self.assertIn("github.event.pull_request.base.sha", actions_config_run)
        self.assertIn(".codeql-runtime/trusted-base", actions_config_run)
        self.assertIn("security-and-quality", actions_config_run)
        self.assertIn(".codeql-runtime/codeql-actions.yml", actions_config_run)

        init_step = next(step for step in steps if step.get("name") == "Initialize CodeQL")
        self.assertEqual(init_step.get("uses"), "github/codeql-action/init@v4.37.3")
        self.assertEqual(
            init_step.get("with") or {},
            {
                "languages": "${{ matrix.language }}",
                "build-mode": "${{ matrix.build-mode }}",
                "config-file": "${{ matrix.config_file }}",
                "dependency-caching": "${{ github.event_name == 'pull_request' && 'restore' || 'full' }}",
            },
        )

        diff_ranges_step = next(
            step
            for step in steps
            if step.get("name")
            == "Restore complete CodeQL diff ranges for large pull requests"
        )
        self.assertEqual(
            diff_ranges_step.get("if"),
            "${{ github.event_name == 'pull_request' && github.event.pull_request.changed_files >= 300 }}",
        )
        self.assertEqual(
            (diff_ranges_step.get("env") or {}).get("BASE_SHA"),
            "${{ github.event.pull_request.base.sha }}",
        )
        diff_ranges_run = diff_ranges_step.get("run") or ""
        self.assertIn("prepare_codeql_diff_ranges.py", diff_ranges_run)
        self.assertIn('> "${RUNNER_TEMP}/pr-diff-range.json"', diff_ranges_run)
        self.assertNotIn("--output", diff_ranges_run)
        self.assertLess(steps.index(init_step), steps.index(diff_ranges_step))
        analyze_step = next(step for step in steps if step.get("name") == "Perform CodeQL Analysis")
        self.assertLess(steps.index(diff_ranges_step), steps.index(analyze_step))

        save_rust_cache_step = next(
            step for step in steps if step.get("name") == "Save Rust dependency cache for CodeQL"
        )
        self.assertEqual(save_rust_cache_step.get("continue-on-error"), "true")
        self.assertEqual(save_rust_cache_step.get("uses"), "actions/cache/save@v6")
        self.assertIn("matrix.language == 'rust'", save_rust_cache_step.get("if") or "")
        self.assertIn("github.event_name != 'pull_request'", save_rust_cache_step.get("if") or "")
        self.assertIn("refs/heads/main", save_rust_cache_step.get("if") or "")
        self.assertIn("refs/heads/upstream-main", save_rust_cache_step.get("if") or "")
        self.assertNotIn("target/", (save_rust_cache_step.get("with") or {}).get("path") or "")

        self.assertEqual(results_job.get("name"), "CodeQL required gate")
        self.assertEqual(results_job.get("needs"), ["analyze"])
        self.assertEqual(results_job.get("if"), "always()")
        self.assertEqual(results_job.get("permissions") or {}, {"actions": "read"})
        timing_step = next(
            step for step in results_job.get("steps") or [] if step.get("name") == "Report CodeQL timing"
        )
        timing_run = timing_step.get("run") or ""
        self.assertIn("CodeQL timing", timing_run)
        self.assertIn("actions/runs/${{ github.run_id }}/jobs", timing_run)
        self.assertIn("Analyze \\\\(", timing_run)
        results_run = "\n".join(step.get("run") or "" for step in results_job.get("steps") or [])
        self.assertIn("CodeQL analysis failed", results_run)

        config = yaml.load(
            (REPO_ROOT / ".github/codeql/codeql-config.yml").read_text(encoding="utf-8"),
            Loader=yaml.BaseLoader,
        )
        self.assertEqual(
            config,
            {
                "name": "codex-codeql-advanced",
                "queries": [{"uses": "security-and-quality"}],
                "threat-models": "local",
            },
        )
        actions_config = yaml.load(
            (REPO_ROOT / ".github/codeql/codeql-actions.yml").read_text(encoding="utf-8"),
            Loader=yaml.BaseLoader,
        )
        self.assertEqual(
            actions_config,
            {
                "name": "codex-codeql-actions",
                "queries": [
                    {"uses": "security-and-quality"},
                    {"uses": "./.github/codeql/actions-workflow-security"},
                ],
                "threat-models": "local",
            },
        )
        actions_pack = yaml.load(
            (REPO_ROOT / ".github/codeql/actions-workflow-security/qlpack.yml").read_text(
                encoding="utf-8"
            ),
            Loader=yaml.BaseLoader,
        )
        self.assertEqual(actions_pack.get("name"), "sednalabs/actions-workflow-security")
        self.assertEqual(actions_pack.get("extractor"), "actions")
        self.assertEqual(actions_pack.get("dependencies"), {"codeql/actions-all": "*"})
        self.assertEqual(actions_pack.get("defaultSuiteFile"), "suites/actions-workflow-security.qls")
        self.assertIn(
            "@id actions/sensitive-workflow-value-to-log",
            (REPO_ROOT / ".github/codeql/actions-workflow-security/SensitiveWorkflowValueToLog.ql")
            .read_text(encoding="utf-8"),
        )
        self.assertIn(
            "@id actions/sensitive-workflow-value-to-verbose-tool",
            (REPO_ROOT / ".github/codeql/actions-workflow-security/SensitiveWorkflowValueToVerboseTool.ql")
            .read_text(encoding="utf-8"),
        )
        for query_id in [
            "actions/unsafe-release-publishing-path",
            "actions/release-publisher-with-untrusted-input",
            "actions/overbroad-workflow-permissions",
            "actions/write-token-on-untrusted-trigger",
            "actions/artifact-published-without-provenance",
            "actions/repo-security-invariant-violation",
        ]:
            self.assertTrue(
                any(
                    f"@id {query_id}" in path.read_text(encoding="utf-8")
                    for path in (REPO_ROOT / ".github/codeql/actions-workflow-security").glob("*.ql")
                ),
                query_id,
            )
        self.assertIn(
            "getAWriteToGitHubEnv",
            (REPO_ROOT / ".github/codeql/actions-workflow-security/LogExposure.qll").read_text(
                encoding="utf-8"
            ),
        )
        workflow_security = (
            REPO_ROOT / ".github/codeql/actions-workflow-security/WorkflowSecurity.qll"
        ).read_text(encoding="utf-8")
        self.assertIn("jobHasPublishingSink", workflow_security)
        self.assertIn("jobEffectiveWritePermission", workflow_security)
        self.assertIn("jobHasRepoApprovedProvenance", workflow_security)
        self.assertIn(
            'jobUsesActionMatching(job, "(?i)^actions/github-script(@.*)?$", _)',
            workflow_security,
        )
        self.assertFalse((REPO_ROOT / ".github/codeql/codeql-rust-pr.yml").exists())
        rust_config = yaml.load(
            (REPO_ROOT / ".github/codeql/codeql-rust.yml").read_text(encoding="utf-8"),
            Loader=yaml.BaseLoader,
        )
        self.assertEqual(
            rust_config,
            {
                "name": "codex-codeql-rust",
                "queries": [
                    {"uses": "security-and-quality"},
                ],
                "paths": ["codex-rs", "tools"],
                "paths-ignore": [".github/codeql/rust-computer-use-contract/test/**"],
                "threat-models": "local",
            },
        )
        rust_contract_pack = yaml.load(
            (REPO_ROOT / ".github/codeql/rust-computer-use-contract/qlpack.yml").read_text(
                encoding="utf-8"
            ),
            Loader=yaml.BaseLoader,
        )
        self.assertEqual(rust_contract_pack.get("name"), "sednalabs/rust-computer-use-contract")
        self.assertEqual(rust_contract_pack.get("extractor"), "rust")
        self.assertEqual(rust_contract_pack.get("dependencies"), {"codeql/rust-all": "*"})
        for query_id in [
            "rust/computer-use-match-drops-native-image",
            "rust/android-visual-tool-missing-native-image-guard",
            "rust/computer-use-response-success-with-error",
        ]:
            self.assertTrue(
                any(
                    f"@id {query_id}" in path.read_text(encoding="utf-8")
                    for path in (REPO_ROOT / ".github/codeql/rust-computer-use-contract/queries").glob("*.ql")
                ),
                query_id,
            )

    def test_closed_pr_run_canceller_preserves_post_merge_branch_runs(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/cancel-pr-runs.yml")
        trigger = payload.get("on") or {}
        job = ((payload.get("jobs") or {}).get("cancel") or {})
        steps = job.get("steps") or []

        self.assertEqual(
            ((trigger.get("pull_request_target") or {}).get("types") or []),
            ["closed"],
        )
        self.assertEqual(payload.get("permissions"), {"actions": "write", "contents": "read"})
        self.assertEqual(job.get("permissions") or {}, {})
        self.assertEqual(job.get("runs-on"), "ubuntu-latest")

        workflow_json = json.dumps(payload, sort_keys=True)
        self.assertNotIn("actions/checkout", workflow_json)
        self.assertNotIn("github.event.pull_request.head.repo.clone_url", workflow_json)

        cancel_step = next(
            step for step in steps if step.get("name") == "Cancel stale runs for the closed PR"
        )
        self.assertEqual(cancel_step.get("uses"), "actions/github-script@v9.0.0")
        script = (cancel_step.get("with") or {}).get("script") or ""

        self.assertIn("github.rest.actions.listWorkflowRunsForRepo", script)
        self.assertIn("event: 'pull_request'", script)
        self.assertIn("run.pull_requests.some((pr) => pr.number === prNumber)", script)
        self.assertIn("github.rest.actions.cancelWorkflowRun", script)
        self.assertIn("headRepo === currentRepo", script)
        self.assertIn("!protectedBranches.has(headBranch)", script)
        self.assertIn("'main'", script)
        self.assertIn("'upstream-main'", script)
        self.assertIn("mayCancelHeadPushRuns &&", script)
        self.assertIn("Post-merge push runs on ${baseBranch}", script)

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

    def test_sedna_sync_upstream_keeps_audit_in_separate_read_only_job(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-sync-upstream.yml")
        jobs = payload.get("jobs") or {}
        sync_job = jobs.get("sync") or {}
        audit_job = jobs.get("audit") or {}

        self.assertEqual(payload.get("permissions"), {"contents": "read"})
        self.assertEqual(audit_job.get("needs"), "sync")
        audit_job_json = json.dumps(audit_job, sort_keys=True)
        self.assertNotIn("secrets.", audit_job_json)
        self.assertNotIn("SYNC_UPSTREAM_APP_TOKEN", audit_job_json)
        self.assertNotIn("SYNC_UPSTREAM_LEGACY_TOKEN", audit_job_json)
        self.assertIn("Generate upstream sync app token", json.dumps(sync_job, sort_keys=True))

    def test_rust_ci_full_fallback_sccache_writes_are_disabled_by_default(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci-full.yml")
        jobs = payload.get("jobs") or {}

        for job_name in ["lint_build"]:
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
                configure_step = next(
                    step
                    for step in job.get("steps") or []
                    if step.get("name") == "Configure sccache backend"
                )
                self.assertIn(
                    "${{ github.workspace }}/.github/scripts/configure_sccache_backend.sh",
                    configure_step.get("run") or "",
                )

                for step_name in [
                    "Summarize clippy failure",
                    "Summarize nextest failures",
                ]:
                    matching_steps = [
                        step for step in job.get("steps") or [] if step.get("name") == step_name
                    ]
                    for step in matching_steps:
                        self.assertIn(
                            "${{ github.workspace }}/.github/scripts/summarize_rust_ci_full.py",
                            step.get("run") or "",
                        )

        archive_job = jobs.get("nextest_archive") or {}
        archive_env = archive_job.get("env") or {}
        self.assertEqual(archive_env.get("USE_SCCACHE"), "false")
        self.assertNotIn("SCCACHE_CACHE_SIZE", archive_env)
        archive_steps_json = json.dumps(archive_job.get("steps") or [], sort_keys=True)
        self.assertNotIn("RUSTC_WRAPPER=sccache", archive_steps_json)
        self.assertNotIn("Configure sccache backend", archive_steps_json)
        self.assertNotIn("cargo nextest run", archive_steps_json)

    def test_rust_ci_full_runs_after_successful_scheduled_rust_ci_only(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci-full.yml")
        trigger = payload.get("on") or {}
        jobs = payload.get("jobs") or {}
        schedule_gate = jobs.get("schedule_gate") or {}

        self.assertEqual(
            ((trigger.get("workflow_run") or {}).get("workflows") or []),
            ["rust-ci"],
        )
        self.assertNotIn("schedule", trigger)
        self.assertEqual(payload.get("permissions"), {"actions": "read", "contents": "read"})

        gate = schedule_gate.get("if") or ""
        self.assertIn("github.event.workflow_run.event == 'schedule'", gate)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", gate)
        self.assertIn("github.event.workflow_run.head_branch == 'main'", gate)

        self.assertEqual((jobs.get("matrix_plan") or {}).get("needs"), "schedule_gate")
        self.assertIn(
            "needs.schedule_gate.outputs.should_run == 'true'",
            (jobs.get("matrix_plan") or {}).get("if") or "",
        )
        dedupe_step = next(
            step
            for step in schedule_gate.get("steps") or []
            if step.get("name") == "Check duplicate scheduled rust-ci-full success"
        )
        dedupe_run = dedupe_step.get("run") or ""
        self.assertIn("skip_duplicate_workflow_run.py", dedupe_run)
        self.assertIn("--workflow rust-ci-full.yml", dedupe_run)
        self.assertIn("github.event.workflow_run.head_sha", dedupe_run)

        result_gate = (jobs.get("results") or {}).get("if") or ""
        self.assertIn("always()", result_gate)
        self.assertIn("github.event.workflow_run.event == 'schedule'", result_gate)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", result_gate)

    def test_rust_ci_schedule_reuses_equivalent_same_sha_success(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci.yml")
        self.assertEqual(
            payload.get("permissions"),
            {
                "actions": "read",
                "contents": "read",
                "checks": "read",
                "pull-requests": "read",
            },
        )
        jobs = payload.get("jobs") or {}
        changed = jobs.get("changed") or {}
        outputs = changed.get("outputs") or {}
        steps = changed.get("steps") or []

        self.assertEqual(
            outputs.get("scheduled_duplicate_skip"),
            "${{ steps.schedule_duplicate.outputs.should_skip || 'false' }}",
        )
        self.assertIn(
            "steps.scheduled_skip_plan.outputs.validation_mode",
            outputs.get("validation_mode") or "",
        )

        dedupe_step = next(
            step
            for step in steps
            if step.get("name") == "Check duplicate scheduled rust-ci success"
        )
        self.assertEqual(dedupe_step.get("if"), "${{ github.event_name == 'schedule' }}")
        dedupe_run = dedupe_step.get("run") or ""
        self.assertIn("skip_duplicate_workflow_run.py", dedupe_run)
        self.assertIn("--workflow rust-ci.yml", dedupe_run)
        self.assertIn("--head-sha \"${{ github.sha }}\"", dedupe_run)

        detect_step = next(
            step for step in steps if step.get("name") == "Detect changed paths and rust-ci mode"
        )
        self.assertIn(
            "steps.schedule_duplicate.outputs.should_skip != 'true'",
            detect_step.get("if") or "",
        )
        skip_step = next(
            step for step in steps if step.get("name") == "Emit duplicate scheduled skip plan"
        )
        self.assertEqual(
            skip_step.get("if"),
            "${{ steps.schedule_duplicate.outputs.should_skip == 'true' }}",
        )
        self.assertIn("validation_mode=scheduled_duplicate", skip_step.get("run") or "")

        results_run = (
            next(
                step
                for step in (jobs.get("results") or {}).get("steps") or []
                if step.get("name") == "Summarize"
            ).get("run")
            or ""
        )
        self.assertIn("scheduled_duplicate_skip", results_run)
        self.assertIn("Equivalent rust-ci run already passed", results_run)

    def test_rust_ci_argument_comment_lint_timeout_matches_lane_contract(self) -> None:
        rust_ci = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci.yml")
        rust_ci_full = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci-full.yml")

        plan_run = (
            (((rust_ci.get("jobs") or {}).get("matrix_plan") or {}).get("steps") or [])[0].get(
                "run"
            )
            or ""
        )
        self.assertIn('"timeout_minutes": 30', plan_run)

        rust_ci_full_job = (rust_ci_full.get("jobs") or {}).get(
            "argument_comment_lint_prebuilt"
        ) or {}
        self.assertEqual(rust_ci_full_job.get("timeout-minutes"), "240")

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
                "schedule_gate",
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
        self.assertIn(
            "${{ github.workspace }}/.github/scripts/summarize_rust_ci_full.py\" aggregate",
            aggregate_step.get("run") or "",
        )
        verify_step = next(step for step in steps if step.get("name") == "Verify full CI result")
        verify_run = verify_step.get("run") or ""
        self.assertIn("needs.schedule_gate.outputs.should_skip", verify_run)
        self.assertIn("Equivalent rust-ci-full run already passed", verify_run)
        self.assertIn("require_success \"nextest_archive\"", verify_run)
        self.assertIn("require_success \"remote_tests\"", verify_run)

    def test_rust_ci_full_archive_test_and_results_jobs_do_not_receive_secrets(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci-full.yml")
        jobs = payload.get("jobs") or {}

        for job_name in [
            "schedule_gate",
            "lint_build",
            "nextest_archive",
            "tests",
            "remote_tests",
            "results",
        ]:
            with self.subTest(job=job_name):
                job = jobs.get(job_name) or {}
                self.assertNotIn("secrets", job)
                self.assertNotIn("secrets.", json.dumps(job, sort_keys=True))
                self.assertNotIn("ACTIONS_RUNTIME_TOKEN", json.dumps(job, sort_keys=True))
                self.assertNotIn("SCCACHE_GHA_ENABLED=true", json.dumps(job, sort_keys=True))

    def test_rust_ci_full_nextest_archive_is_reused_by_test_families(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci-full.yml")
        jobs = payload.get("jobs") or {}

        archive_env = (jobs.get("nextest_archive") or {}).get("env") or {}
        self.assertEqual(archive_env.get("CARGO_PROFILE_CI_TEST_DEBUG"), "0")
        self.assertEqual(archive_env.get("CARGO_PROFILE_CI_TEST_STRIP"), "symbols")

        archive_steps = (jobs.get("nextest_archive") or {}).get("steps") or []
        disk_reclaim_step = next(
            step for step in archive_steps if step.get("name") == "Reclaim runner disk headroom"
        )
        self.assertEqual(disk_reclaim_step.get("if"), "${{ runner.os == 'Linux' }}")
        self.assertIn("/usr/share/dotnet", disk_reclaim_step.get("run") or "")
        self.assertIn("6 GiB safety floor", disk_reclaim_step.get("run") or "")

        archive_run = next(
            step for step in archive_steps if step.get("name") == "Build nextest archive"
        ).get("run") or ""
        self.assertIn("cargo nextest archive", archive_run)
        self.assertIn("--archive-file", archive_run)
        self.assertNotIn("--all-features", archive_run)
        self.assertNotIn("cargo nextest run", archive_run)
        self.assertNotIn("tests", [step.get("name") for step in archive_steps])

        for job_name in ["tests", "remote_tests"]:
            with self.subTest(job=job_name):
                steps = (jobs.get(job_name) or {}).get("steps") or []
                install_step = next(
                    step
                    for step in steps
                    if step.get("name") == "Install Linux build dependencies"
                )
                self.assertIn("bubblewrap", install_step.get("run") or "")

                replay_disk_step = next(
                    step for step in steps if step.get("name") == "Reclaim runner disk headroom"
                )
                self.assertEqual(replay_disk_step.get("if"), "${{ runner.os == 'Linux' }}")
                self.assertIn("/usr/share/dotnet", replay_disk_step.get("run") or "")
                self.assertIn("6 GiB safety floor", replay_disk_step.get("run") or "")

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
        remote_setup_run = next(
            step
            for step in (jobs.get("remote_tests") or {}).get("steps") or []
            if step.get("name") == "Set up remote test env (Docker)"
        ).get("run") or ""
        self.assertIn("CODEX_TEST_REMOTE_ENV_CARGO_TARGET_DIR", remote_setup_run)
        remote_cleanup_run = next(
            step
            for step in (jobs.get("remote_tests") or {}).get("steps") or []
            if step.get("name") == "Reclaim remote env build artifacts"
        ).get("run") or ""
        self.assertIn("20 GiB extraction safety floor", remote_cleanup_run)

    def test_rust_ci_full_summary_parser_extracts_compact_blockers(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            nextest_log = root / "nextest.log"
            nextest_log.write_text(
                "\n".join(
                    [
                        "Starting 6 tests across 2 binaries (1 tests skipped)",
                        "        FAIL [   0.042s] (1/3) codex_core::remote_env::fails_cleanly",
                        "     TIMEOUT [  60.000s] (2/3) codex_core::remote_exec_server::hangs",
                        "   TRY 1 FAIL [   0.120s] codex_core::flaky_once",
                        "   TRY 2 PASS [   0.011s] codex_core::flaky_once",
                        "   TRY 1 FAIL [   0.220s] codex-core session::stable_failure",
                        "   TRY 2 FAIL [   0.230s] codex-core session::stable_failure",
                        "   TRY 1 TIMEOUT [  30.000s] codex-core remote::stable_timeout",
                        "   TRY 2 TIMEOUT [  30.001s] codex-core remote::stable_timeout",
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
                "started": {"tests": 6, "binaries": 2, "skipped": 1},
                "failure_signal_count": 4,
                "unique_failure_count": 4,
                "status_counts": {"FAIL": 2, "TIMEOUT": 2},
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
                    {
                        "status": "fail",
                        "duration": "0.230s",
                        "test": "codex-core session::stable_failure",
                    },
                    {
                        "status": "timeout",
                        "duration": "30.001s",
                        "test": "codex-core remote::stable_timeout",
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

    def test_lane_summary_does_not_treat_panic_crate_names_as_failures(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            log = root / "lane.log"
            output = root / "summary.json"
            log.write_text(
                "\n".join(
                    [
                        "Checking sentry-panic v0.46.2",
                        "error[E0277]: a real compiler failure",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "write_lane_summary.py"),
                    "--lane-id",
                    "codex.example",
                    "--summary-title",
                    "example",
                    "--outcome",
                    "failure",
                    "--log-file",
                    str(log),
                    "--output",
                    str(output),
                ],
                check=True,
            )

            summary = json.loads(output.read_text(encoding="utf-8"))

        self.assertNotIn("error_lines", summary)
        self.assertEqual(summary["primary_signal"], "error[E0277]: a real compiler failure")

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
                    '["blocking-waits-core-targeted"]',
                    "--cache-policy",
                    "restore-only",
                    "--cache-backend",
                    "gha",
                    "--sccache-restore-mode",
                    "not-applicable",
                    "--nextest-archive-artifact-name",
                    "validation-lab-nextest-core-carry-pilot",
                    "--nextest-archive-file-name",
                    "codex-core-carry-nextest.tar.zst",
                    "--nextest-archive-mode",
                    "downloaded",
                    "--output",
                    str(output),
                ],
                check=True,
            )

            summary = json.loads(output.read_text(encoding="utf-8"))

        self.assertEqual(
            summary["script_path"], ".github/scripts/validation-lanes/run-just-recipe.sh"
        )
        self.assertEqual(summary["script_args"], ["blocking-waits-core-targeted"])
        self.assertEqual(summary["cache_policy"], "restore-only")
        self.assertEqual(summary["cache_backend"], "gha")
        self.assertEqual(summary["sccache_restore_mode"], "not-applicable")
        self.assertEqual(
            summary["nextest_archive_artifact_name"],
            "validation-lab-nextest-core-carry-pilot",
        )
        self.assertEqual(summary["nextest_archive_file_name"], "codex-core-carry-nextest.tar.zst")
        self.assertEqual(summary["nextest_archive_mode"], "downloaded")
        self.assertNotIn("run_command", summary)

    def test_lane_summary_detects_server_notification_schema_fixture_drift(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            log = root / "lane.log"
            output = root / "summary.json"
            log.write_text(
                "\n".join(
                    [
                        "thread 'json_schema_fixtures_match_generated' panicked at tests/schema_fixtures.rs:98:9:",
                        "Vendored json app-server schema fixture ServerNotification.json differs from generated output. Run `just write-app-server-schema` to overwrite with your changes.",
                        "--- fixture",
                        "+++ generated",
                        '-        "threadGoalUpdated": {',
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "write_lane_summary.py"),
                    "--lane-id",
                    "codex.app-server-protocol-test",
                    "--summary-title",
                    "app-server protocol",
                    "--script-path",
                    ".github/scripts/validation-lanes/app-server-protocol-test.sh",
                    "--outcome",
                    "failure",
                    "--log-file",
                    str(log),
                    "--output",
                    str(output),
                ],
                check=True,
            )

            summary = json.loads(output.read_text(encoding="utf-8"))

        drift = summary["schema_fixture_drift"]
        self.assertEqual(drift["kind"], "app_server_schema_fixture_drift")
        self.assertEqual(drift["fixture_family"], "json")
        self.assertEqual(drift["fixture_path"], "ServerNotification.json")
        self.assertEqual(drift["direction"], "vendored_differs_from_generated")
        self.assertEqual(drift["recommended_fix"], "just write-app-server-schema")
        self.assertEqual(
            drift["recommended_proof"],
            {"profile": "targeted", "lane_ids": ["codex.app-server-protocol-test"]},
        )
        self.assertEqual(
            summary["primary_signal"],
            "json app-server schema fixture ServerNotification.json differs from generated output",
        )

    def test_aggregate_summary_surfaces_schema_fixture_drift_in_candidates(self) -> None:
        drift = {
            "kind": "app_server_schema_fixture_drift",
            "fixture_family": "json",
            "fixture_path": "ServerNotification.json",
            "direction": "vendored_differs_from_generated",
            "recommended_proof": {
                "profile": "targeted",
                "lane_ids": ["codex.app-server-protocol-test"],
            },
            "summary": "json app-server schema fixture ServerNotification.json differs from generated output",
        }
        results = AGGREGATE_VALIDATION_SUMMARY.build_results(
            planned_matrix=[
                {
                    "lane_id": "codex.app-server-protocol-test",
                    "setup_class": "rust_minimal",
                    "summary_family": "app-server-protocol",
                    "frontier_role": "sentinel",
                    "status_class": "active",
                }
            ],
            selected_lane_ids=["codex.app-server-protocol-test"],
            actual_by_lane={
                "codex.app-server-protocol-test": {
                    "lane_id": "codex.app-server-protocol-test",
                    "outcome": "failure",
                    "exit_code": 101,
                    "schema_fixture_drift": drift,
                }
            },
            smoke_gate_result="skipped",
            setup_class_results={"rust_minimal": "failure"},
            matrix_fail_fast=False,
        )
        setup_rows = AGGREGATE_VALIDATION_SUMMARY.setup_class_rows(
            results, {"rust_minimal": "failure"}
        )
        primary, secondary = AGGREGATE_VALIDATION_SUMMARY.derive_primary_and_secondary(
            results, setup_rows
        )
        queue = [*primary, *secondary]
        candidates = []
        for item in queue:
            candidates.append(
                {
                    "kind": "lane",
                    "lane_id": item["lane_id"],
                    "signal": item.get("signal", ""),
                    "schema_fixture_drift": item.get("schema_fixture_drift") or {},
                }
            )

        self.assertEqual(primary[0]["signal"], drift["summary"])
        self.assertEqual(primary[0]["schema_fixture_drift"], drift)
        self.assertEqual(candidates[0]["schema_fixture_drift"], drift)

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
        self.assertIn("codex.core-multi-agent-orchestration-targeted", selected_lane_ids)
        self.assertNotIn("codex.tui-agent-picker-model-surface-targeted", selected_lane_ids)
        self.assertEqual(payload["planned_job_count"], 40)
        self.assertEqual(payload["selected_workflow_lane_count"], 6)
        self.assertEqual(payload["selected_node_lane_count"], 2)
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 1)
        self.assertEqual(payload["selected_rust_minimal_batch_count"], 13)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 5)
        self.assertEqual(payload["selected_rust_integration_batch_count"], 12)
        self.assertEqual(payload["selected_release_lane_count"], 1)
        self.assertEqual(payload["workflow_max_parallel"], "6")
        self.assertEqual(payload["node_max_parallel"], "2")
        self.assertEqual(payload["rust_minimal_max_parallel"], "24")
        self.assertEqual(payload["rust_integration_max_parallel"], "24")
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
        self.assertIn("codex.core-multi-agent-orchestration-targeted", selected_lane_ids)
        self.assertEqual(payload["planned_job_count"], 43)
        self.assertEqual(payload["selected_workflow_lane_count"], 7)
        self.assertEqual(payload["selected_node_lane_count"], 2)
        self.assertEqual(payload["selected_rust_minimal_lane_count"], 1)
        self.assertEqual(payload["selected_rust_minimal_batch_count"], 14)
        self.assertEqual(payload["selected_rust_integration_lane_count"], 6)
        self.assertEqual(payload["selected_rust_integration_batch_count"], 12)
        self.assertEqual(payload["selected_release_lane_count"], 1)
        self.assertEqual(payload["rust_minimal_max_parallel"], "26")
        self.assertEqual(payload["rust_integration_max_parallel"], "25")

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
        self.assertEqual(payload["workflow_max_parallel"], "7")
        self.assertEqual(payload["node_max_parallel"], "2")
        self.assertEqual(payload["rust_minimal_max_parallel"], "20")
        self.assertEqual(payload["rust_integration_max_parallel"], "8")
        self.assertEqual(payload["release_max_parallel"], "1")
        planned_lane_ids = [lane["lane_id"] for lane in payload["planned_matrix"]["include"]]
        selected_lane_ids = payload["selected_lane_ids"]
        self.assertEqual(
            planned_lane_ids[:6],
            [
                "core-compile-smoke",
                "core-carry-core-smoke",
                "core-carry-ui-smoke",
                "core-ledger-smoke",
                "core-runtime-surface-smoke",
                "sedna.release-linux-smoke",
            ],
        )
        self.assertEqual(planned_lane_ids[6:], selected_lane_ids)
        self.assertIn("codex.core-startup-sync-targeted", selected_lane_ids)
        self.assertIn("codex.downstream-docs-check", selected_lane_ids)
        self.assertNotIn("codex.downstream-divergence-audit", selected_lane_ids)
        self.assertNotIn("sedna.release-linux-smoke", selected_lane_ids)
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
        self.assertEqual(payload["smoke_release_lane_count"], 1)
        self.assertEqual(payload["selected_release_lane_count"], 0)
        self.assertNotIn(
            "codex.nextest-archive-core-carry-pilot",
            payload["selected_lane_ids"],
        )
        self.assertNotIn("sedna.release-linux-smoke", payload["selected_lane_ids"])

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

    def test_validation_lab_explicit_nextest_archive_pilot_splits_archive_matrices(
        self,
    ) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "lab",
            "--profile",
            "targeted",
            "--lane-set",
            "core-carry",
            "--fanout-tier",
            "enterprise",
            "--lanes",
            "codex.nextest-archive-core-carry-pilot",
            "--rust-batching",
            "auto",
            "--artifact-build",
            "false",
            "--include-explicit-lanes",
            "true",
        )

        self.assertEqual(payload["selected_lane_ids"], ["codex.nextest-archive-core-carry-pilot"])
        self.assertEqual(payload["selected_rust_integration_lane_count"], 0)
        self.assertEqual(payload["selected_rust_integration_batch_count"], 0)
        self.assertEqual(payload["selected_nextest_archive_count"], 1)
        self.assertEqual(payload["selected_rust_integration_archive_lane_count"], 1)
        self.assertEqual(payload["planned_job_count"], 2)

        archive = payload["selected_nextest_archive_matrix"]["include"][0]
        self.assertEqual(archive["archive_cohort"], "core-carry-pilot")
        self.assertEqual(archive["artifact_name"], "validation-lab-nextest-core-carry-pilot")
        self.assertEqual(archive["archive_file_name"], "codex-core-carry-nextest.tar.zst")
        self.assertEqual(archive["lane_ids"], ["codex.nextest-archive-core-carry-pilot"])

        archive_lane = payload["selected_rust_integration_archive_matrix"]["include"][0]
        self.assertTrue(archive_lane["uses_nextest_archive"])
        self.assertEqual(
            archive_lane["nextest_archive_artifact_name"],
            "validation-lab-nextest-core-carry-pilot",
        )

    def test_validation_lab_frontier_does_not_silently_select_archive_pilot(self) -> None:
        payload = run_script(
            SCRIPTS_DIR / "resolve_validation_plan.py",
            "lab",
            "--profile",
            "frontier",
            "--lane-set",
            "core-carry",
            "--fanout-tier",
            "enterprise",
            "--lanes",
            "",
            "--rust-batching",
            "auto",
            "--artifact-build",
            "false",
            "--include-explicit-lanes",
            "true",
        )

        self.assertNotIn("codex.nextest-archive-core-carry-pilot", payload["selected_lane_ids"])
        self.assertEqual(payload["selected_nextest_archive_count"], 0)
        self.assertEqual(payload["selected_rust_integration_archive_lane_count"], 0)
        self.assertGreater(len(payload["selected_lane_ids"]), 0)

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

    def test_sedna_heavy_is_advisory_and_does_not_run_in_merge_queue(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-heavy-tests.yml")
        self.assertNotIn("merge_group", payload.get("on") or {})

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
        metadata_steps = (((payload.get("jobs") or {}).get("metadata") or {}).get("steps") or [])
        metadata_run = next(
            step for step in metadata_steps if step.get("name") == "Compute checkout ref"
        ).get("run") or ""

        self.assertEqual(
            metadata_outputs.get("planner_fingerprint"),
            "${{ steps.meta.outputs.planner_fingerprint }}",
        )
        self.assertEqual(
            metadata_outputs.get("dedupe_reason"),
            "${{ steps.meta.outputs.dedupe_reason }}",
        )
        self.assertIn(".ci_proof_v1.schema_version == \"ci-proof-v1\"", metadata_run)
        self.assertIn(".ci_proof_v1.planner_fingerprint == $planner", metadata_run)
        self.assertIn(".ci_proof_v1.conclusion == \"success\"", metadata_run)
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
        uses_steps = [step.get("uses") for step in steps]
        self.assertIn("actions/checkout@v7", uses_steps)
        self.assertIn("actions/download-artifact@v8", uses_steps)
        self.assertIn("actions/upload-artifact@v7", uses_steps)
        self.assertTrue(
            any(step.get("name") == "Record Actions cache occupancy" for step in steps)
        )
        report_step = next(
            (
                step
                for step in steps
                if "aggregate_validation_summary.py" in (step.get("run") or "")
            ),
            {},
        )
        self.assertIn(
            "aggregate_validation_summary.py",
            report_step.get("run") or "",
        )
        self.assertIn(
            '--planned-matrix-json \'${{ needs.metadata.outputs.planned_matrix }}\'',
            report_step.get("run") or "",
        )
        self.assertIn(
            "--cache-occupancy-json",
            report_step.get("run") or "",
        )
        self.assertIn(
            '--head-sha "${{ needs.metadata.outputs.checkout_sha }}"',
            report_step.get("run") or "",
        )
        self.assertIn(
            '--latest-head-sha "${{ needs.metadata.outputs.checkout_sha }}"',
            report_step.get("run") or "",
        )
        self.assertIn(
            '--workflow-file "sedna-heavy-tests.yml"',
            report_step.get("run") or "",
        )
        self.assertIn(
            '--event-policy "pull_request_exact_head_lane_fingerprint"',
            report_step.get("run") or "",
        )
        self.assertIn(
            '--planner-fingerprint "${{ needs.metadata.outputs.planner_fingerprint }}"',
            (steps[3] or {}).get("run") or "",
        )
        self.assertIn(
            '--workflow-result "${WORKFLOW_RESULT}"',
            report_step.get("run") or "",
        )
        self.assertIn(
            '--rust-minimal-result "${rust_minimal_result}"',
            report_step.get("run") or "",
        )
        self.assertIn(
            '--rust-integration-result "${rust_integration_result}"',
            report_step.get("run") or "",
        )

    def test_blocking_ci_uses_documented_merge_group_trigger(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/blocking-ci.yml")
        self.assertEqual(
            (payload.get("on") or {}).get("merge_group"),
            {"types": ["checks_requested"]},
        )

    def test_merge_group_concurrency_is_sha_scoped_and_not_cancelled(self) -> None:
        merge_group_workflows = []
        for workflow_path in sorted((REPO_ROOT / ".github/workflows").glob("*.y*ml")):
            payload = load_workflow_payload(workflow_path)
            if "merge_group" not in (payload.get("on") or {}):
                continue
            merge_group_workflows.append(workflow_path.name)
            workflow_name = workflow_path.name
            with self.subTest(workflow=workflow_name):
                concurrency = payload.get("concurrency") or {}
                group = str(concurrency.get("group") or "")
                self.assertIn("concurrency-group::${{ github.workflow }}::", group)
                self.assertIn("github.event_name == 'merge_group'", group)
                self.assertIn("format('merge-group-{0}', github.sha)", group)
                self.assertIn("format('pr-{0}', github.event.pull_request.number)", group)
                self.assertIn("format('push-{0}', github.sha)", group)
                self.assertIn("format('{0}-{1}', github.event_name, github.run_id)", group)
                cancel = str(concurrency.get("cancel-in-progress") or "")
                self.assertEqual(cancel, "${{ github.event_name == 'pull_request' }}")

        self.assertEqual(
            merge_group_workflows,
            [
                "blocking-ci.yml",
                "codeql.yml",
                "osv-scanner.yml",
                "runner-label-policy.yml",
            ],
        )

        bazel = load_workflow_payload(REPO_ROOT / ".github/workflows/bazel.yml")
        self.assertNotIn(
            "concurrency",
            bazel,
            "Bazel is always admitted through blocking-ci, whose caller-level key owns queue isolation.",
        )

    def test_blob_size_policy_uses_queue_base_for_merge_groups(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/blob-size-policy.yml")
        check_steps = ((payload.get("jobs") or {}).get("check") or {}).get("steps") or []
        range_step = next(
            step for step in check_steps if step.get("name") == "Determine comparison range"
        )
        run_script = range_step.get("run") or ""
        self.assertIn(
            'if [[ "${{ github.event_name }}" == "merge_group" ]]; then',
            run_script,
        )
        self.assertIn("github.event.merge_group.base_sha", run_script)
        self.assertIn("head='${{ github.sha }}'", run_script)


class BazelCiModeScriptTests(unittest.TestCase):
    def test_docs_only_mode_requires_a_complete_nonempty_docs_file_list(self) -> None:
        self.assertEqual(
            RESOLVE_BAZEL_CI_MODE.resolve_bazel_ci_mode(
                comparison_complete=True,
                files=["README.md", "docs/guide.md", "docs/reference/config.md"],
            ),
            {"mode": "docs_only", "run_bazel": "false"},
        )

        for comparison_complete, files in [
            (False, ["README.md"]),
            (True, []),
            (True, ["README.md", "codex-rs/core/src/lib.rs"]),
            (True, [".github/workflows/bazel.yml"]),
            (True, ["docs/guide.md", 42]),
            (True, {"filename": "docs/guide.md"}),
        ]:
            with self.subTest(
                comparison_complete=comparison_complete,
                files=files,
            ):
                self.assertEqual(
                    RESOLVE_BAZEL_CI_MODE.resolve_bazel_ci_mode(
                        comparison_complete=comparison_complete,
                        files=files,
                    ),
                    {"mode": "full", "run_bazel": "true"},
                )

    def test_command_line_mode_resolver_fails_closed_on_bad_json(self) -> None:
        outputs = run_script(
            SCRIPTS_DIR / "resolve_bazel_ci_mode.py",
            "--comparison-complete",
            "true",
            "--files-json",
            "not-json",
        )
        self.assertEqual(outputs, {"mode": "full", "run_bazel": "true"})

    def test_command_line_mode_resolver_writes_github_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            output_path = Path(tmpdir) / "github-output"
            proc = subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "resolve_bazel_ci_mode.py"),
                    "--comparison-complete",
                    "true",
                    "--files-json",
                    '["README.md", "docs/guide.md"]',
                    "--github-output",
                    str(output_path),
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertEqual(
                json.loads(proc.stdout),
                {"mode": "docs_only", "run_bazel": "false"},
            )
            self.assertEqual(
                parse_github_output_file(output_path),
                {"mode": "docs_only", "run_bazel": "false"},
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
            if step.get("uses") == "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
        )
        self.assertEqual((checkout.get("with") or {}).get("fetch-depth"), "1")

        previous_required_step = next(
            step for step in steps if step.get("name") == "Check previous required result on follow-up head"
        )
        self.assertEqual(previous_required_step.get("uses"), "actions/github-script@v9.0.0")
        self.assertIn("github.event.action == 'synchronize'", previous_required_step.get("if") or "")
        previous_required_script = (
            (previous_required_step.get("with") or {}).get("script") or ""
        )
        self.assertIn("github.rest.pulls.listCommits", previous_required_script)
        self.assertIn("github.rest.checks.listForRef", previous_required_script)
        self.assertIn("context.payload.before", previous_required_script)
        self.assertIn("pullRequest?.before", previous_required_script)
        self.assertIn("candidateShas.push(eventBefore)", previous_required_script)
        self.assertIn("previous_green_sha", previous_required_script)
        self.assertIn("Rust CI required gate", previous_required_script)

        metadata_step = next(
            step for step in steps if step.get("name") == "Resolve PR changed files via API"
        )
        self.assertEqual(metadata_step.get("uses"), "actions/github-script@v9.0.0")
        metadata_script = ((metadata_step.get("with") or {}).get("script") or "")
        self.assertIn("github.paginate(github.rest.pulls.listFiles", metadata_script)
        self.assertIn("github.rest.repos.compareCommitsWithBasehead", metadata_script)
        self.assertEqual(
            (metadata_step.get("env") or {}).get("BEFORE_SHA"),
            "${{ steps.previous_required.outputs.previous_green_sha || steps.shas.outputs.before_sha }}",
        )

        fallback_step = next(
            step for step in steps if step.get("name") == "Fetch history for git diff fallback"
        )
        self.assertIn(
            "steps.pr_diff.outputs.needs_git_fallback == 'true'",
            fallback_step.get("if") or "",
        )
        fallback_run = fallback_step.get("run") or ""
        self.assertIn(
            "before_sha='${{ steps.previous_required.outputs.previous_green_sha || steps.shas.outputs.before_sha }}'",
            fallback_run,
        )
        self.assertIn('git fetch --no-tags --depth=1 "${head_repo}" "${before_sha}"', fallback_run)
        self.assertIn('"${before_sha}^{commit}"', fallback_run)

        detect_step = next(
            step for step in steps if step.get("name") == "Detect changed paths and rust-ci mode"
        )
        detect_env = detect_step.get("env") or {}
        self.assertEqual(
            detect_env.get("PREVIOUS_GREEN_REQUIRED"),
            "${{ steps.previous_required.outputs.previous_green_required || 'false' }}",
        )
        self.assertEqual(
            detect_env.get("COMPARISON_BEFORE_SHA"),
            "${{ steps.previous_required.outputs.previous_green_sha || steps.shas.outputs.before_sha }}",
        )
        detect_run = detect_step.get("run") or ""
        self.assertIn('--before-sha "${COMPARISON_BEFORE_SHA}"', detect_run)
        self.assertIn('--previous-green-required "${PREVIOUS_GREEN_REQUIRED}"', detect_run)
        self.assertIn("--primary-files-json", detect_run)
        self.assertIn("--primary-line-count", detect_run)
        self.assertIn("--latest-delta-files-json", detect_run)
        self.assertIn("--latest-delta-line-count", detect_run)

    def test_rust_ci_results_gate_honors_selected_run_flags(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci.yml")
        jobs = payload.get("jobs") or {}
        results_run = (
            next(
                step
                for step in (jobs.get("results") or {}).get("steps") or []
                if step.get("name") == "Summarize"
            ).get("run")
            or ""
        )

        self.assertIn("needs.changed.outputs.run_argument_comment_lint_package", results_run)
        self.assertIn("needs.changed.outputs.run_argument_comment_lint_prebuilt", results_run)
        self.assertIn("needs.changed.outputs.run_general", results_run)
        self.assertIn("needs.changed.outputs.run_cargo_shear", results_run)
        self.assertIn("needs.changed.outputs.run_incremental_validation", results_run)
        self.assertIn("needs.changed.result", results_run)
        self.assertIn("changed planner failed", results_run)
        self.assertIn("needs.matrix_plan.result", results_run)
        self.assertIn("matrix_plan failed", results_run)
        self.assertIn("needs.planner_fixtures.result", results_run)
        self.assertIn('"${NEEDS_CHANGED_OUTPUTS_WORKFLOWS}" == \'true\'', results_run)
        self.assertIn("planner_fixtures failed", results_run)
        self.assertIn("incremental_validation failed", results_run)
        no_relevant_gate = results_run.split("No relevant changes -> CI not required.")[0]
        self.assertIn("NEEDS_CHANGED_OUTPUTS_WORKFLOWS", no_relevant_gate)
        self.assertIn("needs.changed.outputs.run_incremental_validation", no_relevant_gate)
        self.assertNotIn(
            'NEEDS_CHANGED_OUTPUTS_CODEX}" == \'true\' || "${NEEDS_CHANGED_OUTPUTS_WORKFLOWS}" == \'true\'',
            results_run,
        )

        argpkg_job = jobs.get("argument_comment_lint_package") or {}
        self.assertEqual(
            argpkg_job.get("if"),
            "${{ needs.changed.outputs.run_argument_comment_lint_package == 'true' }}",
        )

    def test_rust_ci_argument_comment_lint_uses_single_cached_bazel_action(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/rust-ci.yml")
        jobs = payload.get("jobs") or {}

        matrix_plan_run = (
            next(
                step
                for step in (jobs.get("matrix_plan") or {}).get("steps") or []
                if step.get("name") == "Compute platform matrices"
            ).get("run")
            or ""
        )
        self.assertIn('"timeout_minutes": 30', matrix_plan_run)

        arglint_job = jobs.get("argument_comment_lint_prebuilt") or {}
        self.assertEqual(arglint_job.get("needs"), ["changed", "matrix_plan"])
        self.assertEqual(
            arglint_job.get("if"),
            "${{ needs.changed.outputs.run_argument_comment_lint_prebuilt == 'true' }}",
        )
        self.assertEqual(
            (arglint_job.get("strategy") or {}).get("matrix"),
            "${{ fromJSON(needs.matrix_plan.outputs.argument_comment_lint_matrix) }}",
        )
        self.assertNotIn("environment", arglint_job)

        arglint_steps = arglint_job.get("steps") or []
        lint_steps = [
            step
            for step in arglint_steps
            if step.get("name") == "Run argument comment lint on codex-rs"
        ]
        self.assertEqual(len(lint_steps), 1)
        self.assertEqual(lint_steps[0].get("uses"), "./.github/actions/run-argument-comment-lint")
        self.assertNotIn("buildbuddy-api-key", lint_steps[0].get("with") or {})

    def test_argument_comment_lint_platform_workflow_is_pr_only_advisory_coverage(self) -> None:
        payload = load_workflow_payload(
            REPO_ROOT / ".github/workflows/argument-comment-lint-platform.yml"
        )
        trigger = payload.get("on") or {}
        pull_request = trigger.get("pull_request") or {}
        self.assertEqual(set(trigger), {"pull_request"})
        self.assertEqual(pull_request.get("branches"), ["main", "upstream-main"])
        self.assertEqual(
            pull_request.get("paths"),
            [
                "codex-rs/**",
                "tools/argument-comment-lint/**",
                "Cargo.lock",
                "Cargo.toml",
                "**/Cargo.toml",
                "rust-toolchain.toml",
                "MODULE.bazel",
                "MODULE.bazel.lock",
                ".github/**",
                "justfile",
                "scripts/**",
            ],
        )
        self.assertEqual(payload.get("permissions"), {"contents": "read"})
        self.assertEqual(
            payload.get("concurrency"),
            {
                "group": "concurrency-group::${{ github.workflow }}::pr-${{ github.event.pull_request.number }}",
                "cancel-in-progress": "true",
            },
        )

        lint_job = (payload.get("jobs") or {}).get("lint") or {}
        self.assertNotIn("environment", lint_job)
        self.assertEqual(
            (lint_job.get("strategy") or {}).get("matrix"),
            {
                "include": [
                    {"name": "macOS", "runner": "macos-15", "timeout_minutes": "30"},
                    {
                        "name": "Windows",
                        "runner": "windows-x64",
                        "runs_on": "windows-2022",
                        "timeout_minutes": "30",
                    },
                ]
            },
        )
        steps = lint_job.get("steps") or []
        checkout = steps[0]
        self.assertEqual(
            checkout.get("uses"), "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
        )
        self.assertEqual((checkout.get("with") or {}).get("persist-credentials"), "false")
        lint_step = next(
            step for step in steps if step.get("name") == "Run argument comment lint on codex-rs"
        )
        self.assertEqual(lint_step.get("uses"), "./.github/actions/run-argument-comment-lint")

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
            json.dumps(["codex-rs/prompts/src/review_request.rs"]),
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

    def test_docs_only_light_route_still_requires_incremental_gate(self) -> None:
        outputs = self.run_rust_ci_mode(
            event_action="opened",
            head_files={"docs/native-computer-use.md": "docs\n"},
        )

        self.assertEqual(outputs["validation_mode"], "light_initial")
        self.assertEqual(outputs["argument_comment_lint"], "false")
        self.assertEqual(outputs["codex"], "false")
        self.assertEqual(outputs["workflows"], "false")
        self.assertEqual(outputs["run_incremental_validation"], "true")
        self.assertEqual(
            outputs["incremental_lanes"],
            "codex.downstream-docs-check,codex.downstream-divergence-audit",
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

    def test_review_request_pr_routes_to_custom_prompt_targeted_validation(self) -> None:
        outputs = self.run_rust_ci_mode(
            event_action="opened",
            head_files={"codex-rs/prompts/src/review_request.rs": "fn review_prompt() {}\n"},
        )

        self.assertEqual(outputs["validation_mode"], "light_initial")
        self.assertEqual(outputs["codex"], "true")
        self.assertEqual(outputs["run_general"], "false")
        self.assertEqual(outputs["run_cargo_shear"], "false")
        self.assertEqual(outputs["run_incremental_validation"], "true")
        self.assertEqual(outputs["incremental_lanes"], "codex.custom-prompts-targeted")


class HelperScriptTests(unittest.TestCase):
    def test_duplicate_workflow_finder_matches_same_branch_sha_success(self) -> None:
        runs = [
            {
                "id": 12,
                "head_branch": "main",
                "head_sha": "abc123",
                "status": "completed",
                "conclusion": "success",
                "event": "schedule",
                "html_url": "https://example.test/runs/12",
            },
            {
                "id": 11,
                "head_branch": "main",
                "head_sha": "abc123",
                "status": "completed",
                "conclusion": "success",
                "event": "workflow_dispatch",
                "html_url": "https://example.test/runs/11",
            },
        ]

        match = SKIP_DUPLICATE_WORKFLOW_RUN.find_equivalent_success(
            runs,
            branch="main",
            head_sha="abc123",
            current_run_id=12,
            allowed_events=set(),
        )

        self.assertEqual(match["id"], 11)
        self.assertEqual(
            SKIP_DUPLICATE_WORKFLOW_RUN.result_from_match(match),
            {
                "should_skip": "true",
                "should_run": "false",
                "proof_found": "true",
                "reason": "equivalent_success_found",
                "proof_reason": "equivalent_success_found",
                "matched_run_id": "11",
                "matched_run_url": "https://example.test/runs/11",
                "matched_run_event": "workflow_dispatch",
                "matched_run_created_at": "",
                "proof_run_id": "11",
                "proof_run_url": "https://example.test/runs/11",
                "evidence_key": "main:abc123",
            },
        )

    def test_duplicate_workflow_finder_ignores_wrong_sha_branch_or_failed_runs(self) -> None:
        runs = [
            {
                "id": 21,
                "head_branch": "main",
                "head_sha": "other",
                "status": "completed",
                "conclusion": "success",
                "event": "schedule",
            },
            {
                "id": 22,
                "head_branch": "feature",
                "head_sha": "abc123",
                "status": "completed",
                "conclusion": "success",
                "event": "schedule",
            },
            {
                "id": 23,
                "head_branch": "main",
                "head_sha": "abc123",
                "status": "completed",
                "conclusion": "failure",
                "event": "schedule",
            },
        ]

        self.assertIsNone(
            SKIP_DUPLICATE_WORKFLOW_RUN.find_equivalent_success(
                runs,
                branch="main",
                head_sha="abc123",
                current_run_id=None,
                allowed_events=set(),
            )
        )

    def test_duplicate_workflow_finder_requires_matching_summary_fingerprint(self) -> None:
        runs = [
            {
                "id": 31,
                "head_branch": "main",
                "head_sha": "abc123",
                "status": "completed",
                "conclusion": "success",
                "event": "workflow_dispatch",
            },
            {
                "id": 32,
                "head_branch": "main",
                "head_sha": "abc123",
                "status": "completed",
                "conclusion": "success",
                "event": "workflow_dispatch",
            },
        ]
        summaries = {
            31: {
                "selection": {"planner_fingerprint": "different"},
                "summary": {"overall_conclusion": "success"},
                "dedupe": {"should_skip": False},
            },
            32: {
                "selection": {"planner_fingerprint": "plan-fp"},
                "summary": {"overall_conclusion": "success"},
                "dedupe": {"should_skip": False},
            },
        }

        def metadata_matcher(run: dict) -> bool:
            return SKIP_DUPLICATE_WORKFLOW_RUN.validation_summary_matches(
                summaries[run["id"]],
                planner_fingerprint="plan-fp",
            )

        match = SKIP_DUPLICATE_WORKFLOW_RUN.find_equivalent_success(
            runs,
            branch="main",
            head_sha="abc123",
            current_run_id=None,
            allowed_events={"workflow_dispatch"},
            metadata_matcher=metadata_matcher,
        )

        self.assertEqual(match["id"], 32)

    def test_duplicate_workflow_finder_ignores_reused_summary_artifacts(self) -> None:
        payload = {
            "selection": {"planner_fingerprint": "plan-fp"},
            "summary": {"overall_conclusion": "success"},
            "dedupe": {"should_skip": True},
        }

        self.assertFalse(
            SKIP_DUPLICATE_WORKFLOW_RUN.validation_summary_matches(
                payload,
                planner_fingerprint="plan-fp",
            )
        )

    def test_artifact_download_drops_github_auth_on_signed_redirect(self) -> None:
        requests: list[object] = []

        class FakeResponse:
            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            def read(self) -> bytes:
                return b"artifact bytes"

        def fake_open(request, timeout):
            del timeout
            requests.append(request)
            if len(requests) == 1:
                raise SKIP_DUPLICATE_WORKFLOW_RUN.urllib.error.HTTPError(
                    request.full_url,
                    302,
                    "Found",
                    {"Location": "https://signed-artifacts.example/archive.zip"},
                    None,
                )
            return FakeResponse()

        opener = mock.Mock()
        opener.open.side_effect = fake_open
        with mock.patch.object(
            SKIP_DUPLICATE_WORKFLOW_RUN.urllib.request,
            "build_opener",
            return_value=opener,
        ):
            payload = SKIP_DUPLICATE_WORKFLOW_RUN.api_get_bytes(
                "https://api.github.com/repos/sednalabs/codex/actions/artifacts/1/zip",
                "token-value",
            )

        self.assertEqual(payload, b"artifact bytes")
        first_headers = {key.lower(): value for key, value in requests[0].header_items()}
        second_headers = {key.lower(): value for key, value in requests[1].header_items()}
        self.assertEqual(first_headers.get("authorization"), "Bearer token-value")
        self.assertNotIn("authorization", second_headers)

    def test_github_api_url_validation_rejects_non_github_hosts(self) -> None:
        with self.assertRaises(ValueError):
            SKIP_DUPLICATE_WORKFLOW_RUN.validated_github_api_url(
                "https://example.test/repos/sednalabs/codex/actions/runs"
            )

    def test_duplicate_workflow_script_fails_open_for_bad_current_run_id(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "outputs"
            proc = subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "skip_duplicate_workflow_run.py"),
                    "--repo",
                    "sednalabs/codex",
                    "--workflow",
                    "rust-ci.yml",
                    "--branch",
                    "main",
                    "--head-sha",
                    "abc123",
                    "--current-run-id",
                    "not-an-int",
                    "--github-output",
                    str(output),
                ],
                check=True,
                capture_output=True,
                text=True,
            )

            payload = json.loads(proc.stdout)
            output_lines = parse_github_output_file(output)

        self.assertEqual(payload["should_skip"], "false")
        self.assertEqual(payload["should_run"], "true")
        self.assertEqual(payload["reason"], "lookup_failed_run_conservatively")
        self.assertEqual(output_lines["should_skip"], "false")
        self.assertEqual(output_lines["should_run"], "true")
        self.assertEqual(output_lines["reason"], "lookup_failed_run_conservatively")

    def test_github_output_parser_tolerates_malformed_and_multiline_entries(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            output = Path(tmpdir) / "github-output.txt"
            output.write_text(
                "\n".join(
                    [
                        "plain=value",
                        "malformed",
                        "empty_key_is_ignored",
                        "=missing-key",
                        "multi<<EOF",
                        "line one",
                        "line two",
                        "EOF",
                        "later=still parsed",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            self.assertEqual(
                parse_github_output_file(output),
                {
                    "plain": "value",
                    "multi": "line one\nline two",
                    "later": "still parsed",
                },
            )

    def test_repository_workflows_follow_static_policy(self) -> None:
        self.assertEqual(CHECK_WORKFLOW_POLICY.collect_violations(REPO_ROOT), [])

    def test_sedna_release_manual_dispatch_defaults_to_auto_channel(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-release.yml")
        channel_input = (
            (((payload.get("on") or {}).get("workflow_dispatch") or {}).get("inputs") or {})
            .get("channel")
            or {}
        )

        self.assertEqual(
            {
                "default": channel_input.get("default"),
                "options": channel_input.get("options"),
            },
            {
                "default": "auto",
                "options": ["auto", "stable", "prerelease"],
            },
        )

    def test_sedna_release_main_pushes_are_routed_before_publisher(self) -> None:
        release_payload = load_workflow_payload(
            REPO_ROOT / ".github/workflows/sedna-release.yml"
        )
        release_push = ((release_payload.get("on") or {}).get("push") or {})
        jobs = release_payload.get("jobs") or {}
        route_job = jobs.get("route") or {}
        router_steps = (
            (route_job.get("steps") or [])
        )
        named_steps = {step.get("name"): step for step in router_steps if "name" in step}
        resolve_job = jobs.get("resolve") or {}
        release_job = jobs.get("release-linux") or {}
        publish_job = jobs.get("publish-release") or {}

        self.assertIn("main release gate", release_payload.get("run-name") or "")
        self.assertNotIn("concurrency", release_payload)
        self.assertEqual(route_job.get("name"), "Release request gate")
        self.assertEqual(route_job.get("runs-on"), "ubuntu-slim")
        self.assertEqual(release_push.get("branches"), ["main"])
        self.assertEqual(release_push.get("tags"), ["v*-sedna.*"])
        self.assertEqual(release_payload.get("permissions"), {})
        self.assertEqual(route_job.get("permissions"), {})
        self.assertFalse(any("uses" in step for step in router_steps))
        self.assertIn("HEAD_MESSAGE", named_steps["Resolve release request"].get("env") or {})
        self.assertIn("^Sedna-Release:", named_steps["Resolve release request"].get("run") or "")
        self.assertIn(
            "Publisher job: ${publisher_job}",
            named_steps["Summarize release gate"].get("run") or "",
        )
        self.assertEqual(resolve_job.get("name"), "Resolve release metadata")
        self.assertEqual(resolve_job.get("needs"), "route")
        self.assertIn(
            "needs.route.outputs.release_requested == 'true'",
            resolve_job.get("if") or "",
        )
        self.assertIn("release_tag", resolve_job.get("outputs") or {})
        resolve_steps = resolve_job.get("steps") or []
        resolve_named_steps = {
            step.get("name"): step for step in resolve_steps if "name" in step
        }
        self.assertIn(
            "--missing-marker error",
            resolve_named_steps["Resolve release metadata"].get("run") or "",
        )
        self.assertEqual(release_job.get("name"), "Build Linux release artifacts")
        self.assertEqual(release_job.get("needs"), "resolve")
        self.assertIn(
            "needs.resolve.outputs.release_requested == 'true'",
            release_job.get("if") or "",
        )
        self.assertEqual(
            release_job.get("concurrency"),
            {
                "group": "${{ github.workflow }}-${{ needs.resolve.outputs.release_tag }}",
                "cancel-in-progress": "false",
            },
        )
        self.assertEqual(
            release_job.get("permissions"),
            {"contents": "read", "id-token": "write"},
        )
        self.assertNotIn("environment", release_job)
        self.assertEqual(publish_job.get("name"), "Publish GitHub release")
        self.assertEqual(publish_job.get("needs"), ["resolve", "release-linux"])
        self.assertEqual(publish_job.get("environment"), "release")
        self.assertEqual(
            publish_job.get("permissions"),
            {"actions": "read"},
        )

    def test_sedna_release_uses_dedicated_github_app_for_publication(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-release.yml")
        jobs = payload.get("jobs") or {}
        release_job = jobs.get("release-linux") or {}
        publish_job = jobs.get("publish-release") or {}
        release_steps = release_job.get("steps") or []
        steps = publish_job.get("steps") or []
        release_named_steps = {
            step.get("name"): step for step in release_steps if "name" in step
        }
        named_steps = {step.get("name"): step for step in steps if "name" in step}

        self.assertIn("Upload workflow artifacts", release_named_steps)
        self.assertEqual(
            named_steps["Download Linux release artifacts"].get("uses"),
            "actions/download-artifact@v8",
        )
        self.assertEqual(
            named_steps["Download Linux release artifacts"].get("with") or {},
            {"name": "sedna-release-linux", "path": "dist"},
        )

        config_step = named_steps["Check release publisher app configuration"]
        self.assertEqual(
            {
                "APP_CLIENT_ID": (
                    (config_step.get("env") or {}).get("APP_CLIENT_ID")
                ),
                "APP_PRIVATE_KEY": (
                    (config_step.get("env") or {}).get("APP_PRIVATE_KEY")
                ),
            },
            {
                "APP_CLIENT_ID": "${{ vars.SEDNA_RELEASE_PUBLISHER_APP_CLIENT_ID }}",
                "APP_PRIVATE_KEY": "${{ secrets.SEDNA_RELEASE_PUBLISHER_APP_PRIVATE_KEY }}",
            },
        )
        self.assertIn(
            "Missing release publisher GitHub App configuration",
            config_step.get("run") or "",
        )

        token_step = named_steps["Generate release publisher app token"]
        self.assertEqual(token_step.get("id"), "release_publisher_token")
        self.assertEqual(
            token_step.get("uses"),
            "actions/create-github-app-token@1b10c78c7865c340bc4f6099eb2f838309f1e8c3",
        )
        self.assertEqual(
            token_step.get("with") or {},
            {
                "client-id": "${{ vars.SEDNA_RELEASE_PUBLISHER_APP_CLIENT_ID }}",
                "private-key": "${{ secrets.SEDNA_RELEASE_PUBLISHER_APP_PRIVATE_KEY }}",
                "permission-actions": "write",
                "permission-contents": "write",
            },
        )

        for step_name in ("Create GitHub release", "Dispatch release asset verifier"):
            self.assertEqual(
                (named_steps[step_name].get("env") or {}).get("GH_TOKEN"),
                "${{ steps.release_publisher_token.outputs.token }}",
            )
        self.assertEqual(publish_job.get("permissions"), {"actions": "read"})

    def test_sedna_release_verifier_checks_staged_binary_version_in_dry_run(self) -> None:
        installer = (REPO_ROOT / "scripts/install_sedna_release_asset").read_text(
            encoding="utf-8"
        )
        dry_run_index = installer.index('if [[ "$dry_run" == "true" ]]')
        version_check_index = installer.index(
            'staged_version_output="$("$staged/codex" --version 2>&1)"'
        )

        self.assertLess(version_check_index, dry_run_index)
        self.assertIn(
            'staged codex --version did not report ${release_version@Q}',
            installer,
        )
        self.assertIn('echo "$staged_version_output"', installer)

    def test_sedna_release_verifier_tag_grammar_matches_resolver_shape(self) -> None:
        install_workflow = (
            REPO_ROOT / ".github/workflows/sedna-release-install.yml"
        ).read_text(encoding="utf-8")
        installer = (REPO_ROOT / "scripts/install_sedna_release_asset").read_text(
            encoding="utf-8"
        )
        shared_tail = (
            r"(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?-sedna\.[0-9]+"
            r"(\+upstream\.[0-9]+)?"
        )

        self.assertIn(shared_tail, install_workflow)
        self.assertIn(shared_tail, installer)
        self.assertNotIn("([-.][0-9A-Za-z.]+)*-sedna", install_workflow)
        self.assertNotIn("([-.][0-9A-Za-z]+)*-sedna", installer)

    def test_sedna_release_router_detects_release_marker(self) -> None:
        script = workflow_step_by_name(
            REPO_ROOT / ".github/workflows/sedna-release.yml",
            "route",
            "Resolve release request",
        )["run"]
        event = {
            "ref": "refs/heads/main",
            "after": "abc123",
            "head_commit": {
                "message": "release commit\n\nSedna-Release: prerelease\n",
            },
        }

        proc, outputs = run_workflow_step_script(script, event)

        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(
            outputs,
            {
                "release_requested": "true",
                "reason": "release_marker",
                "target_sha": "abc123",
                "channel": "prerelease",
            },
        )

    def test_sedna_release_router_skips_unmarked_main_pushes(self) -> None:
        script = workflow_step_by_name(
            REPO_ROOT / ".github/workflows/sedna-release.yml",
            "route",
            "Resolve release request",
        )["run"]
        event = {
            "ref": "refs/heads/main",
            "after": "abc123",
            "head_commit": {
                "message": "ordinary maintenance commit",
            },
        }

        proc, outputs = run_workflow_step_script(script, event)

        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(
            outputs,
            {
                "release_requested": "false",
                "reason": "missing_sedna_release_marker",
                "target_sha": "abc123",
                "channel": "",
            },
        )

    def test_sedna_release_router_rejects_invalid_release_marker(self) -> None:
        script = workflow_step_by_name(
            REPO_ROOT / ".github/workflows/sedna-release.yml",
            "route",
            "Resolve release request",
        )["run"]
        event = {
            "ref": "refs/heads/main",
            "after": "abc123",
            "head_commit": {
                "message": "release commit\n\nSedna-Release: beta\n",
            },
        }

        proc, outputs = run_workflow_step_script(script, event)

        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(outputs, {})
        self.assertIn(
            "Sedna-Release marker must be either 'stable' or 'prerelease'",
            proc.stderr,
        )

    def test_sedna_release_uses_synced_upstream_mirror_as_version_base(self) -> None:
        workflow = (REPO_ROOT / ".github/workflows/sedna-release.yml").read_text(
            encoding="utf-8"
        )

        self.assertIn(
            "+refs/heads/upstream-main:refs/remotes/origin/upstream-main",
            workflow,
        )
        self.assertIn("--upstream-ref refs/remotes/origin/upstream-main", workflow)
        self.assertNotIn("--upstream-ref refs/remotes/upstream/main", workflow)

    def test_sedna_release_dispatches_public_asset_verification_only(self) -> None:
        release_workflow = (REPO_ROOT / ".github/workflows/sedna-release.yml").read_text(
            encoding="utf-8"
        )
        install_payload = load_workflow_payload(
            REPO_ROOT / ".github/workflows/sedna-release-install.yml"
        )
        install_job = ((install_payload.get("jobs") or {}).get("install") or {})

        self.assertIn("Dispatch release asset verifier", release_workflow)
        self.assertIn('-f "dry_run=true"', release_workflow)
        self.assertNotIn('-f "dry_run=false"', release_workflow)
        self.assertEqual(install_job.get("runs-on"), "ubuntu-24.04")
        workflow_json = json.dumps(install_payload, sort_keys=True)
        self.assertNotIn("self-hosted", workflow_json)
        self.assertIn("public workflow requires true", workflow_json)
        self.assertIn("--dry-run", workflow_json)
        self.assertIn("external deployment path", workflow_json)

    def test_sedna_release_reuses_caches_without_reusing_smoke_artifacts(self) -> None:
        payload = load_workflow_payload(REPO_ROOT / ".github/workflows/sedna-release.yml")
        release_job = ((payload.get("jobs") or {}).get("release-linux") or {})
        steps = release_job.get("steps") or []
        named_steps = {step.get("name"): step for step in steps if "name" in step}

        self.assertEqual(
            {
                "cargo_home_restore": named_steps["Restore cargo home cache"].get("uses"),
                "sccache_install": named_steps["Install sccache"].get("uses"),
                "sccache_configure_run": named_steps["Configure sccache backend"].get("run"),
                "sccache_restore": named_steps["Restore sccache cache (fallback)"].get("uses"),
                "cargo_home_save": named_steps["Save cargo home cache"].get("uses"),
                "sccache_save": named_steps["Save sccache cache (fallback)"].get("uses"),
            },
            {
                "cargo_home_restore": "actions/cache/restore@v6",
                "sccache_install": "taiki-e/install-action@065d6a08a14e61e89fb0a4c10eecdbdef39c7d8e",
                "sccache_configure_run": "bash .github/scripts/configure_sccache_backend.sh write-fallback",
                "sccache_restore": "actions/cache/restore@v6",
                "cargo_home_save": "actions/cache/save@v6",
                "sccache_save": "actions/cache/save@v6",
            },
        )

        self.assertIn(
            "steps.build_release.outcome == 'success'",
            named_steps["Save cargo home cache"].get("if") or "",
        )
        self.assertIn(
            "steps.build_release.outcome == 'success'",
            named_steps["Save sccache cache (fallback)"].get("if") or "",
        )
        self.assertIn(
            "steps.sccache_backend.outputs.policy == 'write-fallback'",
            named_steps["Save sccache cache (fallback)"].get("if") or "",
        )
        self.assertIn(
            "SCCACHE_BASEDIRS=${GITHUB_WORKSPACE}",
            named_steps["Enable sccache wrapper and reset stats"].get("run") or "",
        )
        self.assertEqual(
            (
                (named_steps["Build release binaries"].get("env") or {}).get(
                    "CODEX_UPSTREAM_DISTANCE_FROM_TAG"
                )
            ),
            "${{ needs.resolve.outputs.upstream_distance_from_tag }}",
        )
        self.assertIn(
            "upstream_position=${UPSTREAM_POSITION}",
            named_steps["Stage release assets"].get("run") or "",
        )
        self.assertIn(
            '"upstream_position": os.environ["UPSTREAM_POSITION"]',
            named_steps["Stage release assets"].get("run") or "",
        )
        self.assertEqual(named_steps["Build release binaries"].get("id"), "build_release")
        self.assertFalse(
            any(step.get("uses", "").startswith("actions/download-artifact") for step in steps)
        )

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

    def test_workflow_policy_rejects_self_hosted_runners_in_public_workflows(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/deploy.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: deploy
on: workflow_dispatch
jobs:
  install:
    runs-on: [self-hosted, linux, x64, example-runner]
    steps:
      - run: true
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(
            violations,
            [
                ".github/workflows/deploy.yml: self-hosted runners are not allowed; "
                "use external deployment automation for host-local operations."
            ],
        )

    def test_workflow_policy_rejects_larger_runner_matrix_labels(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: release
on: workflow_dispatch
jobs:
  build:
    runs-on: ${{ matrix.runner }}
    strategy:
      matrix:
        include:
          - runner: macos-15-xlarge
          - runner: macos-15-large
          - runner: windows-2022-xlarge
          - runner: ${{ github.event.repository.name }}-linux-arm64
    steps:
      - run: true
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        repo_scoped_runner = (
            ".github/workflows/release.yml: repo-scoped runner label "
            + "'${{ github.event.repository.name }}-linux-arm64' is not allowed; "
            + "use a standard public GitHub-hosted runner label."
        )
        macos_large_runner = (
            ".github/workflows/release.yml: runner label 'macos-15-large' uses "
            + "a larger-runner size token; use standard public GitHub-hosted "
            + "runner labels."
        )
        macos_xlarge_runner = (
            ".github/workflows/release.yml: runner label 'macos-15-xlarge' uses "
            + "a larger-runner size token; use standard public GitHub-hosted "
            + "runner labels."
        )
        windows_xlarge_runner = (
            ".github/workflows/release.yml: runner label 'windows-2022-xlarge' "
            + "uses a larger-runner size token; use standard public GitHub-hosted "
            + "runner labels."
        )
        self.assertEqual(
            violations,
            [
                repo_scoped_runner,
                macos_large_runner,
                macos_xlarge_runner,
                windows_xlarge_runner,
            ],
        )

    def test_workflow_policy_rejects_runner_group_selectors(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: release
on: workflow_dispatch
jobs:
  build:
    runs-on:
      group: custom-runners
      labels: custom-windows-x64
    steps:
      - run: true
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        runner_group_violation = (
            ".github/workflows/release.yml: runner groups are not allowed; use "
            + "standard public GitHub-hosted runner labels directly."
        )
        self.assertEqual(
            violations,
            [runner_group_violation],
        )

    def test_workflow_policy_rejects_runner_group_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/reusable.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: reusable
on:
  workflow_call:
    inputs:
      runner:
        required: true
        type: string
      runner_group:
        required: false
        type: string
jobs:
  build:
    runs-on: ${{ inputs.runner }}
    steps:
      - run: true
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        runner_group_input_violation = (
            ".github/workflows/reusable.yml: runner group inputs are not allowed; "
            + "use standard public GitHub-hosted runner labels directly."
        )
        self.assertEqual(
            violations,
            [runner_group_input_violation],
        )

    def test_workflow_policy_does_not_treat_unused_runs_on_as_runner_override(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: release
on: workflow_dispatch
jobs:
  build:
    runs-on: ${{ matrix.runner }}
    strategy:
      matrix:
        include:
          - runner: macos-15-xlarge
            runs_on: ubuntu-24.04
    steps:
      - run: true
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        xlarge_runner_violation = (
            ".github/workflows/release.yml: runner label 'macos-15-xlarge' uses "
            + "a larger-runner size token; use standard public GitHub-hosted "
            + "runner labels."
        )
        self.assertEqual(
            violations,
            [xlarge_runner_violation],
        )

    def test_workflow_policy_allows_standard_public_runners(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: release
on: workflow_dispatch
jobs:
  build:
    runs-on: ${{ matrix.runner }}
    strategy:
      matrix:
        include:
          - runner: ubuntu-latest
          - runner: ubuntu-slim
          - runner: ubuntu-24.04
          - runner: ubuntu-24.04-arm
          - runner: ubuntu-26.04
          - runner: ubuntu-26.04-arm
          - runner: windows-latest
          - runner: windows-2022
          - runner: windows-2025
          - runner: windows-2025-vs2026
          - runner: windows-11-arm
          - runner: windows-11-vs2026-arm
          - runner: macos-latest
          - runner: macos-15
          - runner: macos-15-intel
          - runner: macos-26
          - runner: macos-26-intel
          - runner: macos-14
    steps:
      - run: true
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(violations, [])

    def test_workflow_policy_rejects_release_install_dispatch_without_dry_run(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: release
on: workflow_dispatch
jobs:
  verify:
    runs-on: ubuntu-latest
    steps:
      - run: |
          gh workflow run sedna-release-install.yml \\
            -f "release_tag=v0.126.0-sedna.1" \\
            -f "dry_run=false"
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(
            violations,
            [
                ".github/workflows/release.yml: public workflows must dispatch "
                "sedna-release-install.yml with dry_run=true; use external "
                "deployment automation for host-local installs."
            ],
        )

    def test_workflow_policy_accepts_release_install_dry_run_dispatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: release
on: workflow_dispatch
jobs:
  verify:
    runs-on: ubuntu-latest
    steps:
      - run: |
          gh workflow run sedna-release-install.yml \\
            -f "release_tag=v0.126.0-sedna.1" \\
            -f "dry_run=true"
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(violations, [])

    def test_workflow_policy_rejects_release_install_script_without_dry_run(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/install.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: install
on: workflow_dispatch
jobs:
  install:
    runs-on: ubuntu-latest
    steps:
      - run: |
          scripts/install_sedna_release_asset \\
            --repository sednalabs/codex \\
            --release-tag v0.126.0-sedna.1
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(
            violations,
            [
                ".github/workflows/install.yml: public workflows must call "
                "scripts/install_sedna_release_asset with --dry-run; use external "
                "deployment automation for host-local installs."
            ],
        )

    def test_workflow_policy_rejects_write_all_permissions(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/ci.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: ci
on: push
permissions: write-all
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - run: true
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(
            violations,
            [
                ".github/workflows/ci.yml: permissions must not use write-all; "
                "use job-scoped least privilege instead."
            ],
        )

    def test_workflow_policy_rejects_pull_request_target_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/pr.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: pr
on: pull_request_target
permissions:
  contents: read
jobs:
  inspect:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(
            violations,
            [
                ".github/workflows/pr.yml: pull_request_target jobs must not checkout "
                "repository code; split trusted writes from untrusted PR context."
            ],
        )

    def test_workflow_policy_rejects_unscoped_direct_release_create(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: release
on: workflow_dispatch
permissions:
  contents: read
jobs:
  publish:
    runs-on: ubuntu-latest
    steps:
      - run: gh release create "$TAG" dist/*
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(
            violations,
            [
                ".github/workflows/release.yml: job 'publish' creates a GitHub release "
                + "without the release environment.",
                ".github/workflows/release.yml: job 'publish' creates a GitHub release "
                + "without contents: write scoped to the publishing job.",
                ".github/workflows/release.yml: job 'publish' creates a GitHub release "
                + "without id-token: write for release signing or provenance.",
            ],
        )

    def test_workflow_policy_accepts_guarded_direct_release_create(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: release
on: workflow_dispatch
permissions: {}
jobs:
  publish:
    runs-on: ubuntu-latest
    environment: release
    permissions:
      contents: write
      id-token: write
    steps:
      - run: gh release create "$TAG" dist/*
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(violations, [])

    def test_workflow_policy_accepts_app_token_release_create_with_read_only_token(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                """
name: release
on: workflow_dispatch
permissions: {}
jobs:
  publish:
    runs-on: ubuntu-latest
    environment: release
    permissions:
      actions: read
    steps:
      - uses: actions/download-artifact@v8
        with:
          name: release-assets
          path: dist
      - id: release_publisher_token
        uses: actions/create-github-app-token@v3
        with:
          client-id: app-id
          private-key: app-key
          permission-actions: write
          permission-contents: write
      - run: gh release create "$TAG" dist/*
        env:
          GH_TOKEN: ${{ steps.release_publisher_token.outputs.token }}
""".lstrip(),
                encoding="utf-8",
            )

            violations = CHECK_WORKFLOW_POLICY.collect_violations(root)

        self.assertEqual(violations, [])

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

    def test_aggregate_summary_treats_exact_plan_reuse_as_success(self) -> None:
        args = mock.Mock(
            dedupe_should_skip="true",
            dedupe_matched_run_url="https://example.test/runs/42",
        )

        self.assertEqual(
            AGGREGATE_VALIDATION_SUMMARY.overall_conclusion(
                primary=[{"kind": "lane"}],
                secondary=[],
                downstream_result="failure",
                args=args,
            ),
            "success",
        )

    def test_aggregate_summary_marks_stale_frontier_failures_for_targeted_latest_head_proof(self) -> None:
        args = mock.Mock(
            head_sha="1111111111111111111111111111111111111111",
            latest_head_sha="2222222222222222222222222222222222222222",
            smoke_gate_result="success",
            artifact_result="skipped",
            profile="frontier",
        )
        freshness = AGGREGATE_VALIDATION_SUMMARY.classify_head_freshness(
            args,
            queue=[
                {"kind": "lane", "lane_id": "codex.api-client-targeted", "outcome": "failure"},
                {"kind": "lane", "lane_id": "codex.api-types-targeted", "outcome": "failure"},
            ],
            candidate_next_slices=[
                {"kind": "lane", "lane_id": "codex.api-client-targeted", "signal": "API fixture failed"},
                {"kind": "lane", "lane_id": "codex.api-types-targeted", "signal": "API type drift"},
            ],
            downstream_result="failure",
        )

        self.assertEqual(freshness["run_head_status"], "stale")
        self.assertEqual(
            freshness["failed_lane_classification"],
            "needs_targeted_latest_head_proof",
        )
        self.assertEqual(
            freshness["recommended_rerun"]["lane_ids"],
            ["codex.api-client-targeted", "codex.api-types-targeted"],
        )
        self.assertEqual(freshness["recommended_rerun"]["profile"], "targeted")

    def test_aggregate_summary_marks_schema_fixture_failure_for_latest_head_proof(self) -> None:
        args = mock.Mock(
            head_sha="aaaaaaaabbbbbbbbccccccccddddddddeeeeeeee",
            latest_head_sha="ffffffffeeeeeeeeddddddddccccccccbbbbbbbb",
            smoke_gate_result="success",
            artifact_result="skipped",
            profile="targeted",
        )
        freshness = AGGREGATE_VALIDATION_SUMMARY.classify_head_freshness(
            args,
            queue=[
                {
                    "kind": "lane",
                    "lane_id": "codex.app-server-protocol-test",
                    "outcome": "failure",
                }
            ],
            candidate_next_slices=[
                {
                    "kind": "lane",
                    "lane_id": "codex.app-server-protocol-test",
                    "signal": "schema fixture drift",
                }
            ],
            downstream_result="failure",
        )

        self.assertEqual(freshness["run_head_status"], "stale")
        self.assertEqual(
            freshness["failed_lane_classification"],
            "needs_targeted_latest_head_proof",
        )
        self.assertEqual(
            freshness["recommended_rerun"]["lane_ids"],
            ["codex.app-server-protocol-test"],
        )

    def test_aggregate_summary_keeps_unknown_head_failure_active(self) -> None:
        args = mock.Mock(
            head_sha="aaaaaaaabbbbbbbbccccccccddddddddeeeeeeee",
            latest_head_sha="",
            smoke_gate_result="success",
            artifact_result="skipped",
            profile="targeted",
        )
        freshness = AGGREGATE_VALIDATION_SUMMARY.classify_head_freshness(
            args,
            queue=[
                {
                    "kind": "lane",
                    "lane_id": "codex.app-server-protocol-test",
                    "outcome": "failure",
                }
            ],
            candidate_next_slices=[
                {
                    "kind": "lane",
                    "lane_id": "codex.app-server-protocol-test",
                    "signal": "schema fixture drift",
                }
            ],
            downstream_result="failure",
        )

        self.assertEqual(freshness["run_head_status"], "unknown")
        self.assertEqual(freshness["failed_lane_classification"], "active")
        self.assertFalse(freshness["recommended_rerun"]["needed"])

    def test_aggregate_summary_marks_cancelled_release_smoke_as_cancelled(self) -> None:
        args = mock.Mock(
            head_sha="1234567890abcdef1234567890abcdef12345678",
            latest_head_sha="1234567890abcdef1234567890abcdef12345678",
            smoke_gate_result="cancelled",
            artifact_result="skipped",
            profile="checkpoint",
        )
        freshness = AGGREGATE_VALIDATION_SUMMARY.classify_head_freshness(
            args,
            queue=[
                {
                    "kind": "lane",
                    "lane_id": "codex.release-smoke",
                    "outcome": "cancelled",
                }
            ],
            candidate_next_slices=[
                {
                    "kind": "lane",
                    "lane_id": "codex.release-smoke",
                    "signal": "release smoke cancelled",
                }
            ],
            downstream_result="cancelled",
        )

        self.assertEqual(freshness["run_head_status"], "current")
        self.assertEqual(freshness["failed_lane_classification"], "cancelled")
        self.assertTrue(freshness["recommended_rerun"]["needed"])
        self.assertEqual(freshness["recommended_rerun"]["lane_ids"], ["codex.release-smoke"])

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


class ValidationLaneRunnerTests(unittest.TestCase):
    def test_runner_executes_valid_paths_and_rejects_escape_attempts(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            repo_root = Path(tmpdir) / "repo"
            workdir = repo_root / "workdir"
            script_dir = repo_root / ".github/scripts/validation-lanes"
            repo_root.mkdir(parents=True)
            workdir.mkdir(parents=True)
            script_dir.mkdir(parents=True)

            script_path = script_dir / "capture_pwd.sh"
            script_path.write_text(
                "\n".join(
                    [
                        "#!/usr/bin/env bash",
                        "set -euo pipefail",
                        "pwd > ../cwd.txt",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            valid = subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "run_validation_lane.py"),
                    "--repo-root",
                    str(repo_root),
                    "--working-directory",
                    "workdir",
                    "--script-path",
                    ".github/scripts/validation-lanes/capture_pwd.sh",
                    "--script-args-json",
                    "[]",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(valid.returncode, 0, valid.stderr)
            self.assertEqual(
                (repo_root / "cwd.txt").read_text(encoding="utf-8").strip(),
                str(workdir),
            )

            absolute_script = subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "run_validation_lane.py"),
                    "--repo-root",
                    str(repo_root),
                    "--working-directory",
                    "workdir",
                    "--script-path",
                    str(script_path),
                    "--script-args-json",
                    "[]",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(absolute_script.returncode, 0)
            self.assertIn(
                "must be a relative path within the repository root",
                absolute_script.stderr,
            )

            traversal_cwd = subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "run_validation_lane.py"),
                    "--repo-root",
                    str(repo_root),
                    "--working-directory",
                    "../workdir",
                    "--script-path",
                    ".github/scripts/validation-lanes/capture_pwd.sh",
                    "--script-args-json",
                    "[]",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(traversal_cwd.returncode, 0)
            self.assertIn(
                "must not contain '..' path segments",
                traversal_cwd.stderr,
            )


class ValidationLaneBatchRunnerTests(unittest.TestCase):
    def test_batch_runner_loads_catalog_from_target_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            repo_root = root / "repo"
            output_dir = root / "out"
            script_dir = repo_root / ".github/scripts/validation-lanes"
            script_dir.mkdir(parents=True)

            (script_dir / "branch-only.sh").write_text(
                "\n".join(
                    [
                        "#!/usr/bin/env bash",
                        "set -euo pipefail",
                        "echo branch-only-lane-ran",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            (repo_root / ".github/validation-lanes.json").write_text(
                json.dumps(
                    {
                        "lanes": [
                            {
                                "lane_id": "codex.branch-only-targeted",
                                "groups": ["core"],
                                "lane_sets": ["all"],
                                "status_class": "active",
                                "setup_class": "rust_integration",
                                "frontier_default": True,
                                "frontier_role": "sentinel",
                                "summary_family": "branch-only",
                                "cost_class": "high",
                                "checkout_fetch_depth": 1,
                                "timeout_minutes": 30,
                                "working_directory": ".",
                                "script_path": ".github/scripts/validation-lanes/branch-only.sh",
                                "script_args": [],
                                "needs_just": False,
                                "needs_node": False,
                                "needs_nextest": False,
                                "needs_linux_build_deps": False,
                                "needs_dotslash": False,
                                "needs_sccache": False,
                                "needs_bazel": False,
                            }
                        ]
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            fake_bin = root / "bin"
            fake_bin.mkdir()
            fake_df = fake_bin / "df"
            fake_df.write_text(
                "\n".join(
                    [
                        "#!/usr/bin/env bash",
                        "set -euo pipefail",
                        "printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\n'",
                        "printf 'fake 100000000 1 100000000 1%% .\\n'",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            fake_df.chmod(0o755)

            proc = subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "run_validation_lane_batch.py"),
                    "--repo-root",
                    str(repo_root),
                    "--workflow-src",
                    str(REPO_ROOT),
                    "--setup-class",
                    "rust_integration",
                    "--batch-id",
                    "rust_integration-01",
                    "--lane-ids-json",
                    '["codex.branch-only-targeted"]',
                    "--output-dir",
                    str(output_dir),
                ],
                check=False,
                capture_output=True,
                text=True,
                env={**os.environ, "PATH": f"{fake_bin}:{os.environ['PATH']}"},
            )

            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("branch-only-lane-ran", proc.stdout)
            results = json.loads((output_dir / "batch-results.json").read_text(encoding="utf-8"))
            self.assertEqual(
                results["results"][0]["lane_id"],
                "codex.branch-only-targeted",
            )

    def test_batch_runner_reclaims_disk_headroom_before_first_lane(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            repo_root = root / "repo"
            output_dir = root / "out"
            script_dir = repo_root / ".github/scripts/validation-lanes"
            script_dir.mkdir(parents=True)

            (script_dir / "assert-clean-target.sh").write_text(
                "\n".join(
                    [
                        "#!/usr/bin/env bash",
                        "set -euo pipefail",
                        "test ! -e codex-rs/target/stale-artifact",
                        "echo first-lane-target-was-reclaimed",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            (repo_root / ".github/validation-lanes.json").write_text(
                json.dumps(
                    {
                        "lanes": [
                            {
                                "lane_id": "codex.low-disk-targeted",
                                "groups": ["core"],
                                "lane_sets": ["all"],
                                "status_class": "active",
                                "setup_class": "rust_integration",
                                "frontier_default": True,
                                "frontier_role": "sentinel",
                                "summary_family": "low-disk",
                                "cost_class": "high",
                                "checkout_fetch_depth": 1,
                                "timeout_minutes": 30,
                                "working_directory": ".",
                                "script_path": ".github/scripts/validation-lanes/assert-clean-target.sh",
                                "script_args": [],
                                "needs_just": False,
                                "needs_node": False,
                                "needs_nextest": False,
                                "needs_linux_build_deps": False,
                                "needs_dotslash": False,
                                "needs_sccache": False,
                                "needs_bazel": False,
                            }
                        ]
                    }
                )
                + "\n",
                encoding="utf-8",
            )

            subprocess.run(
                ["git", "init", "--initial-branch=main"],
                cwd=repo_root,
                check=True,
                stdout=subprocess.DEVNULL,
            )
            subprocess.run(
                ["git", "config", "user.name", "CI Planner Tests"],
                cwd=repo_root,
                check=True,
            )
            subprocess.run(
                ["git", "config", "user.email", "ci-planner-tests@example.invalid"],
                cwd=repo_root,
                check=True,
            )
            subprocess.run(["git", "add", "."], cwd=repo_root, check=True)
            subprocess.run(
                ["git", "commit", "-m", "initial"],
                cwd=repo_root,
                check=True,
                stdout=subprocess.DEVNULL,
            )

            target_dir = repo_root / "codex-rs/target"
            target_dir.mkdir(parents=True)
            (target_dir / "stale-artifact").write_text("stale\n", encoding="utf-8")
            fake_bin = root / "bin"
            fake_bin.mkdir()
            fake_df = fake_bin / "df"
            fake_df.write_text(
                "\n".join(
                    [
                        "#!/usr/bin/env bash",
                        "set -euo pipefail",
                        "printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\n'",
                        "printf 'fake 1 1 0 100%% .\\n'",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            fake_df.chmod(0o755)

            proc = subprocess.run(
                [
                    "python3",
                    str(SCRIPTS_DIR / "run_validation_lane_batch.py"),
                    "--repo-root",
                    str(repo_root),
                    "--workflow-src",
                    str(REPO_ROOT),
                    "--setup-class",
                    "rust_integration",
                    "--batch-id",
                    "rust_integration-01",
                    "--lane-ids-json",
                    '["codex.low-disk-targeted"]',
                    "--output-dir",
                    str(output_dir),
                ],
                check=False,
                capture_output=True,
                text=True,
                env={**os.environ, "PATH": f"{fake_bin}:{os.environ['PATH']}"},
            )

            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("first-lane-target-was-reclaimed", proc.stdout)
            self.assertIn(
                "Reclaiming validation workspace disk headroom",
                proc.stdout,
            )


class SednaReleaseVersionResolverTests(unittest.TestCase):
    def create_fixture(
        self,
        marker: str | None = "Sedna-Release: prerelease",
    ) -> tuple[TempGitRepo, str, str]:
        repo = TempGitRepo()
        old_upstream = repo.commit("upstream stable", {"upstream.txt": "stable\n"})
        repo._git("tag", "rust-v0.124.0", old_upstream)
        repo._git("tag", "rust-vv9.999.0", old_upstream)
        upstream = repo.commit("upstream alpha", {"upstream.txt": "alpha\n"})
        repo._git("tag", "rust-v0.126.0-alpha.3", upstream)
        repo._git("update-ref", "refs/remotes/origin/upstream-main", upstream)

        message = "downstream release"
        if marker is not None:
            message = f"{message}\n\n{marker}"
        downstream = repo.commit(message, {"downstream.txt": "release\n"})
        repo._git("update-ref", "refs/remotes/origin/main", downstream)
        return repo, upstream, downstream

    def resolve(
        self,
        repo: TempGitRepo,
        target: str,
        *,
        channel: str = "prerelease",
        release_tag: str | None = None,
        current_release_tag: str | None = None,
        require_marker: bool = False,
        missing_marker: str = "skip",
    ) -> dict:
        return RESOLVE_SEDNA_RELEASE_VERSION.resolve_release(
            repo=repo.root,
            target_sha=target,
            main_ref="refs/remotes/origin/main",
            upstream_ref="refs/remotes/origin/upstream-main",
            repository="",
            channel=channel,
            release_tag=release_tag,
            current_release_tag=current_release_tag,
            require_marker=require_marker,
            missing_marker=missing_marker,
            github_releases="off",
        )

    def test_prerelease_marker_computes_next_sedna_ordinal(self) -> None:
        repo, upstream, downstream = self.create_fixture()
        try:
            repo._git("tag", "v0.126.0-alpha.3-sedna.1", downstream)
            result = self.resolve(repo, downstream, channel="auto", require_marker=True)
        finally:
            repo.cleanup()

        self.assertEqual(
            {
                "release_requested": result["release_requested"],
                "release_channel": result["release_channel"],
                "release_tag": result["release_tag"],
                "release_version": result["release_version"],
                "github_prerelease": result["github_prerelease"],
                "upstream_track": result["upstream_track"],
                "upstream_base_tag": result["upstream_base_tag"],
                "upstream_position": result["upstream_position"],
                "build_provenance": result["build_provenance"],
            },
            {
                "release_requested": True,
                "release_channel": "prerelease",
                "release_tag": "v0.126.0-alpha.3-sedna.2",
                "release_version": "0.126.0-alpha.3-sedna.2",
                "github_prerelease": True,
                "upstream_track": "0.126.0-alpha.3",
                "upstream_base_tag": "rust-v0.126.0-alpha.3",
                "upstream_position": f"rust-v0.126.0-alpha.3@{upstream[:8]}",
                "build_provenance": f"up:rust-v0.126.0-alpha.3@{upstream[:8]} down:{downstream[:8]}",
            },
        )

    def test_upstream_position_marks_commits_after_upstream_tag(self) -> None:
        repo = TempGitRepo()
        tagged_upstream = repo.commit("upstream alpha tag", {"upstream.txt": "alpha\n"})
        repo._git("tag", "rust-v0.126.0-alpha.3", tagged_upstream)
        upstream_plus_one = repo.commit(
            "upstream alpha follow-up",
            {"upstream.txt": "alpha\nfollow-up\n"},
        )
        repo._git("update-ref", "refs/remotes/origin/upstream-main", upstream_plus_one)
        downstream = repo.commit(
            "downstream release\n\nSedna-Release: prerelease",
            {"downstream.txt": "release\n"},
        )
        repo._git("update-ref", "refs/remotes/origin/main", downstream)
        try:
            result = self.resolve(repo, downstream, channel="auto", require_marker=True)
        finally:
            repo.cleanup()

        self.assertEqual(result["upstream_distance_from_tag"], 1)
        self.assertEqual(result["upstream_base_tag_exact"], False)
        self.assertEqual(
            result["upstream_position"],
            f"rust-v0.126.0-alpha.3+1@{upstream_plus_one[:8]}",
        )
        self.assertEqual(
            result["release_tag"],
            "v0.126.0-alpha.3-sedna.1+upstream.1",
        )
        self.assertEqual(
            result["version_display"],
            "0.126.0-alpha.3-sedna.1+upstream.1 "
            f"(up:rust-v0.126.0-alpha.3+1@{upstream_plus_one[:8]} down:{downstream[:8]})",
        )

    def test_consecutive_release_markers_get_deterministic_ordinals(self) -> None:
        repo, _upstream, first_release = self.create_fixture()
        second_release = repo.commit(
            "second downstream release\n\nSedna-Release: prerelease",
            {"downstream.txt": "second release\n"},
        )
        repo._git("update-ref", "refs/remotes/origin/main", second_release)
        try:
            first_result = self.resolve(
                repo,
                first_release,
                channel="auto",
                require_marker=True,
            )
            second_result = self.resolve(
                repo,
                second_release,
                channel="auto",
                require_marker=True,
            )
        finally:
            repo.cleanup()

        self.assertEqual(
            {
                "first": first_result["release_tag"],
                "second": second_result["release_tag"],
            },
            {
                "first": "v0.126.0-alpha.3-sedna.1",
                "second": "v0.126.0-alpha.3-sedna.2",
            },
        )

    def test_release_marker_after_existing_tag_uses_next_ordinal(self) -> None:
        repo, _upstream, first_release = self.create_fixture()
        second_release = repo.commit(
            "second downstream release\n\nSedna-Release: prerelease",
            {"downstream.txt": "second release\n"},
        )
        repo._git("tag", "v0.126.0-alpha.3-sedna.1", first_release)
        repo._git("update-ref", "refs/remotes/origin/main", second_release)
        try:
            result = self.resolve(
                repo,
                second_release,
                channel="auto",
                require_marker=True,
            )
        finally:
            repo.cleanup()

        self.assertEqual(result["release_tag"], "v0.126.0-alpha.3-sedna.2")

    def test_supplied_tag_can_assert_second_pending_release_marker(self) -> None:
        repo, _upstream, _first_release = self.create_fixture()
        second_release = repo.commit(
            "second downstream release\n\nSedna-Release: prerelease",
            {"downstream.txt": "second release\n"},
        )
        repo._git("update-ref", "refs/remotes/origin/main", second_release)
        try:
            result = self.resolve(
                repo,
                second_release,
                channel="auto",
                release_tag="v0.126.0-alpha.3-sedna.2",
            )
        finally:
            repo.cleanup()

        self.assertEqual(result["release_tag"], "v0.126.0-alpha.3-sedna.2")

    def test_manual_markerless_release_without_tag_can_error(self) -> None:
        repo, _upstream, downstream = self.create_fixture(marker=None)
        try:
            with self.assertRaisesRegex(
                RESOLVE_SEDNA_RELEASE_VERSION.ReleaseVersionError,
                "manual markerless releases must supply release_tag",
            ):
                self.resolve(
                    repo,
                    downstream,
                    channel="auto",
                    require_marker=True,
                    missing_marker="error",
                )
        finally:
            repo.cleanup()

    def test_manual_auto_channel_infers_prerelease_from_upstream_track(self) -> None:
        repo, _upstream, downstream = self.create_fixture(marker=None)
        try:
            result = self.resolve(repo, downstream, channel="auto")
            with self.assertRaisesRegex(
                RESOLVE_SEDNA_RELEASE_VERSION.ReleaseVersionError,
                "does not match computed tag v0.126.0-alpha.3-sedna.1",
            ):
                self.resolve(
                    repo,
                    downstream,
                    channel="prerelease",
                    release_tag="v0.126.0-alpha.4-sedna.1",
                )
        finally:
            repo.cleanup()

        self.assertEqual(
            {
                "release_channel": result["release_channel"],
                "release_tag": result["release_tag"],
                "github_prerelease": result["github_prerelease"],
            },
            {
                "release_channel": "prerelease",
                "release_tag": "v0.126.0-alpha.3-sedna.1",
                "github_prerelease": True,
            },
        )

    def test_future_upstream_tag_is_not_used_for_older_synced_upstream_base(self) -> None:
        repo, upstream, downstream = self.create_fixture(marker=None)
        try:
            repo._git(
                "tag",
                "-a",
                "rust-v0.127.0-alpha.1",
                upstream,
                "-m",
                "Release 0.127.0-alpha.1",
                env={"GIT_COMMITTER_DATE": "2099-01-01T00:00:00+00:00"},
            )
            result = self.resolve(repo, downstream, channel="auto")
        finally:
            repo.cleanup()

        self.assertEqual(
            {
                "release_tag": result["release_tag"],
                "upstream_track": result["upstream_track"],
                "upstream_base_tag": result["upstream_base_tag"],
            },
            {
                "release_tag": "v0.126.0-alpha.3-sedna.1",
                "upstream_track": "0.126.0-alpha.3",
                "upstream_base_tag": "rust-v0.126.0-alpha.3",
            },
        )

    def test_upstream_mirror_ahead_of_target_does_not_advance_release_track(self) -> None:
        repo, upstream, downstream = self.create_fixture(marker=None)
        try:
            repo._git("checkout", "-B", "upstream-main-fixture", upstream)
            future_upstream = repo.commit(
                "upstream newer alpha",
                {"upstream.txt": "newer alpha\n"},
            )
            repo._git(
                "tag",
                "-a",
                "rust-v0.126.0-alpha.4",
                future_upstream,
                "-m",
                "Release 0.126.0-alpha.4",
                env={"GIT_COMMITTER_DATE": "2099-01-01T00:00:00+00:00"},
            )
            repo._git("update-ref", "refs/remotes/origin/upstream-main", future_upstream)
            repo._git("checkout", "main")

            result = self.resolve(repo, downstream, channel="auto")
        finally:
            repo.cleanup()

        self.assertEqual(
            {
                "release_tag": result["release_tag"],
                "upstream_track": result["upstream_track"],
                "upstream_base_commit": result["upstream_base_commit"],
                "upstream_base_tag": result["upstream_base_tag"],
            },
            {
                "release_tag": "v0.126.0-alpha.3-sedna.1",
                "upstream_track": "0.126.0-alpha.3",
                "upstream_base_commit": upstream,
                "upstream_base_tag": "rust-v0.126.0-alpha.3",
            },
        )

    def test_stable_channel_rejects_upstream_prerelease_track(self) -> None:
        repo, _upstream, downstream = self.create_fixture("Sedna-Release: stable")
        try:
            with self.assertRaisesRegex(
                RESOLVE_SEDNA_RELEASE_VERSION.ReleaseVersionError,
                "stable Sedna releases cannot use prerelease upstream track",
            ):
                self.resolve(repo, downstream, channel="auto", require_marker=True)
        finally:
            repo.cleanup()

    def test_missing_release_marker_is_clean_noop_for_main_pushes(self) -> None:
        repo, _upstream, downstream = self.create_fixture(marker=None)
        try:
            result = self.resolve(repo, downstream, channel="auto", require_marker=True)
        finally:
            repo.cleanup()

        self.assertEqual(
            result,
            {
                "release_requested": False,
                "skip_reason": "missing_sedna_release_marker",
                "target_commit": downstream,
                "version_policy": "sedna-upstream-track-v2",
            },
        )

    def test_supplied_tag_must_match_computed_tag(self) -> None:
        repo, _upstream, downstream = self.create_fixture()
        try:
            with self.assertRaisesRegex(
                RESOLVE_SEDNA_RELEASE_VERSION.ReleaseVersionError,
                "does not match computed tag v0.126.0-alpha.3-sedna.1",
            ):
                self.resolve(
                    repo,
                    downstream,
                    channel="prerelease",
                    release_tag="v0.126.0-alpha.3-sedna.2",
                )
        finally:
            repo.cleanup()

    def test_current_tag_is_ignored_for_tag_push_validation(self) -> None:
        repo, _upstream, downstream = self.create_fixture()
        try:
            repo._git("tag", "v0.126.0-alpha.3-sedna.1", downstream)
            result = self.resolve(
                repo,
                downstream,
                channel="auto",
                release_tag="v0.126.0-alpha.3-sedna.1",
                current_release_tag="v0.126.0-alpha.3-sedna.1",
            )
        finally:
            repo.cleanup()

        self.assertEqual(result["release_tag"], "v0.126.0-alpha.3-sedna.1")

    def test_existing_supplied_tag_for_target_can_be_released(self) -> None:
        repo, _upstream, downstream = self.create_fixture()
        try:
            repo._git("tag", "v0.126.0-alpha.3-sedna.1", downstream)
            result = self.resolve(
                repo,
                downstream,
                channel="prerelease",
                release_tag="v0.126.0-alpha.3-sedna.1",
            )
        finally:
            repo.cleanup()

        self.assertEqual(result["release_tag"], "v0.126.0-alpha.3-sedna.1")

    def test_existing_supplied_tag_must_point_at_target(self) -> None:
        repo, _upstream, downstream = self.create_fixture()
        try:
            other = repo.commit("other downstream", {"downstream.txt": "other\n"})
            repo._git("tag", "v0.126.0-alpha.3-sedna.1", other)
            with self.assertRaisesRegex(
                RESOLVE_SEDNA_RELEASE_VERSION.ReleaseVersionError,
                "not target commit",
            ):
                self.resolve(
                    repo,
                    downstream,
                    channel="prerelease",
                    release_tag="v0.126.0-alpha.3-sedna.1",
                )
        finally:
            repo.cleanup()

    def test_current_release_tag_is_ignored_after_remote_release_union(self) -> None:
        repo, _upstream, downstream = self.create_fixture()
        try:
            with mock.patch.object(
                RESOLVE_SEDNA_RELEASE_VERSION,
                "github_release_tags",
                return_value={"v0.126.0-alpha.3-sedna.1"},
            ):
                result = self.resolve(
                    repo,
                    downstream,
                    channel="auto",
                    release_tag="v0.126.0-alpha.3-sedna.1",
                    current_release_tag="v0.126.0-alpha.3-sedna.1",
                )
        finally:
            repo.cleanup()

        self.assertEqual(result["release_tag"], "v0.126.0-alpha.3-sedna.1")


if __name__ == "__main__":
    unittest.main()
