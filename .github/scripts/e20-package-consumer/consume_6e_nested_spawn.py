#!/usr/bin/env python3
"""Run one reviewed smoke test against one immutable packaged x86 artifact."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import subprocess
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


PRODUCT_RUN_ID = "38682165848"
PRODUCT_RUN_NUMBER = 179
PRODUCT_SOURCE = "6e1c30c85d5e0e4f6f147d3509868c9821d4bcba"
PRODUCT_TREE = "cbd495babf5b31f7aaffe081236c0e3892e0194f"
PRODUCT_BRANCH = "integration/be2e-e926-20261009"
PRODUCT_WORKFLOW = ".github/workflows/sedna-first-package.yml"
PRODUCT_WORKFLOW_HOST = "4d1a32337baab1a6745a45003f6c2d96f0cc925f"
PRODUCT_WORKFLOW_HOST_TREE = PRODUCT_TREE
PRODUCT_PR_NUMBER = 937
PRODUCT_BASE_SHA = "703389678a1a6c3a9d131245d03d3f1805b9afa1"
PRODUCT_BUILD_JOB_ID = 116251970393
PRODUCT_ARTIFACT_ID = 11951945717
PRODUCT_ARTIFACT_NAME = "sedna-first-package-6e1c30c8-x86_64-38682165848"
PRODUCT_ARTIFACT_DIGEST = (
    "sha256:90372839e35e400d0bc81f760d2748cb969a9e253651aba926cfcda4bf5b1aa6"
)
PRODUCT_ARTIFACT_SIZE = 1198826103
PRODUCT_VERSION_BASE = "0.146.0-alpha.8-sedna.10+upstream.8593"
FIXTURE_SOURCE = "58d392e66ef61a7d195f2155a84b1d56d36224ec"
FIXTURE_TREE = "69cdf5f6d88cc5fece086ea57221fed35463f824"
FIXTURE_PARENT = PRODUCT_SOURCE
FIXTURE_PATH = (
    "scripts/codex_package/smoke_tests/first_binary/"
    "test_agent_control_model_acceptance.py"
)
FIXTURE_BLOB = "41ca2f33dbb04d18d529d89cb7ecf6d9893b8094"
CONSUMER_BRANCH = "validation/w14962-e20-packaged-consumer-20261015"
CONSUMER_BASE_SHA = "703389678a1a6c3a9d131245d03d3f1805b9afa1"
ARCHIVE_FILES = {
    "codex-cli-x86_64-unknown-linux-gnu.tar.gz",
    "codex-app-server-x86_64-unknown-linux-gnu.tar.gz",
    "codex-responses-api-proxy",
    "release-manifest.json",
    "SHA256SUMS.txt",
    "symbols.tar.gz",
}
PAYLOAD_FILES = ARCHIVE_FILES - {"release-manifest.json", "SHA256SUMS.txt"}
SMOKE_TEST = (
    "scripts/codex_package/smoke_tests/first_binary/"
    "test_agent_control_model_acceptance.py::"
    "test_packaged_model_nested_spawn_recovery_and_list_after_resume"
)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(message)


def env(name: str) -> str:
    value = os.environ.get(name, "")
    require(bool(value), f"Missing required workflow value: {name}.")
    return value


def github_api(path: str) -> dict[str, Any]:
    request = urllib.request.Request(
        f"https://api.github.com{path}",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {env('GH_TOKEN')}",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.loads(response.read())
    except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError) as error:
        raise SystemExit("Exact producer provenance lookup failed.") from error
    require(isinstance(payload, dict), "GitHub returned unexpected provenance data.")
    return payload


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def check_consumer_checkout(workspace: Path, fixture: Path) -> None:
    require(
        env("GITHUB_EVENT_NAME") == "pull_request",
        "This one-use consumer accepts only the scoped pull_request event.",
    )
    event = json.loads(Path(env("GITHUB_EVENT_PATH")).read_text())
    pull = event.get("pull_request") or {}
    repo = env("GITHUB_REPOSITORY")
    require(repo == "sednalabs/codex", "Unexpected consumer repository.")
    require(
        ((pull.get("head") or {}).get("repo") or {}).get("full_name") == repo,
        "The consumer requires a same-repository pull request.",
    )
    require(
        (pull.get("head") or {}).get("ref") == CONSUMER_BRANCH,
        "Unexpected consumer branch.",
    )
    require(
        (pull.get("base") or {}).get("ref") == "main"
        and (pull.get("base") or {}).get("sha") == CONSUMER_BASE_SHA,
        "Consumer pull-request base differs from the frozen harness base.",
    )
    head = (pull.get("head") or {}).get("sha")
    require(
        isinstance(head, str)
        and re.fullmatch(r"[0-9a-f]{40}", head) is not None,
        "Malformed consumer head identity.",
    )
    actual_head = subprocess.check_output(
        ["git", "-C", str(workspace), "rev-parse", "HEAD"], text=True
    ).strip()
    require(actual_head == head, "Consumer checkout is not the exact PR head.")

    fixture_head = subprocess.check_output(
        ["git", "-C", str(fixture), "rev-parse", "HEAD"], text=True
    ).strip()
    fixture_tree = subprocess.check_output(
        ["git", "-C", str(fixture), "rev-parse", "HEAD^{tree}"], text=True
    ).strip()
    fixture_parent = subprocess.check_output(
        ["git", "-C", str(fixture), "rev-parse", "HEAD^"], text=True
    ).strip()
    changed = subprocess.check_output(
        [
            "git",
            "-C",
            str(fixture),
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "HEAD",
        ],
        text=True,
    ).splitlines()
    blob = subprocess.check_output(
        ["git", "-C", str(fixture), "rev-parse", f"HEAD:{FIXTURE_PATH}"],
        text=True,
    ).strip()
    require(fixture_head == FIXTURE_SOURCE, "Smoke-test source SHA mismatch.")
    require(fixture_tree == FIXTURE_TREE, "Smoke-test source tree mismatch.")
    require(fixture_parent == FIXTURE_PARENT, "Smoke-test source parent mismatch.")
    require(changed == [FIXTURE_PATH], "Smoke-test source path scope changed.")
    require(blob == FIXTURE_BLOB, "Reviewed smoke-test blob mismatch.")


def verify_producer(repo: str) -> None:
    run = github_api(f"/repos/{repo}/actions/runs/{PRODUCT_RUN_ID}")
    expected_run = {
        "id": int(PRODUCT_RUN_ID),
        "path": PRODUCT_WORKFLOW,
        "head_branch": PRODUCT_BRANCH,
        "head_sha": PRODUCT_SOURCE,
        "event": "pull_request",
        "status": "completed",
        "conclusion": "failure",
        "run_attempt": 1,
        "run_number": PRODUCT_RUN_NUMBER,
    }
    for key, value in expected_run.items():
        require(run.get(key) == value, f"Product run identity mismatch: {key}.")
    require(
        (run.get("head_repository") or {}).get("full_name") == repo,
        "Product run repository mismatch.",
    )
    pull_requests = run.get("pull_requests")
    require(
        isinstance(pull_requests, list) and len(pull_requests) == 1,
        "Product run does not bind to exactly one PR.",
    )
    pull = pull_requests[0]
    require(pull.get("number") == PRODUCT_PR_NUMBER, "Product PR number mismatch.")
    require(
        (pull.get("base") or {}).get("ref") == "main"
        and (pull.get("base") or {}).get("sha") == PRODUCT_BASE_SHA,
        "Product PR base mismatch.",
    )
    require(
        (pull.get("head") or {}).get("ref") == PRODUCT_BRANCH
        and (pull.get("head") or {}).get("sha") == PRODUCT_SOURCE,
        "Product PR head mismatch.",
    )

    jobs = github_api(
        f"/repos/{repo}/actions/runs/{PRODUCT_RUN_ID}/jobs?per_page=100"
    ).get("jobs", [])
    job = next(
        (candidate for candidate in jobs if candidate.get("id") == PRODUCT_BUILD_JOB_ID),
        None,
    )
    require(job is not None, "Exact product build job is absent.")
    require(
        job.get("name")
        == "Build exact Linux package pair (x86_64-unknown-linux-gnu)"
        and job.get("status") == "completed"
        and job.get("conclusion") == "success",
        "Exact x86 product build/upload job did not succeed.",
    )
    require(
        job.get("runner_group_name") == "GitHub Actions"
        and job.get("labels") == ["ubuntu-24.04"],
        "Product package was not produced on the standard x86 GitHub runner.",
    )
    upload = next(
        (
            step
            for step in job.get("steps", [])
            if step.get("name") == "Upload exact package, helper, and symbol artifacts"
        ),
        None,
    )
    require(
        upload is not None and upload.get("conclusion") == "success",
        "Exact x86 package artifact upload did not succeed.",
    )

    artifacts = github_api(
        f"/repos/{repo}/actions/runs/{PRODUCT_RUN_ID}/artifacts?per_page=100"
    ).get("artifacts", [])
    artifact = next(
        (item for item in artifacts if item.get("id") == PRODUCT_ARTIFACT_ID), None
    )
    require(artifact is not None, "Exact product artifact is absent.")
    expected_artifact = {
        "name": PRODUCT_ARTIFACT_NAME,
        "digest": PRODUCT_ARTIFACT_DIGEST,
        "size_in_bytes": PRODUCT_ARTIFACT_SIZE,
        "expired": False,
    }
    for key, value in expected_artifact.items():
        require(artifact.get(key) == value, f"Product artifact mismatch: {key}.")


def verify_bundle(artifact_dir: Path, repo: str) -> tuple[Path, Path, Path]:
    entries = list(artifact_dir.iterdir())
    require(
        all(path.is_file() and not path.is_symlink() for path in entries),
        "Downloaded product artifact contains an unexpected directory or link.",
    )
    require(
        {path.name for path in entries} == ARCHIVE_FILES,
        "Downloaded product artifact has an unexpected payload shape.",
    )
    manifest = json.loads((artifact_dir / "release-manifest.json").read_text())
    expected_manifest = {
        "repository": repo,
        "sourceCommit": PRODUCT_SOURCE,
        "sourceTree": PRODUCT_TREE,
        "workflowHostCommit": PRODUCT_WORKFLOW_HOST,
        "workflowHostTree": PRODUCT_WORKFLOW_HOST_TREE,
        "workflowRunId": PRODUCT_RUN_ID,
        "target": "x86_64-unknown-linux-gnu",
        "packageVersion": (
            f"{PRODUCT_VERSION_BASE}.ci.{PRODUCT_RUN_NUMBER}.g{PRODUCT_SOURCE[:8]}"
        ),
        "workflow": (
            f"https://github.com/{repo}/actions/runs/{PRODUCT_RUN_ID}"
        ),
    }
    for key, value in expected_manifest.items():
        require(manifest.get(key) == value, f"Product manifest mismatch: {key}.")

    hashes: dict[str, str] = {}
    for line in (artifact_dir / "SHA256SUMS.txt").read_text().splitlines():
        parts = line.split(maxsplit=1)
        require(len(parts) == 2, "Malformed product checksum line.")
        digest, filename = parts
        filename = filename.lstrip("* ")
        require(filename not in hashes, "Duplicate product checksum entry.")
        hashes[filename] = digest
    require(
        set(hashes) == PAYLOAD_FILES | {"release-manifest.json"},
        "Product checksum file covers an unexpected payload set.",
    )
    for filename, expected_digest in hashes.items():
        actual_digest = sha256_file(artifact_dir / filename)
        require(actual_digest == expected_digest, f"Product payload hash mismatch: {filename}.")
    manifest_artifacts = manifest.get("artifacts")
    require(isinstance(manifest_artifacts, dict), "Product manifest artifact map is invalid.")
    require(
        set(manifest_artifacts) == PAYLOAD_FILES,
        "Product manifest artifact set mismatch.",
    )
    for filename in PAYLOAD_FILES:
        require(
            manifest_artifacts.get(filename) == hashes[filename],
            f"Manifest payload hash mismatch: {filename}.",
        )

    return (
        artifact_dir / "codex-cli-x86_64-unknown-linux-gnu.tar.gz",
        artifact_dir / "codex-app-server-x86_64-unknown-linux-gnu.tar.gz",
        artifact_dir / "symbols.tar.gz",
    )


def run_exact_test(
    fixture_source: Path, cli_archive: Path, app_server_archive: Path, symbols: Path
) -> None:
    require(
        platform.system() == "Linux" and platform.machine() in {"x86_64", "amd64"},
        "This exact consumer requires the standard x86_64 Linux runner.",
    )
    smoke_project = fixture_source / "scripts/codex_package/smoke_tests"
    project_file = smoke_project / "pyproject.toml"
    lock_file = smoke_project / "uv.lock"
    require(project_file.is_file() and lock_file.is_file(), "Pinned smoke-test project is absent.")
    test_file = fixture_source / FIXTURE_PATH
    require(test_file.is_file(), "Exact nested-spawn test source is absent.")

    child_env = dict(os.environ)
    for secret_name in ("GH_TOKEN", "GITHUB_TOKEN", "ACTIONS_RUNTIME_TOKEN"):
        child_env.pop(secret_name, None)
    command = [
        "uv",
        "run",
        "--project",
        str(smoke_project),
        "--frozen",
        "--no-sync",
        "pytest",
        "-q",
        SMOKE_TEST,
        "--compression",
        "gzip",
        "--package-target",
        "x86_64-unknown-linux-gnu",
        "--cli-archive",
        str(cli_archive),
        "--app-server-archive",
        str(app_server_archive),
        "--symbols-archive",
        str(symbols),
        "--junitxml",
        str(Path(env("RUNNER_TEMP")) / "6e-nested-spawn-consumer.xml"),
    ]
    subprocess.run(command, cwd=fixture_source, env=child_env, check=True)


def main() -> None:
    workspace = Path(env("GITHUB_WORKSPACE")).resolve()
    fixture_source = Path(env("FIXTURE_SOURCE_DIR")).resolve()
    artifact_dir = Path(env("PRODUCT_ARTIFACT_DIR")).resolve()
    require(env("GITHUB_REPOSITORY") == "sednalabs/codex", "Unexpected consumer repository.")
    check_consumer_checkout(workspace, fixture_source)
    verify_producer(env("GITHUB_REPOSITORY"))
    cli_archive, app_server_archive, symbols = verify_bundle(
        artifact_dir, env("GITHUB_REPOSITORY")
    )
    run_exact_test(fixture_source, cli_archive, app_server_archive, symbols)
    print(
        "PASS: exact nested-spawn smoke test against product source "
        f"{PRODUCT_SOURCE}, artifact {PRODUCT_ARTIFACT_ID}, fixture {FIXTURE_SOURCE}."
    )


if __name__ == "__main__":
    main()
