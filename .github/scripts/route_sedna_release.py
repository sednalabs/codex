#!/usr/bin/env python3
"""Route automatic Sedna release requests from a GitHub push event."""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

from resolve_sedna_release_version import ReleaseVersionError, release_marker_channel


def route_event(event: dict[str, object]) -> dict[str, str]:
    ref = event.get("ref")
    if ref != "refs/heads/main":
        return {
            "release_requested": "false",
            "reason": "unsupported_ref",
            "target_sha": "",
            "channel": "",
        }

    after = str(event.get("after") or "")
    if not after or set(after) == {"0"}:
        return {
            "release_requested": "false",
            "reason": "deleted_ref",
            "target_sha": "",
            "channel": "",
        }

    head_commit = event.get("head_commit")
    message = ""
    if isinstance(head_commit, dict):
        message = str(head_commit.get("message") or "")

    channel = release_marker_channel(message)
    if channel is None:
        return {
            "release_requested": "false",
            "reason": "missing_sedna_release_marker",
            "target_sha": after,
            "channel": "",
        }

    return {
        "release_requested": "true",
        "reason": "release_marker",
        "target_sha": after,
        "channel": channel,
    }


def write_outputs(path: Path, outputs: dict[str, str]) -> None:
    with path.open("a", encoding="utf-8") as handle:
        for key, value in outputs.items():
            handle.write(f"{key}={value}\n")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--event-path",
        type=Path,
        default=Path(os.environ["GITHUB_EVENT_PATH"])
        if "GITHUB_EVENT_PATH" in os.environ
        else None,
        help="Path to the GitHub event JSON payload.",
    )
    parser.add_argument(
        "--output-file",
        type=Path,
        default=Path(os.environ["GITHUB_OUTPUT"]) if "GITHUB_OUTPUT" in os.environ else None,
        help="Optional GitHub Actions output file.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if args.event_path is None:
        raise SystemExit("--event-path is required when GITHUB_EVENT_PATH is unset")

    event = json.loads(args.event_path.read_text(encoding="utf-8"))
    try:
        outputs = route_event(event)
    except ReleaseVersionError as exc:
        print(str(exc), file=sys.stderr)
        return 1

    if args.output_file is not None:
        write_outputs(args.output_file, outputs)
    print(json.dumps(outputs, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
