#!/usr/bin/env python3
"""Resolve rust-ci execution mode for PR follow-ups and lightweight gating."""

from __future__ import annotations

import argparse
import fnmatch
import json
import subprocess
from pathlib import Path


HIGH_RISK_PATTERNS = [
    ".github/**",
    "justfile",
    "Cargo.lock",
    "Cargo.toml",
    "**/Cargo.toml",
    "rust-toolchain.toml",
    "MODULE.bazel",
    "MODULE.bazel.lock",
    "scripts/**",
    "tools/argument-comment-lint/**",
]

INITIAL_ROUTE_ACTIONS = {"opened", "reopened", "ready_for_review"}
INITIAL_ROUTE_MAX_FILES = 12
INITIAL_ROUTE_MAX_LINES = 400
FOLLOWUP_ROUTE_MAX_FILES = 4
FOLLOWUP_ROUTE_MAX_LINES = 80

RUST_BUNDLE_PATTERNS = [
    "codex-rs/**/*.rs",
    "codex-rs/**/build.rs",
    "codex-rs/**/Cargo.toml",
    "codex-rs/**/Cargo.lock",
    "codex-rs/**/BUILD.bazel",
    "Cargo.lock",
    "Cargo.toml",
    "**/Cargo.toml",
    "rust-toolchain.toml",
    "MODULE.bazel",
    "MODULE.bazel.lock",
]
WORKFLOW_SURFACE_PATTERNS = [
    ".github/**",
    "justfile",
    "scripts/**",
]


def catalog_path() -> Path:
    return Path(__file__).resolve().parent.parent / "validation-lanes.json"


def load_catalog() -> dict:
    return json.loads(catalog_path().read_text(encoding="utf-8"))


def git_output(repo_root: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(repo_root), *args],
        check=True,
        capture_output=True,
        text=True,
    )
    return proc.stdout


def git_ref_exists(repo_root: Path, ref: str) -> bool:
    if not ref or set(ref) == {"0"}:
        return False
    proc = subprocess.run(
        ["git", "-C", str(repo_root), "rev-parse", "--verify", f"{ref}^{{commit}}"],
        capture_output=True,
        text=True,
    )
    return proc.returncode == 0


def diff_files(repo_root: Path, base: str, head: str) -> list[str]:
    if not (git_ref_exists(repo_root, base) and git_ref_exists(repo_root, head)):
        return []
    output = git_output(repo_root, "diff", "--name-only", "--no-renames", base, head)
    return [line for line in output.splitlines() if line]


def diff_line_count(repo_root: Path, base: str, head: str) -> int:
    if not (git_ref_exists(repo_root, base) and git_ref_exists(repo_root, head)):
        return 0
    output = git_output(repo_root, "diff", "--numstat", "--no-renames", base, head)
    total = 0
    for line in output.splitlines():
        added, deleted, *_rest = line.split("\t", 2)
        if added == "-" or deleted == "-":
            return 10_000
        total += int(added) + int(deleted)
    return total


def parse_files_json(value: str) -> list[str] | None:
    if not value:
        return None
    try:
        payload = json.loads(value)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"invalid JSON input for changed-files: {exc}") from exc
    if not isinstance(payload, list) or not all(isinstance(item, str) for item in payload):
        raise SystemExit("changed-files JSON inputs must be arrays of strings")
    return payload


def explicit_line_count(value: str) -> int | None:
    if not value:
        return None
    try:
        parsed = int(value)
    except ValueError as exc:
        raise SystemExit("line-count inputs must be non-negative integers") from exc
    if parsed < 0:
        raise SystemExit("line-count inputs must be non-negative integers")
    return parsed


def changed_files(
    repo_root: Path,
    base: str,
    head: str,
    explicit_files: list[str] | None,
) -> list[str]:
    if explicit_files is not None:
        return explicit_files
    return diff_files(repo_root, base, head)


