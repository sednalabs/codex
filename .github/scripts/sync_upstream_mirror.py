#!/usr/bin/env python3
"""Prepare or update the downstream upstream-main mirror for CI audits."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any


class MirrorSyncError(RuntimeError):
    pass


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=".")
    parser.add_argument(
        "--mode",
        choices=("required-write", "read-only-fallback"),
        required=True,
        help=(
            "required-write updates the public mirror and fails without a token; "
            "read-only-fallback audits live upstream without writing when the mirror is stale."
        ),
    )
    parser.add_argument("--mirror-remote", default="origin")
    parser.add_argument("--mirror-branch", default="upstream-main")
    parser.add_argument("--upstream-remote", default="upstream")
    parser.add_argument("--upstream-branch", default="main")
    parser.add_argument("--upstream-url", default="https://github.com/openai/codex.git")
    parser.add_argument(
        "--github-repository",
        default=os.environ.get("GITHUB_REPOSITORY", "sednalabs/codex"),
    )
    parser.add_argument("--token-env", default="SYNC_UPSTREAM_PUSH_TOKEN")
    parser.add_argument("--github-output", help="Optional GITHUB_OUTPUT path.")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo = Path(args.repo).resolve()
    token = os.environ.get(args.token_env, "")
    result = sync_upstream_mirror(
        repo=repo,
        mode=args.mode,
        mirror_remote=args.mirror_remote,
        mirror_branch=args.mirror_branch,
        upstream_remote=args.upstream_remote,
        upstream_branch=args.upstream_branch,
        upstream_url=args.upstream_url,
        github_repository=args.github_repository,
        token=token,
    )
    if args.github_output:
        write_github_output(Path(args.github_output), result)
    print(json.dumps(result, sort_keys=True))
    return 0


def sync_upstream_mirror(
    *,
    repo: Path,
    mode: str,
    mirror_remote: str = "origin",
    mirror_branch: str = "upstream-main",
    upstream_remote: str = "upstream",
    upstream_branch: str = "main",
    upstream_url: str = "https://github.com/openai/codex.git",
    github_repository: str = "sednalabs/codex",
    token: str = "",
    mirror_push_url: str | None = None,
) -> dict[str, Any]:
    repo = resolve_repo_root(repo)
    if mode == "required-write" and not token:
        raise MirrorSyncError(
            "missing upstream sync token for required-write mirror mode"
        )

    configure_remote(repo, upstream_remote, upstream_url)
    fetch_ref(repo, upstream_remote, upstream_branch, allow_failure=False)
    fetch_ref(repo, mirror_remote, mirror_branch, allow_failure=True)

    upstream_ref = f"refs/remotes/{upstream_remote}/{upstream_branch}"
    mirror_ref = f"refs/remotes/{mirror_remote}/{mirror_branch}"
    upstream_sha = rev_parse(repo, upstream_ref)
    mirror_exists = show_ref(repo, mirror_ref)
    mirror_sha = rev_parse(repo, mirror_ref) if mirror_exists else None
    mirror_state = classify_mirror(repo, mirror_ref, upstream_ref, mirror_sha, upstream_sha)

    wrote_mirror = False
    audit_baseline = "origin-mirror"
    mirror_audit_args = ["--mirror-remote", mirror_remote, "--mirror-branch", mirror_branch]

    if mirror_state in {"missing", "stale_ff_only"}:
        if mode == "required-write":
            push_url = mirror_push_url or authenticated_origin_url(github_repository, token)
            run_git(repo, ["remote", "set-url", mirror_remote, push_url])
            run_git(
                repo,
                ["push", mirror_remote, f"{upstream_ref}:refs/heads/{mirror_branch}"],
            )
            fetch_ref(repo, mirror_remote, mirror_branch, allow_failure=False)
            mirror_sha = rev_parse(repo, mirror_ref)
            mirror_state = classify_mirror(
                repo, mirror_ref, upstream_ref, mirror_sha, upstream_sha
            )
            wrote_mirror = True
        else:
            print(
                (
                    f"{mirror_remote}/{mirror_branch} is {mirror_state}; "
                    f"auditing against read-only {upstream_ref}"
                ),
                file=sys.stderr,
            )
            audit_baseline = "upstream-ref"
            mirror_audit_args = ["--mirror-ref", upstream_ref]

    if mirror_state not in {"exact", "missing", "stale_ff_only"}:
        raise MirrorSyncError(
            f"{mirror_remote}/{mirror_branch} is not a fast-forward-only mirror of "
            f"{upstream_remote}/{upstream_branch}: {mirror_state}"
        )
    if mode == "required-write" and mirror_state != "exact":
        raise MirrorSyncError(
            f"{mirror_remote}/{mirror_branch} did not match {upstream_remote}/{upstream_branch} "
            f"after sync: {mirror_state}"
        )

    return {
        "audit_baseline": audit_baseline,
        "expected_mirror_sha": upstream_sha,
        "mirror_audit_args": mirror_audit_args,
        "mirror_branch": mirror_branch,
        "mirror_ref": mirror_ref,
        "mirror_remote": mirror_remote,
        "mirror_sha": mirror_sha,
        "mirror_state": mirror_state,
        "mode": mode,
        "synced_upstream_main_sha": upstream_sha,
        "upstream_branch": upstream_branch,
        "upstream_ref": upstream_ref,
        "upstream_remote": upstream_remote,
        "upstream_sha": upstream_sha,
        "wrote_mirror": wrote_mirror,
    }


def resolve_repo_root(repo: Path) -> Path:
    result = run_git(repo, ["rev-parse", "--show-toplevel"], capture_stdout=True)
    return Path(result.stdout.strip())


def configure_remote(repo: Path, remote: str, url: str) -> None:
    result = run_git(
        repo,
        ["remote", "get-url", remote],
        capture_stdout=True,
        allow_failure=True,
    )
    if result.returncode == 0:
        run_git(repo, ["remote", "set-url", remote, url])
    else:
        run_git(repo, ["remote", "add", remote, url])


def fetch_ref(repo: Path, remote: str, branch: str, *, allow_failure: bool) -> None:
    run_git(
        repo,
        ["fetch", "--no-tags", "--prune", remote, branch],
        capture_stdout=True,
        allow_failure=allow_failure,
    )


def classify_mirror(
    repo: Path,
    mirror_ref: str,
    upstream_ref: str,
    mirror_sha: str | None,
    upstream_sha: str,
) -> str:
    if mirror_sha is None:
        return "missing"
    if mirror_sha == upstream_sha:
        return "exact"
    if is_ancestor(repo, mirror_ref, upstream_ref):
        return "stale_ff_only"
    if is_ancestor(repo, upstream_ref, mirror_ref):
        return "illegal_ahead"
    return "illegal_diverged"


def show_ref(repo: Path, ref: str) -> bool:
    result = run_git(
        repo,
        ["show-ref", "--verify", "--quiet", ref],
        capture_stdout=True,
        allow_failure=True,
    )
    return result.returncode == 0


def is_ancestor(repo: Path, older_ref: str, newer_ref: str) -> bool:
    result = run_git(
        repo,
        ["merge-base", "--is-ancestor", older_ref, newer_ref],
        capture_stdout=True,
        allow_failure=True,
    )
    return result.returncode == 0


def rev_parse(repo: Path, ref: str) -> str:
    result = run_git(repo, ["rev-parse", f"{ref}^{{commit}}"], capture_stdout=True)
    return result.stdout.strip()


def authenticated_origin_url(github_repository: str, token: str) -> str:
    if not github_repository:
        raise MirrorSyncError("missing github repository for authenticated mirror push")
    return f"https://x-access-token:{token}@github.com/{github_repository}.git"


def write_github_output(path: Path, result: dict[str, Any]) -> None:
    lines = [
        f"synced_upstream_main_sha={result['synced_upstream_main_sha']}",
        f"expected_mirror_sha={result['expected_mirror_sha']}",
        f"mirror_audit_args_json={json.dumps(result['mirror_audit_args'], separators=(',', ':'))}",
        f"mirror_state={result['mirror_state']}",
        f"audit_baseline={result['audit_baseline']}",
    ]
    with path.open("a", encoding="utf-8") as output:
        for line in lines:
            output.write(f"{line}\n")


def run_git(
    repo: Path,
    args: list[str],
    *,
    capture_stdout: bool = False,
    allow_failure: bool = False,
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=capture_stdout,
        text=True,
        check=False,
    )
    if result.returncode != 0 and not allow_failure:
        stdout = (result.stdout or "").strip()
        stderr = (result.stderr or "").strip()
        raise MirrorSyncError(
            "git command failed: "
            f"{' '.join(args)}\nstdout={stdout}\nstderr={stderr}"
        )
    return result


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except MirrorSyncError as exc:
        print(f"sync-upstream-mirror failed: {exc}", file=sys.stderr)
        raise SystemExit(1)
