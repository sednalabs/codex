#!/usr/bin/env python3
"""Build the exact hosted SDK/build consumer candidate for w13825.

Trusted code comes from the reviewed workflow checkout.  The accepted SDK
artifact and reviewed build-source commit are immutable data inputs.  The
consumer verifies every input tuple, retains the three already-materialized
patch dependencies, applies only eight reviewed source entries, regenerates
up to five coupled outputs with repository-pinned tools, and emits a
fresh-bare-verified Git bundle without publishing a ref.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import tomllib
from typing import Any


REPOSITORY = "sednalabs/codex"
REPOSITORY_ID = "1152496647"
WORKFLOW_PATH = ".github/workflows/apply-upstream-cohort.yml"
VALIDATION_BRANCH = "worker/w13825-sdk-build-consumer"
VALIDATION_REF = f"refs/heads/{VALIDATION_BRANCH}"
PUSH_PREDECESSOR_SHA = "d66a32521e38a51d9a2917d8378795667606f48c"

BASE_SHA = "5eb6ca6519b1a79e8997bf21321885de1fd9ed01"
BASE_TREE = "7a4e9d32c7a13a22215335a850cf879e284fdc63"
GLOBAL_UPSTREAM_SHA = "008bbd5884122dc95aaece19ecfe0fc6a59dcf36"
GLOBAL_UPSTREAM_TREE = "721cd395f53962482b3f6d140d0b9942fef3baac"
MATERIALIZED_SHA = "f5bb378d2e575b8f6f3cf266a0939ef404c37203"
MATERIALIZED_TREE = "49af672a3965958bfb1668f27c0caa27ba48554a"

SDK_SOURCE_SHA = "bc8884624330b6e681cfa3ce5fc575ce8298ed1b"
SDK_SOURCE_TREE = "1e143e2bc5964a4308d9a6f36ca3e2af028e79e9"
SDK_SOURCE_BRANCH = "worker/w13825-sdk-source-authoring-20260906"
SDK_SOURCE_PATHS_SHA256 = "90eeb76b9ab63af38822f137a3afaff05fc06bb5b9032ab66948c389fc6d68a9"
SDK_SOURCE_PATHS = [
    "sdk/python/scripts/update_sdk_artifacts.py",
    "sdk/python/tests/test_client_rpc_methods.py",
    "sdk/typescript/package.json",
]

SDK_INPUT_ARTIFACT_ID = "9992014028"
SDK_INPUT_ARTIFACT_NAME = "upstream-sdk-candidate-34042133059-1"
SDK_INPUT_ARTIFACT_SIZE = 451611604
SDK_INPUT_ARCHIVE_SHA256 = "bce7300bd77efe798a463d8006b4291049bfaff5b996c878853ebab95087d433"
SDK_INPUT_RUN_ID = "34042133059"
SDK_INPUT_RUN_ATTEMPT = "1"
SDK_INPUT_WORKFLOW_SHA = "0384d8ae9205e142fa4b352256d8c85a9e05f8c4"
SDK_INPUT_WORKFLOW_TREE = "636a1c9d84d2adec2d063ad932565b96a0dff7e6"
SDK_INPUT_WORKFLOW_REF = (
    "sednalabs/codex/.github/workflows/apply-upstream-cohort.yml@"
    "refs/heads/worker/w13825-sdk-producer-validation-20260907"
)
SDK_INPUT_BUNDLE_SHA256 = "5dd47aa65221838c8f6d625d83de78bbba101001157866eb5abb1270dae2425c"
SDK_INPUT_RECEIPT_SHA256 = "811992f09b22f610b8ab01983a01da0950ed090931813c67e91f4983d0ca82a6"
SDK_CANDIDATE_SHA = "3a26f7dad12e96ea41dae025e77472af0dd273a8"
SDK_CANDIDATE_TREE = "6867e9e14ea8f416ee3075f959b880d038fe2cc0"
SDK_CANDIDATE_PARENT = MATERIALIZED_SHA

COMMON_SOURCE_RUN_ID = "34035744523"
COMMON_SOURCE_RUN_ATTEMPT = "1"
COMMON_SOURCE_ARTIFACT = "upstream-composition-34035744523-1"
COMMON_BUNDLE_SHA256 = "b383183cf21ade4b50244986cf1589988b248259ee51f099932bb0c06b026dd6"
COMMON_RECEIPT_SHA256 = "2bcebca05cb45d6d2caad475ec5348a3883566f99e6a98d24196382d52d39e93"
COMMON_MANIFEST_SHA256 = "0451d500a2a9868825337ddd0e6c16cd73c5088116131d75b4f27f801885328b"
COMMON_PROVENANCE_SHA256 = "afbf269c8593c978ed706c9f2fddc0031383350fe216d88512ec3707c8a55cb9"
COMMON_STAGED_PATCH_SHA256 = "dd4b59d9be8c2727d08de673085b36a1c61f6cee617855f210706412a5bfc66c"
COMMON_STAGED_PATHS_SHA256 = "90b44134bb538a07fa03dfd674e96f08de4ba04a40252f6dc9f5c740dd5bb1ae"

BUILD_SOURCE_SHA = "54bcd76de2c0c30d655c99faf2e2c9cab271e18b"
BUILD_SOURCE_TREE = "6adcbddd3dde655a77a385fc75d5af9d39f90802"
BUILD_SOURCE_PARENT = BASE_SHA
BUILD_SOURCE_BRANCH = "worker/w13825-build-source-authoring-20260907"
BUILD_PATHS_SHA256 = "fdf7ee7203da4bd7b0d1ddb5ff9d7e0278e0dc0374d6776006cc67dba5460c23"
BUILD_SOURCE_ENTRIES: dict[str, tuple[str, str, str]] = {
    ".github/workflows/bazel.yml": (
        "100644",
        "blob",
        "2f17836a13cabd85309cad454fa999390f9b3a3f",
    ),
    ".github/workflows/blob-size-policy.yml": (
        "100644",
        "blob",
        "51ab52110f3ce388caa37ea8a1bf6fc8773dc92b",
    ),
    ".github/workflows/rust-ci-full.yml": (
        "100644",
        "blob",
        "0c96ec9a62fad00e0c129b2af6ebf2905b4fe9b4",
    ),
    ".github/workflows/rust-ci.yml": (
        "100644",
        "blob",
        "b7815de8d48740b63651290d939e0d8050c365d1",
    ),
    ".github/workflows/v8-canary.yml": (
        "100644",
        "blob",
        "b946a6f84e850e5b0e5e724accd70b3c2c26f7b0",
    ),
    "MODULE.bazel": (
        "100644",
        "blob",
        "647b8edfd1a5bd947106fb64a13261a461bfeca2",
    ),
    "codex-rs/realtime-webrtc/BUILD.bazel": (
        "100644",
        "blob",
        "d9cfeb6cfaf7b7c40e7648f8547b7785c284cc28",
    ),
    "patches/BUILD.bazel": (
        "100644",
        "blob",
        "075b8e30d98baabca4ff60f0b2649d1813ce83d1",
    ),
}
BUILD_PATHS = list(BUILD_SOURCE_ENTRIES)

PATCH_DEPENDENCIES: dict[str, tuple[str, str, str]] = {
    "patches/rules_rs_windows_msvc_linker.patch": (
        "100644",
        "blob",
        "66feb78569348668ee0f4fce86c7b50276fc097d",
    ),
    "patches/rules_rs_zlib_snapshot_urls.patch": (
        "100644",
        "blob",
        "fffbca8fcd265a4c65f911a07f26434ca4f47188",
    ),
    "patches/rules_rust_windows_msvc_direct_link_args.patch": (
        "100644",
        "blob",
        "aa5fb274e1d5e7b473771cf71183c132a80e1b36",
    ),
}

COMPOSITE_RUNTIME_INPUTS: dict[str, tuple[str, str, str]] = {
    "sdk/python/pyproject.toml": (
        "100644",
        "blob",
        "c5a04b7268ae22a1077a711456b997442ac995f4",
    ),
    "sdk/python/uv.lock": (
        "100644",
        "blob",
        "6f6f867ede321b6be3a94612ba3eebb853a1ac04",
    ),
}

GENERATED_PATHS = [
    "MODULE.bazel.lock",
    "pnpm-lock.yaml",
    "sdk/python/src/openai_codex/api.py",
    "sdk/python/src/openai_codex/generated/notification_registry.py",
    "sdk/python/src/openai_codex/generated/v2_all.py",
]
SDK_GENERATED_PATHS = GENERATED_PATHS[2:]
ALLOWED_MUTABLE_PATHS = sorted([*BUILD_PATHS, *GENERATED_PATHS])

SDK_RUNTIME_DEPENDENCY = "openai-codex-cli-bin==0.147.0"
SDK_RUNTIME_VERSION = "0.147.0"
UV_VERSION = "0.11.3"
NODE_MAJOR = 22
BAZELISK_VERSION = "1.28.1"
SHA_PATTERN = re.compile(r"[0-9a-f]{40}")
PACKAGE_MANAGER_PATTERN = re.compile(r"pnpm@(\d+\.\d+\.\d+)\+sha512\.[A-Za-z0-9+/=]+")
MINIMUM_VERSION_PATTERN = re.compile(r">=(\d+)(?:\.(\d+))?(?:\.(\d+))?")
UV_IDENTITY_PATTERN = re.compile(
    r"(?P<name>[a-z][a-z0-9-]{0,31}) (?P<version>\d+\.\d+\.\d+)"
    r"(?: \((?P<target>[A-Za-z0-9][A-Za-z0-9_.+-]{0,63})\))?"
)
MAX_INPUT_TEXT_BYTES = 2 * 1024 * 1024

EXECUTION_INPUT_MODES = {
    ".bazelversion": "100644",
    ".github/actions/setup-bazel-ci/action.yml": "100644",
    ".github/workflows/repo-checks.yml": "100644",
    "MODULE.bazel": "100644",
    "MODULE.bazel.lock": "100644",
    "package.json": "100644",
    "pnpm-lock.yaml": "100644",
    "pnpm-workspace.yaml": "100644",
    "sdk/python/pyproject.toml": "100644",
    "sdk/python/uv.lock": "100644",
    "sdk/python/scripts/update_sdk_artifacts.py": "100755",
    "sdk/python/tests/test_contract_generation.py": "100644",
    "sdk/python/tests/test_client_rpc_methods.py": "100644",
    "sdk/python/src/openai_codex/api.py": "100644",
    "sdk/python/src/openai_codex/generated/notification_registry.py": "100644",
    "sdk/python/src/openai_codex/generated/v2_all.py": "100644",
    "sdk/typescript/package.json": "100644",
    **{path: "100644" for path in BUILD_PATHS},
    **{path: "100644" for path in PATCH_DEPENDENCIES},
}

EXPECTED_SDK_BUNDLE_HEADS = {
    f"refs/w13825-sdk-{SDK_INPUT_RUN_ID}-{SDK_INPUT_RUN_ATTEMPT}/base": BASE_SHA,
    f"refs/w13825-sdk-{SDK_INPUT_RUN_ID}-{SDK_INPUT_RUN_ATTEMPT}/candidate": SDK_CANDIDATE_SHA,
    f"refs/w13825-sdk-{SDK_INPUT_RUN_ID}-{SDK_INPUT_RUN_ATTEMPT}/materialized": MATERIALIZED_SHA,
    f"refs/w13825-sdk-{SDK_INPUT_RUN_ID}-{SDK_INPUT_RUN_ATTEMPT}/source": SDK_SOURCE_SHA,
    f"refs/w13825-sdk-{SDK_INPUT_RUN_ID}-{SDK_INPUT_RUN_ATTEMPT}/upstream": GLOBAL_UPSTREAM_SHA,
}


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


def run_tool(
    label: str,
    *args: str,
    cwd: pathlib.Path,
    env: dict[str, str] | None = None,
) -> str:
    result = subprocess.run(
        args,
        cwd=cwd,
        env=env,
        check=False,
        text=True,
        capture_output=True,
    )
    if result.returncode != 0:
        combined = "\n".join(part for part in (result.stdout, result.stderr) if part)
        safe = combined.replace(str(cwd), "<candidate-worktree>")
        safe = re.sub(r"https?://\S+", "<redacted-url>", safe)
        safe = re.sub(
            r"\b(?:gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+)\b",
            "<redacted-token>",
            safe,
        )
        lines = safe.splitlines()
        excerpt = "\n".join(lines[-80:]) or "<no diagnostic output>"
        raise SystemExit(f"{label} failed with exit {result.returncode}\n{excerpt}")
    return result.stdout


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


def index_entry(repo: pathlib.Path, path: str) -> tuple[str, str, str] | None:
    output = run_bytes("git", "ls-files", "--stage", "-z", "--", path, cwd=repo)
    if not output:
        return None
    require(output.endswith(b"\0") and output.count(b"\0") == 1, f"ambiguous index entry: {path}")
    metadata, listed = output[:-1].split(b"\t", 1)
    mode, oid, stage = metadata.decode("ascii").split()
    require(stage == "0", f"unmerged index entry: {path}")
    require(listed.decode("utf-8", "surrogateescape") == path, f"unexpected index path: {path}")
    return mode, "blob", oid


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
    require(event_path.is_absolute() and event_path.is_file(), "invalid consumer event path")
    event = json.loads(event_path.read_text(encoding="utf-8"))
    require(isinstance(event, dict), "consumer event must be an object")
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
        require(bool(os.environ.get(name)), f"missing consumer identity: {name}")
    require(os.environ["GITHUB_REPOSITORY_ID"] == REPOSITORY_ID, "current repository ID mismatch")
    expected_workflow_ref = f"{REPOSITORY}/{WORKFLOW_PATH}@{VALIDATION_REF}"
    require(os.environ["GITHUB_WORKFLOW_REF"] == expected_workflow_ref, "consumer workflow ref mismatch")

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
        require(event.get("created") is False, "push event must fast-forward the existing consumer ref")
        head_commit = event.get("head_commit")
        require(
            isinstance(head_commit, dict) and head_commit.get("id") == expected_workflow_sha,
            "push head commit mismatch",
        )
    return {
        "consumer_repository": REPOSITORY,
        "consumer_repository_id": os.environ["GITHUB_REPOSITORY_ID"],
        "consumer_workflow_head": expected_workflow_sha,
        "consumer_workflow_tree": expected_workflow_tree,
        "consumer_workflow_ref": os.environ["GITHUB_WORKFLOW_REF"],
        "consumer_run_id": os.environ["GITHUB_RUN_ID"],
        "consumer_run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
        "consumer_event": os.environ["GITHUB_EVENT_NAME"],
    }


def verify_build_source_checkout(repo: pathlib.Path) -> None:
    require(run("git", "rev-parse", "HEAD", cwd=repo).strip() == BUILD_SOURCE_SHA, "build source head mismatch")
    require(run("git", "rev-parse", "HEAD^{tree}", cwd=repo).strip() == BUILD_SOURCE_TREE, "build source tree mismatch")
    require(run("git", "show", "-s", "--format=%P", BUILD_SOURCE_SHA, cwd=repo).split() == [BASE_SHA], "build source parent mismatch")
    require(not run("git", "status", "--porcelain", cwd=repo), "build source checkout is dirty")
    changed = run(
        "git",
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        BASE_SHA,
        BUILD_SOURCE_SHA,
        cwd=repo,
    ).splitlines()
    require(changed == BUILD_PATHS, "build source diff is not the exact eight-path cohort")
    require(path_digest(changed) == BUILD_PATHS_SHA256, "build source path-set digest mismatch")
    for path, expected in BUILD_SOURCE_ENTRIES.items():
        require(tree_entry(repo, BUILD_SOURCE_SHA, path) == expected, f"build source tuple mismatch: {path}")


def expected_sdk_dispositions(repo: pathlib.Path) -> list[dict[str, Any]]:
    dispositions: list[dict[str, Any]] = []
    for path in SDK_SOURCE_PATHS:
        base_entry = tree_entry(repo, BASE_SHA, path)
        upstream_entry = tree_entry(repo, GLOBAL_UPSTREAM_SHA, path)
        source_entry = tree_entry(repo, SDK_SOURCE_SHA, path)
        materialized_entry = tree_entry(repo, MATERIALIZED_SHA, path)
        require(materialized_entry == base_entry, f"materialized SDK entry is not frozen base: {path}")
        dispositions.append(
            {
                "path": path,
                "disposition": classify(base_entry, upstream_entry, source_entry),
                "base": tuple_json(base_entry),
                "upstream": tuple_json(upstream_entry),
                "source": tuple_json(source_entry),
                "materialized": tuple_json(materialized_entry),
                "selected": tuple_json(source_entry),
                "source_equals_base": source_entry == base_entry,
                "source_equals_upstream": source_entry == upstream_entry,
                "materialized_equals_base": materialized_entry == base_entry,
            }
        )
    return dispositions


def verify_sdk_artifact_files(artifact: pathlib.Path) -> dict[str, pathlib.Path]:
    expected_names = {"candidate.bundle", "receipt.json", "provenance.json"}
    require({path.name for path in artifact.iterdir()} == expected_names, "SDK artifact file set mismatch")
    files: dict[str, pathlib.Path] = {}
    for key, name in (("bundle", "candidate.bundle"), ("receipt", "receipt.json"), ("provenance", "provenance.json")):
        raw = artifact / name
        require(raw.is_file() and not raw.is_symlink() and raw.stat().st_size > 0, f"invalid SDK artifact file: {name}")
        path = raw.resolve(strict=True)
        require(path.parent == artifact, f"SDK artifact file escaped input directory: {name}")
        files[key] = path
    require(digest(files["bundle"]) == SDK_INPUT_BUNDLE_SHA256, "SDK input bundle digest mismatch")
    require(digest(files["receipt"]) == SDK_INPUT_RECEIPT_SHA256, "SDK input receipt digest mismatch")

    receipt = load(files["receipt"])
    require_fields(
        receipt,
        {
            "schema": "upstream-cohort-disposition",
            "version": 2,
            "repository": REPOSITORY,
            "cohort": "sdk-public-contract",
            "path_count": len(SDK_SOURCE_PATHS),
            "path_set_sha256": SDK_SOURCE_PATHS_SHA256,
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
            "candidate_sha": SDK_CANDIDATE_SHA,
            "candidate_tree": SDK_CANDIDATE_TREE,
            "candidate_parent": SDK_CANDIDATE_PARENT,
        },
        "SDK input receipt",
    )

    provenance = load(files["provenance"])
    require(provenance.get("signed") is False, "SDK input provenance must remain explicitly unsigned")
    require_fields(
        provenance,
        {
            "schema": "upstream-cohort-candidate-provenance",
            "version": 2,
            "producer_repository": REPOSITORY,
            "producer_repository_id": REPOSITORY_ID,
            "producer_workflow_head": SDK_INPUT_WORKFLOW_SHA,
            "producer_workflow_tree": SDK_INPUT_WORKFLOW_TREE,
            "producer_workflow_ref": SDK_INPUT_WORKFLOW_REF,
            "producer_run_id": SDK_INPUT_RUN_ID,
            "producer_run_attempt": SDK_INPUT_RUN_ATTEMPT,
            "producer_event": "push",
            "source_repository": REPOSITORY,
            "source_run_id": COMMON_SOURCE_RUN_ID,
            "source_run_attempt": COMMON_SOURCE_RUN_ATTEMPT,
            "source_artifact": COMMON_SOURCE_ARTIFACT,
            "source_bundle_sha256": COMMON_BUNDLE_SHA256,
            "source_receipt_sha256": COMMON_RECEIPT_SHA256,
            "source_provenance_sha256": COMMON_PROVENANCE_SHA256,
            "source_manifest_sha256": COMMON_MANIFEST_SHA256,
            "source_staged_patch_sha256": COMMON_STAGED_PATCH_SHA256,
            "source_staged_paths_sha256": COMMON_STAGED_PATHS_SHA256,
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
            "path_count": len(SDK_SOURCE_PATHS),
            "path_set_sha256": SDK_SOURCE_PATHS_SHA256,
            "candidate_sha": SDK_CANDIDATE_SHA,
            "candidate_tree": SDK_CANDIDATE_TREE,
            "candidate_parent": SDK_CANDIDATE_PARENT,
            "candidate_bundle_sha256": SDK_INPUT_BUNDLE_SHA256,
            "disposition_receipt_sha256": SDK_INPUT_RECEIPT_SHA256,
        },
        "SDK input provenance",
    )
    require(provenance.get("candidate_bundle_heads") == EXPECTED_SDK_BUNDLE_HEADS, "SDK input bundle head receipt mismatch")
    return files


def import_sdk_bundle(repo: pathlib.Path, bundle: pathlib.Path, temp: pathlib.Path) -> None:
    bare = temp / "verify-sdk-input.git"
    run("git", "init", "--bare", str(bare))
    run("git", "-C", str(bare), "bundle", "verify", str(bundle))
    heads: dict[str, str] = {}
    for line in run("git", "-C", str(bare), "bundle", "list-heads", str(bundle)).splitlines():
        oid, ref = line.split(maxsplit=1)
        require(ref not in heads, f"duplicate SDK bundle ref: {ref}")
        heads[ref] = oid
    require(heads == EXPECTED_SDK_BUNDLE_HEADS, "SDK input bundle head map mismatch")
    for source_ref, oid in heads.items():
        suffix = source_ref.rsplit("/", 1)[-1]
        target_ref = f"refs/w13825-sdk-input/{suffix}"
        run("git", "fetch", str(bundle), f"+{source_ref}:{target_ref}", cwd=repo)
        require(run("git", "rev-parse", target_ref, cwd=repo).strip() == oid, f"SDK bundle import mismatch: {suffix}")

    verify_imported_sdk_objects(repo)


def verify_imported_sdk_objects(repo: pathlib.Path) -> None:
    for source_ref, oid in EXPECTED_SDK_BUNDLE_HEADS.items():
        suffix = source_ref.rsplit("/", 1)[-1]
        target_ref = f"refs/w13825-sdk-input/{suffix}"
        require(run("git", "rev-parse", target_ref, cwd=repo).strip() == oid, f"imported SDK ref mismatch: {suffix}")

    require(run("git", "rev-parse", f"{SDK_CANDIDATE_SHA}^{{tree}}", cwd=repo).strip() == SDK_CANDIDATE_TREE, "SDK candidate tree mismatch")
    require(run("git", "show", "-s", "--format=%P", SDK_CANDIDATE_SHA, cwd=repo).split() == [MATERIALIZED_SHA], "SDK candidate parent mismatch")
    require(run("git", "rev-parse", f"{MATERIALIZED_SHA}^{{tree}}", cwd=repo).strip() == MATERIALIZED_TREE, "materialized tree mismatch")
    require(run("git", "show", "-s", "--format=%P", MATERIALIZED_SHA, cwd=repo).split() == [BASE_SHA], "materialized parent mismatch")
    require(run("git", "rev-parse", f"{SDK_SOURCE_SHA}^{{tree}}", cwd=repo).strip() == SDK_SOURCE_TREE, "SDK source tree mismatch")
    require(run("git", "show", "-s", "--format=%P", SDK_SOURCE_SHA, cwd=repo).split() == [BASE_SHA], "SDK source parent mismatch")


def verify_sdk_input_entries(repo: pathlib.Path, receipt: dict[str, Any]) -> list[dict[str, Any]]:
    require(
        not set(COMPOSITE_RUNTIME_INPUTS).intersection(ALLOWED_MUTABLE_PATHS),
        "composite runtime input entered the mutable cohort",
    )
    sdk_changed = run(
        "git",
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        MATERIALIZED_SHA,
        SDK_CANDIDATE_SHA,
        cwd=repo,
    ).splitlines()
    require(sdk_changed == SDK_SOURCE_PATHS, "accepted SDK candidate path set mismatch")
    require(path_digest(sdk_changed) == SDK_SOURCE_PATHS_SHA256, "accepted SDK candidate path digest mismatch")
    expected_dispositions = expected_sdk_dispositions(repo)
    require(receipt.get("paths") == expected_dispositions, "SDK input disposition map mismatch")
    for path in SDK_SOURCE_PATHS:
        require(
            tree_entry(repo, SDK_CANDIDATE_SHA, path) == tree_entry(repo, SDK_SOURCE_SHA, path),
            f"accepted SDK candidate/source tuple mismatch: {path}",
        )
    for path, expected in PATCH_DEPENDENCIES.items():
        require(tree_entry(repo, MATERIALIZED_SHA, path) == expected, f"materialized patch dependency mismatch: {path}")
        require(tree_entry(repo, SDK_CANDIDATE_SHA, path) == expected, f"SDK candidate patch dependency mismatch: {path}")
    verified_runtime_inputs: list[dict[str, Any]] = []
    for path, expected in COMPOSITE_RUNTIME_INPUTS.items():
        materialized_entry = tree_entry(repo, MATERIALIZED_SHA, path)
        candidate_entry = tree_entry(repo, SDK_CANDIDATE_SHA, path)
        expected_tuple = "/".join(expected)
        materialized_tuple = "/".join(materialized_entry) if materialized_entry is not None else "missing"
        candidate_tuple = "/".join(candidate_entry) if candidate_entry is not None else "missing"
        require(
            materialized_entry == expected,
            f"materialized runtime input mismatch: {path} expected={expected_tuple} observed={materialized_tuple}",
        )
        require(
            candidate_entry == expected,
            f"SDK candidate runtime input mismatch: {path} expected={expected_tuple} observed={candidate_tuple}",
        )
        verified_runtime_inputs.append(
            {
                "path": path,
                "materialized": tuple_json(materialized_entry),
                "sdk_candidate": tuple_json(candidate_entry),
            }
        )
    return verified_runtime_inputs


def bounded_text(value: Any, *, maximum: int = 256) -> str | None:
    if not isinstance(value, str) or not value or len(value) > maximum:
        return None
    return value


def read_input_text(
    worktree: pathlib.Path,
    entries: dict[str, tuple[str, str, str] | None],
    path: str,
    errors: list[str],
) -> str | None:
    entry = entries.get(path)
    if entry is None or entry[1] != "blob":
        errors.append(f"{path}:content-unavailable")
        return None
    try:
        size = int(run("git", "cat-file", "-s", entry[2], cwd=worktree).strip())
        if size > MAX_INPUT_TEXT_BYTES:
            errors.append(f"{path}:content-too-large")
            return None
        return run_bytes("git", "cat-file", "blob", entry[2], cwd=worktree).decode("utf-8")
    except (subprocess.CalledProcessError, UnicodeDecodeError, ValueError):
        errors.append(f"{path}:content-read-error")
        return None


def parse_package_manager(value: Any, label: str, errors: list[str]) -> dict[str, Any]:
    observed = bounded_text(value)
    match = PACKAGE_MANAGER_PATTERN.fullmatch(observed or "")
    if match is None:
        errors.append(f"{label}:invalid-package-manager")
        return {"status": "invalid", "value": observed, "version": None}
    return {"status": "observed", "value": observed, "version": match.group(1)}


def minimum_version(requirement: Any, label: str, errors: list[str]) -> tuple[int, int, int] | None:
    observed = bounded_text(requirement, maximum=64)
    match = MINIMUM_VERSION_PATTERN.fullmatch(observed or "")
    if match is None:
        errors.append(f"{label}:unsupported-version-requirement")
        return None
    return tuple(int(value or 0) for value in match.groups())


def version_tuple(value: str, *, prefix: str = "") -> tuple[int, int, int] | None:
    pattern = rf"{re.escape(prefix)}(\d+)\.(\d+)\.(\d+)"
    match = re.fullmatch(pattern, value)
    if match is None:
        return None
    return tuple(int(part) for part in match.groups())


def parse_uv_identity(value: Any, expected_version: str) -> tuple[dict[str, Any], list[str]]:
    observed = bounded_text(value, maximum=128)
    match = UV_IDENTITY_PATTERN.fullmatch(observed or "")
    if match is None:
        return (
            {
                "status": "invalid",
                "raw": observed,
                "name": None,
                "version": None,
                "target": None,
            },
            ["uv:malformed-identity"],
        )
    name = match.group("name")
    version = match.group("version")
    errors: list[str] = []
    if name != "uv":
        errors.append("uv:name-mismatch")
    if version != expected_version:
        errors.append("uv:version-mismatch")
    return (
        {
            "status": "observed" if not errors else "invalid",
            "raw": observed,
            "name": name,
            "version": version,
            "target": match.group("target"),
        },
        errors,
    )


def uv_identity_receipt(value: Any) -> dict[str, Any]:
    identity, errors = parse_uv_identity(value, UV_VERSION)
    return {
        "schema": "uv-tool-identity",
        "version": 1,
        "expected_name": "uv",
        "expected_version": UV_VERSION,
        "identity": identity,
        "errors": errors,
        "status": "ready" if not errors else "invalid",
    }


def workspace_packages(text: str | None) -> list[str]:
    if text is None:
        return []
    packages: list[str] = []
    in_packages = False
    for line in text.splitlines():
        if line == "packages:":
            in_packages = True
            continue
        if in_packages and line and not line.startswith((" ", "\t")):
            break
        if in_packages:
            match = re.fullmatch(r"\s+-\s+['\"]?([^'\"]+)['\"]?\s*", line)
            if match is not None and len(packages) < 32:
                packages.append(match.group(1)[:128])
    return packages


def collect_execution_inputs(
    worktree: pathlib.Path,
    runtime: dict[str, str],
    verified_runtime_inputs: list[dict[str, Any]],
) -> dict[str, Any]:
    errors: list[str] = []
    entries: dict[str, tuple[str, str, str] | None] = {}
    path_observations: list[dict[str, Any]] = []
    for path, expected_mode in sorted(EXECUTION_INPUT_MODES.items()):
        try:
            entry = index_entry(worktree, path)
        except (subprocess.CalledProcessError, SystemExit, UnicodeDecodeError, ValueError):
            entry = None
            status = "read-error"
        else:
            if entry is None:
                status = "missing"
            elif entry[0] != expected_mode or entry[1] != "blob":
                status = "mode-type-mismatch"
            else:
                status = "observed"
        entries[path] = entry
        if status != "observed":
            errors.append(f"{path}:{status}")
        path_observations.append(
            {
                "path": path,
                "expected_mode": expected_mode,
                "status": status,
                "entry": tuple_json(entry),
            }
        )

    def parse_json_input(path: str) -> dict[str, Any]:
        text = read_input_text(worktree, entries, path, errors)
        if text is None:
            return {}
        try:
            value = json.loads(text)
        except json.JSONDecodeError:
            errors.append(f"{path}:json-parse-error")
            return {}
        if not isinstance(value, dict):
            errors.append(f"{path}:json-not-object")
            return {}
        return value

    root_package = parse_json_input("package.json")
    sdk_package = parse_json_input("sdk/typescript/package.json")
    root_manager = parse_package_manager(root_package.get("packageManager"), "root-package", errors)
    sdk_manager = parse_package_manager(sdk_package.get("packageManager"), "sdk-package", errors)
    root_engines = root_package.get("engines") if isinstance(root_package.get("engines"), dict) else {}
    sdk_engines = sdk_package.get("engines") if isinstance(sdk_package.get("engines"), dict) else {}
    root_node_requirement = bounded_text(root_engines.get("node"), maximum=64)
    root_pnpm_requirement = bounded_text(root_engines.get("pnpm"), maximum=64)
    sdk_node_requirement = bounded_text(sdk_engines.get("node"), maximum=64)
    minimum_version(root_node_requirement, "root-node-engine", errors)
    minimum_version(root_pnpm_requirement, "root-pnpm-engine", errors)
    minimum_version(sdk_node_requirement, "sdk-node-engine", errors)
    sdk_scripts_source = sdk_package.get("scripts") if isinstance(sdk_package.get("scripts"), dict) else {}
    sdk_scripts = {name: bounded_text(sdk_scripts_source.get(name)) for name in ("build", "lint", "test")}
    for name, value in sdk_scripts.items():
        if value is None:
            errors.append(f"sdk-package:missing-{name}-script")

    workspace_text = read_input_text(worktree, entries, "pnpm-workspace.yaml", errors)
    packages = workspace_packages(workspace_text)
    if "sdk/typescript" not in packages:
        errors.append("pnpm-workspace:sdk-typescript-missing")

    pyproject_text = read_input_text(worktree, entries, "sdk/python/pyproject.toml", errors)
    pyproject: dict[str, Any] = {}
    if pyproject_text is not None:
        try:
            pyproject = tomllib.loads(pyproject_text)
        except tomllib.TOMLDecodeError:
            errors.append("sdk-python-pyproject:toml-parse-error")
    build_system = pyproject.get("build-system") if isinstance(pyproject.get("build-system"), dict) else {}
    project = pyproject.get("project") if isinstance(pyproject.get("project"), dict) else {}
    build_requires = build_system.get("requires") if isinstance(build_system.get("requires"), list) else []
    python_requirement = bounded_text(project.get("requires-python"), maximum=64)
    minimum_version(python_requirement, "sdk-python-engine", errors)
    dependencies = project.get("dependencies") if isinstance(project.get("dependencies"), list) else []
    runtime_dependencies = [
        value[:128]
        for value in dependencies
        if isinstance(value, str) and value.startswith("openai-codex-cli-bin")
    ][:4]
    python_build = {
        "backend": bounded_text(build_system.get("build-backend"), maximum=128),
        "requires": [value[:128] for value in build_requires if isinstance(value, str)][:16],
        "requires_python": python_requirement,
        "runtime_dependencies": runtime_dependencies,
    }
    if python_build["backend"] != "uv_build":
        errors.append("sdk-python-pyproject:build-backend-mismatch")
    if not any(value.startswith("uv_build") for value in python_build["requires"]):
        errors.append("sdk-python-pyproject:uv-build-requirement-missing")
    if runtime_dependencies != [SDK_RUNTIME_DEPENDENCY]:
        errors.append("sdk-python-pyproject:runtime-dependency-mismatch")

    lock_text = read_input_text(worktree, entries, "sdk/python/uv.lock", errors)
    lock: dict[str, Any] = {}
    if lock_text is not None:
        try:
            lock = tomllib.loads(lock_text)
        except tomllib.TOMLDecodeError:
            errors.append("sdk-python-lock:toml-parse-error")
    lock_packages = lock.get("package") if isinstance(lock.get("package"), list) else []
    runtime_versions: list[str] = []
    runtime_specifiers: list[str] = []
    for package in lock_packages:
        if not isinstance(package, dict):
            continue
        if package.get("name") == "openai-codex-cli-bin" and isinstance(package.get("version"), str):
            runtime_versions.append(package["version"][:64])
        if package.get("name") != "openai-codex":
            continue
        metadata = package.get("metadata") if isinstance(package.get("metadata"), dict) else {}
        requires_dist = metadata.get("requires-dist") if isinstance(metadata.get("requires-dist"), list) else []
        for requirement in requires_dist:
            if (
                isinstance(requirement, dict)
                and requirement.get("name") == "openai-codex-cli-bin"
                and isinstance(requirement.get("specifier"), str)
            ):
                runtime_specifiers.append(requirement["specifier"][:64])
    lock_python_requirement = bounded_text(lock.get("requires-python"), maximum=64)
    minimum_version(lock_python_requirement, "sdk-python-lock-engine", errors)
    python_lock = {
        "version": lock.get("version"),
        "requires_python": lock_python_requirement,
        "runtime_versions": sorted(set(runtime_versions)),
        "runtime_specifiers": sorted(set(runtime_specifiers)),
    }
    if python_lock["version"] != 1:
        errors.append("sdk-python-lock:format-mismatch")
    if python_lock["runtime_versions"] != [SDK_RUNTIME_VERSION]:
        errors.append("sdk-python-lock:runtime-version-mismatch")
    if python_lock["runtime_specifiers"] != [f"=={SDK_RUNTIME_VERSION}"]:
        errors.append("sdk-python-lock:runtime-specifier-mismatch")

    bazel_text = read_input_text(worktree, entries, ".bazelversion", errors)
    bazel_version = bounded_text(bazel_text.strip() if bazel_text is not None else None, maximum=64)
    if bazel_version is None or version_tuple(bazel_version) is None:
        errors.append("bazel-version:invalid")

    repo_checks = read_input_text(worktree, entries, ".github/workflows/repo-checks.yml", errors)
    uv_versions = sorted(set(re.findall(r'version:\s*["\']?(\d+\.\d+\.\d+)', repo_checks or "")))
    uv_version = UV_VERSION if UV_VERSION in uv_versions else None
    if uv_version is None:
        errors.append("repo-checks:uv-version-missing")

    bazel_setup = read_input_text(worktree, entries, ".github/actions/setup-bazel-ci/action.yml", errors)
    bazelisk_versions = sorted(set(re.findall(r"bazelisk-version:\s*(\d+\.\d+\.\d+)", bazel_setup or "")))
    bazelisk_version = BAZELISK_VERSION if BAZELISK_VERSION in bazelisk_versions else None
    if bazelisk_version is None:
        errors.append("setup-bazel-ci:bazelisk-version-missing")

    pnpm_lock_text = read_input_text(worktree, entries, "pnpm-lock.yaml", errors)
    lockfile_match = re.search(r"^lockfileVersion:\s*['\"]?([^'\"\s]+)", pnpm_lock_text or "", re.MULTILINE)
    pnpm_lock_version = bounded_text(lockfile_match.group(1), maximum=32) if lockfile_match is not None else None
    if pnpm_lock_version is None:
        errors.append("pnpm-lock:lockfile-version-missing")

    errors = sorted(set(errors))
    return {
        "schema": "sdk-build-execution-inputs",
        "version": 1,
        **runtime,
        "input_sdk_artifact_id": SDK_INPUT_ARTIFACT_ID,
        "input_sdk_candidate": SDK_CANDIDATE_SHA,
        "input_sdk_tree": SDK_CANDIDATE_TREE,
        "build_source_sha": BUILD_SOURCE_SHA,
        "build_source_tree": BUILD_SOURCE_TREE,
        "path_observations": path_observations,
        "root_package": {
            "package_manager": root_manager,
            "node_engine": root_node_requirement,
            "pnpm_engine": root_pnpm_requirement,
        },
        "sdk_package": {
            "package_manager": sdk_manager,
            "node_engine": sdk_node_requirement,
            "scripts": sdk_scripts,
        },
        "package_manager_equality": root_manager.get("value") == sdk_manager.get("value"),
        "package_manager_equality_required": False,
        "workspace_packages": packages,
        "python_build": python_build,
        "python_lock": python_lock,
        "pnpm_lock_version": pnpm_lock_version,
        "uv_version": uv_version,
        "node_major": NODE_MAJOR,
        "bazel_version": bazel_version,
        "bazelisk_version": bazelisk_version,
        "verified_composite_runtime_inputs": verified_runtime_inputs,
        "errors": errors,
        "status": "ready" if not errors else "invalid",
    }


def probe_tool(label: str, *args: str, cwd: pathlib.Path) -> dict[str, Any]:
    try:
        result = subprocess.run(args, cwd=cwd, check=False, text=True, capture_output=True)
    except OSError:
        return {"label": label, "status": "error", "exit_code": None, "observed": None}
    output = result.stdout.strip()
    if result.returncode != 0 or not output or "\n" in output or len(output) > 128:
        return {"label": label, "status": "error", "exit_code": result.returncode, "observed": None}
    return {"label": label, "status": "observed", "exit_code": 0, "observed": output}


def collect_tool_observations(
    worktree: pathlib.Path,
    execution_inputs: dict[str, Any],
) -> tuple[dict[str, Any], dict[str, str]]:
    observations = {
        "uv": probe_tool("uv", "uv", "--version", cwd=worktree),
        "pnpm": probe_tool("pnpm", "pnpm", "--version", cwd=worktree),
        "node": probe_tool("node", "node", "--version", cwd=worktree),
        "bazel": probe_tool("bazel", "bazel", "--version", cwd=worktree),
        "python": {
            "label": "python",
            "status": "observed",
            "exit_code": 0,
            "observed": ".".join(str(part) for part in sys.version_info[:3]),
        },
    }
    errors: list[str] = []
    root_package = execution_inputs["root_package"]
    sdk_package = execution_inputs["sdk_package"]
    root_pnpm_version = root_package["package_manager"].get("version")
    if observations["pnpm"].get("observed") != root_pnpm_version:
        errors.append("pnpm:root-package-version-mismatch")
    uv_identity, uv_errors = parse_uv_identity(
        observations["uv"].get("observed"),
        execution_inputs["uv_version"],
    )
    observations["uv"]["identity"] = uv_identity
    errors.extend(uv_errors)
    node_observed = observations["node"].get("observed")
    node_version = version_tuple(node_observed or "", prefix="v")
    python_version = version_tuple(observations["python"]["observed"])
    pnpm_version = version_tuple(observations["pnpm"].get("observed") or "")
    root_node_minimum = minimum_version(root_package.get("node_engine"), "root-node-engine", errors)
    sdk_node_minimum = minimum_version(sdk_package.get("node_engine"), "sdk-node-engine", errors)
    root_pnpm_minimum = minimum_version(root_package.get("pnpm_engine"), "root-pnpm-engine", errors)
    python_minimum = minimum_version(execution_inputs["python_build"].get("requires_python"), "python-engine", errors)
    if node_version is None or node_version[0] != NODE_MAJOR:
        errors.append("node:major-version-mismatch")
    if node_version is not None and root_node_minimum is not None and node_version < root_node_minimum:
        errors.append("node:root-engine-mismatch")
    if node_version is not None and sdk_node_minimum is not None and node_version < sdk_node_minimum:
        errors.append("node:sdk-engine-mismatch")
    if pnpm_version is None or root_pnpm_minimum is None or pnpm_version < root_pnpm_minimum:
        errors.append("pnpm:root-engine-mismatch")
    if python_version is None or python_minimum is None or python_version < python_minimum:
        errors.append("python:sdk-engine-mismatch")
    bazel_observed = observations["bazel"].get("observed")
    if not isinstance(bazel_observed, str) or not bazel_observed.endswith(f" {execution_inputs['bazel_version']}"):
        errors.append("bazel:version-mismatch")
    for name, observation in observations.items():
        if observation["status"] != "observed":
            errors.append(f"{name}:probe-error")
    errors = sorted(set(errors))
    receipt = {
        "schema": "sdk-build-tool-observations",
        "version": 1,
        "observations": observations,
        "errors": errors,
        "status": "ready" if not errors else "invalid",
    }
    generator_identity = {
        "sdk_generator": "sdk/python/scripts/update_sdk_artifacts.py generate-types",
        "sdk_runtime_dependency": SDK_RUNTIME_DEPENDENCY,
        "uv": observations["uv"].get("observed") or "unavailable",
        "root_pnpm_package_manager": root_package["package_manager"]["value"] or "unavailable",
        "sdk_pnpm_package_manager": sdk_package["package_manager"]["value"] or "unavailable",
        "pnpm": observations["pnpm"].get("observed") or "unavailable",
        "node": observations["node"].get("observed") or "unavailable",
        "python": observations["python"]["observed"],
        "bazel_pin": execution_inputs["bazel_version"] or "unavailable",
        "bazel": observations["bazel"].get("observed") or "unavailable",
        "bazelisk": execution_inputs["bazelisk_version"] or "unavailable",
    }
    return receipt, generator_identity


def require_no_untracked(worktree: pathlib.Path) -> None:
    untracked = run("git", "ls-files", "--others", "--exclude-standard", cwd=worktree).splitlines()
    require(not untracked, f"unexpected untracked candidate paths: {untracked[:8]}")


def require_changed_path_boundary(changed: list[str], allowed: list[str], label: str) -> None:
    require(len(changed) == len(set(changed)), f"{label} contains duplicate changed paths")
    require(changed == sorted(changed), f"{label} changed paths are not canonical")
    changed_set = set(changed)
    require(set(BUILD_PATHS).issubset(changed_set), f"{label} omitted a selected build-source path")
    require(changed_set.issubset(allowed), f"{label} escaped the allowed generated-path subset")


def require_modified_paths_only(status_lines: list[str], changed: list[str], label: str) -> None:
    status_paths: list[str] = []
    for line in status_lines:
        fields = line.split("\t")
        require(len(fields) == 2, f"{label} contains a rename or malformed status: {line}")
        status, path = fields
        require(status == "M", f"{label} contains a non-modification status: {line}")
        status_paths.append(path)
    require(status_paths == changed, f"{label} status/path mismatch")


def require_candidate_paths(worktree: pathlib.Path, allowed: list[str], label: str) -> list[str]:
    changed = run("git", "diff", "--name-only", SDK_CANDIDATE_SHA, "--", cwd=worktree).splitlines()
    require_changed_path_boundary(changed, allowed, label)
    status_lines = run("git", "diff", "--name-status", SDK_CANDIDATE_SHA, "--", cwd=worktree).splitlines()
    require_modified_paths_only(status_lines, changed, label)
    require_no_untracked(worktree)
    return changed


def prepare_candidate_worktree(repo: pathlib.Path, temp: pathlib.Path) -> pathlib.Path:
    for path in BUILD_PATHS:
        require(
            tree_entry(repo, SDK_CANDIDATE_SHA, path) == tree_entry(repo, BASE_SHA, path),
            f"accepted SDK candidate did not retain build source preimage: {path}",
        )
    worktree = temp / "candidate-worktree"
    run("git", "clone", "--shared", "--no-checkout", str(repo), str(worktree))
    run("git", "checkout", "--detach", SDK_CANDIDATE_SHA, cwd=worktree)
    require(not run("git", "status", "--porcelain", cwd=worktree), "candidate worktree is not initially clean")
    run("git", "checkout", BUILD_SOURCE_SHA, "--", *BUILD_PATHS, cwd=worktree)
    for path, expected in BUILD_SOURCE_ENTRIES.items():
        require(index_entry(worktree, path) == expected, f"selected build source index tuple mismatch: {path}")
    staged_source = run("git", "diff", "--cached", "--name-only", SDK_CANDIDATE_SHA, cwd=worktree).splitlines()
    require(staged_source == BUILD_PATHS, "selected build source escaped the eight-path cohort")
    require_candidate_paths(worktree, BUILD_PATHS, "build source selection")
    return worktree


def write_execution_inputs(preflight_dir: pathlib.Path, execution_inputs: dict[str, Any]) -> pathlib.Path:
    require(preflight_dir.parent.is_dir(), "preflight parent must already exist")
    require(not preflight_dir.exists(), "preflight directory already exists")
    preflight_dir.mkdir()
    receipt_path = preflight_dir / "execution-inputs.json"
    receipt_path.write_text(json.dumps(execution_inputs, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    require(load(receipt_path) == execution_inputs, "execution-input receipt readback mismatch")
    print(json.dumps(execution_inputs, sort_keys=True))
    require(execution_inputs["status"] == "ready", f"execution-input preflight invalid: {execution_inputs['errors']}")
    return receipt_path


def load_execution_inputs(preflight_dir: pathlib.Path, runtime: dict[str, str]) -> tuple[pathlib.Path, dict[str, Any]]:
    require(preflight_dir.is_dir(), "preflight input must be a directory")
    require({path.name for path in preflight_dir.iterdir()} == {"execution-inputs.json"}, "preflight file set mismatch")
    receipt_path = (preflight_dir / "execution-inputs.json").resolve(strict=True)
    require(receipt_path.parent == preflight_dir, "preflight receipt escaped input directory")
    execution_inputs = load(receipt_path)
    require(execution_inputs.get("schema") == "sdk-build-execution-inputs", "preflight schema mismatch")
    require(execution_inputs.get("version") == 1, "preflight version mismatch")
    require(
        execution_inputs.get("status") == "ready" and execution_inputs.get("errors") == [],
        "preflight is not ready",
    )
    for key, value in runtime.items():
        require(execution_inputs.get(key) == value, f"preflight runtime identity mismatch: {key}")
    require(execution_inputs.get("input_sdk_artifact_id") == SDK_INPUT_ARTIFACT_ID, "preflight artifact mismatch")
    require(execution_inputs.get("input_sdk_candidate") == SDK_CANDIDATE_SHA, "preflight candidate mismatch")
    require(execution_inputs.get("input_sdk_tree") == SDK_CANDIDATE_TREE, "preflight candidate tree mismatch")
    require(execution_inputs.get("build_source_sha") == BUILD_SOURCE_SHA, "preflight build source mismatch")
    require(execution_inputs.get("build_source_tree") == BUILD_SOURCE_TREE, "preflight build source tree mismatch")
    return receipt_path, execution_inputs


def require_committed_candidate_paths(repo: pathlib.Path, revision: str, label: str) -> list[str]:
    changed = run(
        "git",
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        revision,
        cwd=repo,
    ).splitlines()
    require_changed_path_boundary(changed, ALLOWED_MUTABLE_PATHS, label)
    status_lines = run(
        "git",
        "diff-tree",
        "--no-commit-id",
        "--name-status",
        "-r",
        revision,
        cwd=repo,
    ).splitlines()
    require_modified_paths_only(status_lines, changed, label)
    return changed


def generate_and_test(
    worktree: pathlib.Path,
    temp: pathlib.Path,
    execution_inputs: dict[str, Any],
) -> tuple[dict[str, str], dict[str, Any]]:
    tool_observations, generator_identity = collect_tool_observations(worktree, execution_inputs)
    pre_generation_readback = {
        "schema": "sdk-build-pre-generation-readback",
        "version": 1,
        "execution_inputs": execution_inputs,
        "tool_observations": tool_observations,
    }
    print(json.dumps(pre_generation_readback, sort_keys=True))
    require(execution_inputs["status"] == "ready", "execution inputs are not ready")
    require(tool_observations["status"] == "ready", f"tool smoke invalid: {tool_observations['errors']}")
    python_project = worktree / "sdk/python"
    generation_env = {
        **os.environ,
        "UV_LINK_MODE": "copy",
        "UV_PROJECT_ENVIRONMENT": str(temp / "sdk-python-venv"),
        "UV_PYTHON": sys.executable,
        "UV_PYTHON_DOWNLOADS": "never",
    }
    generation_env.pop("CODEX_EXEC_PATH", None)

    run_tool(
        "SDK Python dependency sync",
        "uv",
        "sync",
        "--project",
        str(python_project),
        "--group",
        "dev",
        "--frozen",
        cwd=worktree,
        env=generation_env,
    )
    installed_runtime = run_tool(
        "SDK runtime pin verification",
        "uv",
        "run",
        "--project",
        str(python_project),
        "--frozen",
        "--no-sync",
        "python",
        "-c",
        "import importlib.metadata; print(importlib.metadata.version('openai-codex-cli-bin'))",
        cwd=worktree,
        env=generation_env,
    ).strip()
    require(
        installed_runtime == SDK_RUNTIME_VERSION,
        f"installed SDK runtime version mismatch: expected={SDK_RUNTIME_VERSION} observed={installed_runtime[:96]}",
    )
    generator_identity["installed_sdk_runtime"] = installed_runtime

    run_tool(
        "SDK artifact generation",
        "uv",
        "run",
        "--project",
        str(python_project),
        "--frozen",
        "--no-sync",
        "python",
        "scripts/update_sdk_artifacts.py",
        "generate-types",
        cwd=python_project,
        env=generation_env,
    )
    require_candidate_paths(worktree, sorted([*BUILD_PATHS, *SDK_GENERATED_PATHS]), "SDK generation")
    run("git", "add", "--", *SDK_GENERATED_PATHS, cwd=worktree)

    run_tool(
        "MODULE.bazel.lock generation",
        "bazel",
        "mod",
        "deps",
        "--lockfile_mode=update",
        cwd=worktree,
    )
    require_candidate_paths(
        worktree,
        sorted([*BUILD_PATHS, *SDK_GENERATED_PATHS, "MODULE.bazel.lock"]),
        "Bazel lock generation",
    )
    run("git", "add", "--", "MODULE.bazel.lock", cwd=worktree)

    run_tool(
        "pnpm lock generation",
        "pnpm",
        "install",
        "--lockfile-only",
        "--no-frozen-lockfile",
        "--ignore-scripts",
        cwd=worktree,
    )
    require_candidate_paths(worktree, ALLOWED_MUTABLE_PATHS, "joint pnpm lock generation")
    run("git", "add", "--", "pnpm-lock.yaml", cwd=worktree)

    run_tool(
        "SDK Python contract tests",
        "uv",
        "run",
        "--project",
        str(python_project),
        "--frozen",
        "--no-sync",
        "pytest",
        "tests/test_contract_generation.py",
        "tests/test_client_rpc_methods.py",
        cwd=python_project,
        env=generation_env,
    )
    run_tool(
        "frozen pnpm install",
        "pnpm",
        "install",
        "--frozen-lockfile",
        "--ignore-scripts",
        cwd=worktree,
    )
    for command in ("build", "lint", "test"):
        run_tool(
            f"TypeScript SDK {command}",
            "pnpm",
            "--filter",
            "@openai/codex-sdk",
            "run",
            command,
            cwd=worktree,
        )
    run_tool(
        "Bazel lock verification",
        "bazel",
        "mod",
        "deps",
        "--lockfile_mode=error",
        cwd=worktree,
    )
    require_candidate_paths(worktree, ALLOWED_MUTABLE_PATHS, "post-test candidate")
    require(not run("git", "diff", "--name-only", cwd=worktree), "candidate has unstaged tracked changes")
    run("git", "diff", "--cached", "--check", SDK_CANDIDATE_SHA, cwd=worktree)
    generator_identity["commands"] = [
        "uv sync --project sdk/python --group dev --frozen",
        "uv run --project sdk/python --frozen --no-sync python scripts/update_sdk_artifacts.py generate-types",
        "bazel mod deps --lockfile_mode=update",
        "pnpm install --lockfile-only --no-frozen-lockfile --ignore-scripts",
        "uv run --project sdk/python --frozen --no-sync pytest tests/test_contract_generation.py tests/test_client_rpc_methods.py",
        "pnpm install --frozen-lockfile --ignore-scripts",
        "pnpm --filter @openai/codex-sdk run build",
        "pnpm --filter @openai/codex-sdk run lint",
        "pnpm --filter @openai/codex-sdk run test",
        "bazel mod deps --lockfile_mode=error",
    ]
    return generator_identity, tool_observations


def verify_emitted_bundle(
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
        require(ref not in actual, f"duplicate emitted bundle ref: {ref}")
        actual[ref] = oid
    require(actual == expected_heads, "candidate bundle head map mismatch")
    candidate_ref = next(ref for ref, oid in actual.items() if oid == candidate_sha)
    fetch_bundle_ref(bare, bundle, candidate_ref, "refs/import/candidate")
    require(run("git", "-C", str(bare), "rev-parse", "refs/import/candidate^{tree}").strip() == candidate_tree, "candidate bundle tree mismatch")
    require(run("git", "-C", str(bare), "show", "-s", "--format=%P", "refs/import/candidate").split() == [SDK_CANDIDATE_SHA], "candidate bundle parent mismatch")
    require_committed_candidate_paths(bare, "refs/import/candidate", "fresh-bare candidate")
    for path in GENERATED_PATHS:
        parent_entry = tree_entry(bare, SDK_CANDIDATE_SHA, path)
        selected_entry = tree_entry(bare, "refs/import/candidate", path)
        require(parent_entry is not None and selected_entry is not None, f"fresh-bare generated path missing: {path}")
        require(selected_entry[:2] == parent_entry[:2], f"fresh-bare generated path type changed: {path}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", type=pathlib.Path)
    parser.add_argument("--artifact-dir", type=pathlib.Path)
    parser.add_argument("--preflight-dir", type=pathlib.Path)
    parser.add_argument("--output-dir", type=pathlib.Path)
    parser.add_argument("--expected-workflow-sha", required=True)
    parser.add_argument("--expected-workflow-tree", required=True)
    parser.add_argument("--validate-runtime-only", action="store_true")
    parser.add_argument("--validate-uv-identity")
    parser.add_argument("--prepare-inputs-only", action="store_true")
    args = parser.parse_args()

    runtime = verify_runtime(args.expected_workflow_sha, args.expected_workflow_tree)
    if args.validate_runtime_only:
        require(
            args.repo_root is None
            and args.artifact_dir is None
            and args.preflight_dir is None
            and args.output_dir is None
            and args.validate_uv_identity is None
            and not args.prepare_inputs_only,
            "runtime-only validation does not accept consumer paths",
        )
        print(json.dumps(runtime, sort_keys=True))
        return
    if args.validate_uv_identity is not None:
        require(
            args.repo_root is None
            and args.artifact_dir is None
            and args.preflight_dir is None
            and args.output_dir is None
            and not args.prepare_inputs_only,
            "uv identity validation does not accept consumer paths",
        )
        receipt = uv_identity_receipt(args.validate_uv_identity)
        print(json.dumps(receipt, sort_keys=True))
        require(receipt["status"] == "ready", f"uv identity invalid: {receipt['errors']}")
        return
    require(
        args.repo_root is not None and args.artifact_dir is not None,
        "repository and artifact paths are required",
    )
    repo = absolute_argument(args.repo_root, "repo-root", must_exist=True)
    artifact = absolute_argument(args.artifact_dir, "artifact-dir", must_exist=True)
    require(repo.is_dir() and artifact.is_dir(), "repository and artifact inputs must be directories")
    verify_build_source_checkout(repo)

    if args.prepare_inputs_only:
        require(args.preflight_dir is not None and args.output_dir is None, "preflight path is required")
        preflight_dir = absolute_argument(args.preflight_dir, "preflight-dir", must_exist=False)
        require(preflight_dir.parent.is_dir(), "preflight parent must already exist")
        files = verify_sdk_artifact_files(artifact)
        with tempfile.TemporaryDirectory(prefix="w13825-sdk-preflight-", dir=str(preflight_dir.parent)) as temp_name:
            temp = pathlib.Path(temp_name).resolve(strict=True)
            import_sdk_bundle(repo, files["bundle"], temp)
            sdk_receipt = load(files["receipt"])
            verified_runtime_inputs = verify_sdk_input_entries(repo, sdk_receipt)
            worktree = prepare_candidate_worktree(repo, temp)
            execution_inputs = collect_execution_inputs(worktree, runtime, verified_runtime_inputs)
        write_execution_inputs(preflight_dir, execution_inputs)
        return

    require(args.preflight_dir is not None and args.output_dir is not None, "preflight and output paths are required")
    preflight_dir = absolute_argument(args.preflight_dir, "preflight-dir", must_exist=True)
    output = absolute_argument(args.output_dir, "output-dir", must_exist=False)
    require(output.parent.is_dir(), "output parent must already exist")
    require(not output.exists(), "output directory already exists")
    preflight_path, execution_inputs = load_execution_inputs(preflight_dir, runtime)
    verify_imported_sdk_objects(repo)
    receipt_path = artifact / "receipt.json"
    require(receipt_path.is_file() and not receipt_path.is_symlink(), "SDK input receipt is unavailable")
    require(digest(receipt_path) == SDK_INPUT_RECEIPT_SHA256, "SDK input receipt digest mismatch after preflight")
    sdk_receipt = load(receipt_path)
    output.mkdir()

    with tempfile.TemporaryDirectory(prefix="w13825-sdk-build-", dir=str(output.parent)) as temp_name:
        temp = pathlib.Path(temp_name).resolve(strict=True)
        verified_runtime_inputs = verify_sdk_input_entries(repo, sdk_receipt)
        worktree = prepare_candidate_worktree(repo, temp)
        observed_inputs = collect_execution_inputs(worktree, runtime, verified_runtime_inputs)
        require(observed_inputs == execution_inputs, "execution inputs changed after preflight")
        generator_identity, tool_observations = generate_and_test(worktree, temp, execution_inputs)
        candidate_tree = run("git", "write-tree", cwd=worktree).strip()
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
            SDK_CANDIDATE_SHA,
            "-m",
            "Compose accepted SDK and build source with generated locks",
            cwd=worktree,
            env=commit_env,
        ).strip()
        require(run("git", "show", "-s", "--format=%P", candidate_sha, cwd=worktree).split() == [SDK_CANDIDATE_SHA], "candidate parent mismatch")
        candidate_paths = require_committed_candidate_paths(worktree, candidate_sha, "candidate diff")
        candidate_path_set = set(candidate_paths)

        path_dispositions: list[dict[str, Any]] = []
        generated_dispositions: list[dict[str, Any]] = []
        for path in ALLOWED_MUTABLE_PATHS:
            parent_entry = tree_entry(worktree, SDK_CANDIDATE_SHA, path)
            selected_entry = tree_entry(worktree, candidate_sha, path)
            role = "build-source" if path in BUILD_SOURCE_ENTRIES else "generated"
            changed = path in candidate_path_set
            if role == "build-source":
                source_entry = tree_entry(worktree, BUILD_SOURCE_SHA, path)
                require(selected_entry == BUILD_SOURCE_ENTRIES[path] == source_entry, f"final build source tuple mismatch: {path}")
                require(changed, f"final candidate omitted selected build source: {path}")
                disposition = "selected-build-source"
            else:
                source_entry = None
                require(parent_entry is not None and selected_entry is not None, f"generated path missing: {path}")
                require(selected_entry[:2] == parent_entry[:2], f"generated path type changed: {path}")
                require(changed == (selected_entry != parent_entry), f"generated path change manifest mismatch: {path}")
                disposition = "regenerated-change" if changed else "regenerated-noop"
            path_disposition = {
                "path": path,
                "role": role,
                "disposition": disposition,
                "changed": changed,
                "parent": tuple_json(parent_entry),
                "build_source": tuple_json(source_entry),
                "selected": tuple_json(selected_entry),
            }
            path_dispositions.append(path_disposition)
            if role == "generated":
                generated_dispositions.append(path_disposition)
        require(len(generated_dispositions) == len(GENERATED_PATHS), "generated disposition count mismatch")

        retained_patches = []
        for path, expected in PATCH_DEPENDENCIES.items():
            actual = tree_entry(worktree, candidate_sha, path)
            require(actual == expected, f"final candidate patch dependency mismatch: {path}")
            retained_patches.append({"path": path, "entry": tuple_json(actual)})
        retained_sdk_source = []
        for path in SDK_SOURCE_PATHS:
            expected = tree_entry(worktree, SDK_CANDIDATE_SHA, path)
            actual = tree_entry(worktree, candidate_sha, path)
            require(actual == expected, f"final candidate changed accepted SDK source: {path}")
            retained_sdk_source.append({"path": path, "entry": tuple_json(actual)})

        prefix = f"refs/w13825-sdk-build-{runtime['consumer_run_id']}-{runtime['consumer_run_attempt']}"
        emitted_heads = {
            f"{prefix}/base": BASE_SHA,
            f"{prefix}/build-source": BUILD_SOURCE_SHA,
            f"{prefix}/candidate": candidate_sha,
            f"{prefix}/materialized": MATERIALIZED_SHA,
            f"{prefix}/sdk-candidate": SDK_CANDIDATE_SHA,
            f"{prefix}/sdk-source": SDK_SOURCE_SHA,
            f"{prefix}/upstream": GLOBAL_UPSTREAM_SHA,
        }
        for ref, oid in emitted_heads.items():
            run("git", "update-ref", ref, oid, cwd=worktree)
        candidate_bundle = (output / "candidate.bundle").resolve()
        run("git", "bundle", "create", str(candidate_bundle), *sorted(emitted_heads), cwd=worktree)
        verify_emitted_bundle(candidate_bundle, emitted_heads, candidate_sha, candidate_tree, temp)

        receipt = {
            "schema": "sdk-build-hosted-consumer-disposition",
            "version": 1,
            "repository": REPOSITORY,
            "input_sdk_artifact_id": SDK_INPUT_ARTIFACT_ID,
            "input_sdk_candidate": SDK_CANDIDATE_SHA,
            "input_sdk_tree": SDK_CANDIDATE_TREE,
            "input_sdk_parent": SDK_CANDIDATE_PARENT,
            "build_source_branch": BUILD_SOURCE_BRANCH,
            "build_source_sha": BUILD_SOURCE_SHA,
            "build_source_tree": BUILD_SOURCE_TREE,
            "build_source_parent": BUILD_SOURCE_PARENT,
            "mutable_path_policy": "exact-build-source-plus-allowed-generated-subset",
            "allowed_mutable_path_count": len(ALLOWED_MUTABLE_PATHS),
            "allowed_mutable_path_set_sha256": path_digest(ALLOWED_MUTABLE_PATHS),
            "actual_changed_path_count": len(candidate_paths),
            "actual_changed_path_set_sha256": path_digest(candidate_paths),
            "actual_changed_paths": candidate_paths,
            "build_source_path_count": len(BUILD_PATHS),
            "build_source_path_set_sha256": BUILD_PATHS_SHA256,
            "generated_path_count": len(GENERATED_PATHS),
            "generated_paths": GENERATED_PATHS,
            "generated_path_policy": "mandatory-generation-allowed-change-subset",
            "generated_path_dispositions": generated_dispositions,
            "candidate_sha": candidate_sha,
            "candidate_tree": candidate_tree,
            "candidate_parent": SDK_CANDIDATE_SHA,
            "paths": path_dispositions,
            "verified_retained_patch_dependencies": retained_patches,
            "verified_composite_runtime_inputs": verified_runtime_inputs,
            "preserved_sdk_source_paths": retained_sdk_source,
            "execution_input_receipt_sha256": digest(preflight_path),
            "execution_inputs": execution_inputs,
            "tool_observations": tool_observations,
            "generator_identity": generator_identity,
        }
        receipt_path = output / "receipt.json"
        receipt_path.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        provenance = {
            "schema": "sdk-build-hosted-consumer-provenance",
            "version": 1,
            "signed": False,
            **runtime,
            "input_sdk_artifact_id": SDK_INPUT_ARTIFACT_ID,
            "input_sdk_artifact_name": SDK_INPUT_ARTIFACT_NAME,
            "input_sdk_artifact_size": SDK_INPUT_ARTIFACT_SIZE,
            "input_sdk_archive_sha256": SDK_INPUT_ARCHIVE_SHA256,
            "input_sdk_run_id": SDK_INPUT_RUN_ID,
            "input_sdk_run_attempt": SDK_INPUT_RUN_ATTEMPT,
            "input_sdk_bundle_sha256": SDK_INPUT_BUNDLE_SHA256,
            "input_sdk_receipt_sha256": SDK_INPUT_RECEIPT_SHA256,
            "input_sdk_candidate": SDK_CANDIDATE_SHA,
            "input_sdk_tree": SDK_CANDIDATE_TREE,
            "input_sdk_parent": SDK_CANDIDATE_PARENT,
            "build_source_branch": BUILD_SOURCE_BRANCH,
            "build_source_sha": BUILD_SOURCE_SHA,
            "build_source_tree": BUILD_SOURCE_TREE,
            "build_source_parent": BUILD_SOURCE_PARENT,
            "mutable_path_policy": "exact-build-source-plus-allowed-generated-subset",
            "allowed_mutable_path_count": len(ALLOWED_MUTABLE_PATHS),
            "allowed_mutable_path_set_sha256": path_digest(ALLOWED_MUTABLE_PATHS),
            "actual_changed_path_count": len(candidate_paths),
            "actual_changed_path_set_sha256": path_digest(candidate_paths),
            "actual_changed_paths": candidate_paths,
            "candidate_sha": candidate_sha,
            "candidate_tree": candidate_tree,
            "candidate_parent": SDK_CANDIDATE_SHA,
            "candidate_bundle_heads": emitted_heads,
            "candidate_bundle_sha256": digest(candidate_bundle),
            "disposition_receipt_sha256": digest(receipt_path),
            "verified_composite_runtime_inputs": verified_runtime_inputs,
            "execution_input_receipt_sha256": digest(preflight_path),
            "execution_inputs": execution_inputs,
            "tool_observations": tool_observations,
            "generator_identity": generator_identity,
        }
        provenance_path = output / "provenance.json"
        provenance_path.write_text(json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        require(load(receipt_path) == receipt, "emitted disposition receipt readback mismatch")
        require(load(provenance_path) == provenance, "emitted provenance readback mismatch")

    print(json.dumps(provenance, sort_keys=True))


if __name__ == "__main__":
    main()
