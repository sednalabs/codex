#!/usr/bin/env python3
"""Write a compact per-lane validation summary artifact."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path

ERROR_RE = re.compile(r"(^error:|^thread '.*' panicked|\bFAILED\b|failures:|error\[|panic\b)")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--lane-id", required=True)
    parser.add_argument("--lane-phase", default="downstream_lanes")
    parser.add_argument("--summary-title", required=True)
    parser.add_argument("--run-command", required=True)
    parser.add_argument("--outcome", default="unknown")
    parser.add_argument("--exit-code", default="")
    parser.add_argument("--log-file", default="")
    parser.add_argument("--started-at-ms", default="")
    parser.add_argument("--finished-at-ms", default="")
    parser.add_argument("--duration-ms", default="")
    parser.add_argument("--artifact-name", default="")
    parser.add_argument("--output", required=True)
    return parser.parse_args()


def read_lines(path: Path) -> list[str]:
    if not path.is_file():
        return []
    return path.read_text(encoding="utf-8", errors="replace").splitlines()


def primary_signal(error_lines: list[str], tail_lines: list[str]) -> str:
    if error_lines:
        return error_lines[0].strip()
    for line in reversed(tail_lines):
        stripped = line.strip()
        if stripped:
            return stripped
    return ""


def parse_exit_code(raw: str) -> int | None:
    value = raw.strip()
    if not value:
        return None
    try:
        return int(value)
    except ValueError:
        return None


def parse_u64(raw: str) -> int | None:
    value = raw.strip()
    if not value:
        return None
    try:
        parsed = int(value)
    except ValueError:
        return None
    if parsed < 0:
        return None
    return parsed


def main() -> None:
    args = parse_args()
    log_path = Path(args.log_file) if args.log_file else None
    lines = read_lines(log_path) if log_path is not None else []
    error_lines = [line for line in lines if ERROR_RE.search(line)][:20]
    tail_lines = lines[-80:]

    payload = {
        "lane_id": args.lane_id,
        "lane_phase": args.lane_phase or "downstream_lanes",
        "summary_title": args.summary_title,
        "run_command": args.run_command,
        "outcome": args.outcome or "unknown",
        "exit_code": parse_exit_code(args.exit_code),
        "started_at_ms": parse_u64(args.started_at_ms),
        "finished_at_ms": parse_u64(args.finished_at_ms),
        "duration_ms": parse_u64(args.duration_ms),
        "log_available": bool(lines),
        "primary_signal": primary_signal(error_lines, tail_lines),
        "error_lines": error_lines,
        "tail_excerpt": tail_lines,
        "artifact_name": args.artifact_name or "",
    }

    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
