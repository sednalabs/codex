#!/usr/bin/env python3
"""Produce the first (SDK-only) hosted upstream cohort candidate.

The accepted composition artifact is the only source of imported Git objects.
The SDK disposition is derived from Git mode/type/OID tuples; no inline
receipt, patch payload, source-code execution, or base64 input is accepted.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import subprocess
import tempfile


BASE_SHA = "5eb6ca6519b1a79e8997bf21321885de1fd9ed01"
BASE_TREE = "7a4e9d32c7a13a22215335a850cf879e284fdc63"
UPSTREAM_SHA = "bc8884624330b6e681cfa3ce5fc575ce8298ed1b"
UPSTREAM_TREE = "1e143e2bc5964a4308d9a6f36ca3e2af028e79e9"
MATERIALIZED_SHA = "f5bb378d2e575b8f6f3cf266a0939ef404c37203"
MATERIALIZED_TREE = "49af672a3965958bfb1668f27c0caa27ba48554a"
SOURCE_RUN_ID = "34035744523"
SOURCE_RUN_ATTEMPT = "1"
SOURCE_ARTIFACT = "upstream-composition-34035744523-1"
BUNDLE_SHA256 = "b383183cf21ade4b50244986cf1589988b248259ee51f099932bb0c06b026dd6"
SOURCE_RECEIPT_SHA256 = "2bcebca05cb45d6d2caad475ec5348a3883566f99e6a98d24196382d52d39e93"
SOURCE_MANIFEST_SHA256 = "0451d500a2a9868825337ddd0e6c16cd73c5088116131d75b4f27f801885328b"
SOURCE_PROVENANCE_SHA256 = "afbf269c8593c978ed706c9f2fddc0031383350fe216d88512ec3707c8a55cb9"
UNRESOLVED_COUNT = 427
UNRESOLVED_PATHS_SHA256 = "7568a0be65dc7c05f591197a49b0b4e18f2c4435b951097d206cb37626233b62"


def run(*args: str, cwd: pathlib.Path | None = None, env: dict[str, str] | None = None) -> str:
    return subprocess.run(args, cwd=cwd, env=env, check=True, text=True, capture_output=True).stdout


def digest(path: pathlib.Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def load(path: pathlib.Path) -> dict:
    with path.open(encoding="utf-8") as stream:
        result = json.load(stream)
    if not isinstance(result, dict):
        raise SystemExit(f"{path} must contain an object")
    return result


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(message)


def classify(base: tuple[str, str, str] | None, materialized: tuple[str, str, str] | None, upstream: tuple[str, str, str] | None) -> str:
    if materialized != base:
        return "manual"
    if upstream == base:
        return "base"
    if upstream is None:
        return "delete"
    return "upstream"


def tree_entry(repo: pathlib.Path, revision: str, path: str) -> tuple[str, str, str] | None:
    output = subprocess.run(["git", "ls-tree", revision, "--", path], cwd=repo, check=True, text=True, capture_output=True).stdout.strip()
    if not output:
        return None
    mode, kind, oid, listed = output.split("\t", 1)[0].split() + [output.split("\t", 1)[1]]
    require(listed == path, f"unexpected tree path for {path}")
    return mode, kind, oid


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", type=pathlib.Path, required=True)
    parser.add_argument("--artifact-dir", type=pathlib.Path, required=True)
    parser.add_argument("--output-dir", type=pathlib.Path, required=True)
    args = parser.parse_args()
    for name, value in vars(args).items():
        require(value.is_absolute(), f"{name} must be absolute")
    repo = args.repo_root
    artifact = args.artifact_dir
    output = args.output_dir
    bundle = artifact / "materialized.bundle"
    source_receipt = artifact / "receipt.json"
    source_provenance = artifact / "provenance.json"
    manifest_path = artifact / "conflict-manifest.json"
    staged_patch = artifact / "staged-auto-merge.patch"
    staged_paths = artifact / "staged-auto-merge.txt"
    for path in (bundle, source_receipt, source_provenance, manifest_path, staged_patch, staged_paths):
        require(path.is_file() and path.stat().st_size > 0, f"missing source evidence: {path}")
    require(digest(bundle) == BUNDLE_SHA256, "source bundle digest mismatch")
    require(digest(source_receipt) == SOURCE_RECEIPT_SHA256, "source receipt digest mismatch")
    require(digest(source_provenance) == SOURCE_PROVENANCE_SHA256, "source provenance digest mismatch")
    require(digest(manifest_path) == SOURCE_MANIFEST_SHA256, "source manifest digest mismatch")

    receipt = load(source_receipt)
    provenance = load(source_provenance)
    manifest = load(manifest_path)
    require(provenance.get("signed") is False, "source provenance must remain unsigned")
    require(provenance.get("repository") == "sednalabs/codex", "source repository mismatch")
    for key, expected in {
        "run_id": SOURCE_RUN_ID, "run_attempt": SOURCE_RUN_ATTEMPT,
        "base_sha": BASE_SHA, "base_tree": BASE_TREE,
        "upstream_sha": UPSTREAM_SHA, "upstream_tree": UPSTREAM_TREE,
        "materialized_sha": MATERIALIZED_SHA, "materialized_tree": MATERIALIZED_TREE,
    }.items():
        require(str(provenance.get(key)) == expected, f"source provenance mismatch: {key}")
    for key, expected in {"base_sha": BASE_SHA, "base_tree": BASE_TREE, "upstream_sha": UPSTREAM_SHA, "upstream_tree": UPSTREAM_TREE}.items():
        require(str(receipt.get(key)) == expected, f"source receipt mismatch: {key}")
    require(manifest.get("total_unmerged_path_count") == UNRESOLVED_COUNT, "unresolved count mismatch")
    require(manifest.get("path_set_sha256") == UNRESOLVED_PATHS_SHA256, "unresolved path digest mismatch")
    for field, path in {
        "bundle_sha256": bundle,
        "receipt_sha256": source_receipt,
        "conflict_manifest_sha256": manifest_path,
        "staged_auto_merge_patch_sha256": staged_patch,
        "staged_auto_merge_path_list_sha256": staged_paths,
    }.items():
        require(provenance.get(field) == digest(path), f"source provenance evidence mismatch: {field}")
    cohort = next((item for item in manifest.get("cohorts", []) if item.get("name") == "sdk-public-contract"), None)
    require(isinstance(cohort, dict), "SDK cohort missing")

    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="w13825-sdk-", dir=str(args.output_dir.parent)) as temp_name:
        temp = pathlib.Path(temp_name)
        bare = temp / "source.git"
        run("git", "init", "--bare", str(bare))
        run("git", "-C", str(bare), "bundle", "verify", str(bundle))
        heads: dict[str, str] = {}
        for line in run("git", "-C", str(bare), "bundle", "list-heads", str(bundle)).splitlines():
            oid, ref = line.split(maxsplit=1)
            heads[ref] = oid
        require(len(heads) == 3, "source bundle must contain exactly three heads")
        require(set(heads.values()) == {BASE_SHA, UPSTREAM_SHA, MATERIALIZED_SHA}, "source bundle heads mismatch")
        for name, oid in (("base", BASE_SHA), ("upstream", UPSTREAM_SHA), ("materialized", MATERIALIZED_SHA)):
            ref = next(ref for ref, value in heads.items() if value == oid)
            run("git", "-C", str(repo), "fetch", str(bundle), f"{ref}:refs/w13825-source/{name}")
        require(run("git", "rev-parse", "refs/w13825-source/base^{tree}").strip() == BASE_TREE, "base tree mismatch")
        require(run("git", "rev-parse", "refs/w13825-source/upstream^{tree}").strip() == UPSTREAM_TREE, "upstream tree mismatch")
        require(run("git", "rev-parse", "refs/w13825-source/materialized^{tree}").strip() == MATERIALIZED_TREE, "materialized tree mismatch")
        require(run("git", "rev-parse", "refs/w13825-source/materialized^").strip() == BASE_SHA, "materialized parent mismatch")
        staged = sorted(line for line in staged_paths.read_text(encoding="utf-8").splitlines() if line)
        materialized_diff = run("git", "diff", "--name-only", BASE_SHA, MATERIALIZED_SHA).splitlines()
        require(materialized_diff == staged, "materialized source diff is not the accepted staged path set")

        source_paths = run("git", "diff", "--name-only", MATERIALIZED_SHA, UPSTREAM_SHA, "--", "sdk/").splitlines()
        require(len(source_paths) == 3 and all(path.startswith("sdk/") for path in source_paths), "source SDK diff must contain exactly three paths")
        representative = cohort.get("representative_paths", [])
        require(sorted(representative) == sorted(source_paths), "SDK cohort membership mismatch")
        classifications: dict[str, str] = {}
        for path in source_paths:
            base_entry = tree_entry(repo, BASE_SHA, path)
            materialized_entry = tree_entry(repo, MATERIALIZED_SHA, path)
            upstream_entry = tree_entry(repo, UPSTREAM_SHA, path)
            classifications[path] = classify(base_entry, materialized_entry, upstream_entry)
            require(materialized_entry == base_entry, f"materialized SDK entry is not base: {path}")
            require(classifications[path] in {"upstream", "delete"}, f"SDK path is not a selected change: {path}")

        index = temp / "index"
        index_env = {**os.environ, "GIT_INDEX_FILE": str(index)}
        run("git", "read-tree", MATERIALIZED_SHA, cwd=repo, env=index_env)
        for path, disposition in classifications.items():
            entry = tree_entry(repo, UPSTREAM_SHA, path)
            if disposition == "delete":
                run("git", "update-index", "--remove", "--", path, cwd=repo, env=index_env)
            else:
                require(entry is not None, f"missing upstream entry: {path}")
                mode, kind, oid = entry
                run("git", "update-index", "--add", "--cacheinfo", f"{mode},{oid},{path}", cwd=repo, env=index_env)
        changed = run("git", "diff", "--cached", "--name-only", cwd=repo, env=index_env).splitlines()
        require(changed == sorted(source_paths), "candidate index diff is not exactly the SDK selection")
        candidate_tree = run("git", "write-tree", cwd=repo, env=index_env).strip()
        env = {**os.environ, "GIT_AUTHOR_NAME": "github-actions[bot]", "GIT_AUTHOR_EMAIL": "41898282+github-actions[bot]@users.noreply.github.com", "GIT_COMMITTER_NAME": "github-actions[bot]", "GIT_COMMITTER_EMAIL": "41898282+github-actions[bot]@users.noreply.github.com"}
        candidate_sha = subprocess.run(["git", "commit-tree", candidate_tree, "-p", MATERIALIZED_SHA, "-m", "Apply admitted SDK upstream cohort"], cwd=repo, check=True, text=True, capture_output=True, env={**env, "GIT_INDEX_FILE": str(index)}).stdout.strip()
        candidate_paths = run("git", "diff", "--name-only", MATERIALIZED_SHA, candidate_sha).splitlines()
        require(candidate_paths == sorted(source_paths), "candidate diff escaped SDK selection")
        run("git", "update-ref", "refs/w13825-source/candidate", candidate_sha, cwd=repo)
        run("git", "bundle", "create", str(output / "candidate.bundle"), "refs/w13825-source/candidate", "refs/w13825-source/materialized", "refs/w13825-source/base", "refs/w13825-source/upstream", cwd=repo)

    current = {"repository": os.environ.get("GITHUB_REPOSITORY", "sednalabs/codex"), "workflow_head": os.environ.get("GITHUB_SHA", ""), "workflow_run_id": os.environ.get("GITHUB_RUN_ID", ""), "workflow_run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", "")}
    candidate_bundle_sha = digest(output / "candidate.bundle")
    emitted = {
        "schema": "upstream-cohort-candidate-provenance", "version": 1, "signed": False, **current,
        "base_sha": BASE_SHA, "base_tree": BASE_TREE, "upstream_sha": UPSTREAM_SHA, "upstream_tree": UPSTREAM_TREE,
        "materialized_sha": MATERIALIZED_SHA, "materialized_tree": MATERIALIZED_TREE, "candidate_sha": candidate_sha,
        "candidate_parent": MATERIALIZED_SHA, "candidate_tree": candidate_tree, "source_run_id": SOURCE_RUN_ID,
        "source_run_attempt": SOURCE_RUN_ATTEMPT, "source_artifact": SOURCE_ARTIFACT, "source_bundle_sha256": BUNDLE_SHA256,
        "source_receipt_sha256": SOURCE_RECEIPT_SHA256, "source_provenance_sha256": SOURCE_PROVENANCE_SHA256,
        "source_manifest_sha256": SOURCE_MANIFEST_SHA256, "sdk_paths": sorted(source_paths), "candidate_bundle_sha256": candidate_bundle_sha,
    }
    (output / "provenance.json").write_text(json.dumps(emitted, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (output / "receipt.json").write_text(json.dumps(emitted, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(emitted, sort_keys=True))


if __name__ == "__main__":
    main()
