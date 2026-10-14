#!/usr/bin/env python3
"""Classify changed repository paths into CI scopes.

The classifier is deliberately conservative: routing or classifier changes expand to
full blocking CI, and unknown source-like files still select the formatter lane.
The CLI can classify explicit paths or a git diff and emits one JSON object.
"""

from __future__ import annotations

import argparse
import json
import subprocess
from dataclasses import asdict, dataclass
from pathlib import PurePosixPath
from typing import Iterable

SOURCE_FORMAT_SUFFIXES = {
    ".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp", ".hxx",
    ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx",
    ".py", ".rs", ".toml", ".json", ".yaml", ".yml",
}

FULL_BLOCKING_PATHS = {
    ".github/workflows/blocking-ci.yml",
    ".github/scripts/classify_ci_paths.py",
    ".github/scripts/test_classify_ci_paths.py",
}

CLIPPY_REQUIRED_PATHS = {
    ".github/scripts/classify_ci_paths.py",
    ".github/scripts/test_classify_ci_paths.py",
    ".github/workflows/blocking-ci.yml",
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "MODULE.bazel",
    "MODULE.bazel.lock",
}

CLIPPY_REQUIRED_PREFIXES = (
    ".github/actions/",
    ".github/workflows/",
    ".cargo/",
    "codex-rs/",
    "vendor/",
)

PACKAGE_ROOT_PATHS = {
    "package.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
}

BAZEL_ROOT_PATHS = {
    ".bazelrc",
    "MODULE.bazel",
    "MODULE.bazel.lock",
    "BUILD.bazel",
}

NATIVE_VISUAL_RESPONSE_CONTRACT_PATHS = {
    ".github/scripts/verify_native_visual_response_contract.py",
    ".github/scripts/test_verify_native_visual_response_contract.py",
    "codex-rs/android-computer-use/src/lib.rs",
    "codex-rs/browser-computer-use/src/lib.rs",
}

NATIVE_VISUAL_RESPONSE_CONTRACT_PREFIXES = (
    ".github/codeql/rust-computer-use-contract/",
)


@dataclass(frozen=True)
class Scope:
    clippy: bool
    cargo_deny: bool
    repo_policy: bool
    repo_package: bool
    repo_format: bool
    repo_readme: bool
    sdk_python: bool
    sdk_typescript: bool
    force_full_blocking: bool

    def to_jsonable(self) -> dict[str, object]:
        return asdict(self)


def _norm(path: str) -> str:
    normalized = path.replace("\\", "/")
    while normalized.startswith("./"):
        normalized = normalized[2:]
    return normalized


def _starts(path: str, *prefixes: str) -> bool:
    return any(path.startswith(prefix) for prefix in prefixes)


