#!/usr/bin/env python3
"""Watch GitHub PR CI and review activity for Codex PR babysitting workflows."""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from urllib.parse import urlparse

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from github_watch_auth import AuthState, is_auth_failure, is_rate_limited, is_retry_safe, rate_resource, redact, wait_for_reset

FAILED_RUN_CONCLUSIONS = {
    "failure",
    "timed_out",
    "cancelled",
    "action_required",
    "startup_failure",
    "stale",
}
RERUNNABLE_FAILURE_CONCLUSIONS = FAILED_RUN_CONCLUSIONS - {"startup_failure"}
HELPER_VERSION = "1.1.0-head-guard"
PENDING_CHECK_STATES = {
    "QUEUED",
    "IN_PROGRESS",
    "PENDING",
    "WAITING",
    "REQUESTED",
}
FAILED_CHECK_STATES = {
    "FAILURE",
    "FAILED",
    "TIMED_OUT",
    "CANCELLED",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
    "STALE",
    "ERROR",
}
REVIEW_BOT_LOGIN_KEYWORDS = {
    "codex",
    "gemini",
}
TRUSTED_AUTHOR_ASSOCIATIONS = {
    "OWNER",
    "MEMBER",
    "COLLABORATOR",
}
MERGE_BLOCKING_REVIEW_DECISIONS = {
    "REVIEW_REQUIRED",
    "CHANGES_REQUESTED",
}
REVIEW_SUBMISSION_BLOCKING_STATES = {
    "CHANGES_REQUESTED",
    "REQUEST_CHANGES",
}
MERGE_CONFLICT_OR_BLOCKING_STATES = {
    "DIRTY",
    "UNKNOWN",
}
MERGE_QUEUE_WAITING_STATES = {
    "AWAITING_CHECKS",
    "LOCKED",
    "MERGEABLE",
    "QUEUED",
}
MERGE_QUEUE_FAILED_STATES = {
    "CANCELLED",
    "FAILED",
    "REMOVED",
    "UNMERGEABLE",
}
COMMAND_ONLY_ISSUE_COMMENT_MAX_TOKENS = 4
CURRENT_HEAD_CHECK_GRACE_SECONDS = 5 * 60
GREEN_STATE_MAX_POLL_SECONDS = 60 * 60
WATCH_UNTIL_ACTION_MAX_POLL_SECONDS = 20 * 60
ACTION_REQUIRED_MERGE_POLICY_BLOCKED = "action_required_merge_policy_blocked"
STOP_MERGE_QUEUE_FAILED = "stop_merge_queue_failed"
STOP_MERGE_QUEUE_REMOVED = "stop_merge_queue_removed"
STOP_MERGE_QUEUE_READ_ERROR = "stop_merge_queue_read_error"
STOP_ACTIONS = {
    "stop_ci_startup_blocked",
    "stop_pr_closed",
    "stop_exhausted_retries",
    "stop_merge_queue_failed",
    "stop_merge_queue_read_error",
    "stop_merge_queue_removed",
    "stop_ready_to_merge",
}
SEEN_FEEDBACK_STATE_KEYS = (
    "seen_issue_comment_ids",
    "seen_review_comment_ids",
    "seen_review_ids",
)
NON_ACTIONABLE_ISSUE_COMMENT_SNIPPETS = (
    "<!-- codex-pull-request-review-summary -->",
    "usage limits for code reviews",
    "codex usage dashboard",
    "add credits to your account",
    "enable them for code reviews in your settings",
    "daily quota limit",
    "wait up to 24 hours",
    "start processing your requests again",
)
NON_ACTIONABLE_REVIEW_NO_FEEDBACK_PATTERN = re.compile(
    r"\bno(?:\s+additional)?\s+feedback(?:\s+to\s+provide)?\b"
)


class GhCommandError(RuntimeError):
    pass


_GH_ENV = None
_GH_AUTH = AuthState()


def _default_gh_dir(kind):
    home = Path.home()
    if kind == "config":
        base = os.environ.get("XDG_CONFIG_HOME")
        if base:
            base_path = Path(base)
        else:
            base_path = home / ".config"
    elif kind == "cache":
        base = os.environ.get("XDG_CACHE_HOME")
        if base:
            base_path = Path(base)
        else:
            base_path = home / ".cache"
    else:
        return None
    return base_path / "gh"


def _ensure_writable_dir(path_like):
    path = Path(path_like)
    try:
        path.mkdir(parents=True, exist_ok=True)
    except OSError:
        return False
    return os.access(path, os.W_OK)


def _is_readable_dir(path_like):
    path = Path(path_like)
    return path.is_dir() and os.access(path, os.R_OK | os.X_OK)


def _ensure_config_dir(env, var):
    current_value = env.get(var)
    if current_value:
        candidate = Path(current_value)
        if _is_readable_dir(candidate):
            env[var] = str(candidate)
            return
        if _ensure_writable_dir(candidate):
            env[var] = str(candidate)
            return

    default_path = _default_gh_dir("config")
    if default_path and _is_readable_dir(default_path):
        env[var] = str(default_path)
        return
    if default_path and _ensure_writable_dir(default_path):
        env[var] = str(default_path)
        return

    temp_dir = Path(tempfile.mkdtemp(prefix=f"gh-{var.lower()}-"))
    env[var] = str(temp_dir)


def _ensure_env_dir(env, var, kind):
    current_value = env.get(var)
    if current_value:
        candidate = Path(current_value)
        if _ensure_writable_dir(candidate):
            env[var] = str(candidate)
            return

    default_path = _default_gh_dir(kind)
    if default_path and _ensure_writable_dir(default_path):
        env[var] = str(default_path)
        return

    temp_dir = Path(tempfile.mkdtemp(prefix=f"gh-{var.lower()}-"))
    env[var] = str(temp_dir)


def _prepare_gh_env(repo=None, force_refresh=False):
    global _GH_ENV
    if _GH_ENV is not None:
        _GH_AUTH.apply(_GH_ENV, repo=repo, force_refresh=force_refresh)
        return _GH_ENV
    env = os.environ.copy()
    _ensure_config_dir(env, "GH_CONFIG_DIR")
    _ensure_env_dir(env, "GH_CACHE_DIR", "cache")
    _GH_AUTH.apply(env, repo=repo, force_refresh=force_refresh)
    _GH_ENV = env
    return env


def parse_args():
    parser = argparse.ArgumentParser(
        description=(
            "Normalize PR/CI/review state for Codex PR babysitting and optionally "
            "trigger flaky reruns."
        )
    )
    parser.add_argument("--pr", default="auto", help="auto, PR number, or PR URL")
    parser.add_argument("--repo", help="Optional OWNER/REPO override")
    parser.add_argument(
        "--installation-observer",
        action="store_true",
        help=(
            "Use an explicitly supplied GitHub App installation token as a read-only "
            "observer; do not query the unavailable /user endpoint"
        ),
    )
    parser.add_argument("--poll-seconds", type=int, default=30, help="Watch poll interval")
    parser.add_argument(
        "--max-flaky-retries",
        type=int,
        default=3,
        help="Max rerun cycles per head SHA before stop recommendation",
    )
    parser.add_argument("--state-file", help="State JSON filename in the system temporary directory (no directory paths)")
    parser.add_argument("--once", action="store_true", help="Emit one snapshot and exit")
    parser.add_argument("--watch", action="store_true", help="Continuously emit JSONL snapshots")
    parser.add_argument(
        "--watch-until-action",
        action="store_true",
        help="Poll until a non-idle action or strict stop appears, then emit one result and exit",
    )
    parser.add_argument(
        "--watch-until-terminal",
        "--wait-until-terminal",
        dest="watch_until_terminal",
        action="store_true",
        help=(
            "Poll until a non-idle action or strict stop appears, but keep waiting past "
            "in-progress CI failures until PR checks are terminal."
        ),
    )
    parser.add_argument(
        "--require-terminal-checks",
        action="store_true",
        help=(
            "(Only relevant with --watch-until-action) keep waiting until PR checks are "
            "terminal before returning CI failure actions."
        ),
    )
    parser.add_argument(
        "--retry-failed-now",
        action="store_true",
        help="Rerun failed jobs for current failed workflow runs when policy allows",
    )
    parser.add_argument(
        "--expected-head-sha",
        "--expected-head",
        dest="expected_head_sha",
        help=(
            "Expected exact PR head SHA required with --retry-failed-now; the PR and "
            "selected runs are re-read against this value before mutation"
        ),
    )
    parser.add_argument(
        "--run-id",
        dest="run_ids",
        action="append",
        help=(
            "Exact workflow run ID to retry; repeat for multiple runs. When omitted, "
            "the current snapshot's failed run IDs are bound into the retry receipt"
        ),
    )
    parser.add_argument(
        "--reset-seen-feedback",
        action="store_true",
        help="Treat currently visible trusted review feedback as unseen on the first snapshot",
    )
    parser.add_argument(
        "--ignore-review-thread",
        action="append",
        default=[],
        help="Review thread URL/id to ignore when computing actionable unresolved threads",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable output (default behavior for --once and --retry-failed-now)",
    )
    parser.add_argument(
        "--progress",
        action="store_true",
        help=(
            "Emit one compact stderr progress line per internal wait cycle. Disabled by "
            "default so blocking waits return only their final receipt."
        ),
    )
    parser.add_argument(
        "--verbose-details",
        action="store_true",
        help=(
            "Return the full PR snapshot from a blocking wait. By default blocking waits "
            "return a compact, action-complete receipt."
        ),
    )
    args = parser.parse_args()

    if args.installation_observer and args.retry_failed_now:
        parser.error(
            "--installation-observer cannot be combined with --retry-failed-now "
            "(observer mode is read-only)"
        )
    if re.fullmatch(r"\d+", args.pr) and not args.repo:
        parser.error(
            "bare PR numbers require --repo OWNER/REPO; use a full PR URL when delegating"
        )
    if args.repo and (
        len(args.repo.split("/")) != 2
        or not all(args.repo.split("/"))
        or any(char.isspace() for char in args.repo)
    ):
        parser.error("--repo must use the exact OWNER/REPO shape")
    if args.poll_seconds <= 0:
        parser.error("--poll-seconds must be > 0")
    if args.max_flaky_retries < 0:
        parser.error("--max-flaky-retries must be >= 0")
    watch_mode_enabled = args.watch_until_action or args.watch_until_terminal
    selected_modes = sum(
        1
        for enabled in (args.once, args.watch, watch_mode_enabled, args.retry_failed_now)
        if enabled
    )
    if selected_modes > 1:
        parser.error(
            "choose only one of --once, --watch, --watch-until-action, "
            "--watch-until-terminal, or --retry-failed-now"
        )
    if watch_mode_enabled:
        args.watch_until_action = True
        if args.watch_until_terminal:
            args.require_terminal_checks = True
    if not args.once and not args.watch and not args.watch_until_action and not args.retry_failed_now:
        args.once = True
    return args


def _format_gh_error(cmd, err):
    stdout = (err.stdout or "").strip()
    stderr = (err.stderr or "").strip()
    parts = [f"GitHub CLI command failed: {' '.join(cmd)} (auth_source={_GH_AUTH.source})"]
    if stdout:
        parts.append(f"stdout: {redact(stdout, (_GH_AUTH._token,))}")
    if stderr:
        parts.append(f"stderr: {redact(stderr, (_GH_AUTH._token,))}")
    return "\n".join(parts)


def classify_gh_error(err):
    normalized = " ".join(str(err).split()).casefold()
    if any(
        snippet in normalized
        for snippet in (
            "http 401",
            "bad credentials",
            "authentication failed",
            "not logged into any github hosts",
            "gh auth login",
        )
    ):
        return "github_auth"
    return "gh_command"


def gh_text(args, repo=None):
    broker_socket = os.environ.get("GITHUB_APP_BROKER_SOCKET")
    if broker_socket:
        from github_app_broker_proxy import request
        broker_args = list(args)
        if repo and (not broker_args or broker_args[0] != "api"):
            broker_args = ["-R", repo, *broker_args]
        result = request(broker_socket, broker_args)
        if int(result.get("returncode", 1)) != 0:
            raise GhCommandError(f"brokered GitHub CLI command failed: {result.get('stderr', '')}")
        return str(result.get("stdout", ""))
    cmd = ["gh"]
    # `gh api` does not accept `-R/--repo` on all gh versions. The watcher's
    # API calls use explicit endpoints (e.g. repos/{owner}/{repo}/...), so the
    # repo flag is unnecessary there.
    if repo and (not args or args[0] != "api"):
        cmd.extend(["-R", repo])
    cmd.extend(args)
    env = _prepare_gh_env(repo=repo)
    retry_safe = is_retry_safe(args)
    for attempt in range(3):
      try:
        proc = subprocess.run(cmd, check=True, capture_output=True, text=True, env=env)
        return proc.stdout
      except FileNotFoundError as err:
        raise GhCommandError("`gh` command not found") from err
      except subprocess.CalledProcessError as err:
        message = _format_gh_error(cmd, err)
        if retry_safe and attempt == 0 and is_auth_failure(message):
            if _GH_AUTH.refresh(env, repo=repo):
                continue
        if retry_safe and attempt == 0 and is_rate_limited(message) and wait_for_reset(_GH_AUTH, env, message, resource=rate_resource(args)):
            continue
        raise GhCommandError(message) from err


