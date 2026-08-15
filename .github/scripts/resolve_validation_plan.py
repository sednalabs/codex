#!/usr/bin/env python3
"""Resolve workflow lane selections for validation-lab and sedna-heavy-tests."""

from __future__ import annotations

import argparse
import fnmatch
import json
import re
from collections import Counter
from collections import OrderedDict
from pathlib import Path

VALID_SETUP_CLASSES = {
    "workflow",
    "node",
    "rust_minimal",
    "rust_integration",
    "release",
}
VALID_FRONTIER_ROLES = {"sentinel", "depth"}
VALID_STATUS_CLASSES = {"active", "legacy"}
VALID_COST_CLASSES = {"low", "medium", "high"}
ORDERED_SETUP_CLASSES = [
    "workflow",
    "node",
    "rust_minimal",
    "rust_integration",
    "release",
]
RUST_BATCH_SETUP_CLASSES = {"rust_minimal", "rust_integration"}
RUST_BATCH_AUTO_MIN_LANES = 3
RUST_BATCH_FORCE_MIN_LANES = 2
# Keep auto batches small enough that link-heavy Rust recipes do not compete
# for too much runner-local disk or memory in one job.
RUST_BATCH_MAX_LANES = 2
RUST_BATCH_TARGET_WEIGHT_SECONDS = 720
DEFAULT_RUST_BATCH_WEIGHT_SECONDS = 360
DEFAULT_FOLLOWUP_ROUTE_PRIORITY = 0
LAB_MATRIX_JOB_LIMIT = 256
VALID_LAB_FANOUT_TIERS = {"balanced", "enterprise", "soak"}
SAFE_NEXTEST_ARCHIVE_FIELD_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")
RECOMMENDATION_DOMAIN_ORDER = [
    "workflow",
    "docs",
    "release",
    "ui_protocol",
    "core",
]
RECOMMENDATION_DOMAIN_LANE_SETS = {
    "workflow": "docs",
    "docs": "docs",
    "release": "release",
    "ui_protocol": "ui-protocol",
    "core": "core-carry",
}
RECOMMENDATION_DOMAIN_LANES = {
    "workflow": ["codex.workflow-ci-sanity", "codex.downstream-docs-check"],
    "docs": ["codex.downstream-docs-check"],
}
RELEASE_RECOMMENDATION_PATTERNS = (
    ".github/workflows/sedna-branch-build.yml",
    ".github/workflows/sedna-release.yml",
    ".github/workflows/release*.yml",
    ".github/workflows/release*.yaml",
    "codex-rs/cli/src/version.rs",
    "codex-rs/cli/src/version/**",
    "scripts/install/**",
    "scripts/resolve_sedna_release_version",
    "scripts/resolve_sedna_release_version/**",
)
LAB_FANOUT_CAPS = {
    "balanced": {
        "targeted": {
            "workflow": 12,
            "node": 6,
            "rust_minimal": 12,
            "rust_integration": 6,
            "release": 1,
        },
        "frontier": {
            "workflow": 24,
            "node": 8,
            "rust_minimal": 40,
            "rust_integration": 24,
            "release": 2,
        },
        "checkpoint": {
            "workflow": 16,
            "node": 6,
            "rust_minimal": 24,
            "rust_integration": 12,
            "release": 1,
        },
    },
    "enterprise": {
        "targeted": {
            "workflow": 24,
            "node": 12,
            "rust_minimal": 32,
            "rust_integration": 16,
            "release": 2,
        },
        "frontier": {
            "workflow": 64,
            "node": 24,
            "rust_minimal": 128,
            "rust_integration": 96,
            "release": 4,
        },
        "checkpoint": {
            "workflow": 48,
            "node": 16,
            "rust_minimal": 96,
            "rust_integration": 64,
            "release": 2,
        },
    },
    "soak": {
        "targeted": {
            "workflow": 24,
            "node": 12,
            "rust_minimal": 32,
            "rust_integration": 16,
            "release": 2,
        },
        "frontier": {
            "workflow": 96,
            "node": 32,
            "rust_minimal": 160,
            "rust_integration": 128,
            "release": 6,
        },
        "checkpoint": {
            "workflow": 96,
            "node": 32,
            "rust_minimal": 160,
            "rust_integration": 128,
            "release": 4,
        },
    },
}


def catalog_path() -> Path:
    return Path(__file__).resolve().parent.parent / "validation-lanes.json"


def load_catalog(path: Path | None = None) -> dict:
    catalog_file = path or catalog_path()
    payload = json.loads(catalog_file.read_text(encoding="utf-8"))
    if not isinstance(payload, dict) or "lanes" not in payload:
        raise SystemExit(f"invalid validation catalog at {catalog_file}")
    return payload


def catalog_repo_root(catalog_file: Path) -> Path:
    return catalog_file.resolve().parent.parent


def derive_summary_family(lane: dict) -> str:
    lane_id = str(lane.get("lane_id") or "")
    if "agent-picker" in lane_id:
        return "agent-picker"
    if "subagent-notification" in lane_id:
        return "subagent-notifications"
    if "app-server-" in lane_id:
        return "app-server"
    if "state-spawn-lineage" in lane_id:
        return "state-lineage"
    if "core-subagent-surface" in lane_id:
        return "subagent-surface"
    if "core-subagent-model-pinning" in lane_id:
        return "subagent-model-pinning"
    if "core-subagent-spawn-approval" in lane_id:
        return "subagent-spawn-approval"
    if "core-persisted-subagent-descendants" in lane_id:
        return "persisted-subagent-descendants"

    normalized = lane_id.removeprefix("codex.")
    for suffix in ("-targeted", "-test", "-smoke"):
        if normalized.endswith(suffix):
            normalized = normalized[: -len(suffix)]
    return normalized or lane_id or "validation-lane"


def derive_cost_class(setup_class: str) -> str:
    return {
        "workflow": "low",
        "node": "low",
        "rust_minimal": "medium",
        "rust_integration": "high",
        "release": "high",
    }[setup_class]


def family_key_for_lane(lane: dict) -> tuple[str, str]:
    lane_id = str(lane.get("lane_id") or "<unknown>")
    try:
        return (lane["status_class"], lane["summary_family"])
    except KeyError as exc:
        missing = exc.args[0]
        raise SystemExit(
            f"lane {lane_id} must define {missing} for validation planning"
        ) from exc


def resolve_repo_relative_path(
    repo_root: Path,
    raw_path: str,
    *,
    label: str,
    must_exist: bool = False,
    must_be_file: bool = False,
    must_be_dir: bool = False,
) -> Path:
    if not raw_path:
        raise SystemExit(f"{label} must be a non-empty relative path within the repository root")
    path = Path(raw_path)
    if path.is_absolute():
        raise SystemExit(f"{label} must be a relative path within the repository root")
    if any(part == ".." for part in path.parts):
        raise SystemExit(f"{label} must not contain '..' path segments")

    repo_root = repo_root.resolve()
    candidate = (repo_root / path).resolve()
    try:
        candidate.relative_to(repo_root)
    except ValueError as exc:
        raise SystemExit(f"{label} must stay within the repository root") from exc

    if must_exist and not candidate.exists():
        raise SystemExit(f"{label} not found: {candidate}")
    if must_be_file and not candidate.is_file():
        raise SystemExit(f"{label} must point to a file: {candidate}")
    if must_be_dir and not candidate.is_dir():
        raise SystemExit(f"{label} must point to a directory: {candidate}")
    return candidate


