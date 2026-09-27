import importlib.util
from pathlib import Path
from unittest.mock import patch


MODULE_PATH = Path(__file__).resolve().parents[1] / "scripts" / "gh_workflow_run_watch.py"
SPEC = importlib.util.spec_from_file_location("gh_workflow_run_watch_rest", MODULE_PATH)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)

REPO = "owner/repo"
RUN_ID = 123


def run_payload(**overrides):
    payload = {
        "id": RUN_ID,
        "repository": {"full_name": REPO},
        "display_title": "Build",
        "event": "merge_group",
        "head_branch": "main",
        "head_sha": "d9cc9461518666bac336c551f29a47e6796c2125",
        "name": "Build",
        "run_number": 17,
        "status": "completed",
        "conclusion": "success",
        "html_url": f"https://github.com/{REPO}/actions/runs/{RUN_ID}",
        "created_at": "2026-09-13T00:00:00Z",
        "updated_at": "2026-09-13T00:01:00Z",
    }
    payload.update(overrides)
    return payload


def job_payload(job_id=501, run_id=RUN_ID):
    return {
        "id": job_id,
        "run_id": run_id,
        "name": "Build",
        "status": "completed",
        "conclusion": "success",
        "html_url": f"https://github.com/{REPO}/actions/runs/{RUN_ID}/job/{job_id}",
        "started_at": "2026-09-13T00:00:01Z",
        "completed_at": "2026-09-13T00:00:30Z",
        "steps": [{
            "name": "Run checks", "number": 1, "status": "completed",
            "conclusion": "success", "started_at": "2026-09-13T00:00:02Z",
            "completed_at": "2026-09-13T00:00:29Z",
        }],
    }


def test_rest_view_rejects_wrong_run_identity_before_jobs_read():
    with patch.object(
        MODULE,
        "gh_json",
        side_effect=[MODULE.GhCommandError("virtual workflow"), run_payload(id=999)],
    ) as read:
        try:
            MODULE.view_run(REPO, RUN_ID)
        except MODULE.GhCommandError as exc:
            assert "does not match requested run" in str(exc)
        else:
            raise AssertionError("wrong run identity must fail closed")
    assert read.call_count == 2


def test_rest_view_requires_complete_paginated_jobs():
    with patch.object(
        MODULE,
        "gh_json",
        side_effect=[MODULE.GhCommandError("virtual workflow"), run_payload(), {"total_count": 2, "jobs": []}],
    ):
        try:
            MODULE.view_run(REPO, RUN_ID)
        except MODULE.GhCommandError as exc:
            assert "ended at 0 jobs" in str(exc)
        else:
            raise AssertionError("incomplete pagination must fail closed")


def test_rest_view_rejects_job_from_different_run():
    with patch.object(
        MODULE,
        "gh_json",
        side_effect=[MODULE.GhCommandError("virtual workflow"), run_payload(), {"total_count": 1, "jobs": [job_payload(run_id=999)]}],
    ):
        try:
            MODULE.view_run(REPO, RUN_ID)
        except MODULE.GhCommandError as exc:
            assert "belongs to run 999" in str(exc)
        else:
            raise AssertionError("wrong job identity must fail closed")
