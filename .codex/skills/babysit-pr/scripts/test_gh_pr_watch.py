import argparse
import importlib.util
import types
from pathlib import Path

import pytest


MODULE_PATH = Path(__file__).with_name("gh_pr_watch.py")
MODULE_SPEC = importlib.util.spec_from_file_location("gh_pr_watch", MODULE_PATH)
gh_pr_watch = importlib.util.module_from_spec(MODULE_SPEC)
assert MODULE_SPEC.loader is not None
MODULE_SPEC.loader.exec_module(gh_pr_watch)


def sample_pr(**overrides):
    pr = {
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
        "merge_queue": {"status": "absent"},
    }
    pr.update(overrides)
    return pr


def sample_checks(**overrides):
    checks = {
        "pending_count": 0,
        "failed_count": 0,
        "passed_count": 12,
        "all_terminal": True,
    }
    checks.update(overrides)
    return checks


def test_broker_routes_pr_gh_text_without_token(monkeypatch):
    calls = []

    def request(path, argv):
        calls.append((path, argv))
        return {"returncode": 0, "stdout": "{}", "stderr": ""}

    monkeypatch.setenv("GITHUB_APP_BROKER_SOCKET", "/tmp/broker.sock")
    monkeypatch.setitem(
        __import__("sys").modules,
        "github_app_broker_proxy",
        types.SimpleNamespace(request=request),
    )
    assert gh_pr_watch.gh_text(["pr", "view", "1"], repo="o/r") == "{}"
    assert calls == [("/tmp/broker.sock", ["-R", "o/r", "pr", "view", "1"])]


def test_resolve_pr_falls_back_when_cli_lacks_base_ref_oid(monkeypatch):
    calls = []
    payload = {
        "number": 123,
        "url": "https://github.com/openai/codex/pull/123",
        "state": "OPEN",
        "mergedAt": "",
        "closedAt": "",
        "headRefName": "feature",
        "headRefOid": "abc123",
        "headRepository": {"nameWithOwner": "openai/codex"},
        "headRepositoryOwner": {"login": "openai"},
        "baseRefName": "main",
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "reviewDecision": "",
    }

    def fake_gh_json(args, repo=None):
        calls.append(list(args))
        if "baseRefOid" in args[-1]:
            raise gh_pr_watch.GhCommandError('Unknown JSON field: "baseRefOid"')
        return payload

    monkeypatch.setattr(gh_pr_watch, "gh_json", fake_gh_json)
    pr = gh_pr_watch.resolve_pr("https://github.com/openai/codex/pull/123")
    assert len(calls) == 2
    assert pr["base_sha"] == ""


def test_get_pr_checks_falls_back_to_status_rollup(monkeypatch):
    calls = []

    def fake_gh_json(args, repo=None):
        calls.append(list(args))
        if args[:2] == ["pr", "checks"]:
            raise gh_pr_watch.GhCommandError("unknown flag: --json")
        return {
            "statusCheckRollup": [
                {
                    "__typename": "CheckRun",
                    "name": "unit tests",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                    "detailsUrl": "https://example.test/run/1",
                    "workflowName": "CI",
                }
            ]
        }

    monkeypatch.setattr(gh_pr_watch, "gh_json", fake_gh_json)
    checks = gh_pr_watch.get_pr_checks("123", "openai/codex")
    assert calls[1] == ["pr", "view", "123", "--json", "statusCheckRollup"]
    assert checks[0]["bucket"] == "pass"


@pytest.mark.parametrize(
    "rollup_payload, expected_message",
    [
        ([], "Malformed `statusCheckRollup` fallback payload"),
        ({"statusCheckRollup": {}}, "Malformed `statusCheckRollup` fallback list"),
        ({"statusCheckRollup": ["not-an-entry"]}, "Malformed status-check rollup entry"),
    ],
)
def test_get_pr_checks_rejects_malformed_status_rollup(monkeypatch, rollup_payload, expected_message):
    def fake_gh_json(args, repo=None):
        if args[:2] == ["pr", "checks"]:
            raise gh_pr_watch.GhCommandError("unknown flag: --json")
        return rollup_payload

    monkeypatch.setattr(gh_pr_watch, "gh_json", fake_gh_json)
    with pytest.raises(gh_pr_watch.GhCommandError, match=expected_message):
        gh_pr_watch.get_pr_checks("123", "openai/codex")