def normalize_catalog(catalog: dict) -> dict:
    """Backfill non-execution metadata for the current catalog."""

    normalized_lanes: list[dict] = []
    family_sentinel_ids: dict[tuple[str, str], str] = {}

    for original in catalog["lanes"]:
        lane = dict(original)
        lane.setdefault("status_class", "active")
        lane.setdefault("summary_family", derive_summary_family(lane))
        lane.setdefault("cost_class", derive_cost_class(lane["setup_class"]))
        lane.setdefault("checkout_fetch_depth", 1)
        lane.setdefault("timeout_minutes", 30)
        lane.setdefault("frontier_default", False)
        lane.setdefault("needs_bazel", False)
        lane.setdefault("smoke_gate_only", False)
        if "frontier_lane_sets" not in lane:
            if lane.get("status_class") == "active" and not lane.get("explicit_only"):
                lane["frontier_lane_sets"] = (
                    [lane_set for lane_set in lane.get("lane_sets", []) if lane_set != "all"]
                    if lane.get("frontier_default")
                    else []
                )
            else:
                lane["frontier_lane_sets"] = []

        family_key = family_key_for_lane(lane)
        lane_id = lane["lane_id"]
        chosen = family_sentinel_ids.get(family_key)
        if chosen is None or lane_id < chosen:
            family_sentinel_ids[family_key] = lane_id

        normalized_lanes.append(lane)

    for lane in normalized_lanes:
        if "frontier_role" not in lane:
            family_key = family_key_for_lane(lane)
            lane["frontier_role"] = (
                "sentinel"
                if lane["lane_id"] == family_sentinel_ids[family_key]
                else "depth"
            )

    normalized_routes: list[dict] = []
    for original in catalog.get("followup_routes", []):
        route = dict(original)
        route.setdefault("priority", DEFAULT_FOLLOWUP_ROUTE_PRIORITY)
        normalized_routes.append(route)

    normalized = dict(catalog)
    normalized["lanes"] = normalized_lanes
    normalized["followup_routes"] = normalized_routes
    return normalized


def validate_catalog(catalog: dict, *, repo_root: Path | None = None) -> None:
    repo_root = repo_root or catalog_path().resolve().parent.parent
    seen_lane_ids: set[str] = set()
    for lane in catalog["lanes"]:
        lane_id = lane["lane_id"]
        if lane_id in seen_lane_ids:
            raise SystemExit(f"duplicate lane id in validation catalog: {lane_id}")
        seen_lane_ids.add(lane_id)

        status_class = lane.get("status_class")
        if status_class not in VALID_STATUS_CLASSES:
            valid = ", ".join(sorted(VALID_STATUS_CLASSES))
            raise SystemExit(f"lane {lane_id} must set status_class to one of: {valid}")

        setup_class = lane.get("setup_class")
        if setup_class not in VALID_SETUP_CLASSES:
            valid = ", ".join(sorted(VALID_SETUP_CLASSES))
            raise SystemExit(f"lane {lane_id} must set setup_class to one of: {valid}")

        frontier_role = lane.get("frontier_role")
        if frontier_role not in VALID_FRONTIER_ROLES:
            valid = ", ".join(sorted(VALID_FRONTIER_ROLES))
            raise SystemExit(f"lane {lane_id} must set frontier_role to one of: {valid}")

        cost_class = lane.get("cost_class")
        if cost_class not in VALID_COST_CLASSES:
            valid = ", ".join(sorted(VALID_COST_CLASSES))
            raise SystemExit(f"lane {lane_id} must set cost_class to one of: {valid}")

        working_directory = lane.get("working_directory")
        if not isinstance(working_directory, str) or not working_directory:
            raise SystemExit(f"lane {lane_id} must set working_directory")
        resolve_repo_relative_path(
            repo_root,
            working_directory,
            label=f"lane {lane_id} working_directory",
        )

        script_path = lane.get("script_path")
        if not isinstance(script_path, str) or not script_path:
            raise SystemExit(f"lane {lane_id} must set script_path")
        resolve_repo_relative_path(
            repo_root,
            script_path,
            label=f"lane {lane_id} script_path",
        )

        script_args = lane.get("script_args")
        if not isinstance(script_args, list) or not all(
            isinstance(arg, str) for arg in script_args
        ):
            raise SystemExit(f"lane {lane_id} must set script_args to a list of strings")

        for field in (
            "needs_just",
            "needs_node",
            "needs_nextest",
            "needs_linux_build_deps",
            "needs_dotslash",
            "needs_sccache",
            "needs_bazel",
        ):
            if not isinstance(lane.get(field), bool):
                raise SystemExit(f"lane {lane_id} must set {field} to true or false")

        if "pilot_only" in lane and not isinstance(lane.get("pilot_only"), bool):
            raise SystemExit(f"lane {lane_id} must set pilot_only to true or false")

        validate_nextest_archive_config(lane, repo_root=repo_root)
        resolve_checkout_fetch_depth(lane)
        resolve_timeout_minutes(lane)

    for route in catalog.get("followup_routes", []):
        if not isinstance(route, dict):
            raise SystemExit("validation catalog follow-up routes must be objects")
        route_id = route.get("route_id")
        if not isinstance(route_id, str) or not route_id:
            raise SystemExit("validation catalog follow-up routes must set route_id")
        followup_route_priority(route)


def validate_safe_nextest_archive_field(lane_id: str, field_name: str, value: object) -> str:
    if not isinstance(value, str) or not SAFE_NEXTEST_ARCHIVE_FIELD_RE.fullmatch(value):
        raise SystemExit(
            f"lane {lane_id} nextest_archive.{field_name} must be 1-128 safe "
            "letters, numbers, dots, underscores, or hyphens"
        )
    return value


def validate_nextest_archive_config(lane: dict, *, repo_root: Path) -> None:
    archive = lane.get("nextest_archive")
    if archive is None:
        return

    lane_id = str(lane.get("lane_id") or "<unknown>")
    if not isinstance(archive, dict):
        raise SystemExit(f"lane {lane_id} must set nextest_archive to an object")
    if lane.get("setup_class") != "rust_integration":
        raise SystemExit(
            f"lane {lane_id} nextest_archive is currently supported only for rust_integration lanes"
        )
    if not lane.get("explicit_only") or not lane.get("pilot_only"):
        raise SystemExit(
            f"lane {lane_id} nextest_archive lanes must be explicit_only and pilot_only"
        )

    required_fields = {
        "cohort",
        "artifact_name",
        "archive_file_name",
        "build_script_path",
    }
    missing = sorted(field for field in required_fields if field not in archive)
    if missing:
        raise SystemExit(
            f"lane {lane_id} nextest_archive is missing required field(s): {', '.join(missing)}"
        )

    for field_name in ("cohort", "artifact_name", "archive_file_name"):
        validate_safe_nextest_archive_field(lane_id, field_name, archive.get(field_name))

    build_script_path = archive.get("build_script_path")
    if not isinstance(build_script_path, str) or not build_script_path:
        raise SystemExit(f"lane {lane_id} nextest_archive.build_script_path must be set")
    resolve_repo_relative_path(
        repo_root,
        build_script_path,
        label=f"lane {lane_id} nextest_archive.build_script_path",
    )


def resolve_checkout_fetch_depth(lane: dict, *, default: int | None = None) -> int:
    lane_id = str(lane.get("lane_id") or "<unknown>")
    checkout_fetch_depth = lane.get("checkout_fetch_depth", default)
    if isinstance(checkout_fetch_depth, bool) or not isinstance(
        checkout_fetch_depth, int
    ):
        raise SystemExit(
            f"lane {lane_id} must set checkout_fetch_depth to a non-negative integer"
        )
    if checkout_fetch_depth < 0:
        raise SystemExit(
            f"lane {lane_id} must set checkout_fetch_depth to a non-negative integer"
        )
    return checkout_fetch_depth


