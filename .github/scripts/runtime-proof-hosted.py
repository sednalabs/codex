#!/usr/bin/env python3
"""Fixed two-phase hosted preparation and validation for the runtime-proof source."""

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import sys
import tempfile
import tomllib
import uuid
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
LINUX_HOST_TARGET = "x86_64-unknown-linux-gnu"
RUNNER_MODE = "--cargo-runtime-proof-runner"
RUNNER_BINDING_KEYS = {
    "schema_version",
    "invocation_id",
    "package",
    "head",
    "script_sha256",
    "cwd",
    "artifact_relative_path",
    "artifact_sha256",
}
RUNNER_RESULT_KEYS = {
    "schema_version",
    "invocation_id",
    "package",
    "head",
    "script_sha256",
    "cwd",
    "artifact_relative_path",
    "artifact_sha256",
    "process",
}
MAX_RUNNER_RECORD_BYTES = 8192
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
ROOT_FIXTURE_STAGE = re.compile(r"^runtime-proof-root-stage:([a-z_]+)$")
ROOT_FIXTURE_SPAWN_ERROR = re.compile(
    r"launch protected codex fixture failed: "
    r"(not_found|permission_denied|invalid_input|other)"
    r"(?: \(errno ([0-9]{1,5})\)| \(no errno\))"
)
ROOT_FIXTURE_STAGE_NAMES = {
    "started",
    "fixture_started",
    "cli_launch_begin",
    "cli_executable_ready",
    "cli_channels_ready",
    "cli_child_spawned",
    "cli_prompt_written",
    "cli_bootstrap_sent",
    "cli_auth_sent",
    "cli_waiting_for_ack",
    "cli_ack_validated",
    "cli_process_exited",
    "cli_output_captured",
    "cli_timed_out",
    "cli_succeeded",
    "cli_failed",
    "claims_validated",
    "wait_targets_validated",
    "wait_result_validated",
    "proofs_validated",
    "model_redaction_validated",
    "auth_headers_validated",
    "cli_output_redacted",
    "fixture_storage_clean",
    "complete",
}
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
STANDARD_RUST_CRATES = {
    "alloc",
    "compiler_builtins",
    "core",
    "proc_macro",
    "std",
    "test",
}
INCOMPATIBLE_RUSTC_METADATA_MESSAGE = re.compile(
    r"found crate [`']([A-Za-z0-9_-]+)[`'] compiled by an incompatible version of rustc"
)
INCOMPATIBLE_RUSTC_METADATA_DEPENDENCY_MESSAGE = re.compile(
    r"found crate [`']([A-Za-z0-9_-]+)[`'] compiled by an incompatible version of rustc which [`']([A-Za-z0-9_-]+)[`'] depends on"
)


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
    if len(value["product_inputs"]) != 41:
        refuse("manifest must bind the exact forty-one frozen product inputs")
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
    verify_test_summary_parser_contract(manifest)
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


def make_runner_binding(
    package: str,
    head: str,
    script_sha256: str,
    cwd: str,
    artifact_relative_path: str,
    artifact_sha256: str,
    invocation_id: str,
) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "invocation_id": invocation_id,
        "package": package,
        "head": head,
        "script_sha256": script_sha256,
        "cwd": cwd,
        "artifact_relative_path": artifact_relative_path,
        "artifact_sha256": artifact_sha256,
    }


def valid_runner_binding(value: Any) -> bool:
    if not isinstance(value, dict) or not isinstance(
        value.get("artifact_relative_path"), str
    ):
        return False
    package = value.get("package")
    artifact_path = PurePosixPath(value["artifact_relative_path"])
    expected_cwd = None
    if isinstance(package, str) and package in EXPECTED_PACKAGE_MANIFESTS:
        expected_cwd = "codex-rs/" + PurePosixPath(
            EXPECTED_PACKAGE_MANIFESTS[package]
        ).parent.as_posix()
    return (
        isinstance(value, dict)
        and set(value) == RUNNER_BINDING_KEYS
        and type(value.get("schema_version")) is int
        and value["schema_version"] == 1
        and package in CARGO_PACKAGES
        and value.get("cwd") == expected_cwd
        and isinstance(value.get("head"), str)
        and re.fullmatch(r"[0-9a-f]{40}", value["head"]) is not None
        and isinstance(value.get("script_sha256"), str)
        and re.fullmatch(r"[0-9a-f]{64}", value["script_sha256"]) is not None
        and not artifact_path.is_absolute()
        and bool(artifact_path.parts)
        and ".." not in artifact_path.parts
        and value["artifact_relative_path"] == artifact_path.as_posix()
        and isinstance(value.get("artifact_sha256"), str)
        and re.fullmatch(r"[0-9a-f]{64}", value["artifact_sha256"]) is not None
        and isinstance(value.get("invocation_id"), str)
        and re.fullmatch(r"[0-9a-f]{32}", value["invocation_id"]) is not None
    )


def make_runner_result(
    binding: dict[str, Any], process: dict[str, Any]
) -> dict[str, Any]:
    return {
        **{key: binding[key] for key in RUNNER_BINDING_KEYS},
        "process": process,
    }