@pytest.mark.parametrize(
    "entry",
    [
        {"__typename": "CheckRun", "name": "", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": None},
        {"__typename": "CheckRun", "name": "ci", "status": "IN_PROGRESS", "conclusion": "SUCCESS"},
        {"__typename": "StatusContext", "context": "", "state": "SUCCESS"},
        {"__typename": "StatusContext", "context": "ci", "state": "UNKNOWN"},
        {"__typename": "Other", "name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"__typename": "Other", "context": "ci", "state": "SUCCESS"},
    ],
)
def test_get_pr_checks_rejects_incomplete_status_rollup_dict(monkeypatch, entry):
    monkeypatch.setattr(
        gh_pr_watch,
        "gh_json",
        lambda args, repo=None: (
            (_ for _ in ()).throw(gh_pr_watch.GhCommandError("unknown flag: --json"))
            if args[:2] == ["pr", "checks"]
            else {"statusCheckRollup": [entry]}
        ),
    )
    with pytest.raises(gh_pr_watch.GhCommandError):
        gh_pr_watch.get_pr_checks("123", "openai/codex")


@pytest.mark.parametrize(
    "entry",
    [
        {"__typename": "CheckRun", "name": "ci", "status": "IN_PROGRESS", "conclusion": None},
        {"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "CANCELLED"},
        {"__typename": "StatusContext", "context": "ci", "state": "PENDING"},
        {"__typename": "StatusContext", "context": "ci", "state": "ERROR"},
    ],
)
def test_get_pr_checks_accepts_supported_status_rollup_shapes(monkeypatch, entry):
    monkeypatch.setattr(
        gh_pr_watch,
        "gh_json",
        lambda args, repo=None: (
            (_ for _ in ()).throw(gh_pr_watch.GhCommandError("unknown flag: --json"))
            if args[:2] == ["pr", "checks"]
            else {"statusCheckRollup": [entry]}
        ),
    )
    checks = gh_pr_watch.get_pr_checks("123", "openai/codex")
    assert len(checks) == 1


@pytest.mark.parametrize("status", ["REQUESTED", "QUEUED", "IN_PROGRESS", "WAITING", "PENDING"])
def test_check_run_nonterminal_statuses_accept_null_conclusion(status):
    check = gh_pr_watch._normalize_status_rollup_check(
        {"__typename": "CheckRun", "name": "ci", "status": status, "conclusion": None}
    )
    assert check["bucket"] == "pending"


@pytest.mark.parametrize(
    "conclusion, expected_bucket",
    [
        ("ACTION_REQUIRED", "fail"),
        ("TIMED_OUT", "fail"),
        ("CANCELLED", "fail"),
        ("FAILURE", "fail"),
        ("SUCCESS", "pass"),
        ("NEUTRAL", "pass"),
        ("SKIPPED", "pass"),
        ("STARTUP_FAILURE", "fail"),
        ("STALE", "fail"),
    ],
)
def test_check_run_terminal_conclusions_are_classified(conclusion, expected_bucket):
    check = gh_pr_watch._normalize_status_rollup_check(
        {"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": conclusion}
    )
    assert check["bucket"] == expected_bucket


@pytest.mark.parametrize("state", ["EXPECTED", "PENDING"])
def test_status_context_pending_states_are_pending(state):
    check = gh_pr_watch._normalize_status_rollup_check(
        {"__typename": "StatusContext", "context": "ci", "state": state}
    )
    assert check["bucket"] == "pending"


@pytest.mark.parametrize("state", ["ERROR", "FAILURE", "SUCCESS"])
def test_status_context_terminal_states_are_classified(state):
    check = gh_pr_watch._normalize_status_rollup_check(
        {"__typename": "StatusContext", "context": "ci", "state": state}
    )
    assert check["bucket"] == ("pass" if state == "SUCCESS" else "fail")


def test_parse_args_watch_until_terminal_implies_action_wait_and_terminal_checks(monkeypatch):
    monkeypatch.setattr(gh_pr_watch.sys, "argv", ["gh_pr_watch.py", "--watch-until-terminal"])
    args = gh_pr_watch.parse_args()
    assert args.watch_until_terminal
    assert args.watch_until_action
    assert args.require_terminal_checks


def test_fetch_new_review_items_returns_review_items_only_when_requested(monkeypatch):
    payloads = [
        [],
        [{"id": 2, "user": {"login": "gemini-code-assist[bot]"}, "body": "finding", "path": "x.py"}],
        [],
    ]
    monkeypatch.setattr(gh_pr_watch, "gh_api_list_paginated", lambda *_args, **_kwargs: payloads.pop(0))
    pr = {"repo": "openai/codex", "number": 53}
    state = {"seen_issue_comment_ids": [], "seen_review_comment_ids": [], "seen_review_ids": []}
    items = gh_pr_watch.fetch_new_review_items(pr, state, fresh_state=False, authenticated_login="maintainer")
    assert [item["kind"] for item in items] == ["review_comment"]
    assert state["seen_review_comment_ids"] == ["2"]


def test_fetch_new_review_items_can_return_reviews_for_head_summary(monkeypatch):
    monkeypatch.setattr(gh_pr_watch, "gh_api_list_paginated", lambda *_args, **_kwargs: [])
    pr = {"repo": "openai/codex", "number": 53}
    result = gh_pr_watch.fetch_new_review_items(
        pr,
        {"seen_issue_comment_ids": [], "seen_review_comment_ids": [], "seen_review_ids": []},
        fresh_state=False,
        authenticated_login=None,
        include_review_items=True,
    )
    assert result == ([], [])


def test_startup_blockers_identify_failed_job_without_steps():
    blockers = gh_pr_watch.startup_blockers_from_failed_runs(
        [{
            "run_id": 7,
            "status": "completed",
            "conclusion": "failure",
            "failed_jobs": [
                {
                    "job_id": 8,
                    "job_name": "unit",
                    "startup_failure": "no runner or steps were recorded",
                }
            ],
        }]
    )
    assert blockers[0]["run_id"] == 7


def test_failed_runs_include_failing_jobs_from_in_progress_runs(monkeypatch):
    calls = []
    monkeypatch.setattr(
        gh_pr_watch,
        "failed_jobs_for_run",
        lambda run_id, repo=None: calls.append((run_id, repo))
        or [{"job_id": 8, "job_name": "unit", "conclusion": "failure"}],
    )
    runs = [{"id": 7, "head_sha": "abc123", "status": "in_progress", "conclusion": None, "run_attempt": 1}]
    failed = gh_pr_watch.failed_runs_from_workflow_runs(runs, "abc123", repo="openai/codex")
    assert failed[0]["run_id"] == 7
    assert failed[0]["failed_jobs"][0]["job_id"] == 8
    assert calls == [(7, "openai/codex")]


def test_failed_runs_cache_completed_attempt_and_refreshes_successor(monkeypatch):
    calls = []
    monkeypatch.setattr(
        gh_pr_watch,
        "failed_jobs_for_run",
        lambda run_id, repo=None: calls.append((run_id, repo))
        or [{"job_id": 8, "job_name": "unit", "conclusion": "failure"}],
    )
    cache = {}
    run_v1 = {"id": 7, "head_sha": "abc123", "status": "completed", "conclusion": "failure", "run_attempt": 1}
    run_v2 = {**run_v1, "run_attempt": 2}
    gh_pr_watch.failed_runs_from_workflow_runs([run_v1], "abc123", repo="openai/codex", cache=cache)
    gh_pr_watch.failed_runs_from_workflow_runs([run_v1], "abc123", repo="openai/codex", cache=cache)
    gh_pr_watch.failed_runs_from_workflow_runs([run_v2], "abc123", repo="openai/codex", cache=cache)
    assert calls == [(7, "openai/codex"), (7, "openai/codex")]


def test_failed_runs_never_queries_jobs_for_other_head(monkeypatch):
    calls = []
    monkeypatch.setattr(gh_pr_watch, "failed_jobs_for_run", lambda *args, **kwargs: calls.append(args) or [])
    runs = [{"id": 7, "head_sha": "other", "status": "in_progress", "conclusion": None, "run_attempt": 1}]
    assert gh_pr_watch.failed_runs_from_workflow_runs(runs, "abc123", repo="openai/codex", cache={}) == []
    assert calls == []


def test_recommend_actions_prioritizes_review_and_diagnosis():
    actions = gh_pr_watch.recommend_actions(
        sample_pr(),
        sample_checks(failed_count=1),
        [{"run_id": 99}],
        [{"kind": "review_comment", "id": "1"}],
        {},
        {"is_blocked_for_merge": False, "reason_kinds": []},
        0,
        3,
    )
    assert actions == ["process_review_comment", "diagnose_ci_failure", "retry_failed_checks"]


def test_active_merge_queue_cannot_report_ready():
    pr = sample_pr(merge_queue={"status": "waiting"})
    assert gh_pr_watch.recommend_actions(
        pr,
        sample_checks(),
        [],
        [],
        {},
        {"is_blocked_for_merge": False, "reason_kinds": ["merge_queue_waiting"]},
        0,
        3,
    ) == ["idle"]


def test_no_feedback_bot_review_submission_is_not_meaningful():
    item = {
        "kind": "review",
        "author": "gemini-code-assist[bot]",
        "body": "There are no review comments and no feedback to provide.",
    }
    assert not gh_pr_watch.is_meaningful_review_submission(item)


def test_policy_blocker_does_not_backoff_and_decision_is_exact_head():
    args = argparse.Namespace(poll_seconds=30)
    snapshot = {
        "pr": {"repo": "openai/codex", "number": 123, "head_sha": "abc123"},
        "checks": sample_checks(),
        "review_state": {},
        "actions": [gh_pr_watch.ACTION_REQUIRED_MERGE_POLICY_BLOCKED],
    }
    delay, _ = gh_pr_watch.next_watch_poll_seconds(args, snapshot, ("unchanged",), 600, 3600)
    assert delay == 30
    decision = gh_pr_watch.build_watch_decision(snapshot, recorded_at=100)
    assert decision["head_sha"] == "abc123"
    assert decision["decision"] == "action_required"


@pytest.mark.parametrize("queue_state", ["QUEUED", "AWAITING_CHECKS"])
def test_active_merge_queue_wait_uses_base_cadence(queue_state):
    args = argparse.Namespace(poll_seconds=30)
    snapshot = {
        "checks": sample_checks(),
        "actions": ["idle"],
        "pr": {
            "repo": "openai/codex",
            "number": 123,
            "head_sha": "abc123",
            "merge_queue": {"status": "waiting", "id": "entry-1", "state": queue_state, "head_sha": "abc123"},
        },
    }
    delay, _ = gh_pr_watch.next_watch_poll_seconds(
        args, snapshot, gh_pr_watch.snapshot_change_key(snapshot), 600, 3600
    )
    assert delay == 30


@pytest.mark.parametrize(
    "queue_status, expected",
    [("failed", gh_pr_watch.STOP_MERGE_QUEUE_FAILED), ("removed", gh_pr_watch.STOP_MERGE_QUEUE_REMOVED)],
)
def test_queue_failure_and_removal_are_actionable(queue_status, expected):
    pr = sample_pr(merge_queue={"status": queue_status, "state": "QUEUED", "id": "entry-1"})
    actions = gh_pr_watch.recommend_actions(
        pr,
        sample_checks(),
        [],
        [],
        [],
        {"is_blocked_for_merge": True, "reason_kinds": [f"merge_queue_{queue_status}"]},
        0,
        3,
    )
    assert expected in actions


def test_queue_tombstone_preserves_failure_until_head_changes():
    state = {}
    failed = {"head_sha": "head-1", "merge_queue": {"status": "failed", "id": "entry-1", "state": "FAILED"}}
    gh_pr_watch.reconcile_merge_queue_entry(failed, state)
    absent = {"head_sha": "head-1", "merge_queue": {"status": "absent"}}
    assert gh_pr_watch.reconcile_merge_queue_entry(absent, state)["status"] == "failed"
    new_head = {"head_sha": "head-2", "merge_queue": {"status": "absent"}}
    assert gh_pr_watch.reconcile_merge_queue_entry(new_head, state)["status"] == "absent"


@pytest.mark.parametrize(
    "payload", [{"data": ["bad"]}, {"data": {"repository": "bad"}}, {"data": {"repository": {"pullRequest": "bad"}}}],
)
def test_malformed_queue_payload_is_unknown_and_not_ready(monkeypatch, payload):
    monkeypatch.setattr(gh_pr_watch, "gh_json", lambda *_args, **_kwargs: payload)
    queue = gh_pr_watch.get_merge_queue_entry("openai/codex", 123)
    assert queue["status"] == "unknown"
    assert gh_pr_watch.recommend_actions(
        sample_pr(merge_queue=queue),
        sample_checks(),
        [],
        [],
        [],
        {"is_blocked_for_merge": True, "reason_kinds": ["merge_queue_read_error"]},
        0,
        3,
    ) == [gh_pr_watch.STOP_MERGE_QUEUE_READ_ERROR]


def test_nonqueued_green_idle_snapshot_keeps_backoff():
    args = argparse.Namespace(poll_seconds=30)
    snapshot = {
        "pr": {"repo": "openai/codex", "number": 123, "head_sha": "abc123"},
        "checks": sample_checks(),
        "actions": ["idle"],
        "merge_blockers": [],
    }
    delay, _ = gh_pr_watch.next_watch_poll_seconds(
        args, snapshot, gh_pr_watch.snapshot_change_key(snapshot), 600, 3600
    )
    assert delay == 1200


def test_schedule_persists_exact_head_and_fake_clock(monkeypatch, tmp_path):
    saved = {}
    monkeypatch.setattr(gh_pr_watch, "load_state", lambda _path: ({}, True))
    monkeypatch.setattr(gh_pr_watch, "save_state", lambda _path, state: saved.update(state))
    snapshot = {"pr": {"repo": "openai/codex", "number": 123, "head_sha": "abc123"}}
    gh_pr_watch.persist_watch_schedule(
        tmp_path / "state.json", snapshot, "watch-until-action", 30, scheduled_at=100
    )
    assert saved["watch_schedule"]["head_sha"] == "abc123"
    assert saved["watch_schedule"]["wake_at"] == 130


@pytest.mark.parametrize("name", ["/tmp/state.json", "nested/state.json", ".", ".."])
def test_state_file_rejects_paths_and_dot_names(name):
    with pytest.raises(RuntimeError):
        gh_pr_watch.safe_state_file_name(name)


def test_state_file_uses_basename_under_system_tempdir(monkeypatch):
    monkeypatch.setattr(gh_pr_watch.tempfile, "gettempdir", lambda: "/tmp/watcher-state")
    args = argparse.Namespace(state_file="watch.json")
    path = gh_pr_watch.state_file_for(args, sample_pr())
    assert path == Path("/tmp/watcher-state/watch.json")


def retry_snapshot(*run_ids, checks=None):
    return {
        "pr": sample_pr(),
        "checks": sample_checks(**(checks or {"failed_count": 1})),
        "failed_runs": [{"run_id": run_id} for run_id in run_ids],
        "retry_state": {"current_sha_retries_used": 0, "max_flaky_retries": 3},
    }


def retry_args(*run_ids, expected_head_sha="abc123", max_flaky_retries=3):
    return argparse.Namespace(
        pr="https://github.com/openai/codex/pull/123",
        repo="openai/codex",
        expected_head_sha=expected_head_sha,
        run_ids=[str(run_id) for run_id in run_ids] or None,
        max_flaky_retries=max_flaky_retries,
    )


def workflow_run(run_id=99, **overrides):
    run = {
        "id": run_id,
        "head_sha": "abc123",
        "status": "completed",
        "conclusion": "failure",
        "run_attempt": 1,
        "pull_requests": [{"number": 123}],
    }
    run.update(overrides)
    return run


def install_retry_snapshot(monkeypatch, snapshot):
    monkeypatch.setattr(
        gh_pr_watch,
        "collect_snapshot",
        lambda _args, cache=None: (snapshot, Path("/tmp/codex-babysit-pr-state.json")),
    )
    monkeypatch.setattr(gh_pr_watch, "load_state", lambda _path: ({}, True))
    monkeypatch.setattr(gh_pr_watch, "save_state", lambda *_args: None)


def test_retry_rejects_pr_head_change_before_mutation(monkeypatch):
    install_retry_snapshot(monkeypatch, retry_snapshot(99))
    changed = sample_pr(head_sha="changed")
    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *_args, **_kwargs: changed)
    calls = []
    monkeypatch.setattr(gh_pr_watch, "gh_text", lambda *args, **kwargs: calls.append((args, kwargs)))
    result = gh_pr_watch.retry_failed_now(retry_args(99))
    assert result["reason"] == "pr_head_mismatch"
    assert result["rerun_attempted"] is False
    assert calls == []


def test_retry_rejects_run_head_mismatch_before_mutation(monkeypatch):
    install_retry_snapshot(monkeypatch, retry_snapshot(99))
    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *_args, **_kwargs: sample_pr())
    monkeypatch.setattr(gh_pr_watch, "get_workflow_run", lambda *_args: workflow_run(head_sha="different"))
    calls = []
    monkeypatch.setattr(gh_pr_watch, "gh_text", lambda *args, **kwargs: calls.append((args, kwargs)))
    result = gh_pr_watch.retry_failed_now(retry_args(99))
    assert result["reason"] == "run_head_mismatch"
    assert result["rerun_attempted"] is False
    assert calls == []


@pytest.mark.parametrize("pr_flags", [{"closed": True}, {"merged": True}])
def test_retry_rejects_closed_or_merged_pr(monkeypatch, pr_flags):
    install_retry_snapshot(monkeypatch, retry_snapshot(99))
    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *_args, **_kwargs: sample_pr(**pr_flags))
    calls = []
    monkeypatch.setattr(gh_pr_watch, "gh_text", lambda *args, **kwargs: calls.append((args, kwargs)))
    result = gh_pr_watch.retry_failed_now(retry_args(99))
    assert result["reason"] == "pr_closed_or_merged"
    assert result["rerun_attempted"] is False
    assert calls == []


