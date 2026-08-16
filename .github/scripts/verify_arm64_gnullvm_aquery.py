#!/usr/bin/env python3
"""Verify one Rust action's ARM64 Windows gnullvm toolchain inputs."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


ACTION_HEADER_RE = re.compile(r"^action '.*'\s*$", re.MULTILINE)
ARM64_GNULLVM_EXECUTION_PLATFORM = "//rs/platforms:aarch64-pc-windows-gnullvm"
ARM64_GNULLVM_TOOLCHAINS = (
    "rustc_windows_aarch64_gnullvm",
    "cargo_windows_aarch64_gnullvm",
)
ARM64_MSVC_TOOLCHAIN_RE = re.compile(r"(?:rustc|cargo)_windows_aarch64_msvc")


class AqueryValidationError(ValueError):
    """Raised when aquery does not prove the selected Rust action contract."""


def action_blocks(aquery_output: str) -> list[str]:
    """Return clearly delimited Bazel text-format action blocks."""

    headers = list(ACTION_HEADER_RE.finditer(aquery_output))
    blocks: list[str] = []
    for index, header in enumerate(headers):
        next_start = headers[index + 1].start() if index + 1 < len(headers) else None
        blocks.append(aquery_output[header.start() : next_start].rstrip())
    return blocks


def field_value(action_block: str, field_name: str) -> str | None:
    match = re.search(
        rf"^\s*{re.escape(field_name)}:\s*(?P<value>.+?)\s*$",
        action_block,
        re.MULTILINE,
    )
    return match["value"] if match else None


def selected_rust_action(aquery_output: str, target: str) -> str:
    candidates = [
        action_block
        for action_block in action_blocks(aquery_output)
        if field_value(action_block, "Mnemonic") == "Rustc"
        and field_value(action_block, "Target") == target
    ]
    if len(candidates) != 1:
        raise AqueryValidationError(
            f"expected exactly one Rustc action for {target}, found {len(candidates)}"
        )
    return candidates[0]


def verify_selected_rust_action(aquery_output: str, target: str) -> None:
    """Require one target Rustc block to prove the execution-toolchain contract."""

    action_block = selected_rust_action(aquery_output, target)
    execution_platform = field_value(action_block, "Execution platform")
    if (
        execution_platform is None
        or ARM64_GNULLVM_EXECUTION_PLATFORM not in execution_platform
    ):
        raise AqueryValidationError(
            f"Rustc action for {target} does not use the ARM64 gnullvm execution platform"
        )

    missing_toolchains = [
        repository
        for repository in ARM64_GNULLVM_TOOLCHAINS
        if repository not in action_block
    ]
    if missing_toolchains:
        raise AqueryValidationError(
            f"Rustc action for {target} is missing ARM64 gnullvm toolchain inputs: "
            + ", ".join(missing_toolchains)
        )

    if ARM64_MSVC_TOOLCHAIN_RE.search(action_block):
        raise AqueryValidationError(
            f"Rustc action for {target} must not use ARM64 MSVC toolchain inputs"
        )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "aquery_output",
        type=Path,
        help="Bazel aquery --output=text output to validate.",
    )
    parser.add_argument(
        "--target",
        default="//codex-rs/otel:otel",
        help="Exact Bazel target whose Rustc action must be validated.",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    try:
        verify_selected_rust_action(
            args.aquery_output.read_text(encoding="utf-8"),
            args.target,
        )
    except AqueryValidationError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1

    print(
        "Verified the selected Rustc action for "
        f"{args.target} uses ARM64 Windows gnullvm toolchain inputs."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
