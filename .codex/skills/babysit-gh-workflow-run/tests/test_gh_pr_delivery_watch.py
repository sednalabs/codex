import importlib.util
import json
import os
import subprocess
import sys
import types
import unittest
from pathlib import Path
from unittest.mock import patch

MODULE_PATH = Path(
    os.environ.get(
        "GH_PR_DELIVERY_WATCH_MODULE_PATH",
        str(
            Path(__file__).resolve().parents[1] / "scripts" / "gh_pr_delivery_watch.py"
        ),
    )
)
SPEC = importlib.util.spec_from_file_location("gh_pr_delivery_watch", MODULE_PATH)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


EXPECTED_HEAD_SHA = "a" * 40
CURRENT_BASE_SHA = "b" * 40
CANDIDATE_SHA = "c" * 40
MERGE_COMMIT_SHA = "d" * 40
OTHER_SHA = "e" * 40
STALE_CANDIDATE_SHA = "f" * 40


def make_args(**overrides):
    values = {
        "repo": "owner/repo",
        "pr": 17,
        "expected_head_sha": EXPECTED_HEAD_SHA,
        "main_ref": "main",
        "merge_group_run_id": 901,
        "merge_group_workflow": "blocking-ci",
        "post_merge_workflow": "postmerge-ci",
        "poll_seconds": 5,
        "appearance_timeout_seconds": 900,
        "merge_observation_timeout_seconds": 300,
        "retry_settle_seconds": 0,
    }
    values.update(overrides)
    return types.SimpleNamespace(**values)


def make_pr(
    *,
    head_sha=EXPECTED_HEAD_SHA,
    base_sha=CURRENT_BASE_SHA,
    merged=False,
    queue_entry_id="entry",
    merge_commit_sha="",
):
    return {
        "number": 17,
        "head_sha": head_sha,
        "base_ref": "main",
        "base_sha": base_sha,
        "merged": merged,
        "merge_queue_entry_id": queue_entry_id,
        "merge_commit_sha": merge_commit_sha,
    }


def candidate_association():
    return patch.object(
        MODULE,
        "verify_candidate_association",
        return_value={
            "expected_pr_head_sha": EXPECTED_HEAD_SHA,
            "expected_base_sha": CURRENT_BASE_SHA,
            "candidate_contains_expected_head": True,
            "candidate_contains_current_base": True,
        },
    )


def candidate_merge_correlation():
    return patch.object(
        MODULE,
        "verify_selected_candidate_merged",
        return_value={
            "candidate_sha": CANDIDATE_SHA,
            "merge_commit_sha": MERGE_COMMIT_SHA,
            "candidate_reaches_merge_commit": True,
        },
    )


def make_candidate(*, run_id=901, candidate_sha=CANDIDATE_SHA):
    return {
        "id": run_id,
        "attempt": 1,
        "workflow": "blocking-ci",
        "url": f"https://example.invalid/runs/{run_id}",
        "event": "merge_group",
        "head_branch": "gh-readonly-queue/main/pr-17-abcdef",
        "head_sha": candidate_sha,
        "status": "queued",
        "conclusion": "",
    }


def make_watcher_receipt(
    *,
    run_id,
    workflow,
    event,
    branch,
    sha,
    conclusion="success",
    failed_jobs=None,
):
    return {
        "targets": [
            {
                "run": {
                    "id": run_id,
                    "attempt": 1,
                    "workflow_name": workflow,
                    "url": f"https://example.invalid/runs/{run_id}",
                    "event": event,
                    "head_branch": branch,
                    "head_sha": sha,
                    "status": "completed",
                    "conclusion": conclusion,
                },
                "failed_jobs": failed_jobs or [],
                "actions": ["stop_run_succeeded"]
                if conclusion == "success"
                else ["diagnose_run_failure"],
            }
        ]
    }


