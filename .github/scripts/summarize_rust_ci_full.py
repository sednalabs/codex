#!/usr/bin/env python3
"""Build compact summaries for the rust-ci-full workflow."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
NEXTEST_START_RE = re.compile(
    r"\bStarting\s+(?P<tests>\d+)\s+tests\s+across\s+(?P<binaries>\d+)\s+binaries"
    r"(?:\s+\((?P<skipped>\d+)\s+tests\s+skipped\))?"
)
NEXTEST_FAILURE_RE = re.compile(
    r"\b(?P<status>FAIL|TIMEOUT)\s+\[\s*(?P<duration>[^\]]+?)\s*\]\s+"
    r"\(\s*(?P<index>\d+)\s*/\s*(?P<total>\d+)\)\s+(?P<test>.+)$"
)
CLIPPY_ERROR_RE = re.compile(r"\berror(?:\[[^\]]+\])?:\s*(?P<message>.+)$")
CLIPPY_LOCATION_RE = re.compile(r"-->\s+(?P<location>[^:\s]+:\d+:\d+)")


def strip_ansi(value: str) -> str:
    return ANSI_RE.sub("", value)


def strip_log_prefix(line: str) -> str:
    """Remove GitHub log table prefixes when parsing fetched job logs."""

    line = strip_ansi(line).rstrip()
    if "\t" not in line:
        return line
    return line.rsplit("\t", 1)[-1]


def load_lines(path: Path) -> list[str]:
    if not path.exists():
        return []
    return [strip_log_prefix(line) for line in path.read_text(encoding="utf-8").splitlines()]


def nextest_summary(path: Path, suite: str) -> dict[str, Any]:
    lines = load_lines(path)
    started: dict[str, int] = {}
    failures: list[dict[str, str]] = []
    seen_tests: set[str] = set()
    status_counts: dict[str, int] = {}

    for line in lines:
        start_match = NEXTEST_START_RE.search(line)
        if start_match:
            started = {
                "tests": int(start_match.group("tests")),
                "binaries": int(start_match.group("binaries")),
                "skipped": int(start_match.group("skipped") or 0),
            }
            continue

        failure_match = NEXTEST_FAILURE_RE.search(line)
        if not failure_match:
            continue

        status = failure_match.group("status")
        status_counts[status] = status_counts.get(status, 0) + 1
        test_name = " ".join(failure_match.group("test").split())
        if test_name in seen_tests:
            continue
        seen_tests.add(test_name)
        failures.append(
            {
                "status": status.lower(),
                "duration": failure_match.group("duration").strip(),
                "test": test_name,
            }
        )

    return {
        "type": "nextest",
        "suite": suite,
        "log_missing": not path.exists(),
        "started": started,
        "failure_signal_count": sum(status_counts.values()),
        "unique_failure_count": len(failures),
        "status_counts": status_counts,
        "failures": failures[:200],
        "truncated": len(failures) > 200,
    }


def clippy_summary(path: Path, suite: str) -> dict[str, Any]:
    lines = load_lines(path)
    errors: list[dict[str, str]] = []

    for index, line in enumerate(lines):
        match = CLIPPY_ERROR_RE.search(line)
        if not match:
            continue
        message = match.group("message").strip()
        if message.startswith("could not compile "):
            continue
        location = ""
        for candidate in lines[index + 1 : index + 12]:
            location_match = CLIPPY_LOCATION_RE.search(candidate)
            if location_match:
                location = location_match.group("location")
                break
        errors.append({"message": message, "location": location})

    return {
        "type": "clippy",
        "suite": suite,
        "log_missing": not path.exists(),
        "error_count": len(errors),
        "errors": errors[:50],
        "truncated": len(errors) > 50,
    }


def load_json_files(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    files = [path] if path.is_file() else sorted(path.rglob("*.json"))
    payloads: list[dict[str, Any]] = []
    for file in files:
        try:
            payload = json.loads(file.read_text(encoding="utf-8"))
        except json.JSONDecodeError as error:
            payloads.append(
                {
                    "type": "invalid-json",
                    "path": str(file),
                    "error": str(error),
                }
            )
            continue
        if isinstance(payload, dict):
            payloads.append(payload)
    return payloads


def job_results(needs_json: str) -> dict[str, str]:
    if not needs_json:
        return {}
    needs = json.loads(needs_json)
    if not isinstance(needs, dict):
        return {}
    results: dict[str, str] = {}
    for job_name, job_payload in needs.items():
        if isinstance(job_payload, dict):
            result = job_payload.get("result")
            if isinstance(result, str):
                results[job_name] = result
    return results


def primary_blockers(jobs: dict[str, str], summaries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    blockers: list[dict[str, Any]] = []
    for job_name, result in jobs.items():
        if result not in {"success", "skipped"}:
            blockers.append({"type": "job", "job": job_name, "result": result})

    for summary in summaries:
        summary_type = summary.get("type")
        if summary_type == "clippy" and summary.get("errors"):
            first_error = summary["errors"][0]
            blockers.append(
                {
                    "type": "clippy",
                    "suite": summary.get("suite"),
                    "message": first_error.get("message"),
                    "location": first_error.get("location"),
                }
            )
        elif summary_type == "nextest" and summary.get("failures"):
            first_failure = summary["failures"][0]
            blockers.append(
                {
                    "type": "nextest",
                    "suite": summary.get("suite"),
                    "status": first_failure.get("status"),
                    "test": first_failure.get("test"),
                    "unique_failure_count": summary.get("unique_failure_count"),
                }
            )
    return blockers


def aggregate_summary(
    *,
    needs_json: str,
    summary_dir: Path,
    checkout_ref: str,
    source_event: str,
    output: Path,
) -> None:
    summaries = load_json_files(summary_dir)
    jobs = job_results(needs_json)
    payload = {
        "schema_version": 1,
        "workflow": "rust-ci-full",
        "checkout_ref": checkout_ref,
        "source_event": source_event,
        "jobs": jobs,
        "summaries": summaries,
        "primary_blockers": primary_blockers(jobs, summaries),
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def write_summary(summary: dict[str, Any], output: Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    nextest = subparsers.add_parser("extract-nextest")
    nextest.add_argument("--log", required=True, type=Path)
    nextest.add_argument("--suite", required=True)
    nextest.add_argument("--output", required=True, type=Path)

    clippy = subparsers.add_parser("extract-clippy")
    clippy.add_argument("--log", required=True, type=Path)
    clippy.add_argument("--suite", required=True)
    clippy.add_argument("--output", required=True, type=Path)

    aggregate = subparsers.add_parser("aggregate")
    aggregate.add_argument("--needs-json", default="")
    aggregate.add_argument("--summary-dir", required=True, type=Path)
    aggregate.add_argument("--checkout-ref", default="")
    aggregate.add_argument("--source-event", default="")
    aggregate.add_argument("--output", required=True, type=Path)

    args = parser.parse_args()
    if args.command == "extract-nextest":
        write_summary(nextest_summary(args.log, args.suite), args.output)
    elif args.command == "extract-clippy":
        write_summary(clippy_summary(args.log, args.suite), args.output)
    elif args.command == "aggregate":
        aggregate_summary(
            needs_json=args.needs_json,
            summary_dir=args.summary_dir,
            checkout_ref=args.checkout_ref,
            source_event=args.source_event,
            output=args.output,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
