#!/usr/bin/env python3
"""Produce a read-only, metadata-only preview of an explicit upstream merge.

The helper accepts only immutable 40-hex commit IDs and fetches them only from
the two named public repositories.  It never writes a ref, working tree, or
merge commit.  Its JSON intentionally excludes Git's human conflict messages,
file contents, and path names.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


SHA40 = re.compile(r"^[0-9a-fA-F]{40}$")
TREE40 = re.compile(rb"^[0-9a-f]{40}$")
GIT_VERSION = re.compile(r"^git version ([0-9][0-9A-Za-z._+-]*)\n?$")
PUBLIC_REMOTES = {
    "downstream": "https://github.com/sednalabs/codex.git",
    "upstream": "https://github.com/openai/codex.git",
}
REQUIRED_MERGE_TREE_OPTIONS = (b"--write-tree", b"--merge-base", b"--name-only", b"--messages", b"-z")
MAX_PATHS_PER_INFORMATION_RECORD = 10_000


class PreviewError(Exception):
    """A deliberately non-sensitive diagnostic category."""


@dataclass(frozen=True)
class InformationalRecord:
    paths: tuple[bytes, ...]
    stable_type: str


@dataclass(frozen=True)
class MergeTreeResult:
    status: str
    tree: str
    staged_paths: tuple[bytes, ...]
    records: tuple[InformationalRecord, ...]


def _environment() -> dict[str, str]:
    environment = dict(os.environ)
    # Public objects never need interactive credentials; fail instead of asking.
    environment["GIT_TERMINAL_PROMPT"] = "0"
    return environment


def run_git(repo: Path, *arguments: str, check: bool = True) -> subprocess.CompletedProcess[bytes]:
    try:
        process = subprocess.run(
            ["git", "-C", os.fspath(repo), *arguments],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            env=_environment(),
        )
    except OSError as error:
        raise PreviewError("git unavailable") from error
    if check and process.returncode != 0:
        # stderr can contain paths and remote/server content.  Do not relay it.
        raise PreviewError("git command failed")
    return process


def exact_sha(value: str, label: str) -> str:
    if not SHA40.fullmatch(value):
        raise PreviewError(f"invalid {label} SHA")
    return value.lower()


def code_sha256() -> str:
    return hashlib.sha256(Path(__file__).read_bytes()).hexdigest()


def git_version(repo: Path) -> str:
    process = run_git(repo, "--version")
    try:
        match = GIT_VERSION.fullmatch(process.stdout.decode("ascii"))
    except UnicodeDecodeError as error:
        raise PreviewError("unrecognised git version") from error
    if not match:
        raise PreviewError("unrecognised git version")
    return match.group(1)


def validate_merge_tree_help(help_output: bytes) -> None:
    if any(option not in help_output for option in REQUIRED_MERGE_TREE_OPTIONS):
        raise PreviewError("unsupported git merge-tree capability")


def require_merge_tree_capability(repo: Path) -> None:
    # `-h` normally exits nonzero; only its option inventory is meaningful.
    process = run_git(repo, "merge-tree", "-h", check=False)
    validate_merge_tree_help(process.stdout + process.stderr)


def actual_commit(repo: Path, sha: str) -> str:
    process = run_git(repo, "rev-parse", "--verify", f"{sha}^{{commit}}", check=False)
    if process.returncode != 0:
        raise PreviewError("required object unavailable or not a commit")
    try:
        resolved = process.stdout.decode("ascii").strip().lower()
    except UnicodeDecodeError as error:
        raise PreviewError("invalid resolved commit identity") from error
    if not SHA40.fullmatch(resolved):
        raise PreviewError("invalid resolved commit identity")
    return resolved


def commit_tree(repo: Path, sha: str) -> str:
    process = run_git(repo, "rev-parse", "--verify", f"{sha}^{{tree}}")
    tree = process.stdout.strip().lower()
    if not TREE40.fullmatch(tree):
        raise PreviewError("invalid commit tree identity")
    return tree.decode("ascii")


def is_ancestor(repo: Path, older: str, newer: str) -> bool:
    process = run_git(repo, "merge-base", "--is-ancestor", older, newer, check=False)
    if process.returncode == 0:
        return True
    if process.returncode == 1:
        return False
    raise PreviewError("unable to establish ancestry")


def common_bases(repo: Path, first: str, second: str) -> tuple[str, ...]:
    process = run_git(repo, "merge-base", "--all", first, second, check=False)
    if process.returncode != 0:
        raise PreviewError("unable to determine common merge base")
    try:
        bases = tuple(line.lower() for line in process.stdout.decode("ascii").splitlines())
    except UnicodeDecodeError as error:
        raise PreviewError("invalid common merge base identity") from error
    if not bases or any(not SHA40.fullmatch(base) for base in bases):
        raise PreviewError("invalid common merge base identity")
    return bases


def _split_nul_field(data: bytes, offset: int) -> tuple[bytes, int]:
    end = data.find(b"\0", offset)
    if end < 0:
        raise PreviewError("malformed merge-tree output")
    return data[offset:end], end + 1


def serialise_repo_path(path: bytes) -> dict[str, str]:
    """Emit a reversible public-repository-relative path identity.

    Git stores path names as bytes.  UTF-8 names remain useful to the consumer;
    non-UTF-8 names are represented by their exact hex bytes rather than a
    lossy replacement.  A path that could be interpreted as a host absolute or
    traversal path rejects the whole preview rather than entering an artifact.
    """
    if not path or path.startswith((b"/", b"\\")):
        raise PreviewError("unsafe merge-tree repository path")
    components = path.split(b"/")
    if any(component in (b"", b".", b"..") for component in components):
        raise PreviewError("unsafe merge-tree repository path")
    if len(path) >= 3 and path[0:1].isalpha() and path[1:2] == b":" and path[2:3] in (b"/", b"\\"):
        raise PreviewError("unsafe merge-tree repository path")
    try:
        return {"encoding": "utf-8", "value": path.decode("utf-8", "strict")}
    except UnicodeDecodeError:
        return {"encoding": "hex", "value": path.hex()}


def parse_informational_records(data: bytes) -> tuple[InformationalRecord, ...]:
    """Parse only the documented stable NUL records; discard free-form messages."""
    if not data:
        return ()
    records: list[InformationalRecord] = []
    offset = 0
    while offset < len(data):
        count_raw, offset = _split_nul_field(data, offset)
        if not count_raw.isascii() or not count_raw.isdigit():
            raise PreviewError("malformed merge-tree informational record")
        path_count = int(count_raw)
        if path_count > MAX_PATHS_PER_INFORMATION_RECORD:
            raise PreviewError("merge-tree informational record exceeds limit")
        paths: list[bytes] = []
        for _ in range(path_count):
            path, offset = _split_nul_field(data, offset)
            serialise_repo_path(path)
            paths.append(path)
        type_raw, offset = _split_nul_field(data, offset)
        # This is the documented stable type.  The following human message is
        # consumed but never decoded, parsed, retained, or emitted.
        try:
            stable_type = type_raw.decode("utf-8", "strict")
        except UnicodeDecodeError as error:
            raise PreviewError("malformed merge-tree informational record") from error
        if not stable_type:
            raise PreviewError("malformed merge-tree informational record")
        _, offset = _split_nul_field(data, offset)
        records.append(InformationalRecord(tuple(paths), stable_type))
    return tuple(records)


def interpret_merge_tree(returncode: int, raw: bytes) -> MergeTreeResult:
    """Interpret documented ``merge-tree --write-tree --name-only --messages -z`` output.

    Exit status, rather than an empty staged-path section, decides whether a
    merge is clean.  This matters for directory and other conflicts with no
    individual staged path records.
    """
    if returncode not in (0, 1):
        raise PreviewError("merge-tree execution failed")
    tree_raw, separator, remainder = raw.partition(b"\0")
    if not separator or not TREE40.fullmatch(tree_raw.lower()):
        raise PreviewError("malformed merge-tree output")
    tree = tree_raw.decode("ascii").lower()
    if returncode == 0:
        # --messages is deliberate: a clean, non-overlapping content merge can
        # still report a documented Auto-merging informational record.  There
        # can be no conflicted-stage path section for exit status 0, so any
        # suffix must begin with the NUL messages-section separator.
        if not remainder:
            return MergeTreeResult("clean", tree, (), ())
        if not remainder.startswith(b"\0"):
            raise PreviewError("malformed clean merge-tree output")
        return MergeTreeResult("clean", tree, (), parse_informational_records(remainder[1:]))

    # With -z, the empty field between name-only paths and informational
    # records is the documented section separator.  When there are no staged
    # paths, the separator is the first byte of the remainder.
    if not remainder:
        raise PreviewError("malformed conflicted merge-tree output")
    if remainder.startswith(b"\0"):
        staged_paths: tuple[bytes, ...] = ()
        informational = remainder[1:]
    else:
        staged_section, marker, informational = remainder.partition(b"\0\0")
        if not marker:
            raise PreviewError("malformed conflicted merge-tree output")
        staged_paths = tuple(staged_section.split(b"\0"))
        if not staged_paths:
            raise PreviewError("malformed conflicted merge-tree output")
        for path in staged_paths:
            serialise_repo_path(path)
    return MergeTreeResult("conflicts", tree, staged_paths, parse_informational_records(informational))


def is_conflict_stable_type(stable_type: str) -> bool:
    """Classify the stable merge-ort vocabulary, including no-space forms."""
    return stable_type.startswith("CONFLICT (") or stable_type.startswith("CONFLICT(")


def _type_counts(records: Iterable[InformationalRecord], *, conflicts_only: bool | None = None) -> list[dict[str, object]]:
    counts: dict[str, int] = {}
    for record in records:
        is_conflict = is_conflict_stable_type(record.stable_type)
        if conflicts_only is not None and conflicts_only != is_conflict:
            continue
        counts[record.stable_type] = counts.get(record.stable_type, 0) + 1
    return [{"type": kind, "count": counts[kind]} for kind in sorted(counts)]


def _observer(repo: Path) -> dict[str, str]:
    return {
        "program": "upstream-merge-preview",
        "format": "2",
        "code_sha256": code_sha256(),
        "git_version": git_version(repo),
    }


def _incomplete(requested: dict[str, str], reason: str, repo: Path | None = None) -> dict[str, object]:
    result: dict[str, object] = {"version": 2, "status": "diagnostic-incomplete", "reason": reason, "requested": requested}
    if repo is not None:
        try:
            result["observer"] = _observer(repo)
        except PreviewError:
            result["observer"] = {"program": "upstream-merge-preview", "format": "2", "code_sha256": code_sha256(), "git_version": "unavailable"}
    return result


def preview_repository(repo: Path, downstream: str, upstream: str, logical_base: str, rewritten_base: str | None = None) -> dict[str, object]:
    """Run the deterministic preview against already-present objects (used by hosted fixtures)."""
    requested: dict[str, str] = {}
    result: dict[str, object] | None = None
    try:
        downstream = exact_sha(downstream, "downstream")
        upstream = exact_sha(upstream, "upstream")
        logical_base = exact_sha(logical_base, "logical base")
        requested = {"downstream": downstream, "upstream": upstream, "logical_base": logical_base}
        if rewritten_base is not None:
            rewritten_base = exact_sha(rewritten_base, "rewritten base")
            requested["rewritten_base"] = rewritten_base

        require_merge_tree_capability(repo)
        actual_downstream = actual_commit(repo, downstream)
        actual_upstream = actual_commit(repo, upstream)
        actual_base = actual_commit(repo, logical_base)
        actual_rewrite = actual_commit(repo, rewritten_base) if rewritten_base else None
        base_tree = commit_tree(repo, actual_base)
        observer = _observer(repo)
        result: dict[str, object] = {
            "version": 2,
            "observer": observer,
            "requested": requested,
            "actual": {"downstream": actual_downstream, "upstream": actual_upstream, "logical_base": actual_base},
        }
        if actual_rewrite:
            result["actual"]["rewritten_base"] = actual_rewrite  # type: ignore[index]

        if actual_rewrite is None:
            if not is_ancestor(repo, actual_base, actual_downstream) or not is_ancestor(repo, actual_base, actual_upstream):
                raise PreviewError("logical base is not an ancestor of both inputs")
            bases = common_bases(repo, actual_downstream, actual_upstream)
            if len(bases) != 1:
                raise PreviewError("direct merge base is ambiguous")
            if bases[0] != actual_base:
                raise PreviewError("logical base is not the unique direct merge base")
            base_contract: dict[str, object] = {
                "mode": "direct-unique-common-base",
                "merge_base_tree": base_tree,
                "natural_common_base_used": False,
            }
        else:
            rewrite_tree = commit_tree(repo, actual_rewrite)
            base_contract = {
                "mode": "mapped-rewrite-explicit-tree",
                "merge_base_tree": base_tree,
                "logical_base_is_ancestor_of_upstream": is_ancestor(repo, actual_base, actual_upstream),
                "rewritten_base_is_ancestor_of_downstream": is_ancestor(repo, actual_rewrite, actual_downstream),
                "logical_base_is_ancestor_of_downstream": is_ancestor(repo, actual_base, actual_downstream),
                "rewrite_tree_equal": base_tree == rewrite_tree,
                # The helper deliberately never chooses or reports an arbitrary
                # historical common ancestor for a rewritten graph.
                "natural_common_base_used": False,
            }
            if not base_contract["logical_base_is_ancestor_of_upstream"] or not base_contract["rewritten_base_is_ancestor_of_downstream"] or not base_contract["rewrite_tree_equal"]:
                raise PreviewError("mapped rewrite base contract is unproven")
        result["base_contract"] = base_contract

        process = run_git(
            repo,
            "merge-tree",
            "--write-tree",
            f"--merge-base={base_tree}",
            "--name-only",
            "--messages",
            "-z",
            actual_downstream,
            actual_upstream,
            check=False,
        )
        merge = interpret_merge_tree(process.returncode, process.stdout)
        result.update(
            status=merge.status,
            merge={
                "result_tree": merge.tree,
                "staged_path_count": len(merge.staged_paths),
                "staged_paths": [serialise_repo_path(path) for path in merge.staged_paths],
                "conflict_record_count": sum(1 for record in merge.records if is_conflict_stable_type(record.stable_type)),
                "conflict_types": _type_counts(merge.records, conflicts_only=True),
                "informational_record_count": len(merge.records),
                "informational_types": _type_counts(merge.records),
                "informational_records": [
                    {"type": record.stable_type, "paths": [serialise_repo_path(path) for path in record.paths]}
                    for record in merge.records
                ],
            },
        )
        return result
    except PreviewError as error:
        if result is not None:
            result.update(status="diagnostic-incomplete", reason=str(error))
            return result
        return _incomplete(requested, str(error), repo)


def fetch(repo: Path, remote: str, sha: str) -> None:
    process = run_git(repo, "fetch", "--no-tags", remote, sha, check=False)
    if process.returncode != 0:
        raise PreviewError("required public object unavailable")


def preview(downstream: str, upstream: str, logical_base: str, rewritten_base: str | None) -> dict[str, object]:
    requested: dict[str, str] = {}
    try:
        downstream = exact_sha(downstream, "downstream")
        upstream = exact_sha(upstream, "upstream")
        logical_base = exact_sha(logical_base, "logical base")
        requested = {"downstream": downstream, "upstream": upstream, "logical_base": logical_base}
        if rewritten_base is not None:
            rewritten_base = exact_sha(rewritten_base, "rewritten base")
            requested["rewritten_base"] = rewritten_base
        with tempfile.TemporaryDirectory(prefix="upstream-merge-preview-") as temporary:
            repo = Path(temporary) / "objects.git"
            try:
                run_git(Path(temporary), "init", "--bare", "--quiet", os.fspath(repo))
                for name, url in PUBLIC_REMOTES.items():
                    run_git(repo, "remote", "add", name, url)
                fetch(repo, "downstream", downstream)
                fetch(repo, "upstream", upstream)
                fetch(repo, "upstream", logical_base)
                if rewritten_base:
                    fetch(repo, "downstream", rewritten_base)
                return preview_repository(repo, downstream, upstream, logical_base, rewritten_base)
            except PreviewError as error:
                return _incomplete(requested, str(error), repo)
    except PreviewError as error:
        return _incomplete(requested, str(error))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--downstream", required=True)
    parser.add_argument("--upstream", required=True)
    parser.add_argument("--base", required=True, dest="logical_base")
    parser.add_argument("--rewritten-base")
    arguments = parser.parse_args()
    try:
        result = preview(arguments.downstream, arguments.upstream, arguments.logical_base, arguments.rewritten_base)
    except Exception:
        # Do not let an unexpected implementation failure place a traceback,
        # host path, or raw subprocess output in a workflow log or artifact.
        result = _incomplete({}, "internal helper failure")
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return exit_status(result)


def exit_status(result: dict[str, object]) -> int:
    status = result.get("status")
    if status == "clean":
        return 0
    if status == "conflicts":
        return 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