def test_retry_rejects_run_without_exact_pr_association(monkeypatch):
    install_retry_snapshot(monkeypatch, retry_snapshot(99))
    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *_args, **_kwargs: sample_pr())
    monkeypatch.setattr(gh_pr_watch, "get_workflow_run", lambda *_args: workflow_run(pull_requests=[]))
    calls = []
    monkeypatch.setattr(gh_pr_watch, "gh_text", lambda *args, **kwargs: calls.append((args, kwargs)))
    result = gh_pr_watch.retry_failed_now(retry_args(99))
    assert result["reason"] == "run_pr_association_missing"
    assert result["rerun_attempted"] is False
    assert calls == []


def test_retry_stops_on_ambiguous_command_without_second_mutation(monkeypatch):
    snapshot = retry_snapshot(99, 100)
    snapshot["retry_state"]["max_flaky_retries"] = 1
    install_retry_snapshot(monkeypatch, snapshot)
    state = {"retries_by_sha": {}}
    monkeypatch.setattr(gh_pr_watch, "load_state", lambda _path: (state, False))
    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *_args, **_kwargs: sample_pr())
    monkeypatch.setattr(gh_pr_watch, "get_workflow_run", lambda _repo, run_id: workflow_run(id=int(run_id)))
    calls = []

    def ambiguous_command(*args, **kwargs):
        calls.append((args, kwargs))
        raise gh_pr_watch.GhCommandError("provider response was ambiguous")

    monkeypatch.setattr(gh_pr_watch, "gh_text", ambiguous_command)
    result = gh_pr_watch.retry_failed_now(retry_args(99, 100, max_flaky_retries=1))
    assert result["reason"] == "rerun_command_ambiguous"
    assert result["rerun_attempted"] is False
    assert len(calls) == 1
    assert state["retries_by_sha"]["abc123"] == 1