def resolve_timeout_minutes(lane: dict, *, default: int | None = None) -> int:
    lane_id = str(lane.get("lane_id") or "<unknown>")
    timeout_minutes = lane.get("timeout_minutes", default)
    if isinstance(timeout_minutes, bool) or not isinstance(timeout_minutes, int):
        raise SystemExit(f"lane {lane_id} must set timeout_minutes to a positive integer")
    if timeout_minutes <= 0:
        raise SystemExit(f"lane {lane_id} must set timeout_minutes to a positive integer")
    return timeout_minutes


def lane_payload(spec: dict, *, lane_phase: str) -> dict:
    nextest_archive = nextest_archive_payload(spec)
    return {
        "lane_id": spec["lane_id"],
        "lane_phase": lane_phase,
        "groups": spec.get("groups") or [],
        "status_class": spec["status_class"],
        "frontier_default": bool(spec.get("frontier_default", False)),
        "setup_class": spec["setup_class"],
        "frontier_role": spec["frontier_role"],
        "summary_family": spec["summary_family"],
        "cost_class": spec["cost_class"],
        "checkout_fetch_depth": resolve_checkout_fetch_depth(spec, default=1),
        "timeout_minutes": resolve_timeout_minutes(spec, default=30),
        "working_directory": spec["working_directory"],
        "script_path": spec["script_path"],
        "script_args": spec.get("script_args") or [],
        "needs_just": bool(spec["needs_just"]),
        "needs_node": bool(spec["needs_node"]),
        "needs_nextest": bool(spec["needs_nextest"]),
        "needs_linux_build_deps": bool(spec["needs_linux_build_deps"]),
        "needs_dotslash": bool(spec["needs_dotslash"]),
        "needs_sccache": bool(spec["needs_sccache"]),
        "needs_bazel": bool(spec.get("needs_bazel", False)),
        "batch_group": str(spec.get("batch_group") or default_batch_group(spec)),
        "batch_weight_seconds": resolve_batch_weight_seconds(spec),
        **nextest_archive,
    }


def nextest_archive_payload(spec: dict) -> dict:
    archive = spec.get("nextest_archive")
    if not archive:
        return {
            "uses_nextest_archive": False,
            "nextest_archive_cohort": "",
            "nextest_archive_artifact_name": "",
            "nextest_archive_file_name": "",
            "nextest_archive_build_script_path": "",
        }
    return {
        "uses_nextest_archive": True,
        "nextest_archive_cohort": archive["cohort"],
        "nextest_archive_artifact_name": archive["artifact_name"],
        "nextest_archive_file_name": archive["archive_file_name"],
        "nextest_archive_build_script_path": archive["build_script_path"],
    }


def default_batch_group(spec: dict) -> str:
    groups = spec.get("groups") or []
    if not groups:
        return "default"
    return "+".join(str(group) for group in groups)


def resolve_batch_weight_seconds(spec: dict) -> int:
    raw = spec.get("batch_weight_seconds", DEFAULT_RUST_BATCH_WEIGHT_SECONDS)
    lane_id = str(spec.get("lane_id") or "<unknown>")
    if isinstance(raw, bool) or not isinstance(raw, int) or raw <= 0:
        raise SystemExit(f"lane {lane_id} must set batch_weight_seconds to a positive integer")
    return raw


def select_exact(
    catalog_by_id: dict[str, dict], lane_ids: list[str], *, lane_phase: str
) -> list[dict]:
    selected: list[dict] = []
    seen: set[str] = set()
    for lane_id in lane_ids:
        spec = catalog_by_id.get(lane_id)
        if spec is None:
            raise SystemExit(f"unknown lane id: {lane_id}")
        if lane_id in seen:
            continue
        seen.add(lane_id)
        selected.append(lane_payload(spec, lane_phase=lane_phase))
    return selected


def path_matches(path: str, pattern: str) -> bool:
    return fnmatch.fnmatch(path, pattern)


def followup_route_priority(route: dict) -> int:
    """Return a validated route priority, defaulting safely for existing routes."""
    priority = route.get("priority", DEFAULT_FOLLOWUP_ROUTE_PRIORITY)
    if isinstance(priority, bool) or not isinstance(priority, int) or priority < 0:
        route_id = str(route.get("route_id") or "<unknown>")
        raise SystemExit(
            f"follow-up route {route_id} must set priority to a non-negative integer"
        )
    return priority


def select_followup_lanes(files: list[str], routes: list[dict]) -> list[str]:
    routes_with_priority = [
        (route, followup_route_priority(route)) for route in routes
    ]
    if not files:
        return []

    matching_routes: list[tuple[dict, int]] = []
    for route, priority in routes_with_priority:
        allowed_paths = route.get("allowed_paths", [])
        required_any_paths = route.get("required_any_paths", [])
        if not allowed_paths:
            continue
        if not all(any(path_matches(path, pattern) for pattern in allowed_paths) for path in files):
            continue
        if required_any_paths and not any(
            any(path_matches(path, pattern) for pattern in required_any_paths) for path in files
        ):
            continue
        matching_routes.append((route, priority))

    if not matching_routes:
        return []

    highest_priority = max(priority for _, priority in matching_routes)
    highest_priority_routes = [
        route
        for route, priority in matching_routes
        if priority == highest_priority
    ]
    if len(highest_priority_routes) != 1:
        return []
    return list(highest_priority_routes[0].get("lane_ids", []))


def parse_changed_files(raw: str) -> list[str]:
    if not raw.strip():
        return []
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise SystemExit("changed-files-json must be a JSON array of strings") from exc
    if not isinstance(payload, list) or not all(isinstance(item, str) for item in payload):
        raise SystemExit("changed-files-json must be a JSON array of strings")
    return [item.strip() for item in payload if item.strip()]


def parse_changed_files_input(raw: str, path: str = "") -> list[str]:
    if path:
        return parse_changed_files(Path(path).read_text(encoding="utf-8"))
    return parse_changed_files(raw)


def parse_recommendation_changed_files(raw: str) -> tuple[list[str], str]:
    if not raw.strip():
        return [], "changed-file metadata was empty"
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError:
        return [], "changed-file metadata was not valid JSON"
    if not isinstance(payload, list) or not all(isinstance(item, str) for item in payload):
        return [], "changed-file metadata was not a JSON array of strings"
    changed_files = [item.strip() for item in payload if item.strip()]
    if not changed_files:
        return [], "changed-file metadata was empty"
    return changed_files, ""


def recommendation_domain(path: str) -> str:
    if any(path_matches(path, pattern) for pattern in RELEASE_RECOMMENDATION_PATTERNS):
        return "release"
    if path.startswith("docs/") or path == "README.md":
        return "docs"
    if path.startswith(".github/") or path.startswith(".codex/skills/"):
        return "workflow"
    if path.startswith("codex-rs/app-server") or path.startswith("codex-rs/tui/"):
        return "ui_protocol"
    if path.startswith("codex-rs/protocol/"):
        return "ui_protocol"
    if (
        path.startswith("codex-rs/core/")
        or path.startswith("codex-rs/exec/")
        or path.startswith("codex-rs/cli/")
    ):
        return "core"
    return "unknown"


def ordered_recommendation_domains(domains: list[str]) -> list[str]:
    return sorted(
        set(domains),
        key=lambda item: (
            RECOMMENDATION_DOMAIN_ORDER.index(item)
            if item in RECOMMENDATION_DOMAIN_ORDER
            else 99
        ),
    )


