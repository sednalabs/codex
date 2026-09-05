#!/usr/bin/env python3
"""Read-only, fail-closed observation of one protected merge-queue entry.

This module is deliberately one-shot.  It never delegates to the PR watcher,
polls, or mutates; callers must obtain a fresh authoritative queue snapshot
when a separately-owned PR-local wait wakes.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import re
import subprocess
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
ACTIVE_QUEUE_STATES = {"AWAITING_CHECKS", "QUEUED", "IN_PROGRESS", "EXPECTED_HEAD_SHA", "PENDING"}
UNMERGEABLE_STATES = {"UNMERGEABLE", "UNMERGEABLE_PR", "CONFLICTING"}
FAILED_QUEUE_STATES = {"FAILED", "REMOVED", "CANCELLED", "ERROR"}
KNOWN_QUEUE_STATES = ACTIVE_QUEUE_STATES | UNMERGEABLE_STATES | FAILED_QUEUE_STATES
FULL_SHA = re.compile(r"^[0-9a-fA-F]{40}$")
RUN_ID = re.compile(r"^[1-9][0-9]*$")
ALLOWED_ANCESTRY_SOURCES = {"hosted-static-ancestry-v1"}

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
        run_id, run_id_alias_conflict = _resolve_aliases(
            [(run, ("id", "databaseId", "run_id"))], normalize=_positive_id
        )
        attempt = _first(run, "run_attempt", "runAttempt", "attempt")
        if isinstance(attempt, str) and attempt.strip().isdigit():
            attempt = int(attempt.strip())
        row = {
            "run_id": _positive_id(run_id),
            "workflow": _text(_first(run, "workflow", "workflow_name", "workflowName", "name")),
            "event": _text(_first(run, "event", "event_name")),
            "head_sha": _text(_first(run, "head_sha", "headSha", "headShaOid", "sha")),
            "status": _upper(_first(run, "status", "state")),
            "conclusion": _lower(_first(run, "conclusion", "result")),
            "run_attempt": attempt,
        }
        if run_id_alias_conflict:
            row["run_id_alias_conflict"] = True
        normalized.append(row)
    return sorted(normalized, key=lambda row: (row["run_id"], row["workflow"]))


def workflow_evidence(runs: Iterable[Mapping[str, Any]] | None, synthetic_sha: str, expected_attempt: Any = None) -> dict[str, Any]:
    rows = normalize_workflow_runs(runs)
    reasons: set[str] = set()
    selected: dict[str, dict[str, Any]] = {}
    attempts: set[int] = set()
    run_id_counts: dict[str, int] = {}
    for row in rows:
        run_id = row["run_id"]
        if run_id:
            run_id_counts[run_id] = run_id_counts.get(run_id, 0) + 1
    if not rows:
        reasons.add("empty_run_set")
    for row in rows:
        run_id = row["run_id"]
        run_id_valid = bool(RUN_ID.fullmatch(run_id))
        duplicate_run_id = bool(run_id and run_id_counts.get(run_id, 0) > 1)
        run_id_alias_conflict = bool(row.get("run_id_alias_conflict"))
        name = row["workflow"]
        if not run_id:
            reasons.add("run_id_missing")
        elif not run_id_valid:
            reasons.add("run_id_malformed")
        if run_id_alias_conflict:
            reasons.add("run_id_alias_conflict")
        if duplicate_run_id:
            reasons.add("duplicate_run_id")
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
        # Required-workflow selection happens only after the provider identity
        # has supplied one well-formed, unique run identifier.  A malformed or
        # repeated ID can never satisfy a required conclusion by position.
        if not run_id_valid or duplicate_run_id or run_id_alias_conflict:
            continue
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
    conflicts: list[str] = []
    group, group_conflict = _resolve_aliases(
        [(raw, ("merge_group", "mergeGroup"))],
        normalize=lambda value: dict(value) if isinstance(value, Mapping) else value,
    )
    if group_conflict:
        conflicts.append("merge_group")
    group = group if isinstance(group, Mapping) else {}
    ancestry, ancestry_conflict = _resolve_aliases(
        [(raw, ("ancestry_evidence", "ancestryEvidence", "containment_evidence", "containment", "ancestry"))],
        normalize=lambda value: dict(value) if isinstance(value, Mapping) else value,
    )
    if ancestry_conflict:
        conflicts.append("ancestry")
    ancestry = ancestry if isinstance(ancestry, Mapping) else {}
    queue_entry_id, alias_conflict = _resolve_aliases(
        [(raw, ("id", "databaseId", "queue_entry_id", "queueEntryId"))], normalize=_positive_id
    )
    if alias_conflict:
        conflicts.append("queue_entry_id")
    queue_entry_ref, alias_conflict = _resolve_aliases(
        [(raw, ("queue_entry_ref", "queueEntryRef"))], normalize=_text
    )
    if alias_conflict:
        conflicts.append("queue_entry_ref")
    state, alias_conflict = _resolve_aliases(
        [(raw, ("state", "status"))], normalize=_upper
    )
    if alias_conflict:
        conflicts.append("state")
    position, alias_conflict = _resolve_aliases(
        [(raw, ("position", "queue_position"))]
    )
    if alias_conflict:
        conflicts.append("position")
    attempt, alias_conflict = _resolve_aliases(
        [(raw, ("attempt", "run_attempt", "runAttempt", "queue_attempt"))], normalize=_normalize_attempt
    )
    if alias_conflict:
        conflicts.append("attempt")
    synthetic_sha, alias_conflict = _resolve_aliases(
        [
            (raw, ("merge_group_sha", "mergeGroupSha", "synthetic_sha")),
            (group, ("head_sha", "headSha", "oid", "sha")),
        ],
        normalize=_text,
    )
    if alias_conflict:
        conflicts.append("synthetic_sha")
    base_sha, alias_conflict = _resolve_aliases(
        [
            (raw, ("merge_group_base_sha", "mergeGroupBaseSha", "base_sha", "baseSha")),
            (group, ("base_sha", "baseSha", "base_oid", "baseOid")),
        ],
        normalize=_text,
    )
    if alias_conflict:
        conflicts.append("base_sha")
    base_ref, alias_conflict = _resolve_aliases(
        [(raw, ("base_ref", "baseRefName")), (group, ("base_ref", "baseRefName"))], normalize=_text
    )
    if alias_conflict:
        conflicts.append("base_ref")
    synthetic_source, alias_conflict = _resolve_aliases(
        [(raw, ("merge_group_source", "synthetic_source")), (group, ("source",))], normalize=_text
    )
    if alias_conflict:
        conflicts.append("synthetic_source")
    pull_requests, alias_conflict = _resolve_aliases(
        [
            (raw, ("pull_requests", "pullRequests", "entries")),
            (group, ("pull_requests", "pullRequests", "entries")),
        ],
        normalize=_nodes,
    )
    if alias_conflict:
        conflicts.append("pull_requests")
    result = {
        "queue_entry_id": _positive_id(queue_entry_id),
        "queue_entry_ref": _text(queue_entry_ref),
        "state": _upper(state),
        "position": position,
        "attempt": attempt,
        "synthetic_sha": _text(synthetic_sha),
        "base_sha": _text(base_sha),
        "base_ref": _text(base_ref),
        "synthetic_source": _text(synthetic_source),
        "ancestry": dict(ancestry),
        "merge_group": dict(group),
        "pull_requests": list(pull_requests or []),
    }
    if conflicts:
        result["alias_conflicts"] = sorted(set(conflicts))
    return result


def normalize_candidate(raw: Mapping[str, Any]) -> dict[str, Any]:
    if not isinstance(raw, Mapping):
        raise QueueObserverError("queue candidate payload is not an object")
    conflicts: list[str] = []
    nested, nested_conflict = _resolve_aliases(
        [(raw, ("queue_entry", "queueEntry", "merge_queue_entry", "mergeQueueEntry"))],
        normalize=lambda value: dict(value) if isinstance(value, Mapping) else value,
    )
    if nested_conflict:
        conflicts.append("queue_entry")
    nested = nested if isinstance(nested, Mapping) else {}
    number, alias_conflict = _resolve_aliases(
        [(raw, ("pr_number", "number", "pull_request_number", "pullRequestNumber"))], normalize=_normalize_number
    )
    if alias_conflict:
        conflicts.append("pr_number")
    try:
        number = int(number) if number is not None else None
    except (TypeError, ValueError):
        number = None
    owner, alias_conflict = _resolve_aliases(
        [(raw, ("owner", "author", "user")), (nested, ("owner", "author", "user"))], normalize=_login
    )
    if alias_conflict:
        conflicts.append("owner")
    head_sha, alias_conflict = _resolve_aliases(
        [(raw, ("head_sha", "headSha", "headRefOid")), (nested, ("head_sha", "headSha", "headRefOid"))], normalize=_text
    )
    if alias_conflict:
        conflicts.append("head_sha")
    base_sha, alias_conflict = _resolve_aliases(
        [(raw, ("base_sha", "baseSha", "baseRefOid")), (nested, ("base_sha", "baseSha", "baseRefOid"))], normalize=_text
    )
    if alias_conflict:
        conflicts.append("base_sha")
    base_ref, alias_conflict = _resolve_aliases(
        [(raw, ("base_ref", "baseRefName")), (nested, ("base_ref", "baseRefName"))], normalize=_text
    )
    if alias_conflict:
        conflicts.append("base_ref")
    queue_entry_id, alias_conflict = _resolve_aliases(
        [(raw, ("queue_entry_id", "queueEntryId")), (nested, ("queue_entry_id", "queueEntryId", "id", "databaseId"))], normalize=_positive_id
    )
    if alias_conflict:
        conflicts.append("queue_entry_id")
    state, alias_conflict = _resolve_aliases(
        [(raw, ("state", "status")), (nested, ("state", "status"))], normalize=_upper
    )
    if alias_conflict:
        conflicts.append("state")
    merge_group_sha, alias_conflict = _resolve_aliases(
        [(raw, ("merge_group_sha", "mergeGroupSha", "synthetic_sha")), (nested, ("merge_group_sha", "mergeGroupSha", "synthetic_sha"))], normalize=_text
    )
    if alias_conflict:
        conflicts.append("merge_group_sha")
    result = {
        "pr_number": number,
        "owner": _login(owner),
        "head_sha": _text(head_sha),
        "base_sha": _text(base_sha),
        "base_ref": _text(base_ref),
        "queue_entry_id": _positive_id(queue_entry_id),
        "state": _upper(state),
        "merge_group_sha": _text(merge_group_sha),
    }
    if conflicts:
        result["alias_conflicts"] = sorted(set(conflicts))
    return result


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


def _resolve_aliases(
    sources: Iterable[tuple[Mapping[str, Any], Iterable[str]]],
    *,
    normalize: Callable[[Any], Any] = lambda value: value,
) -> tuple[Any, bool]:
    """Resolve aliases without allowing conflicting provider shapes to win.

    Provider responses sometimes expose both REST-style and GraphQL-style
    names.  Empty aliases are ignored, but two distinct non-empty values are a
    structural contradiction.  The caller receives ``None`` for a conflict so
    normal binding validation fails closed instead of selecting the first key.
    """

    values: list[Any] = []
    for mapping, names in sources:
        if not isinstance(mapping, Mapping):
            continue
        for name in names:
            if name not in mapping or mapping[name] is None:
                continue
            value = normalize(mapping[name])
            if value in (None, "", []):
                continue
            values.append(value)
    if not values:
        return None, False
    first = values[0]
    conflict = any(_canonical(value) != _canonical(first) for value in values[1:])
    return (None if conflict else first), conflict


def _normalize_attempt(value: Any) -> Any:
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return value


def _normalize_number(value: Any) -> Any:
    try:
        return int(value) if value is not None and not isinstance(value, bool) else value
    except (TypeError, ValueError):
        return value


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
        "queue_alias_conflicts": list((queue or {}).get("alias_conflicts") or []),
        "workflow_run_ids": [run["run_id"] for run in runs],
        "workflow_runs": runs,
        "ruleset_generation": _text(ruleset_readback.get("generation")),
        "ruleset_ids": list(ruleset_readback.get("applicable_ruleset_ids") or []),
    })
    return binding


def binding_missing(binding: Mapping[str, Any], *, require_queue: bool = True) -> list[str]:
    required = ["repository", "pr_number", "owner", "head_sha", "base_sha", "base_ref"]
    if require_queue:
        required += ["queue_entry_id", "queue_entry_ref", "merge_group_sha", "merge_group_source", "queue_base_sha", "queue_base_ref", "queue_state", "queue_attempt"]
    missing = [name for name in required if binding.get(name) in (None, "", [])]
    alias_conflicts = list(binding.get("queue_alias_conflicts") or [])
    if alias_conflicts:
        missing.append("queue_alias_conflict")
        missing.extend(f"queue_alias_conflict_{field}" for field in alias_conflicts)
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
    attempt = binding.get("queue_attempt")
    if isinstance(attempt, bool) or not isinstance(attempt, int) or attempt <= 0:
        missing.append("queue_attempt_valid")
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
        source = _text(ancestry.get("source"))
        if not source:
            missing.append("ancestry_source")
        elif source not in ALLOWED_ANCESTRY_SOURCES:
            missing.append("ancestry_source_untrusted")
    return missing


def compare_bindings(previous: Mapping[str, Any] | None, current: Mapping[str, Any]) -> dict[str, Any]:
    previous = previous or {}
    fields = ("repository", "pr_number", "owner", "head_sha", "base_sha", "base_ref", "queue_entry_id", "queue_entry_ref", "merge_group_sha", "merge_group_source", "queue_base_sha", "queue_base_ref", "queue_state", "queue_attempt", "ancestry", "queue_alias_conflicts", "ruleset_generation", "ruleset_ids")
    changed = {field: {"previous": previous.get(field), "current": current.get(field)} for field in fields if previous.get(field) not in (None, "", []) and current.get(field) not in (None, "", []) and previous.get(field) != current.get(field)}
    head_replaced = "head_sha" in changed
    ruleset_changed = "ruleset_generation" in changed
    queue_changed = any(field in changed for field in ("queue_entry_id", "queue_entry_ref", "merge_group_sha", "queue_base_sha", "queue_state", "queue_attempt", "ancestry", "queue_alias_conflicts"))
    base_mismatch = bool(current.get("base_sha") and current.get("queue_base_sha") and current["base_sha"] != current["queue_base_sha"])
    ref_mismatch = bool(current.get("base_ref") and current.get("queue_base_ref") and current["base_ref"] != current["queue_base_ref"])
    return {"valid": not changed and not base_mismatch and not ref_mismatch, "changed": changed, "head_replaced": head_replaced, "ruleset_changed": ruleset_changed, "queue_identity_changed": queue_changed, "queue_base_mismatch": base_mismatch, "queue_base_ref_mismatch": ref_mismatch, "invalidated_workflow_run_ids": list(previous.get("workflow_run_ids") or []) if head_replaced or queue_changed or ruleset_changed else []}


validate_identity_binding = compare_bindings


def _candidate_status_action(owner: Sequence[Mapping[str, Any]], independent: Sequence[Mapping[str, Any]], unknown: Sequence[Mapping[str, Any]], queue_state: str) -> tuple[list[str], str]:
    if queue_state not in KNOWN_QUEUE_STATES:
        return [UNKNOWN_QUEUE_STATE_ACTION], "unknown_queue_state"
    if unknown or any(x.get("alias_conflicts") or _upper(x.get("state")) not in KNOWN_QUEUE_STATES for x in (*owner, *independent)):
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
    owner_missing.extend(
        {"reason": "candidate_alias_conflict", "fields": list(x.get("alias_conflicts") or [])}
        for x in classified["owner"]
        if x.get("alias_conflicts")
    )
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
    external_evidence_required = [
        field for field in ("queue_entry_ref", "queue_attempt", "ancestry") if field in missing
    ]
    return {
        "helper_version": HELPER_VERSION, "read_only": True, "repository": binding.get("repository"),
        "pr": {"number": binding.get("pr_number"), "owner": binding.get("owner"), "head_sha": binding.get("head_sha"), "base_sha": binding.get("base_sha"), "base_ref": binding.get("base_ref")},
        "queue_entry": queue,
        "merge_group": {"queue_entry_ref": binding.get("queue_entry_ref"), "synthetic_sha": binding.get("merge_group_sha"), "synthetic_source": binding.get("merge_group_source"), "base_sha": binding.get("queue_base_sha"), "base_ref": binding.get("queue_base_ref"), "attempt": binding.get("queue_attempt"), "ancestry": binding.get("ancestry")},
        "workflow_runs": binding.get("workflow_runs", []), "workflow_run_ids": binding.get("workflow_run_ids", []), "workflow_evidence": workflow,
        "labels": ["ALLGREEN"] if allgreen else [], "allgreen": allgreen,
        "wait_contract": {"mode": "one-shot", "delegation": "disabled", "pr_local_coverage": "not-covered", "queue_event_coverage": "not-covered", "requires_authoritative_rehydration": True},
        "ruleset": ruleset, "ruleset_generation": binding.get("ruleset_generation"), "thread_state": dict(thread_state) if isinstance(thread_state, Mapping) else {}, "binding": binding,
        "identity": {"missing": missing, "external_evidence_required": external_evidence_required, "queue_absent": queue_absent, "comparison": comparison, "workflow_mismatches": workflow_mismatches, "owner_candidate_mismatches": owner_mismatch, "owner_candidate_missing": owner_missing, "valid": identity_valid},
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
                # The query intentionally asks only for fields in the
                # provider's supported schema.  queueEntryRef, attempt, and
                # ancestryEvidence are not returned here and must remain
                # unbound rather than being inferred from an ID or SHA.
                owner_entry = {"id": entry.get("id"), "position": entry.get("position"), "state": entry.get("state"), "merge_group_sha": synthetic, "merge_group_source": "MergeQueueEntry.headCommit.oid", "baseSha": base.get("oid"), "baseRefName": pull.get("baseRefName")}
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


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Observe GitHub merge-queue state without mutation")
    parser.add_argument("--pr", required=True); parser.add_argument("--repo"); parser.add_argument("--once", action="store_true"); parser.add_argument("--allow-no-queue", action="store_true")
    args = parser.parse_args(argv)
    if not args.once:
        args.once = True
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv); repo, number = _pr_repo_and_number(args.pr, args.repo)
    snapshot = snapshot_from_provider(ReadOnlyGitHubProvider(repo, number), require_queue=not args.allow_no_queue)
    print(json.dumps(snapshot, sort_keys=True, indent=2)); return 0


if __name__ == "__main__":
    raise SystemExit(main())