def test_installation_observer_filters_untrusted_items_without_identity(monkeypatch):
    state = {}
    items = [
        {
            "id": 1,
            "user": {"login": "owner"},
            "author_association": "OWNER",
            "body": "ok",
        },
        {
            "id": 2,
            "user": {"login": "member"},
            "author_association": "MEMBER",
            "body": "ok",
        },
        {
            "id": 3,
            "user": {"login": "collab"},
            "author_association": "COLLABORATOR",
            "body": "ok",
        },
        {
            "id": 4,
            "user": {"login": "codex-review[bot]"},
            "author_association": "NONE",
            "body": "ok",
        },
        {
            "id": 5,
            "user": {"login": "outsider"},
            "author_association": "NONE",
            "body": "ignore",
        },
    ]
    monkeypatch.setattr(
        gh_pr_watch,
        "gh_api_list_paginated",
        lambda endpoint, **_kwargs: (
            items if endpoint.endswith("/issues/123/comments") else []
        ),
    )

    surfaced = gh_pr_watch.fetch_new_review_items(
        sample_pr(), state, fresh_state=True, authenticated_login=None
    )

    assert {item["id"] for item in surfaced} == {"1", "2", "3", "4"}


def test_parse_args_accepts_installation_observer(monkeypatch):
    monkeypatch.setattr(
        gh_pr_watch.sys, "argv", ["gh_pr_watch.py", "--installation-observer"]
    )

    args = gh_pr_watch.parse_args()

    assert args.installation_observer is True