def infer_lane_set_for_lanes(
    catalog_by_id: dict[str, dict], lane_ids: list[str], changed_files: list[str]
) -> str:
    path_domains = ordered_recommendation_domains(
        [recommendation_domain(path) for path in changed_files]
    )
    known_path_domains = [domain for domain in path_domains if domain != "unknown"]
    if len(known_path_domains) == 1 and "unknown" not in path_domains:
        return RECOMMENDATION_DOMAIN_LANE_SETS[known_path_domains[0]]

    groups = {
        group
        for lane_id in lane_ids
        for group in (catalog_by_id.get(lane_id, {}).get("groups") or [])
    }
    if groups <= {"workflow", "docs"}:
        return "docs"
    if "release" in groups:
        return "release"
    if "ui_protocol" in groups:
        return "ui-protocol"
    if "attestation" in groups:
        return "attestation"
    if "core" in groups:
        return "core-carry"
    return "all"


def require_known_route_lanes(catalog_by_id: dict[str, dict], lane_ids: list[str]) -> None:
    missing_lanes = [lane_id for lane_id in lane_ids if lane_id not in catalog_by_id]
    if missing_lanes:
        raise SystemExit(
            "matched follow-up route contains unknown lane IDs: "
            + ", ".join(missing_lanes)
        )


def recommendation_payload(
    *,
    profile: str,
    lane_set: str,
    lane_ids: list[str],
    reason: str,
    confidence: str,
    source: str,
    domains: list[str],
    changed_files: list[str],
    include_explicit_lanes: bool = False,
) -> dict:
    return {
        "profile": profile,
        "lane_set": lane_set,
        "lane_ids": lane_ids,
        "lanes": ",".join(lane_ids),
        "lanes_csv": ",".join(lane_ids),
        "reason": reason,
        "confidence": confidence,
        "source": source,
        "domains": domains,
        "changed_file_count": len(changed_files),
        "include_explicit_lanes": include_explicit_lanes,
        "dispatch_inputs": {
            "profile": profile,
            "lane_set": lane_set,
            "lanes": ",".join(lane_ids),
            "include_explicit_lanes": "true" if include_explicit_lanes else "false",
        },
        "advisory": True,
    }


def recommend_lab_plan(args: argparse.Namespace) -> None:
    catalog_file = Path(args.catalog_path) if args.catalog_path else catalog_path()
    catalog = normalize_catalog(load_catalog(catalog_file))
    validate_catalog(catalog, repo_root=catalog_repo_root(catalog_file))
    catalog_by_id = {spec["lane_id"]: spec for spec in catalog["lanes"]}
    changed_files, metadata_issue = parse_recommendation_changed_files(
        args.changed_files_json
    )

    def emit_fallback(reason: str, domains: list[str] | None = None) -> None:
        emit(
            recommendation_payload(
                profile="frontier",
                lane_set="all",
                lane_ids=[],
                reason=reason,
                confidence="low",
                source="conservative_fallback",
                domains=domains or ["unknown"],
                changed_files=changed_files,
            )
        )

    if metadata_issue:
        emit_fallback(metadata_issue)
        return

    route_lanes = select_followup_lanes(changed_files, catalog.get("followup_routes", []))
    if route_lanes:
        require_known_route_lanes(catalog_by_id, route_lanes)
        route_domains = sorted(
            {
                group
                for lane_id in route_lanes
                for group in (catalog_by_id.get(lane_id, {}).get("groups") or [])
            }
        )
        emit(
            recommendation_payload(
                profile="targeted",
                lane_set=infer_lane_set_for_lanes(
                    catalog_by_id, route_lanes, changed_files
                ),
                lane_ids=route_lanes,
                reason="changed files matched one exact validation follow-up route",
                confidence="high",
                source="followup_route",
                domains=route_domains,
                changed_files=changed_files,
                include_explicit_lanes=any(
                    bool(catalog_by_id.get(lane_id, {}).get("explicit_only"))
                    for lane_id in route_lanes
                ),
            )
        )
        return

    domains = [recommendation_domain(path) for path in changed_files]
    unique_domains = ordered_recommendation_domains(domains)
    known_domains = [domain for domain in unique_domains if domain != "unknown"]
    if len(known_domains) == 1 and "unknown" not in unique_domains:
        domain = known_domains[0]
        lane_ids = RECOMMENDATION_DOMAIN_LANES.get(domain, [])
        emit(
            recommendation_payload(
                profile="targeted",
                lane_set=RECOMMENDATION_DOMAIN_LANE_SETS[domain],
                lane_ids=lane_ids,
                reason=f"changed files stayed within the {domain} validation domain",
                confidence="medium",
                source="domain_rules",
                domains=[domain],
                changed_files=changed_files,
            )
        )
        return

    emit_fallback(
        "changed files crossed domains or did not match a known validation route",
        unique_domains or ["unknown"],
    )


def select_for_lane_set(
    catalog: dict,
    target_lane_set: str,
    *,
    lane_phase: str,
    field_name: str = "lane_sets",
    include_explicit_only: bool = False,
) -> list[dict]:
    selected: list[dict] = []
    for spec in catalog["lanes"]:
        if target_lane_set not in spec.get(field_name, []):
            continue
        if spec.get("explicit_only") and not include_explicit_only:
            continue
        selected.append(lane_payload(spec, lane_phase=lane_phase))
    return selected


def is_smoke_gate_lane(spec: dict) -> bool:
    return bool(spec.get("smoke_gate_only"))


def select_frontier_all(catalog: dict, *, include_explicit_only: bool = False) -> list[dict]:
    allowed_status_classes = {"active", "legacy"} if include_explicit_only else {"active"}
    return [
        lane_payload(spec, lane_phase="downstream_lanes")
        for spec in catalog["lanes"]
        if spec.get("status_class") in allowed_status_classes
        and (include_explicit_only or not spec.get("explicit_only"))
        and not spec.get("pilot_only")
        and not is_smoke_gate_lane(spec)
    ]


def select_smoke_matrix(catalog: dict, smoke_gate_kind: str) -> list[dict]:
    return [
        lane_payload(spec, lane_phase="smoke_gate")
        for spec in catalog["lanes"]
        if smoke_gate_kind in spec.get("smoke_gate_kinds", [])
    ]


def exclude_smoke_gate_lanes(selected: list[dict], smoke_matrix: list[dict]) -> list[dict]:
    smoke_lane_ids = {lane["lane_id"] for lane in smoke_matrix}
    if not smoke_lane_ids:
        return selected
    return [lane for lane in selected if lane["lane_id"] not in smoke_lane_ids]


def emit(payload: dict) -> None:
    print(json.dumps(payload, separators=(",", ":")))


def parse_bool(value: str) -> bool:
    return value.lower() == "true"


def determine_smoke_gate(groups: set[str]) -> tuple[bool, str]:
    has_runtime = bool(groups & {"core", "ui_protocol", "attestation"})
    has_docs = bool(groups & {"workflow", "docs"})
    smoke_gate_kind = "runtime" if has_runtime else "workflow_docs" if has_docs else ""
    return bool(has_runtime or has_docs), smoke_gate_kind


def group_lanes_by_setup_class(lanes: list[dict]) -> OrderedDict[str, list[dict]]:
    grouped: OrderedDict[str, list[dict]] = OrderedDict(
        (name, []) for name in ORDERED_SETUP_CLASSES
    )
    for lane in lanes:
        grouped[lane["setup_class"]].append(lane)
    return grouped


