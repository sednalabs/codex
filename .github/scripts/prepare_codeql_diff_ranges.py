#!/usr/bin/env python3
"""Generate CodeQL's complete PR diff-range file from local Git history."""

from __future__ import annotations

import argparse
import ast
import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import PurePosixPath


HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")


def _decode_new_path(header: str) -> str | None:
    raw_path = header.removeprefix("+++ ")
    if raw_path == "/dev/null":
        return None
    if raw_path.startswith('"'):
        raw_path = ast.literal_eval(raw_path)
    if not raw_path.startswith("b/"):
        raise ValueError(f"unexpected Git new-path header: {header!r}")
    path = raw_path[2:]
    parsed = PurePosixPath(path)
    if parsed.is_absolute() or ".." in parsed.parts or not path:
        raise ValueError(f"unsafe Git path: {path!r}")
    return path


def parse_diff_ranges(diff: str) -> list[dict[str, int | str]]:
    ranges_by_path: dict[str, list[tuple[int, int]]] = defaultdict(list)
    current_path: str | None = None

    for line in diff.splitlines():
        if line.startswith("+++ "):
            current_path = _decode_new_path(line)
            continue
        if not line.startswith("@@ ") or current_path is None:
            continue
        match = HUNK_RE.match(line)
        if match is None:
            raise ValueError(f"unexpected Git hunk header: {line!r}")
        start_line = int(match.group(1))
        line_count = int(match.group(2) or "1")
        if line_count > 0:
            ranges_by_path[current_path].append(
                (start_line, start_line + line_count - 1)
            )

    result: list[dict[str, int | str]] = []
    for path in sorted(ranges_by_path):
        merged: list[list[int]] = []
        for start_line, end_line in sorted(ranges_by_path[path]):
            if merged and start_line <= merged[-1][1] + 1:
                merged[-1][1] = max(merged[-1][1], end_line)
            else:
                merged.append([start_line, end_line])
        result.extend(
            {"path": path, "startLine": start_line, "endLine": end_line}
            for start_line, end_line in merged
        )
    return result


def collect_diff_ranges(base: str, head: str) -> list[dict[str, int | str]]:
    proc = subprocess.run(
        [
            "git",
            "diff",
            "--unified=0",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            f"{base}..{head}",
            "--",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return parse_diff_ranges(proc.stdout)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", required=True)
    args = parser.parse_args()

    ranges = collect_diff_ranges(args.base, args.head)
    if not ranges:
        raise SystemExit("refusing to write an empty CodeQL PR diff-range file")
    json.dump(ranges, sys.stdout, indent=2)
    sys.stdout.write("\n")
    file_count = len({entry["path"] for entry in ranges})
    print(f"wrote {len(ranges)} ranges across {file_count} files", file=sys.stderr)


if __name__ == "__main__":
    main()