def runner_process_result_shape(value: Any) -> bool:
    return (
        isinstance(value, dict)
        and set(value)
        == {
            "classification",
            "exit_code",
            "signal",
            "test_summary_count",
            "passed",
            "failed",
            "ignored",
            "output_sha256",
            "summaries",
        }
        and isinstance(value.get("classification"), str)
        and value.get("classification")
        in {
            "passed",
            "test_process_spawn_failure",
            "test_process_status_unknown",
            "test_process_signal",
            "test_process_exit_failure",
            "missing_test_summary",
            "test_summary_failure",
        }
        and (
            value.get("exit_code") is None
            or type(value.get("exit_code")) is int
        )
        and (
            value.get("signal") is None
            or (type(value.get("signal")) is int and value["signal"] > 0)
        )
        and all(
            type(value.get(key)) is int and value[key] >= 0
            for key in ("test_summary_count", "passed", "failed", "ignored")
        )
        and isinstance(value.get("output_sha256"), str)
        and re.fullmatch(r"[0-9a-f]{64}", value["output_sha256"]) is not None
        and isinstance(value.get("summaries"), list)
        and all(
            isinstance(summary, dict)
            and set(summary) == {"status", "passed", "failed", "ignored"}
            and isinstance(summary.get("status"), str)
            and summary.get("status") in {"ok", "FAILED"}
            and all(
                type(summary.get(key)) is int and summary[key] >= 0
                for key in ("passed", "failed", "ignored")
            )
            for summary in value["summaries"]
        )
    )


def valid_runner_result(value: Any, binding: dict[str, Any]) -> bool:
    return (
        isinstance(value, dict)
        and set(value) == RUNNER_RESULT_KEYS
        and all(value.get(key) == binding.get(key) for key in RUNNER_BINDING_KEYS)
        and runner_process_result_shape(value.get("process"))
    )


def runner_process_is_consistent(process: dict[str, Any]) -> bool:
    summaries = process["summaries"]
    classification = process["classification"]
    if classification == "test_process_spawn_failure":
        expected = cargo_library_process_result(
            None, [], process["output_sha256"], spawn_failed=True
        )
    elif process["signal"] is not None:
        expected = cargo_library_process_result(
            -process["signal"], summaries, process["output_sha256"]
        )
    elif process["exit_code"] is not None:
        expected = cargo_library_process_result(
            process["exit_code"], summaries, process["output_sha256"]
        )
    else:
        return False
    return {**expected, "summaries": summaries} == process


def cargo_runner_command(
    cargo: str, package: str, runner_args: list[str]
) -> list[str]:
    runner_config = (
        f"target.'{LINUX_HOST_TARGET}'.runner="
        f"{json.dumps(runner_args, separators=(',', ':'))}"
    )
    return [
        cargo,
        "--config",
        runner_config,
        "test",
        "--locked",
        "-p",
        package,
        "--lib",
        "--message-format=json",
    ]


def library_runner_decision(
    cargo_exit_code: int,
    captured_output_sha256: str,
    result_state: str,
    runner_value: Any,
    binding: dict[str, Any],
    cargo_summaries: list[dict[str, int | str]] | None,
    expected_binary_sha256: str,
    actual_binary_sha256: str | None,
) -> dict[str, Any]:
    child_status = "unknown"
    runner_output_sha256 = None
    reason = "accepted"
    if result_state == "missing":
        reason = "runner_result_missing"
    elif result_state == "refused":
        reason = "runner_result_refused"
    elif cargo_summaries is None:
        reason = "cargo_summary_unsupported"
    elif result_state != "present" or not valid_runner_result(runner_value, binding):
        reason = "runner_result_invalid"
    else:
        process = runner_value["process"]
        if not runner_process_is_consistent(process):
            reason = "runner_process_invalid"
        else:
            child_status = process["classification"]
            runner_output_sha256 = process["output_sha256"]
            if cargo_summaries != process["summaries"]:
                reason = "cargo_runner_summaries_disagree"
            elif actual_binary_sha256 is None:
                reason = "bound_binary_unavailable"
            elif expected_binary_sha256 != actual_binary_sha256:
                reason = "bound_binary_changed"
            elif cargo_exit_code != 0:
                reason = "cargo_exit_nonzero"
            elif child_status != "passed":
                reason = "child_not_passed"
    return {
        "accepted": reason == "accepted",
        "reason": reason,
        "cargo_exit_code": cargo_exit_code,
        "captured_output_sha256": captured_output_sha256,
        "child_status": child_status,
        "runner_output_sha256": runner_output_sha256,
    }


def runner_binding_bytes(binding: dict[str, Any]) -> bytes:
    return (json.dumps(binding, sort_keys=True, separators=(",", ":")) + "\n").encode()


def write_private_runner_binding(path: Path, binding: dict[str, Any]) -> None:
    payload = runner_binding_bytes(binding)
    if len(payload) > MAX_RUNNER_RECORD_BYTES:
        refuse("Cargo runner binding exceeds its fixed size limit")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags, 0o600)
    except OSError:
        refuse("Cargo runner binding could not be created exclusively")
    try:
        remaining = memoryview(payload)
        while remaining:
            written = os.write(descriptor, remaining)
            if written <= 0:
                refuse("Cargo runner binding write was incomplete")
            remaining = remaining[written:]
    finally:
        os.close(descriptor)


def private_record_directory(path: Path) -> bool:
    try:
        metadata = path.lstat()
    except OSError:
        return False
    return (
        stat.S_ISDIR(metadata.st_mode)
        and not path.is_symlink()
        and metadata.st_uid == os.geteuid()
        and stat.S_IMODE(metadata.st_mode) == 0o700
    )


