#!/usr/bin/env python3
"""Resolve workflow lane selections for validation-lab and sedna-heavy-tests."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def catalog_path() -> Path:
    return Path(__file__).resolve().parent.parent / "validation-lanes.json"


def load_catalog() -> list[dict]:
    return json.loads(catalog_path().read_text(encoding="utf-8"))["lanes"]


def select_exact(catalog_by_id: dict[str, dict], lane_ids: list[str]) -> list[dict]:
    selected: list[dict] = []
    seen: set[str] = set()
    for lane_id in lane_ids:
        spec = catalog_by_id.get(lane_id)
        if spec is None:
            raise SystemExit(f"unknown lane id: {lane_id}")
        if lane_id in seen:
            continue
        seen.add(lane_id)
        selected.append(
            {
                "lane_id": lane_id,
                "run_command": spec["run_command"],
                "groups": spec["groups"],
            }
        )
    return selected


def select_for_lane_set(
    catalog: list[dict], target_lane_set: str, *, include_explicit_only: bool = False
) -> list[dict]:
    selected: list[dict] = []
    for spec in catalog:
        if target_lane_set not in spec["lane_sets"]:
            continue
        if spec.get("explicit_only") and not include_explicit_only:
            continue
        selected.append(
            {
                "lane_id": spec["lane_id"],
                "run_command": spec["run_command"],
                "groups": spec["groups"],
            }
        )
    return selected


def emit(payload: dict) -> None:
    print(json.dumps(payload, separators=(",", ":")))


def parse_bool(value: str) -> bool:
    return value.lower() == "true"


def determine_smoke_gate(groups: set[str]) -> tuple[bool, str]:
    has_runtime = bool(groups & {"core", "ui_protocol", "attestation"})
    has_docs = bool(groups & {"workflow", "docs"})
    smoke_gate_kind = "runtime" if has_runtime else "workflow_docs" if has_docs else ""
    return bool(has_runtime or has_docs), smoke_gate_kind


def determine_lab_matrix_policy(profile: str) -> tuple[str, str]:
    policies = {
        "targeted": ("true", "2"),
        "frontier": ("false", "3"),
        "broad": ("true", "4"),
        "full": ("true", "3"),
        "artifact": ("true", "2"),
    }
    return policies.get(profile, ("true", "1"))


def lab_plan(args: argparse.Namespace) -> None:
    catalog = load_catalog()
    catalog_by_id = {spec["lane_id"]: spec for spec in catalog}
    requested_lanes = [lane.strip() for lane in args.lanes.split(",") if lane.strip()]
    run_artifact = args.profile == "artifact" or parse_bool(args.artifact_build)
    matrix_fail_fast, matrix_max_parallel = determine_lab_matrix_policy(args.profile)

    if requested_lanes:
        selected = select_exact(catalog_by_id, requested_lanes)
        run_smoke_gate = False
        smoke_gate_kind = ""
    elif args.profile == "smoke":
        selected = []
        run_smoke_gate = True
        smoke_gate_kind = "workflow_docs" if args.lane_set == "docs" else "runtime"
    elif args.profile == "artifact":
        selected = [] if args.lane_set == "all" else select_for_lane_set(catalog, args.lane_set)
        run_smoke_gate = False
        smoke_gate_kind = ""
    elif args.profile == "targeted":
        if args.lane_set == "all":
            raise SystemExit("profile=targeted requires a named lane_set or explicit lanes")
        selected = select_for_lane_set(catalog, args.lane_set)
        run_smoke_gate = False
        smoke_gate_kind = ""
    elif args.profile == "frontier":
        if args.lane_set == "all":
            raise SystemExit("profile=frontier requires a named lane_set or explicit lanes")
        selected = select_for_lane_set(catalog, args.lane_set)
        run_smoke_gate = False
        smoke_gate_kind = ""
    elif args.profile in {"broad", "full"}:
        selected = select_for_lane_set(catalog, "all" if args.lane_set == "all" else args.lane_set)
        groups = {group for spec in selected for group in spec["groups"]}
        has_smoke_gate, smoke_gate_kind = determine_smoke_gate(groups)
        run_smoke_gate = bool(selected) and has_smoke_gate
    else:
        raise SystemExit(f"unsupported profile: {args.profile}")

    emit(
        {
            "selected_matrix": {"include": selected},
            "run_selected_lanes": "true" if bool(selected) else "false",
            "run_smoke_gate": "true" if run_smoke_gate else "false",
            "smoke_gate_kind": smoke_gate_kind,
            "run_artifact": "true" if run_artifact else "false",
            "matrix_fail_fast": matrix_fail_fast,
            "matrix_max_parallel": matrix_max_parallel,
        }
    )


def heavy_plan(args: argparse.Namespace) -> None:
    catalog = load_catalog()
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

    selected: list[dict] = []
    seen: set[str] = set()
    for spec in catalog:
        lane_id = spec["lane_id"]
        if args.event_name == "workflow_dispatch" and args.requested_lane and args.requested_lane != "all":
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
        selected.append(
            {
                "lane_id": lane_id,
                "run_command": spec["run_command"],
                "groups": spec["groups"],
            }
        )

    groups = {group for spec in selected for group in spec["groups"]}
    has_smoke_gate, smoke_gate_kind = determine_smoke_gate(groups)
    run_smoke_gate = (args.event_name != "workflow_dispatch" or parse_bool(args.run_all_lanes)) and has_smoke_gate

    emit(
        {
            "selected_matrix": {"include": selected},
            "run_selected_lanes": "true" if bool(selected) else "false",
            "run_smoke_gate": "true" if run_smoke_gate else "false",
            "smoke_gate_kind": smoke_gate_kind,
        }
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="mode", required=True)

    lab = subparsers.add_parser("lab")
    lab.add_argument("--profile", required=True)
    lab.add_argument("--lane-set", required=True)
    lab.add_argument("--lanes", default="")
    lab.add_argument("--artifact-build", default="false")
    lab.set_defaults(func=lab_plan)

    heavy = subparsers.add_parser("heavy")
    heavy.add_argument("--event-name", required=True)
    heavy.add_argument("--requested-lane", default="")
    heavy.add_argument("--run-all-lanes", required=True)
    heavy.add_argument("--run-core-family", required=True)
    heavy.add_argument("--run-attestation-family", required=True)
    heavy.add_argument("--run-workflow-family", required=True)
    heavy.add_argument("--run-ui-protocol-family", required=True)
    heavy.add_argument("--run-docs-family", required=True)
    heavy.set_defaults(func=heavy_plan)

    return parser


def main() -> None:
    parser = build_parser()
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
