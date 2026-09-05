#!/usr/bin/env python3
"""Read-only, fail-closed observation of one protected merge-queue entry.

The PR watcher may provide one blocking PR-local wait, but it does not watch
queue/ruleset/head transitions.  This module therefore never polls or mutates;
after a wake the caller must obtain a fresh queue snapshot.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping, Sequence
from urllib.parse import urlparse


HELPER_VERSION = "1.0.0-queue-identity"
OWNER_UNMERGEABLE_ACTION = "owner_scoped_unmergeable"
HEAD_REPLACED_ACTION = "head_replaced_rebind_required"
RULESET_CHANGED_ACTION = "ruleset_generation_changed_rebind_required"
IDENTITY_MISMATCH_ACTION = "identity_mismatch_rebind_required"
QUEUE_FAILURE_ACTION = "queue_terminal_failure"
IDLE_ACTION = "idle"
UNKNOWN_QUEUE_STATE_ACTION = "queue_state_unknown_fail_closed"
REQUIRED_WORKFLOWS = ("CI required", "CodeQL required")
RECOGNIZED_WATCH_EXITS = {
    "action_required",
    "stop_pr_closed",
    "stop_ready_to_merge",
    "stop_exhausted_retries",
}
DELEGATED_WATCH_HELPER = "gh_pr_watch.py"
DELEGATED_WATCH_HELPER_VERSION = "1.1.0-head-guard"
DELEGATED_WATCH_MODE = "watch-until-action"
ACTIVE_QUEUE_STATES = {"AWAITING_CHECKS", "QUEUED", "IN_PROGRESS", "EXPECTED_HEAD_SHA", "PENDING"}
UNMERGEABLE_STATES = {"UNMERGEABLE", "UNMERGEABLE_PR", "CONFLICTING"}
FAILED_QUEUE_STATES = {"FAILED", "REMOVED", "CANCELLED", "ERROR"}
KNOWN_QUEUE_STATES = ACTIVE_QUEUE_STATES | UNMERGEABLE_STATES | FAILED_QUEUE_STATES
FULL_SHA = re.compile(r"^[0-9a-fA-F]{40}$")

MERGE_QUEUE_QUERY = """
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    mergeQueue {
      entries(first: 100) {
        nodes {
          id position state
          baseCommit { oid }
          headCommit { oid }
          pullRequest {
            number headRefOid baseRefOid baseRefName headRefName
            author { login }
          }
        }
      }
    }
  }
}
"""


class QueueObserverError(RuntimeError):
    """Raised when a read-only observation cannot be trusted."""


def _text(value: Any) -> str:
    return str(value or "").strip()


def _upper(value: Any) -> str:
    return _text(value).upper()


def _lower(value: Any) -> str:
    return _text(value).casefold()


def _first(mapping: Mapping[str, Any], *names: str) -> Any:
    for name in names:
        if name in mapping and mapping[name] is not None:
            return mapping[name]
    return None


def _login(value: Any) -> str:
    return _text(_first(value, "login", "name", "node_id")) if isinstance(value, Mapping) else _text(value)


def _repo_slug(value: Any) -> str:
    if isinstance(value, Mapping):
        value = _first(value, "nameWithOwner", "fullName", "full_name")
    value = _text(value)
    if "://" not in value and "/" in value:
        return value.removesuffix(".git")
    parts = [p for p in urlparse(value).path.split("/") if p]
    return f"{parts[0]}/{parts[1]}".removesuffix(".git") if len(parts) >= 2 else ""


def _canonical(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)


def _digest(value: Any) -> str:
    return hashlib.sha256(_canonical(value).encode()).hexdigest()


def _positive_id(value: Any) -> str:
    return "" if value is None or isinstance(value, bool) else _text(value)


def _is_full_sha(value: Any) -> bool:
    return bool(FULL_SHA.fullmatch(_text(value)))


def _nodes(value: Any) -> list[Any]:
    if isinstance(value, Mapping):
        value = value.get("nodes") or value.get("items") or value.get("edges") or []
        if isinstance(value, list) and all(isinstance(x, Mapping) and "node" in x for x in value):
            value = [x.get("node") for x in value]
    return list(value) if isinstance(value, list) else []


def _ruleset_applies_to_base(item: Mapping[str, Any], base_ref: str, default_branch: str = "") -> bool:
    if not _positive_id(_first(item, "id", "databaseId", "database_id")):
        return False
    if not _text(_first(item, "updated_at", "updatedAt", "version")):
        return False
    if _upper(_first(item, "enforcement", "enforcement_status")) != "ACTIVE":
        return False
    if _lower(item.get("target")) != "branch":
        return False
    conditions = item.get("conditions")
    includes = conditions.get("ref_name", {}).get("include") if isinstance(conditions, Mapping) else None
    if not isinstance(includes, list) or not includes:
        return False
    target = f"refs/heads/{base_ref}"
    return any(
        isinstance(pattern, str)
        and (pattern == "~ALL" or (pattern == "~DEFAULT_BRANCH" and default_branch and base_ref == default_branch)
             or fnmatch.fnmatchcase(target, pattern) or fnmatch.fnmatchcase(base_ref, pattern))
        for pattern in includes
    )


def ruleset_generation(rulesets: Iterable[Mapping[str, Any]] | Mapping[str, Any] | None) -> str:
    direct = ""
    if isinstance(rulesets, Mapping):
        direct = _text(_first(rulesets, "generation", "ruleset_generation", "version"))
        rulesets = _first(rulesets, "rulesets", "nodes", "items") or []
    rows = []
    for item in rulesets or []:
        if isinstance(item, Mapping):
            rows.append({
                "id": _positive_id(_first(item, "id", "databaseId", "database_id")),
                "name": _text(item.get("name")),
                "updated_at": _text(_first(item, "updated_at", "updatedAt")),
                "target": _text(item.get("target")),
                "enforcement": _text(_first(item, "enforcement", "enforcement_status")),
                "conditions": item.get("conditions") or {},
                "rules": item.get("rules") or [],
            })
    rows.sort(key=lambda row: (row["id"], row["name"]))
    return _digest({"provider_generation": direct, "rulesets": rows})


def normalize_ruleset_readback(
    rulesets: Iterable[Mapping[str, Any]] | Mapping[str, Any] | None,
    observed_generation: Any = None,
    *,
    base_ref: str = "",
    default_branch: str = "",
) -> dict[str, Any]:
    direct = _text(_first(rulesets, "generation", "ruleset_generation", "version")) if isinstance(rulesets, Mapping) else ""
    values = _first(rulesets, "rulesets", "nodes", "items") if isinstance(rulesets, Mapping) else rulesets
    normalized = [dict(x) for x in (values or []) if isinstance(x, Mapping)]
    applicable = [x for x in normalized if base_ref and _ruleset_applies_to_base(x, base_ref, default_branch)]
    generation = ruleset_generation(normalized)
    applicable_generation = ruleset_generation(applicable)
    supplied = _text(observed_generation) or direct
    return {
        "generation": applicable_generation,
        "readback_generation": generation,
        "observed_generation": supplied,
        "matches_observed": not supplied or supplied in (generation, applicable_generation),
        "active_ruleset_count": len(applicable),
        "base_ref": base_ref,
        "default_branch": default_branch,
        "applicable_ruleset_ids": [_positive_id(_first(x, "id", "databaseId", "database_id")) for x in applicable],
        "applicable_rulesets": applicable,
        "rulesets": normalized,
    }


def normalize_workflow_runs(runs: Iterable[Mapping[str, Any]] | None) -> list[dict[str, Any]]:
    normalized = []
    for run in runs or []:
        if not isinstance(run, Mapping):
            run = {}
        attempt = _first(run, "run_attempt", "runAttempt", "attempt")
        if isinstance(attempt, str) and attempt.strip().isdigit():
            attempt = int(attempt.strip())
        normalized.append({
            "run_id": _positive_id(_first(run, "id", "databaseId", "run_id")),
            "workflow": _text(_first(run, "workflow", "workflow_name", "workflowName", "name")),
            "event": _text(_first(run, "event", "event_name")),
            "head_sha": _text(_first(run, "head_sha", "headSha", "headShaOid", "sha")),
            "status": _upper(_first(run, "status", "state")),
            "conclusion": _lower(_first(run, "conclusion", "result")),
            "run_attempt": attempt,
        })
    return sorted(normalized, key=lambda row: (row["run_id"], row["workflow"]))


def workflow_evidence(runs: Iterable[Mapping[str, Any]] | None, synthetic_sha: str, expected_attempt: Any = None) -> dict[str, Any]:
    rows = normalize_workflow_runs(runs)
    reasons: set[str] = set()
    selected: dict[str, dict[str, Any]] = {}
    attempts: set[int] = set()
    if not rows:
        reasons.add("empty_run_set")
    for row in rows:
        name = row["workflow"]
        if not row["run_id"]:
            reasons.add("run_id_missing")
        if name not in REQUIRED_WORKFLOWS:
            reasons.add("unrelated_workflow")
        if row["event"] != "merge_group":
            reasons.add("event_not_merge_group")
        if not _is_full_sha(row["head_sha"]) or row["head_sha"] != synthetic_sha:
            reasons.add("run_head_mismatch")
        if row["status"] != "COMPLETED" or row["conclusion"] != "success":
            reasons.add("run_not_terminal_success")
        attempt = row["run_attempt"]
        if isinstance(attempt, bool) or not isinstance(attempt, int) or attempt <= 0:
            reasons.add("run_attempt_missing")
        else:
            attempts.add(attempt)
        if name in selected:
            reasons.add("duplicate_required_workflow")
        else:
            selected[name] = row
    missing = [name for name in REQUIRED_WORKFLOWS if name not in selected]
    if missing:
        reasons.add("required_workflow_missing")
    if len(attempts) != 1:
        reasons.add("attempt_mismatch")
    if expected_attempt is not None and (isinstance(expected_attempt, bool) or not isinstance(expected_attempt, int)
                                         or expected_attempt <= 0 or attempts != {expected_attempt}):
        reasons.add("queue_attempt_mismatch")
    selected_rows = [selected[name] for name in REQUIRED_WORKFLOWS if name in selected]
    return {
        "valid": not reasons and not missing and len(selected) == len(REQUIRED_WORKFLOWS),
        "required_workflows": list(REQUIRED_WORKFLOWS),
        "selected": selected_rows,
        "missing_workflows": missing,
        "reasons": sorted(reasons),
        "attempt": next(iter(attempts)) if len(attempts) == 1 else None,
        "run_set": [row["run_id"] for row in selected_rows],
        "fingerprint": _digest({"synthetic_sha": synthetic_sha, "runs": selected_rows}),
    }


def normalize_queue_entry(raw: Mapping[str, Any] | None) -> dict[str, Any] | None:
    if raw is None:
        return None
    if not isinstance(raw, Mapping):
        raise QueueObserverError("queue entry payload is not an object")
    group = _first(raw, "merge_group", "mergeGroup")
    group = group if isinstance(group, Mapping) else {}
    ancestry = _first(raw, "ancestry_evidence", "ancestryEvidence", "containment_evidence", "containment", "ancestry")
    ancestry = ancestry if isinstance(ancestry, Mapping) else {}
    attempt = _first(raw, "attempt", "run_attempt", "runAttempt", "queue_attempt")
    if isinstance(attempt, str) and attempt.strip().isdigit():
        attempt = int(attempt.strip())
    return {
        "queue_entry_id": _positive_id(_first(raw, "id", "databaseId", "queue_entry_id", "queueEntryId")),
        "queue_entry_ref": _text(_first(raw, "queue_entry_ref", "queueEntryRef")),
        "state": _upper(_first(raw, "state", "status")),
        "position": _first(raw, "position", "queue_position"),
        "attempt": attempt,
        "synthetic_sha": _text(_first(raw, "merge_group_sha", "mergeGroupSha", "synthetic_sha")
                                or _first(group, "head_sha", "headSha", "oid", "sha")),
        "base_sha": _text(_first(raw, "merge_group_base_sha", "mergeGroupBaseSha")
                           or _first(group, "base_sha", "baseSha", "base_oid", "baseOid")
                           or _first(raw, "base_sha", "baseSha")),
        "base_ref": _text(_first(raw, "base_ref", "baseRefName") or _first(group, "base_ref", "baseRefName")),
        "synthetic_source": _text(_first(raw, "merge_group_source", "synthetic_source") or _first(group, "source")),
        "ancestry": dict(ancestry),
        "merge_group": dict(group),
        "pull_requests": _nodes(_first(raw, "pull_requests", "pullRequests", "entries")
                                 or _first(group, "pull_requests", "pullRequests", "entries") or []),
    }


def normalize_candidate(raw: Mapping[str, Any]) -> dict[str, Any]:
    if not isinstance(raw, Mapping):
        raise QueueObserverError("queue candidate payload is not an object")
    number = _first(raw, "pr_number", "number", "pull_request_number", "pullRequestNumber")
    try:
        number = int(number) if number is not None else None
    except (TypeError, ValueError):
        number = None
    nested = _first(raw, "queue_entry", "queueEntry", "merge_queue_entry", "mergeQueueEntry")
    nested = nested if isinstance(nested, Mapping) else {}
    return {
        "pr_number": number,
        "owner": _login(_first(raw, "owner", "author", "user")),
        "head_sha": _text(_first(raw, "head_sha", "headSha", "headRefOid")),
        "base_sha": _text(_first(raw, "base_sha", "baseSha", "baseRefOid")),
        "base_ref": _text(_first(raw, "base_ref", "baseRefName")),
        "queue_entry_id": _positive_id(_first(raw, "queue_entry_id", "queueEntryId")
                                         or _first(nested, "queue_entry_id", "queueEntryId")),
        "state": _upper(_first(raw, "state", "status") or _first(nested, "state", "status")),
        "merge_group_sha": _text(_first(raw, "merge_group_sha", "mergeGroupSha", "synthetic_sha")
                                  or _first(nested, "merge_group_sha", "mergeGroupSha", "synthetic_sha")),
    }


def candidate_is_owner(candidate: Mapping[str, Any], owner: Mapping[str, Any]) -> bool:
    return candidate.get("pr_number") is not None and owner.get("pr_number") is not None and int(candidate["pr_number"]) == int(owner["pr_number"])


def classify_candidates(candidates: Iterable[Mapping[str, Any]] | None, owner: Mapping[str, Any]) -> dict[str, list[dict[str, Any]]]:
    result = {"owner": [], "independent": [], "unknown": []}
    for raw in candidates or []:
        candidate = normalize_candidate(raw)
        scope = "unknown" if candidate["pr_number"] is None else ("owner" if candidate_is_owner(candidate, owner) else "independent")
        candidate["scope"] = scope
        result[scope].append(candidate)
    return result


classify_queue_candidates = classify_candidates


def owner_identity_from_pr(pr: Mapping[str, Any]) -> dict[str, Any]:
    head = _first(pr, "head") if isinstance(_first(pr, "head"), Mapping) else {}
    base = _first(pr, "base") if isinstance(_first(pr, "base"), Mapping) else {}
    number = _first(pr, "number", "pr_number", "pull_request_number")
    try:
        number = int(number) if number is not None else None
    except (TypeError, ValueError):
        number = None
    return {
        "repository": _repo_slug(_first(pr, "repository", "repo", "url")),
        "pr_number": number,
        "owner": _login(_first(pr, "owner", "author", "user")),
        "head_sha": _text(_first(pr, "head_sha", "headRefOid", "headSha") or head.get("sha")),
        "base_sha": _text(_first(pr, "base_sha", "baseRefOid", "baseSha") or base.get("sha")),
        "base_ref": _text(_first(pr, "base_ref", "baseRefName") or base.get("ref")),
    }


def build_binding(pr: Mapping[str, Any], queue_entry: Mapping[str, Any] | None, workflow_runs: Iterable[Mapping[str, Any]] | None, ruleset_readback: Mapping[str, Any]) -> dict[str, Any]:
    owner = owner_identity_from_pr(pr)
    queue = normalize_queue_entry(queue_entry)
    runs = normalize_workflow_runs(workflow_runs)
    binding = dict(owner)
    binding.update({
        "queue_entry_id": _text((queue or {}).get("queue_entry_id")),
        "queue_entry_ref": _text((queue or {}).get("queue_entry_ref")),
        "merge_group_sha": _text((queue or {}).get("synthetic_sha")),
        "merge_group_source": _text((queue or {}).get("synthetic_source")),
        "queue_base_sha": _text((queue or {}).get("base_sha")),
        "queue_base_ref": _text((queue or {}).get("base_ref")),
        "queue_state": _upper((queue or {}).get("state")),
        "queue_attempt": (queue or {}).get("attempt"),
        "ancestry": dict((queue or {}).get("ancestry") or {}),
        "workflow_run_ids": [run["run_id"] for run in runs],
        "workflow_runs": runs,
        "ruleset_generation": _text(ruleset_readback.get("generation")),
        "ruleset_ids": list(ruleset_readback.get("applicable_ruleset_ids") or []),
    })
    return binding


def binding_missing(binding: Mapping[str, Any], *, require_queue: bool = True) -> list[str]:
    required = ["repository", "pr_number", "owner", "head_sha", "base_sha", "base_ref"]
    if require_queue:
        required += ["queue_entry_id", "queue_entry_ref", "merge_group_sha", "merge_group_source", "queue_base_sha", "queue_base_ref", "queue_state"]
    missing = [name for name in required if binding.get(name) in (None, "", [])]
    if not binding.get("ruleset_generation"):
        missing.append("ruleset_generation")
    ids = list(binding.get("ruleset_ids") or [])
    if not ids or len(ids) != len(set(ids)) or any(not _positive_id(x) for x in ids):
        missing.append("ruleset_ids")
    for field in ("head_sha", "base_sha"):
        if binding.get(field) and not _is_full_sha(binding[field]):
            missing.append(f"{field}_full")
    if not require_queue:
        return missing
    for field in ("merge_group_sha", "queue_base_sha"):
        if binding.get(field) and not _is_full_sha(binding[field]):
            missing.append(f"{field}_full")
    if binding.get("merge_group_sha") == binding.get("head_sha"):
        missing.append("merge_group_distinct_from_pr_head")
    if binding.get("merge_group_source") not in {"MergeQueueEntry.headCommit.oid", "MergeGroup.headSha"}:
        missing.append("merge_group_source_untrusted")
    if binding.get("queue_state") not in KNOWN_QUEUE_STATES:
        missing.append("queue_state_unknown")
    ancestry = binding.get("ancestry")
    expected = {"pr_head_sha": binding.get("head_sha"), "base_sha": binding.get("base_sha"), "synthetic_sha": binding.get("merge_group_sha")}
    if not isinstance(ancestry, Mapping):
        missing.append("ancestry")
    else:
        for field, value in expected.items():
            if ancestry.get(field) != value:
                missing.append(f"ancestry_{field}")
        for field in ("contains_pr_head", "contains_base", "complete", "verified"):
            if ancestry.get(field) is not True:
                missing.append(f"ancestry_{field}")
        if not _text(ancestry.get("source")):
            missing.append("ancestry_source")
    return missing


def compare_bindings(previous: Mapping[str, Any] | None, current: Mapping[str, Any]) -> dict[str, Any]:
    previous = previous or {}
    fields = ("repository", "pr_number", "owner", "head_sha", "base_sha", "base_ref", "queue_entry_id", "queue_entry_ref", "merge_group_sha", "merge_group_source", "queue_base_sha", "queue_base_ref", "queue_state", "queue_attempt", "ancestry", "ruleset_generation", "ruleset_ids")
    changed = {field: {"previous": previous.get(field), "current": current.get(field)} for field in fields if previous.get(field) not in (None, "", []) and current.get(field) not in (None, "", []) and previous.get(field) != current.get(field)}
    head_replaced = "head_sha" in changed
    ruleset_changed = "ruleset_generation" in changed
    queue_changed = any(field in changed for field in ("queue_entry_id", "queue_entry_ref", "merge_group_sha", "queue_base_sha", "queue_state", "queue_attempt", "ancestry"))
    base_mismatch = bool(current.get("base_sha") and current.get("queue_base_sha") and current["base_sha"] != current["queue_base_sha"])
    ref_mismatch = bool(current.get("base_ref") and current.get("queue_base_ref") and current["base_ref"] != current["queue_base_ref"])
    return {"valid": not changed and not base_mismatch and not ref_mismatch, "changed": changed, "head_replaced": head_replaced, "ruleset_changed": ruleset_changed, "queue_identity_changed": queue_changed, "queue_base_mismatch": base_mismatch, "queue_base_ref_mismatch": ref_mismatch, "invalidated_workflow_run_ids": list(previous.get("workflow_run_ids") or []) if head_replaced or queue_changed or ruleset_changed else []}


validate_identity_binding = compare_bindings


def _candidate_status_action(owner: Sequence[Mapping[str, Any]], independent: Sequence[Mapping[str, Any]], unknown: Sequence[Mapping[str, Any]], queue_state: str) -> tuple[list[str], str]:
    if queue_state not in KNOWN_QUEUE_STATES:
        return [UNKNOWN_QUEUE_STATE_ACTION], "unknown_queue_state"
    if unknown or any(_upper(x.get("state")) not in KNOWN_QUEUE_STATES for x in (*owner, *independent)):
        return [UNKNOWN_QUEUE_STATE_ACTION], "unknown_candidate_state"
    if queue_state in UNMERGEABLE_STATES or any(_upper(x.get("state")) in UNMERGEABLE_STATES for x in owner):
        return [OWNER_UNMERGEABLE_ACTION], "owner_unmergeable"
    if any(_upper(x.get("state")) in FAILED_QUEUE_STATES for x in owner):
        return [QUEUE_FAILURE_ACTION], "owner_queue_failure"
    if any(_upper(x.get("state")) in UNMERGEABLE_STATES for x in independent):
        return [IDLE_ACTION], "independent_unmergeable_only"
    return [IDLE_ACTION], "no_owner_action"


def reconcile_snapshot(*, pr: Mapping[str, Any], queue_entry: Mapping[str, Any] | None, candidates: Iterable[Mapping[str, Any]] | None = None, workflow_runs: Iterable[Mapping[str, Any]] | None = None, rulesets: Iterable[Mapping[str, Any]] | Mapping[str, Any] | None = None, observed_ruleset_generation: Any = None, previous_binding: Mapping[str, Any] | None = None, require_queue: bool = True, thread_state: Mapping[str, Any] | None = None) -> dict[str, Any]:
    identity = owner_identity_from_pr(pr)
    repo_data = _first(pr, "repository", "repo")
    default_branch = _text(_first(pr, "default_branch", "defaultBranch") or (_first(repo_data, "default_branch", "defaultBranch") if isinstance(repo_data, Mapping) else ""))
    ruleset = normalize_ruleset_readback(rulesets, observed_ruleset_generation, base_ref=identity.get("base_ref", ""), default_branch=default_branch)
    queue = normalize_queue_entry(queue_entry)
    binding = build_binding(pr, queue, workflow_runs, ruleset)
    missing = binding_missing(binding, require_queue=require_queue)
    if ruleset["active_ruleset_count"] == 0:
        missing.append("active_ruleset")
    comparison = compare_bindings(previous_binding, binding)
    queue_absent = queue is None
    classified = classify_candidates(candidates or [], {"pr_number": binding.get("pr_number")})
    workflow = workflow_evidence(binding.get("workflow_runs", []), binding.get("merge_group_sha", ""), binding.get("queue_attempt"))
    actions, disposition = ([IDENTITY_MISMATCH_ACTION], "identity_unbound") if missing or not ruleset["matches_observed"] or queue_absent else ([HEAD_REPLACED_ACTION], "head_replaced") if comparison["head_replaced"] else ([RULESET_CHANGED_ACTION if comparison["ruleset_changed"] else IDENTITY_MISMATCH_ACTION], "identity_changed") if comparison["ruleset_changed"] or comparison["queue_identity_changed"] else ([IDENTITY_MISMATCH_ACTION], "queue_base_mismatch") if comparison["queue_base_mismatch"] or comparison["queue_base_ref_mismatch"] else _candidate_status_action(classified["owner"], classified["independent"], classified["unknown"], binding.get("queue_state", ""))
    workflow_mismatches = [run for run in binding.get("workflow_runs", []) if binding.get("merge_group_sha") and run.get("head_sha") != binding.get("merge_group_sha")]
    owner_missing = [x for x in classified["owner"] if any(not x.get(field) for field in ("owner", "head_sha", "base_sha", "queue_entry_id", "merge_group_sha", "base_ref")) or any(not _is_full_sha(x.get(field)) for field in ("head_sha", "base_sha", "merge_group_sha"))]
    if not classified["owner"] and not queue_absent:
        owner_missing.append({"reason": "owner_pr_candidate_missing"})
    owner_mismatch = [x for x in classified["owner"] if any(x.get(field) and binding.get(bind) and x.get(field) != binding.get(bind) for field, bind in (("owner", "owner"), ("head_sha", "head_sha"), ("base_sha", "base_sha"), ("base_ref", "base_ref"), ("queue_entry_id", "queue_entry_id"), ("merge_group_sha", "merge_group_sha")))]
    if comparison["head_replaced"]:
        actions, disposition = [HEAD_REPLACED_ACTION], "head_replaced"
    elif workflow_mismatches:
        actions, disposition = [IDENTITY_MISMATCH_ACTION], "workflow_identity_mismatch"
    elif owner_mismatch or owner_missing:
        actions, disposition = [IDENTITY_MISMATCH_ACTION], "owner_candidate_identity_mismatch"
    elif not workflow["valid"]:
        actions, disposition = [IDENTITY_MISMATCH_ACTION], "workflow_evidence_invalid"
    identity_valid = not missing and not queue_absent and ruleset["matches_observed"] and not workflow_mismatches and not owner_mismatch and not owner_missing and not classified["unknown"] and comparison["valid"]
    allgreen = identity_valid and workflow["valid"] and binding.get("queue_state") not in (UNMERGEABLE_STATES | FAILED_QUEUE_STATES) and not any(_upper(x.get("state")) in (UNMERGEABLE_STATES | FAILED_QUEUE_STATES) for x in classified["owner"])
    return {
        "helper_version": HELPER_VERSION, "read_only": True, "repository": binding.get("repository"),
        "pr": {"number": binding.get("pr_number"), "owner": binding.get("owner"), "head_sha": binding.get("head_sha"), "base_sha": binding.get("base_sha"), "base_ref": binding.get("base_ref")},
        "queue_entry": queue,
        "merge_group": {"queue_entry_ref": binding.get("queue_entry_ref"), "synthetic_sha": binding.get("merge_group_sha"), "synthetic_source": binding.get("merge_group_source"), "base_sha": binding.get("queue_base_sha"), "base_ref": binding.get("queue_base_ref"), "attempt": binding.get("queue_attempt"), "ancestry": binding.get("ancestry")},
        "workflow_runs": binding.get("workflow_runs", []), "workflow_run_ids": binding.get("workflow_run_ids", []), "workflow_evidence": workflow,
        "labels": ["ALLGREEN"] if allgreen else [], "allgreen": allgreen,
        "wait_contract": {"helper": DELEGATED_WATCH_HELPER, "mode": DELEGATED_WATCH_MODE, "queue_event_coverage": "not-covered", "requires_authoritative_rehydration": True},
        "ruleset": ruleset, "ruleset_generation": binding.get("ruleset_generation"), "thread_state": dict(thread_state) if isinstance(thread_state, Mapping) else {}, "binding": binding,
        "identity": {"missing": missing, "queue_absent": queue_absent, "comparison": comparison, "workflow_mismatches": workflow_mismatches, "owner_candidate_mismatches": owner_mismatch, "owner_candidate_missing": owner_missing, "valid": identity_valid},
        "candidates": classified, "disposition": disposition, "actions": actions,
        "continuation": {"owner_entry_continues": bool(classified["owner"]), "independent_entries_continue": True, "provider_mutation": False},
    }


build_snapshot = reconcile_snapshot


class ReadOnlyGitHubProvider:
    def __init__(self, repo: str, pr_number: int, runner: Callable[..., Any] | None = None):
        self.repo, self.pr_number, self._runner = repo, int(pr_number), runner or run_gh_json

    def read_pr(self) -> Mapping[str, Any]:
        owner, name = self.repo.split("/", 1)
        payload = self._runner(["api", f"repos/{owner}/{name}/pulls/{self.pr_number}", "--method", "GET"])
        if not isinstance(payload, Mapping):
            raise QueueObserverError("pull request read returned a non-object payload")
        if str(_first(payload, "number", "pr_number", "pull_request_number")) != str(self.pr_number):
            raise QueueObserverError("pull request read returned a different PR number")
        returned_repo = _repo_slug(_first(payload, "repository", "repo", "url"))
        if returned_repo and returned_repo.casefold() != self.repo.casefold():
            raise QueueObserverError("pull request read returned a different repository")
        enriched = dict(payload); enriched.setdefault("repository", self.repo)
        return enriched

    def read_queue_entry(self) -> Mapping[str, Any] | None:
        owner, name = self.repo.split("/", 1)
        payload = self._runner(["api", "graphql", "-f", f"query={MERGE_QUEUE_QUERY}", "-F", f"owner={owner}", "-F", f"name={name}"])
        if not isinstance(payload, Mapping) or payload.get("errors"):
            raise QueueObserverError("merge queue GraphQL response contained errors")
        data = payload.get("data")
        repository = data.get("repository") if isinstance(data, Mapping) else None
        queue = repository.get("mergeQueue") if isinstance(repository, Mapping) else None
        if queue is None:
            return None
        entries = queue.get("entries", {}).get("nodes", []) if isinstance(queue.get("entries"), Mapping) else []
        owner_entry, candidates = None, []
        for entry in entries:
            if not isinstance(entry, Mapping) or not isinstance(entry.get("pullRequest"), Mapping):
                continue
            pull = entry["pullRequest"]
            try:
                number = int(pull.get("number"))
            except (TypeError, ValueError):
                continue
            head = entry.get("headCommit") if isinstance(entry.get("headCommit"), Mapping) else {}
            synthetic = head.get("oid")
            candidates.append({"number": number, "author": pull.get("author"), "headRefOid": pull.get("headRefOid"), "baseRefOid": pull.get("baseRefOid"), "baseRefName": pull.get("baseRefName"), "queueEntryId": entry.get("id"), "mergeGroupSha": synthetic, "state": entry.get("state")})
            if number == self.pr_number:
                base = entry.get("baseCommit") if isinstance(entry.get("baseCommit"), Mapping) else {}
                owner_entry = {"id": entry.get("id"), "queueEntryRef": entry.get("queueEntryRef"), "position": entry.get("position"), "state": entry.get("state"), "merge_group_sha": synthetic, "merge_group_source": "MergeQueueEntry.headCommit.oid", "attempt": entry.get("attempt"), "ancestry": entry.get("ancestryEvidence"), "baseSha": base.get("oid"), "baseRefName": pull.get("baseRefName")}
        if owner_entry is not None:
            owner_entry["pullRequests"] = candidates
        return owner_entry

    def read_rulesets(self) -> Sequence[Mapping[str, Any]] | Mapping[str, Any]:
        owner, name = self.repo.split("/", 1)
        payload = self._runner(["api", f"repos/{owner}/{name}/rulesets", "--method", "GET"])
        return payload if isinstance(payload, (list, Mapping)) else []

    def read_workflow_runs(self, head_sha: str) -> Sequence[Mapping[str, Any]]:
        owner, name = self.repo.split("/", 1)
        payload = self._runner(["api", f"repos/{owner}/{name}/actions/runs", "--method", "GET", "-f", f"head_sha={head_sha}", "-f", "per_page=100"])
        return payload.get("workflow_runs", []) if isinstance(payload, Mapping) else []


def run_gh_json(command: Sequence[str]) -> Any:
    command = list(command)
    if command[:1] != ["api"]:
        raise QueueObserverError("queue observer permits only gh api reads")
    if "--method" in command and command[command.index("--method") + 1].upper() != "GET":
        raise QueueObserverError(f"queue observer forbids gh api method {command[command.index('--method') + 1].upper()}")
    if command[1:2] == ["graphql"]:
        values = [value[6:] for value in command if value.startswith("query=")]
        if not values or not values[0].lstrip().startswith(("query", "{")):
            raise QueueObserverError("queue observer permits only GraphQL read queries")
    try:
        result = subprocess.run(["gh", *command], check=True, capture_output=True, text=True)
    except (FileNotFoundError, subprocess.CalledProcessError) as exc:
        raise QueueObserverError(f"read-only gh api failed: {exc}") from exc
    try:
        return json.loads(result.stdout or "null")
    except json.JSONDecodeError as exc:
        raise QueueObserverError("read-only gh api returned invalid JSON") from exc


def snapshot_from_provider(provider: ReadOnlyGitHubProvider, *, previous_binding: Mapping[str, Any] | None = None, require_queue: bool = True) -> dict[str, Any]:
    pr = provider.read_pr(); entry = provider.read_queue_entry(); normalized = normalize_queue_entry(entry); synthetic = (normalized or {}).get("synthetic_sha", "")
    return reconcile_snapshot(pr=pr, queue_entry=entry, candidates=(normalized or {}).get("pull_requests", []), workflow_runs=provider.read_workflow_runs(synthetic) if synthetic else [], rulesets=provider.read_rulesets(), previous_binding=previous_binding, require_queue=require_queue)


def _pr_repo_and_number(pr: str, repo: str | None) -> tuple[str, int]:
    if pr.isdigit():
        if not repo:
            raise QueueObserverError("bare PR numbers require --repo OWNER/REPO")
        return repo, int(pr)
    parts = [p for p in urlparse(pr).path.split("/") if p]
    if len(parts) < 4 or parts[2] != "pull" or not parts[3].isdigit():
        raise QueueObserverError("--pr must be a PR URL or a number with --repo")
    resolved = f"{parts[0]}/{parts[1]}"
    if repo and repo.casefold() != resolved.casefold():
        raise QueueObserverError("--repo does not match --pr URL")
    return resolved, int(parts[3])


def delegate_bounded_watcher(pr: str, repo: str | None = None, *, runner: Callable[..., Any] | None = None) -> dict[str, Any]:
    command = [sys.executable, str(Path(__file__).with_name("gh_pr_watch.py")), "--pr", pr]
    if repo:
        command += ["--repo", repo]
    command += ["--watch-until-action"]
    try:
        completed = (runner or subprocess.run)(command, check=True, capture_output=True, text=True)
    except (OSError, subprocess.CalledProcessError) as exc:
        raise QueueObserverError(f"bounded PR watcher failed: {exc}") from exc
    try:
        receipt = json.loads(completed.stdout or "")
    except json.JSONDecodeError as exc:
        raise QueueObserverError("bounded PR watcher returned invalid JSON") from exc
    if not isinstance(receipt, dict):
        raise QueueObserverError("bounded PR watcher returned a non-object receipt")
    return receipt


def delegated_receipt_fingerprint(receipt: Mapping[str, Any]) -> str:
    return _digest({key: receipt.get(key) for key in ("helper", "helper_version", "mode", "read_only", "queue_event_coverage", "owner", "run_set", "required_conclusions", "thread_state", "exit_reason")})


def validate_delegated_receipt(receipt: Mapping[str, Any], current: Mapping[str, Any]) -> dict[str, Any]:
    if not isinstance(receipt, Mapping):
        raise QueueObserverError("delegated watcher returned a non-object receipt")
    required = ("helper", "helper_version", "mode", "read_only", "queue_event_coverage", "owner", "run_set", "required_conclusions", "thread_state", "exit_reason", "fingerprint")
    missing = [key for key in required if key not in receipt]
    if missing:
        raise QueueObserverError("delegated watcher receipt omitted provenance: " + ",".join(missing))
    if receipt["helper"] != DELEGATED_WATCH_HELPER or receipt["helper_version"] != DELEGATED_WATCH_HELPER_VERSION or receipt["mode"] != DELEGATED_WATCH_MODE:
        raise QueueObserverError("delegated watcher helper/mode identity mismatch")
    if receipt["read_only"] is not True or receipt["queue_event_coverage"] != "pr_local_only":
        raise QueueObserverError("delegated watcher does not provide bounded read-only PR-local coverage")
    if _text(receipt["exit_reason"]) not in RECOGNIZED_WATCH_EXITS:
        raise QueueObserverError("delegated watcher returned an unrecognized exit")
    current_pr = current.get("pr") if isinstance(current, Mapping) else None
    if not isinstance(current_pr, Mapping):
        raise QueueObserverError("current snapshot omitted PR identity")
    expected_owner = {"repository": current.get("repository"), "pr_number": current_pr.get("number"), "owner": current_pr.get("owner"), "head_sha": current_pr.get("head_sha"), "base_sha": current_pr.get("base_sha")}
    if receipt["owner"] != expected_owner:
        raise QueueObserverError("delegated watcher owner identity mismatch")
    delegated = receipt.get("snapshot", {}).get("pr") if isinstance(receipt.get("snapshot"), Mapping) else None
    delegated_identity = {"repository": delegated.get("repo", delegated.get("repository")) if isinstance(delegated, Mapping) else None, "pr_number": delegated.get("number", delegated.get("pr_number")) if isinstance(delegated, Mapping) else None, "owner": delegated.get("owner") if isinstance(delegated, Mapping) else None, "head_sha": delegated.get("head_sha") if isinstance(delegated, Mapping) else None, "base_sha": delegated.get("base_sha") if isinstance(delegated, Mapping) else None}
    if delegated_identity != expected_owner:
        raise QueueObserverError("delegated watcher snapshot identity mismatch")
    if (current.get("workflow_evidence") or {}).get("valid") is not True or receipt["run_set"] != [{"run_id": x.get("run_id"), "attempt": x.get("run_attempt")} for x in current.get("workflow_runs", [])]:
        raise QueueObserverError("delegated watcher run set/attempt mismatch")
    if receipt["required_conclusions"] != {name: "success" for name in REQUIRED_WORKFLOWS}:
        raise QueueObserverError("delegated watcher required conclusions mismatch")
    if not isinstance(receipt["thread_state"], Mapping) or not receipt["thread_state"] or dict(receipt["thread_state"]) != dict(current.get("thread_state") or {}):
        raise QueueObserverError("delegated watcher thread state is not current")
    if receipt["fingerprint"] != delegated_receipt_fingerprint(receipt):
        raise QueueObserverError("delegated watcher provenance fingerprint mismatch")
    return {"validated": True, "helper": receipt["helper"], "helper_version": receipt["helper_version"], "mode": receipt["mode"], "queue_event_coverage": receipt["queue_event_coverage"], "exit_reason": receipt["exit_reason"], "identity": expected_owner, "run_set": receipt["run_set"], "thread_state": dict(receipt["thread_state"]), "fingerprint": receipt["fingerprint"]}


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Observe GitHub merge-queue state without mutation")
    parser.add_argument("--pr", required=True); parser.add_argument("--repo"); parser.add_argument("--once", action="store_true"); parser.add_argument("--watch-until-action", action="store_true"); parser.add_argument("--allow-no-queue", action="store_true")
    args = parser.parse_args(argv)
    if args.once and args.watch_until_action:
        parser.error("choose only one of --once or --watch-until-action")
    if not args.once and not args.watch_until_action:
        args.once = True
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv); repo, number = _pr_repo_and_number(args.pr, args.repo)
    delegated = delegate_bounded_watcher(args.pr, args.repo) if args.watch_until_action else None
    snapshot = snapshot_from_provider(ReadOnlyGitHubProvider(repo, number), require_queue=not args.allow_no_queue)
    if delegated is not None:
        snapshot["delegated_pr_watcher"] = validate_delegated_receipt(delegated, snapshot)
    print(json.dumps(snapshot, sort_keys=True, indent=2)); return 0


if __name__ == "__main__":
    raise SystemExit(main())