def emit_grouped_setup_class_payload(payload: dict, lanes: list[dict], *, key_prefix: str) -> None:
    grouped = group_lanes_by_setup_class(lanes)
    for setup_class, grouped_lanes in grouped.items():
        payload[f"{key_prefix}_{setup_class}_matrix"] = {"include": grouped_lanes}
        payload[f"{key_prefix}_{setup_class}_lane_count"] = len(grouped_lanes)


def normalize_rust_batching_mode(raw: str) -> str:
    mode = (raw or "auto").strip().lower()
    if mode not in {"auto", "off", "force"}:
        raise SystemExit("rust batching mode must be one of: auto, off, force")
    return mode


def normalize_lab_fanout_tier(raw: str) -> str:
    tier = (raw or "enterprise").strip().lower()
    if tier not in VALID_LAB_FANOUT_TIERS:
        valid = ", ".join(sorted(VALID_LAB_FANOUT_TIERS))
        raise SystemExit(f"validation-lab fanout tier must be one of: {valid}")
    return tier


def effective_rust_batching_mode(
    requested: str, repo_override: str, *, override_label: str
) -> tuple[str, str]:
    requested_mode = normalize_rust_batching_mode(requested)
    override = (repo_override or "").strip().lower()
    if override and override not in {"auto", "off", "force"}:
        return requested_mode, f"ignoring unknown repo override {override!r}"
    if requested_mode == "force":
        return "force", "forced by workflow input"
    if override == "off":
        return "off", f"disabled by {override_label}"
    if override == "force":
        return "force", f"forced by {override_label}"
    if requested_mode == "off":
        return "off", "disabled by workflow input"
    return "auto", "auto"


def split_rust_batch_execution_lanes(
    selected: list[dict], *, mode: str
) -> tuple[list[dict], dict[str, list[dict]], dict[str, str]]:
    batched_by_setup_class: dict[str, list[dict]] = {name: [] for name in RUST_BATCH_SETUP_CLASSES}
    selected_by_setup_class = group_lanes_by_setup_class(selected)
    min_lanes = RUST_BATCH_FORCE_MIN_LANES if mode == "force" else RUST_BATCH_AUTO_MIN_LANES

    if mode != "off":
        for setup_class in sorted(RUST_BATCH_SETUP_CLASSES):
            lanes = selected_by_setup_class.get(setup_class, [])
            grouped: OrderedDict[str, list[dict]] = OrderedDict()
            for lane in lanes:
                grouped.setdefault(str(lane.get("batch_group") or "default"), []).append(lane)
            batched_by_setup_class[setup_class] = [
                lane
                for grouped_lanes in grouped.values()
                if len(grouped_lanes) >= min_lanes
                for lane in grouped_lanes
            ]

    batched_lane_ids = {
        lane["lane_id"]
        for lanes in batched_by_setup_class.values()
        for lane in lanes
    }
    single_lanes = [lane for lane in selected if lane["lane_id"] not in batched_lane_ids]
    reasons = {}
    for setup_class in sorted(RUST_BATCH_SETUP_CLASSES):
        selected_count = len(selected_by_setup_class.get(setup_class, []))
        batched_count = len(batched_by_setup_class[setup_class])
        if mode == "off":
            reasons[setup_class] = "batching disabled"
        elif batched_count:
            reasons[setup_class] = f"batched {batched_count} lanes"
        else:
            reasons[setup_class] = f"only {selected_count} lanes selected"
    return single_lanes, batched_by_setup_class, reasons


def split_nextest_archive_execution_lanes(selected: list[dict]) -> tuple[list[dict], list[dict]]:
    archive_lanes = [lane for lane in selected if lane.get("uses_nextest_archive")]
    ordinary_lanes = [lane for lane in selected if not lane.get("uses_nextest_archive")]
    return ordinary_lanes, archive_lanes


def nextest_archive_matrix(archive_lanes: list[dict]) -> list[dict]:
    by_artifact: OrderedDict[str, dict] = OrderedDict()
    for lane in archive_lanes:
        artifact_name = str(lane["nextest_archive_artifact_name"])
        existing = by_artifact.get(artifact_name)
        row = {
            "archive_cohort": lane["nextest_archive_cohort"],
            "artifact_name": artifact_name,
            "archive_file_name": lane["nextest_archive_file_name"],
            "build_script_path": lane["nextest_archive_build_script_path"],
            "working_directory": lane["working_directory"],
            "checkout_fetch_depth": lane["checkout_fetch_depth"],
            "needs_node": lane["needs_node"],
            "needs_linux_build_deps": lane["needs_linux_build_deps"],
            "needs_dotslash": lane["needs_dotslash"],
            "needs_sccache": lane["needs_sccache"],
            "lane_ids": [lane["lane_id"]],
            "lane_ids_json": json.dumps([lane["lane_id"]], separators=(",", ":")),
        }
        if existing is None:
            by_artifact[artifact_name] = row
            continue

        comparable_fields = [
            "archive_cohort",
            "archive_file_name",
            "build_script_path",
            "working_directory",
        ]
        mismatches = [
            field
            for field in comparable_fields
            if existing.get(field) != row.get(field)
        ]
        if mismatches:
            joined = ", ".join(mismatches)
            raise SystemExit(
                f"nextest archive artifact {artifact_name} has conflicting field(s): {joined}"
            )
        existing["checkout_fetch_depth"] = max(
            int(existing["checkout_fetch_depth"]),
            int(row["checkout_fetch_depth"]),
        )
        for field in (
            "needs_node",
            "needs_linux_build_deps",
            "needs_dotslash",
            "needs_sccache",
        ):
            existing[field] = bool(existing[field] or row[field])
        existing["lane_ids"].append(lane["lane_id"])
        existing["lane_ids_json"] = json.dumps(existing["lane_ids"], separators=(",", ":"))

    return list(by_artifact.values())


def batch_lane_matrix(lanes: list[dict], *, setup_class: str) -> list[dict]:
    groups: OrderedDict[str, list[dict]] = OrderedDict()
    for lane in lanes:
        groups.setdefault(str(lane.get("batch_group") or "default"), []).append(lane)

    batches: list[dict] = []
    batch_index = 0
    for batch_group, grouped_lanes in groups.items():
        sorted_lanes = sorted(
            grouped_lanes,
            key=lambda lane: (-int(lane["batch_weight_seconds"]), str(lane["lane_id"])),
        )
        packed: list[dict] = []
        for lane in sorted_lanes:
            candidate_indexes = [
                idx
                for idx, batch in enumerate(packed)
                if len(batch["lanes"]) < RUST_BATCH_MAX_LANES
                and batch["estimated_weight_seconds"] + int(lane["batch_weight_seconds"])
                <= RUST_BATCH_TARGET_WEIGHT_SECONDS
            ]
            if candidate_indexes:
                target = min(
                    candidate_indexes,
                    key=lambda idx: (
                        packed[idx]["estimated_weight_seconds"],
                        len(packed[idx]["lanes"]),
                        packed[idx]["batch_index"],
                    ),
                )
                batch = packed[target]
            else:
                batch = {
                    "batch_index": batch_index,
                    "batch_group": batch_group,
                    "lanes": [],
                    "estimated_weight_seconds": 0,
                }
                packed.append(batch)
                batch_index += 1
            batch["lanes"].append(lane)
            batch["estimated_weight_seconds"] += int(lane["batch_weight_seconds"])

        for batch in packed:
            batch_lanes = sorted(batch["lanes"], key=lambda lane: str(lane["lane_id"]))
            lane_ids = [lane["lane_id"] for lane in batch_lanes]
            batch_id = f"{setup_class}-{batch['batch_index'] + 1:02d}"
            batches.append(
                {
                    "batch_id": batch_id,
                    "setup_class": setup_class,
                    "batch_index": batch["batch_index"],
                    "batch_group": batch["batch_group"],
                    "batch_lane_count": len(batch_lanes),
                    "estimated_weight_seconds": batch["estimated_weight_seconds"],
                    "lane_ids": lane_ids,
                    "lane_ids_json": json.dumps(lane_ids, separators=(",", ":")),
                    "checkout_fetch_depth": max(
                        resolve_checkout_fetch_depth(lane, default=1) for lane in batch_lanes
                    ),
                    "needs_just": any(lane["needs_just"] for lane in batch_lanes),
                    "needs_node": any(lane["needs_node"] for lane in batch_lanes),
                    "needs_nextest": any(lane["needs_nextest"] for lane in batch_lanes),
                    "needs_linux_build_deps": any(
                        lane["needs_linux_build_deps"] for lane in batch_lanes
                    ),
                    "needs_dotslash": any(lane["needs_dotslash"] for lane in batch_lanes),
                    "needs_sccache": any(lane["needs_sccache"] for lane in batch_lanes),
                }
            )
    return sorted(batches, key=lambda batch: batch["batch_index"])


