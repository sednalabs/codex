#!/usr/bin/env python3
"""Prepare one fixed, provisional upstream-integration branch in hosted CI only.

This helper deliberately creates a single-parent downstream commit.  It does
not assert that upstream is an ancestor, complete a sync, or decide carry
status; reconciliation remains a later, separate hosted operation.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path

import upstream_merge_preview as preview


DOWNSTREAM = "f12747ca5e6eb85d32a823b9450726c76ffbb93e"
UPSTREAM = "7f83d4922d7e92a36c1c1e4f61159a5815d45360"
LOGICAL_BASE = "a4535884169be8da2f81b8a4debecbd4dc11aa97"
REWRITTEN_BASE = "aa505a66940223ac01048e2b75113965fe260118"
INTEGRATION_BRANCH = "integration/upstream-20260916"
INTEGRATION_REF = f"refs/heads/{INTEGRATION_BRANCH}"
PREPARATION_MESSAGE = "Provisional upstream integration preparation; unresolved conflicts and carry remain."


class PrepareError(Exception):
    """A public-safe preparation diagnostic category."""


@dataclass(frozen=True)
class FrozenInputs:
    downstream: str = DOWNSTREAM
    upstream: str = UPSTREAM
    logical_base: str = LOGICAL_BASE
    rewritten_base: str = REWRITTEN_BASE


def validate_inputs(inputs: FrozenInputs) -> FrozenInputs:
    try:
        return FrozenInputs(
            preview.exact_sha(inputs.downstream, "downstream"),
            preview.exact_sha(inputs.upstream, "upstream"),
            preview.exact_sha(inputs.logical_base, "logical base"),
            preview.exact_sha(inputs.rewritten_base, "rewritten base"),
        )
    except preview.PreviewError as error:
        raise PrepareError("invalid frozen integration input") from error


def _public_environment() -> dict[str, str]:
    return preview._environment()


def _commit_environment() -> dict[str, str]:
    environment = _public_environment()
    # Prevent host identity/time from entering a public provisional commit.
    environment.update(
        GIT_AUTHOR_NAME="Upstream Integration Preparation",
        GIT_AUTHOR_EMAIL="noreply@github.com",
        GIT_COMMITTER_NAME="Upstream Integration Preparation",
        GIT_COMMITTER_EMAIL="noreply@github.com",
        GIT_AUTHOR_DATE="1970-01-01T00:00:00Z",
        GIT_COMMITTER_DATE="1970-01-01T00:00:00Z",
    )
    return environment


def _push_environment() -> dict[str, str]:
    token = os.environ.get("GITHUB_TOKEN")
    if not token:
        raise PrepareError("push credential unavailable")
    environment = _public_environment()
    # This transient process configuration authenticates the one push without
    # modifying the bare repository's remote URL or persistent Git config.
    environment.update(
        GIT_CONFIG_COUNT="1",
        GIT_CONFIG_KEY_0="http.https://github.com/.extraheader",
        GIT_CONFIG_VALUE_0=f"AUTHORIZATION: Bearer {token}",
    )
    return environment


def _run(repo: Path, *arguments: str, environment: dict[str, str] | None = None) -> subprocess.CompletedProcess[bytes]:
    try:
        return subprocess.run(
            ["git", "-C", os.fspath(repo), *arguments],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            env=environment or _public_environment(),
        )
    except OSError as error:
        raise PrepareError("git unavailable") from error


def _actual_commit(repo: Path, sha: str) -> str:
    try:
        return preview.actual_commit(repo, sha)
    except preview.PreviewError as error:
        raise PrepareError("required public object unavailable") from error


def recompute_mapped_preview(repo: Path, inputs: FrozenInputs) -> dict[str, object]:
    """Reuse the established mapped-base contract before any ref mutation."""
    result = preview.preview_repository(
        repo,
        inputs.downstream,
        inputs.upstream,
        inputs.logical_base,
        inputs.rewritten_base,
    )
    if result.get("status") == "diagnostic-incomplete":
        raise PrepareError("mapped preview validation failed")
    return result


def result_tree(metadata: dict[str, object]) -> str:
    merge = metadata.get("merge")
    if not isinstance(merge, dict) or not isinstance(merge.get("result_tree"), str):
        raise PrepareError("preview result tree unavailable")
    tree = merge["result_tree"]
    try:
        return preview.exact_sha(tree, "result tree")
    except preview.PreviewError as error:
        raise PrepareError("preview result tree unavailable") from error


def create_provisional_commit(repo: Path, tree: str, downstream: str) -> str:
    """Create a candidate with exactly one parent: downstream D."""
    process = _run(
        repo,
        "commit-tree",
        tree,
        "-p",
        downstream,
        "-m",
        PREPARATION_MESSAGE,
        environment=_commit_environment(),
    )
    if process.returncode != 0:
        raise PrepareError("unable to create provisional integration commit")
    try:
        candidate = process.stdout.decode("ascii").strip().lower()
    except UnicodeDecodeError as error:
        raise PrepareError("invalid provisional integration commit") from error
    try:
        return preview.exact_sha(candidate, "provisional integration commit")
    except preview.PreviewError as error:
        raise PrepareError("invalid provisional integration commit") from error


def refuse_existing_ref(repo: Path) -> None:
    process = preview.run_git(repo, "ls-remote", "--exit-code", "--heads", "downstream", INTEGRATION_REF, check=False)
    if process.returncode == 2:
        return
    if process.returncode == 0:
        raise PrepareError("integration branch already exists")
    raise PrepareError("unable to establish integration branch absence")


def push_new_ref(repo: Path, candidate: str) -> None:
    """Push only a creation, with an empty lease preventing a race overwrite."""
    process = _run(
        repo,
        "push",
        "--porcelain",
        f"--force-with-lease={INTEGRATION_REF}:",
        "downstream",
        f"{candidate}:{INTEGRATION_REF}",
        environment=_push_environment(),
    )
    if process.returncode != 0:
        raise PrepareError("integration branch creation refused")


def _metadata(inputs: FrozenInputs, preview_result: dict[str, object], candidate: str | None = None) -> dict[str, object]:
    merge = preview_result.get("merge")
    if not isinstance(merge, dict):
        raise PrepareError("preview metadata unavailable")
    result: dict[str, object] = {
        "version": 1,
        "status": "provisional-prepared" if candidate else "validated-not-published",
        "successful_sync": False,
        "carry_status": "unknown",
        "inputs": {
            "downstream": inputs.downstream,
            "upstream": inputs.upstream,
            "logical_base": inputs.logical_base,
            "rewritten_base": inputs.rewritten_base,
        },
        "ref": INTEGRATION_REF,
        "result_tree": result_tree(preview_result),
        "merge_status": preview_result.get("status"),
        "staged_path_count": merge.get("staged_path_count"),
        "conflict_record_count": merge.get("conflict_record_count"),
    }
    if candidate is not None:
        result["candidate_commit"] = candidate
        result["parent_commit"] = inputs.downstream
    return result


def prepare_integration(*, publish: bool) -> dict[str, object]:
    """Perform the fixed preparation only when explicitly requested."""
    inputs = validate_inputs(FrozenInputs())
    if not publish:
        return {
            "version": 1,
            "status": "not-requested",
            "preparation_requested": False,
            "successful_sync": False,
            "carry_status": "unknown",
            "inputs": {
                "downstream": inputs.downstream,
                "upstream": inputs.upstream,
                "logical_base": inputs.logical_base,
                "rewritten_base": inputs.rewritten_base,
            },
            "ref": INTEGRATION_REF,
        }
    with tempfile.TemporaryDirectory(prefix="upstream-integration-prepare-") as temporary:
        repo = Path(temporary) / "objects.git"
        try:
            preview.run_git(Path(temporary), "init", "--bare", "--quiet", os.fspath(repo))
            for name, url in preview.PUBLIC_REMOTES.items():
                preview.run_git(repo, "remote", "add", name, url)
            preview.fetch(repo, "downstream", inputs.downstream)
            preview.fetch(repo, "upstream", inputs.upstream)
            preview.fetch(repo, "upstream", inputs.logical_base)
            preview.fetch(repo, "downstream", inputs.rewritten_base)
            actual_downstream = _actual_commit(repo, inputs.downstream)
            if actual_downstream != inputs.downstream:
                raise PrepareError("downstream identity mismatch")
            preview_result = recompute_mapped_preview(repo, inputs)
            tree = result_tree(preview_result)
            refuse_existing_ref(repo)
            candidate = create_provisional_commit(repo, tree, inputs.downstream)
            push_new_ref(repo, candidate)
            return _metadata(inputs, preview_result, candidate)
        except (preview.PreviewError, PrepareError) as error:
            return {
                "version": 1,
                "status": "diagnostic-incomplete",
                "reason": str(error),
                "successful_sync": False,
                "carry_status": "unknown",
                "inputs": {
                    "downstream": inputs.downstream,
                    "upstream": inputs.upstream,
                    "logical_base": inputs.logical_base,
                    "rewritten_base": inputs.rewritten_base,
                },
                "ref": INTEGRATION_REF,
            }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prepare", action="store_true", help="Create only the fixed provisional integration branch")
    arguments = parser.parse_args()
    try:
        result = prepare_integration(publish=arguments.prepare)
    except Exception:
        result = {"version": 1, "status": "diagnostic-incomplete", "reason": "internal helper failure", "successful_sync": False, "carry_status": "unknown"}
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0 if result.get("status") in {"not-requested", "provisional-prepared"} else 2


if __name__ == "__main__":
    raise SystemExit(main())