def gh_json(args, repo=None):
    raw = gh_text(args, repo=repo).strip()
    if not raw:
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError as err:
        raise GhCommandError(f"Failed to parse JSON from gh output for {' '.join(args)}") from err


def parse_pr_spec(pr_spec):
    if pr_spec == "auto":
        return {"mode": "auto", "value": None}
    if re.fullmatch(r"\d+", pr_spec):
        return {"mode": "number", "value": pr_spec}
    parsed = urlparse(pr_spec)
    if parsed.scheme and parsed.netloc and "/pull/" in parsed.path:
        return {"mode": "url", "value": pr_spec}
    raise ValueError("--pr must be 'auto', a PR number, or a PR URL")


def pr_view_fields():
    return (
        "number,url,state,mergedAt,closedAt,headRefName,headRefOid,"
        "headRepository,headRepositoryOwner,baseRefName,baseRefOid,"
        "mergeable,mergeStateStatus,reviewDecision"
    )


def checks_fields():
    return "name,state,bucket,link,workflow,event,startedAt,completedAt"


def is_no_checks_reported_error(err):
    return "no checks reported on the" in str(err).casefold()


def pending_checks_not_reported_item():
    return {
        "name": "GitHub checks not reported yet",
        "state": "PENDING",
        "bucket": "pending",
        "link": "",
        "workflow": "github-checks",
        "event": "",
        "startedAt": "",
        "completedAt": "",
        "synthetic_no_checks_reported": True,
    }


def is_no_checks_reported_item(check):
    return bool(isinstance(check, dict) and check.get("synthetic_no_checks_reported"))


def is_clean_mergeable_pr(pr):
    return (
        str(pr.get("mergeable") or "").upper() == "MERGEABLE"
        and str(pr.get("merge_state_status") or "").upper() == "CLEAN"
    )


def normalize_merge_queue_entry(raw_entry, field_present=True):
    """Normalize GitHub's mergeQueueEntry evidence without inferring missing data."""
    if not field_present:
        return {
            "read_state": "error",
            "status": "unknown",
            "id": "",
            "state": "",
            "position": None,
            "head_sha": "",
            "source": "github",
            "details": "GitHub did not return merge-queue evidence for this open PR.",
        }
    if raw_entry is None:
        return {
            "read_state": "observed_absent",
            "status": "absent",
            "id": "",
            "state": "",
            "position": None,
            "head_sha": "",
            "source": "github",
            "details": "GitHub reports no active merge-queue entry.",
        }
    if not isinstance(raw_entry, dict):
        return {
            "read_state": "error",
            "status": "unknown",
            "id": "",
            "state": "",
            "position": None,
            "head_sha": "",
            "source": "github",
            "details": "GitHub returned an invalid merge-queue entry.",
        }

    state = str(raw_entry.get("state") or "").upper()
    if state in MERGE_QUEUE_WAITING_STATES:
        status = "waiting"
    elif state in MERGE_QUEUE_FAILED_STATES:
        status = "failed"
    else:
        status = "unknown"
    head_commit = raw_entry.get("headCommit") or {}
    return {
        "read_state": "observed",
        "status": status,
        "id": str(raw_entry.get("id") or ""),
        "state": state,
        "position": raw_entry.get("position"),
        "head_sha": str(head_commit.get("oid") or "") if isinstance(head_commit, dict) else "",
        "source": "github",
        "details": "GitHub returned merge-queue evidence.",
    }


def split_repo_owner_and_name(repo):
    owner, separator, name = str(repo or "").partition("/")
    if not separator or not owner or not name or "/" in name:
        raise GhCommandError(f"Repository must use exact OWNER/REPO shape: {repo!r}")
    return owner, name


def merge_queue_graphql_query():
    return (
        "query($owner:String!,$name:String!,$number:Int!){"
        "repository(owner:$owner,name:$name){"
        "pullRequest(number:$number){"
        "mergeQueueEntry{id state position headCommit{oid}}"
        "}}}"
    )


def merge_queue_read_error(details):
    queue = normalize_merge_queue_entry(None, field_present=False)
    queue["details"] = str(details)
    return queue


def get_merge_queue_entry(repo, pr_number):
    """Read queue evidence through GraphQL; `gh pr view --json` does not expose it."""
    try:
        owner, name = split_repo_owner_and_name(repo)
        number = int(pr_number)
    except (TypeError, ValueError, GhCommandError) as err:
        return merge_queue_read_error(f"Unable to form merge-queue GraphQL request: {err}")
    if number <= 0:
        return merge_queue_read_error("PR number must be positive for merge-queue GraphQL request.")

    try:
        payload = gh_json(
            [
                "api",
                "graphql",
                "-f",
                f"query={merge_queue_graphql_query()}",
                "-F",
                f"owner={owner}",
                "-F",
                f"name={name}",
                "-F",
                f"number={number}",
            ]
        )
    except GhCommandError as err:
        return merge_queue_read_error(f"GitHub merge-queue GraphQL read failed: {err}")

    if not isinstance(payload, dict):
        return merge_queue_read_error("GitHub merge-queue GraphQL response was not an object.")
    if payload.get("errors"):
        return merge_queue_read_error(
            "GitHub merge-queue GraphQL response contained errors; partial data is not trusted."
        )
    data = payload.get("data")
    repository = data.get("repository") if isinstance(data, dict) else None
    pull_request = repository.get("pullRequest") if isinstance(repository, dict) else None
    if not isinstance(pull_request, dict):
        return merge_queue_read_error(
            "GitHub merge-queue GraphQL response did not contain repository.pullRequest."
        )
    return normalize_merge_queue_entry(
        pull_request.get("mergeQueueEntry"), field_present="mergeQueueEntry" in pull_request
    )


def merge_queue_terminal_tombstone(queue, pr_head_sha):
    return {
        "pr_head_sha": str(pr_head_sha or ""),
        "status": str(queue.get("status") or "unknown"),
        "id": str(queue.get("id") or ""),
        "state": str(queue.get("state") or ""),
        "position": queue.get("position"),
        "head_sha": str(queue.get("head_sha") or ""),
    }


def queue_from_terminal_tombstone(tombstone):
    status = str(tombstone.get("status") or "unknown")
    return {
        "read_state": "persisted_terminal",
        "status": status,
        "id": str(tombstone.get("id") or ""),
        "state": str(tombstone.get("state") or ""),
        "position": tombstone.get("position"),
        "head_sha": str(tombstone.get("head_sha") or ""),
        "source": "watcher_state",
        "details": (
            "A terminal merge-queue outcome remains authoritative for this unchanged PR head."
        ),
    }


def clear_merge_queue_tracking(state):
    state.pop("last_merge_queue_entry", None)
    state.pop("last_merge_queue_pr_head_sha", None)


def reconcile_merge_queue_entry(pr, state):
    """Carry queue identity and terminal outcomes across watcher restarts per PR head.

    A fresh active entry may replace a terminal tombstone only when GitHub gives it a
    distinct nonempty entry ID. Same-ID active evidence remains tombstoned: it may be
    stale or contradictory, and must not create a false-ready path.
    """
    queue = pr.get("merge_queue") or normalize_merge_queue_entry(None, field_present=False)
    pr_head_sha = str(pr.get("head_sha") or "")
    if pr.get("merged") or pr.get("closed"):
        # A confirmed PR lifecycle transition invalidates all queue continuity.
        state.pop("merge_queue_terminal_tombstone", None)
        clear_merge_queue_tracking(state)
        return queue

    tombstone = state.get("merge_queue_terminal_tombstone")
    if isinstance(tombstone, dict):
        tombstone_head_sha = str(tombstone.get("pr_head_sha") or "")
        if tombstone_head_sha != pr_head_sha:
            state.pop("merge_queue_terminal_tombstone", None)
        elif (
            queue.get("status") == "waiting"
            and queue.get("read_state") == "observed"
            and queue.get("source") == "github"
            and str(queue.get("id") or "")
            and str(queue.get("id") or "") != str(tombstone.get("id") or "")
        ):
            # GitHub has authoritatively assigned a new queue entry to this same PR head.
            state.pop("merge_queue_terminal_tombstone", None)
            state["last_merge_queue_entry"] = dict(queue)
            state["last_merge_queue_pr_head_sha"] = pr_head_sha
            return queue
        else:
            return queue_from_terminal_tombstone(tombstone)

    previous = state.get("last_merge_queue_entry")
    same_pr_head = (
        isinstance(previous, dict)
        and str(state.get("last_merge_queue_pr_head_sha") or "") == pr_head_sha
    )

    if queue["status"] == "waiting":
        state["last_merge_queue_entry"] = dict(queue)
        state["last_merge_queue_pr_head_sha"] = pr_head_sha
        return queue

    if queue["status"] == "failed":
        state["merge_queue_terminal_tombstone"] = merge_queue_terminal_tombstone(
            queue, pr_head_sha
        )
        clear_merge_queue_tracking(state)
        return queue

    if queue["status"] == "absent" and same_pr_head:
        removed_queue = {
            **previous,
            "read_state": "observed_absent",
            "status": "removed",
            "source": "watcher_state",
            "details": "The previously observed merge-queue entry is no longer present for this PR head.",
        }
        state["merge_queue_terminal_tombstone"] = merge_queue_terminal_tombstone(
            removed_queue, pr_head_sha
        )
        clear_merge_queue_tracking(state)
        return removed_queue

    if queue["status"] == "unknown" and same_pr_head:
        return {
            **previous,
            "read_state": "error",
            "status": "unknown",
            "source": "watcher_state",
            "details": "GitHub merge-queue evidence could not be read; retaining the last known queue identity for this unchanged PR head.",
        }

    if queue["status"] in {"absent", "failed"}:
        clear_merge_queue_tracking(state)
    return queue


def apply_no_checks_policy(pr, checks):
    if len(checks or []) == 1 and is_no_checks_reported_item(checks[0]) and is_clean_mergeable_pr(pr):
        return [], {
            "state": "clean_mergeable_no_checks",
            "message": "No GitHub checks are attached and the PR is CLEAN/MERGEABLE; treating checks as terminal by explicit watcher policy.",
        }
    return checks, {"state": "not_applicable", "message": ""}


def resolve_pr(pr_spec, repo_override=None):
    parsed = parse_pr_spec(pr_spec)
    cmd = ["pr", "view"]
    if parsed["value"] is not None:
        cmd.append(parsed["value"])
    cmd.extend(["--json", pr_view_fields()])
    try:
        data = gh_json(cmd, repo=repo_override)
    except GhCommandError as err:
        if "baseRefOid" in str(err) and "baseRefOid" in cmd[-1]:
            cmd[-1] = ",".join(field for field in cmd[-1].split(",") if field != "baseRefOid")
            data = gh_json(cmd, repo=repo_override)
        elif parsed["mode"] in {"auto", "number"} and not repo_override:
            raise GhCommandError(
                f"{err}\nHint: use a full PR URL or --repo to disambiguate repo/worktree context."
            ) from err
        else:
            raise
    if not isinstance(data, dict):
        raise GhCommandError("Unexpected PR payload from `gh pr view`")

    pr_url = str(data.get("url") or "")
    base_repo = extract_repo_from_pr_url(pr_url)
    head_repo = extract_repo_from_pr_view(data)
    if not base_repo:
        raise GhCommandError(
            "Resolved PR payload is missing a canonical base repository URL."
        )
    if repo_override and base_repo and not repos_match(repo_override, base_repo):
        raise GhCommandError(
            f"Resolved PR base repo {base_repo} does not match requested repo "
            f"{repo_override}."
        )
    repo = (
        repo_override
        or base_repo
        or head_repo
    )
    if not repo:
        raise GhCommandError("Unable to determine OWNER/REPO for the PR")

    state = str(data.get("state") or "")
    merged = bool(data.get("mergedAt"))
    closed = bool(data.get("closedAt")) or state.upper() == "CLOSED"

    pr = {
        "number": int(data["number"]),
        "url": pr_url,
        "repo": repo,
        "base_repo": base_repo or repo,
        "head_sha": str(data.get("headRefOid") or ""),
        "head_branch": str(data.get("headRefName") or ""),
        "head_repo": head_repo or repo,
        "base_branch": str(data.get("baseRefName") or ""),
        "base_sha": str(data.get("baseRefOid") or ""),
        "state": state,
        "merged": merged,
        "closed": closed,
        "mergeable": str(data.get("mergeable") or ""),
        "merge_state_status": str(data.get("mergeStateStatus") or ""),
        "review_decision": str(data.get("reviewDecision") or ""),
    }
    pr["merge_queue"] = get_merge_queue_entry(pr["repo"], pr["number"])
    return pr