def cap_parallel_limits(counts: Counter[str], caps: dict[str, int]) -> dict[str, int]:
    return {
        setup_class: max(1, min(counts.get(setup_class, 0), cap))
        for setup_class, cap in caps.items()
    }


def lab_fanout_band(profile: str) -> str:
    if profile == "frontier":
        return "frontier"
    if profile in {"broad", "full"}:
        return "checkpoint"
    return "targeted"


def setup_parallel_limits(
    profile: str,
    selected: list[dict] | None = None,
    *,
    fanout_tier: str = "legacy",
) -> dict[str, int]:
    counts = Counter(lane["setup_class"] for lane in (selected or []))
    if fanout_tier != "legacy" and profile != "smoke":
        tier = normalize_lab_fanout_tier(fanout_tier)
        return cap_parallel_limits(counts, LAB_FANOUT_CAPS[tier][lab_fanout_band(profile)])

    if profile == "frontier":
        return {
            "workflow": max(1, min(counts.get("workflow", 0), 12)),
            "node": max(1, min(counts.get("node", 0), 6)),
            "rust_minimal": max(1, min(counts.get("rust_minimal", 0), 20)),
            "rust_integration": max(1, min(counts.get("rust_integration", 0), 8)),
            "release": max(1, min(counts.get("release", 0), 1)),
        }
    if profile in {"broad", "full"}:
        return {
            "workflow": max(1, min(counts.get("workflow", 0), 10)),
            "node": max(1, min(counts.get("node", 0), 4)),
            "rust_minimal": max(1, min(counts.get("rust_minimal", 0), 12)),
            "rust_integration": max(1, min(counts.get("rust_integration", 0), 6)),
            "release": max(1, min(counts.get("release", 0), 1)),
        }
    if profile == "smoke":
        return {
            "workflow": 6,
            "node": 3,
            "rust_minimal": 4,
            "rust_integration": 5,
            "release": 1,
        }
    return {
        "workflow": 8,
        "node": 4,
        "rust_minimal": 6,
        "rust_integration": 2,
        "release": 1,
    }


def determine_lab_matrix_policy(
    profile: str, selected: list[dict], *, fanout_tier: str
) -> tuple[str, str, dict[str, int]]:
    fail_fast = "false" if profile == "frontier" else "true"
    parallel_limits = setup_parallel_limits(profile, selected, fanout_tier=fanout_tier)
    active_limits = [
        parallel_limits[lane["setup_class"]]
        for lane in selected
        if lane["setup_class"] in parallel_limits
    ]
    max_parallel = str(max(active_limits) if active_limits else 1)
    return fail_fast, max_parallel, parallel_limits


def enforce_lab_matrix_job_limit(
    *,
    smoke_matrix: list[dict],
    execution_selected: list[dict],
    rust_minimal_batch_matrix: list[dict],
    rust_integration_batch_matrix: list[dict],
    nextest_archive_matrix: list[dict],
    nextest_archive_lanes: list[dict],
    run_artifact: bool,
    fanout_tier: str,
    profile: str,
) -> int:
    planned_job_count = (
        len(smoke_matrix)
        + len(execution_selected)
        + len(rust_minimal_batch_matrix)
        + len(rust_integration_batch_matrix)
        + len(nextest_archive_matrix)
        + len(nextest_archive_lanes)
        + (1 if run_artifact else 0)
    )
    if planned_job_count > LAB_MATRIX_JOB_LIMIT:
        raise SystemExit(
            "validation-lab plan would create "
            f"{planned_job_count} matrix/artifact jobs, above the {LAB_MATRIX_JOB_LIMIT} "
            f"job cap for profile={profile} fanout_tier={fanout_tier}; "
            "choose a narrower lane_set, pass explicit lanes, or lower the fanout tier"
        )
    return planned_job_count


def profile_metadata(profile: str) -> tuple[str, str]:
    if profile == "smoke":
        return (
            "smoke",
            "Fast proof that the representative smoke seams still start cleanly before wider validation.",
        )
    if profile == "targeted":
        return (
            "targeted",
            "One active seam only; prove the current question before widening.",
        )
    if profile == "frontier":
        return (
            "frontier",
            "Wide blocker harvest with fail-fast disabled; use the selected family to surface multiple independent failure groups in one remote pass.",
        )
    if profile in {"broad", "full"}:
        return (
            "checkpoint",
            "Explicit checkpoint mode; use for milestone confidence rather than routine iteration.",
        )
    if profile == "artifact":
        return (
            "buildability",
            "Packaging or preview-delivery proof; use when the question is buildability rather than seam correctness alone.",
        )
    raise SystemExit(f"unsupported profile: {profile}")


def summarize_lab_selection(
    *,
    selected: list[dict],
    smoke_matrix: list[dict],
    run_smoke_gate: bool,
    smoke_gate_kind: str,
    run_artifact: bool,
    selected_setup_classes: list[str],
    include_explicit_lanes: bool,
) -> str:
    parts = [f"selected={len(selected)}"]
    if selected_setup_classes:
        parts.append(f"setup={','.join(selected_setup_classes)}")
    if run_smoke_gate:
        parts.append(f"smoke={smoke_gate_kind or 'true'}")
        parts.append(f"smoke_lanes={len(smoke_matrix)}")
    if selected:
        preview_ids = [lane["lane_id"] for lane in selected[:3]]
        suffix = "" if len(selected) <= 3 else ",..."
        parts.append(f"lanes={','.join(preview_ids)}{suffix}")
    if run_artifact:
        parts.append("artifact=true")
    if include_explicit_lanes:
        parts.append("explicit=true")
    return ", ".join(parts)