def test_installation_observer_rejects_retry_without_mutation(monkeypatch):
    mutation_called = False
    gh_called = False

    def unexpected_mutation(*_args, **_kwargs):
        nonlocal mutation_called
        mutation_called = True

    def unexpected_gh(*_args, **_kwargs):
        nonlocal gh_called
        gh_called = True

    monkeypatch.setattr(
        gh_pr_watch.sys,
        "argv",
        [
            "gh_pr_watch.py",
            "--installation-observer",
            "--retry-failed-now",
            "--expected-head-sha",
            "abc123",
        ],
    )
    monkeypatch.setattr(gh_pr_watch, "retry_failed_now", unexpected_mutation)
    monkeypatch.setattr(gh_pr_watch, "gh_json", unexpected_gh)

    with pytest.raises(SystemExit) as error:
        gh_pr_watch.main()

    assert error.value.code == 2
    assert mutation_called is False
    assert gh_called is False


@pytest.mark.parametrize("queue_state", ["QUEUED", "AWAITING_CHECKS"])
def test_active_merge_queue_wait_uses_base_cadence_when_checks_are_green(queue_state):
    args = argparse.Namespace(poll_seconds=30)
    snapshot = {
        "checks": sample_checks(),
        "actions": ["idle"],
        "pr": {"repo": "openai/codex", "number": 123, "head_sha": "abc123", "merge_queue": {"status": "waiting", "id": "entry-1", "state": queue_state, "head_sha": "abc123"}},
    }

    delay, _ = gh_pr_watch.next_watch_poll_seconds(
        args, snapshot, gh_pr_watch.snapshot_change_key(snapshot), 600, 3600
    )
    assert delay == 30


