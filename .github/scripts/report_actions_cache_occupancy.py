#!/usr/bin/env python3
"""Report GitHub Actions cache occupancy for a repository."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections import Counter
from pathlib import Path
from typing import Any


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--markdown-output", required=True)
    return parser.parse_args()


def cache_prefix(key: str) -> str:
    if "/" in key:
        return key.split("/", 1)[0]
    return key.split("-", 1)[0]


def summarize_caches(caches: list[dict[str, Any]]) -> dict[str, Any]:
    by_ref_count: Counter[str] = Counter()
    by_ref_bytes: Counter[str] = Counter()
    by_prefix_count: Counter[str] = Counter()
    by_prefix_bytes: Counter[str] = Counter()

    total_bytes = 0
    for cache in caches:
        key = str(cache.get("key") or "")
        ref = str(cache.get("ref") or "<unknown>")
        size = int(cache.get("size_in_bytes") or cache.get("sizeInBytes") or 0)
        prefix = cache_prefix(key) if key else "<unknown>"

        total_bytes += size
        by_ref_count[ref] += 1
        by_ref_bytes[ref] += size
        by_prefix_count[prefix] += 1
        by_prefix_bytes[prefix] += size

    def rows(counter: Counter[str], sizes: Counter[str]) -> list[dict[str, Any]]:
        return [
            {
                "name": name,
                "entries": counter[name],
                "size_bytes": sizes[name],
            }
            for name, _count in sorted(
                counter.items(), key=lambda item: (sizes[item[0]], item[1]), reverse=True
            )
        ]

    return {
        "available": True,
        "total_entries": len(caches),
        "total_size_bytes": total_bytes,
        "by_ref": rows(by_ref_count, by_ref_bytes),
        "by_prefix": rows(by_prefix_count, by_prefix_bytes),
    }


def collect_caches(repo: str) -> list[dict[str, Any]]:
    proc = subprocess.run(
        [
            "gh",
            "api",
            f"repos/{repo}/actions/caches",
            "--paginate",
            "--jq",
            ".actions_caches[]",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    caches = []
    for line in proc.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        caches.append(json.loads(line))
    return caches


def human_size(size_bytes: int) -> str:
    size = float(size_bytes)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if size < 1024 or unit == "TiB":
            return f"{size:.2f} {unit}" if unit != "B" else f"{int(size)} B"
        size /= 1024
    raise AssertionError("unreachable")


def markdown_table(title: str, rows: list[dict[str, Any]]) -> list[str]:
    lines = [f"#### {title}", "", "| Name | Entries | Size |", "| --- | ---: | ---: |"]
    for row in rows[:8]:
        lines.append(
            f"| `{row['name']}` | {row['entries']} | {human_size(int(row['size_bytes']))} |"
        )
    if not rows:
        lines.append("| none | 0 | 0 B |")
    return lines


def write_markdown(summary: dict[str, Any], output: Path) -> None:
    lines = ["### Actions cache occupancy", ""]
    if not summary.get("available", False):
        lines.append(f"- Cache occupancy unavailable: {summary.get('error', 'unknown error')}")
    else:
        lines.append(f"- Total entries: `{summary['total_entries']}`")
        lines.append(f"- Total size: `{human_size(int(summary['total_size_bytes']))}`")
        lines.append("")
        lines.extend(markdown_table("By key prefix", summary.get("by_prefix", [])))
        lines.append("")
        lines.extend(markdown_table("By ref", summary.get("by_ref", [])))
    output.write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    args = parse_args()
    output = Path(args.output)
    markdown_output = Path(args.markdown_output)
    output.parent.mkdir(parents=True, exist_ok=True)
    markdown_output.parent.mkdir(parents=True, exist_ok=True)

    try:
        summary = summarize_caches(collect_caches(args.repo))
    except Exception as exc:  # pragma: no cover - workflow telemetry must not gate validation
        summary = {"available": False, "error": str(exc)}

    output.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    write_markdown(summary, markdown_output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