def extract_repo_slug(repo_data, owner_data=None):
    if isinstance(repo_data, str) and "/" in repo_data:
        return repo_data

    owner = None
    if isinstance(owner_data, dict):
        owner = owner_data.get("login") or owner_data.get("name")
    elif isinstance(owner_data, str):
        owner = owner_data

    repo_name = None
    if isinstance(repo_data, dict):
        slug = repo_data.get("nameWithOwner") or repo_data.get("fullName")
        if isinstance(slug, str) and "/" in slug:
            return slug
        repo_name = repo_data.get("name")
        repo_owner = repo_data.get("owner")
        if not owner and isinstance(repo_owner, dict):
            owner = repo_owner.get("login") or repo_owner.get("name")
        elif not owner and isinstance(repo_owner, str):
            owner = repo_owner
    elif isinstance(repo_data, str):
        repo_name = repo_data

    if owner and repo_name:
        return f"{owner}/{repo_name}"
    return None


def extract_repo_from_pr_view(data):
    return extract_repo_slug(data.get("headRepository"), data.get("headRepositoryOwner"))


def extract_repo_from_pr_url(pr_url):
    parsed = urlparse(pr_url)
    parts = [p for p in parsed.path.split("/") if p]
    if len(parts) >= 4 and parts[2] == "pull":
        return f"{parts[0]}/{parts[1]}"
    return None


def command_text(cmd):
    try:
        proc = subprocess.run(cmd, check=True, capture_output=True, text=True)
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None
    output = (proc.stdout or "").strip()
    return output or None


def parse_repo_from_remote_url(remote_url):
    if not remote_url:
        return None

    if remote_url.startswith("git@"):
        _, _, path = remote_url.partition(":")
    else:
        parsed = urlparse(remote_url)
        path = parsed.path

    parts = [part for part in path.split("/") if part]
    if len(parts) < 2:
        return None

    owner = parts[-2]
    repo = parts[-1]
    if repo.endswith(".git"):
        repo = repo[:-4]
    if not owner or not repo:
        return None
    return f"{owner}/{repo}"


def repos_match(left, right):
    return bool(left and right and left.casefold() == right.casefold())


def detect_local_git_context():
    git_root = command_text(["git", "rev-parse", "--show-toplevel"])
    origin_url = command_text(["git", "config", "--get", "remote.origin.url"])
    upstream_url = command_text(["git", "config", "--get", "remote.upstream.url"])
    return {
        "cwd": str(Path.cwd()),
        "git_root": git_root or "",
        "origin_url": origin_url or "",
        "origin_repo": parse_repo_from_remote_url(origin_url) or "",
        "upstream_url": upstream_url or "",
        "upstream_repo": parse_repo_from_remote_url(upstream_url) or "",
    }


def validate_pr_resolution(pr_spec, repo_override, pr, local_git_context):
    parsed = parse_pr_spec(pr_spec)
    url_repo = extract_repo_from_pr_url(pr_spec) if parsed["mode"] == "url" else None
    if repo_override and url_repo and not repos_match(repo_override, url_repo):
        raise GhCommandError(
            f"PR URL repo {url_repo} contradicts --repo {repo_override}."
        )
    expected_repo = repo_override or url_repo
    resolved_base_repo = str(pr.get("base_repo") or "")
    if expected_repo and resolved_base_repo and not repos_match(
        resolved_base_repo, expected_repo
    ):
        raise GhCommandError(
            f"Resolved PR base repo {resolved_base_repo} does not match requested "
            f"repo {expected_repo}."
        )
    if expected_repo and not repos_match(pr["repo"], expected_repo):
        raise GhCommandError(
            f"Resolved PR repo {pr['repo']} does not match requested repo {expected_repo}."
        )

    local_origin_repo = str(local_git_context.get("origin_repo") or "")
    if expected_repo or not local_origin_repo:
        return
    if repos_match(pr["repo"], local_origin_repo):
        return
    raise GhCommandError(
        f"Resolved PR repo {pr['repo']} does not match local origin {local_origin_repo}. "
        "Use a full PR URL or --repo to disambiguate repo/worktree context."
    )


def build_watch_context(args, pr, local_git_context):
    parsed = parse_pr_spec(args.pr)
    return {
        "cwd": str(local_git_context.get("cwd") or str(Path.cwd())),
        "git_root": str(local_git_context.get("git_root") or ""),
        "origin_repo": str(local_git_context.get("origin_repo") or ""),
        "origin_url": str(local_git_context.get("origin_url") or ""),
        "upstream_repo": str(local_git_context.get("upstream_repo") or ""),
        "upstream_url": str(local_git_context.get("upstream_url") or ""),
        "pr_input": args.pr,
        "pr_input_mode": parsed["mode"],
        "repo_override": str(args.repo or ""),
        "resolved_repo": pr["repo"],
        "resolved_repo_matches_origin": repos_match(pr["repo"], local_git_context.get("origin_repo")),
        "resolution_note": (
            "Auto resolution depends on the current git/gh repository context; explicit targets should use a full PR URL or --repo."
            if parsed["mode"] == "auto" and not args.repo
            else ""
        ),
    }


def load_state(path):
    if path.exists():
        try:
            data = json.loads(path.read_text())
        except json.JSONDecodeError as err:
            raise RuntimeError(f"State file is not valid JSON: {path}") from err
        if not isinstance(data, dict):
            raise RuntimeError(f"State file must contain an object: {path}")
        return data, False
    return {
        "pr": {},
        "started_at": None,
        "last_seen_head_sha": None,
        "retries_by_sha": {},
        "seen_issue_comment_ids": [],
        "seen_review_comment_ids": [],
        "seen_review_ids": [],
        "last_snapshot_at": None,
    }, True


