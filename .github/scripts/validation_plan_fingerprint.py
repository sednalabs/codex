#!/usr/bin/env python3
"""Fingerprint a resolved validation-lab plan for exact evidence reuse."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
from typing import Any

FINGERPRINT_SCHEMA_VERSION = 1


def parse_bool(value: str | bool | None) -> bool:
    if isinstance(value, bool):
        return value
    return str(value or "").strip().lower() in {"1", "true", "yes", "on"}


def parse_explicit_lanes(value: str | None) -> list[str]:
    return [lane.strip() for lane in str(value or "").split(",") if lane.strip()]


def selection_value(selection_meta: dict[str, Any], key: str, fallback: Any) -> Any:
    value = selection_meta.get(key, fallback)
    return fallback if value is None else value


def plan_fingerprint_payload(
    *,
    selection_meta: dict[str, Any],
    workflow: str,
    workflow_ref: str,
    workflow_sha: str,
    target_head_sha: str,
    profile: str,
    lane_set: str,
    fanout_tier: str,
    lanes: str,
    rust_batching: str,
    artifact_build: str | bool,
    include_explicit_lanes: str | bool,
) -> dict[str, Any]:
    return {
        "schema": FINGERPRINT_SCHEMA_VERSION,
        "workflow": {
            "name": workflow,
            "ref": workflow_ref,
            "sha": workflow_sha,
        },
        "target": {
            "head_sha": target_head_sha,
        },
        "inputs": {
            "profile": profile,
            "lane_set": lane_set,
            "fanout_tier": fanout_tier,
            "explicit_lanes": parse_explicit_lanes(lanes),
            "rust_batching": rust_batching,
            "artifact_build": parse_bool(artifact_build),
            "include_explicit_lanes": parse_bool(include_explicit_lanes),
        },
        "resolved": {
            "fanout_tier": selection_value(selection_meta, "fanout_tier", fanout_tier),
            "run_selected_lanes": selection_value(
                selection_meta, "run_selected_lanes", False
            ),
            "run_smoke_gate": selection_value(selection_meta, "run_smoke_gate", False),
            "smoke_gate_kind": selection_value(selection_meta, "smoke_gate_kind", ""),
            "run_artifact": selection_value(selection_meta, "run_artifact", False),
            "matrix_fail_fast": selection_value(selection_meta, "matrix_fail_fast", False),
            "matrix_max_parallel": selection_value(selection_meta, "matrix_max_parallel", 1),
            "workflow_max_parallel": selection_value(
                selection_meta, "workflow_max_parallel", 1
            ),
            "node_max_parallel": selection_value(selection_meta, "node_max_parallel", 1),
            "rust_minimal_max_parallel": selection_value(
                selection_meta, "rust_minimal_max_parallel", 1
            ),
            "rust_integration_max_parallel": selection_value(
                selection_meta, "rust_integration_max_parallel", 1
            ),
            "release_max_parallel": selection_value(
                selection_meta, "release_max_parallel", 1
            ),
            "rust_batching_mode": selection_value(
                selection_meta, "rust_batching_mode", "off"
            ),
            "selected_setup_classes": selection_value(
                selection_meta, "selected_setup_classes", []
            ),
            "selected_lane_ids": selection_value(selection_meta, "selected_lane_ids", []),
            "planned_matrix": selection_value(selection_meta, "planned_matrix", {}),
            "smoke_matrix": selection_value(selection_meta, "smoke_matrix", {}),
            "selected_matrix": selection_value(selection_meta, "selected_matrix", {}),
            "selected_rust_minimal_batch_matrix": selection_value(
                selection_meta, "selected_rust_minimal_batch_matrix", {}
            ),
            "selected_rust_integration_batch_matrix": selection_value(
                selection_meta, "selected_rust_integration_batch_matrix", {}
            ),
        },
    }


def fingerprint_payload(payload: dict[str, Any]) -> str:
    source = json.dumps(payload, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(source.encode("utf-8")).hexdigest()[:16]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--selection-meta-env", default="SELECTION_META")
    parser.add_argument("--selection-meta-path")
    parser.add_argument("--workflow", required=True)
    parser.add_argument("--workflow-ref", required=True)
    parser.add_argument("--workflow-sha", required=True)
    parser.add_argument("--target-head-sha", required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--lane-set", required=True)
    parser.add_argument("--fanout-tier", required=True)
    parser.add_argument("--lanes", default="")
    parser.add_argument("--rust-batching", default="auto")
    parser.add_argument("--artifact-build", default="false")
    parser.add_argument("--include-explicit-lanes", default="false")
    parser.add_argument("--json", action="store_true")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.selection_meta_path:
        selection_meta_raw = Path(args.selection_meta_path).read_text(encoding="utf-8")
    else:
        selection_meta_raw = os.environ.get(args.selection_meta_env)
        if selection_meta_raw is None:
            raise SystemExit(f"missing selection metadata env: {args.selection_meta_env}")
    selection_meta = json.loads(selection_meta_raw)
    payload = plan_fingerprint_payload(
        selection_meta=selection_meta,
        workflow=args.workflow,
        workflow_ref=args.workflow_ref,
        workflow_sha=args.workflow_sha,
        target_head_sha=args.target_head_sha,
        profile=args.profile,
        lane_set=args.lane_set,
        fanout_tier=args.fanout_tier,
        lanes=args.lanes,
        rust_batching=args.rust_batching,
        artifact_build=args.artifact_build,
        include_explicit_lanes=args.include_explicit_lanes,
    )
    fingerprint = fingerprint_payload(payload)
    if args.json:
        print(json.dumps({"fingerprint": fingerprint, "payload": payload}, sort_keys=True))
    else:
        print(fingerprint)


if __name__ == "__main__":
    main()