def test_unreadable_pending_queue_head_stays_on_base_cadence():
    args = argparse.Namespace(poll_seconds=30)
    snapshot = {
        "checks": sample_checks(),
        "actions": ["idle"],
        "pr": {"repo": "openai/codex", "number": 123, "head_sha": "abc123", "merge_queue": {"status": "waiting", "id": "entry-1", "state": "QUEUED", "head_sha": "", "read_state": "unreadable"}},
    }

    delay, _ = gh_pr_watch.next_watch_poll_seconds(
        args, snapshot, gh_pr_watch.snapshot_change_key(snapshot), 600, 3600
    )
    assert delay == 30


def test_queue_identity_change_resets_cadence():
    args = argparse.Namespace(poll_seconds=30)
    old = {
        "checks": sample_checks(),
        "actions": ["idle"],
        "pr": {"repo": "openai/codex", "number": 123, "head_sha": "abc123", "merge_queue": {"status": "waiting", "id": "entry-1", "state": "QUEUED"}},
    }
    current = {
        **old,
        "pr": {"repo": "openai/codex", "number": 123, "head_sha": "abc123", "merge_queue": {"status": "waiting", "id": "entry-2", "state": "QUEUED"}},
    }

    delay, _ = gh_pr_watch.next_watch_poll_seconds(
        args, current, gh_pr_watch.snapshot_change_key(old), 600, 3600
    )
    assert delay == 30


