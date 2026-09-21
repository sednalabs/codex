#!/usr/bin/env python3
"""Wait for a named GitHub Actions job step to reach a target status."""

import argparse
import json
import sys
import time

from gh_workflow_run_watch import (
    GhCommandError,
    build_targets,
    detect_ref,
    detect_repo,
    list_workflow_runs,
    view_run,
)

TARGET_KIND_RUN_ID = "run_id"
TARGET_KIND_WORKFLOW = "workflow"
PENDING_STATUSES = {"queued", "in_progress", "pending", "requested", "waiting"}


def parse_args():
    parser = argparse.ArgumentParser(
        description=(
            "Wait until a named GitHub Actions job step reaches a target status "
            "such as pending, in_progress, or completed."
        )
    )
    parser.add_argument("--run-id", type=int, help="Exact GitHub Actions run id.")
    parser.add_argument("--workflow", default=None, help="Workflow name or workflow file.")
    parser.add_argument("--ref", default="auto", help="Branch or SHA to watch (default: current branch).")
    parser.add_argument("--head-sha", default=None, help="Optional exact or prefix head SHA.")
    parser.add_argument("--host-ref", default=None, help="Optional host branch for workflow_dispatch runs.")
    parser.add_argument("--target", action="append", default=[], help="Optional watcher-style target spec.")
    parser.add_argument("--repo", help="Optional OWNER/REPO override.")
    parser.add_argument("--poll-seconds", type=int, default=20, help="Poll interval while waiting.")
    parser.add_argument(
        "--appearance-timeout-seconds",
        type=int,
        default=300,
        help="How long to wait for a workflow/ref target to appear before returning timeout state.",
    )
    parser.add_argument("--min-run-id", type=int, default=None, help="Ignore matching runs below this id.")
    parser.add_argument("--job-name", default=None, help="Optional exact job name to disambiguate the step.")
    parser.add_argument("--step-name", required=True, help="Exact step name to watch.")
    parser.add_argument(
        "--step-status",
        choices=("pending", "in_progress", "completed"),
        default="in_progress",
        help="Target step status to wait for (default: in_progress).",
    )
    parser.add_argument("--once", action="store_true", help="Emit one snapshot and exit.")
    args = parser.parse_args()

    if args.poll_seconds <= 0:
        parser.error("--poll-seconds must be > 0")
    if args.appearance_timeout_seconds < 0:
        parser.error("--appearance-timeout-seconds must be >= 0")
    if args.min_run_id is not None and args.min_run_id <= 0:
        parser.error("--min-run-id must be > 0")
    if args.target and (args.run_id is not None or args.workflow is not None):
        parser.error("Use either --target or the direct --run-id/--workflow flags, not both.")
    return args


def emit(payload):
    sys.stdout.write(json.dumps(payload, sort_keys=True) + "\n")
    sys.stdout.flush()


def build_single_target(args):
    targets = build_targets(args)
    if len(targets) != 1:
        raise GhCommandError("gh_wait_for_run_step currently supports exactly one target.")
    return targets[0]


def compact_run(run_view):
    return {
        "id": run_view.get("databaseId"),
        "number": run_view.get("number"),
        "workflow_name": str(run_view.get("workflowName") or ""),
        "url": str(run_view.get("url") or ""),
        "head_branch": str(run_view.get("headBranch") or ""),
        "head_sha": str(run_view.get("headSha") or ""),
        "status": str(run_view.get("status") or ""),
        "conclusion": str(run_view.get("conclusion") or ""),
    }


