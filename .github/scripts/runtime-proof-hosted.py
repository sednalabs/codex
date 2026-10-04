#!/usr/bin/env python3
"""Fixed two-phase hosted preparation and validation for the runtime-proof source."""

import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
CODEX_RS = ROOT / "codex-rs"
MANIFEST_PATH = ROOT / ".github/scripts/runtime-proof-source-inputs.json"
WORKFLOW_PATH = ROOT / ".github/workflows/runtime-proof-source-preparation.yml"
EXPECTED_REPOSITORY = "sednalabs/codex"
EXPECTED_BRANCH = "refs/heads/feature/runtime-proof-hosted-20261005"
CARGO_PACKAGES = [
    "codex-runtime-proof",
    "codex-mcp",
    "codex-rmcp-client",
    "codex-core",
    "codex-exec",
]
WINDOWS_PACKAGES = [*CARGO_PACKAGES, "codex-cli"]
EXPECTED_PACKAGE_MANIFESTS = {
    "codex-cli": "cli/Cargo.toml",
    "codex-mcp": "codex-mcp/Cargo.toml",
    "codex-runtime-proof": "runtime-proof/Cargo.toml",
    "codex-rmcp-client": "rmcp-client/Cargo.toml",
    "codex-core": "core/Cargo.toml",
    "codex-exec": "exec/Cargo.toml",
}
EXPECTED_ARTIFACT_TARGETS = {
    ("codex-cli", "codex", "bin"): "cli/src/main.rs",
    ("codex-cli", "runtime_execution_proof", "test"): "cli/tests/runtime_execution_proof.rs",
    ("codex-mcp", "codex_mcp", "lib"): "codex-mcp/src/lib.rs",
    ("codex-runtime-proof", "codex_runtime_proof", "lib"): "runtime-proof/src/lib.rs",
    ("codex-rmcp-client", "codex_rmcp_client", "lib"): "rmcp-client/src/lib.rs",
    ("codex-core", "codex_core", "lib"): "core/src/lib.rs",
    ("codex-exec", "codex_exec", "lib"): "exec/src/lib.rs",
}
LIBRARY_TEST_TARGETS = {
    "codex-runtime-proof": ("codex_runtime_proof", "runtime-proof/src/lib.rs"),
    "codex-mcp": ("codex_mcp", "codex-mcp/src/lib.rs"),
    "codex-rmcp-client": ("codex_rmcp_client", "rmcp-client/src/lib.rs"),
    "codex-core": ("codex_core", "core/src/lib.rs"),
    "codex-exec": ("codex_exec", "exec/src/lib.rs"),
}
LIBRARY_RUNTIME_ENV_KEYS = (
    "CARGO",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_PKG_VERSION",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_MINOR",
    "CARGO_PKG_VERSION_PATCH",
    "CARGO_PKG_VERSION_PRE",
    "CARGO_PKG_NAME",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PKG_HOMEPAGE",
    "CARGO_PKG_REPOSITORY",
    "CARGO_PKG_LICENSE",
    "CARGO_PKG_LICENSE_FILE",
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_RUST_VERSION",
    "CARGO_PKG_README",
)
LINUX_HOST_TARGET = "x86_64-unknown-linux-gnu"
TARGET_IDENTITY_FIELDS = ("name", "kind", "crate_types", "src_path", "edition", "test", "doctest")
EXPECTED_FORMAT_PACKAGES = [
    "codex-runtime-proof",
    "codex-core",
    "codex-cli",
    "codex-mcp",
    "codex-exec",
    "codex-rmcp-client",
]
EXPECTED_CLI_TESTS = [
    "protected_cli_root_and_delegate_calls_are_signed_and_redacted",
    "protected_trace_logging_does_not_echo_proof_or_bearer",
    "protected_mcp_error_does_not_echo_proof_or_bearer",
    "invalid_auth_bootstraps_are_rejected_before_credential_egress",
    "delegate_provider_override_is_rejected_before_credential_egress",
    "protected_mcp_redirect_is_stopped_before_recipient_change",
    "delayed_mcp_response_is_redacted_after_protected_auth_expiry",
]
EXPECTED_MCP_TESTS = [
    "protected_http_client_tests::retained_protected_http_client_rejects_a_send_after_imported_auth_expiry"
]
EXPECTED_HARNESS_PATHS = [
    ".github/workflows/runtime-proof-source-preparation.yml",
    ".github/scripts/runtime-proof-hosted.py",
    ".github/scripts/runtime-proof-source-inputs.json",
]
EXPECTED_ACTIONS = {
    "actions/checkout": "3d3c42e5aac5ba805825da76410c181273ba90b1",
    "dtolnay/rust-toolchain": "ebb3d1676050bfd0971c36c1e215b5751473994d",
    "actions/upload-artifact": "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
    "bazel-contrib/setup-bazel": "c5acdfb288317d0b5c0bbd7a396a3dc868bb0f86",
    "facebook/install-dotslash": "1e4e7b3e07eaca387acb98f1d4720e0bee8dbb6a",
    "taiki-e/install-action": "44c6d64aa62cd779e873306675c7a58e86d6d532",
}
EXPECTED_LOCAL_HELPERS = {
    ".github/actions/setup-bazel-ci/action.yml": "c8a61a82ac65d3a06e43acb35c9ac2ba97f6f74ab486d0fae1a4729ccf63b209",
    ".github/actions/setup-ci/action.yml": "cef6b87a463c78813f9ba0c0dac8cc451a8be9b9ca218f5130f057af3801904a",
    ".github/actions/setup-rusty-v8/action.yml": "e05628c6361bf0f5ad02cbce304c5894f1ef3079f8b7630c1351cb4c48a118e1",
    "codex-rs/rust-toolchain.toml": "e8b379eaa492bf59d35187de269afd1c4f55d85e0322f6fea32803a3fec1a380",
    "codex-rs/rustfmt.toml": "384d5d8a1366a86cd6fc01aecde2b0a5806a9e77073ff53abd97b4f6cad94d1c",
    "codex-rs/.cargo/config.toml": "3a39947e4aebcb7f4a45108a94e0db9b1cc7b2b84f8f48a4814996e10bfb88e9",
    ".bazelversion": "cdecb300baad839a6f62791229f551a4fa33f3cbdca08e378dc976466354e778",
    ".bazelrc": "b0a49fa058076ba277fd6c923e1c06068fe125a3fadc886801f90a8a6b23c168",
    "MODULE.bazel": "cd20e5e5b65b6532482da40601d54c6593289a32bc4e30c44e9d087bfe82aefa",
    ".github/scripts/rusty_v8_bazel.py": "b177c7d19bf3a68b83483cb001503fe282baefd748a801a160ecb3b4b4ab7c95",
    ".github/scripts/setup-dev-drive.ps1": "e11b6a0bc5f4f51d6a4159e145b75fdee7ed4b5c7a2dfb683b9156e47746c794",
}
TEST_SUMMARY = re.compile(
    r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;"
)
PACKAGE_SOURCE_DIRS = {
    "codex-runtime-proof": "runtime-proof",
    "codex-mcp": "codex-mcp",
    "codex-rmcp-client": "rmcp-client",
    "codex-core": "core",
    "codex-exec": "exec",
}


def refuse(message: str) -> None:
    raise SystemExit(message)