class PullRequestDeliveryWatchTests(unittest.TestCase):
    def test_success_receipt_proves_all_three_identities(self):
        args = make_args()
        candidate = make_candidate()
        candidate_receipt = make_watcher_receipt(
            run_id=901,
            workflow="blocking-ci",
            event="merge_group",
            branch=candidate["head_branch"],
            sha=CANDIDATE_SHA,
        )
        post_merge_receipt = make_watcher_receipt(
            run_id=902,
            workflow="postmerge-ci",
            event="push",
            branch="main",
            sha=MERGE_COMMIT_SHA,
        )

        with (
            patch.object(
                MODULE,
                "fetch_pr",
                side_effect=[
                    make_pr(),
                    make_pr(),
                    make_pr(
                        merged=True,
                        queue_entry_id=None,
                        merge_commit_sha=MERGE_COMMIT_SHA,
                    ),
                    make_pr(
                        merged=True,
                        queue_entry_id=None,
                        merge_commit_sha=MERGE_COMMIT_SHA,
                    ),
                ],
            ) as fetch_pr,
            patch.object(
                MODULE, "resolve_merge_group_candidate", return_value=candidate
            ),
            patch.object(MODULE, "fetch_actions_run", return_value=candidate),
            patch.object(MODULE, "list_merge_group_runs", return_value=[candidate]),
            candidate_association(),
            patch.object(
                MODULE,
                "run_blocking_watcher",
                side_effect=[candidate_receipt, post_merge_receipt],
            ) as watcher,
            patch.object(MODULE, "fetch_ref_sha", return_value=MERGE_COMMIT_SHA),
            patch.object(MODULE, "is_commit_reachable_from_main", return_value=True),
            candidate_merge_correlation(),
            patch.object(MODULE.time, "sleep") as sleep,
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 0)
        self.assertEqual(receipt["actions"], ["stop_pr_delivery_proven"])
        self.assertEqual(
            receipt["proof_scope"],
            {
                "kind": "selected_workflow_delivery",
                "merge_group_workflow": "blocking-ci",
                "selected_post_merge_workflows": ["postmerge-ci"],
                "whole_repository_health_proven": False,
            },
        )
        self.assertEqual(receipt["pr"]["expected_head_sha"], EXPECTED_HEAD_SHA)
        self.assertEqual(receipt["merge_group"]["candidate_sha"], CANDIDATE_SHA)
        self.assertTrue(
            receipt["merge_group"]["merge_correlation"][
                "candidate_reaches_merge_commit"
            ]
        )
        self.assertEqual(receipt["merge_group"]["run"]["id"], 901)
        self.assertEqual(receipt["merge_commit"]["sha"], MERGE_COMMIT_SHA)
        self.assertEqual(receipt["post_merge"]["run"]["id"], 902)
        self.assertEqual(receipt["post_merge"]["selected_workflow"], "postmerge-ci")
        self.assertEqual(watcher.call_count, 2)
        self.assertEqual(fetch_pr.call_count, 4)
        sleep.assert_called_once_with(args.poll_seconds)
        self.assertEqual(
            watcher.call_args_list[0].args[1], f"run-id=901,head-sha={CANDIDATE_SHA}"
        )
        self.assertEqual(
            watcher.call_args_list[1].args[1],
            f"workflow=postmerge-ci,ref=main,head-sha={MERGE_COMMIT_SHA}",
        )

    def test_changed_pr_head_fails_closed_after_merge_group_run(self):
        args = make_args()
        candidate = make_candidate()
        candidate_receipt = make_watcher_receipt(
            run_id=901,
            workflow="blocking-ci",
            event="merge_group",
            branch=candidate["head_branch"],
            sha=CANDIDATE_SHA,
        )

        with (
            patch.object(
                MODULE,
                "fetch_pr",
                side_effect=[make_pr(), make_pr(head_sha=OTHER_SHA)],
            ),
            patch.object(
                MODULE, "resolve_merge_group_candidate", return_value=candidate
            ),
            candidate_association(),
            patch.object(
                MODULE, "run_blocking_watcher", return_value=candidate_receipt
            ),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(receipt["actions"], ["stop_pr_head_changed"])
        self.assertIsNone(receipt["merge_commit"])

    def test_absent_candidate_fails_before_any_blocking_wait(self):
        args = make_args(merge_group_run_id=None)
        with self.assertRaisesRegex(
            MODULE.DeliveryStop, "no merge-queue entry"
        ) as raised:
            MODULE.resolve_merge_group_candidate(
                "owner/repo", make_pr(queue_entry_id=None), args
            )
        self.assertEqual(raised.exception.action, "stop_merge_group_candidate_absent")

    def test_ambiguous_candidate_shas_fail_closed(self):
        args = make_args(merge_group_run_id=None)
        candidate_a = make_candidate(run_id=901, candidate_sha=CANDIDATE_SHA)
        candidate_b = make_candidate(run_id=902, candidate_sha=OTHER_SHA)
        with (
            patch.object(
                MODULE, "list_merge_group_runs", return_value=[candidate_a, candidate_b]
            ),
            self.assertRaisesRegex(
                MODULE.DeliveryStop, "multiple merge-group candidate SHAs"
            ) as raised,
        ):
            MODULE.resolve_merge_group_candidate("owner/repo", make_pr(), args)
        self.assertEqual(
            raised.exception.action, "stop_merge_group_candidate_ambiguous"
        )

    def test_candidate_for_stale_head_is_rejected_against_current_head_and_base(self):
        args = make_args()
        current_pr = make_pr(head_sha=EXPECTED_HEAD_SHA, base_sha=CURRENT_BASE_SHA)
        # The candidate represents a previous H1/base queue attempt: H2 is not
        # an ancestor, although the current base still is.
        with (
            patch.object(
                MODULE,
                "fetch_pr",
                return_value=current_pr,
            ),
            patch.object(
                MODULE,
                "resolve_merge_group_candidate",
                return_value=make_candidate(candidate_sha=STALE_CANDIDATE_SHA),
            ),
            patch.object(
                MODULE,
                "is_commit_ancestor",
                side_effect=[False, True],
            ),
            patch.object(MODULE, "run_blocking_watcher") as watcher,
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(receipt["actions"], ["stop_merge_group_candidate_stale"])
        self.assertEqual(receipt["merge_group"]["candidate_sha"], STALE_CANDIDATE_SHA)
        watcher.assert_not_called()

    def test_commit_ancestor_accepts_ahead_compare_status(self):
        with patch.object(MODULE, "gh_json", return_value={"status": "ahead"}):
            self.assertTrue(
                MODULE.is_commit_ancestor(
                    "owner/repo", EXPECTED_HEAD_SHA, CANDIDATE_SHA
                )
            )

    def test_workflow_matching_accepts_file_paths_and_display_names(self):
        self.assertTrue(
            MODULE.workflow_matches(".github/workflows/blocking-ci.yml", "Blocking CI")
        )
        self.assertTrue(MODULE.workflow_matches("postmerge-ci.yaml", "Postmerge CI"))

    def test_receipt_scope_preserves_the_exact_selected_workflow_selector(self):
        receipt = MODULE.new_receipt(
            "owner/repo",
            make_args(post_merge_workflow=".github/workflows/postmerge-ci.yml"),
        )

        self.assertEqual(
            receipt["proof_scope"]["selected_post_merge_workflows"],
            [".github/workflows/postmerge-ci.yml"],
        )
        self.assertFalse(receipt["proof_scope"]["whole_repository_health_proven"])

    def test_candidate_and_watched_run_accept_workflow_file_and_display_name(self):
        candidate = make_candidate()
        candidate["workflow"] = "Blocking CI"
        args = make_args(merge_group_workflow=".github/workflows/blocking-ci.yml")
        self.assertEqual(
            MODULE.verify_merge_group_candidate(candidate, args), candidate
        )

        receipt = make_watcher_receipt(
            run_id=902,
            workflow="Postmerge CI",
            event="push",
            branch="main",
            sha=MERGE_COMMIT_SHA,
        )
        watched = MODULE.verify_watched_run(
            receipt,
            stage="post_merge",
            expected_sha=MERGE_COMMIT_SHA,
            expected_event="push",
            expected_branch="main",
            expected_workflow=".github/workflows/postmerge-ci.yml",
        )
        self.assertEqual(watched["outcome"], "success")

    def test_queue_entry_disappearing_without_merge_is_a_distinct_stop(self):
        args = make_args()
        candidate = make_candidate()
        candidate_receipt = make_watcher_receipt(
            run_id=901,
            workflow="blocking-ci",
            event="merge_group",
            branch=candidate["head_branch"],
            sha=CANDIDATE_SHA,
        )
        with (
            patch.object(
                MODULE,
                "fetch_pr",
                side_effect=[make_pr(), make_pr(merged=False, queue_entry_id=None)],
            ),
            patch.object(
                MODULE, "resolve_merge_group_candidate", return_value=candidate
            ),
            candidate_association(),
            patch.object(
                MODULE, "run_blocking_watcher", return_value=candidate_receipt
            ),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(
            receipt["actions"], ["stop_merge_queue_entry_disappeared_without_merge"]
        )

    def test_delivery_wait_observes_a_delayed_merge_after_queue_success(self):
        args = make_args()
        candidate = make_candidate()
        association = {
            "expected_pr_head_sha": EXPECTED_HEAD_SHA,
            "expected_base_sha": CURRENT_BASE_SHA,
            "candidate_contains_expected_head": True,
            "candidate_contains_current_base": True,
        }
        merged_pr = make_pr(
            merged=True,
            queue_entry_id=None,
            merge_commit_sha=MERGE_COMMIT_SHA,
        )

        with (
            patch.object(MODULE, "fetch_pr", side_effect=[make_pr(), merged_pr]),
            patch.object(MODULE, "fetch_actions_run", return_value=candidate),
            patch.object(MODULE, "list_merge_group_runs", return_value=[candidate]),
            patch.object(
                MODULE, "verify_candidate_association", return_value=association
            ),
            patch.object(MODULE.time, "sleep") as sleep,
        ):
            observed_pr, observed_association = MODULE.wait_for_pr_delivery(
                "owner/repo", candidate, args
            )

        self.assertEqual(observed_pr, merged_pr)
        self.assertEqual(observed_association, association)
        sleep.assert_called_once_with(args.poll_seconds)

    def test_delivery_wait_times_out_with_a_stable_stop(self):
        args = make_args(merge_observation_timeout_seconds=0)
        candidate = make_candidate()
        candidate_receipt = make_watcher_receipt(
            run_id=901,
            workflow="blocking-ci",
            event="merge_group",
            branch=candidate["head_branch"],
            sha=CANDIDATE_SHA,
        )
        with (
            patch.object(MODULE, "fetch_pr", side_effect=[make_pr(), make_pr()]),
            patch.object(
                MODULE, "resolve_merge_group_candidate", return_value=candidate
            ),
            patch.object(MODULE, "fetch_actions_run", return_value=candidate),
            patch.object(MODULE, "list_merge_group_runs", return_value=[candidate]),
            candidate_association(),
            patch.object(
                MODULE, "run_blocking_watcher", return_value=candidate_receipt
            ),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(receipt["actions"], ["stop_merge_observation_timeout"])
        self.assertFalse(receipt["proof_scope"]["whole_repository_health_proven"])

    def test_delivery_wait_rejects_a_later_candidate_as_superseded(self):
        args = make_args()
        candidate = make_candidate()
        newer_candidate = make_candidate(run_id=902, candidate_sha=OTHER_SHA)
        with (
            patch.object(MODULE, "fetch_pr", return_value=make_pr()),
            patch.object(MODULE, "fetch_actions_run", return_value=candidate),
            patch.object(
                MODULE,
                "list_merge_group_runs",
                return_value=[candidate, newer_candidate],
            ),
            patch.object(MODULE, "verify_candidate_association", return_value={}),
            self.assertRaisesRegex(MODULE.DeliveryStop, "superseded") as raised,
        ):
            MODULE.wait_for_pr_delivery("owner/repo", candidate, args)

        self.assertEqual(
            raised.exception.action, "stop_merge_group_candidate_superseded"
        )

    def test_delivery_wait_read_error_emits_an_operator_receipt(self):
        args = make_args()
        candidate = make_candidate()
        candidate_receipt = make_watcher_receipt(
            run_id=901,
            workflow="blocking-ci",
            event="merge_group",
            branch=candidate["head_branch"],
            sha=CANDIDATE_SHA,
        )
        with (
            patch.object(
                MODULE,
                "fetch_pr",
                side_effect=[make_pr(), MODULE.GhCommandError("read failed")],
            ),
            patch.object(
                MODULE, "resolve_merge_group_candidate", return_value=candidate
            ),
            candidate_association(),
            patch.object(
                MODULE, "run_blocking_watcher", return_value=candidate_receipt
            ),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(receipt["actions"], ["stop_operator_help_required"])
        self.assertIn("read failed", receipt["error"])

    def test_merged_pr_without_a_correlatable_commit_fails_closed(self):
        args = make_args()
        candidate = make_candidate()
        candidate_receipt = make_watcher_receipt(
            run_id=901,
            workflow="blocking-ci",
            event="merge_group",
            branch=candidate["head_branch"],
            sha=CANDIDATE_SHA,
        )
        with (
            patch.object(
                MODULE,
                "fetch_pr",
                side_effect=[make_pr(), make_pr(merged=True, queue_entry_id=None)],
            ),
            patch.object(
                MODULE, "resolve_merge_group_candidate", return_value=candidate
            ),
            candidate_association(),
            patch.object(
                MODULE, "run_blocking_watcher", return_value=candidate_receipt
            ),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(receipt["actions"], ["stop_merge_commit_uncorrelatable"])

    def test_superseded_candidate_cannot_claim_a_later_merge(self):
        args = make_args()
        candidate = make_candidate()
        candidate_receipt = make_watcher_receipt(
            run_id=901,
            workflow="blocking-ci",
            event="merge_group",
            branch=candidate["head_branch"],
            sha=CANDIDATE_SHA,
        )
        with (
            patch.object(
                MODULE,
                "fetch_pr",
                side_effect=[
                    make_pr(),
                    make_pr(
                        merged=True,
                        queue_entry_id=None,
                        merge_commit_sha=MERGE_COMMIT_SHA,
                    ),
                ],
            ),
            patch.object(
                MODULE, "resolve_merge_group_candidate", return_value=candidate
            ),
            candidate_association(),
            patch.object(
                MODULE, "run_blocking_watcher", return_value=candidate_receipt
            ) as watcher,
            patch.object(MODULE, "fetch_ref_sha", return_value=MERGE_COMMIT_SHA),
            patch.object(MODULE, "is_commit_reachable_from_main", return_value=True),
            patch.object(MODULE, "is_commit_ancestor", return_value=False),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(receipt["actions"], ["stop_merge_group_candidate_not_merged"])
        self.assertEqual(
            receipt["merge_group"]["merge_correlation"],
            {"candidate_sha": CANDIDATE_SHA, "merge_commit_sha": MERGE_COMMIT_SHA},
        )
        self.assertEqual(watcher.call_count, 1)

    def test_post_merge_run_sha_mismatch_is_rejected_even_after_merge(self):
        args = make_args()
        candidate = make_candidate()
        candidate_receipt = make_watcher_receipt(
            run_id=901,
            workflow="blocking-ci",
            event="merge_group",
            branch=candidate["head_branch"],
            sha=CANDIDATE_SHA,
        )
        wrong_post_merge_receipt = make_watcher_receipt(
            run_id=902,
            workflow="postmerge-ci",
            event="push",
            branch="main",
            sha=OTHER_SHA,
        )
        with (
            patch.object(
                MODULE,
                "fetch_pr",
                side_effect=[
                    make_pr(),
                    make_pr(
                        merged=True,
                        queue_entry_id=None,
                        merge_commit_sha=MERGE_COMMIT_SHA,
                    ),
                ],
            ),
            patch.object(
                MODULE, "resolve_merge_group_candidate", return_value=candidate
            ),
            candidate_association(),
            patch.object(
                MODULE,
                "run_blocking_watcher",
                side_effect=[candidate_receipt, wrong_post_merge_receipt],
            ),
            patch.object(MODULE, "fetch_ref_sha", return_value=MERGE_COMMIT_SHA),
            patch.object(MODULE, "is_commit_reachable_from_main", return_value=True),
            candidate_merge_correlation(),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(receipt["actions"], ["stop_post_merge_run_sha_mismatch"])

    def test_failed_merge_group_receipt_keeps_the_first_failed_job(self):
        args = make_args()
        candidate = make_candidate()
        candidate_receipt = make_watcher_receipt(
            run_id=901,
            workflow="blocking-ci",
            event="merge_group",
            branch=candidate["head_branch"],
            sha=CANDIDATE_SHA,
            conclusion="failure",
            failed_jobs=[{"id": 41, "name": "CI required", "conclusion": "failure"}],
        )
        with (
            patch.object(MODULE, "fetch_pr", return_value=make_pr()),
            patch.object(
                MODULE, "resolve_merge_group_candidate", return_value=candidate
            ),
            candidate_association(),
            patch.object(
                MODULE, "run_blocking_watcher", return_value=candidate_receipt
            ),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(receipt["actions"], ["stop_merge_group_run_not_succeeded"])
        self.assertEqual(receipt["merge_group"]["failed_job"]["name"], "CI required")

    def test_missing_gh_returns_a_compact_operator_receipt(self):
        with patch.object(MODULE.subprocess, "run", side_effect=FileNotFoundError):
            receipt, status = MODULE.execute_delivery(make_args())

        self.assertEqual(status, 1)
        self.assertEqual(receipt["repo"], "owner/repo")
        self.assertEqual(receipt["actions"], ["stop_operator_help_required"])
        self.assertIn("Unable to execute", receipt["error"])

    def test_repo_autodetection_failure_returns_a_compact_operator_receipt(self):
        failed_lookup = types.SimpleNamespace(returncode=1, stdout="")
        with patch.object(MODULE.subprocess, "run", return_value=failed_lookup):
            receipt, status = MODULE.execute_delivery(make_args(repo=None))

        self.assertEqual(status, 1)
        self.assertEqual(receipt["repo"], "unknown")
        self.assertEqual(receipt["actions"], ["stop_operator_help_required"])
        self.assertIn("Unable to determine OWNER/REPO", receipt["error"])

    def test_missing_gh_launcher_emits_a_compact_json_receipt(self):
        launcher = (
            Path(__file__).resolve().parents[1] / "scripts" / "gh_pr_delivery_watch"
        )
        result = subprocess.run(
            [
                str(launcher),
                "--repo",
                "owner/repo",
                "--pr",
                "17",
                "--expected-head-sha",
                EXPECTED_HEAD_SHA,
            ],
            env={"PATH": "", "GH_PR_DELIVERY_WATCH_PYTHON": sys.executable},
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 1, msg=result.stderr)
        receipt = json.loads(result.stdout)
        self.assertEqual(receipt["repo"], "owner/repo")
        self.assertEqual(receipt["actions"], ["stop_operator_help_required"])

    def test_malformed_expected_head_sha_emits_a_compact_json_receipt(self):
        launcher = (
            Path(__file__).resolve().parents[1] / "scripts" / "gh_pr_delivery_watch"
        )
        result = subprocess.run(
            [
                str(launcher),
                "--repo",
                "owner/repo",
                "--pr",
                "17",
                "--expected-head-sha",
                "not-a-full-sha",
                "--post-merge-workflow",
                ".github/workflows/postmerge-ci.yml",
            ],
            env={"PATH": "", "GH_PR_DELIVERY_WATCH_PYTHON": sys.executable},
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 1, msg=result.stderr)
        self.assertEqual(result.stderr, "")
        receipt = json.loads(result.stdout)
        self.assertEqual(receipt["repo"], "owner/repo")
        self.assertEqual(receipt["pr"]["number"], 17)
        self.assertEqual(receipt["pr"]["expected_head_sha"], "not-a-full-sha")
        self.assertEqual(receipt["actions"], ["stop_invalid_arguments"])
        self.assertIn("full 40-character Git SHA", receipt["error"])
        self.assertEqual(
            receipt["proof_scope"]["selected_post_merge_workflows"],
            [".github/workflows/postmerge-ci.yml"],
        )
        self.assertFalse(receipt["proof_scope"]["whole_repository_health_proven"])

    def test_skill_requires_native_health_workflow_to_land_before_watching(self):
        skill = Path(__file__).resolve().parents[1] / "SKILL.md"
        content = " ".join(skill.read_text(encoding="utf-8").split())

        self.assertIn(
            "must first be introduced and landed on `main` by its separate CI PR",
            content,
        )
        self.assertIn("not resolvable from a watcher-only branch", content)
        self.assertIn(
            "workflow=Native Windows Bazel health,ref=main,head-sha=<merge-commit-sha>",
            content,
        )

    def test_blocking_watcher_invocation_uses_the_existing_terminal_helper(self):
        args = make_args()
        output = json.dumps({"targets": []}) + "\n"
        completed = types.SimpleNamespace(returncode=0, stdout=output)
        with patch.object(MODULE.subprocess, "run", return_value=completed) as run:
            payload = MODULE.run_blocking_watcher(
                "owner/repo",
                f"run-id=901,head-sha={CANDIDATE_SHA}",
                args,
            )

        self.assertEqual(payload, {"targets": []})
        command = run.call_args.args[0]
        self.assertEqual(command[0], str(MODULE.WATCHER_LAUNCHER))
        self.assertIn("--watch-until-terminal", command)
        self.assertIn(f"run-id=901,head-sha={CANDIDATE_SHA}", command)

    def test_blocking_watcher_forwards_the_delivery_python_override(self):
        args = make_args()
        completed = types.SimpleNamespace(
            returncode=0, stdout=json.dumps({"targets": []}) + "\n"
        )
        with (
            patch.dict(
                os.environ,
                {"GH_PR_DELIVERY_WATCH_PYTHON": sys.executable},
                clear=False,
            ),
            patch.object(MODULE.subprocess, "run", return_value=completed) as run,
        ):
            MODULE.run_blocking_watcher(
                "owner/repo", f"run-id=901,head-sha={CANDIDATE_SHA}", args
            )

        watcher_env = run.call_args.kwargs["env"]
        self.assertEqual(watcher_env["GH_WORKFLOW_RUN_WATCH_PYTHON"], sys.executable)

    def test_launcher_uses_configured_python_without_path(self):
        launcher = (
            Path(__file__).resolve().parents[1] / "scripts" / "gh_pr_delivery_watch"
        )
        result = subprocess.run(
            [str(launcher), "--help"],
            env={"PATH": "", "GH_PR_DELIVERY_WATCH_PYTHON": sys.executable},
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, msg=result.stderr)
        self.assertIn("--expected-head-sha", result.stdout)
        self.assertIn("--merge-observation-timeout-seconds", result.stdout)

    def test_launcher_accepts_the_workflow_watcher_python_override(self):
        launcher = (
            Path(__file__).resolve().parents[1] / "scripts" / "gh_pr_delivery_watch"
        )
        result = subprocess.run(
            [str(launcher), "--help"],
            env={"PATH": "", "GH_WORKFLOW_RUN_WATCH_PYTHON": sys.executable},
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, msg=result.stderr)
        self.assertIn("--expected-head-sha", result.stdout)


if __name__ == "__main__":
    unittest.main()
