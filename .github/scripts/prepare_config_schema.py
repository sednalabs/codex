"""Prepare the canonical config schema from one exact hosted source revision."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import stat
import subprocess
import sys


DEFAULT_PROFILE = "cargo-schema"
PROFILES = {DEFAULT_PROFILE, "config-schema"}
MODES = {"build", "prepare-only", "consume-existing"}
GENERATOR_ARGV = [
    "cargo",
    "run",
    "--locked",
    "-p",
    "codex-config-schema",
    "--bin",
    "codex-write-config-schema",
]
SCHEMA_PATH = "codex-rs/core/config.schema.json"
LOCK_PATH = "codex-rs/Cargo.lock"


def resolve_profile(profile: str | None) -> str:
    """Apply the legacy default and reject every profile outside the closed set."""
    resolved = DEFAULT_PROFILE if profile is None else profile
    if resolved not in PROFILES:
        raise ValueError("unsupported or empty preparation profile")
    return resolved


def validate_profile(mode: str, profile: str | None) -> str:
    if mode not in MODES:
        raise ValueError("unsupported workflow mode")
    resolved = resolve_profile(profile)
    if resolved != DEFAULT_PROFILE and mode != "prepare-only":
        raise ValueError("config-schema preparation profile is restricted to prepare-only mode")
    return resolved


def _git(root: Path, *args: str) -> bytes:
    return subprocess.check_output(
        ["git", "-C", str(root), *args], stderr=subprocess.DEVNULL
    )


def _tracked_regular_file(root: Path, relative_path: str) -> bytes:
    path = root / relative_path
    try:
        path_stat = path.lstat()
    except FileNotFoundError as error:
        raise ValueError(f"expected a tracked regular file: {relative_path}") from error
    if path.is_symlink() or not stat.S_ISREG(path_stat.st_mode):
        raise ValueError(f"expected a tracked regular file: {relative_path}")
    _git(root, "ls-files", "--error-unmatch", "--", relative_path)
    return path.read_bytes()


def validate_input_identity(environment: dict[str, str], workspace: Path) -> dict[str, str]:
    expected_h = environment.get("EXPECTED_H", "")
    target_sha = environment.get("TARGET_SHA", "")
    base_sha = environment.get("BASE_SHA", "")
    for label, value in (("H", expected_h), ("T", target_sha), ("B", base_sha)):
        if re.fullmatch(r"[0-9a-f]{40}", value) is None:
            raise ValueError(f"{label} must be a full lowercase commit SHA")

    if environment.get("GITHUB_REPOSITORY") != "sednalabs/codex":
        raise ValueError("unexpected repository identity")
    actual_h = _git(workspace / ".workflow-src", "rev-parse", "HEAD").decode().strip()
    if actual_h != expected_h:
        raise ValueError("workflow host H does not match the dispatched identity")

    product_root = workspace / "product"
    if _git(product_root, "rev-parse", "HEAD").decode().strip() != target_sha:
        raise ValueError("product source T does not match the dispatched identity")
    fetched_base = _git(product_root, "rev-parse", "FETCH_HEAD^{commit}").decode().strip()
    if fetched_base != base_sha:
        raise ValueError("comparison base B does not match the fetched identity")
    _git(product_root, "cat-file", "-e", f"{base_sha}^{{commit}}")
    if platform.machine() != "x86_64":
        raise ValueError("config-schema preparation requires standard x86_64 architecture")

    run_id = environment.get("GITHUB_RUN_ID", "")
    attempt = environment.get("GITHUB_RUN_ATTEMPT", "")
    if re.fullmatch(r"[1-9][0-9]*", run_id) is None:
        raise ValueError("workflow run ID is malformed")
    if re.fullmatch(r"[1-9][0-9]*", attempt) is None:
        raise ValueError("workflow run attempt is malformed")
    if environment.get("GITHUB_WORKFLOW") != "sedna-branch-build":
        raise ValueError("unexpected workflow identity")
    if not environment.get("GITHUB_SERVER_URL"):
        raise ValueError("workflow identity is incomplete")

    return {
        "workflow_host_sha": expected_h,
        "workflow_host_tree": _git(
            workspace / ".workflow-src", "rev-parse", "HEAD^{tree}"
        ).decode().strip(),
        "product_sha": target_sha,
        "product_tree": _git(product_root, "rev-parse", "HEAD^{tree}").decode().strip(),
        "comparison_base_sha": base_sha,
        "comparison_base_tree": _git(
            product_root, "rev-parse", f"{base_sha}^{{tree}}"
        ).decode().strip(),
        "workflow_run_id": run_id,
        "workflow_run_attempt": attempt,
    }


def validate_generated_output(
    product_root: Path, schema_before: bytes, lock_before: bytes
) -> tuple[bytes, list[str]]:
    schema_path = product_root / SCHEMA_PATH
    lock_after = _tracked_regular_file(product_root, LOCK_PATH)
    if lock_after != lock_before:
        raise ValueError("config-schema generation changed Cargo.lock")
    if schema_path.is_symlink():
        raise ValueError("generated config schema must remain a regular nonsymlink file")

    expected_status = f" M {SCHEMA_PATH}\0".encode()
    actual_status = _git(
        product_root, "status", "--porcelain=v1", "-z", "--untracked-files=all"
    )
    if actual_status != expected_status:
        raise ValueError("config-schema generation must change only the exact schema path")
    try:
        schema_stat = schema_path.lstat()
    except FileNotFoundError as error:
        raise ValueError(
            "config-schema generation must change only the exact schema path"
        ) from error
    if not stat.S_ISREG(schema_stat.st_mode):
        raise ValueError("generated config schema must remain a regular nonsymlink file")
    schema_after = schema_path.read_bytes()
    if not schema_after:
        raise ValueError("generated config schema is empty")
    try:
        json.loads(schema_after)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError("generated config schema is not valid JSON") from error
    changed_paths = _git(
        product_root, "diff", "--name-only", "--no-renames", "HEAD", "--"
    ).decode().splitlines()
    if changed_paths != [SCHEMA_PATH]:
        raise ValueError("config-schema diff contains an unexpected path set")
    patch = _git(
        product_root,
        "diff",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        "HEAD",
        "--",
        SCHEMA_PATH,
    )
    if not patch:
        raise ValueError("config-schema generator produced no source patch")
    return patch, changed_paths


def prepare(environment: dict[str, str]) -> None:
    workspace = Path(environment["GITHUB_WORKSPACE"])
    product_root = workspace / "product"
    codex_root = product_root / "codex-rs"
    output_root = Path(environment["RUNNER_TEMP"])
    identity = validate_input_identity(environment, workspace)

    if _git(product_root, "status", "--porcelain=v1", "-z", "--untracked-files=all"):
        raise ValueError("product checkout is not clean before config-schema preparation")
    schema_before = _tracked_regular_file(product_root, SCHEMA_PATH)
    lock_before = _tracked_regular_file(product_root, LOCK_PATH)

    subprocess.run(GENERATOR_ARGV, cwd=codex_root, check=True)
    patch, changed_paths = validate_generated_output(
        product_root, schema_before, lock_before
    )
    schema_after = (product_root / SCHEMA_PATH).read_bytes()

    if output_root.is_symlink() or not output_root.is_dir():
        raise ValueError("runner temporary output directory is unavailable")
    artifact_identity = {
        "schema_version": "sedna-config-schema-prep-v1",
        "mode": environment["MODE"],
        "preparation_profile": environment["PREPARATION_PROFILE"],
        "repository": environment["GITHUB_REPOSITORY"],
        "workflow": environment["GITHUB_WORKFLOW"],
        "workflow_run": (
            f'{environment["GITHUB_SERVER_URL"]}/{environment["GITHUB_REPOSITORY"]}'
            f'/actions/runs/{identity["workflow_run_id"]}'
        ),
        **identity,
        "runner_label": "ubuntu-24.04",
        "architecture": platform.machine(),
        "generator_argv": GENERATOR_ARGV,
        "generator_cwd": "product/codex-rs",
        "schema_before_sha256": hashlib.sha256(schema_before).hexdigest(),
        "schema_after_sha256": hashlib.sha256(schema_after).hexdigest(),
        "cargo_lock_before_sha256": hashlib.sha256(lock_before).hexdigest(),
        "cargo_lock_after_sha256": hashlib.sha256(lock_before).hexdigest(),
        "source_diff_sha256": hashlib.sha256(patch).hexdigest(),
        "changed_paths": changed_paths,
    }
    (output_root / "schema.before.json").write_bytes(schema_before)
    (output_root / "schema.after.json").write_bytes(schema_after)
    (output_root / "source-delta.patch").write_bytes(patch)
    (output_root / "changed-paths.txt").write_text(
        "".join(f"{path}\n" for path in changed_paths), encoding="utf-8"
    )
    (output_root / "identity.json").write_text(
        json.dumps(artifact_identity, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--validate-profile", action="store_true")
    args = parser.parse_args()
    environment = dict(os.environ)
    try:
        profile = validate_profile(
            environment.get("MODE", ""), environment.get("PREPARATION_PROFILE")
        )
        if args.validate_profile:
            return 0
        if profile != "config-schema":
            raise ValueError("config-schema helper requires the config-schema profile")
        prepare(environment)
    except (KeyError, OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"config-schema preparation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
