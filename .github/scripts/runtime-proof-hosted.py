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
    if not re.fullmatch(r"[0-9a-f]{40}", workflow_sha):
        refuse("workflow source SHA is unavailable or invalid")
    return head, workflow_sha


def verify_inputs(manifest: dict[str, Any]) -> tuple[str, str]:
    verify_clean_checkout()
    head, workflow_sha = verify_commit_binding(manifest)
    verify_hash_map(manifest["product_inputs"], "product source")
    verify_hash_map(manifest["local_helpers"], "host helper/toolchain")
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
    return [
        {
            "status": match.group(1),
            "passed": int(match.group(2)),
            "failed": int(match.group(3)),
            "ignored": int(match.group(4)),
        }
        for match in TEST_SUMMARY.finditer(output)
    ]


def run_library_suite(package: str) -> dict[str, Any]:
    command = ["cargo", "test", "--locked", "-p", package, "--lib"]
    result = subprocess.run(
        command,
        cwd=CODEX_RS,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    summaries = parse_test_summaries(result.stdout)
    if result.returncode != 0 or not summaries or any(
        item["status"] != "ok" or item["passed"] == 0 or item["failed"]
        for item in summaries
    ):
        refuse(f"required library suite failed: {package}; output_sha256={sha256_bytes(result.stdout.encode())}")
    return {
        "package": package,
        "command": command,
        "summaries": summaries,
        "output_sha256": sha256_bytes(result.stdout.encode()),
    }


def cargo_artifact(command: list[str], package_name: str, target_name: str, kind: str) -> Path:
    result = subprocess.run(
        command,
        cwd=CODEX_RS,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
    )
    if result.returncode != 0:
        refuse(f"required locked compile failed for {package_name}/{target_name}")
    artifacts: list[Path] = []
    for line in result.stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("reason") != "compiler-artifact":
            continue
        target = event.get("target", {})
        package_id = event.get("package_id", "")
        package_component = package_id.rsplit("#", 1)[-1].split("@", 1)[0]
        if (
            package_component == package_name
            and target.get("name") == target_name
            and kind in target.get("kind", [])
            and event.get("executable")
        ):
            artifacts.append(Path(event["executable"]))
    if len(artifacts) != 1:
        refuse(f"expected exactly one compiler artifact for {package_name}/{target_name}")
    artifact = artifacts[0]
    if not artifact.is_absolute():
        artifact = CODEX_RS / artifact
    target_root = Path(os.environ.get("CARGO_TARGET_DIR", ""))
    if not target_root.is_absolute() or not target_root.is_dir():
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
    return relative


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
        line.split(":", 1)[0]
        for line in result.stdout.splitlines()
        if line.endswith(": test")
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
    if result.returncode != 0 or len(summaries) != 1:
        refuse(f"explicit synthetic root fixture did not produce one terminal test result: {test_name}")
    summary = summaries[0]
    if summary != {"status": "ok", "passed": 1, "failed": 0, "ignored": 0}:
        refuse(f"explicit synthetic root fixture was not exactly one successful executed test: {test_name}")
    if sha256_file(binary) != expected_binary_sha256:
        refuse(f"root fixture binary changed during invocation: {test_name}")
    return {
        "test": test_name,
        "status": "passed",
        "counts": summary,
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

    library_results = [run_library_suite(package) for package in manifest["linux_library_test_packages"]]
    cli_relative = cargo_artifact(
        ["cargo", "build", "--locked", "-p", "codex-cli", "--bin", "codex", "--message-format=json"],
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
        "codex-cli",
        "runtime_execution_proof",
        "test",
    )
    mcp_test_relative = cargo_artifact(
        ["cargo", "test", "--locked", "-p", "codex-mcp", "--lib", "--no-run", "--message-format=json"],
        "codex-mcp",
        "codex_mcp",
        "lib",
    )
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
    mcp_test_sha256 = sha256_file(mcp_binary)
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