@pytest.mark.parametrize("state", sorted(gh_pr_watch.MERGE_QUEUE_WAITING_STATES))
def test_active_queue_explains_blocked_merge_state(state):
    pr = sample_pr()
    pr.update(merge_state_status="BLOCKED", merge_queue={"status": "waiting", "state": state, "id": "entry-1"})
    assert gh_pr_watch.recommend_actions(pr, sample_checks(), [], [], [], {}, 0, 3) == ["idle"]


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
        lambda args, cache=None: (snapshot, Path("/tmp/codex-babysit-pr-state.json")),
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
        "watch_decision": {"head_sha": "abc123", "decision": "action_required"},
    }

    compact = gh_pr_watch.compact_wait_snapshot(snapshot)

    assert "large_unneeded_field" not in compact["pr"]
    assert "large_unneeded_field" not in compact["actionable_review_items"][0]
    assert len(compact["actionable_review_items"][0]["body"]) == 1000
    assert compact["watch_decision"]["head_sha"] == "abc123"


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
        "watch_decision": {"head_sha": "abc123", "decision": "action_required"},
    }
    snapshots = iter(
        [
            (idle_snapshot, tmp_path / "state.json"),
            (action_snapshot, tmp_path / "state.json"),
        ]
    )
    monkeypatch.setattr(gh_pr_watch, "collect_snapshot", lambda args, cache=None: next(snapshots))
    schedules = []
    monkeypatch.setattr(
        gh_pr_watch,
        "persist_watch_schedule",
        lambda _path, _snapshot, mode, delay: schedules.append((mode, delay)),
    )
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
    assert receipt["snapshot"]["watch_decision"]["head_sha"] == "abc123"
    assert schedules[-1] == ("watch-until-action", 0)


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
    monkeypatch.setattr(gh_pr_watch, "collect_snapshot", lambda args, cache=None: next(snapshots))
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


def test_retry_rejects_stale_or_replaced_run_before_mutation(monkeypatch):
    install_retry_snapshot(monkeypatch, retry_snapshot(99))
    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *_args, **_kwargs: sample_pr())
    monkeypatch.setattr(
        gh_pr_watch,
        "get_workflow_run",
        lambda *_args: workflow_run(id=100),
    )
    mutation_calls = []
    monkeypatch.setattr(
        gh_pr_watch,
        "gh_text",
        lambda *args, **kwargs: mutation_calls.append((args, kwargs)),
    )

    result = gh_pr_watch.retry_failed_now(retry_args(99))

    assert result["reason"] == "run_id_mismatch"
    assert result["rerun_attempted"] is False
    assert mutation_calls == []


@pytest.mark.parametrize(
    ("run_overrides", "expected_reason"),
    [
        ({"status": "in_progress", "conclusion": ""}, "run_not_terminal"),
        ({"status": "completed", "conclusion": "startup_failure"}, "run_startup_blocked"),
    ],
)
def test_retry_rejects_pending_or_startup_blocked_run(
    monkeypatch, run_overrides, expected_reason
):
    install_retry_snapshot(monkeypatch, retry_snapshot(99))
    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *_args, **_kwargs: sample_pr())
    run = workflow_run()
    run.update(run_overrides)
    monkeypatch.setattr(gh_pr_watch, "get_workflow_run", lambda *_args: run)
    mutation_calls = []
    monkeypatch.setattr(
        gh_pr_watch,
        "gh_text",
        lambda *args, **kwargs: mutation_calls.append((args, kwargs)),
    )

    result = gh_pr_watch.retry_failed_now(retry_args(99))

    assert result["reason"] == expected_reason
    assert result["rerun_attempted"] is False
    assert mutation_calls == []


