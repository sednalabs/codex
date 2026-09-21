import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
import unittest.mock as mock


MODULE_PATH = Path(
    os.environ.get(
        "GH_PR_WATCH_MODULE_PATH",
        str(Path(__file__).resolve().parents[1] / "scripts" / "gh_pr_watch.py"),
    )
)

_SPEC = importlib.util.spec_from_file_location("gh_pr_watch_under_test", MODULE_PATH)
if _SPEC is None or _SPEC.loader is None:
    raise RuntimeError(f"Unable to load gh_pr_watch module from {MODULE_PATH}")
MODULE = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(MODULE)


class GhPrWatchTests(unittest.TestCase):
    def test_parse_args_accepts_installation_observer(self):
        with mock.patch.object(
            MODULE.sys,
            "argv",
            ["gh_pr_watch.py", "--installation-observer", "--once"],
        ):
            args = MODULE.parse_args()

        self.assertTrue(args.installation_observer)

    def test_installation_observer_rejects_retry_before_mutation(self):
        with mock.patch.object(
            MODULE.sys,
            "argv",
            [
                "gh_pr_watch.py",
                "--installation-observer",
                "--retry-failed-now",
                "--expected-head-sha",
                "a" * 40,
            ],
        ), self.assertRaises(SystemExit):
            MODULE.parse_args()

    def test_parse_args_rejects_bare_pr_number_without_repo(self):
        with mock.patch.object(
            MODULE.sys,
            "argv",
            ["gh_pr_watch.py", "--pr", "554", "--once"],
        ), self.assertRaises(SystemExit):
            MODULE.parse_args()

    def test_parse_args_accepts_bare_pr_number_with_exact_repo(self):
        with mock.patch.object(
            MODULE.sys,
            "argv",
            [
                "gh_pr_watch.py",
                "--pr",
                "554",
                "--repo",
                "sednalabs/codex",
                "--once",
            ],
        ):
            args = MODULE.parse_args()

        self.assertEqual(args.pr, "554")
        self.assertEqual(args.repo, "sednalabs/codex")

    def test_validate_pr_resolution_rejects_url_repo_mismatch(self):
        with self.assertRaisesRegex(
            MODULE.GhCommandError,
            "does not match requested repo",
        ):
            MODULE.validate_pr_resolution(
                "https://github.com/sednalabs/codex/pull/554",
                None,
                {"repo": "sednalabs/agent-ops"},
                {"origin_repo": "sednalabs/agent-ops"},
            )

    def test_validate_pr_resolution_rejects_url_and_repo_override_contradiction(self):
        with self.assertRaisesRegex(MODULE.GhCommandError, "contradicts --repo"):
            MODULE.validate_pr_resolution(
                "https://github.com/sednalabs/codex/pull/554",
                "sednalabs/agent-ops",
                {"repo": "sednalabs/codex"},
                {"origin_repo": "sednalabs/codex"},
            )

    def test_resolve_pr_rejects_response_from_different_base_repo(self):
        payload = {
            "number": 169,
            "url": "https://github.com/sednalabs/codex/pull/169",
            "state": "OPEN",
            "mergedAt": None,
            "closedAt": None,
            "headRefName": "feature/wrong-repository",
            "headRefOid": "a" * 40,
            "headRepository": {"nameWithOwner": "sednalabs/codex"},
            "headRepositoryOwner": {"login": "sednalabs"},
            "baseRefName": "main",
            "baseRefOid": "b" * 40,
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "reviewDecision": "",
        }
        with mock.patch.object(MODULE, "gh_json", return_value=payload), self.assertRaisesRegex(
            MODULE.GhCommandError,
            "base repo sednalabs/codex does not match requested repo sednalabs/mcp-toolkit-rs",
        ):
            MODULE.resolve_pr("169", repo_override="sednalabs/mcp-toolkit-rs")

    def test_validate_pr_resolution_rejects_mismatched_resolved_base_repo(self):
        with self.assertRaisesRegex(
            MODULE.GhCommandError,
            "base repo sednalabs/codex does not match requested repo sednalabs/mcp-toolkit-rs",
        ):
            MODULE.validate_pr_resolution(
                "169",
                "sednalabs/mcp-toolkit-rs",
                {
                    "repo": "sednalabs/mcp-toolkit-rs",
                    "base_repo": "sednalabs/codex",
                },
                {"origin_repo": "sednalabs/agent-ops"},
            )

    def test_resolve_pr_rejects_payload_without_canonical_base_repo(self):
        payload = {
            "number": 169,
            "url": "",
            "state": "OPEN",
            "mergedAt": None,
            "closedAt": None,
            "headRefName": "feature/ambiguous",
            "headRefOid": "a" * 40,
            "headRepository": {"nameWithOwner": "sednalabs/mcp-toolkit-rs"},
            "headRepositoryOwner": {"login": "sednalabs"},
            "baseRefName": "main",
            "baseRefOid": "b" * 40,
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "reviewDecision": "",
        }
        with mock.patch.object(MODULE, "gh_json", return_value=payload), self.assertRaisesRegex(
            MODULE.GhCommandError,
            "missing a canonical base repository URL",
        ):
            MODULE.resolve_pr("169", repo_override="sednalabs/mcp-toolkit-rs")

    def test_resolve_pr_reads_merge_queue_entry_via_separate_graphql_query(self):
        pr_payload = {
            "number": 169,
            "url": "https://github.com/sednalabs/codex/pull/169",
            "state": "OPEN",
            "mergedAt": None,
            "closedAt": None,
            "headRefName": "feature/queued",
            "headRefOid": "a" * 40,
            "headRepository": {"nameWithOwner": "sednalabs/codex"},
            "headRepositoryOwner": {"login": "sednalabs"},
            "baseRefName": "main",
            "baseRefOid": "b" * 40,
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "reviewDecision": "",
        }
        queue_payload = {
            "data": {
                "repository": {
                    "pullRequest": {
                        "mergeQueueEntry": {
                            "id": "MQE_graphql",
                            "state": "AWAITING_CHECKS",
                            "position": 4,
                            "headCommit": {"oid": "queue-head-sha"},
                        }
                    }
                }
            }
        }
        with mock.patch.object(
            MODULE, "gh_json", side_effect=[pr_payload, queue_payload]
        ) as gh_json:
            pr = MODULE.resolve_pr("169", repo_override="sednalabs/codex")

        self.assertEqual(gh_json.call_count, 2)
        pr_view_args = gh_json.call_args_list[0].args[0]
        self.assertEqual(pr_view_args[:2], ["pr", "view"])
        self.assertNotIn("mergeQueueEntry", pr_view_args[-1])
        self.assertEqual(
            gh_json.call_args_list[1].args[0],
            [
                "api",
                "graphql",
                "-f",
                f"query={MODULE.merge_queue_graphql_query()}",
                "-F",
                "owner=sednalabs",
                "-F",
                "name=codex",
                "-F",
                "number=169",
            ],
        )
        self.assertEqual(
            pr["merge_queue"],
            {
                "read_state": "observed",
                "status": "waiting",
                "id": "MQE_graphql",
                "state": "AWAITING_CHECKS",
                "position": 4,
                "head_sha": "queue-head-sha",
                "source": "github",
                "details": "GitHub returned merge-queue evidence.",
            },
        )

    def test_merge_queue_graphql_errors_and_malformed_payloads_fail_closed(self):
        cases = {
            "read_error": MODULE.GhCommandError("graphql unavailable"),
            "missing_pull_request": {"data": {"repository": {"pullRequest": None}}},
            "missing_queue_field": {"data": {"repository": {"pullRequest": {}}}},
            "partial_data_with_errors": {
                "errors": [{"message": "field authorization failed"}],
                "data": {
                    "repository": {
                        "pullRequest": {
                            "mergeQueueEntry": {
                                "id": "MQE_partial",
                                "state": "AWAITING_CHECKS",
                                "position": 1,
                                "headCommit": {"oid": "queue-sha"},
                            }
                        }
                    }
                },
            },
        }
        for name, response in cases.items():
            with self.subTest(name=name), mock.patch.object(MODULE, "gh_json", side_effect=response):
                queue = MODULE.get_merge_queue_entry("sednalabs/agent-ops", 984)

            self.assertEqual(
                {
                    "read_state": queue["read_state"],
                    "status": queue["status"],
                    "id": queue["id"],
                    "state": queue["state"],
                    "head_sha": queue["head_sha"],
                },
                {
                    "read_state": "error",
                    "status": "unknown",
                    "id": "",
                    "state": "",
                    "head_sha": "",
                },
            )

    def test_merge_queue_graphql_null_entry_is_observed_absence(self):
        payload = {"data": {"repository": {"pullRequest": {"mergeQueueEntry": None}}}}
        with mock.patch.object(MODULE, "gh_json", return_value=payload):
            queue = MODULE.get_merge_queue_entry("sednalabs/agent-ops", 984)

        self.assertEqual(
            queue,
            {
                "read_state": "observed_absent",
                "status": "absent",
                "id": "",
                "state": "",
                "position": None,
                "head_sha": "",
                "source": "github",
                "details": "GitHub reports no active merge-queue entry.",
            },
        )

    def test_parse_args_watch_until_terminal_implies_action_wait_and_terminal_checks(self):
        with mock.patch.object(
            MODULE.sys,
            "argv",
            ["gh_pr_watch.py", "--watch-until-terminal"],
        ):
            args = MODULE.parse_args()

        self.assertTrue(args.watch_until_terminal)
        self.assertTrue(args.watch_until_action)
        self.assertTrue(args.require_terminal_checks)
        self.assertFalse(args.once)

    def test_actionable_review_bot_login_includes_gemini(self):
        self.assertTrue(MODULE.is_actionable_review_bot_login("chatgpt-codex-connector[bot]"))
        self.assertTrue(MODULE.is_actionable_review_bot_login("gemini-code-assist[bot]"))
        self.assertFalse(MODULE.is_actionable_review_bot_login("dependabot[bot]"))

    def test_quota_notice_issue_comment_is_not_meaningful(self):
        item = {
            "author": "gemini-code-assist[bot]",
            "body": (
                "> [!WARNING]\n"
                "> You have reached your daily quota limit. Please wait up to 24 "
                "hours and I will start processing your requests again!"
            ),
        }
        self.assertFalse(MODULE.is_meaningful_issue_comment(item))

    def test_codex_review_status_issue_comment_is_not_meaningful(self):
        item = {
            "author": "chatgpt-codex-connector[bot]",
            "body": (
                "<!-- codex-pull-request-review-summary -->\n"
                "## Codex Review Summary\n\n"
                "| Review | Status | Commit |\n"
                "| Security Review | Running | `abc1234` |"
            ),
        }
        self.assertFalse(MODULE.is_meaningful_issue_comment(item))

    def test_no_feedback_bot_review_submission_is_not_meaningful(self):
        for summary in (
            "There are no review comments and no feedback to provide.",
            "There are no review comments, and I have no additional feedback to provide.",
        ):
            with self.subTest(summary=summary):
                item = {
                    "kind": "review",
                    "author": "gemini-code-assist[bot]",
                    "body": (
                        "Gemini Code Assist reviewed this pull request.\n\n"
                        f"{summary}\n\n"
                        "Deprecation notice: this product notice is informational."
                    ),
                }

                self.assertFalse(MODULE.is_meaningful_review_submission(item))

    def test_concrete_bot_review_submission_is_meaningful(self):
        item = {
            "kind": "review",
            "author": "gemini-code-assist[bot]",
            "body": "Please update the selector normalization before merging.",
        }

        self.assertTrue(MODULE.is_meaningful_review_submission(item))

    def test_fetch_new_review_items_ignores_bot_issue_comments_but_keeps_review_comments(self):
        pr = {"repo": "sednalabs/codex", "number": 53}
        state = {
            "seen_issue_comment_ids": [],
            "seen_review_comment_ids": [],
            "seen_review_ids": [],
        }
        issue_payload = [
            {
                "id": 1,
                "created_at": "2026-03-31T20:01:10Z",
                "author_association": "NONE",
                "body": (
                    "> [!WARNING]\n"
                    "> You have reached your daily quota limit. Please wait up to 24 "
                    "hours and I will start processing your requests again!"
                ),
                "html_url": "https://example.invalid/issue/1",
                "user": {"login": "gemini-code-assist[bot]"},
            }
        ]
        review_comment_payload = [
            {
                "id": 2,
                "created_at": "2026-03-31T20:02:10Z",
                "author_association": "NONE",
                "body": "This is a real inline review finding.",
                "path": "src/lib.rs",
                "line": 42,
                "html_url": "https://example.invalid/review/2",
                "user": {"login": "gemini-code-assist[bot]"},
            }
        ]

        with mock.patch.object(
            MODULE,
            "gh_api_list_paginated",
            side_effect=[issue_payload, review_comment_payload, []],
        ):
            items = MODULE.fetch_new_review_items(
                pr,
                state,
                fresh_state=False,
                authenticated_login="GraciousGazelles",
            )

        self.assertEqual(len(items), 1)
        self.assertEqual(items[0]["kind"], "review_comment")
        self.assertEqual(items[0]["author"], "gemini-code-assist[bot]")
        self.assertEqual(state["seen_issue_comment_ids"], [])
        self.assertEqual(state["seen_review_comment_ids"], ["2"])

    def test_build_actionable_review_items_only_includes_current_threads(self):
        pr = {
            "review_decision": "",
            "url": "https://example.invalid/pr/53",
        }
        new_review_items = []
        current_actionable_threads = [
            {
                "kind": "review_thread",
                "id": "thread-current",
                "created_at": "2026-03-31T20:10:00Z",
                "body": "Current unresolved thread",
            }
        ]
        actionable = MODULE.build_actionable_review_items(
            pr,
            new_review_items,
            current_actionable_threads,
        )
        self.assertEqual([item["id"] for item in actionable], ["thread-current"])

    def test_build_actionable_review_items_includes_review_comments(self):
        pr = {
            "review_decision": "",
            "url": "https://example.invalid/pr/53",
        }
        new_review_items = [
            {
                "kind": "review_comment",
                "id": "review-comment-1",
                "created_at": "2026-03-31T20:10:00Z",
                "body": "Please update this branch condition.",
                "author": "gemini-code-assist[bot]",
                "url": "https://example.invalid/review-comment/1",
            }
        ]
        active_unresolved_threads = [
            {
                "kind": "review_thread",
                "id": "thread-current",
                "created_at": "2026-03-31T20:11:00Z",
                "body": "Please update this branch condition.",
                "comment_ids": ["review-comment-1"],
                "comment_urls": ["https://example.invalid/review-comment/1"],
            }
        ]
        actionable = MODULE.build_actionable_review_items(pr, new_review_items, active_unresolved_threads)
        self.assertEqual(
            [(item["kind"], item["id"]) for item in actionable],
            [("review_comment", "review-comment-1"), ("review_thread", "thread-current")],
        )

    def test_build_actionable_review_items_ignores_review_comments_without_active_thread(self):
        pr = {
            "review_decision": "",
            "url": "https://example.invalid/pr/519",
        }
        new_review_items = [
            {
                "kind": "review_comment",
                "id": "3436299330",
                "created_at": "2026-06-18T14:00:00Z",
                "body": "Done, resolved.",
                "author": "test-observer",
                "url": "https://github.com/sednalabs/agent-ops/pull/519#discussion_r3436299330",
            }
        ]

        actionable = MODULE.build_actionable_review_items(pr, new_review_items, [])

        self.assertEqual(actionable, [])

    def test_build_actionable_review_items_ignores_resolved_self_reply(self):
        pr = {
            "review_decision": "",
            "url": "https://github.com/sednalabs/agent-ops/pull/519",
        }
        new_review_items = [
            {
                "kind": "review_comment",
                "id": "3436299330",
                "created_at": "2026-06-18T14:00:00Z",
                "body": "Done, resolved.",
                "author": "test-observer",
                "url": "https://github.com/sednalabs/agent-ops/pull/519#discussion_r3436299330",
            }
        ]

        actionable = MODULE.build_actionable_review_items(
            pr,
            new_review_items,
            active_unresolved_threads=[],
        )
        actions = MODULE.recommend_actions(
            {"closed": False, "merged": False},
            {"all_terminal": False, "failed_count": 0, "pending_count": 1, "passed_count": 0},
            failed_runs=[],
            actionable_review_items=actionable,
            review_state={"active_unresolved_thread_count": 0, "unresolved_thread_count": 0},
            merge_blockers={"is_blocked_for_merge": False, "reason_kinds": []},
            retries_used=0,
            max_retries=3,
        )

        self.assertEqual(actionable, [])
        self.assertEqual(actions, ["idle"])

    def test_build_actionable_review_items_ignores_no_feedback_bot_review_without_threads(self):
        pr = {
            "review_decision": "",
            "url": "https://example.invalid/pr/522",
        }
        new_review_items = [
            {
                "kind": "review",
                "id": "4525627653",
                "created_at": "2026-06-18T14:00:00Z",
                "body": (
                    "There are no review comments, and I have no additional "
                    "feedback to provide.\n\n"
                    "Deprecation notice: this product notice is informational."
                ),
                "author": "gemini-code-assist[bot]",
                "url": "https://github.com/sednalabs/agent-ops/pull/522#pullrequestreview-4525627653",
            }
        ]

        actionable = MODULE.build_actionable_review_items(pr, new_review_items, [])

        self.assertEqual(actionable, [])

    def test_build_actionable_review_items_keeps_concrete_bot_review_without_threads(self):
        pr = {
            "review_decision": "",
            "head_sha": "current-head",
            "url": "https://example.invalid/pr/522",
        }
        review = {
            "kind": "review",
            "id": "4525627654",
            "commit_id": "current-head",
            "created_at": "2026-06-18T14:01:00Z",
            "body": (
                "There are no inline review comments, but please update the "
                "selector normalization before merging."
            ),
            "author": "gemini-code-assist[bot]",
            "url": "https://example.invalid/review/4525627654",
        }

        actionable = MODULE.build_actionable_review_items(pr, [review], [])

        self.assertEqual(actionable, [review])

    def test_build_actionable_review_items_ignores_stale_bot_review_submission(self):
        pr = {
            "review_decision": "",
            "head_sha": "current-head",
            "url": "https://example.invalid/pr/522",
        }
        stale_review = {
            "kind": "review",
            "id": "4525627655",
            "commit_id": "previous-head",
            "created_at": "2026-06-18T14:01:00Z",
            "body": "Please update the selector normalization before merging.",
            "author": "gemini-code-assist[bot]",
            "url": "https://example.invalid/review/4525627655",
        }

        actionable = MODULE.build_actionable_review_items(pr, [stale_review], [])

        self.assertEqual(actionable, [])

    def test_build_actionable_review_items_keeps_stale_human_review_submission(self):
        pr = {
            "review_decision": "",
            "head_sha": "current-head",
            "url": "https://example.invalid/pr/522",
        }
        stale_review = {
            "kind": "review",
            "id": "4525627656",
            "commit_id": "previous-head",
            "created_at": "2026-06-18T14:01:00Z",
            "body": "Please update the selector normalization before merging.",
            "author": "maintainer",
            "url": "https://example.invalid/review/4525627656",
        }

        actionable = MODULE.build_actionable_review_items(pr, [stale_review], [])

        self.assertEqual(actionable, [stale_review])

    def test_summarize_review_submissions_ignores_stale_bot_review(self):
        reviews = [
            {
                "kind": "review",
                "id": "4525627657",
                "commit_id": "previous-head",
                "created_at": "2026-06-18T14:01:00Z",
                "body": "Please update the selector normalization before merging.",
                "author": "gemini-code-assist[bot]",
                "state": "COMMENTED",
                "url": "https://example.invalid/review/4525627657",
            }
        ]

        summary = MODULE.summarize_review_submissions(reviews, "current-head")

        self.assertEqual(summary["meaningful_review_count"], 0)
        self.assertEqual(summary["latest_reviews"], [])

    def test_summarize_check_runs_tracks_failed_and_pending_status(self):
        check_runs = [
            {"name": "unit", "status": "completed", "conclusion": "success"},
            {"name": "integration", "status": "in_progress", "conclusion": ""},
            {"name": "lint", "status": "completed", "conclusion": "failure"},
        ]
        summary = MODULE.summarize_check_runs(check_runs)
        self.assertEqual(summary["total_count"], 3)
        self.assertEqual(summary["passed_count"], 1)
        self.assertEqual(summary["pending_count"], 1)
        self.assertEqual(summary["failed_count"], 1)
        self.assertFalse(summary["all_terminal"])

    def test_summarize_check_runs_tracks_failed_state_without_bucket(self):
        check_runs = [
            {"name": "lint", "state": "FAILURE", "bucket": "", "conclusion": ""},
        ]
        summary = MODULE.summarize_check_runs(check_runs)
        self.assertEqual(summary["failed_count"], 1)
        self.assertEqual(summary["pending_count"], 0)
        self.assertTrue(summary["all_terminal"])

    def test_get_pr_checks_treats_no_checks_reported_as_pending(self):
        with mock.patch.object(
            MODULE,
            "gh_json",
            side_effect=MODULE.GhCommandError(
                "GitHub CLI command failed: gh -R sednalabs/solar-gravity-lab pr checks 93\n"
                "stderr: no checks reported on the 'branch' branch"
            ),
        ):
            checks = MODULE.get_pr_checks("93", repo="sednalabs/solar-gravity-lab")

        self.assertEqual(len(checks), 1)
        self.assertEqual(checks[0]["bucket"], "pending")
        self.assertEqual(checks[0]["state"], "PENDING")
        summary = MODULE.summarize_check_runs(checks)
        self.assertEqual(summary["pending_count"], 1)
        self.assertFalse(summary["all_terminal"])

    def test_apply_no_checks_policy_treats_clean_mergeable_pr_as_terminal(self):
        checks, policy = MODULE.apply_no_checks_policy(
            {"mergeable": "MERGEABLE", "merge_state_status": "CLEAN"},
            [MODULE.pending_checks_not_reported_item()],
        )

        self.assertEqual(checks, [])
        self.assertEqual(policy["state"], "clean_mergeable_no_checks")
        summary = MODULE.summarize_check_runs(checks)
        self.assertEqual(summary["pending_count"], 0)
        self.assertTrue(summary["all_terminal"])

    def test_apply_no_checks_policy_keeps_unclean_pr_pending(self):
        checks, policy = MODULE.apply_no_checks_policy(
            {"mergeable": "UNKNOWN", "merge_state_status": "BLOCKED"},
            [MODULE.pending_checks_not_reported_item()],
        )

        self.assertEqual(len(checks), 1)
        self.assertEqual(policy["state"], "not_applicable")
        summary = MODULE.summarize_check_runs(checks)
        self.assertEqual(summary["pending_count"], 1)
        self.assertFalse(summary["all_terminal"])

    def test_classify_gh_error_detects_bad_credentials(self):
        err = MODULE.GhCommandError(
            "GitHub CLI command failed: gh api repos/example/repo/pulls/2/reviews\n"
            "stderr: HTTP 401: Bad credentials"
        )

        self.assertEqual(MODULE.classify_gh_error(err), "github_auth")

    def test_watch_until_action_suppresses_waiting_status_by_default(self):
        args = mock.Mock(poll_seconds=30, require_terminal_checks=False, progress=False)
        idle_snapshot = {
            "pr": {"head_sha": "abc123"},
            "checks": {
                "all_terminal": False,
                "failed_count": 0,
                "pending_count": 1,
                "passed_count": 0,
            },
            "review_state": {"active_unresolved_thread_count": 0},
            "actionable_review_items": [],
            "actions": ["idle"],
        }
        action_snapshot = {
            "pr": {"head_sha": "abc123"},
            "checks": {
                "all_terminal": True,
                "failed_count": 1,
                "pending_count": 0,
                "passed_count": 0,
            },
            "review_state": {"active_unresolved_thread_count": 0},
            "actionable_review_items": [],
            "actions": ["diagnose_ci_failure"],
        }

        with mock.patch.object(
            MODULE,
            "collect_snapshot",
            side_effect=[(idle_snapshot, Path("/tmp/state.json")), (action_snapshot, Path("/tmp/state.json"))],
        ), mock.patch.object(MODULE.time, "sleep") as sleep, mock.patch.object(
            MODULE, "print_status"
        ) as print_status, mock.patch.object(
            MODULE, "print_json"
        ):
            rc = MODULE.run_watch_until_action(args)

        self.assertEqual(rc, 0)
        sleep.assert_called_once_with(30)
        print_status.assert_not_called()

    def test_watch_until_action_emits_waiting_status_when_requested(self):
        args = mock.Mock(poll_seconds=30, require_terminal_checks=False, progress=True)
        idle_snapshot = {
            "pr": {"head_sha": "abc123"},
            "checks": {
                "all_terminal": False,
                "failed_count": 0,
                "pending_count": 1,
                "passed_count": 0,
            },
            "review_state": {"active_unresolved_thread_count": 0},
            "actionable_review_items": [],
            "actions": ["idle"],
        }
        action_snapshot = {
            "pr": {"head_sha": "abc123"},
            "checks": {
                "all_terminal": True,
                "failed_count": 1,
                "pending_count": 0,
                "passed_count": 0,
            },
            "review_state": {"active_unresolved_thread_count": 0},
            "actionable_review_items": [],
            "actions": ["diagnose_ci_failure"],
        }

        with mock.patch.object(
            MODULE,
            "collect_snapshot",
            side_effect=[
                (idle_snapshot, Path("/tmp/state.json")),
                (action_snapshot, Path("/tmp/state.json")),
            ],
        ), mock.patch.object(MODULE.time, "sleep"), mock.patch.object(
            MODULE, "print_status"
        ) as print_status, mock.patch.object(
            MODULE, "print_json"
        ):
            rc = MODULE.run_watch_until_action(args)

        self.assertEqual(rc, 0)
        print_status.assert_called_once()
        self.assertIn("gh_pr_watch.py waiting", print_status.call_args.args[0])

    def test_watch_until_action_can_wait_for_terminal_ci_failure(self):
        args = mock.Mock(
            poll_seconds=30,
            require_terminal_checks=True,
            progress=False,
        )
        in_progress_failure_snapshot = {
            "pr": {"head_sha": "abc123"},
            "checks": {
                "all_terminal": False,
                "failed_count": 1,
                "pending_count": 1,
                "passed_count": 0,
            },
            "actions": ["diagnose_ci_failure"],
        }
        terminal_failure_snapshot = {
            "pr": {"head_sha": "abc123"},
            "checks": {
                "all_terminal": True,
                "failed_count": 1,
                "pending_count": 0,
                "passed_count": 0,
            },
            "actions": ["diagnose_ci_failure"],
        }

        with mock.patch.object(
            MODULE,
            "collect_snapshot",
            side_effect=[
                (in_progress_failure_snapshot, Path("/tmp/state.json")),
                (terminal_failure_snapshot, Path("/tmp/state.json")),
            ],
        ), mock.patch.object(MODULE.time, "sleep") as sleep, mock.patch.object(
            MODULE, "print_status"
        ) as print_status, mock.patch.object(
            MODULE, "print_json"
        ) as print_json:
            rc = MODULE.run_watch_until_action(args)

        self.assertEqual(rc, 0)
        sleep.assert_called_once_with(30)
        print_status.assert_not_called()
        print_json.assert_called_once()
        payload = print_json.call_args.args[0]
        self.assertEqual(payload["exit_reason"], "action_required")
        self.assertEqual(payload["polls_completed"], 2)

    def test_compact_wait_snapshot_drops_superfluous_raw_collections(self):
        snapshot = {
            "pr": {
                "repo": "owner/repo",
                "number": 7,
                "head_sha": "abc123",
                "url": "https://example.invalid/pr/7",
                "merge_queue": {
                    "read_state": "observed",
                    "status": "waiting",
                    "id": "MQE_compact",
                    "state": "AWAITING_CHECKS",
                    "position": 3,
                    "head_sha": "queue-sha",
                    "source": "github",
                },
                "title": "large title not needed in terminal receipt",
            },
            "checks": {"failed_count": 1, "pending_count": 0},
            "checks_source": "current_head",
            "check_details": {"failing": [{"name": "test"}], "pending": []},
            "raw_checks": {"large": "omitted"},
            "raw_check_details": {"large": "omitted"},
            "new_review_items": [{"body": "historical and omitted"}],
            "actionable_review_items": [
                {
                    "kind": "review_thread",
                    "id": "thread-1",
                    "body": "x" * 2000,
                    "url": "https://example.invalid/thread/1",
                }
            ],
            "actions": ["process_review_comment"],
            "failed_runs": [
                {
                    "run_id": 123,
                    "workflow_name": "test",
                    "status": "completed",
                    "conclusion": "failure",
                    "failed_jobs": [
                        {
                            "job_id": 999,
                            "job_name": "Tests",
                            "status": "completed",
                            "conclusion": "failure",
                        },
                        {
                            "job_id": 1000,
                            "job_name": "Lint",
                            "status": "completed",
                            "conclusion": "failure",
                        },
                    ],
                    "first_failed_job": {
                        "job_id": 999,
                        "job_name": "Tests",
                        "status": "completed",
                        "conclusion": "failure",
                    },
                }
            ],
            "ci_startup_blockers": [],
        }

        compact = MODULE.compact_wait_snapshot(snapshot)

        self.assertNotIn("raw_checks", compact)
        self.assertNotIn("raw_check_details", compact)
        self.assertNotIn("new_review_items", compact)
        self.assertEqual(compact["pr"]["repo"], "owner/repo")
        self.assertEqual(compact["pr"]["head_sha"], "abc123")
        self.assertEqual(
            compact["pr"]["merge_queue"],
            {
                "read_state": "observed",
                "status": "waiting",
                "id": "MQE_compact",
                "state": "AWAITING_CHECKS",
                "position": 3,
                "head_sha": "queue-sha",
                "source": "github",
            },
        )
        self.assertLessEqual(
            len(compact["actionable_review_items"][0]["body"]),
            1000,
        )
        self.assertEqual(compact["failed_runs"][0]["failed_job_count"], 2)
        self.assertNotIn("failed_jobs", compact["failed_runs"][0])

    def test_snapshot_change_key_includes_merge_queue_state_and_head_sha(self):
        base = {
            "pr": {
                "head_sha": "pr-sha",
                "state": "OPEN",
                "mergeable": "MERGEABLE",
                "merge_state_status": "CLEAN",
                "review_decision": "",
                "merge_queue": {
                    "read_state": "observed",
                    "status": "waiting",
                    "id": "MQE_change",
                    "state": "AWAITING_CHECKS",
                    "head_sha": "queue-sha-one",
                },
            },
            "checks": {"passed_count": 1, "failed_count": 0, "pending_count": 0},
            "review_state": {"active_unresolved_thread_count": 0},
            "actionable_review_items": [],
            "actions": ["idle"],
        }
        successor = {
            **base,
            "pr": {
                **base["pr"],
                "merge_queue": {**base["pr"]["merge_queue"], "head_sha": "queue-sha-two"},
            },
        }

        self.assertNotEqual(MODULE.snapshot_change_key(base), MODULE.snapshot_change_key(successor))

    def test_build_effective_ci_state_handles_stale_fallback(self):
        ci_context = {
            "stale_head_sha": "older",
            "stale_failed_runs": [{"run_id": 1}],
            "stale_fallback_active": True,
            "current_head_checks_signal": False,
            "grace_seconds": MODULE.CURRENT_HEAD_CHECK_GRACE_SECONDS,
        }
        effective, source, stale_fallback, message, stale_failed_runs = MODULE.build_effective_ci_state(
            "newer",
            {"total_count": 0},
            ci_context,
        )
        self.assertEqual(stale_fallback, True)
        self.assertEqual(source, "stale_fallback")
        self.assertEqual(effective["failed_count"], 1)
        self.assertIn("using older failed checks", message)
        self.assertEqual(len(stale_failed_runs), 1)

    def test_recommend_actions_stale_fallback_does_not_retry(self):
        pr = {"closed": False, "merged": False}
        checks_summary = {
            "all_terminal": True,
            "failed_count": 1,
            "pending_count": 0,
            "passed_count": 0,
        }
        failed_runs = [{"run_id": 123}]
        review_items = []
        review_state = {"active_unresolved_thread_count": 0}
        actions = MODULE.recommend_actions_with_source(
            pr,
            checks_summary,
            failed_runs,
            review_items,
            review_state,
            retries_used=0,
            max_retries=3,
            source="stale_fallback",
        )

        self.assertEqual(actions, ["diagnose_ci_failure"])

    def test_collect_snapshot_uses_stale_fallback_without_retry_action(self):
        pr = {
            "repo": "sednalabs/codex",
            "number": 286,
            "head_sha": "newhead",
            "closed": False,
            "merged": False,
            "review_decision": "",
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
            "merge_queue": MODULE.normalize_merge_queue_entry(None),
            "url": "https://github.com/sednalabs/codex/pull/286",
        }
        stale_run = {
            "id": 123,
            "name": "rust-ci",
            "status": "completed",
            "conclusion": "failure",
            "html_url": "https://example.invalid/runs/123",
            "head_sha": "oldhead",
        }
        args = mock.Mock(
            pr="286",
            repo="sednalabs/codex",
            state_file=None,
            reset_seen_feedback=False,
            ignore_review_thread=[],
            max_flaky_retries=3,
        )

        with tempfile.TemporaryDirectory() as tmpdir:
            state_path = Path(tmpdir) / "state.json"
            state_path.write_text(
                json.dumps(
                    {
                        "last_seen_head_sha": "oldhead",
                        "last_snapshot_at": 1_000,
                        "seen_issue_comment_ids": [],
                        "seen_review_comment_ids": [],
                        "seen_review_ids": [],
                    }
                )
            )
            args.state_file = state_path.name

            with mock.patch.object(MODULE, "detect_local_git_context", return_value={}), mock.patch.object(
                MODULE, "resolve_pr", return_value=pr
            ), mock.patch.object(MODULE, "validate_pr_resolution"), mock.patch.object(
                MODULE, "get_pr_checks", return_value=[MODULE.pending_checks_not_reported_item()]
            ), mock.patch.object(
                MODULE,
                "get_workflow_runs_for_sha",
                side_effect=[[], [stale_run]],
            ), mock.patch.object(
                MODULE, "failed_jobs_for_run", return_value=[]
            ), mock.patch.object(
                MODULE, "get_authenticated_login", return_value="test-observer"
            ), mock.patch.object(
                MODULE, "fetch_new_review_items", return_value=([], [])
            ), mock.patch.object(
                MODULE, "get_review_threads", return_value=[]
            ), mock.patch.object(
                MODULE.tempfile, "gettempdir", return_value=str(state_path.parent)
            ), mock.patch.object(
                MODULE.time, "time", return_value=1_030
            ):
                snapshot, _ = MODULE.collect_snapshot(args)

            saved_state = json.loads(state_path.read_text())

        self.assertEqual(snapshot["checks_source"], "stale_fallback")
        self.assertEqual(snapshot["checks"]["failed_count"], 1)
        self.assertEqual(snapshot["failed_runs"][0]["run_id"], 123)
        self.assertEqual(snapshot["failed_runs_current_head"], [])
        self.assertEqual(snapshot["ci_head_context"]["stale_head_sha"], "oldhead")
        self.assertEqual(snapshot["actions"], ["diagnose_ci_failure"])
        self.assertNotIn("retry_failed_checks", snapshot["actions"])
        self.assertEqual(saved_state["last_seen_head_sha"], "newhead")
        self.assertEqual(saved_state["last_watch_decision"]["head_sha"], "newhead")
        self.assertEqual(saved_state["last_watch_decision"]["primary_action"], "diagnose_ci_failure")

    def test_merge_queue_active_awaiting_checks_suppresses_ready(self):
        pr = {
            "closed": False,
            "merged": False,
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
            "merge_queue": MODULE.normalize_merge_queue_entry(
                {
                    "id": "MQE_active",
                    "state": "AWAITING_CHECKS",
                    "position": 2,
                    "headCommit": {"oid": "queue-sha"},
                }
            ),
        }
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0, "passed_count": 1}
        blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})

        observed = {
            "actions": MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3),
            "merge_blockers": blockers,
        }

        self.assertEqual(
            observed,
            {
                "actions": ["idle"],
                "merge_blockers": {
                    "is_blocked_for_merge": True,
                    "reason_kinds": ["merge_queue_waiting"],
                    "reasons": [
                        {
                            "kind": "merge_queue_waiting",
                            "state": "AWAITING_CHECKS",
                            "entry_id": "MQE_active",
                            "head_sha": "queue-sha",
                            "details": "PR is actively in the GitHub merge queue; continue waiting for the queue outcome.",
                        }
                    ],
                },
            },
        )

    def test_merge_policy_blocker_is_actionable_even_when_ci_is_green(self):
        pr = {
            "closed": False,
            "merged": False,
            "mergeable": "MERGEABLE",
            "merge_state_status": "BLOCKED",
            "merge_queue": MODULE.normalize_merge_queue_entry(None),
            "head_sha": "policy-head",
        }
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0, "passed_count": 12}
        blockers = MODULE.build_merge_blockers(
            pr,
            checks,
            {"failing": [], "pending": []},
            {"active_unresolved_thread_count": 0, "ignored_unresolved_thread_count": 1},
        )

        self.assertEqual(blockers["reason_kinds"], ["merge_policy_blocked"])
        self.assertEqual(
            MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3),
            [MODULE.ACTION_REQUIRED_MERGE_POLICY_BLOCKED],
        )

    def test_empty_mergeable_clean_is_not_ready(self):
        pr = {
            "closed": False,
            "merged": False,
            "mergeable": "",
            "merge_state_status": "CLEAN",
            "merge_queue": MODULE.normalize_merge_queue_entry(None),
        }
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0}
        blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})

        self.assertIn("merge_conflict_or_dirty_state", blockers["reason_kinds"])
        self.assertNotEqual(
            MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3),
            ["stop_ready_to_merge"],
        )

    def test_empty_mergeable_blocked_is_not_policy_action(self):
        pr = {
            "closed": False,
            "merged": False,
            "mergeable": "",
            "merge_state_status": "BLOCKED",
            "merge_queue": MODULE.normalize_merge_queue_entry(None),
        }
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0}
        blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})

        self.assertIn("merge_conflict_or_dirty_state", blockers["reason_kinds"])
        self.assertNotIn("merge_policy_blocked", blockers["reason_kinds"])
        self.assertNotIn(
            MODULE.ACTION_REQUIRED_MERGE_POLICY_BLOCKED,
            MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3),
        )

    def test_blocked_nonterminal_zero_pending_waits_without_policy_action(self):
        pr = {
            "closed": False,
            "merged": False,
            "mergeable": "MERGEABLE",
            "merge_state_status": "BLOCKED",
            "merge_queue": MODULE.normalize_merge_queue_entry(None),
        }
        checks = {"all_terminal": False, "failed_count": 0, "pending_count": 0}
        blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})

        self.assertNotIn("merge_policy_blocked", blockers["reason_kinds"])
        self.assertEqual(MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3), ["idle"])

    def test_terminal_clean_mergeable_remains_ready(self):
        pr = {
            "closed": False,
            "merged": False,
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
            "merge_queue": MODULE.normalize_merge_queue_entry(None),
        }
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0}
        blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})

        self.assertEqual(MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3), ["stop_ready_to_merge"])

    def test_merge_policy_action_receipt_preserves_exact_head_and_counts(self):
        snapshot = {
            "pr": {
                "repo": "sednalabs/agent-ops",
                "number": 1123,
                "head_sha": "policy-head",
                "mergeable": "MERGEABLE",
                "merge_state_status": "BLOCKED",
            },
            "checks": {
                "total_count": 13,
                "passed_count": 12,
                "failed_count": 0,
                "pending_count": 0,
                "all_terminal": True,
            },
            "checks_source": "current_head",
            "review_state": {
                "active_unresolved_thread_count": 0,
                "ignored_unresolved_thread_count": 1,
                "blocking_top_level_review_submission_count": 0,
            },
            "merge_blockers": {
                "is_blocked_for_merge": True,
                "reason_kinds": ["merge_policy_blocked"],
                "reasons": [{"kind": "merge_policy_blocked", "head_sha": "policy-head"}],
            },
            "actions": [MODULE.ACTION_REQUIRED_MERGE_POLICY_BLOCKED],
        }

        receipt = MODULE.build_watch_decision(snapshot, recorded_at=1234)

        self.assertEqual(receipt["decision"], "action_required")
        self.assertEqual(receipt["primary_action"], MODULE.ACTION_REQUIRED_MERGE_POLICY_BLOCKED)
        self.assertEqual(receipt["head_sha"], "policy-head")
        self.assertEqual(receipt["check_counts"], {"total": 13, "passed": 12, "failed": 0, "pending": 0})
        self.assertEqual(receipt["review_counts"]["ignored_unresolved"], 1)
        self.assertEqual(receipt["merge_blocker_kinds"], ["merge_policy_blocked"])

    def test_merge_policy_blocker_disables_green_exponential_backoff(self):
        args = mock.Mock(poll_seconds=30)
        snapshot = {
            "pr": {"head_sha": "policy-head"},
            "checks": {"all_terminal": True, "failed_count": 0, "pending_count": 0},
            "merge_blockers": {
                "is_blocked_for_merge": True,
                "reason_kinds": ["merge_policy_blocked"],
            },
        }

        self.assertEqual(
            MODULE.next_watch_poll_seconds(args, snapshot, "same", 960, 1200),
            (30, MODULE.snapshot_change_key(snapshot)),
        )

    def test_active_queue_states_use_base_cadence(self):
        args = mock.Mock(poll_seconds=30)
        for state in ("QUEUED", "AWAITING_CHECKS"):
            snapshot = {
                "pr": {
                    "head_sha": "queue-head",
                    "merge_queue": {
                        "status": "waiting",
                        "state": state,
                        "id": "entry-1",
                    },
                },
                "checks": {"all_terminal": True, "failed_count": 0, "pending_count": 0},
                "merge_blockers": {"reason_kinds": ["merge_queue_waiting"]},
            }
            self.assertEqual(
                MODULE.next_watch_poll_seconds(args, snapshot, MODULE.snapshot_change_key(snapshot), 960, 1200)[0],
                30,
            )

    def test_unreadable_pending_queue_head_uses_base_cadence(self):
        args = mock.Mock(poll_seconds=30)
        snapshot = {
            "pr": {
                "head_sha": "queue-head",
                "merge_queue": {"status": "waiting", "state": "QUEUED", "id": "entry-1", "head_sha": ""},
            },
            "checks": {"all_terminal": True, "failed_count": 0, "pending_count": 0},
        }
        self.assertEqual(
            MODULE.next_watch_poll_seconds(args, snapshot, MODULE.snapshot_change_key(snapshot), 960, 1200)[0],
            30,
        )

    def test_queue_identity_change_resets_cadence(self):
        args = mock.Mock(poll_seconds=30)
        old = {
            "pr": {"head_sha": "queue-head", "merge_queue": {"status": "waiting", "state": "QUEUED", "id": "old"}},
            "checks": {"all_terminal": True, "failed_count": 0, "pending_count": 0},
        }
        current = {
            "pr": {"head_sha": "queue-head", "merge_queue": {"status": "waiting", "state": "QUEUED", "id": "new"}},
            "checks": old["checks"],
        }
        self.assertEqual(
            MODULE.next_watch_poll_seconds(args, current, MODULE.snapshot_change_key(old), 960, 1200)[0],
            30,
        )

    def test_nonqueued_green_idle_snapshot_keeps_backoff(self):
        args = mock.Mock(poll_seconds=30)
        snapshot = {
            "pr": {"head_sha": "idle-head", "merge_queue": {"status": "observed_absent"}},
            "checks": {"all_terminal": True, "failed_count": 0, "pending_count": 0},
        }
        self.assertEqual(
            MODULE.next_watch_poll_seconds(args, snapshot, MODULE.snapshot_change_key(snapshot), 960, 1200)[0],
            1200,
        )

    def test_watch_schedule_persists_exact_head_and_wake(self):
        snapshot = {
            "pr": {"repo": "owner/repo", "number": 7, "head_sha": "policy-head"},
            "actions": [MODULE.ACTION_REQUIRED_MERGE_POLICY_BLOCKED],
            "merge_blockers": {"reason_kinds": ["merge_policy_blocked"]},
        }
        with tempfile.TemporaryDirectory() as tmpdir:
            state_path = Path(tmpdir) / "state.json"
            MODULE.persist_watch_schedule(state_path, snapshot, "watch-until-action", 0, scheduled_at=1234)
            state, _ = MODULE.load_state(state_path)

        self.assertEqual(
            state["watch_schedule"],
            {
                "schema_version": 1,
                "mode": "watch-until-action",
                "repo": "owner/repo",
                "number": 7,
                "head_sha": "policy-head",
                "poll_seconds": 0,
                "scheduled_at": 1234,
                "wake_at": 1234,
            },
        )

    def test_active_merge_queue_guard_survives_missing_blocker_summary(self):
        pr = {
            "closed": False,
            "merged": False,
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
            "merge_queue": MODULE.normalize_merge_queue_entry(
                {
                    "id": "MQE_guard",
                    "state": "AWAITING_CHECKS",
                    "position": 1,
                    "headCommit": {"oid": "queue-guard-sha"},
                }
            ),
        }
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0, "passed_count": 1}

        self.assertFalse(
            MODULE.is_pr_ready_to_merge(pr, checks, [], {}, {"is_blocked_for_merge": False})
        )
        self.assertEqual(
            MODULE.recommend_actions(pr, checks, [], [], {}, {}, retries_used=0, max_retries=3),
            ["idle"],
        )

    def test_merge_queue_active_keeps_failed_pr_checks_visible_without_interrupting_wait(self):
        pr = {
            "closed": False,
            "merged": False,
            "mergeable": "MERGEABLE",
            "merge_state_status": "UNSTABLE",
            "merge_queue": MODULE.normalize_merge_queue_entry(
                {
                    "id": "MQE_active",
                    "state": "QUEUED",
                    "position": 1,
                    "headCommit": {"oid": ""},
                }
            ),
        }
        checks = {"all_terminal": True, "failed_count": 2, "pending_count": 0, "passed_count": 12}
        check_details = {
            "failing": [
                {"name": "dependency-governance"},
                {"name": "required-dependency-governance"},
            ],
            "pending": [],
        }
        blockers = MODULE.build_merge_blockers(pr, checks, check_details, {})

        self.assertEqual(
            MODULE.recommend_actions(
                pr,
                checks,
                [{"run_id": 123}],
                [],
                {},
                blockers,
                retries_used=0,
                max_retries=3,
            ),
            ["idle"],
        )
        self.assertEqual(
            blockers["reason_kinds"],
            ["failing_checks", "merge_queue_waiting"],
        )

    def test_merge_queue_absence_preserves_ready_behavior(self):
        pr = {
            "closed": False,
            "merged": False,
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
            "merge_queue": MODULE.normalize_merge_queue_entry(None),
        }
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0, "passed_count": 1}
        blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})

        self.assertEqual(
            {
                "actions": MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3),
                "merge_blockers": blockers,
            },
            {
                "actions": ["stop_ready_to_merge"],
                "merge_blockers": {"is_blocked_for_merge": False, "reason_kinds": [], "reasons": []},
            },
        )

    def test_merge_queue_failed_or_removed_is_an_actionable_stop(self):
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0, "passed_count": 1}
        base_pr = {"closed": False, "merged": False, "mergeable": "MERGEABLE", "merge_state_status": "CLEAN"}
        cases = {
            "failed": (
                {
                    "read_state": "observed",
                    "status": "failed",
                    "id": "MQE_failed",
                    "state": "UNMERGEABLE",
                    "position": 1,
                    "head_sha": "queue-failed-sha",
                    "source": "github",
                },
                "stop_merge_queue_failed",
                "merge_queue_failed",
            ),
            "removed": (
                {
                    "read_state": "observed_absent",
                    "status": "removed",
                    "id": "MQE_removed",
                    "state": "AWAITING_CHECKS",
                    "position": 1,
                    "head_sha": "queue-removed-sha",
                    "source": "watcher_state",
                },
                "stop_merge_queue_removed",
                "merge_queue_removed",
            ),
        }

        for name, (queue, action, reason_kind) in cases.items():
            with self.subTest(name=name):
                pr = {**base_pr, "merge_queue": queue}
                blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})
                self.assertEqual(
                    {
                        "actions": MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3),
                        "reason_kinds": blockers["reason_kinds"],
                    },
                    {"actions": [action], "reason_kinds": [reason_kind]},
                )

    def test_merge_queue_read_error_preserves_identity_and_never_becomes_ready(self):
        state = {
            "last_merge_queue_entry": {
                "read_state": "observed",
                "status": "waiting",
                "id": "MQE_previous",
                "state": "AWAITING_CHECKS",
                "position": 1,
                "head_sha": "queue-sha",
                "source": "github",
            },
            "last_merge_queue_pr_head_sha": "pr-head",
        }
        pr = {
            "closed": False,
            "merged": False,
            "head_sha": "pr-head",
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
            "merge_queue": MODULE.normalize_merge_queue_entry(None, field_present=False),
        }
        pr["merge_queue"] = MODULE.reconcile_merge_queue_entry(pr, state)
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0, "passed_count": 1}
        blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})

        self.assertEqual(
            {
                "actions": MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3),
                "queue": pr["merge_queue"],
            },
            {
                "actions": ["stop_merge_queue_read_error"],
                "queue": {
                    "read_state": "error",
                    "status": "unknown",
                    "id": "MQE_previous",
                    "state": "AWAITING_CHECKS",
                    "position": 1,
                    "head_sha": "queue-sha",
                    "source": "watcher_state",
                    "details": "GitHub merge-queue evidence could not be read; retaining the last known queue identity for this unchanged PR head.",
                },
            },
        )

    def test_terminal_merge_queue_tombstone_survives_restart_until_pr_head_changes(self):
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0, "passed_count": 1}

        def queue_action(pr):
            blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})
            return MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3)

        base_pr = {
            "closed": False,
            "merged": False,
            "head_sha": "pr-head-one",
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
        }
        active = {
            **base_pr,
            "merge_queue": MODULE.normalize_merge_queue_entry(
                {
                    "id": "MQE_removed",
                    "state": "AWAITING_CHECKS",
                    "position": 1,
                    "headCommit": {"oid": "queue-head-one"},
                }
            ),
        }
        absent = {**base_pr, "merge_queue": MODULE.normalize_merge_queue_entry(None)}

        with tempfile.TemporaryDirectory() as tmpdir:
            state_path = Path(tmpdir) / "state.json"
            state = {}
            active["merge_queue"] = MODULE.reconcile_merge_queue_entry(active, state)
            absent["merge_queue"] = MODULE.reconcile_merge_queue_entry(absent, state)
            first = {
                "actions": queue_action(absent),
                "queue": absent["merge_queue"],
                "state": json.loads(json.dumps(state)),
            }
            MODULE.save_state(state_path, state)

            reloaded, _ = MODULE.load_state(state_path)
            restarted = {**base_pr, "merge_queue": MODULE.normalize_merge_queue_entry(None)}
            restarted["merge_queue"] = MODULE.reconcile_merge_queue_entry(restarted, reloaded)
            second = {
                "actions": queue_action(restarted),
                "queue": restarted["merge_queue"],
                "state": json.loads(json.dumps(reloaded)),
            }

            changed_head = {
                **base_pr,
                "head_sha": "pr-head-two",
                "merge_queue": MODULE.normalize_merge_queue_entry(None),
            }
            changed_head["merge_queue"] = MODULE.reconcile_merge_queue_entry(changed_head, reloaded)
            third = {
                "actions": queue_action(changed_head),
                "queue": changed_head["merge_queue"],
                "state": json.loads(json.dumps(reloaded)),
            }

        expected_tombstone = {
            "pr_head_sha": "pr-head-one",
            "status": "removed",
            "id": "MQE_removed",
            "state": "AWAITING_CHECKS",
            "position": 1,
            "head_sha": "queue-head-one",
        }
        self.assertEqual(
            {
                "first": {"actions": first["actions"], "status": first["queue"]["status"], "state": first["state"]},
                "second": {"actions": second["actions"], "status": second["queue"]["status"], "state": second["state"]},
                "third": {"actions": third["actions"], "status": third["queue"]["status"], "state": third["state"]},
            },
            {
                "first": {
                    "actions": ["stop_merge_queue_removed"],
                    "status": "removed",
                    "state": {"merge_queue_terminal_tombstone": expected_tombstone},
                },
                "second": {
                    "actions": ["stop_merge_queue_removed"],
                    "status": "removed",
                    "state": {"merge_queue_terminal_tombstone": expected_tombstone},
                },
                "third": {
                    "actions": ["stop_ready_to_merge"],
                    "status": "absent",
                    "state": {},
                },
            },
        )

    def test_failed_merge_queue_tombstone_survives_restart_until_pr_head_changes(self):
        state = {}
        failed = {
            "closed": False,
            "merged": False,
            "head_sha": "pr-head-one",
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
            "merge_queue": MODULE.normalize_merge_queue_entry(
                {
                    "id": "MQE_failed",
                    "state": "UNMERGEABLE",
                    "position": 2,
                    "headCommit": {"oid": "queue-head-failed"},
                }
            ),
        }
        failed["merge_queue"] = MODULE.reconcile_merge_queue_entry(failed, state)
        with tempfile.TemporaryDirectory() as tmpdir:
            state_path = Path(tmpdir) / "state.json"
            MODULE.save_state(state_path, state)
            reloaded, _ = MODULE.load_state(state_path)
            restarted = {
                "closed": False,
                "merged": False,
                "head_sha": "pr-head-one",
                "mergeable": "MERGEABLE",
                "merge_state_status": "CLEAN",
                "merge_queue": MODULE.normalize_merge_queue_entry(None),
            }
            restarted["merge_queue"] = MODULE.reconcile_merge_queue_entry(restarted, reloaded)
            changed = {
                "closed": False,
                "merged": False,
                "head_sha": "pr-head-two",
                "mergeable": "MERGEABLE",
                "merge_state_status": "CLEAN",
                "merge_queue": MODULE.normalize_merge_queue_entry(None),
            }
            changed["merge_queue"] = MODULE.reconcile_merge_queue_entry(changed, reloaded)

        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0, "passed_count": 1}
        restarted_blockers = MODULE.build_merge_blockers(restarted, checks, {"failing": [], "pending": []}, {})
        changed_blockers = MODULE.build_merge_blockers(changed, checks, {"failing": [], "pending": []}, {})

        self.assertEqual(
            {
                "failed": failed["merge_queue"]["status"],
                "restarted": restarted["merge_queue"]["status"],
                "changed": changed["merge_queue"]["status"],
                "restarted_actions": MODULE.recommend_actions(
                    restarted, checks, [], [], {}, restarted_blockers, 0, 3
                ),
                "changed_actions": MODULE.recommend_actions(
                    changed, checks, [], [], {}, changed_blockers, 0, 3
                ),
                "state": reloaded,
            },
            {
                "failed": "failed",
                "restarted": "failed",
                "changed": "absent",
                "restarted_actions": ["stop_merge_queue_failed"],
                "changed_actions": ["stop_ready_to_merge"],
                "state": {},
            },
        )

    def test_fresh_different_queue_entry_supersedes_same_head_tombstone(self):
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0, "passed_count": 1}

        def queue_action(pr):
            blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})
            return MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3)

        base_pr = {
            "closed": False,
            "merged": False,
            "head_sha": "pr-head-one",
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
        }

        def active_queue(entry_id, queue_head_sha):
            return MODULE.normalize_merge_queue_entry(
                {
                    "id": entry_id,
                    "state": "AWAITING_CHECKS",
                    "position": 1,
                    "headCommit": {"oid": queue_head_sha},
                }
            )

        with tempfile.TemporaryDirectory() as tmpdir:
            state_path = Path(tmpdir) / "state.json"
            state = {}
            old_active = {**base_pr, "merge_queue": active_queue("MQE_old", "queue-old")}
            old_active["merge_queue"] = MODULE.reconcile_merge_queue_entry(old_active, state)
            old_absent = {**base_pr, "merge_queue": MODULE.normalize_merge_queue_entry(None)}
            old_absent["merge_queue"] = MODULE.reconcile_merge_queue_entry(old_absent, state)
            MODULE.save_state(state_path, state)

            reloaded, _ = MODULE.load_state(state_path)
            same_id_active = {**base_pr, "merge_queue": active_queue("MQE_old", "queue-old")}
            same_id_active["merge_queue"] = MODULE.reconcile_merge_queue_entry(same_id_active, reloaded)
            same_id = {
                "actions": queue_action(same_id_active),
                "status": same_id_active["merge_queue"]["status"],
                "state": json.loads(json.dumps(reloaded)),
            }

            new_active = {**base_pr, "merge_queue": active_queue("MQE_new", "queue-new")}
            new_active["merge_queue"] = MODULE.reconcile_merge_queue_entry(new_active, reloaded)
            reenqueue = {
                "actions": queue_action(new_active),
                "status": new_active["merge_queue"]["status"],
                "id": new_active["merge_queue"]["id"],
                "state": json.loads(json.dumps(reloaded)),
            }

            new_absent = {**base_pr, "merge_queue": MODULE.normalize_merge_queue_entry(None)}
            new_absent["merge_queue"] = MODULE.reconcile_merge_queue_entry(new_absent, reloaded)
            subsequent_null = {
                "actions": queue_action(new_absent),
                "status": new_absent["merge_queue"]["status"],
                "id": new_absent["merge_queue"]["id"],
                "state": reloaded,
            }

        self.assertEqual(
            {
                "same_id": same_id,
                "reenqueue": reenqueue,
                "subsequent_null": subsequent_null,
            },
            {
                "same_id": {
                    "actions": ["stop_merge_queue_removed"],
                    "status": "removed",
                    "state": {
                        "merge_queue_terminal_tombstone": {
                            "pr_head_sha": "pr-head-one",
                            "status": "removed",
                            "id": "MQE_old",
                            "state": "AWAITING_CHECKS",
                            "position": 1,
                            "head_sha": "queue-old",
                        }
                    },
                },
                "reenqueue": {
                    "actions": ["idle"],
                    "status": "waiting",
                    "id": "MQE_new",
                    "state": {
                        "last_merge_queue_entry": {
                            "read_state": "observed",
                            "status": "waiting",
                            "id": "MQE_new",
                            "state": "AWAITING_CHECKS",
                            "position": 1,
                            "head_sha": "queue-new",
                            "source": "github",
                            "details": "GitHub returned merge-queue evidence.",
                        },
                        "last_merge_queue_pr_head_sha": "pr-head-one",
                    },
                },
                "subsequent_null": {
                    "actions": ["stop_merge_queue_removed"],
                    "status": "removed",
                    "id": "MQE_new",
                    "state": {
                        "merge_queue_terminal_tombstone": {
                            "pr_head_sha": "pr-head-one",
                            "status": "removed",
                            "id": "MQE_new",
                            "state": "AWAITING_CHECKS",
                            "position": 1,
                            "head_sha": "queue-new",
                        }
                    },
                },
            },
        )

    def test_closed_pr_clears_queue_tracking_before_restart_or_reopen(self):
        checks = {"all_terminal": True, "failed_count": 0, "pending_count": 0, "passed_count": 1}

        def queue_action(pr):
            blockers = MODULE.build_merge_blockers(pr, checks, {"failing": [], "pending": []}, {})
            return MODULE.recommend_actions(pr, checks, [], [], {}, blockers, 0, 3)

        active = {
            "closed": False,
            "merged": False,
            "head_sha": "pr-head-one",
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
            "merge_queue": MODULE.normalize_merge_queue_entry(
                {
                    "id": "MQE_active",
                    "state": "AWAITING_CHECKS",
                    "position": 1,
                    "headCommit": {"oid": "queue-head-one"},
                }
            ),
        }
        closed = {
            "closed": True,
            "merged": False,
            "head_sha": "pr-head-one",
            "mergeable": "MERGEABLE",
            "merge_state_status": "CLEAN",
            "merge_queue": MODULE.normalize_merge_queue_entry(None),
        }

        with tempfile.TemporaryDirectory() as tmpdir:
            state_path = Path(tmpdir) / "state.json"
            state = {}
            active["merge_queue"] = MODULE.reconcile_merge_queue_entry(active, state)
            closed["merge_queue"] = MODULE.reconcile_merge_queue_entry(closed, state)
            closed_result = {
                "actions": queue_action(closed),
                "queue_status": closed["merge_queue"]["status"],
                "state": json.loads(json.dumps(state)),
            }
            MODULE.save_state(state_path, state)

            reloaded, _ = MODULE.load_state(state_path)
            reopened = {
                "closed": False,
                "merged": False,
                "head_sha": "pr-head-one",
                "mergeable": "MERGEABLE",
                "merge_state_status": "CLEAN",
                "merge_queue": MODULE.normalize_merge_queue_entry(None),
            }
            reopened["merge_queue"] = MODULE.reconcile_merge_queue_entry(reopened, reloaded)
            reopened_result = {
                "actions": queue_action(reopened),
                "queue_status": reopened["merge_queue"]["status"],
                "state": reloaded,
            }

        self.assertEqual(
            {"closed": closed_result, "reopened": reopened_result},
            {
                "closed": {
                    "actions": ["stop_pr_closed"],
                    "queue_status": "absent",
                    "state": {},
                },
                "reopened": {
                    "actions": ["stop_ready_to_merge"],
                    "queue_status": "absent",
                    "state": {},
                },
            },
        )

    def test_ci_head_context_does_not_stale_fallback_after_head_advances(self):
        pr = {"repo": "sednalabs/codex", "head_sha": "newhead"}
        state = {
            "last_seen_head_sha": "newhead",
            "last_snapshot_at": 1_000,
        }
        checks = [MODULE.pending_checks_not_reported_item()]
        checks_summary = MODULE.summarize_check_runs(checks)

        with mock.patch.object(MODULE, "get_workflow_runs_for_sha") as get_runs:
            ci_context = MODULE.build_ci_head_context(
                pr,
                state,
                checks,
                checks_summary,
                failed_runs=[],
                now=1_030,
            )

        self.assertFalse(ci_context["stale_fallback_active"])
        self.assertEqual(ci_context["stale_head_sha"], "")
        get_runs.assert_not_called()

    def test_retry_failed_now_refuses_stale_fallback_runs(self):
        args = mock.Mock(expected_head_sha="newhead", run_ids=None)
        snapshot = {
            "pr": {
                "repo": "sednalabs/codex",
                "number": 286,
                "head_sha": "newhead",
                "closed": False,
                "merged": False,
                "state": "OPEN",
            },
            "checks": {
                "all_terminal": True,
                "failed_count": 1,
                "pending_count": 0,
                "passed_count": 0,
            },
            "checks_source": "stale_fallback",
            "failed_runs": [{"run_id": 123}],
            "retry_state": {
                "current_sha_retries_used": 0,
                "max_flaky_retries": 3,
            },
        }

        with mock.patch.object(
            MODULE,
            "collect_snapshot",
            return_value=(snapshot, Path("/tmp/state.json")),
        ), mock.patch.object(MODULE, "gh_text") as gh_text:
            result = MODULE.retry_failed_now(args)

        self.assertFalse(result["rerun_attempted"])
        self.assertEqual(result["reason"], "stale_fallback_diagnostic_only")
        gh_text.assert_not_called()

    def test_failed_runs_include_first_failed_job_refs(self):
        runs = [
            {
                "id": 123,
                "name": "rust-ci",
                "status": "completed",
                "conclusion": "failure",
                "html_url": "https://example.invalid/runs/123",
                "head_sha": "abc123",
            }
        ]
        with mock.patch.object(
            MODULE,
            "failed_jobs_for_run",
            return_value=[
                {
                    "job_id": 999,
                    "job_name": "Tests — ubuntu",
                    "status": "completed",
                    "conclusion": "failure",
                    "html_url": "https://example.invalid/jobs/999",
                }
            ],
        ):
            failed = MODULE.failed_runs_from_workflow_runs(runs, "abc123", repo="sednalabs/codex")

        self.assertEqual(len(failed), 1)
        self.assertEqual(failed[0]["first_failed_job"]["job_id"], 999)
        self.assertEqual(failed[0]["failed_jobs"][0]["job_name"], "Tests — ubuntu")

    def test_failed_jobs_classify_github_billing_startup_failure(self):
        jobs_payload = {
            "jobs": [
                {
                    "id": 999,
                    "name": "Tests",
                    "status": "completed",
                    "conclusion": "failure",
                    "html_url": "https://example.invalid/jobs/999",
                    "runner_name": "",
                    "steps": [],
                }
            ]
        }
        annotations_payload = [
            {
                "annotation_level": "failure",
                "message": (
                    "The job was not started because recent account payments have failed "
                    "or your spending limit needs to be increased."
                ),
            }
        ]
        with mock.patch.object(
            MODULE,
            "gh_json",
            side_effect=[jobs_payload, annotations_payload],
        ):
            failed = MODULE.failed_jobs_for_run(123, "sednalabs/agent-ops")

        self.assertEqual(
            failed[0]["startup_failure"]["category"],
            "github_billing_or_spending_limit",
        )
        self.assertEqual(failed[0]["startup_failure"]["step_count"], 0)
        self.assertEqual(len(failed[0]["startup_failure"]["annotations"]), 1)

    def test_startup_blocker_stops_instead_of_recommending_retry(self):
        actions = MODULE.recommend_actions_with_source(
            pr={"closed": False, "merged": False},
            checks_summary={
                "all_terminal": True,
                "failed_count": 1,
                "pending_count": 0,
                "passed_count": 0,
            },
            failed_runs=[{"run_id": 123}],
            actionable_review_items=[],
            review_state={"active_unresolved_thread_count": 0},
            merge_blockers={"is_blocked_for_merge": True, "reason_kinds": ["failing_checks"]},
            retries_used=0,
            max_retries=3,
            source="current_head",
            ci_startup_blockers=[
                {
                    "run_id": 123,
                    "job_id": 999,
                    "startup_failure": {
                        "category": "github_billing_or_spending_limit",
                    },
                }
            ],
        )

        self.assertEqual(
            actions,
            ["diagnose_ci_failure", "stop_ci_startup_blocked"],
        )
        self.assertNotIn("retry_failed_checks", actions)


if __name__ == "__main__":
    unittest.main()
