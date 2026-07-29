import argparse
import importlib.util
from pathlib import Path

import pytest


MODULE_PATH = Path(__file__).with_name("gh_pr_watch.py")
MODULE_SPEC = importlib.util.spec_from_file_location("gh_pr_watch", MODULE_PATH)
gh_pr_watch = importlib.util.module_from_spec(MODULE_SPEC)
assert MODULE_SPEC.loader is not None
MODULE_SPEC.loader.exec_module(gh_pr_watch)


def sample_pr():
    return {
        "number": 123,
        "url": "https://github.com/openai/codex/pull/123",
        "repo": "openai/codex",
        "head_sha": "abc123",
        "head_branch": "feature",
        "state": "OPEN",
        "merged": False,
        "closed": False,
        "mergeable": "MERGEABLE",
        "merge_state_status": "CLEAN",
        "review_decision": "",
    }


def sample_checks(**overrides):
    checks = {
        "pending_count": 0,
        "failed_count": 0,
        "passed_count": 12,
        "all_terminal": True,
    }
    checks.update(overrides)
    return checks


def test_collect_snapshot_fetches_review_items_before_ci(monkeypatch, tmp_path):
    call_order = []
    pr = sample_pr()

    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *args, **kwargs: pr)
    monkeypatch.setattr(
        gh_pr_watch,
        "detect_local_git_context",
        lambda: {
            "cwd": "",
            "git_root": "",
            "origin_repo": "openai/codex",
            "origin_url": "",
            "upstream_repo": "",
            "upstream_url": "",
        },
    )
    monkeypatch.setattr(gh_pr_watch, "load_state", lambda path: ({}, True))
    monkeypatch.setattr(
        gh_pr_watch,
        "get_authenticated_login",
        lambda: call_order.append("auth") or "octocat",
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "fetch_new_review_items",
        lambda *args, **kwargs: call_order.append("review") or [],
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "get_review_threads",
        lambda *args, **kwargs: call_order.append("threads") or [],
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "partition_unresolved_review_threads",
        lambda *args, **kwargs: ([], []),
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "build_actionable_review_items",
        lambda *args, **kwargs: call_order.append("actionable") or [],
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "get_pr_checks",
        lambda *args, **kwargs: call_order.append("checks") or [],
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "summarize_checks",
        lambda checks: call_order.append("summarize") or sample_checks(),
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "get_workflow_runs_for_sha",
        lambda *args, **kwargs: call_order.append("workflow") or [],
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "failed_runs_from_workflow_runs",
        lambda *args, **kwargs: call_order.append("failed_runs") or [],
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "failed_jobs_from_workflow_runs",
        lambda *args, **kwargs: call_order.append("failed_jobs") or [],
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "recommend_actions",
        lambda *args, **kwargs: call_order.append("recommend") or ["idle"],
    )
    monkeypatch.setattr(gh_pr_watch, "save_state", lambda *args, **kwargs: None)

    args = argparse.Namespace(
        pr="123",
        repo=None,
        state_file=str(tmp_path / "watcher-state.json"),
        ignore_review_thread=[],
        max_flaky_retries=3,
        reset_seen_feedback=False,
    )

    gh_pr_watch.collect_snapshot(args)

    assert call_order.index("review") < call_order.index("checks")
    assert call_order.index("review") < call_order.index("workflow")


def test_recommend_actions_prioritizes_review_comments():
    actions = gh_pr_watch.recommend_actions(
        sample_pr(),
        sample_checks(failed_count=1),
        [{"run_id": 99}],
        [],
        [{"kind": "review_comment", "id": "1"}],
        {},
        0,
        3,
    )

    assert actions == [
        "process_review_comment",
        "diagnose_ci_failure",
        "retry_failed_checks",
    ]