def classify(paths: Iterable[str]) -> Scope:
    changed = tuple(sorted({_norm(path) for path in paths if _norm(path)}))

    if not changed:
        return Scope(
            clippy=True,
            cargo_deny=True,
            repo_policy=True,
            repo_package=True,
            repo_format=True,
            repo_readme=True,
            sdk_python=True,
            sdk_typescript=True,
            force_full_blocking=True,
        )
    force_full_blocking = any(path in FULL_BLOCKING_PATHS for path in changed)
    clippy = any(
        path in CLIPPY_REQUIRED_PATHS
        or _starts(path, *CLIPPY_REQUIRED_PREFIXES)
        for path in changed
    )

    cargo_deny = force_full_blocking
    repo_policy = force_full_blocking
    repo_package = force_full_blocking
    repo_format = force_full_blocking
    repo_readme = force_full_blocking
    sdk_python = force_full_blocking
    sdk_typescript = force_full_blocking

    for path in changed:
        posix_path = PurePosixPath(path)
        suffix = posix_path.suffix.lower()
        name = posix_path.name

        if path == "README.md" or path in {
            "scripts/asciicheck.py",
            "scripts/readme_toc.py",
            ".github/workflows/repo-checks.yml",
        }:
            repo_readme = True

        if (
            path == ".github/workflows/repo-checks.yml"
            or path in {
                ".github/scripts/verify_cargo_workspace_manifests.py",
                ".github/scripts/verify_tui_core_boundary.py",
                ".github/scripts/verify_bazel_clippy_lints.py",
                ".bazelrc",
                "MODULE.bazel",
                "MODULE.bazel.lock",
            }
            or path in NATIVE_VISUAL_RESPONSE_CONTRACT_PATHS
            or _starts(path, *NATIVE_VISUAL_RESPONSE_CONTRACT_PREFIXES)
            or (_starts(path, "codex-rs/") and path.endswith("/Cargo.toml"))
            or path == "codex-rs/Cargo.toml"
            or (_starts(path, "codex-rs/tui/") and suffix == ".rs")
        ):
            repo_policy = True

        if (
            path == ".github/workflows/repo-checks.yml"
            or path in PACKAGE_ROOT_PATHS
            or path == "scripts/stage_npm_packages.py"
            or _starts(path, "scripts/codex_package/", "scripts/install/", "codex-cli/")
        ):
            repo_package = True

        if (
            path == ".github/workflows/repo-checks.yml"
            or path in PACKAGE_ROOT_PATHS
            or path in {"justfile", ".prettierignore", ".prettierrc", ".prettierrc.json"}
            or suffix in SOURCE_FORMAT_SUFFIXES
        ):
            # Markdown is intentionally excluded here. Documentation has its own
            # cheap checks; installing the whole JS/Rust formatting toolchain for
            # prose-only changes is the waste this classifier is meant to avoid.
            if suffix != ".md" or path == ".github/workflows/repo-checks.yml":
                repo_format = True

        if (
            path == ".github/workflows/cargo-deny.yml"
            or _starts(path, ".github/actions/setup-ci/")
            or path == "codex-rs/Cargo.lock"
            or path == "codex-rs/deny.toml"
            or (_starts(path, "codex-rs/") and path.endswith("/Cargo.toml"))
            or path == "codex-rs/Cargo.toml"
            or _starts(path, "codex-rs/.cargo/")
        ):
            cargo_deny = True

        if (
            path == ".github/workflows/sdk.yml"
            or _starts(path, "sdk/python/")
            or _starts(path, "codex-rs/app-server-protocol/", "codex-rs/protocol/")
        ):
            sdk_python = True

        if (
            path == ".github/workflows/sdk.yml"
            or path in PACKAGE_ROOT_PATHS
            or path in BAZEL_ROOT_PATHS
            or _starts(path, "sdk/typescript/", "codex-rs/")
            or _starts(path, ".github/actions/setup-bazel-ci/")
            or path == ".github/scripts/run-bazel-ci.sh"
        ):
            sdk_typescript = True

    return Scope(
        clippy=clippy,
        cargo_deny=cargo_deny,
        repo_policy=repo_policy,
        repo_package=repo_package,
        repo_format=repo_format,
        repo_readme=repo_readme,
        sdk_python=sdk_python,
        sdk_typescript=sdk_typescript,
        force_full_blocking=force_full_blocking,
    )


def changed_paths(base_sha: str, head_sha: str) -> list[str]:
    proc = subprocess.run(
        ["git", "diff", "--name-status", "-z", "--find-renames", base_sha, head_sha],
        check=True,
        stdout=subprocess.PIPE,
    )
    fields = proc.stdout.decode("utf-8", errors="surrogateescape").split("\0")
    if fields and fields[-1] == "":
        fields.pop()

    paths: list[str] = []
    index = 0
    while index < len(fields):
        status = fields[index]
        index += 1
        if not status:
            raise ValueError("empty git diff status")
        kind = status[0]
        if kind in {"R", "C"}:
            if index + 1 >= len(fields):
                raise ValueError(f"truncated git diff record for {status}")
            paths.extend((fields[index], fields[index + 1]))
            index += 2
        else:
            if index >= len(fields):
                raise ValueError(f"truncated git diff record for {status}")
            paths.append(fields[index])
            index += 1
    return paths


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("paths", nargs="*")
    parser.add_argument("--base-sha")
    parser.add_argument("--head-sha")
    args = parser.parse_args()

    if bool(args.base_sha) != bool(args.head_sha):
        parser.error("--base-sha and --head-sha must be provided together")
    if args.base_sha and args.paths:
        parser.error("explicit paths cannot be combined with --base-sha/--head-sha")

    paths = changed_paths(args.base_sha, args.head_sha) if args.base_sha else args.paths
    print(json.dumps(classify(paths).to_jsonable(), separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