def lab_plan(args: argparse.Namespace) -> None:
    catalog_file = Path(args.catalog_path) if args.catalog_path else catalog_path()
    catalog = normalize_catalog(load_catalog(catalog_file))
    validate_catalog(catalog, repo_root=catalog_repo_root(catalog_file))
    catalog_by_id = {spec["lane_id"]: spec for spec in catalog["lanes"]}
    requested_lanes = [lane.strip() for lane in args.lanes.split(",") if lane.strip()]
    run_artifact = args.profile == "artifact" or parse_bool(args.artifact_build)
    include_explicit_lanes = parse_bool(args.include_explicit_lanes)
    fanout_tier = normalize_lab_fanout_tier(args.fanout_tier)

    smoke_matrix: list[dict] = []

    if requested_lanes:
        selected = select_exact(
            catalog_by_id, requested_lanes, lane_phase="downstream_lanes"
        )
        run_smoke_gate = False
        smoke_gate_kind = ""
    elif args.profile == "smoke":
        selected = []
        smoke_gate_kind = "workflow_docs" if args.lane_set == "docs" else "runtime"
        smoke_matrix = select_smoke_matrix(catalog, smoke_gate_kind)
        run_smoke_gate = bool(smoke_matrix)
    elif args.profile == "artifact":
        selected = (
            []
            if args.lane_set == "all"
            else select_for_lane_set(
                catalog, args.lane_set, lane_phase="downstream_lanes"
            )
        )
        run_smoke_gate = False
        smoke_gate_kind = ""
    elif args.profile == "targeted":
        if args.lane_set == "all":
            raise SystemExit("profile=targeted requires a named lane_set or explicit lanes")
        selected = select_for_lane_set(
            catalog, args.lane_set, lane_phase="downstream_lanes"
        )
        run_smoke_gate = False
        smoke_gate_kind = ""
    elif args.profile == "frontier":
        allowed_status_classes = (
            {"active", "legacy"} if include_explicit_lanes else {"active"}
        )
        if args.lane_set == "all":
            selected = select_frontier_all(
                catalog, include_explicit_only=include_explicit_lanes
            )
        else:
            selected = select_for_lane_set(
                catalog,
                args.lane_set,
                lane_phase="downstream_lanes",
                field_name="frontier_lane_sets",
                include_explicit_only=include_explicit_lanes,
            )
            if not selected:
                selected = [
                    lane
                    for lane in select_for_lane_set(
                        catalog,
                        args.lane_set,
                        lane_phase="downstream_lanes",
                        include_explicit_only=include_explicit_lanes,
                    )
                    if lane.get("status_class") in allowed_status_classes
                ]
        run_smoke_gate = False
        smoke_gate_kind = ""
    elif args.profile in {"broad", "full"}:
        selected = select_for_lane_set(
            catalog,
            "all" if args.lane_set == "all" else args.lane_set,
            lane_phase="downstream_lanes",
        )
        groups = {group for spec in selected for group in (spec.get("groups") or [])}
        has_smoke_gate, smoke_gate_kind = determine_smoke_gate(groups)
        smoke_matrix = select_smoke_matrix(catalog, smoke_gate_kind) if has_smoke_gate else []
        run_smoke_gate = bool(selected) and bool(smoke_matrix)
        if run_smoke_gate:
            selected = exclude_smoke_gate_lanes(selected, smoke_matrix)
    else:
        raise SystemExit(f"unsupported profile: {args.profile}")

    rust_batching_mode, rust_batching_reason = effective_rust_batching_mode(
        args.rust_batching,
        args.rust_batching_override,
        override_label="VALIDATION_LAB_RUST_BATCHING",
    )
    ordinary_selected, nextest_archive_lanes = split_nextest_archive_execution_lanes(selected)
    nextest_archives = nextest_archive_matrix(nextest_archive_lanes)
    execution_selected, batched_by_setup_class, rust_batching_reasons = (
        split_rust_batch_execution_lanes(ordinary_selected, mode=rust_batching_mode)
    )
    rust_minimal_batch_matrix = batch_lane_matrix(
        batched_by_setup_class["rust_minimal"], setup_class="rust_minimal"
    )
    rust_integration_batch_matrix = batch_lane_matrix(
        batched_by_setup_class["rust_integration"], setup_class="rust_integration"
    )
    if (
        rust_batching_mode != "off"
        and not rust_minimal_batch_matrix
        and not rust_integration_batch_matrix
    ):
        no_batch_reasons = [rust_batching_reason]
        if nextest_archive_lanes and not ordinary_selected:
            no_batch_reasons.append("archive-backed lanes bypass runner-local batching")
        else:
            no_batch_reasons.extend(
                [
                    rust_batching_reasons["rust_minimal"],
                    rust_batching_reasons["rust_integration"],
                ]
            )
        rust_batching_reason = "; ".join(
            no_batch_reasons
        )
    planned_job_count = enforce_lab_matrix_job_limit(
        smoke_matrix=smoke_matrix,
        execution_selected=execution_selected,
        rust_minimal_batch_matrix=rust_minimal_batch_matrix,
        rust_integration_batch_matrix=rust_integration_batch_matrix,
        nextest_archive_matrix=nextest_archives,
        nextest_archive_lanes=nextest_archive_lanes,
        run_artifact=run_artifact,
        fanout_tier=fanout_tier,
        profile=args.profile,
    )
    matrix_fail_fast, matrix_max_parallel, parallel_limits = determine_lab_matrix_policy(
        args.profile, selected, fanout_tier=fanout_tier
    )
    grouped = group_lanes_by_setup_class(selected)
    selected_setup_classes = [
        setup_class for setup_class, lanes in grouped.items() if lanes
    ]
    profile_intent, profile_notes = profile_metadata(args.profile)
    lane_summary = summarize_lab_selection(
        selected=selected,
        smoke_matrix=smoke_matrix,
        run_smoke_gate=run_smoke_gate,
        smoke_gate_kind=smoke_gate_kind,
        run_artifact=run_artifact,
        selected_setup_classes=selected_setup_classes,
        include_explicit_lanes=include_explicit_lanes,
    )
    planned_matrix = {"include": [*smoke_matrix, *selected]}

    payload = {
        "profile_intent": profile_intent,
        "profile_notes": profile_notes,
        "lane_summary": lane_summary,
        "selected_matrix": {"include": selected},
        "planned_matrix": planned_matrix,
        "selected_lane_ids": [lane["lane_id"] for lane in selected],
        "smoke_matrix": {"include": smoke_matrix},
        "run_selected_lanes": "true" if bool(selected) else "false",
        "run_smoke_gate": "true" if run_smoke_gate else "false",
        "smoke_gate_kind": smoke_gate_kind,
        "run_artifact": "true" if run_artifact else "false",
        "matrix_fail_fast": matrix_fail_fast,
        "matrix_max_parallel": matrix_max_parallel,
        "fanout_tier": fanout_tier,
        "planned_job_count": planned_job_count,
        "rust_batching_mode": rust_batching_mode,
        "rust_batching_reason": rust_batching_reason,
        "selected_rust_minimal_batch_matrix": {"include": rust_minimal_batch_matrix},
        "selected_rust_minimal_batch_count": len(rust_minimal_batch_matrix),
        "selected_rust_integration_batch_matrix": {"include": rust_integration_batch_matrix},
        "selected_rust_integration_batch_count": len(rust_integration_batch_matrix),
        "selected_nextest_archive_matrix": {"include": nextest_archives},
        "selected_nextest_archive_count": len(nextest_archives),
        "selected_rust_integration_archive_matrix": {"include": nextest_archive_lanes},
        "selected_rust_integration_archive_lane_count": len(nextest_archive_lanes),
        "selected_setup_classes": selected_setup_classes,
        "workflow_max_parallel": str(parallel_limits["workflow"]),
        "node_max_parallel": str(parallel_limits["node"]),
        "rust_minimal_max_parallel": str(parallel_limits["rust_minimal"]),
        "rust_integration_max_parallel": str(parallel_limits["rust_integration"]),
        "release_max_parallel": str(parallel_limits["release"]),
    }
    emit_grouped_setup_class_payload(payload, smoke_matrix, key_prefix="smoke")
    emit_grouped_setup_class_payload(payload, execution_selected, key_prefix="selected")
    emit(payload)