def test_pending_review_feedback_surfaces_only_after_publication(monkeypatch):
    state = {
        "seen_review_comment_ids": ["20"],
        "seen_review_ids": ["10"],
    }
    review = {
        "id": 10,
        "user": {"login": "octocat"},
        "author_association": "MEMBER",
        "state": "PENDING",
        "body": "Please rename this.",
        "created_at": "2026-06-08T10:00:00Z",
        "submitted_at": None,
        "html_url": "https://github.com/openai/codex/pull/123#pullrequestreview-10",
    }
    review_comment = {
        "id": 20,
        "pull_request_review_id": 10,
        "user": {"login": "octocat"},
        "author_association": "MEMBER",
        "body": "Please rename this.",
        "created_at": "2026-06-08T10:00:00Z",
        "path": "src/example.rs",
        "line": 7,
        "html_url": "https://github.com/openai/codex/pull/123#discussion_r20",
    }

    def fake_list(endpoint, **kwargs):
        if endpoint.endswith("/issues/123/comments"):
            return []
        if endpoint.endswith("/pulls/123/comments"):
            return [review_comment]
        if endpoint.endswith("/pulls/123/reviews"):
            return [review]
        raise AssertionError(f"unexpected endpoint: {endpoint}")

    monkeypatch.setattr(gh_pr_watch, "gh_api_list_paginated", fake_list)

    assert (
        gh_pr_watch.fetch_new_review_items(
            sample_pr(),
            state,
            fresh_state=True,
            authenticated_login="octocat",
        )
        == []
    )
    assert state["seen_review_comment_ids"] == []
    assert state["seen_review_ids"] == []

    review["state"] = "COMMENTED"
    review["submitted_at"] = "2026-06-08T10:05:00Z"

    published_items = gh_pr_watch.fetch_new_review_items(
        sample_pr(),
        state,
        fresh_state=False,
        authenticated_login="octocat",
    )

    assert {(item["kind"], item["id"]) for item in published_items} == {
        ("review", "10"),
        ("review_comment", "20"),
    }
    assert state["seen_review_comment_ids"] == ["20"]
    assert state["seen_review_ids"] == ["10"]


def test_run_watch_keeps_polling_open_ready_to_merge_pr(monkeypatch):
    sleeps = []
    events = []
    snapshot = {
        "pr": sample_pr(),
        "checks": sample_checks(),
        "failed_runs": [],
        "failed_jobs": [],
        "new_review_items": [],
        "actions": ["ready_to_merge"],
        "retry_state": {
            "current_sha_retries_used": 0,
            "max_flaky_retries": 3,
        },
    }

    monkeypatch.setattr(
        gh_pr_watch,
        "collect_snapshot",
        lambda args: (snapshot, Path("/tmp/codex-babysit-pr-state.json")),
    )
    monkeypatch.setattr(
        gh_pr_watch,
        "print_event",
        lambda event, payload: events.append((event, payload)),
    )

    class StopWatch(Exception):
        pass

    def fake_sleep(seconds):
        sleeps.append(seconds)
        if len(sleeps) >= 2:
            raise StopWatch

    monkeypatch.setattr(gh_pr_watch.time, "sleep", fake_sleep)

    with pytest.raises(StopWatch):
        gh_pr_watch.run_watch(argparse.Namespace(poll_seconds=30))

    assert sleeps == [30, 60]
    assert [event for event, _ in events] == ["snapshot", "snapshot"]


def test_failed_jobs_include_direct_logs_endpoint(monkeypatch):
    jobs_by_run = {
        99: [
            {
                "id": 555,
                "name": "unit tests",
                "status": "completed",
                "conclusion": "failure",
                "html_url": "https://github.com/openai/codex/actions/runs/99/job/555",
            },
            {
                "id": 556,
                "name": "lint",
                "status": "completed",
                "conclusion": "success",
            },
        ]
    }

    monkeypatch.setattr(
        gh_pr_watch,
        "get_jobs_for_run",
        lambda repo, run_id: jobs_by_run[run_id],
    )

    failed_jobs = gh_pr_watch.failed_jobs_from_workflow_runs(
        "openai/codex",
        [
            {
                "id": 99,
                "name": "CI",
                "status": "in_progress",
                "conclusion": "",
                "head_sha": "abc123",
            }
        ],
        "abc123",
    )

    assert failed_jobs == [
        {
            "run_id": 99,
            "workflow_name": "CI",
            "run_status": "in_progress",
            "run_conclusion": "",
            "job_id": 555,
            "job_name": "unit tests",
            "status": "completed",
            "conclusion": "failure",
            "html_url": "https://github.com/openai/codex/actions/runs/99/job/555",
            "logs_endpoint": "repos/openai/codex/actions/jobs/555/logs",
        }
    ]


