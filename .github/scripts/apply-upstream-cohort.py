#!/usr/bin/env python3
"""Produce the first SDK-only hosted upstream cohort candidate.

Trusted code comes from the reviewed workflow checkout.  The accepted
composition artifact and the pinned SDK source commit are data inputs only.
Candidate construction selects exact Git objects path-by-path; it never
executes or applies source-supplied patches.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import subprocess
import tempfile
from typing import Any


REPOSITORY = "sednalabs/codex"
WORKFLOW_PATH = ".github/workflows/apply-upstream-cohort.yml"
VALIDATION_BRANCH = "worker/w13825-sdk-producer-validation-20260907"
VALIDATION_REF = f"refs/heads/{VALIDATION_BRANCH}"
PUSH_PREDECESSOR_SHA = "204fc215e54cba1fa409176d251423b1e31fa652"
BASE_SHA = "5eb6ca6519b1a79e8997bf21321885de1fd9ed01"
BASE_TREE = "7a4e9d32c7a13a22215335a850cf879e284fdc63"
GLOBAL_UPSTREAM_SHA = "008bbd5884122dc95aaece19ecfe0fc6a59dcf36"
GLOBAL_UPSTREAM_TREE = "721cd395f53962482b3f6d140d0b9942fef3baac"
MATERIALIZED_SHA = "f5bb378d2e575b8f6f3cf266a0939ef404c37203"
MATERIALIZED_TREE = "49af672a3965958bfb1668f27c0caa27ba48554a"
SDK_SOURCE_SHA = "bc8884624330b6e681cfa3ce5fc575ce8298ed1b"
SDK_SOURCE_TREE = "1e143e2bc5964a4308d9a6f36ca3e2af028e79e9"
SDK_SOURCE_BRANCH = "worker/w13825-sdk-source-authoring-20260906"
SOURCE_RUN_ID = "34035744523"
SOURCE_RUN_ATTEMPT = "1"
SOURCE_ARTIFACT = "upstream-composition-34035744523-1"
BUNDLE_SHA256 = "b383183cf21ade4b50244986cf1589988b248259ee51f099932bb0c06b026dd6"
SOURCE_RECEIPT_SHA256 = "2bcebca05cb45d6d2caad475ec5348a3883566f99e6a98d24196382d52d39e93"
SOURCE_MANIFEST_SHA256 = "0451d500a2a9868825337ddd0e6c16cd73c5088116131d75b4f27f801885328b"
SOURCE_PROVENANCE_SHA256 = "afbf269c8593c978ed706c9f2fddc0031383350fe216d88512ec3707c8a55cb9"
STAGED_PATCH_SHA256 = "dd4b59d9be8c2727d08de673085b36a1c61f6cee617855f210706412a5bfc66c"
STAGED_PATHS_SHA256 = "90b44134bb538a07fa03dfd674e96f08de4ba04a40252f6dc9f5c740dd5bb1ae"
STAGED_PATH_COUNT = 3516
UNRESOLVED_COUNT = 427
UNRESOLVED_PATHS_SHA256 = "7568a0be65dc7c05f591197a49b0b4e18f2c4435b951097d206cb37626233b62"
SDK_PATHS_SHA256 = "90eeb76b9ab63af38822f137a3afaff05fc06bb5b9032ab66948c389fc6d68a9"
SDK_PATHS = [
    "sdk/python/scripts/update_sdk_artifacts.py",
    "sdk/python/tests/test_client_rpc_methods.py",
    "sdk/typescript/package.json",
]
EXPECTED_COHORTS = {
    "build-ci-generated": (18, "528e950cda3fb131260c92a73c241128d02ccd24c736e2e7d4d1c5cc3bfe8529"),
    "sdk-public-contract": (3, SDK_PATHS_SHA256),
    "app-server-protocol-transport": (90, "4607ed2688c24a7bd4a50ba30e9e63c8f0b8cfbafdb1f490130d84ea388447ce"),
    "tui-history": (100, "a143f2df1c8dd2e4276157eb76975f9faee89fdc217c2cef5d41cb67876af4aa"),
    "core-agent-lifecycle": (215, "e381c18fe231617017a5f63ecf3f12e93d0f0578304d9e3d4e5ed81e5ffb0f99"),
    "docs-policy": (1, "e1339363ffc514d971d191ea37947a46a31cc4972cfd23c269378104408c7b62"),
    "unclassified-user-judgment": (0, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
}
EXPECTED_SOURCE_HEADS = {
    f"refs/w13825-materialized-{SOURCE_RUN_ID}-{SOURCE_RUN_ATTEMPT}/base": BASE_SHA,
    f"refs/w13825-materialized-{SOURCE_RUN_ID}-{SOURCE_RUN_ATTEMPT}/materialized": MATERIALIZED_SHA,
    f"refs/w13825-materialized-{SOURCE_RUN_ID}-{SOURCE_RUN_ATTEMPT}/upstream": GLOBAL_UPSTREAM_SHA,
}
EXPECTED_SUMMARY = (
    f"MANIFEST_TOTAL={UNRESOLVED_COUNT}\n"
    f"MANIFEST_DIGEST={UNRESOLVED_PATHS_SHA256}\n"
    f"STAGED_AUTO_MERGE_TOTAL={STAGED_PATH_COUNT}\n"
    f"STAGED_AUTO_MERGE_DIGEST={STAGED_PATHS_SHA256}\n"
    f"STAGED_AUTO_MERGE_PATCH_DIGEST={STAGED_PATCH_SHA256}\n"
)
SHA_PATTERN = re.compile(r"[0-9a-f]{40}")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(message)


def run(
    *args: str,
    cwd: pathlib.Path | None = None,
    env: dict[str, str] | None = None,
) -> str:
    return subprocess.run(
        args,
        cwd=cwd,
        env=env,
        check=True,
        text=True,
        capture_output=True,
    ).stdout


def run_bytes(*args: str, cwd: pathlib.Path | None = None) -> bytes:
    return subprocess.run(args, cwd=cwd, check=True, capture_output=True).stdout


def sanitize_git_stderr(stderr: str, *private_paths: pathlib.Path) -> str:
    replacements = sorted((str(path) for path in private_paths), key=len, reverse=True)
    diagnostics: list[str] = []
    omitted = False
    for raw_line in stderr.splitlines():
        line = raw_line
        for value in replacements:
            line = line.replace(value, "<path>")
        line = re.sub(r"https?://\S+", "<redacted-url>", line)
        line = re.sub(r"\b(?:gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+)\b", "<redacted-token>", line)
        line = re.sub(
            r"(?i)(authorization|credential|password|token)(\s*[:=]\s*)\S+",
            r"\1\2<redacted>",
            line,
        )
        line = "".join(character for character in line if character.isprintable())
        if re.match(r"^(fatal|error|warning|hint):", line) is None:
            omitted = True
            continue
        if len(diagnostics) == 8:
            omitted = True
            break
        diagnostics.append(line[:240])
        omitted = omitted or len(line) > 240
    if omitted:
        diagnostics.append("<additional Git stderr omitted>")
    return "\n".join(diagnostics) or "<no sanitized Git diagnostic>"


def fetch_bundle_ref(bare: pathlib.Path, bundle: pathlib.Path, source_ref: str, target_ref: str) -> None:
    result = subprocess.run(
        ("git", "-C", str(bare), "fetch", str(bundle), f"+{source_ref}:{target_ref}"),
        check=False,
        text=True,
        capture_output=True,
    )
    if result.returncode != 0:
        diagnostic = sanitize_git_stderr(result.stderr, bare, bundle, bundle.parent)
        raise SystemExit(f"candidate bundle import failed (git exit {result.returncode})\n{diagnostic}")


def digest(path: pathlib.Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def load(path: pathlib.Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as stream:
        result = json.load(stream)
    require(isinstance(result, dict), f"{path.name} must contain an object")
    return result


def path_digest(paths: list[str]) -> str:
    data = ("\n".join(paths) + ("\n" if paths else "")).encode()
    return hashlib.sha256(data).hexdigest()


def absolute_argument(path: pathlib.Path, name: str, *, must_exist: bool) -> pathlib.Path:
    require(path.is_absolute(), f"{name} must be absolute")
    return path.resolve(strict=must_exist)


def tuple_json(entry: tuple[str, str, str] | None) -> dict[str, str] | None:
    if entry is None:
        return None
    mode, object_type, oid = entry
    return {"mode": mode, "type": object_type, "oid": oid}


def tree_entry(
    repo: pathlib.Path,
    revision: str,
    path: str,
) -> tuple[str, str, str] | None:
    output = run_bytes("git", "ls-tree", "--full-tree", "-z", revision, "--", path, cwd=repo)
    if not output:
        return None
    require(output.endswith(b"\0") and output.count(b"\0") == 1, f"ambiguous tree entry: {path}")
    metadata, listed = output[:-1].split(b"\t", 1)
    mode, object_type, oid = metadata.decode("ascii").split()
    require(listed.decode("utf-8", "surrogateescape") == path, f"unexpected tree path: {path}")
    require(mode in {"100644", "100755", "120000", "160000"}, f"unsupported Git mode for {path}: {mode}")
    require(object_type == ("commit" if mode == "160000" else "blob"), f"mode/type mismatch for {path}")
    require(SHA_PATTERN.fullmatch(oid) is not None, f"invalid object ID for {path}")
    return mode, object_type, oid


def classify(
    base: tuple[str, str, str] | None,
    upstream: tuple[str, str, str] | None,
    source: tuple[str, str, str] | None,
) -> str:
    if source == base:
        return "base"
    if source == upstream:
        return "upstream"
    if source is None:
        return "delete"
    return "manual"


def require_fields(actual: dict[str, Any], expected: dict[str, Any], label: str) -> None:
    for key, value in expected.items():
        require(str(actual.get(key)) == str(value), f"{label} mismatch: {key}")


def load_event() -> dict[str, Any]:
    event_path = pathlib.Path(os.environ.get("GITHUB_EVENT_PATH", ""))
    require(event_path.is_absolute() and event_path.is_file(), "invalid producer event path")
    event = json.loads(event_path.read_text(encoding="utf-8"))
    require(isinstance(event, dict), "producer event must be an object")
    return event


def verify_runtime(expected_workflow_sha: str, expected_workflow_tree: str) -> dict[str, str]:
    require(os.environ.get("GITHUB_REPOSITORY") == REPOSITORY, "current repository mismatch")
    event_name = os.environ.get("GITHUB_EVENT_NAME")
    require(event_name in {"push", "workflow_dispatch"}, "current event mismatch")
    require(os.environ.get("GITHUB_REF") == VALIDATION_REF, "current ref mismatch")
    require(SHA_PATTERN.fullmatch(expected_workflow_sha) is not None, "invalid expected workflow SHA")
    require(SHA_PATTERN.fullmatch(expected_workflow_tree) is not None, "invalid expected workflow tree")
    require(os.environ.get("GITHUB_SHA") == expected_workflow_sha, "workflow run SHA mismatch")
    for name in (
        "GITHUB_REPOSITORY_ID",
        "GITHUB_RUN_ID",
        "GITHUB_RUN_ATTEMPT",
        "GITHUB_WORKFLOW_REF",
    ):
        require(bool(os.environ.get(name)), f"missing producer identity: {name}")
    expected_workflow_ref = f"{REPOSITORY}/{WORKFLOW_PATH}@{VALIDATION_REF}"
    require(os.environ["GITHUB_WORKFLOW_REF"] == expected_workflow_ref, "producer workflow ref mismatch")

    event = load_event()
    repository = event.get("repository")
    require(isinstance(repository, dict) and repository.get("full_name") == REPOSITORY, "event repository mismatch")
    requested_sha = os.environ.get("REQUESTED_WORKFLOW_SHA", "")
    requested_tree = os.environ.get("REQUESTED_WORKFLOW_TREE", "")
    if event_name == "workflow_dispatch":
        inputs = event.get("inputs")
        require(
            isinstance(inputs, dict) and set(inputs) == {"expected_workflow_sha", "expected_workflow_tree"},
            "workflow dispatch input set mismatch",
        )
        require(inputs["expected_workflow_sha"] == requested_sha, "workflow dispatch SHA receipt mismatch")
        require(inputs["expected_workflow_tree"] == requested_tree, "workflow dispatch tree receipt mismatch")
        require(requested_sha == expected_workflow_sha, "workflow dispatch SHA does not match run checkout")
        require(requested_tree == expected_workflow_tree, "workflow dispatch tree does not match run checkout")
    else:
        require(not requested_sha and not requested_tree, "push event must not supply workflow dispatch inputs")
        require(event.get("ref") == VALIDATION_REF, "push event ref mismatch")
        require(event.get("before") == PUSH_PREDECESSOR_SHA, "push predecessor mismatch")
        require(event.get("after") == expected_workflow_sha, "push target mismatch")
        require(event.get("forced") is False, "push event must not be forced")
        require(event.get("deleted") is False, "push event must not delete ref")
        require(event.get("created") is False, "push event must update the existing ref")
        head_commit = event.get("head_commit")
        require(
            isinstance(head_commit, dict) and head_commit.get("id") == expected_workflow_sha,
            "push head commit mismatch",
        )
    return {
        "producer_repository": REPOSITORY,
        "producer_repository_id": os.environ["GITHUB_REPOSITORY_ID"],
        "producer_workflow_head": expected_workflow_sha,
        "producer_workflow_tree": expected_workflow_tree,
        "producer_workflow_ref": os.environ["GITHUB_WORKFLOW_REF"],
        "producer_run_id": os.environ["GITHUB_RUN_ID"],
        "producer_run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
        "producer_event": os.environ["GITHUB_EVENT_NAME"],
    }


def verify_source_checkout(repo: pathlib.Path) -> None:
    require(run("git", "rev-parse", "HEAD", cwd=repo).strip() == SDK_SOURCE_SHA, "SDK source head mismatch")
    require(run("git", "rev-parse", "HEAD^{tree}", cwd=repo).strip() == SDK_SOURCE_TREE, "SDK source tree mismatch")
    parents = run("git", "show", "-s", "--format=%P", SDK_SOURCE_SHA, cwd=repo).split()
    require(parents == [BASE_SHA], "SDK source must have the exact frozen base as its sole parent")
    require(not run("git", "status", "--porcelain", cwd=repo), "SDK source checkout is dirty")


def verify_source_evidence(artifact: pathlib.Path) -> dict[str, pathlib.Path]:
    names = {
        "bundle": "materialized.bundle",
        "receipt": "receipt.json",
        "provenance": "provenance.json",
        "manifest": "conflict-manifest.json",
        "staged_patch": "staged-auto-merge.patch",
        "staged_paths": "staged-auto-merge.txt",
        "unmerged_paths": "unmerged.txt",
        "summary": "manifest-summary.env",
    }
    files: dict[str, pathlib.Path] = {}
    for key, name in names.items():
        path = (artifact / name).resolve(strict=True)
        require(path.is_file() and path.stat().st_size > 0, f"missing source evidence: {name}")
        files[key] = path

    # Check small evidence first so negative controls fail before hashing the bundle.
    require(digest(files["receipt"]) == SOURCE_RECEIPT_SHA256, "source receipt digest mismatch")
    require(digest(files["provenance"]) == SOURCE_PROVENANCE_SHA256, "source provenance digest mismatch")
    require(digest(files["manifest"]) == SOURCE_MANIFEST_SHA256, "source manifest digest mismatch")
    require(digest(files["staged_patch"]) == STAGED_PATCH_SHA256, "staged patch digest mismatch")
    require(digest(files["staged_paths"]) == STAGED_PATHS_SHA256, "staged path-list digest mismatch")
    require(digest(files["unmerged_paths"]) == UNRESOLVED_PATHS_SHA256, "unmerged path-list digest mismatch")
    require(files["summary"].read_text(encoding="utf-8") == EXPECTED_SUMMARY, "manifest summary mismatch")
    require(digest(files["bundle"]) == BUNDLE_SHA256, "source bundle digest mismatch")

    receipt = load(files["receipt"])
    provenance = load(files["provenance"])
    manifest = load(files["manifest"])
    require(provenance.get("signed") is False, "source provenance must remain explicitly unsigned")
    require_fields(
        provenance,
        {
            "repository": REPOSITORY,
            "workflow_head": BASE_SHA,
            "base_sha": BASE_SHA,
            "base_tree": BASE_TREE,
            "upstream_sha": GLOBAL_UPSTREAM_SHA,
            "upstream_tree": GLOBAL_UPSTREAM_TREE,
            "materialized_sha": MATERIALIZED_SHA,
            "materialized_tree": MATERIALIZED_TREE,
            "run_id": SOURCE_RUN_ID,
            "run_attempt": SOURCE_RUN_ATTEMPT,
            "bundle_sha256": BUNDLE_SHA256,
            "receipt_sha256": SOURCE_RECEIPT_SHA256,
            "conflict_manifest_sha256": SOURCE_MANIFEST_SHA256,
            "staged_auto_merge_patch_sha256": STAGED_PATCH_SHA256,
            "staged_auto_merge_path_list_sha256": STAGED_PATHS_SHA256,
        },
        "source provenance",
    )
    require_fields(
        receipt,
        {
            "repository": REPOSITORY,
            "expected_head": BASE_SHA,
            "base_sha": BASE_SHA,
            "base_tree": BASE_TREE,
            "upstream_sha": GLOBAL_UPSTREAM_SHA,
            "upstream_tree": GLOBAL_UPSTREAM_TREE,
            "run_id": SOURCE_RUN_ID,
            "run_attempt": SOURCE_RUN_ATTEMPT,
            "pre_conflict_materialized_sha": MATERIALIZED_SHA,
            "pre_conflict_materialized_tree": MATERIALIZED_TREE,
            "total_unmerged_path_count": UNRESOLVED_COUNT,
            "unmerged_path_set_sha256": UNRESOLVED_PATHS_SHA256,
            "staged_auto_merge_patch_sha256": STAGED_PATCH_SHA256,
            "staged_auto_merge_path_count": STAGED_PATH_COUNT,
            "staged_auto_merge_path_set_sha256": STAGED_PATHS_SHA256,
        },
        "source receipt",
    )
    require(receipt.get("conflict") is True, "source receipt must record a conflict")
    require_fields(
        manifest,
        {
            "schema": "hosted-conflict-manifest",
            "version": 1,
            "repository": REPOSITORY,
            "expected_head": BASE_SHA,
            "run_id": SOURCE_RUN_ID,
            "run_attempt": SOURCE_RUN_ATTEMPT,
            "base_sha": BASE_SHA,
            "base_tree": BASE_TREE,
            "upstream_sha": GLOBAL_UPSTREAM_SHA,
            "upstream_tree": GLOBAL_UPSTREAM_TREE,
            "total_unmerged_path_count": UNRESOLVED_COUNT,
            "path_set_sha256": UNRESOLVED_PATHS_SHA256,
            "staged_auto_merge_patch_sha256": STAGED_PATCH_SHA256,
            "staged_auto_merge_path_count": STAGED_PATH_COUNT,
            "staged_auto_merge_path_set_sha256": STAGED_PATHS_SHA256,
        },
        "source manifest",
    )
    cohorts = manifest.get("cohorts")
    require(isinstance(cohorts, list), "source manifest cohorts must be a list")
    cohort_map: dict[str, tuple[int, str]] = {}
    for item in cohorts:
        require(isinstance(item, dict) and isinstance(item.get("name"), str), "invalid source cohort")
        name = item["name"]
        require(name not in cohort_map, f"duplicate source cohort: {name}")
        cohort_map[name] = (int(item.get("path_count", -1)), str(item.get("path_set_sha256", "")))
    require(cohort_map == EXPECTED_COHORTS, "source cohort count/hash map mismatch")
    require(sum(count for count, _ in cohort_map.values()) == UNRESOLVED_COUNT, "source cohort counts do not sum to 427")
    sdk = next(item for item in cohorts if item["name"] == "sdk-public-contract")
    require(sorted(sdk.get("representative_paths", [])) == SDK_PATHS, "SDK cohort paths mismatch")
    unresolved = sorted(line for line in files["unmerged_paths"].read_text(encoding="utf-8").splitlines() if line)
    require(len(unresolved) == UNRESOLVED_COUNT and path_digest(unresolved) == UNRESOLVED_PATHS_SHA256, "unmerged path list is not canonical")
    require(all(path in unresolved for path in SDK_PATHS), "SDK cohort path missing from unmerged list")
    return files


def import_source_bundle(repo: pathlib.Path, bundle: pathlib.Path, temp: pathlib.Path) -> None:
    bare = temp / "verify-source.git"
    run("git", "init", "--bare", str(bare))
    run("git", "-C", str(bare), "bundle", "verify", str(bundle))
    heads: dict[str, str] = {}
    for line in run("git", "-C", str(bare), "bundle", "list-heads", str(bundle)).splitlines():
        oid, ref = line.split(maxsplit=1)
        require(ref not in heads, f"duplicate source bundle ref: {ref}")
        heads[ref] = oid
    require(heads == EXPECTED_SOURCE_HEADS, "source bundle head map mismatch")
    for ref, target in (
        (next(ref for ref, oid in heads.items() if oid == BASE_SHA), "base"),
        (next(ref for ref, oid in heads.items() if oid == GLOBAL_UPSTREAM_SHA), "upstream"),
        (next(ref for ref, oid in heads.items() if oid == MATERIALIZED_SHA), "materialized"),
    ):
        run("git", "fetch", str(bundle), f"+{ref}:refs/w13825-source/{target}", cwd=repo)
    require(run("git", "rev-parse", "refs/w13825-source/base", cwd=repo).strip() == BASE_SHA, "imported base mismatch")
    require(run("git", "rev-parse", "refs/w13825-source/base^{tree}", cwd=repo).strip() == BASE_TREE, "imported base tree mismatch")
    require(run("git", "rev-parse", "refs/w13825-source/upstream", cwd=repo).strip() == GLOBAL_UPSTREAM_SHA, "imported upstream mismatch")
    require(run("git", "rev-parse", "refs/w13825-source/upstream^{tree}", cwd=repo).strip() == GLOBAL_UPSTREAM_TREE, "imported upstream tree mismatch")
    require(run("git", "rev-parse", "refs/w13825-source/materialized", cwd=repo).strip() == MATERIALIZED_SHA, "imported materialized mismatch")
    require(run("git", "rev-parse", "refs/w13825-source/materialized^{tree}", cwd=repo).strip() == MATERIALIZED_TREE, "imported materialized tree mismatch")
    require(run("git", "show", "-s", "--format=%P", MATERIALIZED_SHA, cwd=repo).split() == [BASE_SHA], "materialized parent mismatch")


def verify_emitted_bundle(
    repo: pathlib.Path,
    bundle: pathlib.Path,
    expected_heads: dict[str, str],
    candidate_sha: str,
    candidate_tree: str,
    temp: pathlib.Path,
) -> None:
    bare = temp / "verify-candidate.git"
    run("git", "init", "--bare", str(bare))
    run("git", "-C", str(bare), "bundle", "verify", str(bundle))
    actual: dict[str, str] = {}
    for line in run("git", "-C", str(bare), "bundle", "list-heads", str(bundle)).splitlines():
        oid, ref = line.split(maxsplit=1)
        actual[ref] = oid
    require(actual == expected_heads, "candidate bundle head map mismatch")
    candidate_ref = next(ref for ref, oid in actual.items() if oid == candidate_sha)
    fetch_bundle_ref(bare, bundle, candidate_ref, "refs/import/candidate")
    require(run("git", "-C", str(bare), "rev-parse", "refs/import/candidate^{tree}").strip() == candidate_tree, "candidate bundle tree mismatch")
    require(run("git", "-C", str(bare), "show", "-s", "--format=%P", "refs/import/candidate").split() == [MATERIALIZED_SHA], "candidate bundle parent mismatch")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", type=pathlib.Path)
    parser.add_argument("--artifact-dir", type=pathlib.Path)
    parser.add_argument("--output-dir", type=pathlib.Path)
    parser.add_argument("--expected-workflow-sha", required=True)
    parser.add_argument("--expected-workflow-tree", required=True)
    parser.add_argument("--validate-runtime-only", action="store_true")
    args = parser.parse_args()

    runtime = verify_runtime(args.expected_workflow_sha, args.expected_workflow_tree)
    if args.validate_runtime_only:
        require(
            args.repo_root is None and args.artifact_dir is None and args.output_dir is None,
            "runtime-only validation does not accept producer paths",
        )
        print(json.dumps(runtime, sort_keys=True))
        return
    require(
        args.repo_root is not None and args.artifact_dir is not None and args.output_dir is not None,
        "producer paths are required",
    )
    repo = absolute_argument(args.repo_root, "repo-root", must_exist=True)
    artifact = absolute_argument(args.artifact_dir, "artifact-dir", must_exist=True)
    output = absolute_argument(args.output_dir, "output-dir", must_exist=False)
    require(repo.is_dir() and artifact.is_dir(), "repository and artifact inputs must be directories")
    require(output.parent.is_dir(), "output parent must already exist")
    require(not output.exists(), "output directory already exists")
    verify_source_checkout(repo)
    files = verify_source_evidence(artifact)
    output.mkdir()

    with tempfile.TemporaryDirectory(prefix="w13825-sdk-", dir=str(output.parent)) as temp_name:
        temp = pathlib.Path(temp_name).resolve(strict=True)
        import_source_bundle(repo, files["bundle"], temp)

        source_paths = run("git", "diff-tree", "--no-commit-id", "--name-only", "-r", BASE_SHA, SDK_SOURCE_SHA, cwd=repo).splitlines()
        require(source_paths == SDK_PATHS, "SDK source diff does not equal the exact admitted cohort")
        require(path_digest(source_paths) == SDK_PATHS_SHA256, "SDK source path-set digest mismatch")

        dispositions: list[dict[str, Any]] = []
        index = temp / "candidate.index"
        index_env = {**os.environ, "GIT_INDEX_FILE": str(index)}
        run("git", "read-tree", MATERIALIZED_SHA, cwd=repo, env=index_env)
        expected_changed: list[str] = []
        for path in SDK_PATHS:
            base_entry = tree_entry(repo, BASE_SHA, path)
            upstream_entry = tree_entry(repo, GLOBAL_UPSTREAM_SHA, path)
            source_entry = tree_entry(repo, SDK_SOURCE_SHA, path)
            materialized_entry = tree_entry(repo, MATERIALIZED_SHA, path)
            require(materialized_entry == base_entry, f"materialized SDK entry is not the frozen base: {path}")
            disposition = classify(base_entry, upstream_entry, source_entry)
            selected_entry = source_entry
            if selected_entry is None:
                run("git", "update-index", "--force-remove", "--", path, cwd=repo, env=index_env)
            else:
                mode, _object_type, oid = selected_entry
                run("git", "update-index", "--add", "--cacheinfo", f"{mode},{oid},{path}", cwd=repo, env=index_env)
            if selected_entry != materialized_entry:
                expected_changed.append(path)
            dispositions.append(
                {
                    "path": path,
                    "disposition": disposition,
                    "base": tuple_json(base_entry),
                    "upstream": tuple_json(upstream_entry),
                    "source": tuple_json(source_entry),
                    "materialized": tuple_json(materialized_entry),
                    "selected": tuple_json(selected_entry),
                    "source_equals_base": source_entry == base_entry,
                    "source_equals_upstream": source_entry == upstream_entry,
                    "materialized_equals_base": materialized_entry == base_entry,
                }
            )

        changed = run("git", "diff", "--cached", "--name-only", MATERIALIZED_SHA, cwd=repo, env=index_env).splitlines()
        require(changed == expected_changed == SDK_PATHS, "candidate index diff is not exactly the SDK cohort")
        candidate_tree = run("git", "write-tree", cwd=repo, env=index_env).strip()
        commit_env = {
            **os.environ,
            "GIT_AUTHOR_NAME": "github-actions[bot]",
            "GIT_AUTHOR_EMAIL": "41898282+github-actions[bot]@users.noreply.github.com",
            "GIT_COMMITTER_NAME": "github-actions[bot]",
            "GIT_COMMITTER_EMAIL": "41898282+github-actions[bot]@users.noreply.github.com",
        }
        candidate_sha = run(
            "git",
            "commit-tree",
            candidate_tree,
            "-p",
            MATERIALIZED_SHA,
            "-m",
            "Apply admitted SDK upstream cohort",
            cwd=repo,
            env=commit_env,
        ).strip()
        require(run("git", "show", "-s", "--format=%P", candidate_sha, cwd=repo).split() == [MATERIALIZED_SHA], "candidate parent mismatch")
        candidate_paths = run("git", "diff", "--name-only", MATERIALIZED_SHA, candidate_sha, cwd=repo).splitlines()
        require(candidate_paths == SDK_PATHS, "candidate diff escaped the SDK cohort")
        for disposition in dispositions:
            require(
                tree_entry(repo, candidate_sha, disposition["path"]) == tree_entry(repo, SDK_SOURCE_SHA, disposition["path"]),
                f"candidate/source entry mismatch: {disposition['path']}",
            )

        prefix = f"refs/w13825-sdk-{runtime['producer_run_id']}-{runtime['producer_run_attempt']}"
        emitted_heads = {
            f"{prefix}/base": BASE_SHA,
            f"{prefix}/candidate": candidate_sha,
            f"{prefix}/materialized": MATERIALIZED_SHA,
            f"{prefix}/source": SDK_SOURCE_SHA,
            f"{prefix}/upstream": GLOBAL_UPSTREAM_SHA,
        }
        for ref, oid in emitted_heads.items():
            run("git", "update-ref", ref, oid, cwd=repo)
        candidate_bundle = (output / "candidate.bundle").resolve()
        run("git", "bundle", "create", str(candidate_bundle), *sorted(emitted_heads), cwd=repo)
        verify_emitted_bundle(repo, candidate_bundle, emitted_heads, candidate_sha, candidate_tree, temp)

        receipt = {
            "schema": "upstream-cohort-disposition",
            "version": 2,
            "repository": REPOSITORY,
            "cohort": "sdk-public-contract",
            "path_count": len(SDK_PATHS),
            "path_set_sha256": SDK_PATHS_SHA256,
            "base_sha": BASE_SHA,
            "base_tree": BASE_TREE,
            "upstream_sha": GLOBAL_UPSTREAM_SHA,
            "upstream_tree": GLOBAL_UPSTREAM_TREE,
            "materialized_sha": MATERIALIZED_SHA,
            "materialized_tree": MATERIALIZED_TREE,
            "source_branch": SDK_SOURCE_BRANCH,
            "source_sha": SDK_SOURCE_SHA,
            "source_tree": SDK_SOURCE_TREE,
            "source_parent": BASE_SHA,
            "candidate_sha": candidate_sha,
            "candidate_tree": candidate_tree,
            "candidate_parent": MATERIALIZED_SHA,
            "paths": dispositions,
        }
        receipt_path = output / "receipt.json"
        receipt_path.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        provenance = {
            "schema": "upstream-cohort-candidate-provenance",
            "version": 2,
            "signed": False,
            **runtime,
            "source_repository": REPOSITORY,
            "source_run_id": SOURCE_RUN_ID,
            "source_run_attempt": SOURCE_RUN_ATTEMPT,
            "source_artifact": SOURCE_ARTIFACT,
            "source_bundle_sha256": BUNDLE_SHA256,
            "source_receipt_sha256": SOURCE_RECEIPT_SHA256,
            "source_provenance_sha256": SOURCE_PROVENANCE_SHA256,
            "source_manifest_sha256": SOURCE_MANIFEST_SHA256,
            "source_staged_patch_sha256": STAGED_PATCH_SHA256,
            "source_staged_paths_sha256": STAGED_PATHS_SHA256,
            "base_sha": BASE_SHA,
            "base_tree": BASE_TREE,
            "upstream_sha": GLOBAL_UPSTREAM_SHA,
            "upstream_tree": GLOBAL_UPSTREAM_TREE,
            "materialized_sha": MATERIALIZED_SHA,
            "materialized_tree": MATERIALIZED_TREE,
            "sdk_source_branch": SDK_SOURCE_BRANCH,
            "sdk_source_sha": SDK_SOURCE_SHA,
            "sdk_source_tree": SDK_SOURCE_TREE,
            "sdk_source_parent": BASE_SHA,
            "cohort": "sdk-public-contract",
            "path_count": len(SDK_PATHS),
            "path_set_sha256": SDK_PATHS_SHA256,
            "candidate_sha": candidate_sha,
            "candidate_tree": candidate_tree,
            "candidate_parent": MATERIALIZED_SHA,
            "candidate_bundle_heads": emitted_heads,
            "candidate_bundle_sha256": digest(candidate_bundle),
            "disposition_receipt_sha256": digest(receipt_path),
        }
        provenance_path = output / "provenance.json"
        provenance_path.write_text(json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        require(load(receipt_path) == receipt, "emitted disposition receipt readback mismatch")
        require(load(provenance_path) == provenance, "emitted provenance readback mismatch")

    print(json.dumps(provenance, sort_keys=True))


if __name__ == "__main__":
    main()
