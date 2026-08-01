#!/usr/bin/env python3
"""Prove selected workflow delivery for one merge-queue PR.

This script performs only a bounded, receipt-owned PR-delivery observation.
The long workflow waits remain delegated to gh_workflow_run_watch for the exact
merge-group and selected post-merge runs. It does not claim that every
post-merge workflow, or repository health overall, has passed.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

FULL_SHA_RE = re.compile(r"^[0-9a-fA-F]{40}$")
REPOSITORY_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*/(?!\.{1,2}$)[A-Za-z0-9._-]+$")
QUEUE_PR_COMPONENT_RE = re.compile(r"(?:^|/)pr-(\d+)(?:-|$)")
WATCHER_LAUNCHER = Path(__file__).with_name("gh_workflow_run_watch")

PULL_REQUEST_QUERY = """
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number
      headRefOid
      baseRefName
      baseRefOid
      merged
      mergeQueueEntry { id }
      mergeCommit { oid }
    }
  }
}
"""


class GhCommandError(RuntimeError):
    """Raised when an authoritative GitHub read cannot be completed."""


class GhCommandDeadlineExceeded(GhCommandError):
    """Raised when one bounded observation read exhausts its deadline."""


class DeliveryStop(RuntimeError):
    """A fail-closed delivery-proof stop with a stable receipt action."""

    def __init__(self, action, message):
        super().__init__(message)
        self.action = action


class ArgumentParseError(ValueError):
    """An invalid command-line invocation that still receives a receipt."""

    def __init__(self, message, parsed_args=None):
        super().__init__(message)
        self.parsed_args = parsed_args


class ReceiptArgumentParser(argparse.ArgumentParser):
    """Keep invalid invocations on the compact JSON receipt path."""

    def error(self, message):
        raise ArgumentParseError(message)


def is_full_sha(value):
    return bool(FULL_SHA_RE.fullmatch(str(value or "").strip()))


def normalize_workflow_name(value):
    normalized = str(value or "").strip().lower().replace("\\", "/")
    normalized = normalized.rsplit("/", 1)[-1]
    normalized = normalized.removesuffix(".yaml")
    normalized = normalized.removesuffix(".yml")
    return re.sub(r"[^a-z0-9]+", "-", normalized).strip("-")


def workflow_matches(observed, expected):
    return normalize_workflow_name(observed) == normalize_workflow_name(expected)


def queue_ref_mentions_pr(queue_ref, pr_number):
    return any(
        int(match.group(1)) == int(pr_number)
        for match in QUEUE_PR_COMPONENT_RE.finditer(str(queue_ref or ""))
    )


def compact_run(run):
    compact = {
        "id": run.get("id"),
        "attempt": run.get("attempt"),
        "workflow": run.get("workflow"),
        "url": run.get("url"),
        "event": run.get("event"),
        "head_branch": run.get("head_branch"),
        "head_sha": run.get("head_sha"),
        "status": run.get("status"),
        "conclusion": run.get("conclusion"),
    }
    return {key: value for key, value in compact.items() if value not in (None, "", [])}


def compact_failed_job(failed_jobs):
    if not isinstance(failed_jobs, list) or not failed_jobs:
        return None
    first = failed_jobs[0]
    if not isinstance(first, dict):
        return None
    compact = {
        "id": first.get("id"),
        "name": first.get("name"),
        "conclusion": first.get("conclusion"),
    }
    return {
        key: value for key, value in compact.items() if value not in (None, "", [])
    } or None


def subprocess_kwargs(*, env=None, timeout_seconds=None):
    kwargs = {
        "capture_output": True,
        "text": True,
        "env": env,
    }
    if timeout_seconds is not None:
        kwargs["timeout"] = timeout_seconds
    return kwargs


def run_gh_process(args, *, timeout_seconds=None):
    try:
        return subprocess.run(
            ["gh", *args],
            check=False,
            shell=False,
            **subprocess_kwargs(timeout_seconds=timeout_seconds),
        )
    except subprocess.TimeoutExpired as err:
        raise GhCommandDeadlineExceeded(
            "The GitHub CLI exceeded the delivery observation deadline."
        ) from err
    except OSError as err:
        raise GhCommandError("Unable to execute the GitHub CLI.") from err


def run_watcher_process(args, *, env=None):
    try:
        return subprocess.run(
            [str(WATCHER_LAUNCHER), *args],
            check=False,
            shell=False,
            **subprocess_kwargs(env=env),
        )
    except OSError as err:
        raise GhCommandError(
            "Unable to execute the blocking workflow watcher."
        ) from err


def gh_json(args, *, timeout_seconds=None):
    if timeout_seconds is None:
        result = run_gh_process(args)
    else:
        result = run_gh_process(args, timeout_seconds=timeout_seconds)
    if result.returncode != 0:
        raise GhCommandError(
            f"GitHub CLI command failed with exit status {result.returncode}."
        )
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as err:
        raise GhCommandError("GitHub CLI returned invalid JSON.") from err


def detect_repo():
    configured = os.environ.get("GH_PR_DELIVERY_WATCH_REPO") or os.environ.get(
        "GH_REPO"
    )
    if configured:
        return validate_repo(configured)
    result = run_gh_process(
        ["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"],
    )
    if result.returncode == 0 and result.stdout.strip():
        return validate_repo(result.stdout.strip())
    raise GhCommandError(
        "Unable to determine OWNER/REPO. Pass --repo or set GH_PR_DELIVERY_WATCH_REPO."
    )


def split_repo(repo):
    validated_repo = validate_repo(repo)
    return validated_repo.split("/", 1)


def validate_repo(repo):
    normalized = str(repo or "").strip()
    if not REPOSITORY_RE.fullmatch(normalized):
        raise GhCommandError(
            "Repository must use OWNER/REPO form with only letters, digits, dots, underscores, and hyphens."
        )
    return normalized


def fetch_pr(repo, pr_number, *, timeout_seconds=None):
    owner, name = split_repo(repo)
    payload = gh_json(
        [
            "api",
            "graphql",
            "-f",
            f"query={PULL_REQUEST_QUERY}",
            "-F",
            f"owner={owner}",
            "-F",
            f"name={name}",
            "-F",
            f"number={int(pr_number)}",
        ],
        timeout_seconds=timeout_seconds,
    )
    data = payload.get("data") if isinstance(payload, dict) else None
    repository = data.get("repository") if isinstance(data, dict) else None
    pr = repository.get("pullRequest") if isinstance(repository, dict) else None
    if not isinstance(pr, dict):
        raise GhCommandError(f"Pull request #{pr_number} was not found in {repo}.")
    return {
        "number": pr.get("number"),
        "head_sha": str(pr.get("headRefOid") or ""),
        "base_ref": str(pr.get("baseRefName") or ""),
        "base_sha": str(pr.get("baseRefOid") or ""),
        "merged": bool(pr.get("merged")),
        "merge_queue_entry_id": ((pr.get("mergeQueueEntry") or {}).get("id")),
        "merge_commit_sha": str(((pr.get("mergeCommit") or {}).get("oid")) or ""),
    }


def fetch_actions_run(repo, run_id, *, timeout_seconds=None):
    payload = gh_json(
        ["api", f"repos/{repo}/actions/runs/{int(run_id)}"],
        timeout_seconds=timeout_seconds,
    )
    if not isinstance(payload, dict):
        raise GhCommandError(f"Actions run {run_id} returned an unexpected payload.")
    return normalize_actions_run(payload)


def list_merge_group_runs(repo, *, timeout_seconds=None):
    payload = gh_json(
        ["api", f"repos/{repo}/actions/runs?event=merge_group&per_page=100"],
        timeout_seconds=timeout_seconds,
    )
    runs = payload.get("workflow_runs") if isinstance(payload, dict) else None
    if not isinstance(runs, list):
        raise GhCommandError(
            "Merge-group Actions listing returned an unexpected payload."
        )
    return [normalize_actions_run(run) for run in runs if isinstance(run, dict)]


def normalize_actions_run(run):
    return {
        "id": int(run.get("id") or run.get("databaseId") or 0),
        "attempt": run.get("run_attempt") or run.get("attempt"),
        "workflow": str(run.get("name") or run.get("workflowName") or ""),
        "url": str(run.get("html_url") or run.get("url") or ""),
        "event": str(run.get("event") or ""),
        "head_branch": str(run.get("head_branch") or run.get("headBranch") or ""),
        "head_sha": str(run.get("head_sha") or run.get("headSha") or ""),
        "status": str(run.get("status") or ""),
        "conclusion": str(run.get("conclusion") or ""),
    }


def verify_merge_group_candidate(run, args):
    if run.get("id", 0) <= 0:
        raise DeliveryStop(
            "stop_merge_group_candidate_uncorrelatable",
            "Merge-group run has no exact run id.",
        )
    if run.get("event") != "merge_group":
        raise DeliveryStop(
            "stop_merge_group_candidate_uncorrelatable",
            f"Actions run {run['id']} is not a merge_group run.",
        )
    if not workflow_matches(run.get("workflow"), args.merge_group_workflow):
        raise DeliveryStop(
            "stop_merge_group_candidate_uncorrelatable",
            f"Actions run {run['id']} belongs to workflow '{run.get('workflow')}', not "
            f"'{args.merge_group_workflow}'.",
        )
    if not queue_ref_mentions_pr(run.get("head_branch"), args.pr):
        raise DeliveryStop(
            "stop_merge_group_candidate_uncorrelatable",
            f"Merge-group queue ref '{run.get('head_branch')}' does not identify PR #{args.pr}.",
        )
    if not is_full_sha(run.get("head_sha")):
        raise DeliveryStop(
            "stop_merge_group_candidate_uncorrelatable",
            f"Merge-group run {run['id']} has no full candidate SHA.",
        )
    return run


def resolve_merge_group_candidate(repo, initial_pr, args):
    if args.merge_group_run_id is not None:
        return verify_merge_group_candidate(
            fetch_actions_run(repo, args.merge_group_run_id), args
        )

    if not initial_pr["merged"] and not initial_pr["merge_queue_entry_id"]:
        raise DeliveryStop(
            "stop_merge_group_candidate_absent",
            f"PR #{args.pr} has no merge-queue entry and no exact merge-group run was supplied.",
        )

    matching = [
        run
        for run in list_merge_group_runs(repo)
        if workflow_matches(run.get("workflow"), args.merge_group_workflow)
        and queue_ref_mentions_pr(run.get("head_branch"), args.pr)
    ]
    if not matching:
        raise DeliveryStop(
            "stop_merge_group_candidate_absent",
            f"No {args.merge_group_workflow} merge-group candidate identifies PR #{args.pr}.",
        )

    valid = [verify_merge_group_candidate(run, args) for run in matching]
    candidate_shas = {str(run["head_sha"]).lower() for run in valid}
    if len(candidate_shas) != 1:
        candidate_ids = sorted(int(run["id"]) for run in valid)
        raise DeliveryStop(
            "stop_merge_group_candidate_ambiguous",
            f"PR #{args.pr} has multiple merge-group candidate SHAs across runs {candidate_ids}.",
        )
    return max(valid, key=lambda run: int(run["id"]))


def parse_watcher_payload(stdout):
    for line in reversed(stdout.splitlines()):
        if not line.strip():
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(payload, dict):
            return payload
    raise GhCommandError("The blocking workflow watcher did not emit a JSON receipt.")


def run_blocking_watcher(repo, target, args):
    watcher_args = [
        "--repo",
        repo,
        "--target",
        target,
        "--watch-until-terminal",
        "--poll-seconds",
        str(args.poll_seconds),
        "--appearance-timeout-seconds",
        str(args.appearance_timeout_seconds),
        "--retry-settle-seconds",
        str(args.retry_settle_seconds),
    ]
    watcher_env = os.environ.copy()
    selected_python = watcher_env.get("GH_PR_DELIVERY_WATCH_PYTHON")
    if selected_python:
        watcher_env["GH_WORKFLOW_RUN_WATCH_PYTHON"] = selected_python
    result = run_watcher_process(watcher_args, env=watcher_env)
    payload = parse_watcher_payload(result.stdout)
    if result.returncode != 0:
        raise GhCommandError("The blocking workflow watcher exited unsuccessfully.")
    return payload


def watcher_run(payload, stage):
    targets = payload.get("targets") if isinstance(payload, dict) else None
    if not isinstance(targets, list) or len(targets) != 1:
        raise DeliveryStop(
            f"stop_{stage}_watcher_receipt_invalid",
            f"The {stage} watcher did not return exactly one target.",
        )
    run = targets[0].get("run") if isinstance(targets[0], dict) else None
    if not isinstance(run, dict):
        raise DeliveryStop(
            f"stop_{stage}_watcher_receipt_invalid",
            f"The {stage} watcher receipt has no resolved Actions run.",
        )
    return {
        "run": compact_run(
            {
                "id": run.get("id"),
                "attempt": run.get("attempt"),
                "workflow": run.get("workflow_name"),
                "url": run.get("url"),
                "event": run.get("event"),
                "head_branch": run.get("head_branch"),
                "head_sha": run.get("head_sha"),
                "status": run.get("status"),
                "conclusion": run.get("conclusion"),
            }
        ),
        "failed_job": compact_failed_job(targets[0].get("failed_jobs")),
        "actions": list(targets[0].get("actions") or []),
    }


def verify_watched_run(
    watcher_receipt,
    *,
    stage,
    expected_sha,
    expected_run_id=None,
    expected_event,
    expected_branch,
    expected_workflow,
):
    watched = watcher_run(watcher_receipt, stage)
    run = watched["run"]
    if expected_run_id is not None and int(run.get("id") or 0) != int(expected_run_id):
        raise DeliveryStop(
            f"stop_{stage}_run_id_mismatch",
            f"The {stage} watcher returned run {run.get('id')}, not {expected_run_id}.",
        )
    if str(run.get("head_sha") or "").lower() != str(expected_sha).lower():
        raise DeliveryStop(
            f"stop_{stage}_run_sha_mismatch",
            f"The {stage} watcher returned SHA {run.get('head_sha')}, not {expected_sha}.",
        )
    if run.get("head_branch") != expected_branch:
        raise DeliveryStop(
            f"stop_{stage}_run_identity_mismatch",
            f"The {stage} run branch is '{run.get('head_branch')}', not '{expected_branch}'.",
        )
    if not workflow_matches(run.get("workflow"), expected_workflow):
        raise DeliveryStop(
            f"stop_{stage}_run_identity_mismatch",
            f"The {stage} run workflow is '{run.get('workflow')}', not '{expected_workflow}'.",
        )
    observed_event = run.get("event")
    missing_merge_group_event_is_independently_proven = (
        stage == "merge_group"
        and expected_event == "merge_group"
        and observed_event in (None, "")
        and expected_run_id is not None
        and is_full_sha(expected_sha)
        and bool(expected_branch)
        and bool(expected_workflow)
    )
    if (
        observed_event != expected_event
        and not missing_merge_group_event_is_independently_proven
    ):
        raise DeliveryStop(
            f"stop_{stage}_run_identity_mismatch",
            f"The {stage} run event is '{observed_event}', not '{expected_event}'.",
        )
    watched["outcome"] = (
        "success"
        if run.get("status") == "completed" and run.get("conclusion") == "success"
        else "failure"
    )
    return watched


def assert_pr_identity(pr, args):
    if pr.get("base_ref") != args.main_ref:
        raise DeliveryStop(
            "stop_pr_base_ref_mismatch",
            f"PR #{args.pr} targets '{pr.get('base_ref')}', not '{args.main_ref}'.",
        )
    if str(pr.get("head_sha") or "").lower() != args.expected_head_sha.lower():
        raise DeliveryStop(
            "stop_pr_head_changed",
            f"PR #{args.pr} head is {pr.get('head_sha')}, not {args.expected_head_sha}.",
        )


def fetch_ref_sha(repo, ref):
    payload = gh_json(["api", f"repos/{repo}/git/ref/heads/{ref}"])
    sha = (
        str(((payload.get("object") or {}).get("sha")) or "")
        if isinstance(payload, dict)
        else ""
    )
    if not is_full_sha(sha):
        raise GhCommandError(f"Branch '{ref}' did not return a full Git SHA.")
    return sha


def is_commit_ancestor(repo, ancestor_sha, descendant_sha, *, timeout_seconds=None):
    if ancestor_sha.lower() == descendant_sha.lower():
        return True
    payload = gh_json(
        ["api", f"repos/{repo}/compare/{ancestor_sha}...{descendant_sha}"],
        timeout_seconds=timeout_seconds,
    )
    return isinstance(payload, dict) and payload.get("status") in {
        "ahead",
        "identical",
    }


def observation_timeout_stop(args):
    return DeliveryStop(
        "stop_merge_observation_timeout",
        f"PR #{args.pr} did not merge within "
        f"{args.merge_observation_timeout_seconds} seconds after its "
        "successful merge-group run.",
    )


def remaining_observation_seconds(deadline, args):
    remaining_seconds = deadline - time.monotonic()
    if remaining_seconds <= 0:
        raise observation_timeout_stop(args)
    return remaining_seconds


def verify_candidate_association(repo, candidate_sha, pr, args, *, deadline=None):
    expected_base_sha = str(pr.get("base_sha") or "")
    if not is_full_sha(expected_base_sha):
        raise DeliveryStop(
            "stop_merge_group_candidate_uncorrelatable",
            f"PR #{args.pr} did not return a full current base SHA.",
        )
    try:
        includes_expected_head = is_commit_ancestor(
            repo,
            args.expected_head_sha,
            candidate_sha,
            timeout_seconds=(
                remaining_observation_seconds(deadline, args)
                if deadline is not None
                else None
            ),
        )
        includes_current_base = is_commit_ancestor(
            repo,
            expected_base_sha,
            candidate_sha,
            timeout_seconds=(
                remaining_observation_seconds(deadline, args)
                if deadline is not None
                else None
            ),
        )
    except GhCommandDeadlineExceeded as err:
        raise observation_timeout_stop(args) from err
    except GhCommandError as err:
        raise DeliveryStop(
            "stop_merge_group_candidate_uncorrelatable",
            "GitHub could not establish the merge-group candidate commit ancestry.",
        ) from err
    if not includes_expected_head or not includes_current_base:
        raise DeliveryStop(
            "stop_merge_group_candidate_stale",
            f"Merge-group candidate {candidate_sha} does not contain PR #{args.pr}'s "
            "current expected head and base.",
        )
    return {
        "expected_pr_head_sha": args.expected_head_sha,
        "expected_base_sha": expected_base_sha,
        "candidate_contains_expected_head": True,
        "candidate_contains_current_base": True,
    }


def verify_selected_candidate_merged(repo, candidate_sha, merge_commit_sha):
    try:
        candidate_reaches_merge_commit = is_commit_ancestor(
            repo, candidate_sha, merge_commit_sha
        )
    except GhCommandError as err:
        raise DeliveryStop(
            "stop_merge_group_candidate_uncorrelatable",
            "GitHub could not correlate the selected merge-group candidate to the merge commit.",
        ) from err
    if not candidate_reaches_merge_commit:
        raise DeliveryStop(
            "stop_merge_group_candidate_not_merged",
            f"Selected merge-group candidate {candidate_sha} is not an ancestor of "
            f"merge commit {merge_commit_sha}.",
        )
    return {
        "candidate_sha": candidate_sha,
        "merge_commit_sha": merge_commit_sha,
        "candidate_reaches_merge_commit": True,
    }


def reassert_selected_candidate_identity(repo, candidate, args, *, deadline):
    observed = verify_merge_group_candidate(
        fetch_actions_run(
            repo,
            candidate["id"],
            timeout_seconds=remaining_observation_seconds(deadline, args),
        ),
        args,
    )
    if (
        str(observed["head_sha"]).lower() != str(candidate["head_sha"]).lower()
        or observed["head_branch"] != candidate["head_branch"]
    ):
        raise DeliveryStop(
            "stop_merge_group_candidate_changed",
            f"Selected merge-group run {candidate['id']} no longer identifies the "
            "same candidate SHA and queue ref.",
        )
    return observed


def reassert_candidate_not_superseded(repo, candidate, args, *, deadline):
    later_candidate_shas = {
        str(run["head_sha"]).lower()
        for run in (
            verify_merge_group_candidate(run, args)
            for run in list_merge_group_runs(
                repo,
                timeout_seconds=remaining_observation_seconds(deadline, args),
            )
            if workflow_matches(run.get("workflow"), args.merge_group_workflow)
            and queue_ref_mentions_pr(run.get("head_branch"), args.pr)
        )
        if int(run["id"]) > int(candidate["id"])
    }
    if later_candidate_shas - {str(candidate["head_sha"]).lower()}:
        raise DeliveryStop(
            "stop_merge_group_candidate_superseded",
            f"A later merge-group candidate superseded selected run {candidate['id']}.",
        )


def wait_for_pr_delivery(repo, candidate, initial_pr, args):
    latest_association = None
    try:
        deadline = time.monotonic() + args.merge_observation_timeout_seconds
        while True:
            observed_pr = fetch_pr(
                repo,
                args.pr,
                timeout_seconds=remaining_observation_seconds(deadline, args),
            )
            assert_pr_identity(observed_pr, args)
            reassert_selected_candidate_identity(
                repo, candidate, args, deadline=deadline
            )
            association_pr = initial_pr if observed_pr["merged"] else observed_pr
            latest_association = verify_candidate_association(
                repo,
                candidate["head_sha"],
                association_pr,
                args,
                deadline=deadline,
            )
            reassert_candidate_not_superseded(repo, candidate, args, deadline=deadline)
            if observed_pr["merged"]:
                return observed_pr, latest_association
            if not observed_pr["merge_queue_entry_id"]:
                raise DeliveryStop(
                    "stop_merge_queue_entry_disappeared_without_merge",
                    f"PR #{args.pr} left the merge queue without a merge commit.",
                )
            time.sleep(
                min(args.poll_seconds, remaining_observation_seconds(deadline, args))
            )
    except GhCommandDeadlineExceeded as err:
        raise observation_timeout_stop(args) from err
    except KeyboardInterrupt as err:
        raise DeliveryStop(
            "stop_merge_observation_interrupted",
            "PR delivery observation was interrupted before merge completion.",
        ) from err


def is_commit_reachable_from_main(repo, merge_commit_sha, main_head_sha):
    return is_commit_ancestor(repo, merge_commit_sha, main_head_sha)


def new_receipt(repo, args):
    return {
        "repo": repo,
        "pr": {
            "number": args.pr,
            "expected_head_sha": args.expected_head_sha,
            "main_ref": args.main_ref,
        },
        "merge_group": None,
        "merge_commit": None,
        "post_merge": None,
        "proof_scope": {
            "kind": "selected_workflow_delivery",
            "merge_group_workflow": args.merge_group_workflow,
            "selected_post_merge_workflows": [args.post_merge_workflow],
            "whole_repository_health_proven": False,
        },
        "actions": [],
        "ts": int(time.time()),
    }


def new_argument_error_receipt(error):
    args = error.parsed_args or argparse.Namespace(
        repo=None,
        pr=None,
        expected_head_sha="",
        main_ref="main",
        merge_group_workflow="blocking-ci",
        post_merge_workflow="postmerge-ci",
    )
    receipt = new_receipt(args.repo or "unknown", args)
    receipt["actions"] = ["stop_invalid_arguments"]
    receipt["error"] = str(error)
    return receipt


def execute_delivery(args):
    receipt = new_receipt(args.repo or "unknown", args)
    try:
        repo = validate_repo(args.repo) if args.repo else detect_repo()
        receipt["repo"] = repo
        initial_pr = fetch_pr(repo, args.pr)
        assert_pr_identity(initial_pr, args)
        receipt["pr"].update(
            {
                "observed_head_sha": initial_pr["head_sha"],
                "merge_queue_entry_id": initial_pr["merge_queue_entry_id"],
                "observed_base_sha": initial_pr["base_sha"],
            }
        )

        candidate = resolve_merge_group_candidate(repo, initial_pr, args)
        candidate_sha = candidate["head_sha"]
        receipt["merge_group"] = {
            "candidate_sha": candidate_sha,
            "queue_ref": candidate["head_branch"],
            "source": "exact_run_id"
            if args.merge_group_run_id is not None
            else "unique_discovery",
        }
        receipt["merge_group"]["association"] = verify_candidate_association(
            repo, candidate_sha, initial_pr, args
        )
        candidate_payload = run_blocking_watcher(
            repo,
            f"run-id={candidate['id']},head-sha={candidate_sha}",
            args,
        )
        candidate_result = verify_watched_run(
            candidate_payload,
            stage="merge_group",
            expected_sha=candidate_sha,
            expected_run_id=candidate["id"],
            expected_event="merge_group",
            expected_branch=candidate["head_branch"],
            expected_workflow=args.merge_group_workflow,
        )
        receipt["merge_group"].update(candidate_result)
        if candidate_result["outcome"] != "success":
            raise DeliveryStop(
                "stop_merge_group_run_not_succeeded",
                f"Merge-group run {candidate['id']} did not complete successfully.",
            )

        delivered_pr, latest_association = wait_for_pr_delivery(
            repo, candidate, initial_pr, args
        )
        assert_pr_identity(delivered_pr, args)
        if latest_association is not None:
            receipt["merge_group"]["association"] = latest_association
        receipt["pr"]["post_merge_observed_head_sha"] = delivered_pr["head_sha"]
        if not delivered_pr["merged"]:
            raise DeliveryStop(
                "stop_merge_not_observed",
                f"PR #{args.pr} remains queued after the delivery observation.",
            )
        merge_commit_sha = delivered_pr["merge_commit_sha"]
        if not is_full_sha(merge_commit_sha):
            raise DeliveryStop(
                "stop_merge_commit_uncorrelatable",
                f"PR #{args.pr} is merged but GitHub did not return an exact merge commit SHA.",
            )
        main_head_sha = fetch_ref_sha(repo, args.main_ref)
        if not is_commit_reachable_from_main(repo, merge_commit_sha, main_head_sha):
            raise DeliveryStop(
                "stop_merge_commit_uncorrelatable",
                f"PR #{args.pr} merge commit {merge_commit_sha} is not reachable from {args.main_ref}.",
            )
        receipt["merge_commit"] = {
            "sha": merge_commit_sha,
            "main_ref": args.main_ref,
            "observed_main_head_sha": main_head_sha,
            "reachable_from_main": True,
        }
        receipt["merge_group"]["merge_correlation"] = {
            "candidate_sha": candidate_sha,
            "merge_commit_sha": merge_commit_sha,
        }
        receipt["merge_group"]["merge_correlation"].update(
            verify_selected_candidate_merged(repo, candidate_sha, merge_commit_sha)
        )

        post_merge_payload = run_blocking_watcher(
            repo,
            f"workflow={args.post_merge_workflow},ref={args.main_ref},head-sha={merge_commit_sha}",
            args,
        )
        post_merge_result = verify_watched_run(
            post_merge_payload,
            stage="post_merge",
            expected_sha=merge_commit_sha,
            expected_event="push",
            expected_branch=args.main_ref,
            expected_workflow=args.post_merge_workflow,
        )
        post_merge_result["selected_workflow"] = args.post_merge_workflow
        receipt["post_merge"] = post_merge_result
        if post_merge_result["outcome"] != "success":
            raise DeliveryStop(
                "stop_post_merge_run_not_succeeded",
                f"Post-merge run {post_merge_result['run'].get('id')} did not complete successfully.",
            )
        final_pr = fetch_pr(repo, args.pr)
        assert_pr_identity(final_pr, args)
        receipt["pr"]["final_observed_head_sha"] = final_pr["head_sha"]
        receipt["actions"] = ["stop_pr_delivery_proven"]
        return receipt, 0
    except DeliveryStop as error:
        receipt["actions"] = [error.action]
        receipt["error"] = str(error)
        return receipt, 1
    except GhCommandError as error:
        receipt["actions"] = ["stop_operator_help_required"]
        receipt["error"] = str(error)
        return receipt, 1
    except (AttributeError, KeyError, TypeError, ValueError):
        receipt["actions"] = ["stop_operator_help_required"]
        receipt["error"] = "Unexpected error while building the delivery receipt."
        return receipt, 1


def parse_args(argv=None):
    parser = ReceiptArgumentParser(
        description="Prove one PR delivery across its exact head, merge-group candidate, and main commit."
    )
    parser.add_argument("--pr", type=int, required=True, help="Pull request number.")
    parser.add_argument(
        "--expected-head-sha",
        required=True,
        help="Required full 40-character PR head SHA; prefixes are rejected.",
    )
    parser.add_argument("--repo", help="Optional OWNER/REPO override.")
    parser.add_argument(
        "--main-ref", default="main", help="Protected target branch (default: main)."
    )
    parser.add_argument(
        "--merge-group-run-id",
        type=int,
        help="Exact merge-group Actions run id. Recommended to avoid candidate discovery ambiguity.",
    )
    parser.add_argument(
        "--merge-group-workflow",
        default="blocking-ci",
        help="Merge-group workflow name or file (default: blocking-ci).",
    )
    parser.add_argument(
        "--post-merge-workflow",
        default="postmerge-ci",
        help=(
            "One main-push workflow name or file to prove (default: postmerge-ci); "
            "this does not prove all post-merge workflows or repository health."
        ),
    )
    parser.add_argument(
        "--poll-seconds", type=int, default=60, help="Blocking watcher poll interval."
    )
    parser.add_argument(
        "--appearance-timeout-seconds",
        type=int,
        default=900,
        help="How long the post-merge watcher waits for the exact main run to appear.",
    )
    parser.add_argument(
        "--merge-observation-timeout-seconds",
        type=int,
        default=300,
        help=(
            "Hard deadline after a successful merge-group run for the PR merge "
            "transition and its GitHub reads (default: 300)."
        ),
    )
    parser.add_argument(
        "--retry-settle-seconds",
        type=int,
        default=90,
        help="Retry-settle window forwarded to the blocking workflow watcher.",
    )
    args = parser.parse_args(argv)
    if args.pr <= 0:
        raise ArgumentParseError("--pr must be > 0", args)
    if not is_full_sha(args.expected_head_sha):
        raise ArgumentParseError(
            "--expected-head-sha must be a full 40-character Git SHA", args
        )
    args.expected_head_sha = args.expected_head_sha.lower()
    if args.merge_group_run_id is not None and args.merge_group_run_id <= 0:
        raise ArgumentParseError("--merge-group-run-id must be > 0", args)
    if args.poll_seconds <= 0:
        raise ArgumentParseError("--poll-seconds must be > 0", args)
    if args.appearance_timeout_seconds < 0:
        raise ArgumentParseError("--appearance-timeout-seconds must be >= 0", args)
    if args.merge_observation_timeout_seconds < 0:
        raise ArgumentParseError(
            "--merge-observation-timeout-seconds must be >= 0", args
        )
    if args.retry_settle_seconds < 0:
        raise ArgumentParseError("--retry-settle-seconds must be >= 0", args)
    return args


def emit(receipt):
    sys.stdout.write(json.dumps(receipt, sort_keys=True) + "\n")


def main(argv=None):
    try:
        args = parse_args(argv)
    except ArgumentParseError as error:
        emit(new_argument_error_receipt(error))
        return 1
    receipt, status = execute_delivery(args)
    emit(receipt)
    return status


if __name__ == "__main__":
    sys.exit(main())