def heavy_plan(args: argparse.Namespace) -> None:
    catalog_file = catalog_path()
    catalog = normalize_catalog(load_catalog(catalog_file))
    validate_catalog(catalog, repo_root=catalog_repo_root(catalog_file))
    catalog_by_id = {spec["lane_id"]: spec for spec in catalog["lanes"]}
    changed_files = parse_changed_files_input(
        args.changed_files_json,
        args.changed_files_json_file,
    )
    route_lanes = (
        []
        if parse_bool(args.run_all_lanes)
        else select_followup_lanes(changed_files, catalog.get("followup_routes", []))
    )
    active_groups = {
        group
        for enabled, group in [
            (parse_bool(args.run_core_family), "core"),
            (parse_bool(args.run_attestation_family), "attestation"),
            (parse_bool(args.run_workflow_family), "workflow"),
            (parse_bool(args.run_ui_protocol_family), "ui_protocol"),
            (parse_bool(args.run_docs_family), "docs"),
        ]
        if enabled
    }

    explicit_requested_lane = (
        args.event_name == "workflow_dispatch"
        and bool(args.requested_lane)
        and args.requested_lane != "all"
    )

    if route_lanes:
        selected = select_exact(
            catalog_by_id, route_lanes, lane_phase="downstream_lanes"
        )
        smoke_matrix: list[dict] = []
        run_smoke_gate = False
        smoke_gate_kind = ""
    else:
        selected = []
        seen: set[str] = set()
        for spec in catalog["lanes"]:
            lane_id = spec["lane_id"]
            if (
                args.event_name == "workflow_dispatch"
                and args.requested_lane
                and args.requested_lane != "all"
            ):
                if lane_id != args.requested_lane:
                    continue
            elif not parse_bool(args.run_all_lanes):
                if spec.get("explicit_only"):
                    continue
                if not active_groups.intersection(spec.get("groups") or []):
                    continue
            elif spec.get("pilot_only"):
                continue
            if lane_id in seen:
                continue
            seen.add(lane_id)
            selected.append(lane_payload(spec, lane_phase="downstream_lanes"))

        if explicit_requested_lane:
            smoke_matrix = []
            smoke_gate_kind = ""
            run_smoke_gate = False
        else:
            groups = {group for spec in selected for group in (spec.get("groups") or [])}
            has_smoke_gate, smoke_gate_kind = determine_smoke_gate(groups)
            smoke_matrix = (
                select_smoke_matrix(catalog, smoke_gate_kind) if has_smoke_gate else []
            )
            run_smoke_gate = (
                args.event_name != "workflow_dispatch" or parse_bool(args.run_all_lanes)
            ) and bool(smoke_matrix)
            if run_smoke_gate:
                selected = exclude_smoke_gate_lanes(selected, smoke_matrix)

    full_heavy_harvest = explicit_requested_lane is False and parse_bool(args.run_all_lanes)
    parallel_limits = setup_parallel_limits(
        "frontier" if full_heavy_harvest else "targeted", [*smoke_matrix, *selected]
    )
    rust_batching_mode, rust_batching_reason = effective_rust_batching_mode(
        args.rust_batching,
        args.rust_batching_override,
        override_label="SEDNA_HEAVY_RUST_BATCHING",
    )
    execution_selected, batched_by_setup_class, rust_batching_reasons = (
        split_rust_batch_execution_lanes(selected, mode=rust_batching_mode)
    )
    rust_minimal_batch_matrix = batch_lane_matrix(
        batched_by_setup_class["rust_minimal"], setup_class="rust_minimal"
    )
    rust_integration_batch_matrix = batch_lane_matrix(
        batched_by_setup_class["rust_integration"], setup_class="rust_integration"
    )
    if (
        rust_batching_mode != "off"
        and not rust_minimal_batch_matrix
        and not rust_integration_batch_matrix
    ):
        rust_batching_reason = "; ".join(
            [
                rust_batching_reason,
                rust_batching_reasons["rust_minimal"],
                rust_batching_reasons["rust_integration"],
            ]
        )
    planned_matrix = {"include": [*smoke_matrix, *selected]}
    payload = {
        "planned_matrix": planned_matrix,
        "selected_matrix": {"include": selected},
        "execution_selected_matrix": {"include": execution_selected},
        "selected_lane_ids": [lane["lane_id"] for lane in selected],
        "smoke_matrix": {"include": smoke_matrix},
        "run_selected_lanes": "true" if bool(selected) else "false",
        "run_smoke_gate": "true" if run_smoke_gate else "false",
        "smoke_gate_kind": smoke_gate_kind,
        "matrix_fail_fast": "false" if full_heavy_harvest else "true",
        "continue_after_smoke_failure": "true" if full_heavy_harvest else "false",
        "eager_release_lanes": "true" if full_heavy_harvest else "false",
        "workflow_max_parallel": str(parallel_limits["workflow"]),
        "node_max_parallel": str(parallel_limits["node"]),
        "rust_minimal_max_parallel": str(parallel_limits["rust_minimal"]),
        "rust_integration_max_parallel": str(parallel_limits["rust_integration"]),
        "release_max_parallel": str(parallel_limits["release"]),
        "rust_batching_mode": rust_batching_mode,
        "rust_batching_reason": rust_batching_reason,
        "selected_rust_minimal_batch_matrix": {"include": rust_minimal_batch_matrix},
        "selected_rust_minimal_batch_count": len(rust_minimal_batch_matrix),
        "selected_rust_integration_batch_matrix": {"include": rust_integration_batch_matrix},
        "selected_rust_integration_batch_count": len(rust_integration_batch_matrix),
    }
    emit_grouped_setup_class_payload(payload, execution_selected, key_prefix="selected")
    emit_grouped_setup_class_payload(payload, smoke_matrix, key_prefix="smoke")
    emit(payload)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="mode", required=True)

    lab = subparsers.add_parser("lab")
    lab.add_argument("--profile", required=True)
    lab.add_argument("--lane-set", required=True)
    lab.add_argument("--lanes", default="")
    lab.add_argument("--artifact-build", default="false")
    lab.add_argument("--include-explicit-lanes", default="false")
    lab.add_argument("--rust-batching", default="auto")
    lab.add_argument("--rust-batching-override", default="")
    lab.add_argument("--fanout-tier", default="enterprise")
    lab.add_argument("--catalog-path", default="")
    lab.set_defaults(func=lab_plan)

    recommend = subparsers.add_parser("recommend-lab")
    recommend.add_argument("--changed-files-json", default="")
    recommend.add_argument("--catalog-path", default="")
    recommend.set_defaults(func=recommend_lab_plan)

    heavy = subparsers.add_parser("heavy")
    heavy.add_argument("--event-name", required=True)
    heavy.add_argument("--requested-lane", default="")
    heavy.add_argument("--run-all-lanes", required=True)
    heavy.add_argument("--run-core-family", required=True)
    heavy.add_argument("--run-attestation-family", required=True)
    heavy.add_argument("--run-workflow-family", dest="run_workflow_family", required=True)
    heavy.add_argument("--run-ui-protocol-family", required=True)
    heavy.add_argument("--run-docs-family", required=True)
    heavy.add_argument("--changed-files-json", default="")
    heavy.add_argument("--changed-files-json-file", default="")
    heavy.add_argument("--rust-batching", default="auto")
    heavy.add_argument("--rust-batching-override", default="")
    heavy.set_defaults(func=heavy_plan)

    return parser


def main() -> None:
    parser = build_parser()
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
