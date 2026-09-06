#!/usr/bin/env python3
"""Focused contract checks for the installer lower-bound argument."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "install_sedna_release_asset"
VERSION = re.compile(
    r"^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*))?-sedna\.(\d+)"
    r"(?:\+upstream\.\d+)?$"
)


def parsed(value: str) -> tuple[tuple[int, int, int], tuple[str, ...], int] | None:
    match = VERSION.fullmatch(value.removeprefix("v"))
    if match is None:
        return None
    return (
        tuple(int(part) for part in match.group(1, 2, 3)),
        tuple(match.group(4).split(".")) if match.group(4) else (),
        int(match.group(5)),
    )


def newer(candidate: str, bound: str) -> bool:
    left, right = parsed(candidate), parsed(bound)
    assert left is not None and right is not None
    if left[0] != right[0]:
        return left[0] > right[0]
    if left[1] != right[1]:
        return not left[1] if right[1] else False
    return left[2] > right[2]


def main() -> None:
    source = SCRIPT.read_text()
    assert "--require-newer-than" in source
    assert "automatic update candidate" in source
    assert "not newer than" in source
    help_result = subprocess.run(
        ["bash", str(SCRIPT), "--help"], capture_output=True, text=True, check=True
    )
    assert "--require-newer-than VERSION" in help_result.stderr

    cases = [
        ("1.2.3-sedna.3", "1.2.3-sedna.3", False),
        ("1.2.3-sedna.2", "1.2.3-sedna.3", False),
        ("1.2.3-sedna.4", "1.2.3-sedna.3", True),
        ("1.2.3-alpha.1-sedna.4", "1.2.3-sedna.3", False),
    ]
    for candidate, bound, expected in cases:
        assert newer(candidate, bound) is expected, (candidate, bound)
    assert parsed("not-a-sedna-release") is None
    assert parsed("1.2.3-sedna.x") is None
    assert "x86_64-unknown-linux-gnu" in source
    assert "aarch64-unknown-linux-gnu" in source
    assert "x86_64-apple-darwin" in source


if __name__ == "__main__":
    main()
