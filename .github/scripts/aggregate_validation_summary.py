#!/usr/bin/env python3
"""Aggregate per-lane validation summaries into one workflow summary artifact."""

from __future__ import annotations

import argparse
import json
from collections import OrderedDict
from json import JSONDecodeError
from pathlib import Path

FAILED_OUTCOMES = {"failure", "cancelled", "missing", "timed_out", "action_required", "startup_failure", "stale"}
SUCCESS_OUTCOMES = {"success", "neutral", "skipped"}
BLOCKER_OUTCOMES = {"failure", "cancelled", "missing"}
OUTCOME_PRIORITY = {"failure": 0, "cancelled": 1, "missing": 2}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", required=True)
    parser.add_argument("--display-ref", required=True)
    parser.add_argument("--checkout-ref", required=True)
    parser.add_argument("--head-sha", required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--lane-set", required=True)
    parser.add_argument("--profile-intent", default="")
    parser.add_argument("--profile-notes", default="")
    parser.add_argument("--lane-summary", default="")
    parser.add_argument("--planned-matrix-json", default="")
    parser.add_argument("--selected-lane-ids-json", default="")
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
    parser.add_argument("--light-result", default="skipped")
    parser.add_argument("--rust-result", default="skipped")
    parser.add_argument("--heavy-result", default="skipped")
    parser.add_argument("--run-artifact", required=True)
    parser.add_argument("--artifact-result", default="skipped")
    parser.add_argument("--matrix-fail-fast", default="false")
    parser.add_argument("--lane-summary-dir", required=True)
    parser.add_argument("--output", required=True)
    return parser.parse_args()


def parse_bool(raw: str) -> bool:
    return raw.strip().lower() in {"1", "true", "yes", "on"}


def parse_json_argument(raw: str, fallback: object) -> tuple[object, bool]:
    if not raw.strip():
        return fallback, False
    try:
        return json.loads(raw), False
    except JSONDecodeError:
        return fallback, True


def load_lane_summaries(directory: Path) -> dict[str, dict]:
    summaries: dict[str, dict] = {}
    if not directory.exists():
        return summaries
    for path in sorted(directory.rglob("*.json")):
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except JSONDecodeError:
            continue
        if not isinstance(payload, dict):
            continue
        lane_id = payload.get("lane_id")
        if not lane_id:
            continue
        summaries[str(lane_id)] = payload
    return summaries


def lane_status(lane: dict) -> str:
    outcome = lane["outcome"]
    if outcome == "success":
        return "pass"
    if outcome == "failure":
        return "fail"
    if outcome == "cancelled":
        return "cancelled"
    if outcome == "missing":
        return "missing"
    if outcome == "skipped":
        return "skipped"
    return "unknown"


def lane_signal(lane: dict) -> str:
    signal = str(lane.get("primary_signal") or "").strip()
    if signal:
        return signal
    exit_code = lane.get("exit_code")
    outcome = lane.get("outcome")
    if outcome == "failure" and exit_code is not None:
        return f"exit {exit_code}"
    if outcome == "failure":
        return "lane command failed"
    if outcome == "cancelled":
        return "lane cancelled before payload upload"
    if outcome == "missing":
        return "lane artifact missing"
    return ""


def choose_family_blocker(failing: list[dict]) -> dict:
    def sort_key(lane: dict) -> tuple[int, int, str]:
        return (
            OUTCOME_PRIORITY.get(lane["outcome"], len(OUTCOME_PRIORITY)),
            0 if lane["frontier_role"] == "sentinel" else 1,
            str(lane["lane_id"]),
        )

    return min(failing, key=sort_key)


def blocked_finding_count(primary: list[dict], secondary: list[dict]) -> int:
    return len(primary) + len(secondary)


def blocker_sort_key(item: dict) -> tuple[int, int, int, str]:
    kind_priority = {
        "planner": 0,
        "setup_class": 1,
        "family": 2,
        "lane": 3,
    }
    return (
        kind_priority.get(str(item.get("kind") or ""), 9),
        OUTCOME_PRIORITY.get(str(item.get("outcome") or ""), len(OUTCOME_PRIORITY)),
        0 if item.get("frontier_role") == "sentinel" else 1,
        str(item.get("lane_id") or ""),
    )


def combined_result(*job_results: str) -> str:
    normalized = [result for result in job_results if result and result != "skipped"]
    if any(result == "failure" for result in normalized):
        return "failure"
    if any(result == "cancelled" for result in normalized):
        return "cancelled"
    if any(result == "success" for result in normalized):
        return "success"
    return "skipped"


def expected_lane_order(planned_matrix: list[dict], selected_lane_ids: list[str]) -> list[str]:
    ordered: list[str] = []
    seen: set[str] = set()
    for lane in planned_matrix:
        lane_id = str(lane.get("lane_id") or "")
        if lane_id and lane_id not in seen:
            seen.add(lane_id)
            ordered.append(lane_id)
    for lane_id in selected_lane_ids:
        if lane_id and lane_id not in seen:
            seen.add(lane_id)
            ordered.append(lane_id)
    return ordered


def build_results(
    planned_matrix: list[dict],
    selected_lane_ids: list[str],
    actual_by_lane: dict[str, dict],
    smoke_gate_result: str,
    setup_class_results: dict[str, str],
    *,
    matrix_fail_fast: bool,
) -> list[dict]:
    expected = OrderedDict((lane["lane_id"], dict(lane)) for lane in planned_matrix)
    ordered_lane_ids = expected_lane_order(planned_matrix, selected_lane_ids)
    setup_class_has_failure_artifact: dict[str, bool] = {}

    for lane in planned_matrix:
        lane_id = lane["lane_id"]
        payload = actual_by_lane.get(lane_id)
        if payload and payload.get("outcome") == "failure":
            setup_class = str(lane.get("setup_class") or payload.get("setup_class") or "rust")
            setup_class_has_failure_artifact[setup_class] = True

    results: list[dict] = []
    for lane_id in ordered_lane_ids:
        lane = dict(expected.get(lane_id) or {"lane_id": lane_id})
        payload = actual_by_lane.get(lane_id)
        if payload is None:
            if lane.get("lane_phase") == "smoke_gate":
                outcome = (
                    "cancelled"
                    if smoke_gate_result in {"failure", "cancelled"}
                    else "missing"
                )
            else:
                setup_class = str(lane.get("setup_class") or "rust")
                job_result = setup_class_results.get(setup_class, "skipped")
                cancelled_by_fail_fast = (
                    matrix_fail_fast
                    and job_result == "failure"
                    and setup_class_has_failure_artifact.get(setup_class, False)
                )
                if job_result == "cancelled" or cancelled_by_fail_fast:
                    outcome = "cancelled"
                elif job_result in {"failure", "success", "skipped"}:
                    outcome = "missing"
                else:
                    outcome = "missing"
            lane.update(
                {
                    "outcome": outcome,
                    "exit_code": None,
                    "status_class": lane.get("status_class", "active"),
                    "started_at_ms": None,
                    "finished_at_ms": None,
                    "duration_ms": None,
                    "log_available": False,
                    "lane_phase": lane.get("lane_phase", "downstream_lanes"),
                    "frontier_default": bool(lane.get("frontier_default", False)),
                    "setup_class": lane.get("setup_class", "rust"),
                    "frontier_role": lane.get("frontier_role", "sentinel"),
                    "summary_family": lane.get("summary_family", lane_id),
                    "cost_class": lane.get("cost_class", "medium"),
                    "primary_signal": "",
                    "error_lines": [],
                    "tail_excerpt": [],
                    "artifact_name": "",
                }
            )
        else:
            lane.update(payload)
            lane.setdefault("status_class", lane.get("status_class", "active"))
            lane.setdefault("frontier_default", bool(lane.get("frontier_default", False)))
            lane.setdefault("setup_class", lane.get("setup_class", "rust"))
            lane.setdefault("frontier_role", lane.get("frontier_role", "sentinel"))
            lane.setdefault("summary_family", lane.get("summary_family", lane_id))
            lane.setdefault("cost_class", lane.get("cost_class", "medium"))
        results.append(lane)
    return results


def setup_class_rows(results: list[dict], setup_class_results: dict[str, str]) -> list[dict]:
    rows: list[dict] = []
    ordered_classes: list[str] = []
    for lane in results:
        if lane.get("lane_phase") != "downstream_lanes":
            continue
        setup_class = str(lane["setup_class"])
        if setup_class not in ordered_classes:
            ordered_classes.append(setup_class)
    for setup_class in ordered_classes:
        lanes = [
            lane
            for lane in results
            if lane.get("lane_phase") == "downstream_lanes"
            and lane["setup_class"] == setup_class
        ]
        started = sum(1 for lane in lanes if lane.get("started_at_ms") is not None)
        rows.append(
            {
                "setup_class": setup_class,
                "job_result": setup_class_results.get(setup_class, "skipped"),
                "selected_lane_count": len(lanes),
                "started_lane_count": started,
                "status_classes": sorted({lane["status_class"] for lane in lanes}),
                "lane_ids": [lane["lane_id"] for lane in lanes],
            }
        )
    return rows


def derive_primary_and_secondary(
    results: list[dict], setup_rows: list[dict]
) -> tuple[list[dict], list[dict]]:
    primary: list[dict] = []
    secondary: list[dict] = []
    setup_blocked_classes: set[str] = set()

    for row in setup_rows:
        setup_class = row["setup_class"]
        job_result = row["job_result"]
        lanes = [
            lane
            for lane in results
            if lane.get("lane_phase") == "downstream_lanes"
            and lane["setup_class"] == setup_class
        ]
        if job_result not in {"failure", "cancelled"}:
            continue
        if any(lane.get("started_at_ms") is not None for lane in lanes):
            continue
        setup_blocked_classes.add(setup_class)
        primary.append(
            {
                "kind": "setup_class",
                "lane_id": f"setup-class:{setup_class}",
                "setup_class": setup_class,
                "status_class": "active",
                "job_result": job_result,
                "lane_ids": row["lane_ids"],
                "signal": "setup failed before any lane started"
                if job_result == "failure"
                else "setup class fanout cancelled before any lane started",
            }
        )

    families: OrderedDict[tuple[str, str], list[dict]] = OrderedDict()
    for lane in results:
        key = (lane["status_class"], lane["summary_family"])
        families.setdefault(key, []).append(lane)

    primary_lane_ids: set[str] = set()
    for (status_class, family), lanes in families.items():
        if status_class != "active":
            continue
        failing = [
            lane
            for lane in lanes
            if lane["outcome"] in BLOCKER_OUTCOMES
            and lane.get("setup_class") not in setup_blocked_classes
        ]
        if not failing:
            continue
        chosen = choose_family_blocker(failing)
        primary_lane_ids.add(chosen["lane_id"])
        primary.append(
            {
                "kind": "family",
                "status_class": status_class,
                "summary_family": family,
                "lane_id": chosen["lane_id"],
                "frontier_role": chosen["frontier_role"],
                "setup_class": chosen["setup_class"],
                "outcome": chosen["outcome"],
                "exit_code": chosen.get("exit_code"),
                "signal": lane_signal(chosen),
            }
        )

    for lane in results:
        if lane["outcome"] not in BLOCKER_OUTCOMES:
            continue
        if lane.get("lane_phase") == "downstream_lanes" and lane["setup_class"] in setup_blocked_classes:
            continue
        if lane["lane_id"] in primary_lane_ids:
            continue
        secondary.append(
            {
                "kind": "lane",
                "status_class": lane["status_class"],
                "summary_family": lane["summary_family"],
                "lane_id": lane["lane_id"],
                "frontier_role": lane["frontier_role"],
                "setup_class": lane["setup_class"],
                "outcome": lane["outcome"],
                "exit_code": lane.get("exit_code"),
                "signal": lane_signal(lane),
            }
        )

    return primary, secondary


def summarize_runtime(results: list[dict]) -> tuple[int, dict[str, int], list[dict]]:
    total_duration_ms = 0
    phase_runtime_ms: dict[str, int] = {}
    lanes_with_runtime: list[dict] = []
    for lane in results:
        duration_ms = lane.get("duration_ms")
        if not isinstance(duration_ms, int) or duration_ms < 0:
            continue
        total_duration_ms += duration_ms
        lane_phase = str(lane.get("lane_phase") or "downstream_lanes")
        phase_runtime_ms[lane_phase] = phase_runtime_ms.get(lane_phase, 0) + duration_ms
        lanes_with_runtime.append(
            {
                "lane_id": lane.get("lane_id"),
                "lane_phase": lane_phase,
                "duration_ms": duration_ms,
                "outcome": lane.get("outcome"),
                "setup_class": lane.get("setup_class"),
            }
        )
    return (
        total_duration_ms,
        dict(sorted(phase_runtime_ms.items(), key=lambda item: item[1], reverse=True)),
        sorted(lanes_with_runtime, key=lambda lane: lane["duration_ms"], reverse=True)[:10],
    )


def overall_conclusion(
    primary: list[dict], secondary: list[dict], downstream_result: str, args: argparse.Namespace
) -> str:
    terminal_results = {
        args.smoke_gate_result,
        downstream_result,
        args.artifact_result,
    }
    if primary or secondary or terminal_results & FAILED_OUTCOMES:
        return "failure"
    if parse_bool(args.run_artifact) and args.artifact_result == "success":
        return "success"
    if parse_bool(args.run_selected_lanes) and downstream_result == "success":
        return "success"
    if parse_bool(args.run_smoke_gate) and args.smoke_gate_result == "success":
        return "success"
    return "unknown"


def main() -> None:
    args = parse_args()
    planned_matrix_payload, planned_matrix_invalid = parse_json_argument(
        args.planned_matrix_json, {"include": []}
    )
    selected_lane_ids_payload, selected_lane_ids_invalid = parse_json_argument(
        args.selected_lane_ids_json, []
    )
    planned_matrix = (
        planned_matrix_payload.get("include", [])
        if isinstance(planned_matrix_payload, dict)
        else []
    )
    selected_lane_ids = (
        selected_lane_ids_payload if isinstance(selected_lane_ids_payload, list) else []
    )
    explicit_lanes = [lane.strip() for lane in args.explicit_lanes.split(",") if lane.strip()]

    summary_input_signals: list[str] = []
    if planned_matrix_invalid:
        summary_input_signals.append("planned_matrix_json was malformed")
    if selected_lane_ids_invalid:
        summary_input_signals.append("selected_lane_ids_json was malformed")

    actual_by_lane = load_lane_summaries(Path(args.lane_summary_dir))
    setup_class_results = {
        "light": args.light_result,
        "rust": args.rust_result,
        "heavy": args.heavy_result,
    }
    matrix_fail_fast = parse_bool(args.matrix_fail_fast)

    results = build_results(
        planned_matrix,
        selected_lane_ids,
        actual_by_lane,
        args.smoke_gate_result,
        setup_class_results,
        matrix_fail_fast=matrix_fail_fast,
    )
    setup_rows = setup_class_rows(results, setup_class_results)

    if summary_input_signals:
        primary = [
            {
                "kind": "planner",
                "lane_id": "validation-plan",
                "job_result": "failure",
                "signal": "; ".join(summary_input_signals),
            }
        ]
        secondary = []
    else:
        primary, secondary = derive_primary_and_secondary(results, setup_rows)

    primary = sorted(primary, key=blocker_sort_key)
    secondary = sorted(secondary, key=blocker_sort_key)
    queue = [*primary, *secondary]
    total_duration_ms, phase_runtime_ms, top_slowest_lanes = summarize_runtime(results)
    downstream_result = combined_result(args.light_result, args.rust_result, args.heavy_result)

    lane_count = len(results)
    successful_lane_count = sum(1 for lane in results if lane["outcome"] in SUCCESS_OUTCOMES)
    raw_failed_lane_count = sum(1 for lane in results if lane["outcome"] in BLOCKER_OUTCOMES)
    other_lane_count = lane_count - successful_lane_count - raw_failed_lane_count

    candidate_next_slices: list[dict] = []
    for item in queue[:20]:
        if item["kind"] == "setup_class":
            candidate_next_slices.append(
                {
                    "kind": "setup_class",
                    "lane_id": item["lane_id"],
                    "setup_class": item["setup_class"],
                    "signal": item["signal"],
                    "lane_ids": item["lane_ids"],
                }
            )
        elif item["kind"] == "planner":
            candidate_next_slices.append(
                {
                    "kind": "planner",
                    "lane_id": item["lane_id"],
                    "signal": item["signal"],
                }
            )
        else:
            candidate_next_slices.append(
                {
                    "kind": "lane",
                    "lane_id": item["lane_id"],
                    "summary_family": item.get("summary_family"),
                    "setup_class": item.get("setup_class"),
                    "signal": item.get("signal", ""),
                }
            )

    summary = {
        "lane_count": lane_count,
        "successful_lane_count": successful_lane_count,
        "failed_lane_count": blocked_finding_count(primary, secondary),
        "raw_failed_lane_count": raw_failed_lane_count,
        "other_lane_count": other_lane_count,
        "total_duration_ms": total_duration_ms,
        "phase_runtime_ms": phase_runtime_ms,
        "top_slowest_lanes": top_slowest_lanes,
        "first_failure": queue[0] if queue else None,
        "failed_lanes": [
            {
                "lane_id": lane["lane_id"],
                "outcome": lane["outcome"],
                "signal": lane_signal(lane),
            }
            for lane in results
            if lane["outcome"] in BLOCKER_OUTCOMES
        ],
        "setup_classes": setup_rows,
        "primary_blockers": primary,
        "secondary_findings": secondary,
        "candidate_next_slices": candidate_next_slices,
        "overall_conclusion": overall_conclusion(primary, secondary, downstream_result, args),
    }

    payload = {
        "repo": args.repo,
        "ref": {
            "display_ref": args.display_ref,
            "checkout_ref": args.checkout_ref,
            "head_sha": args.head_sha,
        },
        "selection": {
            "profile": args.profile,
            "profile_intent": args.profile_intent or "",
            "profile_notes": args.profile_notes or "",
            "lane_set": args.lane_set,
            "lane_summary": args.lane_summary or "",
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
            "light_lanes": {
                "planned": any(lane.get("setup_class") == "light" for lane in planned_matrix),
                "result": args.light_result,
            },
            "rust_lanes": {
                "planned": any(lane.get("setup_class") == "rust" for lane in planned_matrix),
                "result": args.rust_result,
            },
            "heavy_lanes": {
                "planned": any(lane.get("setup_class") == "heavy" for lane in planned_matrix),
                "result": args.heavy_result,
            },
            "downstream_lanes": {
                "planned": parse_bool(args.run_selected_lanes),
                "result": downstream_result,
            },
            "artifact": {
                "planned": parse_bool(args.run_artifact),
                "result": args.artifact_result,
            },
        },
        "lanes": results,
        "summary": summary,
    }

    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
