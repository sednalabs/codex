#!/usr/bin/env python3
"""Static policy checks for GitHub workflow files."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Any

import yaml


REPO_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW_PATTERNS = (
    ".github/workflows/*.yml",
    ".github/workflows/*.yaml",
    "codex-rs/.github/workflows/*.yml",
    "codex-rs/.github/workflows/*.yaml",
)


def workflow_paths(root: Path) -> list[Path]:
    paths: list[Path] = []
    for pattern in WORKFLOW_PATTERNS:
        paths.extend(root.glob(pattern))
    return sorted({path for path in paths if path.is_file()})


def load_workflow(path: Path) -> Any:
    return yaml.load(path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)


def walk_mappings(value: Any):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from walk_mappings(child)
    elif isinstance(value, list):
        for child in value:
            yield from walk_mappings(child)


def is_action_ref(uses: Any, action: str) -> bool:
    return isinstance(uses, str) and uses.startswith(f"{action}@")


def collect_violations(root: Path = REPO_ROOT) -> list[str]:
    violations: list[str] = []
    for path in workflow_paths(root):
        relative_path = path.relative_to(root)
        payload = load_workflow(path)
        for node in walk_mappings(payload):
            uses = node.get("uses")
            inputs = node.get("with") if isinstance(node.get("with"), dict) else {}

            if is_action_ref(uses, "actions/setup-node"):
                node_version_file = inputs.get("node-version-file")
                if isinstance(node_version_file, str):
                    version_path = root / node_version_file
                    if not version_path.exists():
                        violations.append(
                            f"{relative_path}: actions/setup-node references missing "
                            f"node-version-file '{node_version_file}'; use node-version "
                            "when the version is repository policy."
                        )

            if is_action_ref(uses, "taiki-e/install-action") and "version" in inputs:
                tool = inputs.get("tool", "<missing tool>")
                version = inputs["version"]
                violations.append(
                    f"{relative_path}: taiki-e/install-action does not support "
                    f"with.version; use tool: {tool}@{version} instead."
                )
    return violations


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=REPO_ROOT,
        help="Repository root to check.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    violations = collect_violations(args.repo_root.resolve())
    if violations:
        for violation in violations:
            print(violation, file=sys.stderr)
        return 1
    print("workflow-policy-ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