def matched_step_candidates(run_view, *, job_name, step_name):
    matches = []
    for job in run_view.get("jobs") or []:
        if not isinstance(job, dict):
            continue
        current_job_name = str(job.get("name") or "")
        if job_name and current_job_name != job_name:
            continue
        for step in job.get("steps") or []:
            if not isinstance(step, dict):
                continue
            current_step_name = str(step.get("name") or "")
            if current_step_name != step_name:
                continue
            matches.append(
                {
                    "job_id": int(job.get("databaseId") or 0),
                    "job_name": current_job_name,
                    "job_status": str(job.get("status") or ""),
                    "job_conclusion": str(job.get("conclusion") or ""),
                    "step_name": current_step_name,
                    "step_status": str(step.get("status") or ""),
                    "step_conclusion": str(step.get("conclusion") or ""),
                    "step_number": step.get("number"),
                }
            )
    return matches


def resolve_run_for_target(repo, target):
    if target["kind"] == TARGET_KIND_RUN_ID:
        return view_run(repo, target["run_id"]), None

    ref = detect_ref(target["ref"])
    matching_runs = list_workflow_runs(
        repo,
        target["workflow"],
        ref,
        expected_head_sha=target.get("head_sha"),
        minimum_run_id=target.get("min_run_id"),
        host_ref=target.get("host_ref"),
    )
    return (view_run(repo, int(matching_runs[0]["databaseId"])), ref) if matching_runs else (None, ref)


def snapshot_for_target(args, repo, target, *, wait_started_at=None):
    now = int(time.time())
    run_view, resolved_ref = resolve_run_for_target(repo, target)
    if run_view is None:
        elapsed_seconds = 0 if wait_started_at is None else max(0, now - int(wait_started_at))
        timed_out = args.appearance_timeout_seconds > 0 and elapsed_seconds >= args.appearance_timeout_seconds
        actions = ["stop_run_appearance_timeout"] if timed_out else ["idle"]
        return {
            "repo": repo,
            "target": target,
            "resolved_ref": resolved_ref,
            "run": None,
            "step_wait": {
                "job_name": args.job_name,
                "step_name": args.step_name,
                "target_status": args.step_status,
                "matched": False,
                "matches": [],
                "waiting_for_run_match": True,
                "elapsed_seconds": elapsed_seconds,
                "timeout_seconds": args.appearance_timeout_seconds,
                "timed_out": timed_out,
            },
            "actions": actions,
            "ts": now,
        }

    run_payload = compact_run(run_view)
    matches = matched_step_candidates(run_view, job_name=args.job_name, step_name=args.step_name)
    reached = any(match.get("step_status") == args.step_status for match in matches)
    run_status = str(run_payload.get("status") or "")
    run_conclusion = str(run_payload.get("conclusion") or "")

    if reached:
        actions = ["stop_step_status_reached"]
    elif run_status == "completed":
        actions = ["stop_run_terminal"]
    else:
        actions = ["idle"]

    return {
        "repo": repo,
        "target": target,
        "resolved_ref": resolved_ref,
        "run": run_payload,
        "step_wait": {
            "job_name": args.job_name,
            "step_name": args.step_name,
            "target_status": args.step_status,
            "matched": bool(matches),
            "matches": matches,
            "waiting_for_run_match": False,
            "run_status": run_status,
            "run_conclusion": run_conclusion,
        },
        "actions": actions,
        "ts": now,
    }


def main():
    args = parse_args()
    repo = args.repo or detect_repo()
    if not repo:
      raise GhCommandError(
          "Unable to determine OWNER/REPO from GH_REPO, git remotes, or `gh repo view`; pass --repo explicitly."
      )

    target = build_single_target(args)
    wait_started_at = int(time.time())
    if args.once:
        emit(snapshot_for_target(args, repo, target, wait_started_at=wait_started_at))
        return

    while True:
        payload = snapshot_for_target(args, repo, target, wait_started_at=wait_started_at)
        if payload.get("actions") != ["idle"]:
            emit(payload)
            return
        time.sleep(args.poll_seconds)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        emit({"actions": ["stop_operator_interrupted"], "ts": int(time.time())})
        sys.exit(130)
    except GhCommandError as err:
        emit({"error": str(err), "actions": ["stop_operator_help_required"], "ts": int(time.time())})
        sys.exit(2)
