#!/usr/bin/env python3
"""Resolve workflow lane selections for validation-lab and sedna-heavy-tests."""

from __future__ import annotations

import argparse
import fnmatch
import json
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
RUST_BATCH_MAX_LANES = 3
RUST_BATCH_TARGET_WEIGHT_SECONDS = 1200
DEFAULT_RUST_BATCH_WEIGHT_SECONDS = 360


def catalog_path() -> Path:
    return Path(__file__).resolve().parent.parent / "validation-lanes.json"


def load_catalog(path: Path | None = None) -> dict:
    catalog_file = path or catalog_path()
    payload = json.loads(catalog_file.read_text(encoding="utf-8"))
    if not isinstance(payload, dict) or "lanes" not in payload:
        raise SystemExit(f"invalid validation catalog at {catalog_file}")
    return payload


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
        lane.setdefault("frontier_default", False)
        lane.setdefault("needs_bazel", False)
        lane.setdefault(
            "smoke_gate_only",
            bool(lane.get("smoke_gate_kinds"))
            and str(lane.get("lane_id") or "").endswith("-smoke"),
        )
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

    normalized = dict(catalog)
    normalized["lanes"] = normalized_lanes
    return normalized


def validate_catalog(catalog: dict) -> None:
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

        script_path = lane.get("script_path")
        if not isinstance(script_path, str) or not script_path:
            raise SystemExit(f"lane {lane_id} must set script_path")

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

        resolve_checkout_fetch_depth(lane)


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


def lane_payload(spec: dict, *, lane_phase: str) -> dict:
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


def select_followup_lanes(files: list[str], routes: list[dict]) -> list[str]:
    if not files:
        return []

    matching_routes: list[dict] = []
    for route in routes:
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
        matching_routes.append(route)

    if len(matching_routes) != 1:
        return []
    return list(matching_routes[0].get("lane_ids", []))


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
    return bool(spec.get("smoke_gate_only")) or (
        bool(spec.get("smoke_gate_kinds"))
        and str(spec.get("lane_id") or "").endswith("-smoke")
    )


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


def effective_rust_batching_mode(requested: str, repo_override: str) -> tuple[str, str]:
    requested_mode = normalize_rust_batching_mode(requested)
    override = (repo_override or "").strip().lower()
    if override and override not in {"auto", "off", "force"}:
        return requested_mode, f"ignoring unknown repo override {override!r}"
    if requested_mode == "force":
        return "force", "forced by workflow input"
    if override == "off":
        return "off", "disabled by SEDNA_HEAVY_RUST_BATCHING"
    if override == "force":
        return "force", "forced by SEDNA_HEAVY_RUST_BATCHING"
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


def setup_parallel_limits(profile: str, selected: list[dict] | None = None) -> dict[str, int]:
    counts = Counter(lane["setup_class"] for lane in (selected or []))
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


def determine_lab_matrix_policy(profile: str, selected: list[dict]) -> tuple[str, str, dict[str, int]]:
    fail_fast = "false" if profile == "frontier" else "true"
    parallel_limits = setup_parallel_limits(profile, selected)
    active_limits = [
        parallel_limits[lane["setup_class"]]
        for lane in selected
        if lane["setup_class"] in parallel_limits
    ]
    max_parallel = str(max(active_limits) if active_limits else 1)
    return fail_fast, max_parallel, parallel_limits


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
    catalog = normalize_catalog(
        load_catalog(Path(args.catalog_path) if args.catalog_path else None)
    )
    validate_catalog(catalog)
    catalog_by_id = {spec["lane_id"]: spec for spec in catalog["lanes"]}
    requested_lanes = [lane.strip() for lane in args.lanes.split(",") if lane.strip()]
    run_artifact = args.profile == "artifact" or parse_bool(args.artifact_build)
    include_explicit_lanes = parse_bool(args.include_explicit_lanes)

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

    matrix_fail_fast, matrix_max_parallel, parallel_limits = determine_lab_matrix_policy(
        args.profile, selected
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
        "selected_setup_classes": selected_setup_classes,
        "workflow_max_parallel": str(parallel_limits["workflow"]),
        "node_max_parallel": str(parallel_limits["node"]),
        "rust_minimal_max_parallel": str(parallel_limits["rust_minimal"]),
        "rust_integration_max_parallel": str(parallel_limits["rust_integration"]),
        "release_max_parallel": str(parallel_limits["release"]),
    }
    emit_grouped_setup_class_payload(payload, smoke_matrix, key_prefix="smoke")
    emit_grouped_setup_class_payload(payload, selected, key_prefix="selected")
    emit(payload)


def heavy_plan(args: argparse.Namespace) -> None:
    catalog = normalize_catalog(load_catalog())
    validate_catalog(catalog)
    catalog_by_id = {spec["lane_id"]: spec for spec in catalog["lanes"]}
    changed_files = json.loads(args.changed_files_json) if args.changed_files_json else []
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
        args.rust_batching, args.rust_batching_override
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
    if not rust_minimal_batch_matrix and not rust_integration_batch_matrix:
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
    lab.add_argument("--catalog-path", default="")
    lab.set_defaults(func=lab_plan)

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
