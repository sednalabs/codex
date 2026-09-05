#!/usr/bin/env python3
"""Resolve whether a Bazel CI invocation can use the docs-only path.

The caller supplies an exact, complete comparison result.  This script treats
any uncertainty as a request for the full Bazel suite: a truncated, malformed,
or empty file list is never sufficient evidence to skip executable checks.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def is_docs_only_path(path: str) -> bool:
    return path == "README.md" or path.startswith("docs/")


OBSERVER_ONLY_PATHS = frozenset(
    {
        ".github/scripts/validation-lanes/agent-workflow-sanity.sh",
        ".github/scripts/validation-lanes/workflow-security-targeted.sh",
    }
)


def resolve_bazel_ci_mode(
    *, comparison_complete: bool, files: Any, statuses: Any = None
) -> dict[str, str]:
    """Return GitHub-output-compatible mode values for the supplied comparison."""
    aligned_statuses = (
        comparison_complete
        and isinstance(files, list)
        and isinstance(statuses, list)
        and bool(files)
        and len(files) == len(statuses)
        and all(isinstance(path, str) and isinstance(status, str) for path, status in zip(files, statuses))
    )
    if not aligned_statuses:
        return {"mode": "full", "run_bazel": "true", "run_observer": "false"}

    allowed_statuses = all(status in {"A", "M"} for status in statuses)
    files_are_docs_only = (
        allowed_statuses and all(is_docs_only_path(path) for path in files)
    )
    if files_are_docs_only:
        return {"mode": "docs_only", "run_bazel": "false", "run_observer": "false"}
    files_are_observer_only = allowed_statuses and all(path in OBSERVER_ONLY_PATHS for path in files)
    if files_are_observer_only:
        return {"mode": "observer_only", "run_bazel": "false", "run_observer": "true"}
    return {"mode": "full", "run_bazel": "true", "run_observer": "false"}


def parse_boolean(value: str) -> bool:
    return value.strip().lower() == "true"


def write_github_output(path: Path, outputs: dict[str, str]) -> None:
    with path.open("a", encoding="utf-8") as output_file:
        for key, value in outputs.items():
            output_file.write(f"{key}={value}\n")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--comparison-complete", required=True)
    parser.add_argument("--files-json", required=True)
    parser.add_argument("--statuses-json", required=True)
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()

    try:
        files = json.loads(args.files_json)
    except json.JSONDecodeError:
        files = None
    try:
        statuses = json.loads(args.statuses_json)
    except json.JSONDecodeError:
        statuses = None

    outputs = resolve_bazel_ci_mode(
        comparison_complete=parse_boolean(args.comparison_complete),
        files=files,
        statuses=statuses,
    )
    if args.github_output is not None:
        write_github_output(args.github_output, outputs)
    print(json.dumps(outputs, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
