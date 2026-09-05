#!/usr/bin/env python3
"""Read-only merge-queue observation for PR babysitting lanes.

The PR watcher is the owner of the GitHub wait cadence.  This module deliberately
does not implement a second polling loop: ``--watch-until-action`` delegates one
blocking invocation to :mod:`gh_pr_watch` and then takes one authoritative queue
snapshot.  The pure reconciliation functions are kept separate so queue state can
be tested without credentials or a network connection.
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

ACTIVE_QUEUE_STATES = {
    "AWAITING_CHECKS",
    "QUEUED",
    "IN_PROGRESS",
    "EXPECTED_HEAD_SHA",
    "PENDING",
}
UNMERGEABLE_STATES = {"UNMERGEABLE", "UNMERGEABLE_PR", "CONFLICTING"}
FAILED_QUEUE_STATES = {"FAILED", "REMOVED", "CANCELLED", "ERROR"}
FULL_SHA = re.compile(r"^[0-9a-fA-F]{40}$")
MERGE_QUEUE_QUERY = """
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    mergeQueue {
      entries(first: 100) {
        nodes {
          id
          position
          state
          baseCommit { oid }
          headCommit { oid }
          pullRequest {
            number
            headRefOid
            baseRefOid
            baseRefName
            headRefName
            author { login }
          }
        }
      }
    }
  }
}
"""


class QueueObserverError(RuntimeError):
    """Raised when a read-only queue observation cannot be trusted."""


def _text(value: Any) -> str:
    return str(value or "").strip()


def _upper(value: Any) -> str:
    return _text(value).upper()


def _first(mapping: Mapping[str, Any], *names: str) -> Any:
    for name in names:
        if name in mapping and mapping[name] is not None:
            return mapping[name]
    return None


def _login(value: Any) -> str:
    if isinstance(value, Mapping):
        return _text(_first(value, "login", "name", "node_id"))
    return _text(value)


def _repo_from_url(value: str) -> str:
    parsed = urlparse(value)
    parts = [part for part in parsed.path.split("/") if part]
    if len(parts) >= 2:
        return f"{parts[0]}/{parts[1]}".removesuffix(".git")
    return ""


def _repo_slug(value: Any) -> str:
    if isinstance(value, Mapping):
        value = _first(value, "nameWithOwner", "fullName", "full_name") or ""
    value = _text(value)
    if "/" in value:
        return value
    return _repo_from_url(value)


def _canonical(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)


def _digest(value: Any) -> str:
    return hashlib.sha256(_canonical(value).encode("utf-8")).hexdigest()


def _positive_id(value: Any) -> str:
    """Return a stable string identifier, rejecting booleans and empty values."""

    if isinstance(value, bool) or value is None:
        return ""
    return _text(value)


def _is_full_sha(value: Any) -> bool:
    return bool(FULL_SHA.fullmatch(_text(value)))


def _ruleset_applies_to_base(item: Mapping[str, Any], base_ref: str) -> bool:
    """Accept only an active branch ruleset whose ref condition covers base."""

    if not _positive_id(_first(item, "id", "databaseId", "database_id")):
        return False
    if not _text(_first(item, "updated_at", "updatedAt", "version")):
        return False
    if _upper(_first(item, "enforcement", "enforcement_status")) != "ACTIVE":
        return False
    if _text(item.get("target")).casefold() != "branch":
        return False
    conditions = item.get("conditions")
    if not isinstance(conditions, Mapping):
        return False
    ref_name = conditions.get("ref_name")
    if not isinstance(ref_name, Mapping):
        return False
    includes = ref_name.get("include")
    if not isinstance(includes, list) or not includes:
        return False
    target = f"refs/heads/{base_ref}"
    return any(
        isinstance(pattern, str)
        and (
            pattern in ("~ALL", "~DEFAULT_BRANCH")
            or fnmatch.fnmatchcase(target, pattern)
            or fnmatch.fnmatchcase(base_ref, pattern)
        )
        for pattern in includes
    )


def _nodes(value: Any) -> list[Any]:
    """Unwrap a GraphQL connection while accepting REST list fixtures."""

    if isinstance(value, Mapping):
        value = value.get("nodes") or value.get("items") or value.get("edges") or []
        if (
            isinstance(value, list)
            and value
            and all(isinstance(item, Mapping) and "node" in item for item in value)
        ):
            value = [item.get("node") for item in value]
    return list(value) if isinstance(value, list) else []


def normalize_workflow_runs(runs: Iterable[Mapping[str, Any]] | None) -> list[dict[str, str]]:
    """Normalize and sort workflow run IDs while retaining their exact head SHA."""

    normalized: list[dict[str, str]] = []
    for run in runs or []:
        if not isinstance(run, Mapping):
            continue
        run_id = _positive_id(_first(run, "id", "databaseId", "run_id"))
        head_sha = _text(_first(run, "head_sha", "headSha", "headShaOid", "sha"))
        if not run_id:
            continue
        normalized.append(
            {
                "run_id": run_id,
                "head_sha": head_sha,
                "status": _text(_first(run, "status", "state")),
                "conclusion": _text(_first(run, "conclusion", "result")),
            }
        )
    return sorted(normalized, key=lambda run: (run["run_id"], run["head_sha"]))


def ruleset_generation(rulesets: Iterable[Mapping[str, Any]] | None) -> str:
    """Derive a deterministic readback generation from active ruleset metadata.

    GitHub's REST ruleset representation has no portable generation counter.  The
    immutable digest below is therefore the generation readback: it changes when
    an active ruleset's identity, update timestamp, enforcement, target, or rule
    payload changes, and it is stable for the same provider response.
    """

    direct_generation = ""
    if isinstance(rulesets, Mapping):
        direct_generation = _text(
            _first(rulesets, "generation", "ruleset_generation", "version")
        )
        rulesets = _first(rulesets, "rulesets", "nodes", "items") or []
    if direct_generation:
        return direct_generation
    rows: list[dict[str, Any]] = []
    for item in rulesets or []:
        if not isinstance(item, Mapping):
            continue
        rows.append(
            {
                "id": _positive_id(_first(item, "id", "databaseId", "database_id")),
                "name": _text(item.get("name")),
                "updated_at": _text(_first(item, "updated_at", "updatedAt")),
                "target": _text(item.get("target")),
                "enforcement": _text(_first(item, "enforcement", "enforcement_status")),
                "rules": item.get("rules") or [],
            }
        )
    rows.sort(key=lambda row: (row["id"], row["name"]))
    return _digest(rows)


def normalize_ruleset_readback(
    rulesets: Iterable[Mapping[str, Any]] | None,
    observed_generation: Any = None,
    *,
    base_ref: str = "",
) -> dict[str, Any]:
    """Return the complete, auditable ruleset generation readback."""

    generation = ruleset_generation(rulesets)
    direct_generation = ""
    if isinstance(rulesets, Mapping):
        direct_generation = _text(
            _first(rulesets, "generation", "ruleset_generation", "version")
        )
        rulesets = _first(rulesets, "rulesets", "nodes", "items") or []
    normalized = []
    for item in rulesets or []:
        if isinstance(item, Mapping):
            normalized.append(dict(item))
    applicable = [
        item for item in normalized
        if base_ref and _ruleset_applies_to_base(item, base_ref)
    ]
    applicable_generation = ruleset_generation(applicable)
    supplied = _text(observed_generation) or direct_generation
    return {
        "generation": applicable_generation,
        "readback_generation": generation,
        "observed_generation": supplied,
        "matches_observed": not supplied or supplied in (generation, applicable_generation),
        "active_ruleset_count": len(applicable),
        "applicable_ruleset_ids": [
            _positive_id(_first(item, "id", "databaseId", "database_id"))
            for item in applicable
        ],
        "applicable_rulesets": applicable,
        "rulesets": normalized,
    }


def normalize_queue_entry(raw: Mapping[str, Any] | None) -> dict[str, Any] | None:
    if not raw:
        return None
    if not isinstance(raw, Mapping):
        raise QueueObserverError("queue entry payload is not an object")
    merge_group = _first(raw, "merge_group", "mergeGroup") or {}
    if not isinstance(merge_group, Mapping):
        merge_group = {}
    queue_id = _positive_id(_first(raw, "id", "databaseId", "queue_entry_id", "queueEntryId"))
    # Only an explicit merge-group field is trusted as G. A raw queue-entry
    # head is deliberately not accepted because GitHub uses that name for both
    # PR and synthetic candidates in different provider surfaces.
    synthetic_sha = _text(
        _first(raw, "merge_group_sha", "mergeGroupSha", "synthetic_sha")
        or _first(merge_group, "head_sha", "headSha", "oid", "sha")
    )
    base_sha = _text(
        _first(raw, "merge_group_base_sha", "mergeGroupBaseSha")
        or _first(merge_group, "base_sha", "baseSha", "base_oid", "baseOid")
        or _first(raw, "base_sha", "baseSha")
    )
    pull_requests = (
        _first(raw, "pull_requests", "pullRequests", "entries")
        or _first(merge_group, "pull_requests", "pullRequests", "entries")
        or []
    )
    return {
        "queue_entry_id": queue_id,
        "queue_entry_ref": _text(_first(raw, "queue_entry_ref", "queueEntryRef"))
        or queue_id,
        "state": _upper(_first(raw, "state", "status")),
        "position": _first(raw, "position", "queue_position"),
        "synthetic_sha": synthetic_sha,
        "base_sha": base_sha,
        "base_ref": _text(_first(raw, "base_ref", "baseRefName"))
        or _text(_first(merge_group, "base_ref", "baseRefName")),
        "synthetic_source": _text(
            _first(raw, "merge_group_source", "synthetic_source")
            or _first(merge_group, "source")
        ),
        "merge_group": dict(merge_group),
        "pull_requests": _nodes(pull_requests),
    }


def normalize_candidate(raw: Mapping[str, Any]) -> dict[str, Any]:
    if not isinstance(raw, Mapping):
        raise QueueObserverError("queue candidate payload is not an object")
    pr_number = _first(raw, "pr_number", "number", "pull_request_number", "pullRequestNumber")
    try:
        pr_number = int(pr_number) if pr_number is not None else None
    except (TypeError, ValueError):
        pr_number = None
    queue = _first(raw, "queue_entry", "queueEntry", "merge_queue_entry", "mergeQueueEntry") or {}
    if not isinstance(queue, Mapping):
        queue = {}
    return {
        "pr_number": pr_number,
        "owner": _login(_first(raw, "owner", "author", "user")),
        "head_sha": _text(_first(raw, "head_sha", "headSha", "headRefOid")),
        "base_sha": _text(_first(raw, "base_sha", "baseSha", "baseRefOid")),
        "base_ref": _text(_first(raw, "base_ref", "baseRefName")),
        "queue_entry_id": _positive_id(
            _first(raw, "queue_entry_id", "queueEntryId", "id", "databaseId")
            or _first(queue, "id", "databaseId")
        ),
        "state": _upper(_first(raw, "state", "status") or _first(queue, "state", "status")),
        "merge_group_sha": _text(
            _first(raw, "merge_group_sha", "mergeGroupSha", "synthetic_sha", "headSha")
            or _first(queue, "merge_group_sha", "mergeGroupSha", "headSha")
        ),
    }


def candidate_is_owner(candidate: Mapping[str, Any], owner: Mapping[str, Any]) -> bool:
    """Match a candidate to the watched PR using immutable identity, not position."""

    candidate_number = candidate.get("pr_number")
    owner_number = owner.get("pr_number")
    if candidate_number is not None and owner_number is not None:
        return int(candidate_number) == int(owner_number)
    candidate_entry = _positive_id(candidate.get("queue_entry_id"))
    owner_entry = _positive_id(owner.get("queue_entry_id"))
    return bool(candidate_entry and owner_entry and candidate_entry == owner_entry)


def classify_candidates(
    candidates: Iterable[Mapping[str, Any]] | None,
    owner: Mapping[str, Any],
) -> dict[str, list[dict[str, Any]]]:
    """Partition queue entries into owner-isolated and independent work."""

    owner_candidates: list[dict[str, Any]] = []
    independent_candidates: list[dict[str, Any]] = []
    for raw in candidates or []:
        candidate = normalize_candidate(raw)
        if candidate_is_owner(candidate, owner):
            candidate["scope"] = "owner"
            owner_candidates.append(candidate)
        else:
            candidate["scope"] = "independent"
            independent_candidates.append(candidate)
    return {
        "owner": owner_candidates,
        "independent": independent_candidates,
    }


# Public descriptive aliases keep the pure seam discoverable to callers that use
# the queue vocabulary rather than the implementation's shorter names.
classify_queue_candidates = classify_candidates


def owner_identity_from_pr(pr: Mapping[str, Any]) -> dict[str, Any]:
    """Build the watched identity from a normalized PR response."""

    number = _first(pr, "number", "pr_number", "pull_request_number")
    try:
        number = int(number) if number is not None else None
    except (TypeError, ValueError):
        number = None
    head = _first(pr, "head") or {}
    base = _first(pr, "base") or {}
    if not isinstance(head, Mapping):
        head = {}
    if not isinstance(base, Mapping):
        base = {}
    return {
        "repository": _repo_slug(_first(pr, "repository", "repo", "url")),
        "pr_number": number,
        "owner": _login(_first(pr, "owner", "author", "user")),
        "head_sha": _text(_first(pr, "head_sha", "headRefOid", "headSha") or head.get("sha")),
        "base_sha": _text(_first(pr, "base_sha", "baseRefOid", "baseSha") or base.get("sha")),
        "base_ref": _text(_first(pr, "base_ref", "baseRefName") or base.get("ref")),
    }


def build_binding(
    pr: Mapping[str, Any],
    queue_entry: Mapping[str, Any] | None,
    workflow_runs: Iterable[Mapping[str, Any]] | None,
    ruleset_readback: Mapping[str, Any],
) -> dict[str, Any]:
    """Bind all queue evidence to one exact PR/head/base and ruleset generation."""

    owner = owner_identity_from_pr(pr)
    queue = normalize_queue_entry(queue_entry)
    runs = normalize_workflow_runs(workflow_runs)
    binding = dict(owner)
    binding.update(
        {
            "queue_entry_id": _text((queue or {}).get("queue_entry_id")),
            "queue_entry_ref": _text((queue or {}).get("queue_entry_ref")),
            "merge_group_sha": _text((queue or {}).get("synthetic_sha")),
            "merge_group_source": _text((queue or {}).get("synthetic_source")),
            "queue_base_sha": _text((queue or {}).get("base_sha")),
            "queue_base_ref": _text((queue or {}).get("base_ref")),
            "workflow_run_ids": [run["run_id"] for run in runs],
            "workflow_runs": runs,
            "ruleset_generation": _text(ruleset_readback.get("generation")),
            "ruleset_ids": list(ruleset_readback.get("applicable_ruleset_ids") or []),
        }
    )
    return binding


def binding_missing(binding: Mapping[str, Any], *, require_queue: bool = True) -> list[str]:
    required = [
        "repository",
        "pr_number",
        "owner",
        "head_sha",
        "base_sha",
        "base_ref",
    ]
    if require_queue:
        required.extend(
            [
                "queue_entry_id",
                "queue_entry_ref",
                "merge_group_sha",
                "merge_group_source",
                "queue_base_sha",
                "queue_base_ref",
            ]
        )
    missing = [name for name in required if binding.get(name) in (None, "", [])]
    if not binding.get("ruleset_generation"):
        missing.append("ruleset_generation")
    ruleset_ids = list(binding.get("ruleset_ids") or [])
    if (
        not ruleset_ids
        or len(ruleset_ids) != len(set(ruleset_ids))
        or any(not _positive_id(value) for value in ruleset_ids)
    ):
        missing.append("ruleset_ids")
    for field in ("head_sha", "base_sha"):
        if binding.get(field) and not _is_full_sha(binding[field]):
            missing.append(f"{field}_full")
    if require_queue:
        for field in ("merge_group_sha", "queue_base_sha"):
            if binding.get(field) and not _is_full_sha(binding[field]):
                missing.append(f"{field}_full")
        if binding.get("merge_group_sha") == binding.get("head_sha"):
            missing.append("merge_group_distinct_from_pr_head")
        if binding.get("merge_group_source") not in {
            "MergeQueueEntry.headCommit.oid",
            "MergeGroup.headSha",
        }:
            missing.append("merge_group_source_untrusted")
    return missing


def compare_bindings(
    previous: Mapping[str, Any] | None,
    current: Mapping[str, Any],
) -> dict[str, Any]:
    """Compare exact identities and report invalidation causes, fail closed."""

    previous = previous or {}
    changed: dict[str, dict[str, Any]] = {}
    fields = (
        "repository",
        "pr_number",
        "owner",
        "head_sha",
        "base_sha",
        "base_ref",
        "queue_entry_id",
        "queue_entry_ref",
        "merge_group_sha",
        "merge_group_source",
        "queue_base_sha",
        "queue_base_ref",
        "ruleset_generation",
        "ruleset_ids",
    )
    for field in fields:
        old = previous.get(field)
        new = current.get(field)
        if old not in (None, "", []) and new not in (None, "", []) and old != new:
            changed[field] = {"previous": old, "current": new}
    head_replaced = "head_sha" in changed
    ruleset_changed = "ruleset_generation" in changed
    queue_identity_changed = any(
        name in changed
        for name in (
            "queue_entry_id",
            "queue_entry_ref",
            "merge_group_sha",
            "queue_base_sha",
        )
    )
    queue_base_mismatch = bool(
        current.get("base_sha")
        and current.get("queue_base_sha")
        and current.get("base_sha") != current.get("queue_base_sha")
    )
    queue_base_ref_mismatch = bool(
        current.get("base_ref")
        and current.get("queue_base_ref")
        and current.get("base_ref") != current.get("queue_base_ref")
    )
    return {
        "valid": not changed and not queue_base_mismatch and not queue_base_ref_mismatch,
        "changed": changed,
        "head_replaced": head_replaced,
        "ruleset_changed": ruleset_changed,
        "queue_identity_changed": queue_identity_changed,
        "queue_base_mismatch": queue_base_mismatch,
        "queue_base_ref_mismatch": queue_base_ref_mismatch,
        "invalidated_workflow_run_ids": list(previous.get("workflow_run_ids") or [])
        if head_replaced or queue_identity_changed or ruleset_changed
        else [],
    }


validate_identity_binding = compare_bindings


def _candidate_status_action(
    owner_candidates: Sequence[Mapping[str, Any]],
    independent_candidates: Sequence[Mapping[str, Any]],
) -> tuple[list[str], str]:
    owner_unmergeable = [
        candidate
        for candidate in owner_candidates
        if _upper(candidate.get("state")) in UNMERGEABLE_STATES
    ]
    independent_unmergeable = [
        candidate
        for candidate in independent_candidates
        if _upper(candidate.get("state")) in UNMERGEABLE_STATES
    ]
    owner_failed = [
        candidate
        for candidate in owner_candidates
        if _upper(candidate.get("state")) in FAILED_QUEUE_STATES
    ]
    if owner_unmergeable:
        return [OWNER_UNMERGEABLE_ACTION], "owner_unmergeable"
    if owner_failed:
        return [QUEUE_FAILURE_ACTION], "owner_queue_failure"
    # Independent failures are explicitly informational; they must not stop the
    # watched owner or cause a queue mutation.  Keep them in classification data.
    if independent_unmergeable:
        return [IDLE_ACTION], "independent_unmergeable_only"
    return [IDLE_ACTION], "no_owner_action"


def reconcile_snapshot(
    *,
    pr: Mapping[str, Any],
    queue_entry: Mapping[str, Any] | None,
    candidates: Iterable[Mapping[str, Any]] | None = None,
    workflow_runs: Iterable[Mapping[str, Any]] | None = None,
    rulesets: Iterable[Mapping[str, Any]] | None = None,
    observed_ruleset_generation: Any = None,
    previous_binding: Mapping[str, Any] | None = None,
    require_queue: bool = True,
) -> dict[str, Any]:
    """Produce one deterministic queue snapshot; never mutates remote state."""

    pr_identity = owner_identity_from_pr(pr)
    ruleset_readback = normalize_ruleset_readback(
        rulesets,
        observed_ruleset_generation,
        base_ref=pr_identity.get("base_ref", ""),
    )
    normalized_queue = normalize_queue_entry(queue_entry)
    normalized_candidates = list(candidates or [])
    binding = build_binding(pr, normalized_queue, workflow_runs, ruleset_readback)
    missing = binding_missing(binding, require_queue=require_queue)
    # A ruleset digest over an empty response is deterministic but does not
    # identify active protection.  Treat an absent readback as unbound rather
    # than allowing a queue snapshot to claim protected readiness.
    if ruleset_readback["active_ruleset_count"] == 0:
        missing.append("active_ruleset")
    comparison = compare_bindings(previous_binding, binding)
    queue_absent = normalized_queue is None
    owner = {
        "pr_number": binding.get("pr_number"),
        "queue_entry_id": binding.get("queue_entry_id"),
    }
    classified = classify_candidates(normalized_candidates, owner)

    actions: list[str]
    if missing or not ruleset_readback["matches_observed"] or queue_absent:
        actions = [IDENTITY_MISMATCH_ACTION]
        disposition = "identity_unbound"
    elif comparison["head_replaced"]:
        actions = [HEAD_REPLACED_ACTION]
        disposition = "head_replaced"
    elif comparison["ruleset_changed"] or comparison["queue_identity_changed"]:
        actions = [
            RULESET_CHANGED_ACTION
            if comparison["ruleset_changed"]
            else IDENTITY_MISMATCH_ACTION
        ]
        disposition = "identity_changed"
    elif comparison["queue_base_mismatch"] or comparison["queue_base_ref_mismatch"]:
        actions = [IDENTITY_MISMATCH_ACTION]
        disposition = "queue_base_mismatch"
    else:
        actions, disposition = _candidate_status_action(
            classified["owner"], classified["independent"]
        )

    # A workflow run is valid evidence only for the exact synthetic merge-group
    # SHA.  An empty run list is allowed while a queue entry is still starting,
    # but an observed run with an absent or mismatched SHA fails closed.
    workflow_mismatches = [
        run
        for run in binding.get("workflow_runs", [])
        if binding.get("merge_group_sha")
        and (
            not run.get("head_sha")
            or run.get("head_sha") != binding.get("merge_group_sha")
        )
    ]
    owner_candidate_missing = [
        candidate
        for candidate in classified["owner"]
        if any(
            not candidate.get(field)
            for field in (
                "owner",
                "head_sha",
                "base_sha",
                "queue_entry_id",
                "merge_group_sha",
                "base_ref",
            )
        )
        or any(
            not _is_full_sha(candidate.get(field))
            for field in ("head_sha", "base_sha", "merge_group_sha")
        )
    ]
    owner_candidate_mismatches = [
        candidate
        for candidate in classified["owner"]
        if (
            candidate.get("owner")
            and binding.get("owner")
            and candidate.get("owner") != binding.get("owner")
        )
        or (
            candidate.get("head_sha")
            and binding.get("head_sha")
            and candidate.get("head_sha") != binding.get("head_sha")
        )
        or (
            candidate.get("base_sha")
            and binding.get("base_sha")
            and candidate.get("base_sha") != binding.get("base_sha")
        )
        or (
            candidate.get("base_ref")
            and binding.get("base_ref")
            and candidate.get("base_ref") != binding.get("base_ref")
        )
        or (
            candidate.get("queue_entry_id")
            and binding.get("queue_entry_id")
            and candidate.get("queue_entry_id") != binding.get("queue_entry_id")
        )
        or (
            candidate.get("merge_group_sha")
            and binding.get("merge_group_sha")
            and candidate.get("merge_group_sha") != binding.get("merge_group_sha")
        )
    ]
    if workflow_mismatches:
        actions = [IDENTITY_MISMATCH_ACTION]
        disposition = "workflow_identity_mismatch"
    elif owner_candidate_mismatches or owner_candidate_missing:
        actions = [IDENTITY_MISMATCH_ACTION]
        disposition = "owner_candidate_identity_mismatch"

    return {
        "helper_version": HELPER_VERSION,
        "read_only": True,
        "repository": binding.get("repository"),
        "pr": {
            "number": binding.get("pr_number"),
            "owner": binding.get("owner"),
            "head_sha": binding.get("head_sha"),
            "base_sha": binding.get("base_sha"),
            "base_ref": binding.get("base_ref"),
        },
        "queue_entry": normalized_queue,
        "merge_group": {
            "queue_entry_ref": binding.get("queue_entry_ref"),
            "synthetic_sha": binding.get("merge_group_sha"),
            "synthetic_source": binding.get("merge_group_source"),
            "base_sha": binding.get("queue_base_sha"),
            "base_ref": binding.get("queue_base_ref"),
        },
        "workflow_runs": binding.get("workflow_runs", []),
        "workflow_run_ids": binding.get("workflow_run_ids", []),
        "ruleset": ruleset_readback,
        "ruleset_generation": binding.get("ruleset_generation"),
        "binding": binding,
        "identity": {
            "missing": missing,
            "queue_absent": queue_absent,
            "comparison": comparison,
            "workflow_mismatches": workflow_mismatches,
            "owner_candidate_mismatches": owner_candidate_mismatches,
            "owner_candidate_missing": owner_candidate_missing,
            "valid": not missing
            and not queue_absent
            and ruleset_readback["matches_observed"]
            and not workflow_mismatches
            and not owner_candidate_mismatches
            and not owner_candidate_missing
            and comparison["valid"],
        },
        "candidates": classified,
        "disposition": disposition,
        "actions": actions,
        "continuation": {
            "owner_entry_continues": bool(classified["owner"]),
            "independent_entries_continue": True,
            "provider_mutation": False,
        },
    }


build_snapshot = reconcile_snapshot


class ReadOnlyGitHubProvider:
    """Small read-only ``gh api`` adapter.

    The adapter has no mutation methods by construction.  Tests inject a provider
    instead of invoking this class, and callers that need long waits delegate to
    the existing PR watcher helper.
    """

    def __init__(self, repo: str, pr_number: int, runner: Callable[..., Any] | None = None):
        self.repo = repo
        self.pr_number = int(pr_number)
        self._runner = runner or run_gh_json

    def read_pr(self) -> Mapping[str, Any]:
        owner, name = self.repo.split("/", 1)
        payload = self._runner(
            [
                "api",
                f"repos/{owner}/{name}/pulls/{self.pr_number}",
                "--method",
                "GET",
            ]
        )
        if not isinstance(payload, Mapping):
            raise QueueObserverError("pull request read returned a non-object payload")
        payload_number = _first(payload, "number", "pr_number", "pull_request_number")
        if payload_number is None or str(payload_number) != str(self.pr_number):
            raise QueueObserverError("pull request read returned a different PR number")
        payload_repo = _repo_slug(_first(payload, "repository", "repo"))
        if not payload_repo:
            payload_repo = _repo_slug(_first(payload, "url"))
        if payload_repo and payload_repo.casefold() != self.repo.casefold():
            raise QueueObserverError("pull request read returned a different repository")
        enriched = dict(payload)
        # REST does not include a top-level repository in every response.  Bind
        # the endpoint's explicit repository rather than guessing from a branch.
        enriched.setdefault("repository", self.repo)
        return enriched

    def read_queue_entry(self) -> Mapping[str, Any] | None:
        # REST does not expose merge-queue entries consistently. Read the
        # repository queue through the currently supported GraphQL schema: a
        # MergeQueueEntry exposes baseCommit/headCommit and pullRequest, while
        # synthetic merge-group fields are not fields on the entry itself.
        owner, name = self.repo.split("/", 1)
        payload = self._runner(
            [
                "api",
                "graphql",
                "-f",
                f"query={MERGE_QUEUE_QUERY}",
                "-F",
                f"owner={owner}",
                "-F",
                f"name={name}",
            ]
        )
        if not isinstance(payload, Mapping):
            raise QueueObserverError("merge queue read returned a non-object payload")
        errors = payload.get("errors")
        if errors:
            raise QueueObserverError("merge queue GraphQL response contained errors")
        data = payload.get("data")
        if not isinstance(data, Mapping) or not isinstance(data.get("repository"), Mapping):
            raise QueueObserverError("merge queue GraphQL response omitted repository data")
        repository = data.get("repository")
        merge_queue = repository.get("mergeQueue") if isinstance(repository, Mapping) else None
        if merge_queue is None:
            return None
        entries_connection = merge_queue.get("entries") if isinstance(merge_queue, Mapping) else None
        entries = (
            entries_connection.get("nodes", [])
            if isinstance(entries_connection, Mapping)
            else []
        )
        owner_entry = None
        candidates = []
        for entry in entries if isinstance(entries, list) else []:
            if not isinstance(entry, Mapping):
                continue
            pull_request = entry.get("pullRequest")
            if not isinstance(pull_request, Mapping):
                continue
            try:
                number = int(pull_request.get("number"))
            except (TypeError, ValueError):
                continue
            base_commit = entry.get("baseCommit") or {}
            head_commit = entry.get("headCommit") or {}
            synthetic_sha = (
                head_commit.get("oid") if isinstance(head_commit, Mapping) else None
            )
            candidate = {
                "number": number,
                "author": pull_request.get("author"),
                "headRefOid": pull_request.get("headRefOid"),
                "baseRefOid": pull_request.get("baseRefOid"),
                "baseRefName": pull_request.get("baseRefName"),
                "queueEntryId": entry.get("id"),
                "mergeGroupSha": synthetic_sha,
                "state": entry.get("state"),
            }
            candidates.append(candidate)
            if number != self.pr_number:
                continue
            owner_entry = {
                "id": entry.get("id"),
                "position": entry.get("position"),
                "state": entry.get("state"),
                "merge_group_sha": synthetic_sha,
                "merge_group_source": "MergeQueueEntry.headCommit.oid",
                "baseSha": base_commit.get("oid")
                if isinstance(base_commit, Mapping)
                else None,
                "baseRefName": pull_request.get("baseRefName"),
            }
        if owner_entry is not None:
            owner_entry["pullRequests"] = candidates
        return owner_entry

    def read_rulesets(self) -> Sequence[Mapping[str, Any]] | Mapping[str, Any]:
        owner, name = self.repo.split("/", 1)
        payload = self._runner(["api", f"repos/{owner}/{name}/rulesets", "--method", "GET"])
        return payload if isinstance(payload, (list, Mapping)) else []

    def read_workflow_runs(self, head_sha: str) -> Sequence[Mapping[str, Any]]:
        owner, name = self.repo.split("/", 1)
        payload = self._runner(
            [
                "api",
                f"repos/{owner}/{name}/actions/runs",
                "--method",
                "GET",
                "-f",
                f"head_sha={head_sha}",
                "-f",
                "per_page=100",
            ]
        )
        return (payload or {}).get("workflow_runs", []) if isinstance(payload, Mapping) else []


def run_gh_json(command: Sequence[str]) -> Any:
    """Run one explicitly read-only gh command and decode JSON."""

    command = list(command)
    if command[:2] != ["api", "graphql"] and command[:1] != ["api"]:
        raise QueueObserverError("queue observer permits only gh api reads")
    if "--method" in command:
        method = command[command.index("--method") + 1].upper()
        if method != "GET":
            raise QueueObserverError(f"queue observer forbids gh api method {method}")
    if len(command) >= 2 and command[:2] == ["api", "graphql"]:
        query_values = [value[6:] for value in command if value.startswith("query=")]
        if not query_values or not query_values[0].lstrip().startswith(("query", "{")):
            raise QueueObserverError("queue observer permits only GraphQL read queries")
    try:
        result = subprocess.run(
            ["gh", *command], check=True, capture_output=True, text=True
        )
    except (FileNotFoundError, subprocess.CalledProcessError) as exc:
        raise QueueObserverError(f"read-only gh api failed: {exc}") from exc
    try:
        return json.loads(result.stdout or "null")
    except json.JSONDecodeError as exc:
        raise QueueObserverError("read-only gh api returned invalid JSON") from exc


def snapshot_from_provider(
    provider: ReadOnlyGitHubProvider,
    *,
    previous_binding: Mapping[str, Any] | None = None,
    require_queue: bool = True,
) -> dict[str, Any]:
    """Read each exact surface once, in a deterministic order."""

    pr = provider.read_pr()
    queue_entry = provider.read_queue_entry()
    queue = normalize_queue_entry(queue_entry)
    synthetic_sha = (queue or {}).get("synthetic_sha", "")
    workflow_runs = provider.read_workflow_runs(synthetic_sha) if synthetic_sha else []
    rulesets = provider.read_rulesets()
    candidates = (queue or {}).get("pull_requests", [])
    return reconcile_snapshot(
        pr=pr,
        queue_entry=queue,
        candidates=candidates,
        workflow_runs=workflow_runs,
        rulesets=rulesets,
        previous_binding=previous_binding,
        require_queue=require_queue,
    )


def _pr_repo_and_number(pr: str, repo: str | None) -> tuple[str, int]:
    if pr.isdigit():
        if not repo:
            raise QueueObserverError("bare PR numbers require --repo OWNER/REPO")
        return repo, int(pr)
    parsed = urlparse(pr)
    parts = [part for part in parsed.path.split("/") if part]
    if len(parts) < 4 or parts[2] != "pull" or not parts[3].isdigit():
        raise QueueObserverError("--pr must be a PR URL or a number with --repo")
    resolved_repo = f"{parts[0]}/{parts[1]}"
    if repo and repo.casefold() != resolved_repo.casefold():
        raise QueueObserverError("--repo does not match --pr URL")
    return resolved_repo, int(parts[3])


def delegate_bounded_watcher(
    pr: str,
    repo: str | None = None,
    *,
    runner: Callable[..., Any] | None = None,
) -> dict[str, Any]:
    """Delegate one blocking wait to ``gh_pr_watch``; never poll in this module."""

    command = [sys.executable, str(Path(__file__).with_name("gh_pr_watch.py")), "--pr", pr]
    if repo:
        command.extend(["--repo", repo])
    command.append("--watch-until-action")
    run = runner or subprocess.run
    try:
        completed = run(command, check=True, capture_output=True, text=True)
    except (OSError, subprocess.CalledProcessError) as exc:
        raise QueueObserverError(f"bounded PR watcher failed: {exc}") from exc
    try:
        receipt = json.loads(completed.stdout or "")
    except json.JSONDecodeError as exc:
        raise QueueObserverError("bounded PR watcher returned invalid JSON") from exc
    if not isinstance(receipt, dict):
        raise QueueObserverError("bounded PR watcher returned a non-object receipt")
    return receipt


def validate_delegated_receipt(
    receipt: Mapping[str, Any], current: Mapping[str, Any]
) -> dict[str, Any]:
    """Bind a delegated PR-watcher receipt to the current exact PR identity."""

    if not isinstance(receipt, Mapping):
        raise QueueObserverError("delegated watcher returned a non-object receipt")
    delegated_snapshot = receipt.get("snapshot")
    delegated_pr = delegated_snapshot.get("pr") if isinstance(delegated_snapshot, Mapping) else None
    current_pr = current.get("pr") if isinstance(current, Mapping) else None
    if isinstance(current_pr, Mapping):
        current_pr = dict(current_pr)
        current_pr["repo"] = current.get("repository")
    if not isinstance(delegated_pr, Mapping) or not isinstance(current_pr, Mapping):
        raise QueueObserverError("delegated watcher receipt omitted PR identity")
    fields = ("repo", "number", "head_sha", "base_sha")
    mismatches = {
        field: {
            "delegated": delegated_pr.get(field),
            "current": current_pr.get(field),
        }
        for field in fields
        if not delegated_pr.get(field)
        or not current_pr.get(field)
        or str(delegated_pr.get(field)) != str(current_pr.get(field))
    }
    if mismatches:
        raise QueueObserverError("delegated watcher receipt identity mismatch")
    watch_context = receipt.get("watch_context")
    if isinstance(watch_context, Mapping):
        resolved_repo = _text(watch_context.get("resolved_repo"))
        if resolved_repo and resolved_repo.casefold() != str(current_pr["repo"]).casefold():
            raise QueueObserverError("delegated watcher repository context mismatch")
    return {
        "validated": True,
        "exit_reason": _text(receipt.get("exit_reason")),
        "identity": {field: current_pr.get(field) for field in fields},
    }


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Observe GitHub merge-queue state without mutation"
    )
    parser.add_argument("--pr", required=True, help="PR URL or number")
    parser.add_argument("--repo", help="OWNER/REPO, required for a bare PR number")
    parser.add_argument("--once", action="store_true", help="Take one queue snapshot")
    parser.add_argument(
        "--watch-until-action",
        action="store_true",
        help="Delegate one blocking wait to gh_pr_watch, then take one queue snapshot",
    )
    parser.add_argument(
        "--allow-no-queue",
        action="store_true",
        help="Report an empty queue as an unbound snapshot",
    )
    args = parser.parse_args(argv)
    if args.once and args.watch_until_action:
        parser.error("choose only one of --once or --watch-until-action")
    if not args.once and not args.watch_until_action:
        args.once = True
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    repo, number = _pr_repo_and_number(args.pr, args.repo)
    if args.watch_until_action:
        # The receipt is useful context, but the queue snapshot remains the
        # authoritative output and is taken only after the helper terminates.
        delegated = delegate_bounded_watcher(args.pr, args.repo)
    else:
        delegated = None
    provider = ReadOnlyGitHubProvider(repo, number)
    snapshot = snapshot_from_provider(provider, require_queue=not args.allow_no_queue)
    if delegated is not None:
        snapshot["delegated_pr_watcher"] = validate_delegated_receipt(delegated, snapshot)
    print(json.dumps(snapshot, sort_keys=True, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