def changed_line_count(
    repo_root: Path,
    base: str,
    head: str,
    explicit_count: int | None,
) -> int:
    if explicit_count is not None:
        return explicit_count
    return diff_line_count(repo_root, base, head)


def path_matches(path: str, pattern: str) -> bool:
    return fnmatch.fnmatch(path, pattern)


def any_path_matches(paths: list[str], patterns: list[str]) -> bool:
    return any(path_matches(path, pattern) for path in paths for pattern in patterns)


def classify_files(files: list[str]) -> dict[str, bool]:
    codex = any(any(path_matches(path, pattern) for pattern in RUST_BUNDLE_PATTERNS) for path in files)
    argument_comment_lint = any(
        any(path_matches(path, pattern) for pattern in RUST_BUNDLE_PATTERNS)
        or path.startswith("tools/argument-comment-lint/")
        for path in files
    )
    argument_comment_lint_package = any(
        path.startswith("tools/argument-comment-lint/")
        or path == ".github/workflows/rust-ci.yml"
        or path == ".github/workflows/rust-ci-full.yml"
        for path in files
    )
    workflows = any(
        any(path_matches(path, pattern) for pattern in WORKFLOW_SURFACE_PATTERNS)
        for path in files
    )
    high_risk = any(any(path_matches(path, pattern) for pattern in HIGH_RISK_PATTERNS) for path in files)
    return {
        "codex": codex,
        "argument_comment_lint": argument_comment_lint,
        "argument_comment_lint_package": argument_comment_lint_package,
        "workflows": workflows,
        "high_risk": high_risk,
        "has_relevant_changes": codex or argument_comment_lint or argument_comment_lint_package or workflows or high_risk,
    }


def select_followup_lanes(files: list[str], routes: list[dict]) -> list[str]:
    if not files:
        return []

    matching_routes = []
    for route in routes:
        allowed_paths = route.get("allowed_paths", [])
        required_any_paths = route.get("required_any_paths", [])
        if not allowed_paths:
            continue
        if not all(any(path_matches(path, pattern) for pattern in allowed_paths) for path in files):
            continue
        if required_any_paths and not any(
            any(path_matches(path, pattern) for pattern in required_any_paths) for path in files
        ):
            continue
        matching_routes.append(route)

    if len(matching_routes) != 1:
        return []
    return matching_routes[0].get("lane_ids", [])


def route_lanes_are_light_workflow_only(lane_ids: list[str], catalog: dict) -> bool:
    if not lane_ids:
        return False
    catalog_by_id = {lane["lane_id"]: lane for lane in catalog.get("lanes", [])}
    for lane_id in lane_ids:
        lane = catalog_by_id.get(lane_id)
        if lane is None:
            return False
        groups = set(lane.get("groups", []))
        if not groups or not groups.issubset({"workflow", "docs"}):
            return False
    return True


def forced_full_outputs() -> dict[str, str]:
    return {
        "validation_mode": "full",
        "codex": "true",
        "argument_comment_lint": "true",
        "argument_comment_lint_package": "true",
        "workflows": "true",
        "has_relevant_changes": "true",
        "run_planner_fixtures": "true",
        "run_general": "true",
        "run_cargo_shear": "true",
        "run_argument_comment_lint_package": "true",
        "run_argument_comment_lint_prebuilt": "true",
        "run_incremental_validation": "false",
        "incremental_profile": "",
        "incremental_lane_set": "",
        "incremental_lanes": "",
    }


def as_output(value: bool) -> str:
    return "true" if value else "false"


