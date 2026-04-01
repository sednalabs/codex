#!/usr/bin/env python3
"""Aggregate per-lane validation summaries into one workflow summary artifact."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

FAILED_OUTCOMES = {"failure", "cancelled", "timed_out", "action_required", "startup_failure", "stale"}
SUCCESS_OUTCOMES = {"success", "neutral", "skipped"}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", required=True)
    parser.add_argument("--display-ref", required=True)
    parser.add_argument("--checkout-ref", required=True)
    parser.add_argument("--head-sha", required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--lane-set", required=True)
    parser.add_argument("--explicit-lanes", default="")
    parser.add_argument("--supersession-mode", default="auto")
    parser.add_argument("--supersession-key", default="")
    parser.add_argument("--notes-supplied", default="false")
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument("--run-url", required=True)
    parser.add_argument("--run-selected-lanes", required=True)
    parser.add_argument("--run-smoke-gate", required=True)
    parser.add_argument("--smoke-gate-kind", default="")
    parser.add_argument("--smoke-gate-result", default="skipped")
    parser.add_argument("--downstream-result", default="skipped")
    parser.add_argument("--run-artifact", required=True)
    parser.add_argument("--artifact-result", default="skipped")
    parser.add_argument("--lane-summary-dir", required=True)
    parser.add_argument("--output", required=True)
    return parser.parse_args()


def parse_bool(raw: str) -> bool:
    return raw.lower() == "true"


def load_lane_summaries(directory: Path) -> list[dict]:
    summaries: list[dict] = []
    if not directory.exists():
        return summaries
    for path in sorted(directory.rglob("*.json")):
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            continue
        if isinstance(payload, dict):
            summaries.append(payload)
    return summaries


def summarize_lanes(lanes: list[dict], *, profile: str) -> tuple[dict | None, dict]:
    lane_count = 0
    successful_lane_count = 0
    failed_lane_count = 0
    other_lane_count = 0
    first_failure = None
    failed_lanes = []
    total_duration_ms = 0
    phase_runtime_ms: dict[str, int] = {}
    lanes_with_runtime = []

    for lane in lanes:
        lane_count += 1
        outcome = str(lane.get("outcome") or "")
        lane_phase = str(lane.get("lane_phase") or "downstream_lanes")
        duration_ms = lane.get("duration_ms")
        if isinstance(duration_ms, int) and duration_ms >= 0:
            total_duration_ms += duration_ms
            phase_runtime_ms[lane_phase] = phase_runtime_ms.get(lane_phase, 0) + duration_ms
            lanes_with_runtime.append(
                {
                    "lane_id": lane.get("lane_id"),
                    "lane_phase": lane_phase,
                    "duration_ms": duration_ms,
                    "outcome": lane.get("outcome"),
                }
            )
        if outcome in SUCCESS_OUTCOMES:
            successful_lane_count += 1
        elif outcome in FAILED_OUTCOMES:
            failed_lane_count += 1
            failed_lane = {
                "lane_id": lane.get("lane_id"),
                "outcome": lane.get("outcome"),
                "signal": lane.get("primary_signal") or "",
            }
            failed_lanes.append(failed_lane)
            if first_failure is None:
                first_failure = failed_lane
        else:
            other_lane_count += 1

    summary = {
        "lane_count": lane_count,
        "successful_lane_count": successful_lane_count,
        "failed_lane_count": failed_lane_count,
        "other_lane_count": other_lane_count,
        "total_duration_ms": total_duration_ms,
        "phase_runtime_ms": dict(
            sorted(phase_runtime_ms.items(), key=lambda item: item[1], reverse=True)
        ),
        "top_slowest_lanes": sorted(
            lanes_with_runtime, key=lambda lane: lane["duration_ms"], reverse=True
        )[:5],
        "first_failure": first_failure,
        "failed_lanes": failed_lanes,
        "candidate_next_slices": failed_lanes if profile == "frontier" else [],
    }
    return first_failure, summary


def overall_conclusion(first_failure: dict | None, args: argparse.Namespace) -> str:
    terminal_results = {
        args.smoke_gate_result,
        args.downstream_result,
        args.artifact_result,
    }
    if first_failure is not None or terminal_results & FAILED_OUTCOMES:
        return "failure"
    if parse_bool(args.run_artifact) and args.artifact_result == "success":
        return "success"
    if parse_bool(args.run_selected_lanes) and args.downstream_result == "success":
        return "success"
    if parse_bool(args.run_smoke_gate) and args.smoke_gate_result == "success":
        return "success"
    return "unknown"


def main() -> None:
    args = parse_args()
    lane_summaries = load_lane_summaries(Path(args.lane_summary_dir))
    first_failure, lane_summary = summarize_lanes(lane_summaries, profile=args.profile)
    explicit_lanes = [lane.strip() for lane in args.explicit_lanes.split(",") if lane.strip()]

    payload = {
        "repo": args.repo,
        "ref": {
            "display_ref": args.display_ref,
            "checkout_ref": args.checkout_ref,
            "head_sha": args.head_sha,
        },
        "selection": {
            "profile": args.profile,
            "lane_set": args.lane_set,
            "explicit_lanes": explicit_lanes,
            "notes_supplied": parse_bool(args.notes_supplied),
            "baseline_required": args.profile == "frontier",
            "supersession": {
                "mode": args.supersession_mode or "auto",
                "key": args.supersession_key or "",
                "auto_supersedes": (args.supersession_mode or "auto") == "auto",
            },
        },
        "run": {
            "run_id": args.run_id,
            "run_attempt": args.run_attempt,
            "url": args.run_url,
        },
        "jobs": {
            "smoke_gate": {
                "planned": parse_bool(args.run_smoke_gate),
                "kind": args.smoke_gate_kind,
                "result": args.smoke_gate_result,
            },
            "downstream_lanes": {
                "planned": parse_bool(args.run_selected_lanes),
                "result": args.downstream_result,
            },
            "artifact": {
                "planned": parse_bool(args.run_artifact),
                "result": args.artifact_result,
            },
        },
        "lanes": lane_summaries,
        "summary": {
            **lane_summary,
            "overall_conclusion": overall_conclusion(first_failure, args),
        },
    }

    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
