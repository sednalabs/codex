#!/usr/bin/env python3
"""Read exact bounded metadata from the accepted SDK artifact for w13825.

Trusted code comes from the reviewed workflow checkout.  The accepted SDK
artifact and reviewed build-source commit are immutable data inputs.  The
reader verifies the accepted artifact, bundle, lineage, SDK source entries,
and retained patch dependencies; reports four bounded runtime-input
observations plus two materialized TUI entry identities; and stops before any
install, generation, test, candidate, or bundle-output operation.
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
import tomllib
from typing import Any


REPOSITORY = "sednalabs/codex"
REPOSITORY_ID = "1152496647"
WORKFLOW_PATH = ".github/workflows/apply-upstream-cohort.yml"
VALIDATION_BRANCH = "worker/w13825-sdk-build-consumer"
VALIDATION_REF = f"refs/heads/{VALIDATION_BRANCH}"
PUSH_PREDECESSOR_SHA = "8bdc3fca8c8796c12dd8c2aff4946ba2a5e73c60"

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

RUNTIME_INPUT_PATHS = (
    "sdk/python/pyproject.toml",
    "sdk/python/uv.lock",
)
RUNTIME_INPUT_REVISIONS = (
    ("materialized-parent", MATERIALIZED_SHA),
    ("sdk-candidate", SDK_CANDIDATE_SHA),
)
MATERIALIZED_TUI_IDENTITY_PATHS = (
    "codex-rs/tui/src/bottom_pane/approval_overlay.rs",
    "codex-rs/tui/src/chatwidget/interrupts.rs",
)
MAX_RUNTIME_INPUT_BYTES = 4 * 1024 * 1024

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
PNPM_VERSION = "10.33.0"
PACKAGE_MANAGER = (
    "pnpm@10.33.0+sha512.10568bb4a6afb58c9eb3630da90cc9516417abebd3fabbe6739f0ae795728da1491e9db5a544c76ad8eb7570f5c4bb3d6c637b2cb41bfdcdb47fa823c8649319"
)
SHA_PATTERN = re.compile(r"[0-9a-f]{40}")

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

    require(run("git", "rev-parse", f"{SDK_CANDIDATE_SHA}^{{tree}}", cwd=repo).strip() == SDK_CANDIDATE_TREE, "SDK candidate tree mismatch")
    require(run("git", "show", "-s", "--format=%P", SDK_CANDIDATE_SHA, cwd=repo).split() == [MATERIALIZED_SHA], "SDK candidate parent mismatch")
    require(run("git", "rev-parse", f"{MATERIALIZED_SHA}^{{tree}}", cwd=repo).strip() == MATERIALIZED_TREE, "materialized tree mismatch")
    require(run("git", "show", "-s", "--format=%P", MATERIALIZED_SHA, cwd=repo).split() == [BASE_SHA], "materialized parent mismatch")
    require(run("git", "rev-parse", f"{SDK_SOURCE_SHA}^{{tree}}", cwd=repo).strip() == SDK_SOURCE_TREE, "SDK source tree mismatch")
    require(run("git", "show", "-s", "--format=%P", SDK_SOURCE_SHA, cwd=repo).split() == [BASE_SHA], "SDK source parent mismatch")


def verify_sdk_input_entries(repo: pathlib.Path, receipt: dict[str, Any]) -> None:
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


def bounded_single_value(values: list[str]) -> dict[str, Any]:
    distinct = sorted(set(values))
    if not distinct:
        return {"status": "missing", "value": None, "match_count": 0}
    if len(distinct) != 1:
        return {"status": "ambiguous", "value": None, "match_count": len(distinct)}
    value = distinct[0]
    if len(value) > 96:
        return {"status": "oversized", "value": None, "match_count": 1}
    return {"status": "observed", "value": value, "match_count": 1}


def runtime_fields(repo: pathlib.Path, path: str, entry: tuple[str, str, str] | None) -> dict[str, Any]:
    if entry is None:
        return {"parse_status": "entry-missing"}
    if entry[1] != "blob":
        return {"parse_status": "entry-not-blob"}
    oid = entry[2]
    try:
        size = int(run("git", "cat-file", "-s", oid, cwd=repo).strip())
    except (subprocess.CalledProcessError, ValueError):
        return {"parse_status": "blob-size-read-error"}
    if size > MAX_RUNTIME_INPUT_BYTES:
        return {"parse_status": "blob-too-large", "blob_size": size}
    try:
        data = tomllib.loads(run_bytes("git", "cat-file", "blob", oid, cwd=repo).decode("utf-8"))
    except (subprocess.CalledProcessError, UnicodeDecodeError, tomllib.TOMLDecodeError):
        return {"parse_status": "toml-read-error", "blob_size": size}

    if path == "sdk/python/pyproject.toml":
        project = data.get("project", {})
        dependencies = project.get("dependencies", []) if isinstance(project, dict) else []
        matches = (
            [value for value in dependencies if isinstance(value, str) and value.startswith("openai-codex-cli-bin")]
            if isinstance(dependencies, list)
            else []
        )
        return {
            "parse_status": "parsed",
            "blob_size": size,
            "runtime_requirement": bounded_single_value(matches),
        }

    packages = data.get("package", [])
    package_versions: list[str] = []
    requirement_specifiers: list[str] = []
    if isinstance(packages, list):
        for package in packages:
            if not isinstance(package, dict):
                continue
            if package.get("name") == "openai-codex-cli-bin" and isinstance(package.get("version"), str):
                package_versions.append(package["version"])
            if package.get("name") != "openai-codex":
                continue
            metadata = package.get("metadata", {})
            requires_dist = metadata.get("requires-dist", []) if isinstance(metadata, dict) else []
            if not isinstance(requires_dist, list):
                continue
            for requirement in requires_dist:
                if (
                    isinstance(requirement, dict)
                    and requirement.get("name") == "openai-codex-cli-bin"
                    and isinstance(requirement.get("specifier"), str)
                ):
                    requirement_specifiers.append(requirement["specifier"])
    return {
        "parse_status": "parsed",
        "blob_size": size,
        "runtime_package_version": bounded_single_value(package_versions),
        "runtime_requirement_specifier": bounded_single_value(requirement_specifiers),
    }


def collect_metadata_observations(repo: pathlib.Path) -> list[dict[str, Any]]:
    observations: list[dict[str, Any]] = []
    for role, revision in RUNTIME_INPUT_REVISIONS:
        for path in RUNTIME_INPUT_PATHS:
            try:
                entry = tree_entry(repo, revision, path)
                entry_status = "observed" if entry is not None else "missing"
            except subprocess.CalledProcessError:
                entry = None
                entry_status = "tree-read-error"
            except (SystemExit, UnicodeDecodeError, ValueError):
                entry = None
                entry_status = "invalid-tree-entry"
            observations.append(
                {
                    "observation_kind": "runtime-input",
                    "role": role,
                    "revision": revision,
                    "path": path,
                    "entry_status": entry_status,
                    "entry": tuple_json(entry),
                    "runtime": runtime_fields(repo, path, entry),
                }
            )
    for path in MATERIALIZED_TUI_IDENTITY_PATHS:
        try:
            entry = tree_entry(repo, MATERIALIZED_SHA, path)
            entry_status = "observed" if entry is not None else "missing"
        except subprocess.CalledProcessError:
            entry = None
            entry_status = "tree-read-error"
        except (SystemExit, UnicodeDecodeError, ValueError):
            entry = None
            entry_status = "invalid-tree-entry"
        observations.append(
            {
                "observation_kind": "tree-entry-identity",
                "role": "materialized-parent",
                "revision": MATERIALIZED_SHA,
                "path": path,
                "entry_status": entry_status,
                "entry": tuple_json(entry),
            }
        )
    return observations


def observation_complete(observation: dict[str, Any]) -> bool:
    if observation["entry_status"] != "observed":
        return False
    if observation["observation_kind"] == "tree-entry-identity":
        return True
    runtime = observation["runtime"]
    if runtime.get("parse_status") != "parsed":
        return False
    required_fields = (
        ("runtime_requirement",)
        if observation["path"] == "sdk/python/pyproject.toml"
        else ("runtime_package_version", "runtime_requirement_specifier")
    )
    return all(runtime.get(field, {}).get("status") == "observed" for field in required_fields)


def require_source_pins(worktree: pathlib.Path) -> dict[str, str]:
    pyproject = tomllib.loads((worktree / "sdk/python/pyproject.toml").read_text(encoding="utf-8"))
    dependencies = pyproject.get("project", {}).get("dependencies", [])
    require(isinstance(dependencies, list), "SDK Python dependencies must be a list")
    runtime_dependencies = [
        value for value in dependencies if isinstance(value, str) and value.startswith("openai-codex-cli-bin")
    ]
    observed_runtime_dependencies = [value[:96] for value in runtime_dependencies[:3]]
    require(
        runtime_dependencies == [SDK_RUNTIME_DEPENDENCY],
        f"SDK runtime dependency pin mismatch: expected={SDK_RUNTIME_DEPENDENCY} "
        f"observed={observed_runtime_dependencies}",
    )
    root_package = load(worktree / "package.json")
    sdk_package = load(worktree / "sdk/typescript/package.json")
    require(root_package.get("packageManager") == PACKAGE_MANAGER, "root pnpm package-manager pin mismatch")
    require(sdk_package.get("packageManager") == PACKAGE_MANAGER, "SDK pnpm package-manager pin mismatch")
    uv_identity = run("uv", "--version", cwd=worktree).strip()
    require(uv_identity == f"uv {UV_VERSION}", "uv version mismatch")
    pnpm_identity = run("pnpm", "--version", cwd=worktree).strip()
    require(pnpm_identity == PNPM_VERSION, "pnpm version mismatch")
    node_identity = run("node", "--version", cwd=worktree).strip()
    require(re.fullmatch(r"v22(?:\.\d+){2}", node_identity) is not None, "Node major version mismatch")
    bazel_pin = (worktree / ".bazelversion").read_text(encoding="utf-8").strip()
    bazel_identity = run("bazel", "--version", cwd=worktree).strip()
    require(bazel_identity.endswith(f" {bazel_pin}"), "Bazel version does not match .bazelversion")
    return {
        "sdk_generator": "sdk/python/scripts/update_sdk_artifacts.py generate-types",
        "sdk_runtime_dependency": SDK_RUNTIME_DEPENDENCY,
        "uv": uv_identity,
        "pnpm_package_manager": PACKAGE_MANAGER,
        "pnpm": pnpm_identity,
        "node": node_identity,
        "bazel_pin": bazel_pin,
        "bazel": bazel_identity,
    }


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


def generate_and_test(worktree: pathlib.Path, temp: pathlib.Path) -> dict[str, Any]:
    generator_identity = require_source_pins(worktree)
    python_project = worktree / "sdk/python"
    generation_env = {**os.environ, "UV_PROJECT_ENVIRONMENT": str(temp / "sdk-python-venv"), "UV_LINK_MODE": "copy"}
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
    return generator_identity


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
    parser.add_argument("--output-dir", type=pathlib.Path)
    parser.add_argument("--expected-workflow-sha", required=True)
    parser.add_argument("--expected-workflow-tree", required=True)
    parser.add_argument("--validate-runtime-only", action="store_true")
    parser.add_argument("--metadata-readback-only", action="store_true")
    args = parser.parse_args()

    runtime = verify_runtime(args.expected_workflow_sha, args.expected_workflow_tree)
    if args.validate_runtime_only:
        require(
            args.repo_root is None
            and args.artifact_dir is None
            and args.output_dir is None
            and not args.metadata_readback_only,
            "runtime-only validation does not accept consumer paths",
        )
        print(json.dumps(runtime, sort_keys=True))
        return
    require(args.metadata_readback_only, "only metadata readback is enabled")
    require(
        args.repo_root is not None and args.artifact_dir is not None and args.output_dir is None,
        "metadata readback requires repository and artifact inputs only",
    )
    repo = absolute_argument(args.repo_root, "repo-root", must_exist=True)
    artifact = absolute_argument(args.artifact_dir, "artifact-dir", must_exist=True)
    require(repo.is_dir() and artifact.is_dir(), "repository and artifact inputs must be directories")
    verify_build_source_checkout(repo)
    files = verify_sdk_artifact_files(artifact)
    with tempfile.TemporaryDirectory(prefix="w13825-sdk-metadata-", dir=str(artifact.parent)) as temp_name:
        temp = pathlib.Path(temp_name).resolve(strict=True)
        import_sdk_bundle(repo, files["bundle"], temp)
        sdk_receipt = load(files["receipt"])
        verify_sdk_input_entries(repo, sdk_receipt)
        observations = collect_metadata_observations(repo)
    readback_complete = all(observation_complete(observation) for observation in observations)
    runtime_observation_count = len(RUNTIME_INPUT_REVISIONS) * len(RUNTIME_INPUT_PATHS)
    tui_identity_observation_count = len(MATERIALIZED_TUI_IDENTITY_PATHS)
    evidence = {
        "schema": "accepted-composite-metadata-readback",
        "version": 1,
        "evidence_only": True,
        "consumer_acceptance": "not-evaluated",
        "candidate_generation": "not-started",
        "candidate_bundle": "not-created",
        "repository": REPOSITORY,
        **runtime,
        "input_sdk_artifact_id": SDK_INPUT_ARTIFACT_ID,
        "input_sdk_artifact_name": SDK_INPUT_ARTIFACT_NAME,
        "input_sdk_archive_sha256": SDK_INPUT_ARCHIVE_SHA256,
        "input_sdk_bundle_sha256": SDK_INPUT_BUNDLE_SHA256,
        "input_sdk_receipt_sha256": SDK_INPUT_RECEIPT_SHA256,
        "materialized_sha": MATERIALIZED_SHA,
        "sdk_candidate_sha": SDK_CANDIDATE_SHA,
        "expected_runtime_observation_count": runtime_observation_count,
        "expected_materialized_tui_identity_observation_count": tui_identity_observation_count,
        "expected_observation_count": runtime_observation_count + tui_identity_observation_count,
        "observation_count": len(observations),
        "readback_status": "complete" if readback_complete else "evidence-gap",
        "observations": observations,
    }
    print(json.dumps(evidence, sort_keys=True))
    return
    require(
        args.repo_root is not None and args.artifact_dir is not None and args.output_dir is not None,
        "consumer paths are required",
    )
    repo = absolute_argument(args.repo_root, "repo-root", must_exist=True)
    artifact = absolute_argument(args.artifact_dir, "artifact-dir", must_exist=True)
    output = absolute_argument(args.output_dir, "output-dir", must_exist=False)
    require(repo.is_dir() and artifact.is_dir(), "repository and artifact inputs must be directories")
    require(output.parent.is_dir(), "output parent must already exist")
    require(not output.exists(), "output directory already exists")
    verify_build_source_checkout(repo)
    files = verify_sdk_artifact_files(artifact)
    output.mkdir()

    with tempfile.TemporaryDirectory(prefix="w13825-sdk-build-", dir=str(output.parent)) as temp_name:
        temp = pathlib.Path(temp_name).resolve(strict=True)
        import_sdk_bundle(repo, files["bundle"], temp)
        sdk_receipt = load(files["receipt"])
        verified_runtime_inputs = verify_sdk_input_entries(repo, sdk_receipt)

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

        generator_identity = generate_and_test(worktree, temp)
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
            "generator_identity": generator_identity,
        }
        provenance_path = output / "provenance.json"
        provenance_path.write_text(json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        require(load(receipt_path) == receipt, "emitted disposition receipt readback mismatch")
        require(load(provenance_path) == provenance, "emitted provenance readback mismatch")

    print(json.dumps(provenance, sort_keys=True))


if __name__ == "__main__":
    main()
