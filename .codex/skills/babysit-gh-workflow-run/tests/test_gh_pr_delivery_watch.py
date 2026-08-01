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
CANDIDATE_SHA = "b" * 40
MERGE_COMMIT_SHA = "c" * 40
OTHER_SHA = "d" * 40


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
        "retry_settle_seconds": 0,
    }
    values.update(overrides)
    return types.SimpleNamespace(**values)


def make_pr(
    *,
    head_sha=EXPECTED_HEAD_SHA,
    merged=False,
    queue_entry_id="entry",
    merge_commit_sha="",
):
    return {
        "number": 17,
        "head_sha": head_sha,
        "base_ref": "main",
        "merged": merged,
        "merge_queue_entry_id": queue_entry_id,
        "merge_commit_sha": merge_commit_sha,
    }


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
            patch.object(
                MODULE,
                "run_blocking_watcher",
                side_effect=[candidate_receipt, post_merge_receipt],
            ) as watcher,
            patch.object(MODULE, "fetch_ref_sha", return_value=MERGE_COMMIT_SHA),
            patch.object(MODULE, "is_commit_reachable_from_main", return_value=True),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 0)
        self.assertEqual(receipt["actions"], ["stop_pr_delivery_proven"])
        self.assertEqual(receipt["pr"]["expected_head_sha"], EXPECTED_HEAD_SHA)
        self.assertEqual(receipt["merge_group"]["candidate_sha"], CANDIDATE_SHA)
        self.assertEqual(receipt["merge_group"]["run"]["id"], 901)
        self.assertEqual(receipt["merge_commit"]["sha"], MERGE_COMMIT_SHA)
        self.assertEqual(receipt["post_merge"]["run"]["id"], 902)
        self.assertEqual(watcher.call_count, 2)
        self.assertEqual(fetch_pr.call_count, 3)
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
            patch.object(
                MODULE, "run_blocking_watcher", return_value=candidate_receipt
            ),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(
            receipt["actions"], ["stop_merge_queue_entry_disappeared_without_merge"]
        )

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
            patch.object(
                MODULE, "run_blocking_watcher", return_value=candidate_receipt
            ),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(receipt["actions"], ["stop_merge_commit_uncorrelatable"])

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
            patch.object(
                MODULE,
                "run_blocking_watcher",
                side_effect=[candidate_receipt, wrong_post_merge_receipt],
            ),
            patch.object(MODULE, "fetch_ref_sha", return_value=MERGE_COMMIT_SHA),
            patch.object(MODULE, "is_commit_reachable_from_main", return_value=True),
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
            patch.object(
                MODULE, "run_blocking_watcher", return_value=candidate_receipt
            ),
        ):
            receipt, status = MODULE.execute_delivery(args)

        self.assertEqual(status, 1)
        self.assertEqual(receipt["actions"], ["stop_merge_group_run_not_succeeded"])
        self.assertEqual(receipt["merge_group"]["failed_job"]["name"], "CI required")

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


if __name__ == "__main__":
    unittest.main()