def run_git(*args: str, capture: bool = True) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=ROOT,
        check=False,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
    )
    if result.returncode != 0:
        refuse(f"git binding check failed: {args[0]}")
    return result.stdout or ""


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def load_manifest() -> dict[str, Any]:
    try:
        value = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        refuse(f"source-input manifest is invalid: {type(error).__name__}")
    required = {
        "schema_version",
        "phase",
        "repository",
        "base_commit",
        "product_source_commit",
        "product_inputs",
        "harness_paths",
        "format_packages",
        "linux_library_test_packages",
        "cli_ignored_tests",
        "mcp_ignored_tests",
        "generated_allowed_paths",
        "pinned_actions",
        "toolchains",
        "local_helpers",
    }
    if not isinstance(value, dict) or set(value) != required:
        refuse("source-input manifest keys do not match the fixed schema")
    if value["schema_version"] != 1 or value["repository"] != EXPECTED_REPOSITORY:
        refuse("source-input manifest identity is unsupported")
    if value["phase"] not in ("prepare", "validate"):
        refuse("source-input manifest phase is unsupported")
    if value["harness_paths"] != EXPECTED_HARNESS_PATHS:
        refuse("manifest harness path scope changed")
    if value["format_packages"] != EXPECTED_FORMAT_PACKAGES:
        refuse("manifest formatter package set changed")
    if value["linux_library_test_packages"] != CARGO_PACKAGES:
        refuse("manifest Linux library test package set changed")
    if value["cli_ignored_tests"] != EXPECTED_CLI_TESTS or value["mcp_ignored_tests"] != EXPECTED_MCP_TESTS:
        refuse("manifest ignored-test inventory changed")
    expected_generated = ["codex-rs/Cargo.lock", "MODULE.bazel.lock"] + sorted(
        relative for relative in value["product_inputs"] if relative.endswith(".rs")
    )
    if value["generated_allowed_paths"] != expected_generated:
        refuse("manifest generated-file allowlist changed")
    if value["pinned_actions"] != EXPECTED_ACTIONS or value["local_helpers"] != EXPECTED_LOCAL_HELPERS:
        refuse("manifest action or local-helper pins changed")
    if value["toolchains"] != {
        "cargo_rust": "1.96.0",
        "format_rust": "nightly-2025-09-18",
        "bazel": "9.0.0",
        "bazelisk": "1.28.1",
    }:
        refuse("manifest toolchain pins changed")
    if len(value["product_inputs"]) != 40:
        refuse("manifest must bind the exact forty frozen product inputs")
    return value


def verify_clean_checkout() -> None:
    if run_git("status", "--porcelain=v1", "--untracked-files=all").strip():
        refuse("hosted checkout is not clean")


def verify_hash_map(paths: dict[str, str], description: str) -> None:
    for relative, expected in paths.items():
        relative_path = Path(relative)
        if relative_path.is_absolute() or ".." in relative_path.parts:
            refuse(f"{description} path escapes the repository: {relative}")
        path = ROOT / relative_path
        if not path.resolve(strict=True).is_relative_to(ROOT):
            refuse(f"{description} path resolves outside the repository: {relative}")
        if not path.is_file() or path.is_symlink():
            refuse(f"{description} input is missing or not a regular file: {relative}")
        actual = sha256_file(path)
        if actual != expected:
            refuse(f"{description} input digest mismatch: {relative}")


def verify_commit_binding(manifest: dict[str, Any]) -> tuple[str, str]:
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    event = os.environ.get("GITHUB_EVENT_NAME", "")
    ref = os.environ.get("GITHUB_REF", "")
    event_sha = os.environ.get("GITHUB_SHA", "")
    workflow_sha = os.environ.get("EXPECTED_WORKFLOW_SHA", "")
    if repository != EXPECTED_REPOSITORY or event != "push" or ref != EXPECTED_BRANCH:
        refuse("hosted source-preparation trigger is outside the fixed repository branch")
    head = run_git("rev-parse", "HEAD").strip()
    if not re.fullmatch(r"[0-9a-f]{40}", event_sha) or head != event_sha:
        refuse("checked-out commit does not match the push event SHA")
    source_commit = manifest["product_source_commit"]
    if not re.fullmatch(r"[0-9a-f]{40}", source_commit):
        refuse("manifest product source commit is invalid")
    run_git("merge-base", "--is-ancestor", source_commit, head)
    changed = set(run_git("diff", "--name-only", f"{source_commit}..{head}").splitlines())
    harness_paths = set(manifest["harness_paths"])
    if not changed or not changed <= harness_paths:
        refuse("candidate differs from its product source outside the three harness paths")
    if manifest["base_commit"] != "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7":
        refuse("manifest base commit differs from the admitted source base")
    run_git("merge-base", "--is-ancestor", manifest["base_commit"], source_commit)
    workflow_source = WORKFLOW_PATH.read_text(encoding="utf-8")
    action_sources = "\n".join(
        (ROOT / relative).read_text(encoding="utf-8")
        for relative in (
            ".github/actions/setup-bazel-ci/action.yml",
            ".github/actions/setup-ci/action.yml",
        )
    )
    pinned_source = workflow_source + "\n" + action_sources
    required_refs = [
        f"actions/checkout@{manifest['pinned_actions']['actions/checkout']}",
        f"dtolnay/rust-toolchain@{manifest['pinned_actions']['dtolnay/rust-toolchain']}",
        f"actions/upload-artifact@{manifest['pinned_actions']['actions/upload-artifact']}",
        f"bazel-contrib/setup-bazel@{manifest['pinned_actions']['bazel-contrib/setup-bazel']}",
        f"facebook/install-dotslash@{manifest['pinned_actions']['facebook/install-dotslash']}",
        f"taiki-e/install-action@{manifest['pinned_actions']['taiki-e/install-action']}",
    ]
    if any(reference not in pinned_source for reference in required_refs):
        refuse("workflow does not use every admitted immutable action reference")
    if (
        "branches: [feature/runtime-proof-hosted-20261005]" not in workflow_source
        or "workflow_dispatch:" in workflow_source
        or "pull_request:" in workflow_source
        or "permissions:\n  contents: read" not in workflow_source
    ):
        refuse("workflow trigger or permissions differ from the admitted fixed contract")
    autocrlf_config = "git config --global core.autocrlf false"
    eol_config = "git config --global core.eol lf"
    windows_job = workflow_source.split("  validate-windows:", 1)[-1]
    if (
        "  validate-windows:" not in workflow_source
        or autocrlf_config not in windows_job
        or eol_config not in windows_job
        or max(windows_job.index(autocrlf_config), windows_job.index(eol_config))
        > windows_job.index("uses: actions/checkout@")
    ):
        refuse("Windows checkout must be preceded by fixed LF Git configuration")
    if not re.fullmatch(r"[0-9a-f]{40}", workflow_sha):
        refuse("workflow source SHA is unavailable or invalid")
    return head, workflow_sha


def verify_inputs(manifest: dict[str, Any]) -> tuple[str, str]:
    verify_clean_checkout()
    head, workflow_sha = verify_commit_binding(manifest)
    verify_hash_map(manifest["product_inputs"], "product source")
    verify_hash_map(manifest["local_helpers"], "host helper/toolchain")
    verify_test_summary_parser_contract()
    return head, workflow_sha


def emit_phase() -> None:
    manifest = load_manifest()
    verify_inputs(manifest)
    output = os.environ.get("GITHUB_OUTPUT")
    if not output:
        refuse("workflow output channel is missing")
    with Path(output).open("a", encoding="utf-8") as stream:
        stream.write(f"phase={manifest['phase']}\n")


def current_diff_paths() -> list[str]:
    return sorted(set(run_git("diff", "--name-only", "HEAD").splitlines()))


def verify_no_untracked() -> None:
    if run_git("ls-files", "--others", "--exclude-standard").strip():
        refuse("hosted command created an untracked source artifact")