def test_retry_performs_exactly_one_rerun_and_reports_new_attempt_identity(monkeypatch):
    install_retry_snapshot(monkeypatch, retry_snapshot(99))
    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *_args, **_kwargs: sample_pr())
    run_reads = iter(
        [
            workflow_run(),
            workflow_run(status="in_progress", conclusion=None, run_attempt=2),
        ]
    )
    monkeypatch.setattr(gh_pr_watch, "get_workflow_run", lambda *_args: next(run_reads))
    mutation_calls = []
    monkeypatch.setattr(
        gh_pr_watch,
        "gh_text",
        lambda *args, **kwargs: mutation_calls.append((args, kwargs)) or "",
    )

    result = gh_pr_watch.retry_failed_now(retry_args(99))

    assert result["reason"] == "rerun_triggered"
    assert result["rerun_attempted"] is True
    assert result["rerun_count"] == 1
    assert result["rerun_run_ids"] == [99]
    assert len(mutation_calls) == 1
    assert mutation_calls[0][0] == (["run", "rerun", "99", "--failed"],)
    assert result["attempts"][0]["new_attempt"] is True
    assert result["attempts"][0]["attempt_identity"] == "99:2"
    assert result["action_fingerprint"]["binding"]["selected_failed_run_ids"] == ["99"]


def test_retry_does_not_accept_stale_post_rerun_readback(monkeypatch):
    install_retry_snapshot(monkeypatch, retry_snapshot(99))
    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *_args, **_kwargs: sample_pr())
    monkeypatch.setattr(
        gh_pr_watch,
        "get_workflow_run",
        lambda *_args: workflow_run(run_attempt=1),
    )
    mutation_calls = []
    monkeypatch.setattr(
        gh_pr_watch,
        "gh_text",
        lambda *args, **kwargs: mutation_calls.append((args, kwargs)) or "",
    )

    result = gh_pr_watch.retry_failed_now(retry_args(99))

    assert result["reason"] == "post_rerun_readback_inconclusive"
    assert result["rerun_attempted"] is True
    assert result["rerun_count"] == 1
    assert len(mutation_calls) == 1
    assert result["attempts"][0]["attempt_identity_observable"] is True
    assert result["attempts"][0]["new_attempt"] is False


@pytest.mark.parametrize("installation_observer", [False, True])
def test_collect_snapshot_reviews_first_and_rediscovers_exact_head(monkeypatch, tmp_path, installation_observer):
    events = []
    heads = iter(["abc123", "def456"])
    monkeypatch.setattr(gh_pr_watch, "resolve_pr", lambda *a, **k: sample_pr(head_sha=next(heads)))
    monkeypatch.setattr(gh_pr_watch, "detect_local_git_context", lambda: {})
    monkeypatch.setattr(gh_pr_watch.tempfile, "gettempdir", lambda: str(tmp_path))
    monkeypatch.setattr(gh_pr_watch, "get_authenticated_login", lambda: events.append("login") or "observer")

    def reviews(pr, state, **kwargs):
        events.append("reviews")
        assert kwargs["authenticated_login"] == (None if installation_observer else "observer")
        assert kwargs["include_review_items"] is True
        return [], []

    monkeypatch.setattr(gh_pr_watch, "fetch_new_review_items", reviews)
    monkeypatch.setattr(gh_pr_watch, "get_review_threads", lambda pr: [])
    monkeypatch.setattr(gh_pr_watch, "get_pr_checks", lambda *a, **k: events.append("checks") or [{"bucket": "pass", "state": "SUCCESS"}])
    monkeypatch.setattr(gh_pr_watch, "get_workflow_runs_for_sha", lambda repo, head: events.append(head) or [])
    args = argparse.Namespace(pr="123", repo="openai/codex", state_file="state.json",
        ignore_review_thread=[], max_flaky_retries=3, reset_seen_feedback=False,
        installation_observer=installation_observer)
    for expected_head in ["abc123", "def456"]:
        events.clear()
        snapshot, state_path = gh_pr_watch.collect_snapshot(args)
        assert snapshot["pr"]["head_sha"] == expected_head
        assert snapshot["actions"] == ["stop_ready_to_merge"]
        assert events.index("reviews") < events.index("checks") < events.index(expected_head)
        assert ("login" in events) is not installation_observer
        assert state_path == tmp_path / "state.json"
