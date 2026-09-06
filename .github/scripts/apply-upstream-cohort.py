#!/usr/bin/env python3
"""Apply one admitted upstream conflict cohort to the hosted materialized cut.

The input is deliberately data-only: a JSON disposition receipt and the
already-produced composition artifact.  No source code or receipt content is
executed.  All Git objects are imported from the pinned bundle before any
path is changed.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import pathlib
import subprocess
import tempfile


BASE_SHA = "5eb6ca6519b1a79e8997bf21321885de1fd9ed01"
BASE_TREE = "7a4e9d32c7a13a22215335a850cf879e284fdc63"
UPSTREAM_SHA = "008bbd5884122dc95aaece19ecfe0fc6a59dcf36"
UPSTREAM_TREE = "721cd395f53962482b3f6d140d0b9942fef3baac"
SOURCE_RUN_ID = "34035744523"
SOURCE_RUN_ATTEMPT = "1"
SOURCE_ARTIFACT = "upstream-composition-34035744523-1"
BUNDLE_SHA256 = "b383183cf21ade4b50244986cf1589988b248259ee51f099932bb0c06b026dd6"
SOURCE_RECEIPT_SHA256 = "2bcebca05cb45d6d2caad475ec5348a3883566f99e6a98d24196382d52d39e93"
SOURCE_MANIFEST_SHA256 = "0451d500a2a9868825337ddd0e6c16cd73c5088116131d75b4f27f801885328b"
UNRESOLVED_COUNT = 427
UNRESOLVED_PATHS_SHA256 = "7568a0be65dc7c05f591197a49b0b4e18f2c4435b951097d206cb37626233b62"


def run(*args: str, cwd: pathlib.Path | None = None, text: bool = True) -> str:
    result = subprocess.run(args, cwd=cwd, check=True, text=text, capture_output=True)
    return result.stdout


def digest(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def load_json(path: pathlib.Path) -> dict:
    with path.open(encoding="utf-8") as stream:
        value = json.load(stream)
    if not isinstance(value, dict):
        raise SystemExit(f"{path} must contain an object")
    return value


def path_digest(paths: list[str]) -> str:
    data = ("\n".join(paths) + ("\n" if paths else "")).encode()
    return hashlib.sha256(data).hexdigest()


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(message)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifact-dir", type=pathlib.Path, required=True)
    parser.add_argument("--disposition", type=pathlib.Path, required=True)
    parser.add_argument("--output-dir", type=pathlib.Path, required=True)
    args = parser.parse_args()

    artifact = args.artifact_dir
    bundle = artifact / "materialized.bundle"
    source_receipt = artifact / "receipt.json"
    source_provenance = artifact / "provenance.json"
    manifest_path = artifact / "conflict-manifest.json"
    staged_patch = artifact / "staged-auto-merge.patch"
    staged_paths = artifact / "staged-auto-merge.txt"
    for path in (bundle, source_receipt, source_provenance, manifest_path, staged_patch, staged_paths):
        require(path.is_file() and path.stat().st_size > 0, f"missing or empty source evidence: {path.name}")
    require(digest(bundle) == BUNDLE_SHA256, "source bundle digest mismatch")
    require(digest(source_receipt) == SOURCE_RECEIPT_SHA256, "source receipt digest mismatch")
    require(digest(manifest_path) == SOURCE_MANIFEST_SHA256, "source manifest digest mismatch")

    source_receipt_obj = load_json(source_receipt)
    source_provenance_obj = load_json(source_provenance)
    manifest = load_json(manifest_path)
    require(source_provenance_obj.get("signed") is False, "source provenance must be explicitly unsigned")
    require(source_provenance_obj.get("repository") == "sednalabs/codex", "source repository mismatch")
    for key, expected in {
        "base_sha": BASE_SHA,
        "base_tree": BASE_TREE,
        "upstream_sha": UPSTREAM_SHA,
        "upstream_tree": UPSTREAM_TREE,
        "run_id": SOURCE_RUN_ID,
        "run_attempt": SOURCE_RUN_ATTEMPT,
        }.items():
        require(str(source_provenance_obj.get(key)) == expected, f"source provenance mismatch: {key}")
    require(manifest.get("total_unmerged_path_count") == UNRESOLVED_COUNT, "unresolved path count mismatch")
    require(manifest.get("path_set_sha256") == UNRESOLVED_PATHS_SHA256, "unresolved path digest mismatch")
    require(source_receipt_obj.get("upstream_sha") == UPSTREAM_SHA, "source receipt upstream mismatch")

    disposition = load_json(args.disposition)
    require(disposition.get("schema") == "upstream-cohort-disposition", "invalid disposition schema")
    require(disposition.get("version") == 1, "unsupported disposition version")
    require(disposition.get("repository") == "sednalabs/codex", "disposition repository mismatch")
    for key, expected in {
        "base_sha": BASE_SHA,
        "base_tree": BASE_TREE,
        "upstream_sha": UPSTREAM_SHA,
        "upstream_tree": UPSTREAM_TREE,
        "source_run_id": SOURCE_RUN_ID,
        "source_run_attempt": SOURCE_RUN_ATTEMPT,
        "source_artifact": SOURCE_ARTIFACT,
    }.items():
        require(str(disposition.get(key)) == expected, f"disposition mismatch: {key}")
    cohort_name = disposition.get("cohort")
    cohorts = [c for c in manifest.get("cohorts", []) if c.get("name") == cohort_name]
    require(len(cohorts) == 1, "disposition cohort is not present exactly once")
    cohort = cohorts[0]
    entries = disposition.get("paths")
    require(isinstance(entries, list), "disposition paths must be a list")
    by_path: dict[str, dict] = {}
    for entry in entries:
        require(isinstance(entry, dict) and isinstance(entry.get("path"), str), "invalid disposition path entry")
        path = entry["path"]
        require(path and not path.startswith("/") and ".." not in pathlib.PurePosixPath(path).parts, f"unsafe path: {path}")
        require(path not in by_path, f"duplicate disposition path: {path}")
        by_path[path] = entry
    paths = sorted(by_path)
    require(len(paths) == int(cohort.get("path_count", -1)), "cohort disposition count mismatch")
    require(path_digest(paths) == cohort.get("path_set_sha256"), "cohort path digest mismatch")
    require(disposition.get("path_set_sha256") == cohort.get("path_set_sha256"), "disposition path digest mismatch")
    allowed = {"base", "upstream", "manual", "delete"}
    for entry in by_path.values():
        require(entry.get("disposition") in allowed, "invalid path disposition")
        if entry["disposition"] == "manual":
            require(isinstance(entry.get("patch_base64"), str) and entry["patch_base64"], "manual path lacks patch")

    args.output_dir.mkdir(parents=True, exist_ok=True)
    repo = pathlib.Path.cwd()
    with tempfile.TemporaryDirectory(prefix="apply-upstream-cohort-") as temp_name:
        temp = pathlib.Path(temp_name)
        bare = temp / "import.git"
        run("git", "init", "--bare", str(bare))
        run("git", "-C", str(bare), "bundle", "verify", str(bundle))
        heads = {}
        for line in run("git", "-C", str(bare), "bundle", "list-heads", str(bundle)).splitlines():
            oid, ref = line.split(maxsplit=1)
            heads[ref] = oid
        require(BASE_SHA in heads.values() and UPSTREAM_SHA in heads.values(), "source bundle lacks pinned heads")
        materialized_sha = source_provenance_obj.get("materialized_sha")
        require(isinstance(materialized_sha, str) and materialized_sha in heads.values(), "source materialized head missing")
        refs = {"base": BASE_SHA, "upstream": UPSTREAM_SHA, "materialized": materialized_sha}
        for name, oid in refs.items():
            source_ref = next(ref for ref, value in heads.items() if value == oid)
            run("git", "-C", str(repo), "fetch", str(bundle), f"{source_ref}:refs/w13825-import/{name}")
        require(run("git", "rev-parse", "refs/w13825-import/base") == BASE_SHA + "\n", "imported base mismatch")
        require(run("git", "rev-parse", "refs/w13825-import/base^{tree}") == BASE_TREE + "\n", "imported base tree mismatch")
        require(run("git", "rev-parse", "refs/w13825-import/upstream^{tree}") == UPSTREAM_TREE + "\n", "imported upstream tree mismatch")
        require(run("git", "rev-parse", "refs/w13825-import/materialized^{tree}").strip() == source_provenance_obj.get("materialized_tree"), "imported materialized tree mismatch")
        require(run("git", "rev-parse", "refs/w13825-import/materialized^") == BASE_SHA + "\n", "materialized parent mismatch")

        worktree = temp / "candidate"
        run("git", "worktree", "add", "--detach", str(worktree), materialized_sha, cwd=repo)
        try:
            patch = temp / "cohort.patch"
            pieces: list[bytes] = []
            for path in paths:
                disposition_name = by_path[path]["disposition"]
                if disposition_name == "upstream":
                    pieces.append(subprocess.run(["git", "diff", "--binary", materialized_sha, UPSTREAM_SHA, "--", path], cwd=repo, check=True, capture_output=True).stdout)
                elif disposition_name == "manual":
                    pieces.append(base64.b64decode(by_path[path]["patch_base64"], validate=True))
            patch.write_bytes(b"".join(pieces))
            if patch.stat().st_size:
                run("git", "-C", str(worktree), "apply", "--index", "--binary", "--whitespace=nowarn", str(patch))
            for path in paths:
                if by_path[path]["disposition"] == "delete":
                    run("git", "-C", str(worktree), "rm", "-f", "--ignore-unmatch", "--", path)
            changed = run("git", "-C", str(worktree), "diff", "--cached", "--name-only", "--diff-filter=ACDMRTXB").splitlines()
            require(set(changed) <= set(paths), "candidate changed a path outside the admitted cohort")
            tree = run("git", "-C", str(worktree), "write-tree").strip()
            candidate_message = f"Apply admitted upstream cohort {cohort_name}"
            env = {**os.environ, "GIT_AUTHOR_NAME": "github-actions[bot]", "GIT_AUTHOR_EMAIL": "41898282+github-actions[bot]@users.noreply.github.com", "GIT_COMMITTER_NAME": "github-actions[bot]", "GIT_COMMITTER_EMAIL": "41898282+github-actions[bot]@users.noreply.github.com"}
            candidate_sha = subprocess.run(["git", "-C", str(worktree), "commit-tree", tree, "-p", materialized_sha, "-m", candidate_message], check=True, text=True, capture_output=True, env=env).stdout.strip()
            run("git", "-C", str(repo), "update-ref", "refs/w13825-import/candidate", candidate_sha)
            run("git", "-C", str(repo), "bundle", "create", str(args.output_dir / "candidate.bundle"), "refs/w13825-import/candidate", "refs/w13825-import/materialized", "refs/w13825-import/base", "refs/w13825-import/upstream")
            (args.output_dir / "candidate.patch").write_bytes(subprocess.run(["git", "diff", "--binary", materialized_sha, candidate_sha], cwd=repo, check=True, capture_output=True).stdout)
        finally:
            run("git", "worktree", "remove", "--force", str(worktree), cwd=repo)

    provenance = {
        "schema": "upstream-cohort-candidate-provenance", "version": 1,
        "signed": False, "repository": "sednalabs/codex", "base_sha": BASE_SHA, "base_tree": BASE_TREE,
        "upstream_sha": UPSTREAM_SHA, "upstream_tree": UPSTREAM_TREE, "materialized_sha": materialized_sha,
        "candidate_sha": candidate_sha, "candidate_parent": materialized_sha, "candidate_tree": tree,
        "source_run_id": SOURCE_RUN_ID, "source_run_attempt": SOURCE_RUN_ATTEMPT, "source_artifact": SOURCE_ARTIFACT,
        "source_bundle_sha256": BUNDLE_SHA256, "source_receipt_sha256": SOURCE_RECEIPT_SHA256,
        "source_manifest_sha256": SOURCE_MANIFEST_SHA256, "cohort": cohort_name,
        "disposition_sha256": digest(args.disposition), "candidate_bundle_sha256": digest(args.output_dir / "candidate.bundle"),
    }
    (args.output_dir / "provenance.json").write_text(json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    receipt = {**disposition, "candidate_sha": candidate_sha, "candidate_parent": materialized_sha, "candidate_tree": tree, "candidate_bundle_sha256": provenance["candidate_bundle_sha256"]}
    (args.output_dir / "receipt.json").write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(provenance, sort_keys=True))


if __name__ == "__main__":
    main()
