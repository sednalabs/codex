#!/usr/bin/env python3
"""Prepare a frozen control-plane candidate on a disposable hosted checkout.

This helper deliberately accepts identities and checkout paths only. The command
sequence, environment, generated-path allowlist, and public receipt vocabulary
are constants, not request data.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from collections import Counter
import tomllib


MAX_PATCH_BYTES = 8 * 1024 * 1024
MAX_RECEIPT_BYTES = 1024 * 1024
SELF_TEST_ARGV = (
    "python3",
    "-m",
    "unittest",
    "discover",
    "-s",
    ".github/scripts",
    "-p",
    "test_prepare_control_plane.py",
)
SOURCE_STYLE_ARGV = (
    "uv",
    "run",
    "--frozen",
    "--project",
    "scripts",
    "ruff",
    "format",
    "--check",
    ".github/scripts/prepare_control_plane.py",
    ".github/scripts/test_prepare_control_plane.py",
)
SOURCE_PATHS = frozenset(
    {
        "codex-rs/cli/Cargo.toml",
        "codex-rs/cli/src/control_plane.rs",
        "codex-rs/cli/src/control_plane/envelopes.rs",
        "codex-rs/cli/src/control_plane/summary.rs",
        "codex-rs/cli/src/control_plane/usage.rs",
        "codex-rs/cli/src/control_plane/usage_tests.rs",
        "codex-rs/cli/src/control_plane_tests.rs",
        "codex-rs/cli/src/main.rs",
        "codex-rs/diagnostics/src/control_plane.rs",
        "codex-rs/diagnostics/src/control_plane/lifecycle_timelines.rs",
        "codex-rs/diagnostics/src/control_plane/recorder.rs",
        "codex-rs/diagnostics/src/control_plane/summary.rs",
        "codex-rs/diagnostics/src/control_plane/types.rs",
        "codex-rs/diagnostics/src/control_plane/usage.rs",
        "codex-rs/diagnostics/src/control_plane/usage_provider_reference.rs",
        "codex-rs/diagnostics/src/control_plane/usage_tests.rs",
        "codex-rs/diagnostics/src/control_plane_tests.rs",
        "codex-rs/diagnostics/src/lib.rs",
        "codex-rs/state/src/control_plane_usage.rs",
        "codex-rs/state/src/control_plane_usage_reader.rs",
        "codex-rs/state/src/control_plane_usage_tests.rs",
        "codex-rs/state/src/lib.rs",
        "docs/divergences/index.yaml",
        "docs/carry-divergence-ledger.md",
        "docs/downstream-regression-matrix.md",
    }
)
ALLOWED_PATHS = SOURCE_PATHS | {"codex-rs/Cargo.lock", "MODULE.bazel.lock"}
PHASES = (
    ("workspace_update", "codex-rs", ("cargo", "update", "--workspace")),
    (
        "fix",
        ".",
        (
            "just",
            "fix",
            "--locked",
            "-p",
            "codex-diagnostics",
            "-p",
            "codex-cli",
            "-p",
            "codex-state",
        ),
    ),
    ("format", ".", ("just", "fmt")),
    ("bazel_lock_update", ".", ("just", "bazel-lock-update")),
    (
        "metadata",
        "codex-rs",
        ("cargo", "metadata", "--locked", "--format-version", "1"),
    ),
    ("format_check", ".", ("just", "fmt-check")),
    (
        "clippy",
        ".",
        (
            "just",
            "clippy",
            "--locked",
            "-p",
            "codex-diagnostics",
            "-p",
            "codex-cli",
            "-p",
            "codex-state",
        ),
    ),
    ("bazel_lock_check", ".", ("just", "bazel-lock-check")),
)
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
REF_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._/-]{0,199}$")
TOOL_VERSION_PATTERNS = {
    "rust": re.compile(r"^rustc (1\.95\.0)(?: \([0-9a-f]{7,} \d{4}-\d{2}-\d{2}\))?$"),
    "rustfmt": re.compile(
        r"^rustfmt (1\.9\.0(?:-stable)?)(?: \([0-9a-f]{7,} \d{4}-\d{2}-\d{2}\))?$"
    ),
    "clippy": re.compile(
        r"^clippy (0\.1\.95)(?: \([0-9a-f]{7,} \d{4}-\d{2}-\d{2}\))?$"
    ),
    "just": re.compile(r"^just (1\.51\.0)$"),
    "uv": re.compile(
        r"^uv (0\.11\.3) \((?:[0-9a-f]{7,40} [0-9]{4}-[0-9]{2}-[0-9]{2} )?"
        r"x86_64-unknown-linux-gnu\)$"
    ),
    "bazelisk": re.compile(r"^Bazelisk v(1\.28\.1)$"),
    "bazel": re.compile(r"^bazel (9\.0\.0)$"),
    "dotslash": re.compile(
        r"^DotSlash ([0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)$"
    ),
}
MAX_VERSION_OUTPUT_BYTES = 256


def parse_version_output(raw: bytes, name: str) -> re.Match[str] | None:
    if len(raw) > MAX_VERSION_OUTPUT_BYTES:
        return None
    try:
        output = raw.decode("utf-8", "strict").replace("\r\n", "\n")
    except UnicodeDecodeError:
        return None
    if output.endswith("\n"):
        output = output[:-1]
    if name == "bazel_version_pair":
        lines = output.split("\n")
        if len(lines) != 2:
            return None
        first = TOOL_VERSION_PATTERNS["bazelisk"].fullmatch(lines[0])
        second = TOOL_VERSION_PATTERNS["bazel"].fullmatch(lines[1])
        if first is None or second is None:
            return None
        return first
    return TOOL_VERSION_PATTERNS[name].fullmatch(output)


class PreparationError(Exception):
    def __init__(self, code: str):
        super().__init__(code)
        self.code = code


class SafeArgumentParser(argparse.ArgumentParser):
    def error(self, message: str) -> None:
        raise PreparationError("invalid_arguments")


def run(
    argv: tuple[str, ...], cwd: Path, env: dict[str, str]
) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        argv,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def git(root: Path, *args: str, env: dict[str, str] | None = None) -> bytes:
    result = run(("git", "-C", str(root), *args), root, env or minimal_env())
    if result.returncode:
        raise PreparationError("git_command_failed")
    return result.stdout


def minimal_env() -> dict[str, str]:
    path = os.environ.get("PATH", "")
    runner_temp = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir())).resolve()
    home = runner_temp / "control-plane-home"
    env = {
        "PATH": path,
        "HOME": str(home),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "PYTHONDONTWRITEBYTECODE": "1",
        "RUFF_CACHE_DIR": str(runner_temp / "control-plane-ruff-cache"),
        "UV_CACHE_DIR": str(runner_temp / "control-plane-uv-cache"),
        "UV_PROJECT_ENVIRONMENT": str(runner_temp / "control-plane-uv-project"),
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
    }
    if os.environ.get("RUSTUP_HOME"):
        env["RUSTUP_HOME"] = os.environ["RUSTUP_HOME"]
    return env


def check_user_configuration(product: Path) -> None:
    home = Path(minimal_env()["HOME"])
    candidates = (
        home / ".cargo" / "config",
        home / ".cargo" / "config.toml",
        home / ".bazelrc",
        home / ".bazeliskrc",
        home / ".config" / "bazel" / "bazelrc",
    )
    if any(path.exists() for path in candidates):
        raise PreparationError("unexpected_user_configuration")
    prohibited = {
        "BAZELRC",
        "BAZELISK_BASE_URL",
        "BAZELISK_GITHUB_TOKEN",
        "BAZELISK_USER_AGENT",
        "USE_BAZEL_VERSION",
        "USE_BAZEL_FALLBACK_VERSION",
        "BUILDBUDDY_API_KEY",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTFLAGS",
    }
    if any(os.environ.get(key) for key in prohibited):
        raise PreparationError("unexpected_execution_environment")
    for relative in ("user.bazelrc", ".bazelrc.local", ".bazeliskrc"):
        if (product / relative).exists():
            raise PreparationError("unexpected_product_bazel_configuration")
    check_product_bazelrc(product)


def check_product_bazelrc(product: Path) -> None:
    try:
        lines = (product / ".bazelrc").read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError) as exc:
        raise PreparationError("product_bazelrc_unreadable") from exc
    imports = []
    active_scopes = {"", "linux"}
    for line in lines:
        active_line = line.split("#", 1)[0].strip()
        words = active_line.split()
        if words and words[0] in {"import", "try-import"}:
            imports.append(tuple(words))
            continue
        if len(words) < 2:
            continue
        scope = words[0].split(":", 1)[1] if ":" in words[0] else ""
        if scope not in active_scopes:
            continue
        for option in words[1:]:
            if option.startswith(
                (
                    "--remote_cache=",
                    "--remote_executor=",
                    "--bes_backend=",
                    "--bes_results_url=",
                    "--experimental_remote_downloader=",
                )
            ):
                raise PreparationError("active_remote_bazel_configuration")
            if option == "--config" or option.startswith("--config="):
                raise PreparationError("active_bazel_config_import")
    if any(item != ("try-import", "%workspace%/user.bazelrc") for item in imports):
        raise PreparationError("unexpected_product_bazelrc_import")
    if (product / "user.bazelrc").exists():
        raise PreparationError("unexpected_product_bazelrc_import")
    if any(
        arg == "--config" or arg.startswith("--config=")
        for _, _, argv in PHASES
        for arg in argv
    ):
        raise PreparationError("unexpected_bazel_configuration_selection")


def check_trusted_inputs(product: Path, base: Path) -> None:
    # These files control dependency resolution, formatting, and generator behavior.
    # Compare bytes without exposing their contents in the receipt or logs.
    for relative in (
        ".bazelrc",
        ".bazelversion",
        "justfile",
        "scripts/format.py",
        "codex-rs/.cargo/config.toml",
    ):
        target_path = product / relative
        base_path = base / relative
        if target_path.exists() != base_path.exists():
            raise PreparationError("trusted_execution_input_changed")
        if target_path.exists() and target_path.read_bytes() != base_path.read_bytes():
            raise PreparationError("trusted_execution_input_changed")


def identity(root: Path) -> tuple[str, str]:
    sha = git(root, "rev-parse", "HEAD^{commit}").decode("ascii").strip()
    tree = git(root, "rev-parse", "HEAD^{tree}").decode("ascii").strip()
    return sha, tree


def clean_checkout(root: Path) -> bool:
    return not git(root, "status", "--porcelain=v1", "--untracked-files=all").strip()


def changed_entries(
    product: Path, args: tuple[str, ...], env: dict[str, str] | None = None
) -> list[tuple[str, str, str, str]]:
    raw = git(product, *args, env=env)
    fields = raw.split(b"\0")
    entries = []
    index = 0
    while index < len(fields) - 1:
        header = fields[index].split()
        if len(header) != 5:
            raise PreparationError("diff_inventory_invalid")
        status = header[4].decode("ascii", "strict")
        path = fields[index + 1].decode("utf-8", "strict")
        entries.append(
            (
                status,
                path,
                header[0].decode("ascii", "strict").lstrip(":"),
                header[1].decode("ascii", "strict"),
            )
        )
        index += 2
    return entries


def validate_entries(
    entries: list[tuple[str, str, str, str]], allowlist: frozenset[str]
) -> None:
    for status, path, old_mode, new_mode in entries:
        if status not in {"A", "M"} or path not in allowlist:
            raise PreparationError("candidate_delta_outside_allowlist")
        if new_mode != "100644" or (status == "M" and old_mode != new_mode):
            raise PreparationError("candidate_delta_mode_change")


def check_candidate_delta(
    product: Path, base_sha: str, target_sha: str, base_tree: str
) -> int:
    if (
        git(product, "rev-parse", f"{base_sha}^{{tree}}").decode("ascii").strip()
        != base_tree
    ):
        raise PreparationError("base_tree_mismatch")
    entries = changed_entries(
        product,
        ("diff", "--no-renames", "--raw", "-z", base_sha, target_sha),
    )
    validate_entries(entries, ALLOWED_PATHS)
    return len(entries)


def workspace_packages(metadata: dict) -> frozenset[tuple[str, str]]:
    return frozenset(
        (package["name"], package["version"])
        for package in metadata["packages"]
        if package["source"] is None
    )


def external_metadata_packages(metadata: dict) -> Counter[tuple[str, str, str]]:
    return Counter(
        (
            package["name"],
            package["version"],
            package["source"],
        )
        for package in metadata["packages"]
        if package["source"] is not None
    )


def lock_inventory(path: Path) -> tuple[frozenset[tuple[str, str]], Counter]:
    try:
        lock = tomllib.loads(path.read_text(encoding="utf-8"))
        packages = lock["package"]
        workspace = frozenset(
            (package["name"], package["version"])
            for package in packages
            if "source" not in package
        )
        external = Counter(
            (
                package["name"],
                package["version"],
                package["source"],
                package.get("checksum"),
            )
            for package in packages
            if "source" in package
        )
    except (OSError, KeyError, TypeError, tomllib.TOMLDecodeError) as exc:
        raise PreparationError("cargo_lock_inventory_invalid") from exc
    return workspace, external


def compare_lock_inventories(
    before: tuple[frozenset, Counter], after: tuple[frozenset, Counter], code: str
) -> None:
    if before[0] != after[0]:
        raise PreparationError("workspace_package_set_changed")
    if before[1] != after[1]:
        raise PreparationError(code)


def compare_metadata_to_lock(metadata: dict, lock: tuple[frozenset, Counter]) -> None:
    if workspace_packages(metadata) != lock[0]:
        raise PreparationError("workspace_metadata_lock_mismatch")
    locked_identities = Counter()
    for (name, version, source, _checksum), count in lock[1].items():
        locked_identities[(name, version, source)] += count
    metadata_identities = external_metadata_packages(metadata)
    if metadata_identities - locked_identities:
        raise PreparationError("external_metadata_not_in_lock")


def lock_fingerprint(inventory: tuple[frozenset, Counter]) -> dict:
    workspace, external = inventory
    workspace_records = sorted([name, version] for name, version in workspace)
    external_records = sorted(
        [
            [name, version, source, checksum, count]
            for (name, version, source, checksum), count in external.items()
        ],
        key=lambda row: (row[0], row[1], row[2], row[3] or ""),
    )
    workspace_bytes = json.dumps(workspace_records, separators=(",", ":")).encode()
    external_bytes = json.dumps(external_records, separators=(",", ":")).encode()
    return {
        "workspace_package_count": len(workspace_records),
        "workspace_sha256": hashlib.sha256(workspace_bytes).hexdigest(),
        "external_record_count": sum(external.values()),
        "external_sha256": hashlib.sha256(external_bytes).hexdigest(),
    }


def record_lock_fingerprint(
    receipt: dict, name: str, inventory: tuple[frozenset, Counter]
) -> None:
    receipt["dependency_inventory"][name] = lock_fingerprint(inventory)


def record_metadata_coverage(
    receipt: dict, metadata: dict, lock: tuple[frozenset, Counter]
) -> None:
    identities = external_metadata_packages(metadata)
    selected = sorted(
        [name, version, source, count]
        for (name, version, source), count in identities.items()
    )
    receipt["metadata_coverage"] = {
        "coverage_scope": "selected_resolved_external_packages_only",
        "workspace_package_count": len(workspace_packages(metadata)),
        "external_package_count": sum(identities.values()),
        "locked_external_record_count": sum(lock[1].values()),
        "selected_external_identity_sha256": hashlib.sha256(
            json.dumps(selected, separators=(",", ":")).encode()
        ).hexdigest(),
        "unselected_locked_external_record_count": sum(lock[1].values())
        - sum(identities.values()),
    }


def parse_metadata(raw: bytes) -> dict:
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PreparationError("cargo_metadata_invalid") from exc
    if not isinstance(value, dict) or not isinstance(value.get("packages"), list):
        raise PreparationError("cargo_metadata_invalid")
    return value


def check_toolchain(
    env: dict[str, str], root: Path, receipt: dict, receipt_path: Path
) -> None:
    receipt["toolchain"].setdefault("commands", {})
    checks = (
        ("rust", ("rustc", "--version")),
        ("rustfmt", ("rustfmt", "--version")),
        ("clippy", ("cargo", "clippy", "--version")),
        ("just", ("just", "--version")),
        ("uv", ("uv", "--version")),
        ("bazelisk", ("bazel", "version", "--gnu_format")),
        ("bazel", ("bazel", "--version")),
    )
    for name, argv in checks:
        receipt["toolchain"]["current_phase"] = name
        receipt["toolchain"]["commands"][name] = {
            "status": "in_progress",
            "exit_code": None,
        }
        persist_receipt(receipt_path, receipt)
        try:
            proc = run(argv, root, env)
        except BaseException:
            receipt["toolchain"]["commands"][name] = {
                "status": "unknown",
                "exit_code": None,
            }
            persist_receipt(receipt_path, receipt)
            raise
        receipt["toolchain"]["current_phase"] = None
        match = parse_version_output(
            proc.stdout,
            "bazel_version_pair" if name == "bazelisk" else name,
        )
        if proc.returncode or match is None:
            receipt["toolchain"]["commands"][name] = {
                "status": "failed",
                "exit_code": proc.returncode,
            }
            receipt["toolchain"]["status"] = "failed"
            receipt["toolchain"]["failure_code"] = f"tool_version_mismatch_{name}"
            persist_receipt(receipt_path, receipt)
            raise PreparationError(f"tool_version_mismatch_{name}")
        receipt["toolchain"]["observed"][name] = match.group(1)
        receipt["toolchain"]["commands"][name] = {
            "status": "passed",
            "exit_code": proc.returncode,
        }
        if name == "bazelisk":
            lines = proc.stdout.decode("utf-8", "strict").replace("\r\n", "\n")
            if lines.endswith("\n"):
                lines = lines[:-1]
            receipt["toolchain"]["observed"]["bazel_from_version_pair"] = (
                TOOL_VERSION_PATTERNS["bazel"].fullmatch(lines.split("\n")[1]).group(1)
            )
        persist_receipt(receipt_path, receipt)
    if (
        receipt["toolchain"]["observed"]["bazel"]
        != receipt["toolchain"]["observed"]["bazel_from_version_pair"]
    ):
        receipt["toolchain"]["status"] = "failed"
        receipt["toolchain"]["failure_code"] = "tool_version_mismatch_bazel_crosscheck"
        persist_receipt(receipt_path, receipt)
        raise PreparationError("tool_version_mismatch_bazel_crosscheck")
    name = "dotslash"
    receipt["toolchain"]["current_phase"] = name
    receipt["toolchain"]["commands"][name] = {
        "status": "in_progress",
        "exit_code": None,
    }
    persist_receipt(receipt_path, receipt)
    try:
        dotslash = run(("dotslash", "--version"), root, env)
    except BaseException:
        receipt["toolchain"]["commands"][name] = {
            "status": "unknown",
            "exit_code": None,
        }
        persist_receipt(receipt_path, receipt)
        raise
    receipt["toolchain"]["current_phase"] = None
    dotslash_match = parse_version_output(dotslash.stdout, "dotslash")
    if dotslash.returncode or dotslash_match is None:
        receipt["toolchain"]["commands"][name] = {
            "status": "failed",
            "exit_code": dotslash.returncode,
        }
        receipt["toolchain"]["status"] = "failed"
        receipt["toolchain"]["failure_code"] = "tool_version_mismatch_dotslash"
        persist_receipt(receipt_path, receipt)
        raise PreparationError("tool_version_mismatch_dotslash")
    receipt["toolchain"]["observed"]["dotslash"] = dotslash_match.group(1)
    receipt["toolchain"]["commands"][name] = {
        "status": "passed",
        "exit_code": dotslash.returncode,
    }
    receipt["toolchain"]["status"] = (
        "expected_versions_and_installer_provenance_observed"
    )
    receipt["toolchain"]["current_phase"] = None
    persist_receipt(receipt_path, receipt)


def changed_paths(product: Path, index_path: Path) -> list[str]:
    env = minimal_env()
    env["GIT_INDEX_FILE"] = str(index_path)
    git(product, "read-tree", "HEAD", env=env)
    git(product, "add", "-A", env=env)
    entries = changed_entries(
        product,
        ("diff", "--cached", "--no-renames", "--raw", "-z", "--"),
        env,
    )
    validate_entries(entries, ALLOWED_PATHS)
    return [path for _, path, _, _ in entries]


def patch_bytes(product: Path, index_path: Path) -> bytes:
    env = minimal_env()
    env["GIT_INDEX_FILE"] = str(index_path)
    git(product, "read-tree", "HEAD", env=env)
    git(product, "add", "-A", env=env)
    entries = changed_entries(
        product,
        ("diff", "--cached", "--no-renames", "--raw", "-z", "--"),
        env,
    )
    validate_entries(entries, ALLOWED_PATHS)
    return git(
        product,
        "diff",
        "--cached",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        env=env,
    )


def validate_patch_size(patch: bytes) -> None:
    if len(patch) > MAX_PATCH_BYTES:
        raise PreparationError("patch_overflow")


def safe_receipt(receipt: dict) -> bytes:
    data = (json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if len(data) > MAX_RECEIPT_BYTES:
        raise PreparationError("receipt_overflow")
    return data


def atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_suffix(path.suffix + ".tmp")
    temp.write_bytes(data)
    temp.replace(path)


def persist_receipt(path: Path, receipt: dict) -> None:
    try:
        atomic_write(path, safe_receipt(receipt))
    except PreparationError:
        raise
    except Exception:
        raise PreparationError("receipt_persist_failed") from None


def run_preflight(
    name: str,
    argv: tuple[str, ...],
    cwd: Path,
    env: dict[str, str],
    receipt: dict,
    receipt_path: Path,
) -> subprocess.CompletedProcess[bytes]:
    receipt[name].update(status="in_progress", exit_code=None)
    receipt["preflight_current_phase"] = name
    persist_receipt(receipt_path, receipt)
    try:
        proc = run(argv, cwd, env)
    except BaseException:
        receipt[name].update(status="unknown", exit_code=None)
        persist_receipt(receipt_path, receipt)
        raise
    receipt[name].update(
        status="passed" if proc.returncode == 0 else "failed",
        exit_code=proc.returncode,
    )
    receipt["preflight_current_phase"] = None
    persist_receipt(receipt_path, receipt)
    return proc


def make_receipt(args: argparse.Namespace) -> dict:
    safe_base_sha = args.base_sha if SHA_RE.fullmatch(args.base_sha) else "invalid"
    safe_target_sha = (
        args.target_sha if SHA_RE.fullmatch(args.target_sha) else "invalid"
    )
    safe_helper_sha = (
        args.helper_sha if SHA_RE.fullmatch(args.helper_sha) else "invalid"
    )
    safe_base_ref = args.base_ref if valid_ref(args.base_ref) else "invalid"
    return {
        "schema": "control-plane-preparation-v1",
        "status": "incomplete",
        "failure_code": None,
        "identity": {
            "repository": os.environ.get("GITHUB_REPOSITORY", "unavailable"),
            "workflow_path": ".github/workflows/validation-control-plane-prep.yml",
            "workflow_commit": os.environ.get("GITHUB_SHA", "unavailable"),
            "helper_sha": safe_helper_sha,
            "base_sha": safe_base_sha,
            "base_tree": None,
            "target_sha": safe_target_sha,
            "target_tree": None,
            "workflow_ref": os.environ.get("GITHUB_REF", "unavailable"),
            "comparison_ref": safe_base_ref,
            "helper_tree": None,
            "run_id": os.environ.get("GITHUB_RUN_ID", "unavailable"),
            "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", "unavailable"),
        },
        "request_fingerprint": hashlib.sha256(
            json.dumps(
                [safe_base_sha, safe_target_sha, safe_base_ref],
                separators=(",", ":"),
            ).encode()
        ).hexdigest(),
        "catalog_fingerprint": hashlib.sha256(
            "\n".join(sorted(ALLOWED_PATHS)).encode()
        ).hexdigest(),
        "toolchain": {
            "status": "not_run",
            "failure_code": None,
            "observed": {},
            "commands": {},
            "current_phase": None,
            "rust": "1.95.0",
            "rustfmt": "1.9.0",
            "clippy": "0.1.95",
            "just": "1.51.0",
            "uv": "0.11.3",
            "bazelisk": "1.28.1",
            "bazel": "9.0.0",
            "dotslash_expected": "unknown_dynamic_latest",
            "dotslash_binary_selection": "dynamic_latest",
            "dotslash_installer_action_sha": "1e4e7b3e07eaca387acb98f1d4720e0bee8dbb6a",
            "dotslash_binary_pin_verified": False,
        },
        "fixed_commands": [
            {
                "phase": "source_style",
                "cwd": "workflow",
                "argv": list(SOURCE_STYLE_ARGV),
            },
            {"phase": "self_tests", "cwd": "workflow", "argv": list(SELF_TEST_ARGV)},
            *[
                {"phase": name, "cwd": cwd, "argv": list(argv)}
                for name, cwd, argv in PHASES
            ],
        ],
        "self_tests": {"status": "not_run", "exit_code": None},
        "source_style": {"status": "not_run", "exit_code": None},
        "preflight_current_phase": None,
        "phases": {
            name: {"status": "not_run", "exit_code": None} for name, _, _ in PHASES
        },
        "inventory": {"changed_path_count": 0, "omitted_path_count": 0},
        "workspace_package_count": None,
        "external_package_record_count": None,
        "dependency_inventory": {
            "base": None,
            "target_initial": None,
            "target_after_update": None,
            "target_final": None,
        },
        "metadata_coverage": None,
        "patch": {"status": "not_emitted", "bytes": 0, "sha256": None},
        "omissions": [],
    }


def prepare(args: argparse.Namespace) -> int:
    receipt = None
    receipt_path = None
    patch_path = None
    try:
        receipt = make_receipt(args)
        receipt_path = Path(args.receipt)
        patch_path = Path(args.patch)
        persist_receipt(receipt_path, receipt)
        if (
            not SHA_RE.fullmatch(args.target_sha)
            or not SHA_RE.fullmatch(args.base_sha)
            or not SHA_RE.fullmatch(args.helper_sha)
        ):
            receipt["failure_code"] = "invalid_sha"
            persist_receipt(receipt_path, receipt)
            return 2
        if not valid_ref(args.base_ref):
            receipt["failure_code"] = "invalid_base_ref"
            persist_receipt(receipt_path, receipt)
            return 2
        product = Path(args.product).resolve()
        base = Path(args.base).resolve()
        helper = Path(args.helper).resolve()
        check_user_configuration(product)
        check_trusted_inputs(product, base)
        target_sha, target_tree = identity(product)
        base_sha, base_tree = identity(base)
        receipt["identity"].update(target_tree=target_tree, base_tree=base_tree)
        if target_sha != args.target_sha or base_sha != args.base_sha:
            raise PreparationError("checkout_identity_mismatch")
        if not clean_checkout(product) or not clean_checkout(base):
            raise PreparationError("checkout_not_clean")
        receipt["inventory"]["candidate_path_count"] = check_candidate_delta(
            product, args.base_sha, args.target_sha, base_tree
        )
        persist_receipt(receipt_path, receipt)
        # The workflow helper must be the exact workflow checkout, distinct from T.
        if not helper.is_dir() or not clean_checkout(helper):
            raise PreparationError("helper_checkout_missing")
        helper_sha, helper_tree = identity(helper)
        if helper_sha != args.helper_sha:
            raise PreparationError("helper_checkout_missing")
        receipt["identity"]["helper_tree"] = helper_tree
        persist_receipt(receipt_path, receipt)
        isolated_tool_paths = (
            Path(minimal_env()["RUFF_CACHE_DIR"]),
            Path(minimal_env()["UV_CACHE_DIR"]),
            Path(minimal_env()["UV_PROJECT_ENVIRONMENT"]),
        )
        if any(path.exists() for path in isolated_tool_paths):
            raise PreparationError("tool_cache_not_fresh")
        style = run_preflight(
            "source_style",
            SOURCE_STYLE_ARGV,
            helper,
            minimal_env(),
            receipt,
            receipt_path,
        )
        if style.returncode:
            raise PreparationError("preparation_source_style_failed")
        self_tests = run_preflight(
            "self_tests",
            SELF_TEST_ARGV,
            helper,
            minimal_env(),
            receipt,
            receipt_path,
        )
        if self_tests.returncode:
            raise PreparationError("preparation_self_tests_failed")
        if identity(helper) != (helper_sha, helper_tree) or not clean_checkout(helper):
            raise PreparationError("helper_checkout_changed")
        isolated_home = Path(minimal_env()["HOME"])
        isolated_home.mkdir(parents=True, exist_ok=True)
        cargo_env = minimal_env()
        cargo_env.update(
            {
                "CARGO_INCREMENTAL": "0",
                "RUST_MIN_STACK": "8388608",
                "CARGO_TARGET_DIR": str(
                    Path(os.environ["RUNNER_TEMP"]).resolve()
                    / "control-plane-cargo-target"
                ),
                "CARGO_HOME": str(isolated_home / ".cargo"),
                "CARGO_NET_GIT_FETCH_WITH_CLI": "true",
            }
        )
        target_dir = Path(cargo_env["CARGO_TARGET_DIR"])
        if target_dir.exists():
            raise PreparationError("target_directory_not_fresh")
        lock_path = product / "codex-rs/Cargo.lock"
        base_lock_path = base / "codex-rs/Cargo.lock"
        base_lock = lock_inventory(base_lock_path)
        initial_lock = lock_inventory(lock_path)
        record_lock_fingerprint(receipt, "base", base_lock)
        record_lock_fingerprint(receipt, "target_initial", initial_lock)
        persist_receipt(receipt_path, receipt)
        compare_lock_inventories(
            base_lock, initial_lock, "input_external_package_records_changed"
        )
        check_toolchain(cargo_env, helper, receipt, receipt_path)
        generated_lock = None
        for name, relative_cwd, argv in PHASES:
            receipt["phases"][name] = {"status": "in_progress", "exit_code": None}
            persist_receipt(receipt_path, receipt)
            try:
                proc = run(argv, phase_cwd(product, relative_cwd), cargo_env)
            except BaseException:
                receipt["phases"][name] = {"status": "unknown", "exit_code": None}
                persist_receipt(receipt_path, receipt)
                raise
            receipt["phases"][name] = {
                "status": "passed" if proc.returncode == 0 else "failed",
                "exit_code": proc.returncode,
            }
            persist_receipt(receipt_path, receipt)
            if proc.returncode:
                raise PreparationError(f"command_failed_{name}")
            if name == "workspace_update":
                generated_lock = lock_inventory(lock_path)
                record_lock_fingerprint(receipt, "target_after_update", generated_lock)
                persist_receipt(receipt_path, receipt)
                compare_lock_inventories(
                    base_lock,
                    generated_lock,
                    "generated_external_package_records_changed",
                )
            if name == "metadata":
                metadata_after = parse_metadata(proc.stdout)
                if generated_lock is None:
                    raise PreparationError("generated_lock_inventory_missing")
                compare_metadata_to_lock(metadata_after, generated_lock)
                record_metadata_coverage(receipt, metadata_after, generated_lock)
                receipt["workspace_package_count"] = len(
                    workspace_packages(metadata_after)
                )
                receipt["external_package_record_count"] = sum(
                    external_metadata_packages(metadata_after).values()
                )
                persist_receipt(receipt_path, receipt)
        final_lock = lock_inventory(lock_path)
        record_lock_fingerprint(receipt, "target_final", final_lock)
        persist_receipt(receipt_path, receipt)
        compare_lock_inventories(
            base_lock, final_lock, "final_external_package_records_changed"
        )
        if identity(product) != (target_sha, target_tree) or identity(base) != (
            base_sha,
            base_tree,
        ):
            raise PreparationError("checkout_identity_changed")
        if identity(helper) != (helper_sha, helper_tree) or not clean_checkout(helper):
            raise PreparationError("helper_checkout_changed")
        if not clean_checkout(base):
            raise PreparationError("base_checkout_changed")
        with tempfile.TemporaryDirectory(prefix="control-plane-index-") as tempdir:
            index = Path(tempdir) / "index"
            changed = changed_paths(product, index)
            receipt["inventory"].update(
                changed_path_count=len(changed),
                omitted_path_count=0,
            )
            patch = patch_bytes(product, index)
        validate_patch_size(patch)
        atomic_write(patch_path, patch)
        receipt["status"] = "complete"
        receipt["failure_code"] = None
        receipt["patch"] = {
            "status": "emitted",
            "bytes": len(patch),
            "sha256": hashlib.sha256(patch).hexdigest(),
        }
        persist_receipt(receipt_path, receipt)
        return 0
    except BaseException as exc:
        if receipt is not None:
            receipt["status"] = "incomplete"
            receipt["patch"] = {"status": "not_emitted", "bytes": 0, "sha256": None}
            for phase in receipt["phases"].values():
                if phase["status"] == "in_progress":
                    phase.update(status="unknown", exit_code=None)
            for name in ("source_style", "self_tests"):
                if receipt[name]["status"] == "in_progress":
                    receipt[name].update(status="unknown", exit_code=None)
            for command in receipt["toolchain"].get("commands", {}).values():
                if command["status"] == "in_progress":
                    command.update(status="unknown", exit_code=None)
            if receipt["toolchain"].get("current_phase") is not None:
                receipt["toolchain"]["status"] = "unknown"
            if isinstance(exc, PreparationError):
                receipt["failure_code"] = exc.code
            elif isinstance(exc, KeyboardInterrupt):
                receipt["failure_code"] = "execution_interrupted"
            else:
                receipt["failure_code"] = "unexpected_exception"
        incomplete_receipt_persisted = False
        if receipt is not None and receipt_path is not None:
            try:
                persist_receipt(receipt_path, receipt)
                incomplete_receipt_persisted = True
            except BaseException:
                pass
        if incomplete_receipt_persisted and patch_path is not None:
            try:
                patch_path.unlink(missing_ok=True)
            except BaseException:
                pass
        if receipt is not None and receipt.get("failure_code") in {
            "invalid_sha",
            "invalid_base_ref",
        }:
            return 2
    return 1


def parser() -> argparse.ArgumentParser:
    result = SafeArgumentParser()
    result.add_argument("--product", required=True)
    result.add_argument("--base", required=True)
    result.add_argument("--helper", required=True)
    result.add_argument("--helper-sha", required=True)
    result.add_argument("--target-sha", required=True)
    result.add_argument("--base-sha", required=True)
    result.add_argument("--base-ref", required=True)
    result.add_argument("--receipt", required=True)
    result.add_argument("--patch", required=True)
    return result


def valid_ref(value: str) -> bool:
    if not REF_RE.fullmatch(value) or ".." in value or "//" in value or "@{" in value:
        return False
    return all(
        segment
        and not segment.startswith(".")
        and not segment.endswith(".")
        and not segment.endswith(".lock")
        for segment in value.split("/")
    )


def phase_cwd(product: Path, relative_cwd: str) -> Path:
    return (product / relative_cwd).resolve()


def main() -> int:
    try:
        return prepare(parser().parse_args())
    except BaseException:
        return 2


if __name__ == "__main__":
    sys.exit(main())