def run_fixed(command: list[str], cwd: Path = CODEX_RS, quiet: bool = False) -> None:
    result = subprocess.run(
        command,
        cwd=cwd,
        check=False,
        stdout=subprocess.DEVNULL if quiet else None,
    )
    if result.returncode != 0:
        refuse(f"fixed hosted command failed with exit {result.returncode}: {Path(command[0]).name}")


def prepare(manifest: dict[str, Any]) -> None:
    head, workflow_sha = verify_inputs(manifest)
    if manifest["phase"] != "prepare":
        refuse("generation is permitted only in the prepare phase")
    if current_diff_paths():
        refuse("preparation checkout unexpectedly contains a working diff")
    run_fixed(
        ["cargo", "metadata", "--manifest-path", "codex-rs/Cargo.toml", "--format-version", "1"],
        ROOT,
        quiet=True,
    )
    run_fixed(["bazel", "mod", "deps", "--lockfile_mode=update"], ROOT)
    run_fixed(
        [
            "rustup",
            "toolchain",
            "install",
            "nightly-2025-09-18",
            "--profile",
            "minimal",
            "--component",
            "rustfmt",
        ],
        ROOT,
    )
    package_args = [
        argument
        for package in manifest["format_packages"]
        for argument in ("-p", package)
    ]
    run_fixed(
        [
            "cargo",
            "+nightly-2025-09-18",
            "fmt",
            "--manifest-path",
            "codex-rs/Cargo.toml",
            *package_args,
            "--",
            "--config",
            "imports_granularity=Item",
        ],
        ROOT,
    )
    changed = current_diff_paths()
    allowed = set(manifest["generated_allowed_paths"])
    if not changed or not set(changed) <= allowed:
        refuse("generated source delta is empty or outside the exact formatter and lock allowlist")
    verify_no_untracked()
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    output = runner_temp / "runtime-proof-preparation"
    output.mkdir(mode=0o700)
    patch = subprocess.check_output(["git", "diff", "--binary", "HEAD", "--", *changed], cwd=ROOT)
    (output / "preparation.patch").write_bytes(patch)
    provenance = {
        "repository": EXPECTED_REPOSITORY,
        "input_commit": head,
        "workflow_commit": workflow_sha,
        "run_id": os.environ.get("GITHUB_RUN_ID", ""),
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", ""),
        "phase": "prepare",
        "toolchains": manifest["toolchains"],
        "pinned_actions": manifest["pinned_actions"],
        "local_helper_sha256": manifest["local_helpers"],
        "changed_paths": changed,
        "patch_sha256": sha256_bytes(patch),
        "output_sha256": {relative: sha256_file(ROOT / relative) for relative in changed},
        "acceptance": "bounded source preparation only; no compile, fixture, platform, or delivery acceptance",
    }
    (output / "provenance.json").write_text(
        json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def parse_test_summaries(output: str) -> list[dict[str, int | str]]:
    summaries = []
    for match in TEST_SUMMARY.finditer(output):
        decimal_values = match.groups()[1:]
        if any(
            len(value) > 10 or not value.isascii() or not value.isdecimal()
            for value in decimal_values
        ):
            refuse("test summary count is unsupported")
        try:
            passed, failed, ignored = (int(value) for value in decimal_values)
        except ValueError:
            refuse("test summary count is unsupported")
        summaries.append(
            {
                "status": match.group(1),
                "passed": passed,
                "failed": failed,
                "ignored": ignored,
            }
        )
    return summaries


def cargo_package_runtime_variables(
    package: dict[str, Any], cargo_executable: str
) -> dict[str, str]:
    version = package.get("version")
    version_match = (
        re.fullmatch(
            r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
            r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
            r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?",
            version,
        )
        if isinstance(version, str)
        else None
    )
    if not version_match or any(
        len(part) > 20 or int(part) > 18446744073709551615
        for part in version_match.groups()[:3]
    ):
        refuse("selected Cargo package version metadata is invalid")
    prerelease = version_match.group(4)
    if prerelease and any(
        identifier.isdecimal() and len(identifier) > 1 and identifier.startswith("0")
        for identifier in prerelease.split(".")
    ):
        refuse("selected Cargo package version metadata is invalid")

    manifest_value = package.get("manifest_path")
    if not isinstance(manifest_value, str):
        refuse("selected Cargo package manifest metadata is invalid")
    manifest_path = Path(manifest_value)
    if not manifest_path.is_absolute():
        refuse("selected Cargo package manifest metadata is invalid")
    manifest_path = manifest_path.resolve(strict=True)
    package_root = manifest_path.parent

    authors = package.get("authors")
    if not isinstance(authors, list) or not all(isinstance(item, str) for item in authors):
        refuse("selected Cargo package author metadata is invalid")

    def optional_metadata_string(key: str) -> str:
        value = package.get(key)
        if value is None:
            return ""
        if not isinstance(value, str):
            refuse("selected Cargo package metadata is invalid")
        return value

    name = package.get("name")
    if not isinstance(name, str) or not name:
        refuse("selected Cargo package name metadata is invalid")
    variables = {
        "CARGO": cargo_executable,
        "CARGO_MANIFEST_DIR": str(package_root),
        "CARGO_MANIFEST_PATH": str(manifest_path),
        "CARGO_PKG_VERSION": version,
        "CARGO_PKG_VERSION_MAJOR": version_match.group(1),
        "CARGO_PKG_VERSION_MINOR": version_match.group(2),
        "CARGO_PKG_VERSION_PATCH": version_match.group(3),
        "CARGO_PKG_VERSION_PRE": version_match.group(4) or "",
        "CARGO_PKG_NAME": name,
        "CARGO_PKG_DESCRIPTION": optional_metadata_string("description"),
        "CARGO_PKG_HOMEPAGE": optional_metadata_string("homepage"),
        "CARGO_PKG_REPOSITORY": optional_metadata_string("repository"),
        "CARGO_PKG_LICENSE": optional_metadata_string("license"),
        "CARGO_PKG_LICENSE_FILE": optional_metadata_string("license_file"),
        "CARGO_PKG_AUTHORS": ":".join(authors),
        "CARGO_PKG_RUST_VERSION": optional_metadata_string("rust_version"),
        "CARGO_PKG_README": optional_metadata_string("readme"),
    }
    if tuple(variables) != LIBRARY_RUNTIME_ENV_KEYS:
        refuse("Cargo package runtime environment inventory changed")
    return variables


def cargo_native_directories(linked_paths: list[str], root_output: str) -> list[str]:
    cargo_kinds = {"native", "crate", "dependency", "framework", "all"}
    root = Path(root_output)
    native_dirs = set()
    for raw_path in linked_paths:
        kind, separator, path_value = raw_path.partition("=")
        if separator and kind in cargo_kinds:
            raw_path = path_value
        candidate = Path(raw_path)
        if candidate.is_absolute() and candidate.is_relative_to(root):
            native_dirs.add(str(candidate))
    return sorted(native_dirs)


def cargo_runtime_search_path_value(
    native_dirs: list[str],
    root_output: str,
    deps_output: str,
    sysroot_libdir: str,
    inherited_path: str | None,
) -> str:
    search_path = [*native_dirs, root_output, deps_output, sysroot_libdir]
    inherited = inherited_path.split(os.pathsep) if inherited_path is not None else []
    if inherited[: len(search_path)] == search_path:
        search_path = inherited
    else:
        search_path.extend(inherited)
    return os.pathsep.join(search_path)


def cargo_runtime_search_path(
    linked_paths: list[str],
    root_output: Path,
    deps_output: Path,
    sysroot_libdir: Path,
    inherited_path: str | None,
) -> str:
    root_output = root_output.resolve(strict=True)
    deps_output = deps_output.resolve(strict=True)
    sysroot_libdir = sysroot_libdir.resolve(strict=True)
    if not all(path.is_dir() for path in (root_output, deps_output, sysroot_libdir)):
        refuse("Cargo runtime library search directory is unavailable")

    if not all(isinstance(raw_path, str) for raw_path in linked_paths):
        refuse("Cargo build-script library path metadata is invalid")
    native_dirs = []
    for raw_path in cargo_native_directories(linked_paths, str(root_output)):
        candidate = Path(raw_path)
        try:
            canonical_candidate = candidate.resolve(strict=True)
        except OSError:
            refuse("Cargo build-script library path is unavailable")
        if (
            canonical_candidate != candidate
            or not canonical_candidate.is_dir()
            or not canonical_candidate.is_relative_to(root_output)
        ):
            refuse("Cargo build-script library path is invalid")
        native_dirs.append(canonical_candidate)

    return cargo_runtime_search_path_value(
        [str(path) for path in native_dirs],
        str(root_output),
        str(deps_output),
        str(sysroot_libdir),
        inherited_path,
    )


def compose_cargo_test_environment(
    inherited_environment: dict[str, str],
    package_variables: dict[str, str],
    dylib_path: str,
) -> dict[str, str]:
    environment = dict(inherited_environment)
    environment.update(package_variables)
    environment["LD_LIBRARY_PATH"] = dylib_path
    return environment


def cargo_library_process_result(
    returncode: int | None,
    summaries: list[dict[str, int | str]],
    output_sha256: str,
    spawn_failed: bool = False,
) -> dict[str, Any]:
    passed = sum(int(item["passed"]) for item in summaries)
    failed = sum(int(item["failed"]) for item in summaries)
    ignored = sum(int(item["ignored"]) for item in summaries)
    if spawn_failed:
        status: dict[str, int | str | None] = {
            "classification": "test_process_spawn_failure",
            "exit_code": None,
            "signal": None,
        }
    elif returncode is None:
        status = {
            "classification": "test_process_status_unknown",
            "exit_code": None,
            "signal": None,
        }
    elif returncode < 0:
        status = {
            "classification": "test_process_signal",
            "exit_code": None,
            "signal": -returncode,
        }
    elif returncode != 0:
        status = {
            "classification": "test_process_exit_failure",
            "exit_code": returncode,
            "signal": None,
        }
    elif not summaries:
        status = {
            "classification": "missing_test_summary",
            "exit_code": 0,
            "signal": None,
        }
    elif any(
        item["status"] != "ok" or item["passed"] == 0 or item["failed"] != 0
        for item in summaries
    ):
        status = {
            "classification": "test_summary_failure",
            "exit_code": 0,
            "signal": None,
        }
    else:
        status = {
            "classification": "passed",
            "exit_code": 0,
            "signal": None,
        }
    return {
        **status,
        "test_summary_count": len(summaries),
        "passed": passed,
        "failed": failed,
        "ignored": ignored,
        "output_sha256": output_sha256,
    }


def verify_test_summary_parser_contract() -> None:
    single_summary = "test result: ok. 1 passed; 0 failed; 0 ignored;"
    expected_single = [
        {"status": "ok", "passed": 1, "failed": 0, "ignored": 0}
    ]
    if parse_test_summaries(single_summary) != expected_single:
        refuse("test summary parser contract failed")

    child_and_outer = f"{single_summary}\n{single_summary}"
    if parse_test_summaries(child_and_outer) != expected_single * 2:
        refuse("test summary parser contract failed")

    oversized_failed_then_success = (
        "test result: FAILED. 10000000000 passed; 1 failed; 0 ignored;\n"
        "test result: ok. 1 passed; 0 failed; 0 ignored;"
    )
    try:
        parse_test_summaries(oversized_failed_then_success)
    except SystemExit as error:
        if error.code != "test summary count is unsupported":
            refuse("test summary parser contract failed")
    else:
        refuse("test summary parser contract failed")

    package = {
        "name": "sample",
        "version": "1.2.3-rc.4+build.7",
        "manifest_path": str(CODEX_RS / "runtime-proof/Cargo.toml"),
        "authors": ["A", "B"],
        "description": None,
        "homepage": "https://example.invalid",
        "repository": None,
        "license": "MIT",
        "license_file": None,
        "rust_version": "1.70.0",
        "readme": "README.md",
    }
    expected_variables = {
        "CARGO": "/toolchain/bin/cargo",
        "CARGO_MANIFEST_DIR": str((CODEX_RS / "runtime-proof").resolve()),
        "CARGO_MANIFEST_PATH": str((CODEX_RS / "runtime-proof/Cargo.toml").resolve()),
        "CARGO_PKG_VERSION": "1.2.3-rc.4+build.7",
        "CARGO_PKG_VERSION_MAJOR": "1",
        "CARGO_PKG_VERSION_MINOR": "2",
        "CARGO_PKG_VERSION_PATCH": "3",
        "CARGO_PKG_VERSION_PRE": "rc.4",
        "CARGO_PKG_NAME": "sample",
        "CARGO_PKG_DESCRIPTION": "",
        "CARGO_PKG_HOMEPAGE": "https://example.invalid",
        "CARGO_PKG_REPOSITORY": "",
        "CARGO_PKG_LICENSE": "MIT",
        "CARGO_PKG_LICENSE_FILE": "",
        "CARGO_PKG_AUTHORS": "A:B",
        "CARGO_PKG_RUST_VERSION": "1.70.0",
        "CARGO_PKG_README": "README.md",
    }
    if cargo_package_runtime_variables(package, "/toolchain/bin/cargo") != expected_variables:
        refuse("Cargo package runtime environment contract failed")
    artifact_target = {
        "name": "codex_runtime_proof",
        "kind": ["lib"],
        "crate_types": ["lib"],
        "src_path": "/source/runtime-proof/src/lib.rs",
        "edition": "2024",
        "test": True,
        "doctest": True,
    }
    artifact = {
        "package_id": "path+file:///source/runtime-proof#0.0.0",
        "target": artifact_target,
        "profile": {"test": True},
        "executable": "/target/debug/deps/codex_runtime_proof-test",
    }
    if not compiler_artifact_matches(
        artifact, artifact["package_id"], artifact_target, True
    ):
        refuse("Cargo compiler artifact identity contract failed")
    unrelated_artifact = {**artifact, "package_id": "path+file:///other#0.0.0"}
    if compiler_artifact_matches(
        unrelated_artifact,
        "path+file:///source/runtime-proof#0.0.0",
        unrelated_artifact["target"],
        True,
    ):
        refuse("Cargo compiler artifact identity contract failed")
    dylib_prefix = [
        "/target/debug/build/a/out",
        "/target/debug/build/z/out",
        "/target/debug",
        "/target/debug/deps",
        "/toolchain/lib",
    ]
    if cargo_native_directories(
        [
            "framework=/outside/lib",
            "native=/target/debug/build/z/out",
            "native=/target/debug/build/a/out",
            "native=/target/debug/build/z/out",
        ],
        "/target/debug",
    ) != dylib_prefix[:2]:
        refuse("Cargo native library search ordering contract failed")
    joined_prefix = os.pathsep.join(dylib_prefix)
    if cargo_runtime_search_path_value(
        dylib_prefix[:2], "/target/debug", "/target/debug/deps", "/toolchain/lib",
        joined_prefix + os.pathsep + "/inherited/lib",
    ) != joined_prefix + os.pathsep + "/inherited/lib":
        refuse("Cargo inherited library search prefix contract failed")
    if cargo_runtime_search_path_value(
        dylib_prefix[:2], "/target/debug", "/target/debug/deps", "/toolchain/lib",
        "/inherited/lib",
    ) != joined_prefix + os.pathsep + "/inherited/lib":
        refuse("Cargo library search append contract failed")
    inherited_environment = {
        "PATH": "/usr/bin",
        "KEEP": "unchanged",
        "CARGO": "stale-cargo",
        "CARGO_PKG_NAME": "stale-package",
        "CARGO_TARGET_TMPDIR": "/stale/tmp",
        "CARGO_CRATE_NAME": "stale-crate",
        "CARGO_BIN_EXE_stale": "/stale/bin",
        "LD_LIBRARY_PATH": "/old/lib",
    }
    expected_environment = {
        "PATH": "/usr/bin",
        "KEEP": "unchanged",
        "CARGO_TARGET_TMPDIR": "/stale/tmp",
        "CARGO_CRATE_NAME": "stale-crate",
        "CARGO_BIN_EXE_stale": "/stale/bin",
        **expected_variables,
        "LD_LIBRARY_PATH": joined_prefix,
    }
    if compose_cargo_test_environment(
        inherited_environment, expected_variables, joined_prefix
    ) != expected_environment:
        refuse("Cargo test child environment contract failed")

    success = [{"status": "ok", "passed": 2, "failed": 0, "ignored": 1}]
    success_counts = {"test_summary_count": 1, "passed": 2, "failed": 0, "ignored": 1}
    if cargo_library_process_result(0, success, "a" * 64) != {
        "classification": "passed",
        "exit_code": 0,
        "signal": None,
        **success_counts,
        "output_sha256": "a" * 64,
    }:
        refuse("library process result contract failed")
    if cargo_library_process_result(7, success, "b" * 64) != {
        "classification": "test_process_exit_failure",
        "exit_code": 7,
        "signal": None,
        **success_counts,
        "output_sha256": "b" * 64,
    }:
        refuse("library process result contract failed")
    empty_counts = {"test_summary_count": 0, "passed": 0, "failed": 0, "ignored": 0}
    empty_cases = [
        (
            -9,
            "c" * 64,
            False,
            {"classification": "test_process_signal", "exit_code": None, "signal": 9},
        ),
        (
            0,
            "d" * 64,
            False,
            {"classification": "missing_test_summary", "exit_code": 0, "signal": None},
        ),
        (
            None,
            "e" * 64,
            True,
            {"classification": "test_process_spawn_failure", "exit_code": None, "signal": None},
        ),
    ]
    for returncode, digest, spawn_failed, status in empty_cases:
        if cargo_library_process_result(
            returncode, [], digest, spawn_failed=spawn_failed
        ) != {**status, **empty_counts, "output_sha256": digest}:
            refuse("library process result contract failed")
    prebuild_error = json.dumps(
        {
            "reason": "compiler-message",
            "message": {"level": "error", "code": {"code": "E0001"}},
        }
    )
    build_failed = json.dumps({"reason": "build-finished", "success": False})
    no_run_tests = {"test_summary_count": 0, "passed": 0, "failed": 0, "ignored": 0}
    if bounded_library_failure_summary(
        "codex-core", f"{prebuild_error}\n{build_failed}", 101, set()
    ) != {
        "classification": "compiler_error",
        "cargo_exit_code": 101,
        "build_finished_count": 1,
        "build_succeeded": False,
        "compiler_error_count": 1,
        "compiler_error_codes": ["E0001"],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo no-run compiler failure contract failed")
    postbuild_error = json.dumps(
        {
            "reason": "compiler-message",
            "message": {"level": "error", "code": {"code": "E0002"}},
        }
    )
    build_succeeded = json.dumps({"reason": "build-finished", "success": True})
    if bounded_library_failure_summary(
        "codex-core",
        f'{build_succeeded}\n{postbuild_error}\n'
        "test result: FAILED. 0 passed; 1 failed; 0 ignored;",
        101,
        set(),
    ) != {
        "classification": "post_build_failure_unclassified",
        "cargo_exit_code": 101,
        "build_finished_count": 1,
        "build_succeeded": True,
        "compiler_error_count": 0,
        "compiler_error_codes": [],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo no-run post-build failure contract failed")


def package_source_location(
    package: str, file_name: str, line: int, admitted_paths: set[str]
) -> str | None:
    source_dir = PACKAGE_SOURCE_DIRS[package]
    if not file_name or len(file_name) > 512 or line < 1:
        return None
    path = Path(file_name)
    if path.is_absolute():
        if ".." in path.parts:
            return None
        try:
            path = path.relative_to(CODEX_RS.resolve())
        except ValueError:
            return None
    elif ".." in path.parts:
        return None
    parts = path.parts
    if parts[:1] == ("src",):
        parts = (source_dir, *parts)
    if (
        not parts
        or parts[0] != source_dir
        or ".." in parts
        or any(not re.fullmatch(r"[A-Za-z0-9_.-]+", part) for part in parts)
        or path.suffix != ".rs"
    ):
        return None
    relative_path = f"codex-rs/{Path(*parts).as_posix()}"
    if relative_path not in admitted_paths:
        return None

    entry = run_git("ls-tree", "-z", "HEAD", "--", relative_path)
    entry = entry[:-1] if entry.endswith("\0") else entry
    metadata, separator, tracked_path = entry.partition("\t")
    fields = metadata.split()
    if (
        not separator
        or tracked_path != relative_path
        or len(fields) != 3
        or fields[0] not in {"100644", "100755"}
        or fields[1] != "blob"
    ):
        return None
    source = run_git("show", f"HEAD:{relative_path}")
    if line > len(source.splitlines()):
        return None
    location = f"{Path(*parts).as_posix()}:{line}"
    return location if len(location) <= 240 else None


def bounded_library_failure_summary(
    package: str, output: str, exit_code: int, admitted_paths: set[str]
) -> dict[str, Any]:
    build_events: list[bool | None] = []
    prebuild_compiler_errors = 0
    compiler_codes: list[str] = []
    source_locations: list[str] = []

    for raw_line in output.splitlines():
        try:
            event = json.loads(raw_line)
        except (ValueError, RecursionError):
            event = None
        if isinstance(event, dict) and event.get("reason") == "build-finished":
            success = event.get("success")
            build_events.append(success if type(success) is bool else None)
            continue
        if isinstance(event, dict) and event.get("reason") == "compiler-message":
            if build_events:
                continue
            message = event.get("message")
            if not isinstance(message, dict) or message.get("level") != "error":
                continue
            prebuild_compiler_errors += 1
            code = message.get("code")
            code_value = code.get("code") if isinstance(code, dict) else None
            if isinstance(code_value, str) and re.fullmatch(r"E[0-9]{4}", code_value):
                if code_value not in compiler_codes and len(compiler_codes) < 8:
                    compiler_codes.append(code_value)
            spans = message.get("spans")
            if not isinstance(spans, list):
                spans = []
            for span in spans:
                if not isinstance(span, dict) or span.get("is_primary") is not True:
                    continue
                file_name = span.get("file_name")
                line = span.get("line_start")
                if (
                    not isinstance(file_name, str)
                    or not isinstance(line, int)
                    or isinstance(line, bool)
                    or line < 1
                ):
                    continue
                location = package_source_location(
                    package, file_name, line, admitted_paths
                )
                if (
                    location
                    and location not in source_locations
                    and len(source_locations) < 8
                ):
                    source_locations.append(location)
            continue

    valid_build_event = len(build_events) == 1 and build_events[0] is not None
    build_succeeded = build_events[0] if valid_build_event else None
    compiler_errors = (
        prebuild_compiler_errors
        if valid_build_event and build_succeeded is False
        else 0
    )
    if not valid_build_event:
        classification = "build_phase_unknown"
        compiler_codes = []
        source_locations = []
    elif build_succeeded is False and compiler_errors:
        classification = "compiler_error"
    elif build_succeeded is False:
        classification = "build_failure_unclassified"
    elif exit_code:
        classification = "post_build_failure_unclassified"
    else:
        classification = "build_failure_unclassified"
    return {
        "classification": classification,
        "cargo_exit_code": exit_code,
        "build_finished_count": len(build_events),
        "build_succeeded": build_succeeded,
        "compiler_error_count": compiler_errors,
        "compiler_error_codes": compiler_codes,
        "primary_source_locations": source_locations,
        "test_summary_count": 0,
        "passed": 0,
        "failed": 0,
        "ignored": 0,
    }


def run_library_suite(
    package_name: str,
    metadata: dict[str, dict[str, Any]],
    admitted_paths: set[str],
    context: dict[str, Any],
) -> dict[str, Any]:
    target = LIBRARY_TEST_TARGETS.get(package_name)
    package = metadata.get(package_name)
    if target is None or package is None:
        refuse("library test package is outside the fixed Cargo metadata inventory")
    target_name, _ = target
    command = [
        str(context["cargo"]),
        "test",
        "--locked",
        "-p",
        package_name,
        "--lib",
        "--no-run",
        "--message-format=json",
    ]
    artifact = cargo_artifact_details(
        command,
        metadata,
        package_name,
        target_name,
        "lib",
        admitted_paths=admitted_paths,
        require_test_profile=True,
        capture_runtime_inputs=True,
    )
    binary = artifact_path(artifact["relative_path"])
    before_hash = sha256_file(binary)
    if before_hash != artifact["sha256"]:
        refuse(f"bound library test executable changed before invocation: {package_name}")
    environment = cargo_library_test_environment(
        package_name,
        package,
        context,
        artifact["linked_paths"],
        dict(os.environ),
    )
    try:
        result = subprocess.run(
            [str(binary)],
            cwd=Path(package["manifest_path"]).resolve(strict=True).parent,
            env=environment,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        output = result.stdout.decode("utf-8", errors="replace")
        output_hash = sha256_bytes(result.stdout)
        process_result = cargo_library_process_result(
            result.returncode, parse_test_summaries(output), output_hash
        )
    except OSError:
        output_hash = sha256_bytes(b"")
        process_result = cargo_library_process_result(
            None, [], output_hash, spawn_failed=True
        )
    after_hash = sha256_file(binary)
    if after_hash != before_hash:
        refuse(f"bound library test executable changed during invocation: {package_name}")
    if process_result["classification"] != "passed":
        refuse(
            "required library suite failed: "
            f"{package_name}; output_sha256={process_result['output_sha256']}; "
            "diagnostic_summary="
            f"{json.dumps(process_result, sort_keys=True, separators=(',', ':'))}"
        )
    return {
        "package": package_name,
        "command": [
            "cargo test --locked -p <fixed-package> --lib --no-run --message-format=json",
            "<bound-library-test-executable>",
        ],
        "summaries": parse_test_summaries(output),
        "process": process_result,
        "output_sha256": output_hash,
        "artifact_relative_path": str(artifact["relative_path"]),
        "artifact_sha256": before_hash,
    }


def cargo_metadata_index() -> dict[str, dict[str, Any]]:
    command = [
        "cargo",
        "metadata",
        "--locked",
        "--no-deps",
        "--format-version",
        "1",
        "--manifest-path",
        "Cargo.toml",
    ]
    result = subprocess.run(
        command,
        cwd=CODEX_RS,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        refuse(f"locked Cargo metadata failed with exit {result.returncode}")
    try:
        metadata = json.loads(result.stdout)
    except json.JSONDecodeError:
        refuse("locked Cargo metadata did not return JSON")
    packages = metadata.get("packages")
    if not isinstance(packages, list):
        refuse("locked Cargo metadata has no package inventory")
    indexed: dict[str, dict[str, Any]] = {}
    for name, relative_manifest in EXPECTED_PACKAGE_MANIFESTS.items():
        expected_manifest = (CODEX_RS / relative_manifest).resolve(strict=True)
        matches = [
            package
            for package in packages
            if package.get("name") == name
            and Path(package.get("manifest_path", "")).resolve(strict=True) == expected_manifest
        ]
        if (
            len(matches) != 1
            or not isinstance(matches[0].get("id"), str)
            or not matches[0]["id"]
        ):
            refuse(f"Cargo metadata did not bind one exact package identity and manifest: {name}")
        indexed[name] = matches[0]
    return indexed


def target_identity(target: dict[str, Any]) -> dict[str, Any]:
    return {field: target.get(field) for field in TARGET_IDENTITY_FIELDS}


def compiler_artifact_matches(
    event: dict[str, Any],
    package_id: str,
    expected_target: dict[str, Any],
    require_test_profile: bool,
) -> bool:
    target = event.get("target")
    profile = event.get("profile")
    executable = event.get("executable")
    return (
        event.get("package_id") == package_id
        and isinstance(target, dict)
        and target_identity(target) == expected_target
        and isinstance(executable, str)
        and bool(executable)
        and isinstance(profile, dict)
        and (not require_test_profile or profile.get("test") is True)
    )


def cargo_artifact_details(
    command: list[str],
    metadata: dict[str, dict[str, Any]],
    package_name: str,
    target_name: str,
    kind: str,
    admitted_paths: set[str] | None = None,
    require_test_profile: bool = False,
    capture_runtime_inputs: bool = False,
) -> dict[str, Any]:
    package = metadata.get(package_name)
    expected_source = EXPECTED_ARTIFACT_TARGETS.get((package_name, target_name, kind))
    if package is None or expected_source is None:
        refuse(f"artifact request is outside the exact Cargo metadata inventory: {package_name}/{target_name}")
    expected_target_source = (CODEX_RS / expected_source).resolve(strict=True)
    target_matches = [
        target
        for target in package.get("targets", [])
        if target.get("name") == target_name
        and kind in target.get("kind", [])
        and Path(target.get("src_path", "")).resolve(strict=True) == expected_target_source
    ]
    if len(target_matches) != 1:
        refuse(f"Cargo metadata did not bind one exact target source: {package_name}/{target_name}")
    expected_target = target_identity(target_matches[0])
    if require_test_profile and (
        target_matches[0].get("kind") != ["lib"]
        or target_matches[0].get("test") is not True
        or not isinstance(target_matches[0].get("crate_types"), list)
        or "lib" not in target_matches[0]["crate_types"]
    ):
        refuse(f"Cargo metadata library test target is unsupported: {package_name}")
    if capture_runtime_inputs and any(
        "custom-build" in target.get("kind", [])
        for target in package.get("targets", [])
        if isinstance(target, dict)
    ):
        refuse(f"library package build-script runtime environment is unsupported: {package_name}")
    result = subprocess.run(
        command,
        cwd=CODEX_RS,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT if capture_runtime_inputs else None,
    )
    if result.returncode != 0:
        if capture_runtime_inputs:
            output_hash = sha256_bytes(result.stdout.encode())
            failure = bounded_library_failure_summary(
                package_name, result.stdout, result.returncode, admitted_paths or set()
            )
            refuse(
                f"required locked library build failed: {package_name}; "
                f"output_sha256={output_hash}; "
                f"diagnostic_summary={json.dumps(failure, sort_keys=True, separators=(',', ':'))}"
            )
        refuse(f"required locked compile failed for {package_name}/{target_name}")
    artifacts: list[Path] = []
    build_finished: list[bool | None] = []
    linked_paths: list[str] = []
    for line in result.stdout.splitlines():
        try:
            event = json.loads(line)
        except (ValueError, RecursionError):
            continue
        if not isinstance(event, dict):
            continue
        if event.get("reason") == "build-finished":
            success = event.get("success")
            build_finished.append(success if type(success) is bool else None)
            continue
        if capture_runtime_inputs and event.get("reason") == "build-script-executed":
            event_paths = event.get("linked_paths")
            if not isinstance(event_paths, list) or not all(
                isinstance(path, str) for path in event_paths
            ):
                refuse("Cargo build-script library path metadata is invalid")
            linked_paths.extend(event_paths)
            if event.get("package_id") == package["id"] and event.get("env") != []:
                refuse(f"library package build-script environment is unsupported: {package_name}")
        if event.get("reason") != "compiler-artifact":
            continue
        if compiler_artifact_matches(
            event, package["id"], expected_target, require_test_profile
        ):
            artifacts.append(Path(event["executable"]))
    if len(artifacts) != 1:
        refuse(f"expected exactly one compiler artifact for {package_name}/{target_name}")
    if capture_runtime_inputs and build_finished != [True]:
        refuse(f"locked library build phase was not exactly successful: {package_name}")
    artifact = artifacts[0]
    if not artifact.is_absolute():
        artifact = CODEX_RS / artifact
    target_root = Path(os.environ.get("CARGO_TARGET_DIR", ""))
    if not target_root.is_absolute():
        refuse("fixed Cargo target directory is unavailable")
    if artifact.is_symlink() or not artifact.is_file() or not os.access(artifact, os.X_OK):
        refuse(f"compiler artifact is not a regular executable: {target_name}")
    resolved_target = target_root.resolve(strict=True)
    resolved_artifact = artifact.resolve(strict=True)
    try:
        relative = resolved_artifact.relative_to(resolved_target)
    except ValueError:
        refuse(f"compiler artifact escaped the fixed Cargo target root: {target_name}")
    if not stat.S_ISREG(resolved_artifact.stat().st_mode):
        refuse(f"compiler artifact is not regular: {target_name}")
    return {
        "relative_path": relative,
        "sha256": sha256_file(resolved_artifact),
        "linked_paths": linked_paths,
        "build_finished": build_finished,
        "output_sha256": sha256_bytes(result.stdout.encode()),
    }


def cargo_artifact(
    command: list[str],
    metadata: dict[str, dict[str, Any]],
    package_name: str,
    target_name: str,
    kind: str,
) -> Path:
    return cargo_artifact_details(
        command, metadata, package_name, target_name, kind
    )["relative_path"]


def pinned_toolchain_executable(name: str) -> Path:
    result = subprocess.run(
        ["rustup", "which", name, "--toolchain", "1.96.0"],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    values = result.stdout.splitlines()
    if result.returncode != 0 or len(values) != 1:
        refuse("pinned Rust toolchain executable is unavailable")
    executable = Path(values[0])
    if (
        not executable.is_absolute()
        or executable.is_symlink()
        or not executable.is_file()
        or not os.access(executable, os.X_OK)
    ):
        refuse("pinned Rust toolchain executable is invalid")
    try:
        if executable.resolve(strict=True) != executable:
            refuse("pinned Rust toolchain executable is invalid")
    except OSError:
        refuse("pinned Rust toolchain executable is invalid")
    return executable


def library_runtime_context() -> dict[str, Any]:
    cargo_executable = pinned_toolchain_executable("cargo")
    rustc_executable = pinned_toolchain_executable("rustc")
    version_result = subprocess.run(
        [str(rustc_executable), "--version", "--verbose"],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    host_values = [
        line.removeprefix("host: ")
        for line in version_result.stdout.splitlines()
        if line.startswith("host: ")
    ]
    if version_result.returncode != 0 or host_values != [LINUX_HOST_TARGET]:
        refuse("pinned Rust host target is unsupported")
    libdir_result = subprocess.run(
        [
            str(rustc_executable),
            "--print",
            "target-libdir",
            "--target",
            LINUX_HOST_TARGET,
        ],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    libdirs = libdir_result.stdout.splitlines()
    if libdir_result.returncode != 0 or len(libdirs) != 1:
        refuse("pinned Rust sysroot library directory is unavailable")
    sysroot_libdir = Path(libdirs[0])
    if not sysroot_libdir.is_absolute() or not sysroot_libdir.is_dir():
        refuse("pinned Rust sysroot library directory is invalid")
    try:
        sysroot_libdir = sysroot_libdir.resolve(strict=True)
    except OSError:
        refuse("pinned Rust sysroot library directory is invalid")
    target_root = Path(os.environ.get("CARGO_TARGET_DIR", ""))
    if not target_root.is_absolute():
        refuse("fixed Cargo target directory is unavailable")
    return {
        "cargo": cargo_executable,
        "rustc": rustc_executable,
        "sysroot_libdir": sysroot_libdir,
        "target_root": target_root.resolve(strict=False),
    }


def cargo_library_test_environment(
    package_name: str,
    package: dict[str, Any],
    context: dict[str, Any],
    linked_paths: list[str],
    inherited_environment: dict[str, str],
) -> dict[str, str]:
    relative_manifest = EXPECTED_PACKAGE_MANIFESTS.get(package_name)
    if relative_manifest is None or package_name not in LIBRARY_TEST_TARGETS:
        refuse("library test package is outside the fixed package inventory")
    expected_manifest = (CODEX_RS / relative_manifest).resolve(strict=True)
    manifest_value = package.get("manifest_path")
    if not isinstance(manifest_value, str):
        refuse("selected Cargo package manifest metadata is invalid")
    if Path(manifest_value).resolve(strict=True) != expected_manifest:
        refuse("selected Cargo package manifest identity changed")
    variables = cargo_package_runtime_variables(package, str(context["cargo"]))
    if variables["CARGO_PKG_NAME"] != package_name:
        refuse("selected Cargo package identity changed")
    root_output = context["target_root"] / "debug"
    deps_output = root_output / "deps"
    dylib_path = cargo_runtime_search_path(
        linked_paths,
        root_output,
        deps_output,
        context["sysroot_libdir"],
        inherited_environment.get("LD_LIBRARY_PATH"),
    )
    return compose_cargo_test_environment(inherited_environment, variables, dylib_path)


def artifact_path(relative: Path) -> Path:
    target_root = Path(os.environ["CARGO_TARGET_DIR"]).resolve(strict=True)
    result = target_root / relative
    if result.is_symlink() or not result.is_file() or not os.access(result, os.X_OK):
        refuse(f"bound compiler artifact disappeared: {relative.name}")
    return result


def cargo_bin_fallback(test_binary: Path, name: str) -> Path:
    directory = test_binary.parent
    if directory.name == "deps":
        directory = directory.parent
    return directory / name


def ignored_inventory(binary: Path) -> set[str]:
    result = subprocess.run(
        [str(binary), "--list", "--ignored"],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        env={"PATH": "/usr/bin:/bin", "TMPDIR": "/tmp", "RUST_TEST_THREADS": "1"},
    )
    if result.returncode != 0:
        refuse(f"could not enumerate ignored test inventory: {binary.name}")
    return {
        line[: -len(": test")]
        for line in result.stdout.splitlines()
        if line.endswith(": test") and len(line) > len(": test")
    }


def run_ignored_root_test(
    binary: Path, test_name: str, expected_binary_sha256: str
) -> dict[str, Any]:
    if sha256_file(binary) != expected_binary_sha256:
        refuse(f"root fixture binary changed before invocation: {test_name}")
    command = [
        "/usr/bin/sudo",
        "-n",
        "--",
        "/usr/bin/env",
        "-i",
        "PATH=/usr/bin:/bin",
        "TMPDIR=/tmp",
        "RUST_TEST_THREADS=1",
        str(binary),
        "--exact",
        test_name,
        "--ignored",
        "--nocapture",
    ]
    result = subprocess.run(
        command,
        cwd=ROOT,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    summaries = parse_test_summaries(result.stdout)
    success = {"status": "ok", "passed": 1, "failed": 0, "ignored": 0}
    if test_name == EXPECTED_MCP_TESTS[0]:
        nested_runs = sum(line.strip() == "running 1 test" for line in result.stdout.splitlines())
        named_runs = sum(line.startswith(f"test {test_name} ") for line in result.stdout.splitlines())
        terminal = result.stdout.rstrip().splitlines()[-1] if result.stdout.strip() else ""
        terminal_summary = parse_test_summaries(terminal)
        if (
            result.returncode != 0
            or summaries != [success, success]
            or nested_runs != 2
            or named_runs != 2
            or terminal_summary != [success]
        ):
            refuse("retained-client fixture did not produce one successful child and terminal outer test")
    elif result.returncode != 0 or summaries != [success]:
        refuse(f"explicit synthetic root fixture was not exactly one successful executed test: {test_name}")
    if sha256_file(binary) != expected_binary_sha256:
        refuse(f"root fixture binary changed during invocation: {test_name}")
    return {
        "test": test_name,
        "status": "passed",
        "counts": summaries[-1],
        "summaries": summaries,
        "binary_sha256": expected_binary_sha256,
        "output_sha256": sha256_bytes(result.stdout.encode()),
    }


def write_report(name: str, report: dict[str, Any]) -> None:
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    output = runner_temp / name
    output.mkdir(mode=0o700)
    (output / "result.json").write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def validate_linux(manifest: dict[str, Any]) -> None:
    head, workflow_sha = verify_inputs(manifest)
    if manifest["phase"] != "validate":
        refuse("compiled Linux validation is permitted only in validate phase")
    if os.environ.get("EXPECTED_RUNNER_OS") != "Linux" or os.environ.get("RUNNER_TEMP") is None:
        refuse("Linux validation runner identity is unavailable")

    metadata = cargo_metadata_index()
    admitted_paths = set(manifest["product_inputs"])
    runtime_context = library_runtime_context()
    library_results = [
        run_library_suite(package, metadata, admitted_paths, runtime_context)
        for package in manifest["linux_library_test_packages"]
    ]
    cli_relative = cargo_artifact(
        ["cargo", "build", "--locked", "-p", "codex-cli", "--bin", "codex", "--message-format=json"],
        metadata,
        "codex-cli",
        "codex",
        "bin",
    )
    cli_test_relative = cargo_artifact(
        [
            "cargo",
            "test",
            "--locked",
            "-p",
            "codex-cli",
            "--test",
            "runtime_execution_proof",
            "--no-run",
            "--message-format=json",
        ],
        metadata,
        "codex-cli",
        "runtime_execution_proof",
        "test",
    )
    mcp_library_result = next(
        (item for item in library_results if item["package"] == "codex-mcp"), None
    )
    if mcp_library_result is None:
        refuse("required MCP library test artifact is unavailable")
    mcp_test_relative = Path(mcp_library_result["artifact_relative_path"])
    cli_binary = artifact_path(cli_test_relative)
    mcp_binary = artifact_path(mcp_test_relative)
    cli_image = artifact_path(cli_relative)
    if cargo_bin_fallback(cli_binary, "codex").resolve(strict=True) != cli_image.resolve(strict=True):
        refuse("CLI fixture's cargo_bin fallback does not resolve to the bound CLI image")
    expected_cli = set(manifest["cli_ignored_tests"])
    expected_mcp = set(manifest["mcp_ignored_tests"])
    if ignored_inventory(cli_binary) != expected_cli:
        refuse("CLI ignored-test inventory differs from the exact admitted seven fixtures")
    if ignored_inventory(mcp_binary) != expected_mcp:
        refuse("MCP ignored-test inventory differs from the exact admitted retained-send fixture")

    cli_image_sha256 = sha256_file(cli_image)
    cli_test_sha256 = sha256_file(cli_binary)
    mcp_test_sha256 = mcp_library_result["artifact_sha256"]
    if sha256_file(mcp_binary) != mcp_test_sha256:
        refuse("MCP library test artifact changed after its bound test run")
    fixtures = []
    for test_name in manifest["cli_ignored_tests"]:
        fixtures.append(run_ignored_root_test(cli_binary, test_name, cli_test_sha256))
    for test_name in manifest["mcp_ignored_tests"]:
        fixtures.append(run_ignored_root_test(mcp_binary, test_name, mcp_test_sha256))
    if len(fixtures) != 8 or any(item["status"] != "passed" for item in fixtures):
        refuse("not all eight exact synthetic root fixtures passed")
    if sha256_file(cli_image) != cli_image_sha256:
        refuse("CLI image changed during synthetic root fixture execution")
    verify_clean_checkout()

    report = {
        "repository": EXPECTED_REPOSITORY,
        "source_commit": head,
        "workflow_commit": workflow_sha,
        "run_id": os.environ.get("GITHUB_RUN_ID", ""),
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", ""),
        "phase": "validate-linux",
        "runner": "ubuntu-24.04",
        "library_tests": library_results,
        "artifacts": {
            "cli_executable": {"target_relative_path": str(cli_relative), "sha256": cli_image_sha256},
            "cli_runtime_execution_proof_test": {
                "target_relative_path": str(cli_test_relative),
                "sha256": cli_test_sha256,
            },
            "codex_mcp_lib_test": {
                "target_relative_path": str(mcp_test_relative),
                "sha256": mcp_test_sha256,
            },
        },
        "ignored_root_fixtures": fixtures,
        "acceptance": "source-specific Linux compile and synthetic root fixture proof only",
    }
    write_report("runtime-proof-validation-linux", report)


def validate_windows(manifest: dict[str, Any]) -> None:
    head, workflow_sha = verify_inputs(manifest)
    if manifest["phase"] != "validate" or os.environ.get("EXPECTED_RUNNER_OS") != "Windows":
        refuse("Windows validation runner or phase does not match the fixed contract")
    command = ["cargo", "check", "--locked", "--all-targets"]
    for package in WINDOWS_PACKAGES:
        command.extend(["-p", package])
    result = subprocess.run(command, cwd=CODEX_RS, check=False)
    if result.returncode != 0:
        refuse(f"ordinary Windows all-target source check failed with exit {result.returncode}")
    verify_clean_checkout()
    write_report(
        "runtime-proof-validation-windows",
        {
            "repository": EXPECTED_REPOSITORY,
            "source_commit": head,
            "workflow_commit": workflow_sha,
            "run_id": os.environ.get("GITHUB_RUN_ID", ""),
            "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", ""),
            "phase": "validate-windows",
            "runner": "windows-2022",
            "command": command,
            "packages": WINDOWS_PACKAGES,
            "status": "passed",
            "acceptance": "ordinary supported-platform all-target compile only; no Linux protected fixture on Windows",
        },
    )


def main() -> None:
    if len(sys.argv) != 2 or sys.argv[1] not in {"bind", "prepare", "validate-linux", "validate-windows"}:
        refuse("use one fixed phase command: bind, prepare, validate-linux, or validate-windows")
    mode = sys.argv[1]
    manifest = load_manifest()
    if mode == "bind":
        emit_phase()
    elif mode == "prepare":
        prepare(manifest)
    elif mode == "validate-linux":
        validate_linux(manifest)
    else:
        validate_windows(manifest)


if __name__ == "__main__":
    main()