def parse_output_bool(value: str) -> bool:
    return value.strip().lower() == "true"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", required=True)
    parser.add_argument("--event-name", required=True)
    parser.add_argument("--event-action", default="")
    parser.add_argument("--base-sha", default="")
    parser.add_argument("--head-sha", default="")
    parser.add_argument("--before-sha", default="")
    parser.add_argument("--previous-green-required", default="false")
    parser.add_argument("--previous-planner-fixtures", default="false")
    parser.add_argument("--previous-general", default="false")
    parser.add_argument("--previous-cargo-shear", default="false")
    parser.add_argument("--previous-argument-comment-lint-package", default="false")
    parser.add_argument("--previous-argument-comment-lint-prebuilt", default="false")
    parser.add_argument("--primary-files-json", default="")
    parser.add_argument("--primary-line-count", default="")
    parser.add_argument("--latest-delta-files-json", default="")
    parser.add_argument("--latest-delta-line-count", default="")
    args = parser.parse_args()

    if args.event_name in {"workflow_dispatch", "merge_group", "schedule"}:
        print(json.dumps(forced_full_outputs(), separators=(",", ":")))
        return

    repo_root = Path(args.repo_root)
    catalog = load_catalog()
    routes = catalog.get("followup_routes", [])
    primary_files_input = parse_files_json(args.primary_files_json)
    primary_line_count_input = explicit_line_count(args.primary_line_count)
    latest_delta_files_input = parse_files_json(args.latest_delta_files_json)
    latest_delta_line_count_input = explicit_line_count(args.latest_delta_line_count)

    if args.event_name == "pull_request":
        primary_files = changed_files(
            repo_root,
            args.base_sha,
            args.head_sha,
            primary_files_input,
        )
    else:
        primary_files = []

    primary = classify_files(primary_files)
    primary_lines = changed_line_count(
        repo_root,
        args.base_sha,
        args.head_sha,
        primary_line_count_input,
    )
    primary_lanes = select_followup_lanes(primary_files, routes)
    primary_light_workflow_route = route_lanes_are_light_workflow_only(primary_lanes, catalog)

    latest_delta_files = changed_files(
        repo_root,
        args.before_sha,
        args.head_sha,
        latest_delta_files_input,
    )
    latest_delta = classify_files(latest_delta_files)
    latest_delta_lines = changed_line_count(
        repo_root,
        args.before_sha,
        args.head_sha,
        latest_delta_line_count_input,
    )

    followup_lanes = select_followup_lanes(latest_delta_files, routes)
    followup_light_workflow_route = route_lanes_are_light_workflow_only(followup_lanes, catalog)
    mapped_followup = (
        args.event_name == "pull_request"
        and args.event_action == "synchronize"
        and bool(latest_delta_files)
        and len(latest_delta_files) <= FOLLOWUP_ROUTE_MAX_FILES
        and latest_delta_lines <= FOLLOWUP_ROUTE_MAX_LINES
        and (not latest_delta["high_risk"] or followup_light_workflow_route)
        and bool(followup_lanes)
    )
    light_initial = (
        args.event_name == "pull_request"
        and args.event_action in INITIAL_ROUTE_ACTIONS
        and bool(primary_files)
        and len(primary_files) <= INITIAL_ROUTE_MAX_FILES
        and primary_lines <= INITIAL_ROUTE_MAX_LINES
        and (not primary["high_risk"] or primary_light_workflow_route)
        and bool(primary_lanes)
    )
    light_followup = (
        mapped_followup
        and args.previous_green_required == "true"
    )
    reuse_followup = mapped_followup and not light_followup

    if light_followup:
        outputs = {
            "validation_mode": "light_followup",
            "codex": as_output(primary["codex"]),
            "argument_comment_lint": as_output(primary["argument_comment_lint"]),
            "argument_comment_lint_package": as_output(primary["argument_comment_lint_package"]),
            "workflows": as_output(primary["workflows"]),
            "has_relevant_changes": as_output(primary["has_relevant_changes"]),
            # Once a PR head is already green, tiny mapped follow-ups should
            # prove the exact seam instead of re-running the whole fast bundle.
            "run_planner_fixtures": as_output(latest_delta["workflows"]),
            "run_general": "false",
            "run_cargo_shear": "false",
            "run_argument_comment_lint_package": "false",
            "run_argument_comment_lint_prebuilt": "false",
            "run_incremental_validation": "true",
            "incremental_profile": "targeted",
            "incremental_lane_set": "all",
            "incremental_lanes": ",".join(followup_lanes),
        }
    elif light_initial:
        outputs = {
            "validation_mode": "light_initial",
            "codex": as_output(primary["codex"]),
            "argument_comment_lint": as_output(primary["argument_comment_lint"]),
            "argument_comment_lint_package": as_output(primary["argument_comment_lint_package"]),
            "workflows": as_output(primary["workflows"]),
            "has_relevant_changes": as_output(primary["has_relevant_changes"]),
            # For small initial PRs that map cleanly to one guarded seam, prove
            # the exact route first instead of broadening to the full fast bundle.
            "run_planner_fixtures": as_output(primary["workflows"]),
            "run_general": "false",
            "run_cargo_shear": "false",
            "run_argument_comment_lint_package": "false",
            "run_argument_comment_lint_prebuilt": "false",
            "run_incremental_validation": "true",
            "incremental_profile": "targeted",
            "incremental_lane_set": "all",
            "incremental_lanes": ",".join(primary_lanes),
        }
    elif reuse_followup:
        outputs = {
            "validation_mode": "evidence_followup",
            "codex": as_output(primary["codex"]),
            "argument_comment_lint": as_output(primary["argument_comment_lint"]),
            "argument_comment_lint_package": as_output(primary["argument_comment_lint_package"]),
            "workflows": as_output(primary["workflows"]),
            "has_relevant_changes": as_output(primary["has_relevant_changes"]),
            "run_planner_fixtures": as_output(
                latest_delta["workflows"]
                or (
                    primary["workflows"]
                    and not parse_output_bool(args.previous_planner_fixtures)
                )
            ),
            "run_general": as_output(
                latest_delta["codex"]
                or (primary["codex"] and not parse_output_bool(args.previous_general))
            ),
            "run_cargo_shear": as_output(
                latest_delta["codex"]
                or (primary["codex"] and not parse_output_bool(args.previous_cargo_shear))
            ),
            "run_argument_comment_lint_package": as_output(
                latest_delta["argument_comment_lint_package"]
                or (
                    primary["argument_comment_lint_package"]
                    and not parse_output_bool(args.previous_argument_comment_lint_package)
                )
            ),
            "run_argument_comment_lint_prebuilt": as_output(
                latest_delta["argument_comment_lint"]
                or (
                    primary["argument_comment_lint"]
                    and not parse_output_bool(args.previous_argument_comment_lint_prebuilt)
                )
            ),
            "run_incremental_validation": "true",
            "incremental_profile": "targeted",
            "incremental_lane_set": "all",
            "incremental_lanes": ",".join(followup_lanes),
        }
    else:
        outputs = {
            "validation_mode": "full",
            "codex": as_output(primary["codex"]),
            "argument_comment_lint": as_output(primary["argument_comment_lint"]),
            "argument_comment_lint_package": as_output(primary["argument_comment_lint_package"]),
            "workflows": as_output(primary["workflows"]),
            "has_relevant_changes": as_output(primary["has_relevant_changes"]),
            "run_planner_fixtures": as_output(primary["workflows"]),
            "run_general": as_output(primary["codex"]),
            "run_cargo_shear": as_output(primary["codex"]),
            "run_argument_comment_lint_package": as_output(primary["argument_comment_lint_package"]),
            "run_argument_comment_lint_prebuilt": as_output(primary["argument_comment_lint"]),
            "run_incremental_validation": "false",
            "incremental_profile": "",
            "incremental_lane_set": "",
            "incremental_lanes": "",
        }

    print(json.dumps(outputs, separators=(",", ":")))


if __name__ == "__main__":
    main()
