#!/usr/bin/env python3
"""Dispatch a workflow and watch the matching run with SHA guardrails."""

import argparse
import importlib.util
import json
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import quote


WATCHER_PYTHON_PATH = Path(__file__).resolve().with_name("gh_workflow_run_watch.py")
WATCHER_LAUNCHER_PATH = Path(__file__).resolve().with_name("gh_workflow_run_watch")


def _load_watcher():
    spec = importlib.util.spec_from_file_location("gh_workflow_run_watch", WATCHER_PYTHON_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError("Unable to load gh_workflow_run_watch.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _is_head_sha_prefix(value):
    if not value:
        return False
    value = str(value).strip()
    if len(value) < 4:
        return False
    return all(ch in "0123456789abcdefABCDEF" for ch in value)


def _head_sha_matches_prefix(observed_sha, expected_sha):
    return bool(str(observed_sha or "").strip()) and str(observed_sha).startswith(
        str(expected_sha).strip()
    )


def parse_args():
    parser = argparse.ArgumentParser(
        description="Dispatch a workflow at an expected head SHA and wait for its run."
    )
    parser.add_argument(
        "--workflow",
        required=True,
        help="Workflow id or workflow file name to dispatch.",
    )
    parser.add_argument(
        "--ref",
        default="auto",
        help=(
            "Branch or ref to dispatch against (default: auto; downstream-style ref inputs "
            "use the repository default branch when auto is left in place)."
        ),
    )
    parser.add_argument(
        "--head-sha",
        default=None,
        help=(
            "Expected head SHA (or 4+ char abbreviation) to wait for before dispatching and to pin "
            "the watch target. "
            "Defaults to local HEAD SHA."
        ),
    )
    parser.add_argument("--repo", default=None, help="Optional OWNER/REPO override.")
    parser.add_argument(
        "--poll-seconds",
        type=int,
        default=10,
        help="Polling interval while waiting for remote ref and matching run readiness.",
    )
    parser.add_argument(
        "--max-wait-seconds",
        type=int,
        default=600,
        help=(
            "Maximum seconds to wait overall for precondition checks and new matching run "
            "discovery."
        ),
    )
    parser.add_argument(
        "--max-retries",
        type=int,
        default=0,
        help=(
            "Optional hard cap on polling retries (0 = unlimited). Applies across both precheck "
            "and matching-run discovery."
        ),
    )
    parser.add_argument(
        "--wait-for",
        choices=("first_action", "all_done"),
        default="first_action",
        help="Watcher completion policy once a matching workflow run is selected.",
    )
    parser.add_argument(
        "--appearance-timeout-seconds",
        type=int,
        default=300,
        help=(
            "Pass-through to watcher --appearance-timeout-seconds. Defaults to 300s so a fresh "
            "dispatch gets the same appearance warm-up window as the watcher."
        ),
    )
    parser.add_argument(
        "--supersession-mode",
        choices=("auto", "compare", "milestone", "retain"),
        default="auto",
        help=(
            "Dispatch retention intent. 'auto' allows same-question reruns to supersede older "
            "runs; retained modes preserve the dispatched run as distinct evidence."
        ),
    )
    parser.add_argument(
        "--supersession-key",
        default="",
        help="Optional retention label for compare/milestone/retain runs.",
    )
    parser.add_argument(
        "--input",
        action="append",
        default=[],
        help=(
            "Workflow input as key=value. Repeat --input for multiple values; each is passed "
            "through as an additional `gh workflow run -f key=value` field."
        ),
    )
    parser.add_argument(
        "--stale-head-retries",
        type=int,
        default=1,
        help=(
            "Additional dispatch attempts when a newly created run on the watched ref resolves "
            "to a head SHA other than --head-sha (default: 1)."
        ),
    )
    parser.add_argument(
        "--min-run-id",
        type=int,
        default=None,
        help="Optional lower bound on watched run ids so older matches are skipped.",
    )
    return parser.parse_args()


def _emit_error(message):
    payload = {
        "actions": ["stop_operator_help_required"],
        "error": str(message),
        "ts": int(time.time()),
    }
    sys.stdout.write(json.dumps(payload, sort_keys=True) + "\n")
    sys.stdout.flush()
    return 1


def _emit_stale_head_timeout(*, workflow, ref, expected_sha, attempts_used, retries_allowed, stale_runs):
    latest = stale_runs[-1] if stale_runs else {}
    payload = {
        "actions": ["stop_stale_head_dispatch_detected"],
        "error": (
            f"Detected newly created run(s) for workflow '{workflow}' on ref '{ref}' with stale "
            f"head SHA while expecting '{expected_sha}'."
        ),
        "stale_head_dispatch": {
            "workflow": workflow,
            "ref": ref,
            "expected_head_sha": expected_sha,
            "attempts_used": int(attempts_used),
            "stale_head_retries_allowed": int(retries_allowed),
            "latest_observed": latest,
            "observed": stale_runs,
        },
        "ts": int(time.time()),
    }
    sys.stdout.write(json.dumps(payload, sort_keys=True) + "\n")
    sys.stdout.flush()
    return 1


def _emit_expected_head_sha_mismatch(*, workflow, ref, expected_sha, observed_sha):
    payload = {
        "actions": ["stop_expected_head_sha_mismatch"],
        "error": (
            f"Expected head SHA '{expected_sha}' for workflow '{workflow}' on ref '{ref}' "
            f"does not match the actual branch head '{observed_sha}'."
        ),
        "expected_head_sha_mismatch": {
            "workflow": workflow,
            "ref": ref,
            "expected_head_sha": expected_sha,
            "observed_head_sha": observed_sha,
        },
        "ts": int(time.time()),
    }
    sys.stdout.write(json.dumps(payload, sort_keys=True) + "\n")
    sys.stdout.flush()
    return 1


def _emit_dispatch_appearance_timeout(
    *,
    workflow,
    ref,
    expected_sha,
    appearance_timeout_seconds,
    attempts_used,
):
    payload = {
        "actions": ["stop_dispatch_run_not_visible"],
        "error": (
            f"Dispatched workflow '{workflow}' on ref '{ref}' but no new run became visible "
            f"within {int(appearance_timeout_seconds)}s for expected head SHA '{expected_sha}'."
        ),
        "dispatch_visibility": {
            "workflow": workflow,
            "ref": ref,
            "expected_head_sha": expected_sha,
            "appearance_timeout_seconds": int(appearance_timeout_seconds),
            "attempts_used": int(attempts_used),
        },
        "ts": int(time.time()),
    }
    sys.stdout.write(json.dumps(payload, sort_keys=True) + "\n")
    sys.stdout.flush()
    return 1


def _budget_exceeded(start_time, attempts, max_wait_seconds, max_retries):
    if max_wait_seconds and max_wait_seconds > 0 and (time.monotonic() - start_time) >= max_wait_seconds:
        return True
    if max_retries and max_retries > 0 and attempts > max_retries:
        return True
    return False


def _effective_minimum_run_id(baseline_max_run_id, requested_min_run_id):
    minimum = int(baseline_max_run_id) + 1
    if requested_min_run_id is None:
        return minimum
    return max(minimum, int(requested_min_run_id))


def _query_remote_ref_sha(watcher, repo, ref):
    endpoint = f"/repos/{repo}/git/ref/heads/{quote(ref, safe='')}"
    payload = watcher.gh_json(["api", endpoint], repo=repo)
    if not isinstance(payload, dict):
        raise RuntimeError(f"Unexpected response when reading remote ref '{ref}'.")
    sha = str(((payload.get("object") or {}).get("sha") or "").strip())
    if not sha:
        raise RuntimeError(f"Remote ref '{ref}' did not include a sha.")
    return sha


def _resolve_remote_ref_sha(watcher, repo, ref):
    try:
        if watcher.is_sha_like(ref):
            return None
        return _query_remote_ref_sha(watcher, repo, ref)
    except Exception:
        return None


def _wait_for_ref_to_match_expected(watcher, repo, ref, expected_sha, *, start_time, attempts, max_wait_seconds, max_retries, poll_seconds):
    while True:
        remote_head_sha = _resolve_remote_ref_sha(watcher, repo, ref)
        if remote_head_sha and _head_sha_matches_prefix(remote_head_sha, expected_sha):
            return True, attempts

        if _budget_exceeded(start_time, attempts, max_wait_seconds, max_retries):
            return False, attempts

        attempts += 1
        time.sleep(max(1, poll_seconds))


def _select_newest_matching_run(
    watcher,
    repo,
    workflow,
    ref,
    expected_head_sha,
    minimum_run_id,
    *,
    start_time,
    attempts,
    max_wait_seconds,
    max_retries,
    poll_seconds,
    dispatch_start_time,
    appearance_timeout_seconds,
):
    while True:
        runs = watcher.list_workflow_runs(
            repo,
            workflow,
            ref,
            expected_head_sha=expected_head_sha,
            minimum_run_id=minimum_run_id,
        )
        if runs:
            return {"kind": "matched", "run": runs[0]}, attempts

        saw_unhydrated_head = False
        stale_runs = watcher.list_workflow_runs(
            repo,
            workflow,
            ref,
            minimum_run_id=minimum_run_id,
        )
        if stale_runs:
            newest = stale_runs[0]
            stale_head_sha = str(newest.get("headSha") or "").strip()
            if expected_head_sha and stale_head_sha and not _head_sha_matches_prefix(
                stale_head_sha, expected_head_sha
            ):
                return {
                    "kind": "stale_head",
                    "run": newest,
                    "stale_head_sha": stale_head_sha,
                }, attempts
            if expected_head_sha and not stale_head_sha:
                # A just-created run can exist with an unhydrated headSha briefly.
                # Treat this as visibility warm-up and keep waiting rather than timing out.
                saw_unhydrated_head = True

        if (
            appearance_timeout_seconds
            and appearance_timeout_seconds > 0
            and not saw_unhydrated_head
            and (time.monotonic() - dispatch_start_time) >= appearance_timeout_seconds
        ):
            return {"kind": "appearance_timed_out", "run": None}, attempts

        if _budget_exceeded(start_time, attempts, max_wait_seconds, max_retries):
            return {"kind": "timed_out", "run": None}, attempts

        attempts += 1
        time.sleep(max(1, poll_seconds))


def _resolve_local_ref_sha(watcher, ref):
    if watcher.is_sha_like(ref):
        return None
    candidates = (f"refs/heads/{ref}", ref)
    for candidate in candidates:
        sha = watcher.command_text(["git", "rev-parse", "--verify", candidate])
        if sha:
            return sha.strip()
    return None


def _validate_expected_head_sha_against_remote_branch(watcher, repo, ref, expected_sha):
    if not expected_sha or watcher.is_sha_like(ref):
        return None
    remote_sha = _resolve_remote_ref_sha(watcher, repo, ref)
    if not remote_sha:
        return None
    if _head_sha_matches_prefix(remote_sha, expected_sha):
        return None
    return remote_sha


def _parse_dispatch_inputs(raw_inputs):
    parsed = []
    for raw in raw_inputs or []:
        token = str(raw or "").strip()
        if not token:
            continue
        if "=" not in token:
            raise ValueError(f"Invalid --input '{token}': expected key=value.")
        key, value = token.split("=", 1)
        key = key.strip()
        value = value.strip()
        if not key:
            raise ValueError(f"Invalid --input '{token}': key is empty.")
        parsed.append((key, value))
    return parsed


def _dispatch_input_value(dispatch_inputs, key):
    key = str(key or "").strip()
    if not key:
        return None
    value = None
    for input_key, input_value in dispatch_inputs or []:
        if input_key == key:
            value = input_value
    return value


def _resolve_default_dispatch_ref(watcher, repo):
    try:
        payload = watcher.gh_json(["repo", "view", "--json", "defaultBranchRef"], repo=repo)
    except Exception:
        return None
    if not isinstance(payload, dict):
        return None
    default_branch_ref = payload.get("defaultBranchRef")
    if not isinstance(default_branch_ref, dict):
        return None
    branch = str(default_branch_ref.get("name") or "").strip()
    return branch or None


def _resolve_dispatch_ref(watcher, repo, requested_ref, dispatch_inputs):
    if requested_ref != "auto":
        return watcher.detect_ref(requested_ref)

    # Downstream-style dispatches usually validate a logical ref input while the
    # workflow itself must still be dispatched from the repo's default branch.
    if _dispatch_input_value(dispatch_inputs, "ref"):
        default_branch = _resolve_default_dispatch_ref(watcher, repo)
        if default_branch:
            return default_branch

    return watcher.detect_ref(requested_ref)


def _resolve_validation_ref(requested_ref, dispatch_ref, dispatch_inputs):
    if requested_ref == "auto":
        logical_ref = _dispatch_input_value(dispatch_inputs, "ref")
        if logical_ref:
            return logical_ref
    return dispatch_ref


def _is_unexpected_supersession_input_error(err):
    message = str(err)
    return (
        "Unexpected inputs provided" in message
        and (
            "supersession_mode" in message
            or "supersession_key" in message
        )
    )


def _dispatch_workflow(
    watcher,
    repo,
    workflow,
    ref,
    supersession_mode,
    supersession_key,
    dispatch_inputs,
):
    def build_args(include_supersession):
        args = ["workflow", "run", workflow, "--ref", ref]
        if include_supersession:
            args.extend(["-f", f"supersession_mode={supersession_mode}"])
            if supersession_key:
                args.extend(["-f", f"supersession_key={supersession_key}"])
        for key, value in dispatch_inputs:
            args.extend(["-f", f"{key}={value}"])
        return args

    try:
        watcher.gh_text(build_args(include_supersession=True), repo=repo)
    except Exception as err:
        if not _is_unexpected_supersession_input_error(err):
            raise
        watcher.gh_text(build_args(include_supersession=False), repo=repo)


def _run_watcher(watcher, run_id, repo, wait_for, poll_seconds, appearance_timeout):
    command = [
        str(WATCHER_LAUNCHER_PATH),
        "--run-id",
        str(int(run_id)),
        "--repo",
        str(repo),
        "--watch-until-action",
        "--wait-for",
        wait_for,
        "--poll-seconds",
        str(poll_seconds),
        "--appearance-timeout-seconds",
        str(appearance_timeout),
    ]

    last_line = ""
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True) as proc:
        if proc.stdout:
            for line in proc.stdout:
                sys.stdout.write(line)
                sys.stdout.flush()
                if line.strip():
                    last_line = line.strip()
        proc.wait()
        if proc.returncode != 0:
            return 1
    if last_line:
        try:
            payload = json.loads(last_line)
        except json.JSONDecodeError:
            return 0
        actions = payload.get("actions") or []
        if "stop_run_appearance_timeout" in actions:
            return 1
    return 0


def main():
    args = parse_args()
    if args.poll_seconds <= 0:
        return _emit_error("--poll-seconds must be > 0")
    if args.max_wait_seconds < 0:
        return _emit_error("--max-wait-seconds must be >= 0")
    if args.max_retries < 0:
        return _emit_error("--max-retries must be >= 0")
    if args.appearance_timeout_seconds < 0:
        return _emit_error("--appearance-timeout-seconds must be >= 0")
    if args.stale_head_retries < 0:
        return _emit_error("--stale-head-retries must be >= 0")
    if args.min_run_id is not None and args.min_run_id <= 0:
        return _emit_error("--min-run-id must be > 0")

    watcher = _load_watcher()
    repo = args.repo or watcher.detect_repo()
    if not repo:
        return _emit_error(
            "Unable to determine OWNER/REPO from args/repo nor from git remotes/gh config."
        )

    try:
        dispatch_inputs = _parse_dispatch_inputs(args.input)
    except ValueError as err:
        return _emit_error(str(err))

    try:
        dispatch_ref = _resolve_dispatch_ref(watcher, repo, args.ref, dispatch_inputs)
    except watcher.GhCommandError as err:
        return _emit_error(err)
    validation_ref = _resolve_validation_ref(args.ref, dispatch_ref, dispatch_inputs)

    expected_sha = str(args.head_sha or "").strip()
    expected_sha_from_args = bool(expected_sha)
    if not expected_sha:
    if not expected_sha:
        expected_sha = _resolve_remote_ref_sha(watcher, repo, validation_ref) or \
                       _resolve_local_ref_sha(watcher, validation_ref) or \
                       (watcher.command_text(["git", "rev-parse", "HEAD"]) or "").strip()
        return _emit_error("Expected head SHA is missing and `git rev-parse HEAD` returned nothing.")
    if not _is_head_sha_prefix(expected_sha):
        return _emit_error(f"Expected head SHA '{expected_sha}' is not a valid commit sha.")

    start_time = time.monotonic()
    attempts = 0

    if expected_sha_from_args:
        mismatch_observed_sha = _validate_expected_head_sha_against_remote_branch(
            watcher,
            repo,
            validation_ref,
            expected_sha,
        )
        if mismatch_observed_sha:
            return _emit_expected_head_sha_mismatch(
                workflow=args.workflow,
                ref=validation_ref,
                expected_sha=expected_sha,
                observed_sha=mismatch_observed_sha,
            )

    if not watcher.is_sha_like(validation_ref):
        ok, attempts = _wait_for_ref_to_match_expected(
            watcher,
            repo,
            validation_ref,
            expected_sha,
            start_time=start_time,
            attempts=attempts,
            max_wait_seconds=args.max_wait_seconds,
            max_retries=args.max_retries,
            poll_seconds=args.poll_seconds,
        )
        if not ok:
            return _emit_error(
                f"Timed out waiting for remote ref '{validation_ref}' to match expected SHA prefix '{expected_sha}'."
            )

    try:
        baseline_runs = watcher.list_workflow_runs(repo, args.workflow, dispatch_ref)
        baseline_max_run_id = max(
            (int(run.get("databaseId") or 0) for run in baseline_runs),
            default=0,
        )
        if baseline_max_run_id < 0:
            baseline_max_run_id = 0
    except watcher.GhCommandError as err:
        return _emit_error(err)

    max_dispatch_attempts = max(1, int(args.stale_head_retries) + 1)
    stale_runs = []
    selected_run = None
    for dispatch_attempt in range(1, max_dispatch_attempts + 1):
        dispatch_start_time = time.monotonic()
        try:
            _dispatch_workflow(
                watcher,
                repo,
                args.workflow,
                dispatch_ref,
                args.supersession_mode,
                args.supersession_key,
                dispatch_inputs,
            )
        except watcher.GhCommandError as err:
            return _emit_error(f"Workflow dispatch failed: {err}")

        min_run_id = _effective_minimum_run_id(baseline_max_run_id, args.min_run_id)
        selection_expected_head_sha = expected_sha if validation_ref == dispatch_ref else None
        selection, attempts = _select_newest_matching_run(
            watcher,
            repo,
            args.workflow,
            dispatch_ref,
            expected_head_sha=selection_expected_head_sha,
            minimum_run_id=min_run_id,
            start_time=start_time,
            attempts=attempts,
            max_wait_seconds=args.max_wait_seconds,
            max_retries=args.max_retries,
            poll_seconds=args.poll_seconds,
            dispatch_start_time=dispatch_start_time,
            appearance_timeout_seconds=args.appearance_timeout_seconds,
        )
        kind = selection.get("kind")
        selected = selection.get("run")
        if kind == "matched" and selected:
            selected_run = selected
            break
        if kind == "stale_head" and selected:
            stale_run_id = int(selected.get("databaseId") or 0)
            baseline_max_run_id = max(baseline_max_run_id, stale_run_id)
            stale_runs.append(
                {
                    "dispatch_attempt": dispatch_attempt,
                    "run_id": stale_run_id,
                    "run_url": str(selected.get("url") or ""),
                    "run_head_sha": str(selected.get("headSha") or ""),
                    "expected_head_sha": expected_sha,
                }
            )
            if dispatch_attempt >= max_dispatch_attempts:
                return _emit_stale_head_timeout(
                    workflow=args.workflow,
                    ref=dispatch_ref,
                    expected_sha=expected_sha,
                    attempts_used=dispatch_attempt,
                    retries_allowed=args.stale_head_retries,
                    stale_runs=stale_runs,
                )
            if not watcher.is_sha_like(validation_ref):
                ok, attempts = _wait_for_ref_to_match_expected(
                    watcher,
                    repo,
                    validation_ref,
                    expected_sha,
                    start_time=start_time,
                    attempts=attempts,
                    max_wait_seconds=args.max_wait_seconds,
                    max_retries=args.max_retries,
                    poll_seconds=args.poll_seconds,
                )
                if not ok:
                    return _emit_error(
                        f"Timed out waiting for remote ref '{validation_ref}' to match expected SHA prefix '{expected_sha}'."
                    )
            continue
        if kind == "appearance_timed_out":
            return _emit_dispatch_appearance_timeout(
                workflow=args.workflow,
                ref=dispatch_ref,
                expected_sha=expected_sha,
                appearance_timeout_seconds=args.appearance_timeout_seconds,
                attempts_used=dispatch_attempt,
            )
        if kind == "timed_out":
            break

    if not selected_run:
        if stale_runs:
            return _emit_stale_head_timeout(
                workflow=args.workflow,
                ref=dispatch_ref,
                expected_sha=expected_sha,
                attempts_used=max_dispatch_attempts,
                retries_allowed=args.stale_head_retries,
                stale_runs=stale_runs,
            )
        return _emit_error(
            f"Timed out waiting for a new matching run for workflow '{args.workflow}', ref '{dispatch_ref}', "
            f"head-sha '{expected_sha}' after dispatch."
        )

    run_id = int(selected_run.get("databaseId") or 0)
    if run_id <= baseline_max_run_id:
        return _emit_error(
            f"Selected run id {run_id} did not advance past baseline {baseline_max_run_id}; "
            "dispatch race guard triggered."
        )

    return _run_watcher(
        watcher,
        run_id,
        repo=repo,
        wait_for=args.wait_for,
        poll_seconds=args.poll_seconds,
        appearance_timeout=args.appearance_timeout_seconds,
    )


if __name__ == "__main__":
    raise SystemExit(main())
