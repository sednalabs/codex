#!/usr/bin/env python3
"""Resolve workflow lane selections for validation-lab and sedna-heavy-tests."""

from __future__ import annotations

import argparse
import fnmatch
import json
from collections import Counter
from collections import OrderedDict
from pathlib import Path

VALID_SETUP_CLASSES = {"light", "rust", "heavy"}
VALID_FRONTIER_ROLES = {"sentinel", "depth"}
VALID_STATUS_CLASSES = {"active", "legacy"}
VALID_COST_CLASSES = {"low", "medium", "high"}
ORDERED_SETUP_CLASSES = ["light", "rust", "heavy"]


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


def derive_setup_class(lane: dict) -> str:
    groups = set(lane.get("groups", []))
    lane_id = str(lane.get("lane_id") or "")
    lane_sets = set(lane.get("lane_sets", []))

    if groups & {"workflow", "docs"}:
        return "light"
    if lane.get("install_nextest"):
        return "heavy"
    if lane.get("smoke_gate_kinds"):
        return "heavy"
    if "release" in lane_sets:
        return "heavy"
    if any(
        token in lane_id
        for token in (
            "compile-smoke",
            "core-smoke",
            "ui-smoke",
            "ledger-smoke",
            "runtime-surface-smoke",
        )
    ):
        return "heavy"
    return "rust"


def derive_cost_class(setup_class: str) -> str:
    return {
        "light": "low",
        "rust": "medium",
        "heavy": "high",
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
    """Backfill derived lane metadata for older target refs."""

    normalized_lanes: list[dict] = []
    family_sentinel_ids: dict[tuple[str, str], str] = {}

    for original in catalog["lanes"]:
        lane = dict(original)
        lane.setdefault("status_class", "active")
        lane.setdefault("setup_class", derive_setup_class(lane))
        lane.setdefault("summary_family", derive_summary_family(lane))
        lane.setdefault("cost_class", derive_cost_class(lane["setup_class"]))
        lane.setdefault("frontier_default", False)
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


def lane_payload(spec: dict, *, lane_phase: str) -> dict:
    return {
        "lane_id": spec["lane_id"],
        "lane_phase": lane_phase,
        "run_command": spec["run_command"],
        "groups": spec["groups"],
        "install_nextest": bool(spec.get("install_nextest", False)),
        "status_class": spec["status_class"],
        "frontier_default": bool(spec.get("frontier_default", False)),
        "setup_class": spec["setup_class"],
        "frontier_role": spec["frontier_role"],
        "summary_family": spec["summary_family"],
        "cost_class": spec["cost_class"],
    }


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
    return bool(spec.get("smoke_gate_only"))


def select_frontier_all(catalog: dict, *, include_explicit_only: bool = False) -> list[dict]:
    allowed_status_classes = {"active", "legacy"} if include_explicit_only else {"active"}
    return [
        lane_payload(spec, lane_phase="downstream_lanes")
        for spec in catalog["lanes"]
        if spec.get("status_class") in allowed_status_classes
        and (include_explicit_only or not spec.get("explicit_only"))
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


def setup_parallel_limits(profile: str, selected: list[dict] | None = None) -> dict[str, int]:
    counts = Counter(lane["setup_class"] for lane in (selected or []))
    if profile == "frontier":
        return {
            "light": max(1, min(counts.get("light", 0), 12)),
            "rust": max(1, min(counts.get("rust", 0), 32)),
            "heavy": max(1, min(counts.get("heavy", 0), 16)),
        }
    if profile in {"broad", "full"}:
        return {
            "light": max(1, min(counts.get("light", 0), 10)),
            "rust": max(1, min(counts.get("rust", 0), 24)),
            "heavy": max(1, min(counts.get("heavy", 0), 10)),
        }
    if profile == "smoke":
        return {"light": 6, "rust": 4, "heavy": 3}
    return {"light": 8, "rust": 4, "heavy": 2}


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
        groups = {group for spec in selected for group in spec["groups"]}
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

    emit(
        {
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
            "selected_light_matrix": {"include": grouped["light"]},
            "selected_rust_matrix": {"include": grouped["rust"]},
            "selected_heavy_matrix": {"include": grouped["heavy"]},
            "selected_light_lane_count": len(grouped["light"]),
            "selected_rust_lane_count": len(grouped["rust"]),
            "selected_heavy_lane_count": len(grouped["heavy"]),
            "light_max_parallel": str(parallel_limits["light"]),
            "rust_max_parallel": str(parallel_limits["rust"]),
            "heavy_max_parallel": str(parallel_limits["heavy"]),
        }
    )


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
                if not active_groups.intersection(spec["groups"]):
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
            groups = {group for spec in selected for group in spec["groups"]}
            has_smoke_gate, smoke_gate_kind = determine_smoke_gate(groups)
            smoke_matrix = (
                select_smoke_matrix(catalog, smoke_gate_kind) if has_smoke_gate else []
            )
            run_smoke_gate = (
                args.event_name != "workflow_dispatch" or parse_bool(args.run_all_lanes)
            ) and bool(smoke_matrix)
            if run_smoke_gate:
                selected = exclude_smoke_gate_lanes(selected, smoke_matrix)

    manual_harvest = explicit_requested_lane is False and (
        args.event_name == "workflow_dispatch" and parse_bool(args.run_all_lanes)
    )
    parallel_limits = setup_parallel_limits(
        "frontier" if manual_harvest else "targeted", [*smoke_matrix, *selected]
    )
    planned_matrix = {"include": [*smoke_matrix, *selected]}
    payload = {
        "planned_matrix": planned_matrix,
        "selected_matrix": {"include": selected},
        "selected_lane_ids": [lane["lane_id"] for lane in selected],
        "smoke_matrix": {"include": smoke_matrix},
        "run_selected_lanes": "true" if bool(selected) else "false",
        "run_smoke_gate": "true" if run_smoke_gate else "false",
        "smoke_gate_kind": smoke_gate_kind,
        "matrix_fail_fast": "false" if manual_harvest else "true",
        "continue_after_smoke_failure": "true" if manual_harvest else "false",
        "light_max_parallel": str(parallel_limits["light"]),
        "rust_max_parallel": str(parallel_limits["rust"]),
        "heavy_max_parallel": str(parallel_limits["heavy"]),
    }
    emit_grouped_setup_class_payload(payload, selected, key_prefix="selected")
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
    heavy.set_defaults(func=heavy_plan)

    return parser


def main() -> None:
    parser = build_parser()
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