def save_state(path, state):
    state_dir = Path(tempfile.gettempdir())
    state_dir.mkdir(parents=True, exist_ok=True)
    path = state_dir / safe_state_file_name(path.name)
    payload = json.dumps(state, indent=2, sort_keys=True) + "\n"
    fd, tmp_name = tempfile.mkstemp(
        prefix="codex-babysit-pr-state.", suffix=".tmp", dir=state_dir
    )
    tmp_path = Path(tmp_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as tmp_file:
            tmp_file.write(payload)
        os.replace(tmp_path, path)
    except Exception:
        try:
            tmp_path.unlink(missing_ok=True)
        except OSError:
            pass
        raise


STATE_FILE_NAME_RE = re.compile(r"[A-Za-z0-9_.-]+")


def safe_state_file_name(name):
    base_name = os.path.basename(name)
    if base_name != name or base_name in {".", ".."}:
        raise RuntimeError("--state-file must be a file name, not a path")
    if not STATE_FILE_NAME_RE.fullmatch(base_name):
        raise RuntimeError(
            "--state-file may contain only letters, numbers, '.', '_', and '-'"
        )
    return base_name


def default_state_file_for(pr):
    repo_slug = pr["repo"].replace("/", "-")
    file_name = safe_state_file_name(
        f"codex-babysit-pr-{repo_slug}-pr{pr['number']}.json"
    )
    return Path(tempfile.gettempdir()) / file_name


def state_file_for(args, pr):
    if args.state_file:
        return Path(tempfile.gettempdir()) / safe_state_file_name(args.state_file)
    return default_state_file_for(pr)


def reset_seen_feedback_state(state):
    for key in SEEN_FEEDBACK_STATE_KEYS:
        state[key] = []


def maybe_reset_seen_feedback(args, state):
    if not args.reset_seen_feedback:
        return
    if getattr(args, "_seen_feedback_reset_done", False):
        return
    reset_seen_feedback_state(state)
    args._seen_feedback_reset_done = True



def _normalize_status_rollup_check(item):
    if not isinstance(item, dict):
        raise GhCommandError("Malformed status-check rollup entry")
    typename = str(item.get("__typename") or "").strip()
    if typename and typename not in {"CheckRun", "StatusContext"}:
        raise GhCommandError("Unknown status-check rollup entry shape")
    has_check_run_keys = any(key in item for key in ("name", "status", "conclusion"))
    has_status_context_keys = any(key in item for key in ("context", "state"))
    if not typename and has_check_run_keys == has_status_context_keys:
        raise GhCommandError("Ambiguous status-check rollup entry shape")
    is_check_run = typename == "CheckRun" or (not typename and has_check_run_keys)
    if is_check_run:
        name = str(item.get("name") or "").strip()
        status = str(item.get("status") or "").upper()
        conclusion_value = item.get("conclusion")
        conclusion = str(conclusion_value or "").upper()
        if not name or status not in {
            "REQUESTED", "QUEUED", "IN_PROGRESS", "COMPLETED", "WAITING", "PENDING",
        }:
            raise GhCommandError("Malformed CheckRun status-check rollup entry")
        if status == "COMPLETED" and conclusion not in {
            "ACTION_REQUIRED", "TIMED_OUT", "CANCELLED", "FAILURE", "SUCCESS",
            "NEUTRAL", "SKIPPED", "STARTUP_FAILURE", "STALE",
        }:
            raise GhCommandError("Malformed completed CheckRun status-check rollup entry")
        if status != "COMPLETED" and conclusion_value is not None:
            raise GhCommandError("Malformed pending CheckRun status-check rollup entry")
        state = conclusion or status
        bucket = "pending" if status != "COMPLETED" else (
            "pass" if conclusion in {"SUCCESS", "NEUTRAL", "SKIPPED"} else "fail"
        )
    else:
        name = str(item.get("context") or "").strip()
        state = str(item.get("state") or "").upper()
        if not name or state not in {"EXPECTED", "ERROR", "FAILURE", "PENDING", "SUCCESS"}:
            raise GhCommandError("Malformed StatusContext status-check rollup entry")
        if item.get("conclusion") not in (None, ""):
            raise GhCommandError("Malformed StatusContext status-check rollup entry")
        bucket = "pending" if state in {"EXPECTED", "PENDING"} else "pass" if state == "SUCCESS" else "fail"
    return {
        "name": name,
        "state": state,
        "bucket": bucket,
        "link": str(item.get("detailsUrl") or item.get("targetUrl") or ""),
        "workflow": str(item.get("workflowName") or ""),
        "event": "",
        "startedAt": str(item.get("startedAt") or ""),
        "completedAt": str(item.get("completedAt") or ""),
    }

def get_pr_checks(pr_spec, repo):
    parsed = parse_pr_spec(pr_spec)
    cmd = ["pr", "checks"]
    if parsed["value"] is not None:
        cmd.append(parsed["value"])
    cmd.extend(["--json", checks_fields()])
    try:
        data = gh_json(cmd, repo=repo)
    except GhCommandError as err:
        if is_no_checks_reported_error(err):
            return [pending_checks_not_reported_item()]
        if "unknown flag: --json" not in str(err).lower() and "unknown json field" not in str(err).lower():
            raise
        rollup_cmd = ["pr", "view"]
        if parsed["value"] is not None:
            rollup_cmd.append(parsed["value"])
        rollup_cmd.extend(["--json", "statusCheckRollup"])
        rollup = gh_json(rollup_cmd, repo=repo)
        if not isinstance(rollup, dict) or not isinstance(rollup.get("statusCheckRollup"), list):
            raise GhCommandError("Unexpected statusCheckRollup payload from gh pr view")
        data = [_normalize_status_rollup_check(item) for item in rollup["statusCheckRollup"]]
    if data is None:
        return []
    if not isinstance(data, list):
        raise GhCommandError("Unexpected payload from `gh pr checks`")
    return data


def is_pending_check(check):
    bucket = str(check.get("bucket") or "").lower()
    state = str(check.get("state") or check.get("status") or "").upper()
    return bucket == "pending" or state in PENDING_CHECK_STATES


def summarize_checks(checks):
    pending_count = 0
    failed_count = 0
    passed_count = 0
    for check in checks:
        bucket = str(check.get("bucket") or "").lower()
        conclusion = str(check.get("conclusion") or "").lower()
        state = str(check.get("state") or check.get("status") or "").upper()
        if is_pending_check(check):
            pending_count += 1
        if bucket == "fail" or conclusion in FAILED_RUN_CONCLUSIONS or state in FAILED_CHECK_STATES:
            failed_count += 1
        if bucket == "pass" or conclusion == "success":
            passed_count += 1
    return {
        "pending_count": pending_count,
        "failed_count": failed_count,
        "passed_count": passed_count,
        "all_terminal": pending_count == 0,
    }


def summarize_check_runs(check_runs):
    summary = summarize_checks(check_runs)
    summary["total_count"] = len(check_runs or [])
    return summary


def get_workflow_runs_for_sha(repo, head_sha):
    endpoint = f"repos/{repo}/actions/runs"
    data = gh_json(
        ["api", endpoint, "-X", "GET", "-f", f"head_sha={head_sha}", "-f", "per_page=100"],
        repo=repo,
    )
    if not isinstance(data, dict):
        raise GhCommandError("Unexpected payload from actions runs API")
    runs = data.get("workflow_runs") or []
    if not isinstance(runs, list):
        raise GhCommandError("Expected `workflow_runs` to be a list")
    return runs


def failed_jobs_for_run(run_id, repo):
    endpoint = f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"
    data = gh_json(["api", endpoint], repo=repo)
    if not isinstance(data, dict):
        raise GhCommandError("Unexpected payload from workflow jobs API")
    jobs = data.get("jobs") or []
    if not isinstance(jobs, list):
        raise GhCommandError("Expected `jobs` to be a list")

    failed_jobs = []
    for job in jobs:
        if not isinstance(job, dict):
            continue
        conclusion = str(job.get("conclusion") or "")
        if conclusion not in FAILED_RUN_CONCLUSIONS:
            continue
        runner_name = str(job.get("runner_name") or "")
        steps = job.get("steps") or []
        startup_failure = None
        if not runner_name and not steps and job.get("id"):
            annotations = []
            try:
                payload = gh_json(
                    [
                        "api",
                        f"repos/{repo}/check-runs/{job['id']}/annotations?per_page=10",
                    ],
                    repo=repo,
                )
                if isinstance(payload, list):
                    annotations = [
                        {
                            "annotation_level": str(item.get("annotation_level") or ""),
                            "message": str(item.get("message") or ""),
                        }
                        for item in payload
                        if isinstance(item, dict) and str(item.get("message") or "")
                    ]
            except GhCommandError:
                annotations = []
            normalized_messages = " ".join(
                item["message"].casefold() for item in annotations
            )
            category = "runner_not_started_unclassified"
            if (
                "recent account payments have failed" in normalized_messages
                or "spending limit needs to be increased" in normalized_messages
            ):
                category = "github_billing_or_spending_limit"
            elif "job was not started" in normalized_messages:
                category = "github_runner_startup"
            startup_failure = {
                "category": category,
                "runner_name": runner_name,
                "step_count": len(steps),
                "annotations": annotations[:3],
            }
        failed_jobs.append(
            {
                "job_id": job.get("id"),
                "job_name": str(job.get("name") or ""),
                "status": str(job.get("status") or ""),
                "conclusion": conclusion,
                "html_url": str(job.get("html_url") or ""),
                "startup_failure": startup_failure,
                "logs_endpoint": f"repos/{repo}/actions/jobs/{job['id']}/logs" if job.get("id") else None,
            }
        )
    return failed_jobs


def failed_runs_from_workflow_runs(runs, head_sha, repo=None, cache=None):
    failed_runs = []
    jobs_cache = cache if cache is not None else {}
    for run in runs:
        if not isinstance(run, dict):
            continue
        if str(run.get("head_sha") or "") != head_sha:
            continue
        conclusion = str(run.get("conclusion") or "")
        completed = str(run.get("status") or "").lower() == "completed"
        if completed and conclusion not in FAILED_RUN_CONCLUSIONS:
            continue
        attempt = run.get("run_attempt")
        cache_key = (repo, str(run.get("id")), head_sha, attempt)
        reusable = completed and isinstance(attempt, int) and not isinstance(attempt, bool)
        if reusable and cache_key in jobs_cache:
            failed_jobs = jobs_cache[cache_key]
        else:
            failed_jobs = failed_jobs_for_run(run.get("id"), repo) if repo and run.get("id") else []
            if reusable:
                jobs_cache[cache_key] = failed_jobs
        if conclusion not in FAILED_RUN_CONCLUSIONS and not failed_jobs:
            continue
        failed_runs.append(
            {
                "run_id": run.get("id"),
                "workflow_name": run.get("name") or run.get("display_title") or "",
                "status": str(run.get("status") or ""),
                "conclusion": conclusion,
                "html_url": str(run.get("html_url") or ""),
                "failed_jobs": failed_jobs,
                "first_failed_job": failed_jobs[0] if failed_jobs else None,
            }
        )
    failed_runs.sort(key=lambda item: (str(item.get("workflow_name") or ""), str(item.get("run_id") or "")))
    return failed_runs


def startup_blockers_from_failed_runs(failed_runs):
    blockers = []
    for run in failed_runs or []:
        if not isinstance(run, dict):
            continue
        for job in run.get("failed_jobs") or []:
            if not isinstance(job, dict) or not job.get("startup_failure"):
                continue
            blockers.append(
                {
                    "run_id": run.get("run_id"),
                    "workflow_name": str(run.get("workflow_name") or ""),
                    "job_id": job.get("job_id"),
                    "job_name": str(job.get("job_name") or ""),
                    "startup_failure": job.get("startup_failure"),
                }
            )
    return blockers


def get_authenticated_login():
    data = gh_json(["api", "user"])
    if not isinstance(data, dict) or not data.get("login"):
        raise GhCommandError("Unable to determine authenticated GitHub login from `gh api user`")
    return str(data["login"])


def comment_endpoints(repo, pr_number):
    return {
        "issue_comment": f"repos/{repo}/issues/{pr_number}/comments",
        "review_comment": f"repos/{repo}/pulls/{pr_number}/comments",
        "review": f"repos/{repo}/pulls/{pr_number}/reviews",
    }


def gh_api_list_paginated(endpoint, repo=None, per_page=100):
    items = []
    page = 1
    while True:
        sep = "&" if "?" in endpoint else "?"
        page_endpoint = f"{endpoint}{sep}per_page={per_page}&page={page}"
        payload = gh_json(["api", page_endpoint], repo=repo)
        if payload is None:
            break
        if not isinstance(payload, list):
            raise GhCommandError(f"Unexpected paginated payload from gh api {endpoint}")
        items.extend(payload)
        if len(payload) < per_page:
            break
        page += 1
    return items


def normalize_issue_comments(items):
    out = []
    for item in items:
        if not isinstance(item, dict):
            continue
        out.append(
            {
                "kind": "issue_comment",
                "id": str(item.get("id") or ""),
                "author": extract_login(item.get("user")),
                "author_association": str(item.get("author_association") or ""),
                "created_at": str(item.get("created_at") or ""),
                "body": str(item.get("body") or ""),
                "path": None,
                "line": None,
                "url": str(item.get("html_url") or ""),
            }
        )
    return out


def normalize_review_comments(items, review_states=None):
    review_states = review_states or {}
    out = []
    for item in items:
        if not isinstance(item, dict):
            continue
        if review_states.get(str(item.get("pull_request_review_id") or "")) == "PENDING":
            continue
        line = item.get("line")
        if line is None:
            line = item.get("original_line")
        out.append(
            {
                "kind": "review_comment",
                "id": str(item.get("id") or ""),
                "author": extract_login(item.get("user")),
                "author_association": str(item.get("author_association") or ""),
                "created_at": str(item.get("created_at") or ""),
                "body": str(item.get("body") or ""),
                "path": item.get("path"),
                "line": line,
                "url": str(item.get("html_url") or ""),
            }
        )
    return out


def normalize_reviews(items):
    out = []
    for item in items:
        if not isinstance(item, dict):
            continue
        if str(item.get("state") or "").upper() == "PENDING":
            continue
        out.append(
            {
                "kind": "review",
                "id": str(item.get("id") or ""),
                "author": extract_login(item.get("user")),
                "author_association": str(item.get("author_association") or ""),
                "created_at": str(item.get("submitted_at") or item.get("created_at") or ""),
                "body": str(item.get("body") or ""),
                "state": str(item.get("state") or ""),
                "commit_id": str(item.get("commit_id") or ""),
                "path": None,
                "line": None,
                "url": str(item.get("html_url") or ""),
            }
        )
    return out


def extract_login(user_obj):
    if isinstance(user_obj, dict):
        return str(user_obj.get("login") or "")
    return ""


def is_bot_login(login):
    return bool(login) and login.endswith("[bot]")


def is_actionable_review_bot_login(login):
    if not is_bot_login(login):
        return False
    lower_login = login.lower()
    return any(keyword in lower_login for keyword in REVIEW_BOT_LOGIN_KEYWORDS)


def is_trusted_human_review_author(item, authenticated_login):
    author = str(item.get("author") or "")
    if not author:
        return False
    if authenticated_login and author == authenticated_login:
        return True
    association = str(item.get("author_association") or "").upper()
    return association in TRUSTED_AUTHOR_ASSOCIATIONS


def fetch_new_review_items(pr, state, fresh_state, authenticated_login=None, include_review_items=False):
    repo = pr["repo"]
    pr_number = pr["number"]
    endpoints = comment_endpoints(repo, pr_number)

    issue_payload = gh_api_list_paginated(endpoints["issue_comment"], repo=repo)
    review_comment_payload = gh_api_list_paginated(endpoints["review_comment"], repo=repo)
    review_payload = gh_api_list_paginated(endpoints["review"], repo=repo)

    issue_items = normalize_issue_comments(issue_payload)
    review_states = {
        str(item.get("id")): str(item.get("state") or "").upper()
        for item in review_payload
        if isinstance(item, dict) and item.get("id") not in (None, "")
    }
    pending_review_ids = {
        review_id for review_id, review_state in review_states.items() if review_state == "PENDING"
    }
    pending_review_comment_ids = {
        str(item.get("id"))
        for item in review_comment_payload
        if isinstance(item, dict)
        and item.get("id") not in (None, "")
        and str(item.get("pull_request_review_id") or "") in pending_review_ids
    }
    review_comment_items = normalize_review_comments(review_comment_payload, review_states)
    review_items = normalize_reviews(review_payload)
    all_items = issue_items + review_comment_items + review_items

    seen_issue = {str(x) for x in state.get("seen_issue_comment_ids") or []}
    seen_review_comment = {str(x) for x in state.get("seen_review_comment_ids") or []}
    seen_review = {str(x) for x in state.get("seen_review_ids") or []}
    seen_review_comment.difference_update(pending_review_comment_ids)
    seen_review.difference_update(pending_review_ids)

    # On a brand-new state file, surface existing review activity instead of
    # silently treating it as seen. This avoids missing already-pending review
    # feedback when monitoring starts after comments were posted.

    new_items = []
    for item in all_items:
        item_id = item.get("id")
        if not item_id:
            continue
        author = item.get("author") or ""
        if not author:
            continue
        if is_bot_login(author):
            if not is_actionable_review_bot_login(author):
                continue
        elif not is_trusted_human_review_author(item, authenticated_login):
            continue

        kind = item["kind"]
        if kind == "issue_comment" and not is_meaningful_issue_comment(item):
            continue
        if kind == "issue_comment" and item_id in seen_issue:
            continue
        if kind == "review_comment" and item_id in seen_review_comment:
            continue
        if kind == "review" and item_id in seen_review:
            continue

        new_items.append(item)
        if kind == "issue_comment":
            seen_issue.add(item_id)
        elif kind == "review_comment":
            seen_review_comment.add(item_id)
        elif kind == "review":
            seen_review.add(item_id)

    new_items.sort(key=lambda item: (item.get("created_at") or "", item.get("kind") or "", item.get("id") or ""))
    state["seen_issue_comment_ids"] = sorted(seen_issue)
    state["seen_review_comment_ids"] = sorted(seen_review_comment)
    state["seen_review_ids"] = sorted(seen_review)
    if include_review_items:
        return new_items, review_items
    return new_items


def summarize_review_submissions(review_items, current_head_sha="", max_items=3):
    all_reviews = []
    blocking_reviews = []
    for item in review_items:
        if not isinstance(item, dict):
            continue
        if item.get("kind") != "review":
            continue
        if not review_submission_applies_to_current_head(current_head_sha, item):
            continue
        if not is_meaningful_review_submission(item):
            continue
        normalized = {
            "kind": "review",
            "id": item.get("id") or "",
            "author": item.get("author") or "",
            "state": item.get("state") or "",
            "created_at": item.get("created_at") or "",
            "body": body_excerpt(item.get("body") or ""),
            "url": item.get("url") or "",
        }
        all_reviews.append(normalized)
        if is_review_submission_blocking(item.get("state")):
            blocking_reviews.append(normalized)

    all_reviews.sort(key=lambda item: (item.get("created_at") or "", item.get("id") or ""))
    blocking_reviews.sort(key=lambda item: (item.get("created_at") or "", item.get("id") or ""))

    return {
        "meaningful_review_count": len(all_reviews),
        "blocking_review_count": len(blocking_reviews),
        "latest_reviews": all_reviews[-max_items:],
        "blocking_reviews": blocking_reviews[-max_items:],
    }


REVIEW_THREADS_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          comments(first: 100) {
            nodes {
              databaseId
              url
              body
              createdAt
              author {
                login
              }
              pullRequestReview {
                databaseId
                url
                state
                author {
                  login
                }
              }
            }
          }
        }
      }
    }
  }
}
""".strip()


def body_excerpt(text, limit=200):
    normalized = " ".join(str(text or "").split())
    if len(normalized) <= limit:
        return normalized
    return normalized[: limit - 3].rstrip() + "..."


def get_review_threads(pr):
    owner, repo_name = pr["repo"].split("/", 1)
    threads = []
    cursor = None

    while True:
        cmd = [
            "api",
            "graphql",
            "-f",
            f"query={REVIEW_THREADS_QUERY}",
            "-F",
            f"owner={owner}",
            "-F",
            f"name={repo_name}",
            "-F",
            f"number={pr['number']}",
        ]
        if cursor:
            cmd.extend(["-F", f"cursor={cursor}"])

        payload = gh_json(cmd)
        if not isinstance(payload, dict):
            raise GhCommandError("Unexpected payload from reviewThreads GraphQL query")
        if payload.get("errors"):
            messages = []
            for error in payload.get("errors") or []:
                if isinstance(error, dict):
                    message = str(error.get("message") or "").strip()
                    if message:
                        messages.append(message)
            detail = "; ".join(messages) or "unknown GraphQL error"
            raise GhCommandError(f"reviewThreads GraphQL query failed: {detail}")

        review_threads = (
            payload.get("data", {})
            .get("repository", {})
            .get("pullRequest", {})
            .get("reviewThreads", {})
        )
        nodes = review_threads.get("nodes") or []
        if not isinstance(nodes, list):
            raise GhCommandError("Expected reviewThreads.nodes to be a list")
        threads.extend(normalize_review_threads(nodes))

        page_info = review_threads.get("pageInfo") or {}
        if not page_info.get("hasNextPage"):
            break
        cursor = page_info.get("endCursor")
        if not cursor:
            break

    return threads


def normalize_review_threads(items):
    out = []
    for item in items:
        if not isinstance(item, dict):
            continue

        comments_payload = item.get("comments", {})
        comment_nodes = comments_payload.get("nodes") or []
        comments = []
        for node in comment_nodes:
            if not isinstance(node, dict):
                continue
            review = node.get("pullRequestReview") or {}
            comments.append(
                {
                    "id": str(node.get("databaseId") or ""),
                    "url": str(node.get("url") or ""),
                    "body": str(node.get("body") or ""),
                    "body_excerpt": body_excerpt(node.get("body") or ""),
                    "created_at": str(node.get("createdAt") or ""),
                    "author": extract_login(node.get("author")),
                    "review_id": str(review.get("databaseId") or ""),
                    "review_url": str(review.get("url") or ""),
                    "review_state": str(review.get("state") or ""),
                    "review_author": extract_login(review.get("author")),
                }
            )

        comments.sort(key=lambda comment: (comment.get("created_at") or "", comment.get("id") or ""))
        latest_comment = comments[-1] if comments else {}
        out.append(
            {
                "kind": "review_thread",
                "id": str(item.get("id") or ""),
                "thread_id": str(item.get("id") or ""),
                "is_resolved": bool(item.get("isResolved")),
                "is_outdated": bool(item.get("isOutdated")),
                "author": str(latest_comment.get("author") or ""),
                "author_association": "",
                "created_at": str(latest_comment.get("created_at") or ""),
                "body": str(latest_comment.get("body_excerpt") or ""),
                "path": item.get("path"),
                "line": item.get("line"),
                "url": str(latest_comment.get("url") or ""),
                "latest_comment_id": str(latest_comment.get("id") or ""),
                "comment_ids": [comment["id"] for comment in comments if comment.get("id")],
                "comment_urls": [comment["url"] for comment in comments if comment.get("url")],
                "review_ids": [comment["review_id"] for comment in comments if comment.get("review_id")],
                "review_urls": [comment["review_url"] for comment in comments if comment.get("review_url")],
            }
        )
    return out


def normalize_ignore_review_thread(value):
    return str(value or "").strip().rstrip("/")


def thread_matches_ignore_value(thread, ignore_values):
    normalized_values = {
        normalize_ignore_review_thread(value) for value in ignore_values if normalize_ignore_review_thread(value)
    }
    if not normalized_values:
        return False

    candidates = {
        normalize_ignore_review_thread(thread.get("id") or ""),
        normalize_ignore_review_thread(thread.get("thread_id") or ""),
        normalize_ignore_review_thread(thread.get("url") or ""),
        normalize_ignore_review_thread(thread.get("latest_comment_id") or ""),
    }
    candidates.update(normalize_ignore_review_thread(value) for value in thread.get("comment_ids") or [])
    candidates.update(normalize_ignore_review_thread(value) for value in thread.get("comment_urls") or [])
    candidates.update(normalize_ignore_review_thread(value) for value in thread.get("review_ids") or [])
    candidates.update(normalize_ignore_review_thread(value) for value in thread.get("review_urls") or [])
    candidates.discard("")
    return bool(candidates & normalized_values)


def partition_unresolved_review_threads(review_threads, ignore_values):
    active = []
    ignored = []
    for thread in review_threads:
        if not isinstance(thread, dict):
            continue
        if thread.get("is_resolved"):
            continue
        if thread_matches_ignore_value(thread, ignore_values):
            ignored.append(thread)
        else:
            active.append(thread)
    return active, ignored


def is_review_submission_blocking(state):
    return str(state or "").upper() in REVIEW_SUBMISSION_BLOCKING_STATES


def build_review_blocker_summary(merge_decision):
    decision = str(merge_decision or "").upper()
    if decision in MERGE_BLOCKING_REVIEW_DECISIONS:
        return {
            "kind": "review_gate_not_satisfied",
            "value": decision,
            "details": "PR is waiting for review approval or change request resolution.",
        }
    return None


def build_merge_blockers(pr, checks_summary, check_details, review_state):
    blockers = []

    merge_queue = pr.get("merge_queue") or {}
    merge_queue_status = str(merge_queue.get("status") or "unknown")
    if merge_queue_status == "waiting":
        blockers.append(
            {
                "kind": "merge_queue_waiting",
                "state": str(merge_queue.get("state") or ""),
                "entry_id": str(merge_queue.get("id") or ""),
                "head_sha": str(merge_queue.get("head_sha") or ""),
                "details": "PR is actively in the GitHub merge queue; continue waiting for the queue outcome.",
            }
        )
    elif merge_queue_status == "failed":
        blockers.append(
            {
                "kind": "merge_queue_failed",
                "state": str(merge_queue.get("state") or ""),
                "entry_id": str(merge_queue.get("id") or ""),
                "head_sha": str(merge_queue.get("head_sha") or ""),
                "details": "GitHub reports that the merge-queue entry did not complete successfully.",
            }
        )
    elif merge_queue_status == "removed":
        blockers.append(
            {
                "kind": "merge_queue_removed",
                "entry_id": str(merge_queue.get("id") or ""),
                "head_sha": str(merge_queue.get("head_sha") or ""),
                "details": "A previously observed merge-queue entry disappeared before a merge receipt was observed.",
            }
        )
    elif merge_queue_status == "unknown":
        blockers.append(
            {
                "kind": "merge_queue_read_error",
                "entry_id": str(merge_queue.get("id") or ""),
                "head_sha": str(merge_queue.get("head_sha") or ""),
                "details": str(merge_queue.get("details") or "Merge-queue state is unknown."),
            }
        )

    pending_count = int(checks_summary.get("pending_count") or 0)
    if pending_count > 0:
        pending_checks = [item.get("name") for item in (check_details.get("pending", []) or [])]
        blockers.append(
            {
                "kind": "pending_checks",
                "count": pending_count,
                "examples": pending_checks[:3],
            }
        )

    failed_count = int(checks_summary.get("failed_count") or 0)
    if failed_count > 0:
        failing_checks = [item.get("name") for item in (check_details.get("failing", []) or [])]
        blockers.append(
            {
                "kind": "failing_checks",
                "count": failed_count,
                "examples": failing_checks[:3],
            }
        )

    active_unresolved_count = int(review_state.get("active_unresolved_thread_count") or 0)
    if active_unresolved_count > 0:
        blockers.append(
            {
                "kind": "unresolved_review_threads",
                "count": active_unresolved_count,
                "details": "Open inline review threads are blocking merge until addressed.",
            }
        )

    review_gate = build_review_blocker_summary(pr.get("review_decision"))
    if review_gate is not None:
        blockers.append(review_gate)

    blocking_review_submissions = int(review_state.get("blocking_top_level_review_submission_count") or 0)
    if blocking_review_submissions > 0:
        blockers.append(
            {
                "kind": "blocking_review_submissions",
                "count": blocking_review_submissions,
                "details": "Recent meaningful top-level review submissions still require action.",
            }
        )

    mergeable = str(pr.get("mergeable") or "")
    merge_state_status = str(pr.get("merge_state_status") or "")
    if mergeable != "MERGEABLE":
        blockers.append(
            {
                "kind": "merge_conflict_or_dirty_state",
                "field": "mergeable",
                "value": mergeable,
                "details": "PR is not currently mergeable.",
            }
        )
    elif merge_state_status.upper() in MERGE_CONFLICT_OR_BLOCKING_STATES:
        blockers.append(
            {
                "kind": "merge_conflict_or_dirty_state",
                "field": "merge_state_status",
                "value": merge_state_status,
                "details": "PR merge state indicates a conflict/dirty/blocking condition.",
            }
        )
    elif merge_state_status.upper() == "DRAFT":
        blockers.append(
            {
                "kind": "draft_pr",
                "field": "merge_state_status",
                "value": merge_state_status,
                "details": "PR is still a draft.",
            }
        )
    elif (
        merge_state_status.upper() == "BLOCKED"
        and not blockers
        and mergeable.upper() == "MERGEABLE"
        and checks_summary.get("all_terminal")
    ):
        blockers.append(
            {
                "kind": "merge_policy_blocked",
                "field": "merge_state_status",
                "value": merge_state_status,
                "details": "GitHub reports a merge-policy blocker not explained by the current check or review evidence.",
            }
        )

    return {
        "is_blocked_for_merge": len(blockers) > 0,
        "reasons": blockers,
        "reason_kinds": sorted({item.get("kind") or "" for item in blockers}),
    }


def has_merge_policy_blocker(snapshot):
    """Return whether GitHub exposed an unexplained merge-policy blocker."""
    merge_blockers = snapshot.get("merge_blockers") or {}
    return "merge_policy_blocked" in (merge_blockers.get("reason_kinds") or [])


def build_watch_decision(snapshot, recorded_at=None):
    """Build a compact, exact-head decision receipt safe to persist."""
    pr = snapshot.get("pr") or {}
    checks = snapshot.get("checks") or {}
    review_state = snapshot.get("review_state") or {}
    merge_blockers = snapshot.get("merge_blockers") or {}
    actions = [str(action) for action in snapshot.get("actions") or []]
    if recorded_at is None:
        recorded_at = int(time.time())
    return {
        "schema_version": 1,
        "recorded_at": int(recorded_at),
        "repo": str(pr.get("repo") or ""),
        "number": pr.get("number"),
        "head_sha": str(pr.get("head_sha") or ""),
        "decision": "action_required" if any(action != "idle" for action in actions) else "idle",
        "primary_action": actions[0] if actions else "idle",
        "actions": actions,
        "checks_source": str(snapshot.get("checks_source") or ""),
        "check_counts": {
            "total": int(checks.get("total_count") or 0),
            "passed": int(checks.get("passed_count") or 0),
            "failed": int(checks.get("failed_count") or 0),
            "pending": int(checks.get("pending_count") or 0),
        },
        "review_counts": {
            "active_unresolved": int(review_state.get("active_unresolved_thread_count") or 0),
            "ignored_unresolved": int(review_state.get("ignored_unresolved_thread_count") or 0),
            "blocking_submissions": int(
                review_state.get("blocking_top_level_review_submission_count") or 0
            ),
        },
        "merge_blocker_kinds": sorted(
            str(kind) for kind in merge_blockers.get("reason_kinds") or [] if str(kind)
        ),
    }


def persist_watch_schedule(state_path, snapshot, mode, next_poll_seconds, scheduled_at=None):
    """Persist the next exact-head wake without retaining raw provider output."""
    state, _ = load_state(state_path)
    pr = snapshot.get("pr") or {}
    if scheduled_at is None:
        scheduled_at = int(time.time())
    delay = max(int(next_poll_seconds), 0)
    state["watch_schedule"] = {
        "schema_version": 1,
        "mode": str(mode),
        "repo": str(pr.get("repo") or ""),
        "number": pr.get("number"),
        "head_sha": str(pr.get("head_sha") or ""),
        "poll_seconds": delay,
        "scheduled_at": int(scheduled_at),
        "wake_at": int(scheduled_at) + delay,
    }
    save_state(state_path, state)


def is_meaningful_issue_comment(item):
    body = str(item.get("body") or "").strip()
    if not body:
        return False
    normalized = " ".join(body.split()).casefold()
    if any(snippet in normalized for snippet in NON_ACTIONABLE_ISSUE_COMMENT_SNIPPETS):
        return False
    if "\n" in body:
        return True

    tokens = body.split()
    if len(tokens) > COMMAND_ONLY_ISSUE_COMMENT_MAX_TOKENS:
        return True

    first_token = tokens[0].casefold()
    if first_token.startswith("/"):
        return False
    return first_token not in {
        "@codex",
        "@gemini",
        "@chatgpt-codex-connector[bot]",
    }


def is_meaningful_review_submission(item):
    body = str(item.get("body") or "").strip()
    if not body:
        return False
    author = str(item.get("author") or "")
    normalized = " ".join(body.split()).casefold()
    if is_bot_login(author) and normalized.startswith("### codex review"):
        return False
    if is_bot_login(author) and (
        "no review comments" in normalized
        and NON_ACTIONABLE_REVIEW_NO_FEEDBACK_PATTERN.search(normalized)
    ):
        return False
    return True


def review_submission_applies_to_current_head(current_head_sha, item):
    author = str(item.get("author") or "")
    if not is_bot_login(author):
        return True
    review_commit_id = str(item.get("commit_id") or "").strip().casefold()
    current_head_sha = str(current_head_sha or "").strip().casefold()
    if not review_commit_id or not current_head_sha:
        return True
    return review_commit_id == current_head_sha


def review_comment_in_active_unresolved_thread(item, active_unresolved_threads):
    item_refs = {
        normalize_ignore_review_thread(item.get("id") or ""),
        normalize_ignore_review_thread(item.get("url") or ""),
    }
    item_refs.discard("")
    if not item_refs:
        return False

    for thread in active_unresolved_threads:
        if not isinstance(thread, dict):
            continue
        thread_refs = {
            normalize_ignore_review_thread(thread.get("latest_comment_id") or ""),
            normalize_ignore_review_thread(thread.get("url") or ""),
        }
        thread_refs.update(normalize_ignore_review_thread(value) for value in thread.get("comment_ids") or [])
        thread_refs.update(normalize_ignore_review_thread(value) for value in thread.get("comment_urls") or [])
        thread_refs.discard("")
        if item_refs & thread_refs:
            return True
    return False


def build_actionable_review_items(pr, new_review_items, active_unresolved_threads):
    actionable_items = []
    for item in new_review_items:
        kind = item.get("kind")
        if kind == "issue_comment" and is_meaningful_issue_comment(item):
            actionable_items.append(item)
        elif kind == "review_comment" and review_comment_in_active_unresolved_thread(
            item,
            active_unresolved_threads,
        ):
            actionable_items.append(item)
        elif (
            kind == "review"
            and review_submission_applies_to_current_head(pr.get("head_sha"), item)
            and is_meaningful_review_submission(item)
        ):
            actionable_items.append(item)

    actionable_items.extend(active_unresolved_threads)

    if (
        pr.get("review_decision") == "CHANGES_REQUESTED"
        and not any(item.get("kind") in {"review", "review_thread", "review_decision"} for item in actionable_items)
    ):
        actionable_items.append(
            {
                "kind": "review_decision",
                "id": "changes_requested",
                "author": "",
                "author_association": "",
                "created_at": "",
                "body": "GitHub reviewDecision is CHANGES_REQUESTED.",
                "path": None,
                "line": None,
                "url": pr["url"],
            }
        )

    actionable_items.sort(
        key=lambda item: (
            item.get("created_at") or "",
            item.get("kind") or "",
            item.get("id") or "",
        )
    )
    return actionable_items


def build_effective_ci_state(current_head_sha, checks_summary, ci_context):
    effective = dict(checks_summary or {})
    effective.setdefault("pending_count", 0)
    effective.setdefault("failed_count", 0)
    effective.setdefault("passed_count", 0)
    effective.setdefault("all_terminal", True)
    effective.setdefault("total_count", 0)

    stale_failed_runs = list(ci_context.get("stale_failed_runs") or [])
    stale_fallback = bool(ci_context.get("stale_fallback_active"))
    source = "current_head"
    message = ""

    if stale_fallback and not ci_context.get("current_head_checks_signal") and stale_failed_runs:
        effective["failed_count"] = len(stale_failed_runs)
        effective["pending_count"] = 0
        effective["all_terminal"] = True
        effective["total_count"] = max(int(effective.get("total_count") or 0), len(stale_failed_runs))
        source = "stale_fallback"
        stale_head_sha = str(ci_context.get("stale_head_sha") or "")
        message = (
            f"Current head {current_head_sha} has no check signal yet; using older failed checks "
            f"from {stale_head_sha or 'a previous head'} during the grace window."
        )

    return effective, source, stale_fallback, message, stale_failed_runs


def is_no_checks_reported_item(check):
    if not isinstance(check, dict):
        return False
    name = str(check.get("name") or "").strip().casefold()
    bucket = str(check.get("bucket") or "").strip().casefold()
    state = str(check.get("state") or check.get("status") or "").strip().upper()
    return "checks not reported" in name and bucket == "pending" and state == "PENDING"


def has_current_head_check_signal(checks, checks_summary, failed_runs):
    if failed_runs:
        return True
    checks = checks or []
    if checks and not (len(checks) == 1 and is_no_checks_reported_item(checks[0])):
        return True
    return int((checks_summary or {}).get("total_count") or 0) > 0 and not checks


def build_stale_check_details(stale_failed_runs):
    details = {"failing": [], "pending": []}
    for run in stale_failed_runs or []:
        if not isinstance(run, dict):
            continue
        details["failing"].append(
            {
                "name": str(run.get("workflow_name") or run.get("name") or run.get("run_id") or "stale failed run"),
                "state": str(run.get("conclusion") or ""),
                "bucket": "stale_fallback",
                "workflow": str(run.get("workflow_name") or ""),
                "link": str(run.get("html_url") or ""),
            }
        )
    return details


def build_ci_head_context(pr, state, checks, checks_summary, failed_runs, now=None):
    now = int(time.time() if now is None else now)
    current_head_sha = str(pr.get("head_sha") or "")
    previous_head_sha = str(state.get("last_seen_head_sha") or "")
    last_snapshot_at = state.get("last_snapshot_at")
    try:
        last_snapshot_at = int(last_snapshot_at)
    except (TypeError, ValueError):
        last_snapshot_at = None

    current_signal = has_current_head_check_signal(checks, checks_summary, failed_runs)
    age_seconds = None if last_snapshot_at is None else max(0, now - last_snapshot_at)
    stale_fallback_active = (
        bool(previous_head_sha)
        and previous_head_sha != current_head_sha
        and not current_signal
        and age_seconds is not None
        and age_seconds <= CURRENT_HEAD_CHECK_GRACE_SECONDS
    )
    stale_failed_runs = []
    if stale_fallback_active:
        stale_runs = get_workflow_runs_for_sha(pr["repo"], previous_head_sha)
        stale_failed_runs = failed_runs_from_workflow_runs(stale_runs, previous_head_sha, repo=pr["repo"])

    return {
        "current_head_sha": current_head_sha,
        "current_head_checks_signal": current_signal,
        "stale_head_sha": previous_head_sha if stale_fallback_active else "",
        "stale_fallback_active": stale_fallback_active,
        "stale_failed_runs": stale_failed_runs,
        "stale_head_age_seconds": age_seconds,
        "grace_seconds": CURRENT_HEAD_CHECK_GRACE_SECONDS,
    }


def current_retry_count(state, head_sha):
    retries = state.get("retries_by_sha") or {}
    value = retries.get(head_sha, 0)
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


def set_retry_count(state, head_sha, count):
    retries = state.get("retries_by_sha")
    if not isinstance(retries, dict):
        retries = {}
    retries[head_sha] = int(count)
    state["retries_by_sha"] = retries


def reserve_retry_budget(state_path, head_sha, max_retries):
    """Durably consume one retry cycle before the first provider mutation."""
    state, _ = load_state(state_path)
    retries_used = current_retry_count(state, head_sha)
    if retries_used >= max_retries:
        return None
    new_count = retries_used + 1
    set_retry_count(state, head_sha, new_count)
    state["last_snapshot_at"] = int(time.time())
    save_state(state_path, state)
    return {"before": retries_used, "after": new_count}


def normalize_run_id(value):
    """Return a canonical numeric workflow-run id or None for invalid input."""
    if isinstance(value, bool) or value is None:
        return None
    normalized = str(value).strip()
    if not normalized or not normalized.isdigit():
        return None
    return normalized


def canonical_run_ids(values):
    """Deduplicate and numerically sort exact workflow-run ids."""
    normalized = []
    for value in values or []:
        run_id = normalize_run_id(value)
        if run_id is None:
            return None
        if run_id not in normalized:
            normalized.append(run_id)
    return sorted(normalized, key=lambda value: (int(value), value))


def retry_action_fingerprint(
    repo,
    pr_number,
    expected_head_sha,
    selected_run_ids,
    max_retries,
):
    """Bind all retry authorization inputs into a stable, content-addressed id."""
    binding = {
        "operation": "retry-failed-now",
        "helper": "gh_pr_watch.py",
        "helper_version": HELPER_VERSION,
        "repo": str(repo),
        "pr_number": int(pr_number),
        "expected_head_sha": str(expected_head_sha or ""),
        "selected_failed_run_ids": list(selected_run_ids or []),
        "retry_budget": {"max_flaky_retries": int(max_retries)},
    }
    payload = json.dumps(binding, sort_keys=True, separators=(",", ":"))
    return {
        "algorithm": "sha256",
        "value": hashlib.sha256(payload.encode("utf-8")).hexdigest(),
        "binding": binding,
    }


def get_workflow_run(repo, run_id):
    """Read one workflow run authoritatively from GitHub."""
    data = gh_json(
        ["api", f"repos/{repo}/actions/runs/{run_id}", "-X", "GET"],
        repo=repo,
    )
    if not isinstance(data, dict):
        raise GhCommandError("Unexpected payload from actions run API")
    return data


def validate_retry_pr(pr, current_pr, expected_head_sha):
    """Return a typed mismatch reason for the authoritative PR re-read."""
    if not isinstance(current_pr, dict):
        return "pr_read_invalid"
    if not repos_match(current_pr.get("repo"), pr.get("repo")):
        return "pr_repository_mismatch"
    try:
        current_number = int(current_pr.get("number") or 0)
        expected_number = int(pr.get("number") or 0)
    except (TypeError, ValueError):
        return "pr_read_invalid"
    if current_number != expected_number:
        return "pr_number_mismatch"
    if current_pr.get("closed") or current_pr.get("merged"):
        return "pr_closed_or_merged"
    if str(current_pr.get("state") or "").upper() != "OPEN":
        return "pr_not_open"
    if str(current_pr.get("head_sha") or "") != str(expected_head_sha):
        return "pr_head_mismatch"
    return None


def validate_retry_run(run, selected_run_id, expected_head_sha, expected_pr_number=None):
    """Return a typed mismatch reason for an authoritative run re-read."""
    if not isinstance(run, dict):
        return "run_read_invalid"
    if normalize_run_id(run.get("id")) != normalize_run_id(selected_run_id):
        return "run_id_mismatch"
    if str(run.get("head_sha") or "") != str(expected_head_sha):
        return "run_head_mismatch"
    if expected_pr_number is not None:
        pull_requests = run.get("pull_requests")
        if not isinstance(pull_requests, list) or not pull_requests:
            return "run_pr_association_missing"
        try:
            expected_pr_number = int(expected_pr_number)
        except (TypeError, ValueError):
            return "pr_read_invalid"
        associated_numbers = set()
        for associated_pr in pull_requests:
            if not isinstance(associated_pr, dict):
                continue
            try:
                associated_numbers.add(int(associated_pr.get("number")))
            except (TypeError, ValueError):
                continue
        if expected_pr_number not in associated_numbers:
            return "run_pr_mismatch"
    if str(run.get("status") or "").lower() != "completed":
        return "run_not_terminal"
    run_attempt = run.get("run_attempt")
    if not isinstance(run_attempt, int) or run_attempt <= 0:
        return "run_attempt_unobservable"
    conclusion = str(run.get("conclusion") or "").lower()
    if conclusion not in RERUNNABLE_FAILURE_CONCLUSIONS:
        if conclusion == "startup_failure":
            return "run_startup_blocked"
        return "run_not_rerunnable_failure"
    return None


def readback_rerun_attempt(repo, run_id, previous_run):
    """Read the rerun's resulting attempt identity without inferring missing fields."""
    try:
        current_run = get_workflow_run(repo, run_id)
    except GhCommandError as err:
        return {
            "readback_state": "error",
            "attempt_identity_observable": False,
            "run_id": str(run_id),
            "error": str(err),
        }

    observed_id = normalize_run_id(current_run.get("id"))
    run_attempt = current_run.get("run_attempt")
    attempt_identity = None
    if observed_id and isinstance(run_attempt, int) and run_attempt > 0:
        attempt_identity = f"{observed_id}:{run_attempt}"
    previous_attempt = previous_run.get("run_attempt") if isinstance(previous_run, dict) else None
    is_new_attempt = (
        isinstance(run_attempt, int)
        and isinstance(previous_attempt, int)
        and run_attempt > previous_attempt
    )
    readback_state = "observed"
    if observed_id != normalize_run_id(run_id):
        readback_state = "mismatch"
    if str(current_run.get("head_sha") or "") != str(previous_run.get("head_sha") or ""):
        readback_state = "mismatch"
    if readback_state == "observed" and (
        not isinstance(previous_attempt, int)
        or previous_attempt <= 0
        or not isinstance(run_attempt, int)
        or run_attempt <= previous_attempt
    ):
        readback_state = "inconclusive"
    return {
        "readback_state": readback_state,
        "run_id": observed_id or str(run_id),
        "run_attempt": run_attempt,
        "previous_run_attempt": previous_attempt,
        "attempt_identity": attempt_identity,
        "new_attempt": is_new_attempt,
        "attempt_identity_observable": attempt_identity is not None,
        "head_sha": str(current_run.get("head_sha") or ""),
        "status": str(current_run.get("status") or ""),
        "conclusion": str(current_run.get("conclusion") or ""),
        "read_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }


def unique_actions(actions):
    out = []
    seen = set()
    for action in actions:
        if action not in seen:
            out.append(action)
            seen.add(action)
    return out


def is_pr_ready_to_merge(pr, checks_summary, actionable_review_items, review_state, merge_blockers):
    if pr["closed"] or pr["merged"]:
        return False
    # Keep the queue invariant local to the readiness predicate as well as in
    # the derived blocker summary.  A future call path that forgets to pass
    # `build_merge_blockers()` must not turn an active queue entry into a
    # ready-to-merge receipt.
    merge_queue = pr.get("merge_queue") or {}
    if str(merge_queue.get("status") or "").lower() == "waiting":
        return False
    if merge_blockers.get("is_blocked_for_merge"):
        return False
    if not checks_summary["all_terminal"]:
        return False
    if checks_summary["failed_count"] > 0 or checks_summary["pending_count"] > 0:
        return False
    if actionable_review_items:
        return False
    return True


def recommend_actions(
    pr,
    checks_summary,
    failed_runs,
    actionable_review_items,
    review_state,
    merge_blockers,
    retries_used,
    max_retries,
    source="current_head",
    ci_startup_blockers=None,
):
    return recommend_actions_with_source(
        pr,
        checks_summary,
        failed_runs,
        actionable_review_items,
        review_state,
        merge_blockers,
        retries_used,
        max_retries,
        source=source,
        ci_startup_blockers=ci_startup_blockers,
    )


def recommend_actions_with_source(
    pr,
    checks_summary,
    failed_runs,
    actionable_review_items,
    review_state,
    merge_blockers=None,
    retries_used=0,
    max_retries=0,
    source="current_head",
    ci_startup_blockers=None,
):
    merge_blockers = merge_blockers or {"is_blocked_for_merge": False, "reason_kinds": []}
    actions = []
    if pr["closed"] or pr["merged"]:
        if actionable_review_items:
            actions.append("process_review_comment")
        actions.append("stop_pr_closed")
        return unique_actions(actions)

    if is_pr_ready_to_merge(pr, checks_summary, actionable_review_items, review_state, merge_blockers):
        actions.append("stop_ready_to_merge")
        return unique_actions(actions)

    review_blocking_reasons = set(merge_blockers.get("reason_kinds") or [])
    if "merge_queue_failed" in review_blocking_reasons:
        actions.append("stop_merge_queue_failed")
        return unique_actions(actions)
    if "merge_queue_removed" in review_blocking_reasons:
        actions.append("stop_merge_queue_removed")
        return unique_actions(actions)
    if "merge_queue_read_error" in review_blocking_reasons:
        actions.append("stop_merge_queue_read_error")
        return unique_actions(actions)
    if "merge_policy_blocked" in review_blocking_reasons:
        actions.append(ACTION_REQUIRED_MERGE_POLICY_BLOCKED)
    if actionable_review_items or any(
        reason in review_blocking_reasons
        for reason in {"unresolved_review_threads", "review_gate_not_satisfied", "blocking_review_submissions"}
    ):
        actions.append("process_review_comment")

    # Once GitHub has accepted the PR into an active merge queue, the queue is
    # the authoritative terminal-state machine.  Keep PR-check failures visible
    # in the snapshot, but do not interrupt the blocking watch merely because a
    # non-required check failed before a merge-group candidate exists.  A real
    # admission failure remains actionable through merge_queue_failed/removed.
    merge_queue_waiting = "merge_queue_waiting" in review_blocking_reasons
    has_failed_pr_checks = checks_summary["failed_count"] > 0 and not merge_queue_waiting
    if has_failed_pr_checks:
        actions.append("diagnose_ci_failure")
        if source == "stale_fallback":
            return unique_actions(actions)
        if ci_startup_blockers:
            actions.append("stop_ci_startup_blocked")
            return unique_actions(actions)
        if checks_summary["all_terminal"] and retries_used >= max_retries:
            actions.append("stop_exhausted_retries")
        elif checks_summary["all_terminal"] and failed_runs and retries_used < max_retries:
            actions.append("retry_failed_checks")

    if not actions:
        actions.append("idle")
    return unique_actions(actions)


def collect_snapshot(args):
    local_git_context = detect_local_git_context()
    pr = resolve_pr(args.pr, repo_override=args.repo)
    validate_pr_resolution(args.pr, args.repo, pr, local_git_context)
    state_path = state_file_for(args, pr)
    state, fresh_state = load_state(state_path)
    maybe_reset_seen_feedback(args, state)
    pr["merge_queue"] = reconcile_merge_queue_entry(pr, state)

    if not state.get("started_at"):
        state["started_at"] = int(time.time())

    # GitHub App installation tokens do not support GET /user. In the explicit
    # observer mode, leave identity unbound and retain conservative association
    # and approved-bot filtering for review activity.
    authenticated_login = None
    if not getattr(args, "installation_observer", False):
        authenticated_login = get_authenticated_login()
    new_review_items, review_items = fetch_new_review_items(
        pr,
        state,
        fresh_state=fresh_state,
        authenticated_login=authenticated_login,
        include_review_items=True,
    )
    review_threads = get_review_threads(pr)
    active_unresolved_threads, ignored_unresolved_threads = partition_unresolved_review_threads(
        review_threads,
        args.ignore_review_thread,
    )
    actionable_review_items = build_actionable_review_items(
        pr,
        new_review_items,
        active_unresolved_threads,
    )
    top_level_reviews = summarize_review_submissions(review_items, pr.get("head_sha"))
    review_state = {
        "total_thread_count": len(review_threads),
        "unresolved_thread_count": sum(1 for thread in review_threads if not thread.get("is_resolved")),
        "active_unresolved_thread_count": len(active_unresolved_threads),
        "ignored_unresolved_thread_count": len(ignored_unresolved_threads),
        "unresolved_threads": active_unresolved_threads,
        "ignored_unresolved_threads": ignored_unresolved_threads,
        "top_level_review_submissions": top_level_reviews["latest_reviews"],
        "top_level_review_submission_count": top_level_reviews["meaningful_review_count"],
        "blocking_top_level_review_submissions": top_level_reviews["blocking_reviews"],
        "blocking_top_level_review_submission_count": top_level_reviews["blocking_review_count"],
        "ignored_thread_selectors": [str(value) for value in args.ignore_review_thread or []],
    }
    # `gh pr checks -R <repo>` requires an explicit PR/branch/url argument.
    # After resolving `--pr auto`, reuse the concrete PR number.
    raw_checks = get_pr_checks(str(pr["number"]), repo=pr["repo"])
    checks, no_checks_policy = apply_no_checks_policy(pr, raw_checks)
    checks_summary = summarize_check_runs(checks)
    check_details = summarize_check_details(checks)
    workflow_runs = get_workflow_runs_for_sha(pr["repo"], pr["head_sha"])
    if not hasattr(args, "_workflow_jobs_cache"):
        args._workflow_jobs_cache = {}
    failed_runs = failed_runs_from_workflow_runs(
        workflow_runs, pr["head_sha"], repo=pr["repo"], cache=args._workflow_jobs_cache
    )
    ci_head_context = build_ci_head_context(pr, state, checks, checks_summary, failed_runs)
    (
        effective_checks_summary,
        checks_source,
        stale_fallback,
        ci_head_message,
        stale_failed_runs,
    ) = build_effective_ci_state(pr["head_sha"], checks_summary, ci_head_context)
    effective_failed_runs = stale_failed_runs if checks_source == "stale_fallback" else failed_runs
    ci_startup_blockers = startup_blockers_from_failed_runs(effective_failed_runs)
    effective_check_details = (
        build_stale_check_details(stale_failed_runs) if checks_source == "stale_fallback" else check_details
    )
    merge_blockers = build_merge_blockers(
        pr,
        effective_checks_summary,
        effective_check_details,
        review_state,
    )
    watch_context = build_watch_context(args, pr, local_git_context)

    retries_used = current_retry_count(state, pr["head_sha"])
    actions = recommend_actions(
        pr,
        effective_checks_summary,
        effective_failed_runs,
        actionable_review_items,
        review_state,
        merge_blockers,
        retries_used,
        args.max_flaky_retries,
        source=checks_source,
        ci_startup_blockers=ci_startup_blockers,
    )

    observed_at = int(time.time())
    state["pr"] = {"repo": pr["repo"], "number": pr["number"]}
    state["last_seen_head_sha"] = pr["head_sha"]
    state["last_snapshot_at"] = observed_at
    save_state(state_path, state)

    snapshot = {
        "pr": pr,
        "watch_context": watch_context,
        "checks": effective_checks_summary,
        "no_checks_policy": no_checks_policy,
        "raw_checks": checks_summary,
        "checks_source": checks_source,
        "check_details": effective_check_details,
        "raw_check_details": check_details,
        "failed_runs": effective_failed_runs,
        "failed_runs_current_head": failed_runs,
        "failed_runs_stale_head": stale_failed_runs,
        "ci_startup_blockers": ci_startup_blockers,
        "ci_head_context": ci_head_context,
        "ci_head_message": ci_head_message,
        "stale_fallback": stale_fallback,
        "new_review_items": new_review_items,
        "actionable_review_items": actionable_review_items,
        "review_state": review_state,
        "merge_blockers": merge_blockers,
        "actions": actions,
        "retry_state": {
            "current_sha_retries_used": retries_used,
            "max_flaky_retries": args.max_flaky_retries,
        },
    }
    watch_decision = build_watch_decision(snapshot, recorded_at=observed_at)
    state, _ = load_state(state_path)
    state["last_watch_decision"] = watch_decision
    save_state(state_path, state)
    snapshot["watch_decision"] = watch_decision
    return snapshot, state_path


def retry_failed_now(args):
    expected_head_sha = str(getattr(args, "expected_head_sha", "") or "").strip()
    if not expected_head_sha:
        return {
            "schema_version": 1,
            "helper_version": HELPER_VERSION,
            "rerun_attempted": False,
            "rerun_count": 0,
            "rerun_run_ids": [],
            "attempts": [],
            "reason": "expected_head_sha_required",
            "action_fingerprint": None,
            "receipt": {
                "recorded_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "mutation": "none",
            },
        }

    snapshot, state_path = collect_snapshot(args)
    pr = snapshot["pr"]
    checks_summary = snapshot["checks"]
    failed_runs = snapshot["failed_runs"]
    retries_used = snapshot["retry_state"]["current_sha_retries_used"]
    max_retries = snapshot["retry_state"]["max_flaky_retries"]

    requested_run_ids = getattr(args, "run_ids", None)
    if requested_run_ids is None:
        requested_run_ids = [run.get("run_id") for run in failed_runs]
    selected_run_ids = canonical_run_ids(requested_run_ids)
    action_fingerprint = retry_action_fingerprint(
        pr["repo"],
        pr["number"],
        expected_head_sha,
        selected_run_ids or [],
        max_retries,
    )

    result = {
        "schema_version": 1,
        "helper_version": HELPER_VERSION,
        "snapshot": snapshot,
        "state_file": str(state_path),
        "rerun_attempted": False,
        "rerun_count": 0,
        "rerun_run_ids": [],
        "attempts": [],
        "reason": None,
        "action_fingerprint": action_fingerprint,
        "receipt": {
            "recorded_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "mutation": "none",
        },
    }

    if selected_run_ids is None:
        result["reason"] = "invalid_run_id"
        return result
    if not selected_run_ids:
        result["reason"] = "no_failed_runs"
        return result
    if str(pr.get("head_sha") or "") != expected_head_sha:
        result["reason"] = "pr_head_mismatch"
        return result
    if pr["closed"] or pr["merged"] or str(pr.get("state") or "").upper() != "OPEN":
        result["reason"] = "pr_closed_or_merged"
        return result
    if snapshot.get("checks_source") == "stale_fallback":
        result["reason"] = "stale_fallback_diagnostic_only"
        return result
    if checks_summary["failed_count"] <= 0:
        result["reason"] = "no_failed_pr_checks"
        return result
    if not checks_summary["all_terminal"]:
        result["reason"] = "checks_still_pending"
        return result
    if retries_used >= max_retries:
        result["reason"] = "retry_budget_exhausted"
        return result

    # Re-read every authorization input before any mutation. This two-phase
    # preflight means a stale/replaced run or changed PR head cannot trigger a
    # partial batch rerun.
    validated_runs = []
    current_pr = resolve_pr(str(pr["number"]), repo_override=pr["repo"])
    pr_reason = validate_retry_pr(pr, current_pr, expected_head_sha)
    if pr_reason:
        result["reason"] = pr_reason
        result["receipt"]["validation"] = {"pr": pr_reason}
        return result
    for run_id in selected_run_ids:
        try:
            current_run = get_workflow_run(pr["repo"], run_id)
        except GhCommandError as err:
            result["reason"] = "run_read_failed"
            result["receipt"]["validation"] = {
                "run_id": run_id,
                "error": str(err),
            }
            return result
        run_reason = validate_retry_run(
            current_run,
            run_id,
            expected_head_sha,
            expected_pr_number=pr["number"],
        )
        if run_reason:
            result["reason"] = run_reason
            result["receipt"]["validation"] = {
                "run_id": run_id,
                "reason": run_reason,
            }
            return result
        validated_runs.append((run_id, current_run))

    result["receipt"]["validation"] = {
        "pr": "open_head_bound",
        "runs": [str(run_id) for run_id, _ in validated_runs],
        "validated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    try:
        budget = reserve_retry_budget(state_path, pr["head_sha"], max_retries)
    except (OSError, RuntimeError) as err:
        result["reason"] = "retry_budget_reservation_failed"
        result["receipt"]["budget_error"] = str(err)
        return result
    if budget is None:
        result["reason"] = "retry_budget_exhausted"
        result["receipt"]["validation"] = {
            "budget": "changed_during_preflight",
        }
        return result
    result["receipt"]["retry_budget"] = budget
    for run_id, previous_run in validated_runs:
        try:
            gh_text(["run", "rerun", str(run_id), "--failed"], repo=pr["repo"])
        except GhCommandError as err:
            # A provider error is ambiguous: stop and never issue a second
            # mutation or blindly retry the same run.
            result["reason"] = "rerun_command_ambiguous"
            result["receipt"].update(
                {
                    "mutation": "ambiguous",
                    "failed_run_id": str(run_id),
                    "error": str(err),
                }
            )
            return result
        # Keep the historical JSON shape (numeric GitHub run ids) while the
        # fingerprint and validation paths use canonical decimal strings.
        result["rerun_run_ids"].append(int(run_id))
        result["rerun_attempted"] = True
        result["rerun_count"] += 1
        readback = readback_rerun_attempt(pr["repo"], run_id, previous_run)
        result["attempts"].append(readback)
        if readback.get("readback_state") != "observed":
            result["reason"] = (
                "post_rerun_readback_inconclusive"
                if readback.get("readback_state") == "inconclusive"
                else "post_rerun_readback_failed"
            )
            result["receipt"]["mutation"] = "completed_readback_failed"
            return result

    if result["rerun_run_ids"]:
        result["reason"] = "rerun_triggered"
        result["receipt"].update(
            {
                "mutation": "completed",
                "completed_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            }
        )
    else:
        result["reason"] = "failed_runs_missing_ids"

    return result


def print_json(obj):
    sys.stdout.write(json.dumps(obj, sort_keys=True) + "\n")
    sys.stdout.flush()


def print_event(event, payload):
    print_json({"event": event, "payload": payload})


def print_status(message):
    sys.stderr.write(message.rstrip() + "\n")
    sys.stderr.flush()


def is_ci_green(snapshot):
    checks = snapshot.get("checks") or {}
    return (
        bool(checks.get("all_terminal"))
        and int(checks.get("failed_count") or 0) == 0
        and int(checks.get("pending_count") or 0) == 0
    )


def summarize_check_details(checks):
    details = {"failing": [], "pending": []}
    for check in checks:
        if not isinstance(check, dict):
            continue
        detail = {
            "name": str(check.get("name") or ""),
            "state": str(check.get("state") or ""),
            "bucket": str(check.get("bucket") or ""),
            "workflow": str(check.get("workflow") or ""),
            "link": str(check.get("link") or ""),
        }
        if is_pending_check(check):
            details["pending"].append(detail)
        if str(check.get("bucket") or "").lower() == "fail":
            details["failing"].append(detail)
    return details


def snapshot_change_key(snapshot):
    pr = snapshot.get("pr") or {}
    checks = snapshot.get("checks") or {}
    review_state = snapshot.get("review_state") or {}
    review_items = snapshot.get("actionable_review_items") or []
    return (
        str(pr.get("head_sha") or ""),
        str(pr.get("state") or ""),
        str(pr.get("mergeable") or ""),
        str(pr.get("merge_state_status") or ""),
        str(pr.get("review_decision") or ""),
        str((pr.get("merge_queue") or {}).get("read_state") or ""),
        str((pr.get("merge_queue") or {}).get("status") or ""),
        str((pr.get("merge_queue") or {}).get("id") or ""),
        str((pr.get("merge_queue") or {}).get("state") or ""),
        str((pr.get("merge_queue") or {}).get("head_sha") or ""),
        int(checks.get("passed_count") or 0),
        int(checks.get("failed_count") or 0),
        int(checks.get("pending_count") or 0),
        int(review_state.get("active_unresolved_thread_count") or 0),
        tuple(
            (str(item.get("kind") or ""), str(item.get("id") or ""))
            for item in review_items
            if isinstance(item, dict)
        ),
        tuple(snapshot.get("actions") or []),
    )


def has_non_idle_actions(snapshot):
    return any(action != "idle" for action in (snapshot.get("actions") or []))


def has_active_merge_queue_wait(snapshot):
    """Keep the base cadence while an exact queue entry is still active."""
    merge_queue = (snapshot.get("pr") or {}).get("merge_queue") or {}
    if str(merge_queue.get("status") or "").lower() != "waiting":
        return False
    return str(merge_queue.get("state") or "").upper() in MERGE_QUEUE_WAITING_STATES


def _compact_review_item(item):
    if not isinstance(item, dict):
        return item
    compact = {
        key: item.get(key)
        for key in (
            "kind",
            "id",
            "author",
            "path",
            "line",
            "url",
            "created_at",
            "is_resolved",
            "is_outdated",
        )
        if item.get(key) not in (None, "", [], {})
    }
    body = str(item.get("body") or "")
    if body:
        compact["body"] = body if len(body) <= 1000 else body[:997] + "..."
    return compact


def _compact_failed_job(job):
    if not isinstance(job, dict):
        return job
    compact = {
        key: job.get(key)
        for key in (
            "job_id",
            "job_name",
            "status",
            "conclusion",
            "html_url",
        )
        if job.get(key) not in (None, "", [], {})
    }
    startup_failure = job.get("startup_failure")
    if isinstance(startup_failure, dict):
        compact_startup = {
            "category": startup_failure.get("category"),
            "step_count": startup_failure.get("step_count"),
        }
        annotations = startup_failure.get("annotations") or []
        if annotations and isinstance(annotations[0], dict):
            compact_startup["message"] = str(annotations[0].get("message") or "")
        compact["startup_failure"] = compact_startup
    return compact


def _compact_failed_run(run):
    if not isinstance(run, dict):
        return run
    compact = {
        key: run.get(key)
        for key in (
            "run_id",
            "workflow_name",
            "status",
            "conclusion",
            "html_url",
        )
        if run.get(key) not in (None, "", [], {})
    }
    if run.get("first_failed_job"):
        compact["first_failed_job"] = _compact_failed_job(run["first_failed_job"])
    failed_jobs = run.get("failed_jobs") or []
    compact["failed_job_count"] = len(failed_jobs)
    return compact


def _compact_startup_blockers(blockers):
    blockers = [item for item in blockers or [] if isinstance(item, dict)]
    categories = sorted(
        {
            str((item.get("startup_failure") or {}).get("category") or "")
            for item in blockers
            if (item.get("startup_failure") or {}).get("category")
        }
    )
    examples = []
    for item in blockers[:3]:
        startup_failure = item.get("startup_failure") or {}
        annotations = startup_failure.get("annotations") or []
        message = ""
        if annotations and isinstance(annotations[0], dict):
            message = str(annotations[0].get("message") or "")
        examples.append(
            {
                "run_id": item.get("run_id"),
                "job_id": item.get("job_id"),
                "job_name": str(item.get("job_name") or ""),
                "category": str(startup_failure.get("category") or ""),
                "message": message,
            }
        )
    return {
        "count": len(blockers),
        "categories": categories,
        "examples": examples,
    }


def compact_wait_snapshot(snapshot):
    pr = snapshot.get("pr") or {}
    compact_pr = {
        key: pr.get(key)
        for key in (
            "repo",
            "number",
            "url",
            "head_sha",
            "base_sha",
            "state",
            "merged",
            "closed",
            "mergeable",
            "merge_state_status",
            "review_decision",
            "merge_queue",
        )
        if pr.get(key) not in (None, "", [], {})
    }
    return {
        "pr": compact_pr,
        "watch_context": snapshot.get("watch_context"),
        "checks": snapshot.get("checks"),
        "checks_source": snapshot.get("checks_source"),
        "check_details": snapshot.get("check_details"),
        "failed_runs": [
            _compact_failed_run(item)
            for item in (snapshot.get("failed_runs") or [])
        ],
        "ci_startup_blockers": _compact_startup_blockers(
            snapshot.get("ci_startup_blockers")
        ),
        "ci_head_context": snapshot.get("ci_head_context"),
        "ci_head_message": snapshot.get("ci_head_message"),
        "actionable_review_items": [
            _compact_review_item(item)
            for item in (snapshot.get("actionable_review_items") or [])
        ],
        "review_state": snapshot.get("review_state"),
        "merge_blockers": snapshot.get("merge_blockers"),
        "actions": snapshot.get("actions"),
        "retry_state": snapshot.get("retry_state"),
        "watch_decision": snapshot.get("watch_decision"),
    }


def should_wait_for_terminal_checks(args, snapshot):
    if not getattr(args, "require_terminal_checks", False):
        return False
    checks = snapshot.get("checks") or {}
    if checks.get("all_terminal"):
        return False
    actions = set(snapshot.get("actions") or [])
    ci_failure_actions = {"diagnose_ci_failure", "retry_failed_checks", "stop_exhausted_retries"}
    return bool(actions & ci_failure_actions) and not bool(actions - ci_failure_actions - {"idle"})


def next_watch_poll_seconds(args, snapshot, last_change_key, poll_seconds, max_poll_seconds):
    current_change_key = snapshot_change_key(snapshot)
    changed = current_change_key != last_change_key
    green = is_ci_green(snapshot)

    if has_merge_policy_blocker(snapshot) or has_active_merge_queue_wait(snapshot):
        # An unexplained policy blocker is actionable even when CI is green.
        # Keep foreground watches bounded instead of allowing the green-state
        # exponential backoff to hide the action for up to twenty minutes.
        next_poll_seconds = args.poll_seconds
    elif not green:
        next_poll_seconds = args.poll_seconds
    elif changed or last_change_key is None:
        next_poll_seconds = args.poll_seconds
    else:
        next_poll_seconds = min(poll_seconds * 2, max_poll_seconds)

    return next_poll_seconds, current_change_key


def run_watch(args):
    poll_seconds = args.poll_seconds
    last_change_key = None
    while True:
        snapshot, state_path = collect_snapshot(args)
        poll_seconds, last_change_key = next_watch_poll_seconds(
            args,
            snapshot,
            last_change_key,
            poll_seconds,
            GREEN_STATE_MAX_POLL_SECONDS,
        )
        print_event(
            "snapshot",
            {
                "snapshot": snapshot,
                "state_file": str(state_path),
                "next_poll_seconds": poll_seconds,
            },
        )
        actions = set(snapshot.get("actions") or [])
        if actions & STOP_ACTIONS:
            persist_watch_schedule(state_path, snapshot, "watch", 0)
            print_event("stop", {"actions": snapshot.get("actions"), "pr": snapshot.get("pr")})
            return 0

        persist_watch_schedule(state_path, snapshot, "watch", poll_seconds)
        time.sleep(poll_seconds)


def run_watch_until_action(args):
    poll_seconds = args.poll_seconds
    last_change_key = None
    started_at = time.time()
    polls_completed = 0
    while True:
        snapshot, state_path = collect_snapshot(args)
        polls_completed += 1
        if should_wait_for_terminal_checks(args, snapshot):
            pass
        elif has_non_idle_actions(snapshot):
            actions = snapshot.get("actions") or []
            exit_reason = "action_required"
            for action in actions:
                if action in STOP_ACTIONS:
                    exit_reason = action
                    break
            output_snapshot = (
                snapshot
                if getattr(args, "verbose_details", False)
                else compact_wait_snapshot(snapshot)
            )
            persist_watch_schedule(state_path, snapshot, "watch-until-action", 0)
            print_json(
                {
                    "elapsed_seconds": int(max(time.time() - started_at, 0)),
                    "exit_reason": exit_reason,
                    "polls_completed": polls_completed,
                    "snapshot": output_snapshot,
                    "state_file": str(state_path),
                }
            )
            return 0

        poll_seconds, last_change_key = next_watch_poll_seconds(
            args,
            snapshot,
            last_change_key,
            poll_seconds,
            WATCH_UNTIL_ACTION_MAX_POLL_SECONDS,
        )
        persist_watch_schedule(state_path, snapshot, "watch-until-action", poll_seconds)
        if getattr(args, "progress", False):
            print_status(
                "gh_pr_watch.py waiting: "
                f"poll={polls_completed} "
                f"head={snapshot.get('pr', {}).get('head_sha', '')} "
                f"next_poll_seconds={poll_seconds}"
            )
        time.sleep(poll_seconds)


def main():
    args = parse_args()
    try:
        if args.retry_failed_now:
            print_json(retry_failed_now(args))
            return 0
        if args.watch:
            return run_watch(args)
        if args.watch_until_action:
            return run_watch_until_action(args)
        snapshot, state_path = collect_snapshot(args)
        snapshot["state_file"] = str(state_path)
        print_json(snapshot)
        return 0
    except (GhCommandError, RuntimeError, ValueError) as err:
        classification = classify_gh_error(err) if isinstance(err, GhCommandError) else "runtime"
        sys.stderr.write(f"gh_pr_watch.py error classification={classification}: {err}\n")
        return 1
    except KeyboardInterrupt:
        sys.stderr.write("gh_pr_watch.py interrupted\n")
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