def read_private_runner_record(path: Path) -> Any | None:
    directory = path.parent
    if not private_record_directory(directory):
        refuse("Cargo runner private result directory is invalid")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except FileNotFoundError:
        return None
    except OSError:
        refuse("Cargo runner result could not be opened safely")
    try:
        metadata = os.fstat(descriptor)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or stat.S_IMODE(metadata.st_mode) != 0o600
            or metadata.st_size > MAX_RUNNER_RECORD_BYTES
        ):
            refuse("Cargo runner result file is invalid")
        chunks = bytearray()
        while len(chunks) <= MAX_RUNNER_RECORD_BYTES:
            chunk = os.read(
                descriptor,
                MAX_RUNNER_RECORD_BYTES + 1 - len(chunks),
            )
            if not chunk:
                break
            chunks.extend(chunk)
        payload = bytes(chunks)
    finally:
        os.close(descriptor)
    if not payload or len(payload) > MAX_RUNNER_RECORD_BYTES:
        return None
    try:
        return json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError, RecursionError):
        return None


def reserve_private_runner_result(path: Path) -> int:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    try:
        return os.open(path, flags, 0o600)
    except OSError:
        refuse("Cargo runner invocation could not be claimed exclusively")


def write_reserved_runner_result(
    descriptor: int, binding: dict[str, Any], process: dict[str, Any]
) -> None:
    result = make_runner_result(binding, process)
    payload = (json.dumps(result, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if len(payload) > MAX_RUNNER_RECORD_BYTES:
        refuse("Cargo runner result exceeds its fixed size limit")
    remaining = memoryview(payload)
    while remaining:
        written = os.write(descriptor, remaining)
        if written <= 0:
            refuse("Cargo runner result write was incomplete")
        remaining = remaining[written:]


def runner_binding_path(path_value: str) -> tuple[Path, dict[str, Any]]:
    path = Path(path_value)
    if not path.is_absolute() or path.name != "binding.json":
        refuse("Cargo runner binding path is invalid")
    directory = path.parent
    runner_temp = Path(os.environ.get("RUNNER_TEMP", ""))
    try:
        resolved_temp = runner_temp.resolve(strict=True)
        resolved_directory = directory.resolve(strict=True)
        if not resolved_directory.is_relative_to(resolved_temp):
            refuse("Cargo runner binding path is outside the private runner directory")
    except OSError:
        refuse("Cargo runner private directory is unavailable")
    if not private_record_directory(directory):
        refuse("Cargo runner private directory is invalid")
    record = read_private_runner_record(path)
    if not valid_runner_binding(record):
        refuse("Cargo runner binding record is invalid")
    return path, record


def runner_package_source_digest(
    package: str, source_relative: str, manifest: dict[str, Any]
) -> str:
    expected_target = LIBRARY_TEST_TARGETS.get(package)
    if (
        expected_target is None
        or source_relative != f"codex-rs/{expected_target[1]}"
    ):
        refuse("Cargo runner package source is not admitted")

    product_commit = manifest["product_source_commit"]
    if not re.fullmatch(r"[0-9a-f]{40}", product_commit):
        refuse("Cargo runner package source commit is invalid")
    entry = run_git("ls-tree", "-z", product_commit, "--", source_relative)
    entry = entry[:-1] if entry.endswith("\0") else entry
    metadata, separator, tracked_path = entry.partition("\t")
    fields = metadata.split()
    if (
        not separator
        or tracked_path != source_relative
        or len(fields) != 3
        or fields[0] not in {"100644", "100755"}
        or fields[1] != "blob"
    ):
        refuse("Cargo runner package source is not admitted")
    committed_source = run_git("show", f"{product_commit}:{source_relative}").encode()
    digest = sha256_bytes(committed_source)
    source_path = ROOT / source_relative
    if (
        source_path.is_symlink()
        or not source_path.is_file()
        or sha256_file(source_path) != digest
    ):
        refuse("Cargo runner package source binding changed")
    return digest


def check_runner_binary(binding: dict[str, Any], binary_arg: str) -> Path:
    package = binding["package"]
    target_name, _ = LIBRARY_TEST_TARGETS[package]
    source_relative = EXPECTED_ARTIFACT_TARGETS[(package, target_name, "lib")]
    manifest = load_manifest()
    head, _ = verify_inputs(manifest)
    if manifest["phase"] != "validate" or head != binding["head"]:
        refuse("Cargo runner source binding does not match the validation candidate")
    if sha256_file(Path(__file__).resolve(strict=True)) != binding["script_sha256"]:
        refuse("Cargo runner script digest changed")
    source_relative = f"codex-rs/{source_relative}"
    expected_source_hash = runner_package_source_digest(
        package, source_relative, manifest
    )
    source_path = ROOT / source_relative
    if (
        expected_source_hash is None
        or source_path.is_symlink()
        or not source_path.is_file()
        or sha256_file(source_path) != expected_source_hash
    ):
        refuse("Cargo runner package source binding changed")
    expected_cwd = (
        CODEX_RS / Path(EXPECTED_PACKAGE_MANIFESTS[package]).parent
    ).resolve(strict=True)
    try:
        current_cwd = Path.cwd().resolve(strict=True)
    except OSError:
        refuse("Cargo runner package working directory is unavailable")
    if current_cwd != expected_cwd:
        refuse("Cargo runner package working directory does not match")
    target_root = Path(os.environ.get("CARGO_TARGET_DIR", ""))
    if not target_root.is_absolute():
        refuse("Cargo runner target root is unavailable")
    try:
        target_root = target_root.resolve(strict=True)
        relative = Path(binding["artifact_relative_path"])
        expected_binary = target_root / relative
        binary = Path(binary_arg)
        if not binary.is_absolute():
            binary = current_cwd / binary
        binary = binary.resolve(strict=True)
        expected_binary = expected_binary.resolve(strict=True)
        binary.relative_to(target_root)
    except (OSError, ValueError):
        refuse("Cargo runner executable path is outside the bound target")
    if (
        binary != expected_binary
        or Path(binary_arg).is_symlink()
        or not binary.is_file()
        or not os.access(binary, os.X_OK)
        or sha256_file(binary) != binding["artifact_sha256"]
    ):
        refuse("Cargo runner executable does not match its bound artifact")
    return binary


def cargo_runtime_test_runner(binding_path_value: str, binary_arg: str) -> None:
    if not sys.platform.startswith("linux") or not hasattr(os, "geteuid"):
        refuse("Cargo runtime-proof runner is supported only on Linux")
    binding_path, binding = runner_binding_path(binding_path_value)
    if binding_path.resolve(strict=True) != binding_path:
        refuse("Cargo runner binding path is not canonical")
    binary = check_runner_binary(binding, binary_arg)
    result_path = binding_path.parent / "result.json"
    result_fd = reserve_private_runner_result(result_path)
    try:
        try:
            child = subprocess.run(
                [str(binary)],
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                close_fds=False,
            )
            output = child.stdout
            output_text = output.decode("utf-8", errors="replace")
            try:
                summaries = parse_test_summaries(output_text)
            except SystemExit:
                summaries = None
            process = (
                {
                    **cargo_library_process_result(
                        child.returncode, summaries, sha256_bytes(output)
                    ),
                    "summaries": summaries,
                }
                if summaries is not None
                else {
                    "classification": "missing_test_summary",
                    "exit_code": child.returncode,
                    "signal": None,
                    "test_summary_count": 0,
                    "passed": 0,
                    "failed": 0,
                    "ignored": 0,
                    "output_sha256": sha256_bytes(output),
                    "summaries": [],
                }
            )
        except OSError:
            output = b""
            process = {
                **cargo_library_process_result(
                    None, [], sha256_bytes(output), spawn_failed=True
                ),
                "summaries": [],
            }
        write_reserved_runner_result(result_fd, binding, process)
        os.fsync(result_fd)
    finally:
        os.close(result_fd)
    if output:
        sys.stdout.buffer.write(output)
        sys.stdout.buffer.flush()
    if process["classification"] != "passed":
        code = process.get("exit_code")
        if isinstance(code, int) and code > 0:
            raise SystemExit(min(code, 255))
        signal = process.get("signal")
        raise SystemExit(min(128 + signal, 255) if isinstance(signal, int) else 1)


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


def verify_test_summary_parser_contract(manifest: dict[str, Any]) -> None:
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

    binding = make_runner_binding(
        "codex-core",
        "1" * 40,
        "2" * 64,
        "codex-rs/core",
        "debug/deps/codex_core-test",
        "3" * 64,
        "4" * 32,
    )
    result_contract = make_runner_result(binding, {})
    if not valid_runner_binding(binding) or set(result_contract) != RUNNER_RESULT_KEYS:
        refuse("Cargo runner binding contract failed")
    if valid_runner_binding({**binding, "artifact_relative_path": "../escape"}):
        refuse("Cargo runner binding contract failed")
    if valid_runner_binding({**binding, "package": "codex-cli"}):
        refuse("Cargo runner binding contract failed")
    if valid_runner_binding({**binding, "cwd": "codex-rs/cli"}):
        refuse("Cargo runner binding contract failed")
    if valid_runner_binding({**binding, "unknown": True}):
        refuse("Cargo runner binding contract failed")

    if manifest["phase"] == "validate":
        for package, source in (
            ("codex-rmcp-client", "codex-rs/rmcp-client/src/lib.rs"),
            ("codex-core", "codex-rs/core/src/lib.rs"),
        ):
            if source in manifest["product_inputs"]:
                refuse("Cargo runner source provenance contract failed")
            if runner_package_source_digest(
                package, source, manifest
            ) != sha256_file(ROOT / source):
                refuse("Cargo runner source provenance contract failed")
        try:
            runner_package_source_digest(
                "codex-core", "codex-rs/core/src/not-admitted.rs", manifest
            )
        except SystemExit as error:
            if error.code != "Cargo runner package source is not admitted":
                refuse("Cargo runner source provenance contract failed")
        else:
            refuse("Cargo runner source provenance contract failed")
    runner_summaries = [{"status": "ok", "passed": 1, "failed": 0, "ignored": 0}]
    runner_process = {
        **cargo_library_process_result(0, runner_summaries, "5" * 64),
        "summaries": runner_summaries,
    }
    runner_result = make_runner_result(binding, runner_process)
    if not valid_runner_result(runner_result, binding) or not runner_process_is_consistent(
        runner_process
    ):
        refuse("Cargo runner result contract failed")
    if valid_runner_result({**runner_result, "extra": True}, binding):
        refuse("Cargo runner result contract failed")
    if valid_runner_result({**runner_result, "cwd": "codex-rs/cli"}, binding):
        refuse("Cargo runner result contract failed")
    failed_runner_process = {
        **cargo_library_process_result(9, runner_summaries, "6" * 64),
        "summaries": runner_summaries,
    }
    if runner_process_is_consistent(
        {**failed_runner_process, "classification": "passed"}
    ):
        refuse("Cargo runner result contract failed")
    signaled_runner_process = {
        **cargo_library_process_result(-9, [], "7" * 64),
        "summaries": [],
    }
    if not runner_process_is_consistent(signaled_runner_process) or runner_process_is_consistent(
        {**signaled_runner_process, "signal": 0}
    ):
        refuse("Cargo runner result contract failed")
    spawn_failed_runner_process = {
        **cargo_library_process_result(None, [], "8" * 64, spawn_failed=True),
        "summaries": [],
    }
    if not runner_process_is_consistent(spawn_failed_runner_process):
        refuse("Cargo runner result contract failed")

    runner_args = [
        "/usr/bin/python3",
        "/repo/.github/scripts/runtime-proof-hosted.py",
        RUNNER_MODE,
        "/tmp/private/binding.json",
    ]
    if cargo_runner_command("/toolchain/bin/cargo", "codex-core", runner_args) != [
        "/toolchain/bin/cargo",
        "--config",
        "target.'x86_64-unknown-linux-gnu'.runner="
        '["/usr/bin/python3","/repo/.github/scripts/runtime-proof-hosted.py",'
        '"--cargo-runtime-proof-runner","/tmp/private/binding.json"]',
        "test",
        "--locked",
        "-p",
        "codex-core",
        "--lib",
        "--message-format=json",
    ]:
        refuse("Cargo runner command contract failed")

    expected_decision = {
        "accepted": False,
        "reason": "runner_result_missing",
        "cargo_exit_code": 17,
        "captured_output_sha256": "9" * 64,
        "child_status": "unknown",
        "runner_output_sha256": None,
    }
    if library_runner_decision(
        17, "9" * 64, "missing", None, binding, runner_summaries, "3" * 64, "3" * 64
    ) != expected_decision:
        refuse("Cargo runner decision contract failed")
    if library_runner_decision(
        17, "9" * 64, "missing", None, binding, runner_summaries, "3" * 64, None
    ) != expected_decision:
        refuse("Cargo runner decision contract failed")
    refused_decision = {
        **expected_decision,
        "reason": "runner_result_refused",
    }
    if library_runner_decision(
        17, "9" * 64, "refused", None, binding, runner_summaries, "3" * 64, "3" * 64
    ) != refused_decision:
        refuse("Cargo runner decision contract failed")
    unsupported_decision = {
        **expected_decision,
        "reason": "cargo_summary_unsupported",
    }
    if library_runner_decision(
        17, "9" * 64, "present", runner_result, binding, None, "3" * 64, "3" * 64
    ) != unsupported_decision:
        refuse("Cargo runner decision contract failed")
    wrong_invocation_result = make_runner_result(
        {**binding, "invocation_id": "f" * 32}, runner_process
    )
    wrong_digest_result = make_runner_result(
        {**binding, "artifact_sha256": "a" * 64}, runner_process
    )
    invalid_result_decision = {
        **expected_decision,
        "reason": "runner_result_invalid",
        "cargo_exit_code": 0,
    }
    for invalid_result in (wrong_invocation_result, wrong_digest_result):
        if library_runner_decision(
            0,
            "9" * 64,
            "present",
            invalid_result,
            binding,
            runner_summaries,
            "3" * 64,
            "3" * 64,
        ) != invalid_result_decision:
            refuse("Cargo runner decision contract failed")
    nonzero_cargo_decision = {
        "accepted": False,
        "reason": "cargo_exit_nonzero",
        "cargo_exit_code": 1,
        "captured_output_sha256": "9" * 64,
        "child_status": "passed",
        "runner_output_sha256": "5" * 64,
    }
    if library_runner_decision(
        1,
        "9" * 64,
        "present",
        runner_result,
        binding,
        runner_summaries,
        "3" * 64,
        "3" * 64,
    ) != nonzero_cargo_decision:
        refuse("Cargo runner decision contract failed")
    successful_decision = {
        **nonzero_cargo_decision,
        "accepted": True,
        "reason": "accepted",
        "cargo_exit_code": 0,
    }
    if library_runner_decision(
        0,
        "9" * 64,
        "present",
        runner_result,
        binding,
        runner_summaries,
        "3" * 64,
        "3" * 64,
    ) != successful_decision:
        refuse("Cargo runner decision contract failed")
    changed_binary_decision = {
        **successful_decision,
        "accepted": False,
        "reason": "bound_binary_changed",
    }
    if library_runner_decision(
        0,
        "9" * 64,
        "present",
        runner_result,
        binding,
        runner_summaries,
        "3" * 64,
        "4" * 64,
    ) != changed_binary_decision:
        refuse("Cargo runner decision contract failed")
    unavailable_binary_decision = {
        **successful_decision,
        "accepted": False,
        "reason": "bound_binary_unavailable",
    }
    if library_runner_decision(
        0,
        "9" * 64,
        "present",
        runner_result,
        binding,
        runner_summaries,
        "3" * 64,
        None,
    ) != unavailable_binary_decision:
        refuse("Cargo runner decision contract failed")
    summary_mismatch_decision = {
        **successful_decision,
        "accepted": False,
        "reason": "cargo_runner_summaries_disagree",
    }
    if library_runner_decision(
        0,
        "9" * 64,
        "present",
        runner_result,
        binding,
        [{"status": "ok", "passed": 2, "failed": 0, "ignored": 0}],
        "3" * 64,
        "3" * 64,
    ) != summary_mismatch_decision:
        refuse("Cargo runner decision contract failed")

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
    build_succeeded = json.dumps({"reason": "build-finished", "success": True})
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
        "compiler_metadata_cause": None,
        "incompatible_crates": [],
        "incompatible_dependency_roots": [],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo no-run compiler failure contract failed")

    incompatible_metadata_error = json.dumps(
        {
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "code": {"code": "E0514"},
                "message": "found crate `serde` compiled by an incompatible version of rustc",
            },
        }
    )
    incompatible_metadata_failure = bounded_library_failure_summary(
        "codex-core",
        f"{incompatible_metadata_error}\n{build_failed}",
        101,
        set(),
        {"serde"},
    )
    if incompatible_metadata_failure != {
        "classification": "compiler_error",
        "cargo_exit_code": 101,
        "build_finished_count": 1,
        "build_succeeded": False,
        "compiler_error_count": 1,
        "compiler_error_codes": ["E0514"],
        "compiler_metadata_cause": "incompatible_rustc_metadata",
        "incompatible_crates": ["serde"],
        "incompatible_dependency_roots": [],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo incompatible metadata diagnostic contract failed")
    dependency_metadata_error = json.dumps(
        {
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "code": {"code": "E0514"},
                "message": (
                    "found crate `private_transitive` compiled by an incompatible version "
                    "of rustc which `serde` depends on"
                ),
            },
        }
    )
    dependency_metadata_failure = bounded_library_failure_summary(
        "codex-core", f"{dependency_metadata_error}\n{build_failed}", 101, set(), {"serde"}
    )
    if dependency_metadata_failure != {
        "classification": "compiler_error",
        "cargo_exit_code": 101,
        "build_finished_count": 1,
        "build_succeeded": False,
        "compiler_error_count": 1,
        "compiler_error_codes": ["E0514"],
        "compiler_metadata_cause": "incompatible_rustc_metadata",
        "incompatible_crates": [],
        "incompatible_dependency_roots": ["serde"],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo incompatible dependency root contract failed")

    unknown_dependency_root_failure = bounded_library_failure_summary(
        "codex-core", f"{dependency_metadata_error}\n{build_failed}", 101, set(), {"unrelated"}
    )
    if unknown_dependency_root_failure != {
        "classification": "compiler_error",
        "cargo_exit_code": 101,
        "build_finished_count": 1,
        "build_succeeded": False,
        "compiler_error_count": 1,
        "compiler_error_codes": ["E0514"],
        "compiler_metadata_cause": None,
        "incompatible_crates": [],
        "incompatible_dependency_roots": [],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo unknown dependency root withholding contract failed")

    malformed_dependency_error = json.dumps(
        {
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "code": {"code": "E0514"},
                "message": (
                    "found crate `private_transitive` compiled by an incompatible version "
                    "of rustc which `serde` depends on extra"
                ),
            },
        }
    )
    malformed_dependency_failure = bounded_library_failure_summary(
        "codex-core", f"{malformed_dependency_error}\n{build_failed}", 101, set(), {"serde"}
    )
    if malformed_dependency_failure != {
        "classification": "compiler_error",
        "cargo_exit_code": 101,
        "build_finished_count": 1,
        "build_succeeded": False,
        "compiler_error_count": 1,
        "compiler_error_codes": ["E0514"],
        "compiler_metadata_cause": None,
        "incompatible_crates": [],
        "incompatible_dependency_roots": [],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo malformed dependency metadata withholding contract failed")

    preterminal_dependency_metadata_success = bounded_library_failure_summary(
        "codex-core", f"{dependency_metadata_error}\n{build_succeeded}", 0, set(), {"serde"}
    )
    if preterminal_dependency_metadata_success != {
        "classification": "build_failure_unclassified",
        "cargo_exit_code": 0,
        "build_finished_count": 1,
        "build_succeeded": True,
        "compiler_error_count": 0,
        "compiler_error_codes": ["E0514"],
        "compiler_metadata_cause": None,
        "incompatible_crates": [],
        "incompatible_dependency_roots": [],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo successful dependency metadata withholding contract failed")
    unknown_metadata_failure = bounded_library_failure_summary(
        "codex-core",
        f"{incompatible_metadata_error.replace('serde', 'not-a-direct-dependency')}\n{build_failed}",
        101,
        set(),
        {"serde"},
    )
    if unknown_metadata_failure != {
        "classification": "compiler_error",
        "cargo_exit_code": 101,
        "build_finished_count": 1,
        "build_succeeded": False,
        "compiler_error_count": 1,
        "compiler_error_codes": ["E0514"],
        "compiler_metadata_cause": None,
        "incompatible_crates": [],
        "incompatible_dependency_roots": [],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo unknown metadata crate withholding contract failed")

    successful_metadata_build = bounded_library_failure_summary(
        "codex-core",
        f"{incompatible_metadata_error}\n{build_succeeded}",
        0,
        set(),
        {"serde"},
    )
    if successful_metadata_build != {
        "classification": "build_failure_unclassified",
        "cargo_exit_code": 0,
        "build_finished_count": 1,
        "build_succeeded": True,
        "compiler_error_count": 0,
        "compiler_error_codes": ["E0514"],
        "compiler_metadata_cause": None,
        "incompatible_crates": [],
        "incompatible_dependency_roots": [],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo successful build metadata withholding contract failed")

    postbuild_error = json.dumps(
        {
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "code": {"code": "E0514"},
                "message": "found crate `serde` compiled by an incompatible version of rustc",
            },
        }
    )
    if bounded_library_failure_summary(
        "codex-core",
        f'{build_succeeded}\n{postbuild_error}\n'
        "test result: FAILED. 0 passed; 1 failed; 0 ignored;",
        101,
        set(),
        {"serde"},
    ) != {
        "classification": "post_build_failure_unclassified",
        "cargo_exit_code": 101,
        "build_finished_count": 1,
        "build_succeeded": True,
        "compiler_error_count": 0,
        "compiler_error_codes": [],
        "compiler_metadata_cause": None,
        "incompatible_crates": [],
        "incompatible_dependency_roots": [],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo no-run post-build failure contract failed")

    if bounded_library_failure_summary(
        "codex-core",
        f'{build_succeeded}\n{dependency_metadata_error}\n'
        "test result: FAILED. 0 passed; 1 failed; 0 ignored;",
        101,
        set(),
        {"serde"},
    ) != {
        "classification": "post_build_failure_unclassified",
        "cargo_exit_code": 101,
        "build_finished_count": 1,
        "build_succeeded": True,
        "compiler_error_count": 0,
        "compiler_error_codes": [],
        "compiler_metadata_cause": None,
        "incompatible_crates": [],
        "incompatible_dependency_roots": [],
        "primary_source_locations": [],
        **no_run_tests,
    }:
        refuse("Cargo no-run post-build dependency withholding contract failed")


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


def direct_rust_crate_allowlist(package: str) -> set[str]:
    manifest_path = EXPECTED_PACKAGE_MANIFESTS.get(package)
    if manifest_path is None:
        return set()
    relative_path = f"codex-rs/{manifest_path}"
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
        return set()
    try:
        manifest = tomllib.loads(run_git("show", f"HEAD:{relative_path}"))
    except (tomllib.TOMLDecodeError, ValueError):
        return set()

    crate_names = set(STANDARD_RUST_CRATES)

    def add_dependencies(table: Any) -> None:
        if not isinstance(table, dict):
            return
        for alias, specification in table.items():
            if not isinstance(alias, str):
                continue
            names = [alias]
            if isinstance(specification, dict):
                package_name = specification.get("package")
                if isinstance(package_name, str):
                    names.append(package_name)
            for name in names:
                normalized = name.replace("-", "_")
                if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", normalized):
                    crate_names.add(normalized)

    def visit(value: Any) -> None:
        if not isinstance(value, dict):
            return
        for key, child in value.items():
            if key in {"dependencies", "dev-dependencies", "build-dependencies"}:
                add_dependencies(child)
            else:
                visit(child)

    visit(manifest)
    return crate_names


def bounded_library_failure_summary(
    package: str,
    output: str,
    exit_code: int,
    admitted_paths: set[str],
    allowed_crates: set[str] | None = None,
) -> dict[str, Any]:
    build_events: list[bool | None] = []
    prebuild_compiler_errors = 0
    compiler_codes: list[str] = []
    source_locations: list[str] = []
    incompatible_crates: list[str] = []
    incompatible_dependency_roots: list[str] = []

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
                if code_value == "E0514" and allowed_crates is not None:
                    diagnostic = message.get("message")
                    direct_match = (
                        INCOMPATIBLE_RUSTC_METADATA_MESSAGE.fullmatch(diagnostic)
                        if isinstance(diagnostic, str)
                        else None
                    )
                    dependency_match = (
                        INCOMPATIBLE_RUSTC_METADATA_DEPENDENCY_MESSAGE.fullmatch(
                            diagnostic
                        )
                        if isinstance(diagnostic, str)
                        else None
                    )
                    if direct_match:
                        crate = direct_match.group(1).replace("-", "_")
                        if (
                            crate in allowed_crates
                            and crate not in incompatible_crates
                            and len(incompatible_crates) < 8
                        ):
                            incompatible_crates.append(crate)
                    elif dependency_match:
                        root = dependency_match.group(2).replace("-", "_")
                        if (
                            root in allowed_crates
                            and root not in incompatible_dependency_roots
                            and len(incompatible_dependency_roots) < 8
                        ):
                            incompatible_dependency_roots.append(root)
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
        incompatible_crates = []
        incompatible_dependency_roots = []
    elif build_succeeded is False and compiler_errors:
        classification = "compiler_error"
    elif build_succeeded is False:
        classification = "build_failure_unclassified"
    elif exit_code:
        classification = "post_build_failure_unclassified"
    else:
        classification = "build_failure_unclassified"
    if not valid_build_event or build_succeeded is not False:
        incompatible_crates = []
        incompatible_dependency_roots = []
    metadata_cause = (
        "incompatible_rustc_metadata"
        if valid_build_event
        and build_succeeded is False
        and (incompatible_crates or incompatible_dependency_roots)
        else None
    )
    return {
        "classification": classification,
        "cargo_exit_code": exit_code,
        "build_finished_count": len(build_events),
        "build_succeeded": build_succeeded,
        "compiler_error_count": compiler_errors,
        "compiler_error_codes": compiler_codes,
        "compiler_metadata_cause": metadata_cause,
        "incompatible_crates": incompatible_crates,
        "incompatible_dependency_roots": incompatible_dependency_roots,
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
        capture_library_failure=True,
    )
    binary = artifact_path(artifact["relative_path"])
    before_hash = sha256_file(binary)
    if before_hash != artifact["sha256"]:
        refuse(f"bound library test executable changed before invocation: {package_name}")
    head = run_git("rev-parse", "HEAD").strip()
    script_sha256 = sha256_file(Path(__file__).resolve(strict=True))
    invocation_id = uuid.uuid4().hex
    binding = make_runner_binding(
        package_name,
        head,
        script_sha256,
        f"codex-rs/{Path(EXPECTED_PACKAGE_MANIFESTS[package_name]).parent.as_posix()}",
        str(artifact["relative_path"]),
        before_hash,
        invocation_id,
    )
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    private_dir = Path(
        tempfile.mkdtemp(prefix="runtime-proof-cargo-runner-", dir=runner_temp)
    )
    binding_path = private_dir / "binding.json"
    result_path = private_dir / "result.json"
    try:
        write_private_runner_binding(binding_path, binding)
        runner_args = [
            sys.executable,
            str(Path(__file__).resolve(strict=True)),
            RUNNER_MODE,
            str(binding_path),
        ]
        test_command = cargo_runner_command(
            str(context["cargo"]), package_name, runner_args
        )
        cargo_result = subprocess.run(
            test_command,
            cwd=CODEX_RS,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            close_fds=False,
        )
        captured_output_sha256 = sha256_bytes(cargo_result.stdout)
        output = cargo_result.stdout.decode("utf-8", errors="replace")
        try:
            cargo_summaries = parse_test_summaries(output)
        except SystemExit:
            cargo_summaries = None
        result_state = "present"
        try:
            runner_value = read_private_runner_record(result_path)
        except SystemExit:
            runner_value = None
            result_state = "refused"
        if runner_value is None and result_state == "present":
            try:
                result_path.lstat()
            except FileNotFoundError:
                result_state = "missing"
            except OSError:
                result_state = "refused"
            else:
                result_state = "refused"
        try:
            after_hash = sha256_file(binary)
        except OSError:
            after_hash = None
        decision = library_runner_decision(
            cargo_result.returncode,
            captured_output_sha256,
            result_state,
            runner_value,
            binding,
            cargo_summaries,
            before_hash,
            after_hash,
        )
        if not decision["accepted"]:
            refuse(
                "required library suite did not pass: "
                f"{package_name}; "
                f"{json.dumps(decision, sort_keys=True, separators=(',', ':'))}"
            )
        process_result = runner_value["process"]
        return {
            "package": package_name,
            "command": [
                "cargo test --locked -p <fixed-package> --lib --no-run --message-format=json",
                "cargo --config <fixed-host-runner> test --locked -p <fixed-package> --lib --message-format=json",
            ],
            "summaries": cargo_summaries,
            "cargo_exit_code": cargo_result.returncode,
            "process": process_result,
            "output_sha256": process_result["output_sha256"],
            "artifact_relative_path": str(artifact["relative_path"]),
            "artifact_sha256": before_hash,
        }
    finally:
        if private_record_directory(private_dir):
            for owned_file in (binding_path, result_path):
                try:
                    owned_file.unlink()
                except FileNotFoundError:
                    pass
            try:
                private_dir.rmdir()
            except OSError:
                refuse("Cargo runner private directory could not be cleaned safely")


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
    capture_library_failure: bool = False,
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
    result = subprocess.run(
        command,
        cwd=CODEX_RS,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT if capture_library_failure else None,
    )
    if result.returncode != 0:
        if capture_library_failure:
            output_hash = sha256_bytes(result.stdout.encode())
            failure = bounded_library_failure_summary(
                package_name,
                result.stdout,
                result.returncode,
                admitted_paths or set(),
                direct_rust_crate_allowlist(package_name),
            )
            refuse(
                f"required locked library build failed: {package_name}; "
                f"output_sha256={output_hash}; "
                f"diagnostic_summary={json.dumps(failure, sort_keys=True, separators=(',', ':'))}"
            )
        refuse(f"required locked compile failed for {package_name}/{target_name}")
    artifacts: list[Path] = []
    build_finished: list[bool | None] = []
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
        if event.get("reason") != "compiler-artifact":
            continue
        if compiler_artifact_matches(
            event, package["id"], expected_target, require_test_profile
        ):
            artifacts.append(Path(event["executable"]))
    if len(artifacts) != 1:
        refuse(f"expected exactly one compiler artifact for {package_name}/{target_name}")
    if capture_library_failure and build_finished != [True]:
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
    return {"cargo": pinned_toolchain_executable("cargo")}


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
        details = ""
        if test_name == EXPECTED_CLI_TESTS[0]:
            stages = []
            spawn_errors = set()
            for line in result.stdout.splitlines():
                match = ROOT_FIXTURE_STAGE.fullmatch(line.strip())
                if match is not None and match.group(1) in ROOT_FIXTURE_STAGE_NAMES:
                    stages.append(match.group(1))
                match = ROOT_FIXTURE_SPAWN_ERROR.search(line)
                if match is not None:
                    spawn_errors.add((match.group(1), match.group(2) or "none"))
            last_stage = stages[-1] if stages else "none"
            details = f"; exit_code={result.returncode}; last_stage={last_stage}"
            if len(spawn_errors) == 1:
                category, errno = next(iter(spawn_errors))
                details += f"; spawn_error_category={category}; spawn_errno={errno}"
            elif spawn_errors:
                details += "; spawn_error_category=ambiguous"
            else:
                details += "; spawn_error_category=not_reported"
        refuse(
            "explicit synthetic root fixture was not exactly one successful executed test: "
            f"{test_name}{details}"
        )
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
    if len(sys.argv) == 4 and sys.argv[1] == RUNNER_MODE:
        cargo_runtime_test_runner(sys.argv[2], sys.argv[3])
        return
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
