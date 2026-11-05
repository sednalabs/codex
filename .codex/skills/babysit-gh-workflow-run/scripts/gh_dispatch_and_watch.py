#!/usr/bin/env python3
"""Dispatch a workflow and watch the matching run with SHA guardrails."""

import argparse
import importlib.util
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import quote, urlsplit


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
        help="Branch or ref to dispatch against (default: auto).",
    )
    parser.add_argument(
        "--head-sha",
        default=None,
        help=(
            "Expected workflow-host SHA (or 4+ char abbreviation) to wait for before dispatching "
            "and to pin the GitHub run. "
            "Defaults to local HEAD SHA."
        ),
    )
    parser.add_argument(
        "--validation-target-ref",
        default=None,
        help=(
            "Optional expected ref validated by a separately hosted workflow. Passed to the "
            "watcher as proof identity, not as the dispatch ref."
        ),
    )
    parser.add_argument(
        "--validation-target-sha",
        default=None,
        help=(
            "Optional expected exact or prefix SHA validated by a separately hosted workflow. "
            "Passed to the watcher as proof identity, not as the run head."
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
        "--watch-until-terminal",
        "--wait-until-terminal",
        dest="watch_until_terminal",
        action="store_true",
        help=(
            "Watch until the selected run reaches terminal state. Equivalent to passing "
            "--require-terminal-run to the watcher."
        ),
    )
    parser.add_argument(
        "--appearance-timeout-seconds",
        type=int,
        default=0,
        help="Pass-through to watcher --appearance-timeout-seconds.",
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
        default=0,
        help=(
            "Legacy flag: only 0 is supported; reconcile a mismatched run before another dispatch."
        ),
    )
    parser.add_argument(
        "--min-run-id",
        type=int,
        default=None,
        help="Optional lower bound on watched run ids so older matches are skipped.",
    )
    parser.add_argument(
        "--no-gemini-diagnosis",
        action="store_true",
        help=argparse.SUPPRESS,
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


def _budget_exceeded(start_time, attempts, max_wait_seconds, max_retries):
    if max_wait_seconds and max_wait_seconds > 0 and (time.monotonic() - start_time) >= max_wait_seconds:
        return True
    if max_retries and max_retries > 0 and attempts > max_retries:
        return True
    return False


def _query_remote_ref_sha(watcher, repo, ref):
    _, owner_repo = _repo_identity(repo)
    endpoint = f"/repos/{owner_repo}/git/ref/heads/{quote(ref, safe='')}"
    payload = _api_json(watcher, repo, endpoint)
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
    if hasattr(watcher, "_GH_AUTH"):
        watcher._GH_AUTH.deadline = start_time + max_wait_seconds if max_wait_seconds else None
    while True:
        remote_head_sha = _query_remote_ref_sha(watcher, repo, ref)
        if _head_sha_matches_prefix(remote_head_sha, expected_sha):
            return True, attempts

        if _budget_exceeded(start_time, attempts, max_wait_seconds, max_retries):
            return False, attempts

        attempts += 1
        time.sleep(max(1, poll_seconds))


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


def _is_unexpected_supersession_input_error(err):
    message = str(err).partition("\nstderr: ")[2]
    return (
        "HTTP 422" in message
        and "Unexpected inputs provided" in message
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
        return watcher.gh_text(build_args(include_supersession=True), repo=repo)
    except Exception as err:
        if (not _is_unexpected_supersession_input_error(err)
                or any(key in ("supersession_mode", "supersession_key") for key, _ in dispatch_inputs)):
            raise
        return watcher.gh_text(build_args(include_supersession=False), repo=repo)


def _repo_identity(repo):
    parts = str(repo).split("/")
    if len(parts) == 2:
        host, owner, name = os.environ.get("GH_HOST") or "github.com", *parts
    elif len(parts) == 3:
        host, owner, name = parts
    else:
        raise ValueError("Repository must be [HOST/]OWNER/REPO.")
    if not re.fullmatch(r"[A-Za-z0-9.-]+", host) or not all(
        re.fullmatch(r"[A-Za-z0-9_.-]+", item) and item not in (".", "..")
        for item in (owner, name)
    ):
        raise ValueError("Invalid repository identity.")
    return host.lower(), f"{owner}/{name}"


def _resolve_repo_host(watcher, repo, *, current_repo=False):
    # Cwd autodetection has discarded the remote host. Ask gh for its current
    # repository without a hostless selector, then cross-check owner/repo.
    # Explicit selectors retain gh's normal selection semantics.
    parts = str(repo).split("/")
    if len(parts) not in (2, 3):
        raise ValueError("Repository must be [HOST/]OWNER/REPO.")
    requested_host = parts[0].lower() if len(parts) == 3 else None
    env_host = (os.environ.get("GH_HOST") or "").strip().lower() or None
    if requested_host and env_host and requested_host != env_host:
        raise ValueError("Explicit repository host conflicts with GH_HOST.")
    # Validate the caller's syntax before asking gh to resolve it.
    _repo_identity(repo)
    payload = watcher.gh_json(["repo", "view", "--json", "nameWithOwner,url"],
                              repo=None if current_repo else repo)
    if not isinstance(payload, dict):
        raise ValueError("Repository host could not be established.")
    name = payload.get("nameWithOwner")
    url = payload.get("url")
    if not isinstance(name, str) or not isinstance(url, str):
        raise ValueError("Repository host could not be established.")
    parsed = urlsplit(url)
    host = parsed.hostname or ""
    labels = host.split(".")
    if (parsed.scheme != "https" or parsed.netloc.lower() != host.lower()
            or parsed.query or parsed.fragment or not host
            or any(not re.fullmatch(r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?", label) for label in labels)
            or parsed.path.rstrip("/").lower() != f"/{name}".lower()
            or name.lower() != "/".join(parts[-2:]).lower()
            or (requested_host and requested_host != host.lower())
            or (env_host and env_host != host.lower())):
        raise ValueError("Repository host or identity is conflicting or malformed.")
    qualified = f"{host.lower()}/{name}"
    _repo_identity(qualified)
    return qualified


def _api_json(watcher, repo, endpoint):
    host, _ = _repo_identity(repo)
    return watcher.gh_json(["api", endpoint, "--hostname", host], repo=repo)


def _workflow_id(watcher, repo, workflow):
    _, owner_repo = _repo_identity(repo)
    if workflow.isdigit() or workflow.endswith((".yml", ".yaml")):
        payload = _api_json(watcher, repo, f"/repos/{owner_repo}/actions/workflows/{quote(workflow, safe='')}")
        candidates = [payload]
    else:
        candidates = []
        page = 1
        total = None
        while total is None or len(candidates) < total:
            payload = _api_json(watcher, repo, f"/repos/{owner_repo}/actions/workflows?per_page=100&page={page}")
            if not isinstance(payload, dict) or type(payload.get("total_count")) is not int or not isinstance(payload.get("workflows"), list):
                raise ValueError("Incomplete workflow catalog.")
            if total is not None and total != payload["total_count"]:
                raise ValueError("Workflow catalog changed during lookup.")
            total = payload["total_count"]
            rows = payload["workflows"]
            if not rows and len(candidates) < total:
                raise ValueError("Incomplete workflow catalog.")
            candidates.extend(rows)
            page += 1
        if len(candidates) != total:
            raise ValueError("Incomplete workflow catalog.")
        candidates = [item for item in candidates if isinstance(item, dict) and (
            item.get("name") == workflow or Path(str(item.get("path") or "")).stem == workflow
        )]
    if len(candidates) != 1 or not isinstance(candidates[0], dict):
        raise ValueError("Workflow selector is missing or ambiguous.")
    workflow_id = candidates[0].get("id")
    if type(workflow_id) is not int or workflow_id <= 0:
        raise ValueError("Workflow has no valid id.")
    return workflow_id


def _dispatch_run_id(output, repo):
    host, owner_repo = _repo_identity(repo)
    value = str(output or "").strip()
    parsed = urlsplit(value)
    match = re.fullmatch(r"/([^/]+)/([^/]+)/actions/runs/([1-9][0-9]*)", parsed.path)
    if (parsed.scheme != "https" or parsed.netloc.lower() != host or parsed.query or parsed.fragment
            or not match or f"{match.group(1)}/{match.group(2)}".lower() != owner_repo.lower()):
        raise ValueError("Dispatch response has no trustworthy run identity; do not redispatch.")
    return int(match.group(3))


def _emit_dispatch_receipt(repo, run_id, stage, workflow_id, ref, expected_sha):
    host, owner_repo = _repo_identity(repo)
    print(json.dumps({"dispatch_receipt": {"stage": stage, "repo": owner_repo,
        "run_id": run_id, "run_url": f"https://{host}/{owner_repo}/actions/runs/{run_id}",
        "expected_workflow_id": workflow_id, "expected_host_ref": ref,
        "expected_host_sha": expected_sha}}, sort_keys=True), flush=True)


def _wait_for_exact_run(watcher, repo, run_id, workflow_id, ref, expected_sha, *,
                        start_time, attempts, max_wait_seconds, max_retries,
                        poll_seconds, appearance_timeout_seconds):
    _, owner_repo = _repo_identity(repo)
    deadline = time.monotonic() + (appearance_timeout_seconds or max_wait_seconds or 600)
    if hasattr(watcher, "_GH_AUTH"):
        watcher._GH_AUTH.deadline = min(deadline, start_time + max_wait_seconds) if max_wait_seconds else deadline
    while True:
        try:
            run = _api_json(watcher, repo, f"/repos/{owner_repo}/actions/runs/{run_id}")
        except watcher.GhCommandError as err:
            if not re.search(r"\bHTTP 404\b", str(err)):
                raise
            run = None
        if run is not None:
            if not isinstance(run, dict) or type(run.get("id")) is not int:
                raise ValueError("Incomplete exact-run identity.")
            repository = run.get("repository")
            if (run["id"] != run_id or not isinstance(repository, dict)
                    or str(repository.get("full_name") or "").lower() != owner_repo.lower()
                    or type(run.get("workflow_id")) is not int or run.get("workflow_id") != workflow_id
                    or run.get("event") != "workflow_dispatch"
                    or run.get("head_branch") != ref or run.get("head_sha") != expected_sha):
                raise ValueError("Exact dispatched run identity did not match; do not redispatch.")
            return run
        if time.monotonic() >= deadline or _budget_exceeded(start_time, attempts, max_wait_seconds, max_retries):
            raise ValueError("Exact dispatched run did not become visible; do not redispatch.")
        attempts += 1
        time.sleep(max(1, poll_seconds))


def _run_watcher(
    watcher,
    run_id,
    repo,
    wait_for,
    poll_seconds,
    appearance_timeout,
    require_terminal_run,
    validation_target_ref=None,
    validation_target_sha=None,
    host=None,
):
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
    if require_terminal_run:
        command.append("--require-terminal-run")
    if validation_target_ref:
        command.extend(["--validation-target-ref", str(validation_target_ref)])
    if validation_target_sha:
        command.extend(["--validation-target-sha", str(validation_target_sha)])

    env = dict(os.environ, GH_HOST=host, GH_REPO=f"{host}/{repo}") if host else None
    result = subprocess.run([str(WATCHER_LAUNCHER_PATH), *command[1:]], shell=False, capture_output=True, text=True, check=False, env=env)
    if result.stdout:
        sys.stdout.write(result.stdout)
        if not result.stdout.endswith("\n"):
            sys.stdout.write("\n")
    sys.stdout.flush()

    if result.returncode != 0:
        if result.stderr:
            sys.stderr.write(result.stderr + ("\n" if not result.stderr.endswith("\n") else ""))
        return 1

    if not result.stdout:
        return 0

    last_line = ""
    for line in result.stdout.splitlines():
        if line.strip():
            last_line = line.strip()
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
    if args.stale_head_retries:
        return _emit_error("--stale-head-retries is no longer supported; reconcile the exact run before another dispatch.")
    if args.min_run_id is not None and args.min_run_id <= 0:
        return _emit_error("--min-run-id must be > 0")
    if os.environ.get("GITHUB_APP_BROKER_SOCKET"):
        return _emit_error("The configured GitHub App broker is read-only and does not support workflow dispatch; dispatch not attempted.")
    for selector in ("GH_WORKFLOW_RUN_WATCH_REPO", "GH_REPO"):
        if selector in os.environ and not os.environ[selector].strip():
            return _emit_error(f"{selector} is empty or whitespace; dispatch not attempted.")

    watcher = _load_watcher()
    env_repo = (os.environ.get("GH_WORKFLOW_RUN_WATCH_REPO") or os.environ.get("GH_REPO") or "").strip()
    explicit_repo = bool(args.repo or env_repo)
    repo = args.repo or watcher.detect_repo()
    if not repo:
        return _emit_error(
            "Unable to determine OWNER/REPO from args/repo nor from git remotes/gh config."
        )

    try:
        repo = _resolve_repo_host(watcher, repo, current_repo=not explicit_repo)
    except (ValueError, watcher.GhCommandError):
        return _emit_error("Unable to resolve an unambiguous repository host; dispatch not attempted.")

    try:
        ref = watcher.detect_ref(args.ref)
    except watcher.GhCommandError as err:
        return _emit_error(err)
    if watcher.is_sha_like(ref) or ref.startswith("refs/"):
        return _emit_error("A branch-name dispatch ref is required for exact host verification.")

    expected_sha = str(args.head_sha or "").strip()
    expected_sha_from_args = bool(expected_sha)
    if not expected_sha:
        expected_sha = _resolve_remote_ref_sha(watcher, repo, ref) or (
            watcher.command_text(["git", "rev-parse", "HEAD"]) or ""
        ).strip()
    if not expected_sha:
        return _emit_error("Expected head SHA is missing and `git rev-parse HEAD` returned nothing.")
    if not _is_head_sha_prefix(expected_sha):
        return _emit_error(f"Expected head SHA '{expected_sha}' is not a valid commit sha.")
    validation_target_sha = str(args.validation_target_sha or "").strip()
    if validation_target_sha and not _is_head_sha_prefix(validation_target_sha):
        return _emit_error(
            f"Expected validation target SHA '{validation_target_sha}' is not a valid commit sha."
        )
    try:
        dispatch_inputs = _parse_dispatch_inputs(args.input)
    except ValueError as err:
        return _emit_error(str(err))

    start_time = time.monotonic()
    attempts = 0

    if expected_sha_from_args:
        mismatch_observed_sha = _validate_expected_head_sha_against_remote_branch(
            watcher,
            repo,
            ref,
            expected_sha,
        )
        if mismatch_observed_sha:
            return _emit_expected_head_sha_mismatch(
                workflow=args.workflow,
                ref=ref,
                expected_sha=expected_sha,
                observed_sha=mismatch_observed_sha,
            )

    if not watcher.is_sha_like(ref):
        ok, attempts = _wait_for_ref_to_match_expected(
            watcher,
            repo,
            ref,
            expected_sha,
            start_time=start_time,
            attempts=attempts,
            max_wait_seconds=args.max_wait_seconds,
            max_retries=args.max_retries,
            poll_seconds=args.poll_seconds,
        )
        if not ok:
            return _emit_error(
                f"Timed out waiting for remote ref '{ref}' to match expected SHA prefix '{expected_sha}'."
            )

    try:
        full_expected_sha = _query_remote_ref_sha(watcher, repo, ref)
        if (not re.fullmatch(r"[0-9a-fA-F]{40}", full_expected_sha)
                or not _head_sha_matches_prefix(full_expected_sha, expected_sha)):
            return _emit_error("Remote branch changed or did not provide a full expected SHA; dispatch not attempted.")
        selected_workflow_id = _workflow_id(watcher, repo, args.workflow)
    except (ValueError, watcher.GhCommandError):
        return _emit_error("Unable to resolve the exact workflow and host identity; dispatch not attempted.")

    try:
        output = _dispatch_workflow(
            watcher, repo, str(selected_workflow_id), ref,
            args.supersession_mode, args.supersession_key, dispatch_inputs,
        )
    except watcher.GhCommandError:
        return _emit_error("Workflow dispatch failed or its outcome is unknown; do not redispatch without reconciliation.")

    try:
        run_id = _dispatch_run_id(output, repo)
        _emit_dispatch_receipt(repo, run_id, "provisional", selected_workflow_id, ref, full_expected_sha)
        if args.min_run_id is not None and run_id < args.min_run_id:
            raise ValueError("Returned run id is below the requested minimum; do not redispatch.")
        _wait_for_exact_run(
            watcher, repo, run_id, selected_workflow_id, ref, full_expected_sha,
            start_time=start_time, attempts=attempts, max_wait_seconds=args.max_wait_seconds,
            max_retries=args.max_retries, poll_seconds=args.poll_seconds,
            appearance_timeout_seconds=args.appearance_timeout_seconds,
        )
        _emit_dispatch_receipt(repo, run_id, "verified", selected_workflow_id, ref, full_expected_sha)
    except ValueError as err:
        return _emit_error(err)
    except watcher.GhCommandError:
        return _emit_error("Exact dispatched run could not be verified; do not redispatch.")

    host, owner_repo = _repo_identity(repo)
    return _run_watcher(
        watcher,
        run_id,
        repo=owner_repo,
        wait_for=args.wait_for,
        poll_seconds=args.poll_seconds,
        appearance_timeout=args.appearance_timeout_seconds,
        require_terminal_run=args.watch_until_terminal,
        validation_target_ref=args.validation_target_ref,
        validation_target_sha=validation_target_sha or None,
        host=host,
    )


if __name__ == "__main__":
    raise SystemExit(main())
