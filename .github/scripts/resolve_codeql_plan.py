#!/usr/bin/env python3
"""Resolve CodeQL language selection for pull-request routing."""

from __future__ import annotations

import argparse
import fnmatch
import json
import subprocess
from pathlib import Path


LANGUAGES = [
    ("actions", "none"),
    ("c-cpp", "none"),
    ("javascript-typescript", "none"),
    ("python", "none"),
    ("rust", "none"),
]

FULL_SCAN_PATTERNS = [
    ".github/codeql/**",
    ".github/workflows/codeql.yml",
    ".github/scripts/resolve_codeql_plan.py",
    ".github/scripts/test_ci_planners.py",
]

LANGUAGE_PATTERNS = {
    "actions": [
        ".github/actions/**",
        ".github/workflows/**",
        "action.yml",
        "action.yaml",
        "**/action.yml",
        "**/action.yaml",
    ],
    "c-cpp": [
        "**/*.c",
        "**/*.cc",
        "**/*.cpp",
        "**/*.cxx",
        "**/*.h",
        "**/*.hh",
        "**/*.hpp",
        "**/*.hxx",
        "codex-rs/linux-sandbox/**",
        "codex-rs/seatbelt/**",
        "codex-rs/vendor/**",
    ],
    "javascript-typescript": [
        "**/*.js",
        "**/*.jsx",
        "**/*.mjs",
        "**/*.cjs",
        "**/*.ts",
        "**/*.tsx",
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "codex-cli/**",
        "codex-rs/app-server-protocol/schema/typescript/**",
        "codex-rs/responses-api-proxy/npm/**",
        "sdks/typescript/**",
        "shell-tool-mcp/**",
    ],
    "python": [
        "**/*.py",
        "pyproject.toml",
        "requirements*.txt",
        ".github/scripts/**",
        "scripts/**",
    ],
    "rust": [
        "**/*.rs",
        "**/Cargo.toml",
        "**/Cargo.lock",
        "**/BUILD.bazel",
        "Cargo.toml",
        "Cargo.lock",
        "MODULE.bazel",
        "MODULE.bazel.lock",
        "rust-toolchain",
        "rust-toolchain.toml",
        "codex-rs/**",
        "tools/argument-comment-lint/**",
    ],
}


def path_matches(path: str, pattern: str) -> bool:
    return fnmatch.fnmatch(path, pattern)


def any_path_matches(paths: list[str], patterns: list[str]) -> bool:
    return any(path_matches(path, pattern) for path in paths for pattern in patterns)


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


def git_ref_exists(repo_root: Path, ref: str) -> bool:
    if not ref or set(ref) == {"0"}:
        return False
    proc = subprocess.run(
        ["git", "-C", str(repo_root), "rev-parse", "--verify", f"{ref}^{{commit}}"],
        capture_output=True,
        text=True,
    )
    return proc.returncode == 0


def git_output(repo_root: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(repo_root), *args],
        check=True,
        capture_output=True,
        text=True,
    )
    return proc.stdout


def diff_files(repo_root: Path, base: str, head: str) -> list[str] | None:
    if not (git_ref_exists(repo_root, base) and git_ref_exists(repo_root, head)):
        return None
    output = git_output(repo_root, "diff", "--name-only", "--no-renames", base, head)
    return [line for line in output.splitlines() if line]


def matrix_for(languages: list[str]) -> dict[str, list[dict[str, str]]]:
    build_mode_by_language = dict(LANGUAGES)
    return {
        "include": [
            {"language": language, "build-mode": build_mode_by_language[language]}
            for language in languages
        ]
    }


def full_plan(reason: str) -> dict[str, str]:
    languages = [language for language, _build_mode in LANGUAGES]
    return {
        "matrix": json.dumps(matrix_for(languages), separators=(",", ":")),
        "languages": ",".join(languages),
        "has_codeql_relevant_changes": "true",
        "run_all_languages": "true",
        "reason": reason,
    }


def no_scan_plan(reason: str) -> dict[str, str]:
    return {
        "matrix": json.dumps({"include": []}, separators=(",", ":")),
        "languages": "",
        "has_codeql_relevant_changes": "false",
        "run_all_languages": "false",
        "reason": reason,
    }


def plan_for_files(files: list[str]) -> dict[str, str]:
    if not files:
        return no_scan_plan("no CodeQL-relevant changed files")

    if any_path_matches(files, FULL_SCAN_PATTERNS):
        return full_plan("CodeQL workflow or planner changed")

    selected = [
        language
        for language, _build_mode in LANGUAGES
        if any_path_matches(files, LANGUAGE_PATTERNS[language])
    ]
    if not selected:
        return no_scan_plan("no CodeQL-relevant changed files")

    return {
        "matrix": json.dumps(matrix_for(selected), separators=(",", ":")),
        "languages": ",".join(selected),
        "has_codeql_relevant_changes": "true",
        "run_all_languages": "false",
        "reason": f"matched changed paths for {','.join(selected)}",
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", required=True)
    parser.add_argument("--event-name", required=True)
    parser.add_argument("--base-sha", default="")
    parser.add_argument("--head-sha", default="")
    parser.add_argument("--changed-files-json", default="")
    parser.add_argument(
        "--allow-git-diff-fallback",
        action="store_true",
        help="Allow local git history to be used when trusted PR metadata is unavailable.",
    )
    args = parser.parse_args()

    if args.event_name != "pull_request":
        print(json.dumps(full_plan(f"{args.event_name} requires full CodeQL scan"), separators=(",", ":")))
        return

    explicit_files = parse_files_json(args.changed_files_json)
    files = explicit_files
    if files is None and args.allow_git_diff_fallback:
        files = diff_files(Path(args.repo_root), args.base_sha, args.head_sha)
    if files is None:
        print(
            json.dumps(
                full_plan("unable to determine changed files from trusted PR metadata"),
                separators=(",", ":"),
            )
        )
        return

    print(json.dumps(plan_for_files(files), separators=(",", ":")))


if __name__ == "__main__":
    main()