def test_parse_args_watch_until_terminal_implies_terminal_checks(monkeypatch):
    monkeypatch.setattr(
        gh_pr_watch.sys,
        "argv",
        ["gh_pr_watch.py", "--watch-until-terminal"],
    )

    args = gh_pr_watch.parse_args()

    assert args.watch_until_terminal is True
    assert args.watch_until_action is True
    assert args.require_terminal_checks is True
    assert args.once is False


def test_compact_wait_snapshot_caps_review_body():
    snapshot = {
        "pr": {
            "repo": "openai/codex",
            "number": 123,
            "head_sha": "abc123",
            "state": "OPEN",
            "large_unneeded_field": "x" * 5000,
        },
        "checks": {"all_terminal": True},
        "actionable_review_items": [
            {
                "kind": "thread",
                "id": "thread-1",
                "body": "y" * 2000,
                "large_unneeded_field": "z" * 5000,
            }
        ],
        "actions": ["address_review_feedback"],
    }

    compact = gh_pr_watch.compact_wait_snapshot(snapshot)

    assert "large_unneeded_field" not in compact["pr"]
    assert "large_unneeded_field" not in compact["actionable_review_items"][0]
    assert len(compact["actionable_review_items"][0]["body"]) == 1000


def test_watch_until_action_is_silent_by_default(monkeypatch, tmp_path, capsys):
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
    snapshots = iter(
        [
            (idle_snapshot, tmp_path / "state.json"),
            (action_snapshot, tmp_path / "state.json"),
        ]
    )
    monkeypatch.setattr(gh_pr_watch, "collect_snapshot", lambda args: next(snapshots))
    monkeypatch.setattr(gh_pr_watch.time, "sleep", lambda _seconds: None)
    args = argparse.Namespace(
        poll_seconds=30,
        require_terminal_checks=False,
        progress=False,
        verbose_details=False,
    )

    assert gh_pr_watch.run_watch_until_action(args) == 0
    captured = capsys.readouterr()
    assert captured.err == ""
    receipt = json.loads(captured.out)
    assert receipt["polls_completed"] == 2
    assert receipt["snapshot"]["actions"] == ["diagnose_ci_failure"]


def test_watch_until_action_waits_for_terminal_ci_failure(
    monkeypatch, tmp_path, capsys
):
    in_progress_failure = {
        "pr": {"head_sha": "abc123"},
        "checks": {
            "all_terminal": False,
            "failed_count": 1,
            "pending_count": 1,
            "passed_count": 0,
        },
        "actions": ["diagnose_ci_failure"],
    }
    terminal_failure = {
        "pr": {"head_sha": "abc123"},
        "checks": {
            "all_terminal": True,
            "failed_count": 1,
            "pending_count": 0,
            "passed_count": 0,
        },
        "actions": ["diagnose_ci_failure"],
    }
    snapshots = iter(
        [
            (in_progress_failure, tmp_path / "state.json"),
            (terminal_failure, tmp_path / "state.json"),
        ]
    )
    monkeypatch.setattr(gh_pr_watch, "collect_snapshot", lambda args: next(snapshots))
    monkeypatch.setattr(gh_pr_watch.time, "sleep", lambda _seconds: None)
    args = argparse.Namespace(
        poll_seconds=30,
        require_terminal_checks=True,
        progress=False,
        verbose_details=False,
    )

    assert gh_pr_watch.run_watch_until_action(args) == 0
    receipt = json.loads(capsys.readouterr().out)
    assert receipt["polls_completed"] == 2
    assert receipt["snapshot"]["checks"]["all_terminal"] is True
