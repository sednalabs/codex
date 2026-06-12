#!/usr/bin/env python3
"""Run multiple validation lanes in one prepared Rust runner environment."""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


RETRY_RUSTY_V8_RE = re.compile(r"rusty_v8", re.IGNORECASE)
RETRY_HTTP_502_RE = re.compile(r"HTTP Error 502", re.IGNORECASE)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", required=True)
    parser.add_argument("--workflow-src", required=True)
    parser.add_argument("--setup-class", required=True)
    parser.add_argument("--batch-id", required=True)
    parser.add_argument("--lane-ids-json", required=True)
    parser.add_argument("--output-dir", required=True)
    return parser.parse_args()


def slugify(value: str) -> str:
    slug = re.sub(r"[^A-Za-z0-9]+", "-", value.lower()).strip("-")
    return slug[:64] or "validation-lane"


def load_catalog(workflow_src: Path) -> dict[str, dict[str, Any]]:
    planner_path = workflow_src / ".github" / "scripts" / "resolve_validation_plan.py"
    spec = importlib.util.spec_from_file_location("resolve_validation_plan_for_batch", planner_path)
    if spec is None or spec.loader is None:
        raise SystemExit(f"unable to load validation planner: {planner_path}")
    planner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(planner)
    catalog = planner.normalize_catalog(planner.load_catalog(workflow_src / ".github" / "validation-lanes.json"))
    planner.validate_catalog(catalog)
    return {str(lane.get("lane_id")): lane for lane in catalog["lanes"] if lane.get("lane_id")}


def lane_payload(catalog_by_id: dict[str, dict[str, Any]], lane_id: str) -> dict[str, Any]:
    lane = catalog_by_id.get(lane_id)
    if lane is None:
        raise SystemExit(f"unknown lane id in batch: {lane_id}")
    return {
        "lane_id": lane_id,
        "groups": lane.get("groups") or [],
        "status_class": lane["status_class"],
        "frontier_default": bool(lane.get("frontier_default", False)),
        "setup_class": lane["setup_class"],
        "frontier_role": lane["frontier_role"],
        "summary_family": lane["summary_family"],
        "cost_class": lane["cost_class"],
        "working_directory": lane["working_directory"],
        "script_path": lane["script_path"],
        "script_args": lane.get("script_args") or [],
        "batch_group": str(lane.get("batch_group") or "+".join(lane.get("groups") or []) or "default"),
        "batch_weight_seconds": int(lane.get("batch_weight_seconds") or 360),
    }


def stream_command(cmd: list[str], *, cwd: Path, log_path: Path) -> int:
    with log_path.open("ab") as log_file:
        proc = subprocess.Popen(
            cmd,
            cwd=str(cwd),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        assert proc.stdout is not None
        for chunk in iter(proc.stdout.readline, b""):
            if not chunk:
                break
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
            log_file.write(chunk)
            log_file.flush()
        return proc.wait()


def should_retry(log_path: Path) -> bool:
    text = log_path.read_text(encoding="utf-8", errors="replace")
    return bool(RETRY_RUSTY_V8_RE.search(text) and RETRY_HTTP_502_RE.search(text))


def clean_workspace(repo_root: Path) -> None:
    subprocess.run(["git", "reset", "--hard", "HEAD"], cwd=repo_root, check=True)
    subprocess.run(
        [
            "git",
            "clean",
            "-ffd",
            "-e",
            ".workflow-src/",
            "-e",
            "codex-rs/target/",
            "-e",
            ".sccache/",
        ],
        cwd=repo_root,
        check=True,
    )


def run_lane(repo_root: Path, workflow_src: Path, output_dir: Path, lane: dict[str, Any], index: int) -> dict[str, Any]:
    lane_id = lane["lane_id"]
    lane_slug = slugify(lane_id)
    log_path = output_dir / f"validation-lane-{index + 1:02d}-{lane_slug}.log"
    if index > 0:
        clean_workspace(repo_root)

    started_at_ms = int(time.time() * 1000)
    attempt = 1
    max_attempts = 2
    while True:
        if attempt > 1:
            with log_path.open("a", encoding="utf-8") as log:
                log.write(f"\n=== retry attempt {attempt}/{max_attempts} ===\n")
            print(f"::warning title=Retrying flaky rusty_v8 download::{lane_id} hit HTTP 502 while downloading rusty_v8; retrying once.")
        cmd = [
            "python3",
            str(workflow_src / ".github" / "scripts" / "run_validation_lane.py"),
            "--repo-root",
            str(repo_root),
            "--working-directory",
            str(lane["working_directory"]),
            "--script-path",
            str(lane["script_path"]),
            "--script-args-json",
            json.dumps(lane.get("script_args") or [], separators=(",", ":")),
        ]
        exit_code = stream_command(cmd, cwd=repo_root, log_path=log_path)
        if exit_code == 0:
            break
        if attempt >= max_attempts or not should_retry(log_path):
            break
        attempt += 1
        time.sleep(5)

    finished_at_ms = int(time.time() * 1000)
    outcome = "success" if exit_code == 0 else "failure"
    if exit_code != 0:
        print(f"::error title=Downstream lane failed::{lane_id} failed (exit {exit_code}). Public summary omits raw commands and log excerpts; inspect the job log for detailed runner context.")
    return {
        **lane,
        "outcome": outcome,
        "exit_code": exit_code,
        "log_file": str(log_path),
        "started_at_ms": started_at_ms,
        "finished_at_ms": finished_at_ms,
        "command_duration_ms": max(0, finished_at_ms - started_at_ms),
    }


def main() -> int:
    args = parse_args()
    repo_root = Path(args.repo_root).resolve()
    workflow_src = Path(args.workflow_src).resolve()
    output_dir = Path(args.output_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    catalog_by_id = load_catalog(workflow_src)
    lane_ids = json.loads(args.lane_ids_json or "[]")
    if not isinstance(lane_ids, list) or not all(isinstance(item, str) for item in lane_ids):
        raise SystemExit("lane ids must decode to a JSON array of strings")

    results = []
    for index, lane_id in enumerate(lane_ids):
        lane = lane_payload(catalog_by_id, lane_id)
        if lane["setup_class"] != args.setup_class:
            raise SystemExit(
                f"lane {lane_id} has setup_class {lane['setup_class']}, expected {args.setup_class}"
            )
        results.append(run_lane(repo_root, workflow_src, output_dir, lane, index))

    result_path = output_dir / "batch-results.json"
    result_path.write_text(json.dumps({"batch_id": args.batch_id, "results": results}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 1 if any(result["exit_code"] != 0 for result in results) else 0


if __name__ == "__main__":
    raise SystemExit(main())
