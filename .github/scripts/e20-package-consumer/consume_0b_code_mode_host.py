#!/usr/bin/env python3
"""Exercise the shipped code-mode host from immutable 0b package artifacts.

This consumer does not build product binaries. It binds one architecture's
downloaded canonical package to its original successful producer run, verifies
the release manifest and payload hashes, then drives the packaged CLI and
app-server through the pinned Python SDK and a local mock Responses server.
The short-lived strace is private to the runner and records execve argv only;
it is never uploaded or printed.
"""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import traceback
from pathlib import Path
from typing import Any


PRODUCT_SOURCE = "0b9133a342b463511794d16226d25bb91f72d793"
PRODUCT_TREE = "0892c0f040a46c3ac601ec2302c9dec20db8e402"
PRODUCT_BRANCH = "integration/be2e-e926-20261009"
PRODUCT_RUN_ID = "38738124997"
PRODUCT_RUN_NUMBER = 199
PRODUCT_WORKFLOW = ".github/workflows/sedna-first-package.yml"
PRODUCT_WORKFLOW_HOST = "ace29d0b2541d289c5fdaf9349469d8963dc1024"
PRODUCT_WORKFLOW_HOST_TREE = PRODUCT_TREE
PRODUCT_BASE_SHA = "e135df532669849b682dfac712c3f59aded039ba"
PRODUCT_PR_NUMBER = 937
PACKAGE_VERSION = "0.146.0-alpha.8-sedna.10+upstream.8593.ci.199.g0b9133a3"
HARNESS_BRANCH = "validation/w14962-e20-packaged-consumer-20261015"
HARNESS_BASE_SHA = "703389678a1a6c3a9d131245d03d3f1805b9afa1"
ENV_WITNESS_NAME = "CODE_MODE_W17122_SYNTHETIC_WITNESS"
ENV_WITNESS_VALUE = "synthetic-0b-packaged-code-mode"
CALL_ID = "w17122-packaged-code-mode"
ROOT_MODEL = "gpt-6.1-sol"
CHILD_MODEL = "gpt-6-luna"
COMMENTARY_SENTENCE = (
    "The user appreciates consistent, frequent communication during your turn, "
    "and should not be left without a commentary update for more than 60 seconds "
    "during ongoing work."
)
WAIT_SENTENCE = (
    "Avoid performing blocking sleep or wait calls longer than 60 seconds, as "
    "they may prevent you from communicating with the user for their duration."
)
DEFAULT_INSTRUCTION_SENTINEL = (
    "When the user expresses intent to perform new work or fix an existing issue, "
    "persist until the user's intended goal is complete."
)
PROBE_FAILURE_CATEGORIES = {
    "Unexpected package entrypoint.": "unexpected_entrypoint",
    "Packaged code-mode host is missing or not executable.": "host_binary_unavailable",
    "Packaged code-mode turn did not complete.": "turn_not_completed",
    "Packaged code-mode tool output was not returned exactly once.": "tool_output_cardinality",
    "Packaged code-mode command failed.": "command_nonzero",
    "Standard-output witness was not returned.": "stdout_witness_missing",
    "Standard-error redirection witness was not returned.": "stderr_witness_missing",
    "Packaged command ran from an unexpected working path.": "cwd_witness_mismatch",
    "Synthetic environment was not propagated to the packaged command.": "environment_witness_missing",
    "Packaged runtime selected an unexpected ripgrep path.": "ripgrep_path_mismatch",
    "Packaged ripgrep did not execute.": "ripgrep_not_executed",
    "Default-instruction fixture did not capture exactly one server.": "fixture_server_count",
    "Default root model request was not captured.": "root_request_missing",
    "Default child model requests were not captured.": "child_requests_missing",
    "Default child requests did not cover both nested levels.": "nested_child_requests_missing",
    "Root request did not use the bundled model slug.": "root_model_mismatch",
    "Child request did not use the bundled model slug.": "child_model_mismatch",
    "Default model instructions were not captured as text.": "instructions_missing",
    "Root instructions still include the commentary cadence clause.": "root_commentary_clause_present",
    "Root instructions still include the wait cadence clause.": "root_wait_clause_present",
    "Child instructions still include the commentary cadence clause.": "child_commentary_clause_present",
    "Child instructions still include the wait cadence clause.": "child_wait_clause_present",
    "Root default instruction sentinel was not retained.": "root_default_sentinel_missing",
    "Child default instruction sentinel was not retained.": "child_default_sentinel_missing",
}

ARCHITECTURES: dict[str, dict[str, Any]] = {
    "x86_64": {
        "target": "x86_64-unknown-linux-gnu",
        "runner": "ubuntu-24.04",
        "build_job_id": 116440855885,
        "build_job_name": "Build exact Linux package pair (x86_64-unknown-linux-gnu)",
        "consumer_job_id": 116474004846,
        "consumer_job_name": "Consume exact downloaded packages (x86_64-unknown-linux-gnu)",
        "artifact_id": 11981081138,
        "artifact_name": "sedna-first-package-0b9133a3-x86_64-38738124997",
        "artifact_digest": "sha256:c1b1ae782470d459b5e4b0d98085704c00cc8d9ac6ee37028f65a475093308c8",
        "artifact_size": 1198291997,
    },
    "aarch64": {
        "target": "aarch64-unknown-linux-gnu",
        "runner": "ubuntu-24.04-arm",
        "build_job_id": 116440855597,
        "build_job_name": "Build exact Linux package pair (aarch64-unknown-linux-gnu)",
        "consumer_job_id": 116474004782,
        "consumer_job_name": "Consume exact downloaded packages (aarch64-unknown-linux-gnu)",
        "artifact_id": 11978788897,
        "artifact_name": "sedna-first-package-0b9133a3-aarch64-38738124997",
        "artifact_digest": "sha256:8a2f4008083c23ddcc1216a5cc7fac2e6e19b17aa3afa64b3ae2feb648e8bbf8",
        "artifact_size": 1176610838,
    },
}


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def safe_child_diagnostic(stderr: str) -> str:
    """Accept only the child's fixed type/category record; never log raw stderr."""
    byte_count = len(stderr.encode("utf-8", errors="replace"))
    last_line = next((line.strip() for line in reversed(stderr.splitlines()) if line.strip()), "")
    match = re.fullmatch(
        r"code-mode consumer failed: error_type=([A-Za-z_][A-Za-z0-9_]{0,63}); "
        r"category=([a-z0-9_]{1,64})(?:; http_status=([1-5][0-9]{2}))?"
        r"(?:; rpc_code=(-?[0-9]{1,8}))?",
        last_line,
    )
    if match:
        fields = [f"child_error_type={match.group(1)}", f"category={match.group(2)}"]
        if match.group(3):
            fields.append(f"http_status={match.group(3)}")
        if match.group(4):
            fields.append(f"rpc_code={match.group(4)}")
        return "; ".join(fields)
    return f"child_failure_unclassified; stderr_bytes={byte_count}"


def safe_trace_summary(trace_path: Path, entrypoint: str) -> str:
    """Summarize only fixed executable basenames and syscall outcomes."""
    if not trace_path.is_file():
        return "trace=absent; entrypoint_execve=unknown; helper_execve=unknown"
    lines = trace_path.read_text(encoding="utf-8", errors="replace").splitlines()

    def status_for(basename: str) -> str:
        matches = [
            line
            for line in lines
            if re.match(r"^(?:\[pid\s+\d+\]\s+|\d+\s+)?execve\(", line)
            and f'/{basename}"' in line
        ]
        if not matches:
            return "not_seen"
        if any(line.rstrip().endswith("= 0") for line in matches):
            return "started"
        errors = re.findall(r"= -1 ([A-Z0-9]+)", "\n".join(matches))
        if errors:
            allowed = {error for error in errors if re.fullmatch(r"[A-Z][A-Z0-9_]{0,31}", error)}
            if allowed:
                return "failed_" + ",".join(sorted(allowed))
        return "attempted"

    return (
        f"trace=present; entrypoint_execve={status_for(entrypoint)}; "
        f"helper_execve={status_for('codex-code-mode-host')}"
    )


def safe_exception_codes(error: Exception) -> tuple[str | None, str | None]:
    """Expose only numeric HTTP/JSON-RPC codes from the synthetic child."""
    http_status: int | None = None
    rpc_code: int | None = None
    current: BaseException | None = error
    for _ in range(4):
        if current is None:
            break
        value = getattr(current, "status_code", None)
        response = getattr(current, "response", None)
        if value is None and response is not None:
            value = getattr(response, "status_code", None)
        if type(value) is int and 100 <= value <= 599:
            http_status = value
        code = getattr(current, "code", None)
        if type(code) is int and -99_999_999 <= code <= 99_999_999:
            rpc_code = code
        current = current.__cause__ or current.__context__
    return (
        str(http_status) if http_status is not None else None,
        str(rpc_code) if rpc_code is not None else None,
    )


def env(name: str) -> str:
    value = os.environ.get(name, "")
    require(bool(value), f"Required consumer input is missing: {name}.")
    return value


def github_api(path: str) -> dict[str, Any]:
    # Import lazily so the probe child has no token to inherit.
    import urllib.error
    import urllib.request

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
        raise RuntimeError("Exact package provenance lookup failed.") from error
    require(isinstance(payload, dict), "GitHub returned unexpected provenance data.")
    return payload


def verify_consumer_checkout() -> tuple[str, str, str]:
    require(env("GITHUB_EVENT_NAME") == "pull_request", "Only the scoped PR consumer is permitted.")
    event = json.loads(Path(env("GITHUB_EVENT_PATH")).read_text(encoding="utf-8"))
    pull = event.get("pull_request") or {}
    repo = env("GITHUB_REPOSITORY")
    require(repo == "sednalabs/codex", "Unexpected consumer repository.")
    require(
        ((pull.get("head") or {}).get("repo") or {}).get("full_name") == repo,
        "The consumer requires a same-repository pull request.",
    )
    require(
        (pull.get("head") or {}).get("ref") == HARNESS_BRANCH,
        "Unexpected consumer branch.",
    )
    require(
        (pull.get("base") or {}).get("ref") == "main"
        and (pull.get("base") or {}).get("sha") == HARNESS_BASE_SHA,
        "Consumer pull-request base differs from the frozen harness base.",
    )
    head = (pull.get("head") or {}).get("sha")
    require(
        isinstance(head, str) and re.fullmatch(r"[0-9a-f]{40}", head) is not None,
        "Malformed consumer head identity.",
    )
    workspace = Path(env("HARNESS_SOURCE_DIR"))
    actual_head = subprocess.check_output(
        ["git", "-C", str(workspace), "rev-parse", "HEAD"], text=True
    ).strip()
    require(actual_head == head, "Consumer checkout is not the exact PR head.")
    actual_tree = subprocess.check_output(
        ["git", "-C", str(workspace), "rev-parse", "HEAD^{tree}"], text=True
    ).strip()
    return repo, head, actual_tree


def verify_producer(repo: str, architecture: str) -> dict[str, Any]:
    expected = ARCHITECTURES[architecture]
    run = github_api(f"/repos/{repo}/actions/runs/{PRODUCT_RUN_ID}")
    expected_run = {
        "id": int(PRODUCT_RUN_ID),
        "run_number": PRODUCT_RUN_NUMBER,
        "run_attempt": 1,
        "event": "pull_request",
        "path": PRODUCT_WORKFLOW,
        "head_branch": PRODUCT_BRANCH,
        "head_sha": PRODUCT_SOURCE,
        "status": "completed",
        "conclusion": "success",
    }
    for key, value in expected_run.items():
        require(run.get(key) == value, f"Product producer identity mismatch: {key}.")
    require(
        (run.get("head_repository") or {}).get("full_name") == repo,
        "Product producer repository mismatch.",
    )
    linked = run.get("pull_requests")
    require(isinstance(linked, list) and len(linked) == 1, "Producer must identify one PR.")
    pull = linked[0]
    require(pull.get("number") == PRODUCT_PR_NUMBER, "Product producer PR identity mismatch.")
    require(
        (pull.get("base") or {}).get("ref") == "main"
        and (pull.get("head") or {}).get("ref") == PRODUCT_BRANCH,
        "Product producer PR refs mismatch.",
    )

    # Immutable run.head_sha binds the product source. The associated PR head
    # is mutable; the workflow-host commit proves the exact historical base.
    host = github_api(f"/repos/{repo}/commits/{PRODUCT_WORKFLOW_HOST}")
    require(host.get("sha") == PRODUCT_WORKFLOW_HOST, "Workflow-host commit mismatch.")
    require(
        ((host.get("commit") or {}).get("tree") or {}).get("sha")
        == PRODUCT_WORKFLOW_HOST_TREE,
        "Workflow-host tree mismatch.",
    )
    parents = [parent.get("sha") for parent in host.get("parents", [])]
    require(
        parents == [PRODUCT_BASE_SHA, PRODUCT_SOURCE],
        "Workflow-host ordered parents do not bind the exact base and source.",
    )

    jobs = github_api(f"/repos/{repo}/actions/runs/{PRODUCT_RUN_ID}/jobs?per_page=100").get("jobs", [])
    require(isinstance(jobs, list), "Producer job list is malformed.")
    build = next((job for job in jobs if job.get("id") == expected["build_job_id"]), None)
    consumer = next((job for job in jobs if job.get("id") == expected["consumer_job_id"]), None)
    for job, name, role in (
        (build, expected["build_job_name"], "build"),
        (consumer, expected["consumer_job_name"], "consumer"),
    ):
        require(job is not None, f"Exact {architecture} producer {role} job is absent.")
        require(
            job.get("name") == name
            and job.get("run_id") == int(PRODUCT_RUN_ID)
            and job.get("status") == "completed"
            and job.get("conclusion") == "success",
            f"Exact {architecture} producer {role} job did not succeed.",
        )
        require(
            job.get("runner_group_name") == "GitHub Actions"
            and job.get("labels") == [expected["runner"]],
            f"Exact {architecture} producer {role} did not use the standard runner.",
        )

    artifacts = github_api(f"/repos/{repo}/actions/runs/{PRODUCT_RUN_ID}/artifacts?per_page=100").get("artifacts", [])
    artifact = next(
        (item for item in artifacts if item.get("id") == expected["artifact_id"]), None
    )
    require(artifact is not None, "Exact package artifact metadata is absent.")
    require(
        artifact.get("name") == expected["artifact_name"]
        and artifact.get("digest") == expected["artifact_digest"]
        and artifact.get("size_in_bytes") == expected["artifact_size"]
        and artifact.get("expired") is False,
        "Exact package artifact identity, digest, size, or retention mismatch.",
    )
    return expected


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_downloaded_artifact(directory: Path, expected: dict[str, Any]) -> dict[str, Path]:
    target = expected["target"]
    expected_files = {
        f"codex-cli-{target}.tar.gz",
        f"codex-app-server-{target}.tar.gz",
        "codex-responses-api-proxy",
        "release-manifest.json",
        "SHA256SUMS.txt",
        "symbols.tar.gz",
    }
    require(directory.is_dir(), "Downloaded product artifact directory is absent.")
    actual_files = {path.name for path in directory.iterdir() if path.is_file()}
    require(actual_files == expected_files, "Downloaded product artifact has an unexpected file set.")
    require(
        all(path.is_file() and not path.is_symlink() for path in directory.iterdir()),
        "Downloaded product artifact contains a non-regular or symlink entry.",
    )

    manifest = json.loads((directory / "release-manifest.json").read_text(encoding="utf-8"))
    for key, value in {
        "repository": "sednalabs/codex",
        "sourceCommit": PRODUCT_SOURCE,
        "sourceTree": PRODUCT_TREE,
        "workflowHostCommit": PRODUCT_WORKFLOW_HOST,
        "workflowHostTree": PRODUCT_WORKFLOW_HOST_TREE,
        "workflowRunId": PRODUCT_RUN_ID,
        "target": target,
        "packageVersion": PACKAGE_VERSION,
    }.items():
        require(manifest.get(key) == value, f"Downloaded package manifest mismatch: {key}.")
    require(
        manifest.get("workflow")
        == f"https://github.com/sednalabs/codex/actions/runs/{PRODUCT_RUN_ID}",
        "Downloaded package manifest workflow URL mismatch.",
    )

    checksum_lines = (directory / "SHA256SUMS.txt").read_text(encoding="utf-8").splitlines()
    checksums: dict[str, str] = {}
    for line in checksum_lines:
        digest, separator, name = line.partition("  ")
        require(bool(separator) and name not in checksums, "Package checksum file is malformed.")
        checksums[name] = digest
    require(
        set(checksums) == expected_files - {"SHA256SUMS.txt"},
        "Package checksum file has an unexpected member set.",
    )
    for name, digest in checksums.items():
        require(
            re.fullmatch(r"[0-9a-f]{64}", digest) is not None
            and sha256_file(directory / name) == digest,
            "A downloaded package payload failed SHA-256 verification.",
        )
    payload_hashes = {name: digest for name, digest in checksums.items() if name != "release-manifest.json"}
    require(
        manifest.get("artifacts") == payload_hashes,
        "Release-manifest payload digests disagree with SHA256SUMS.txt.",
    )
    return {
        "cli": directory / f"codex-cli-{target}.tar.gz",
        "app_server": directory / f"codex-app-server-{target}.tar.gz",
        "symbols": directory / "symbols.tar.gz",
    }


def run_probe(
    *,
    target: str,
    entrypoint: str,
    cli_archive: Path,
    app_server_archive: Path,
    result_file: Path,
) -> None:
    from app_server_harness import (
        MockResponsesServer,
        ev_completed,
        ev_response_created,
        sse,
    )
    from fixtures import SmokePackage
    from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

    require(entrypoint in {"codex", "codex-app-server"}, "Unexpected package entrypoint.")
    os.environ[ENV_WITNESS_NAME] = ENV_WITNESS_VALUE
    with tempfile.TemporaryDirectory(prefix="w17122-code-mode-") as temp_directory:
        package_directory = Path(temp_directory)
        package = SmokePackage.from_archives(
            package_directory,
            target,
            cli_archive,
            app_server_archive,
            "gzip",
        )
        if entrypoint == "codex":
            executable = package.cli
            package_root = package.cli_root
            package_path_dir = package.cli_path_dir
            config = CodexConfig(
                codex_bin=str(executable),
                cwd=str(package.directory),
                env=package.environment,
            )
        else:
            executable = package.app_server
            package_root = package.app_server_root
            package_path_dir = package.app_server_path_dir
            config = CodexConfig(
                launch_args_override=(str(executable),),
                cwd=str(package.directory),
                env=package.environment,
            )

        host_path = package_root / "codex-resources" / "codex-code-mode-host"
        require(host_path.is_file() and os.access(host_path, os.X_OK), "Packaged code-mode host is missing or not executable.")
        host_path = host_path.resolve(strict=True)
        entrypoint_path = executable.resolve(strict=True)
        host_digest = sha256_file(host_path)

        config_path = Path(package.environment["CODEX_HOME"]) / "config.toml"
        original_config = config_path.read_text(encoding="utf-8")
        with MockResponsesServer() as responses_server:
            config_path.write_text(
                f'''model = "package-smoke"
model_provider = "package_smoke"

{original_config}

[model_providers.package_smoke]
name = "package smoke"
base_url = "{responses_server.url}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
''',
                encoding="utf-8",
            )
            command = (
                "printf 'W17122_STDOUT=stdout-redirection-ok\\n'; "
                "printf 'W17122_STDERR=stderr-redirection-ok\\n' >&2; "
                "printf 'W17122_CWD=%s\\n' \"$PWD\"; "
                f"printf 'W17122_ENV=%s\\n' \"${ENV_WITNESS_NAME}\"; "
                "printf 'W17122_RG=%s\\n' \"$(command -v rg)\"; "
                "rg --version"
            )
            exec_arguments = {"cmd": command, "login": False, "yield_time_ms": 10_000}
            responses_server.enqueue_sse(
                sse(
                    [
                        ev_response_created("code-mode"),
                        {
                            "type": "response.output_item.done",
                            "item": {
                                "type": "custom_tool_call",
                                "call_id": CALL_ID,
                                "name": "exec",
                                "input": (
                                    "text(JSON.stringify(await tools.exec_command("
                                    f"{json.dumps(exec_arguments)})))"
                                ),
                            },
                        },
                        ev_completed("code-mode"),
                    ]
                )
            )
            responses_server.enqueue_assistant_message("Done", response_id="code-mode-done")
            with Codex(config=config) as client:
                turn = client.thread_start(
                    ephemeral=True,
                    approval_mode=ApprovalMode.deny_all,
                    sandbox=Sandbox.workspace_write,
                ).run("run packaged code-mode host smoke")
                require(turn.final_response == "Done", "Packaged code-mode turn did not complete.")

            outputs = [
                item["output"]
                for request in responses_server.requests()
                if request.path == "/v1/responses"
                for item in request.input()
                if item.get("type") == "custom_tool_call_output" and item.get("call_id") == CALL_ID
            ]
            require(len(outputs) == 1, "Packaged code-mode tool output was not returned exactly once.")
            output = outputs[0]
            if not isinstance(output, str):
                output = next(
                    item["text"]
                    for item in output
                    if isinstance(item.get("text"), str)
                    and item["text"].startswith("{")
                )
            execution = json.loads(output)
            execution_output = execution.get("output")
            require(execution.get("exit_code") == 0 and isinstance(execution_output, str), "Packaged code-mode command failed.")

            expected_cwd = str(package.directory.resolve())
            expected_rg = str((package_path_dir / "rg").resolve())
            require("W17122_STDOUT=stdout-redirection-ok" in execution_output, "Standard-output witness was not returned.")
            require("W17122_STDERR=stderr-redirection-ok" in execution_output, "Standard-error redirection witness was not returned.")
            require(f"W17122_CWD={expected_cwd}" in execution_output, "Packaged command ran from an unexpected working path.")
            require(f"W17122_ENV={ENV_WITNESS_VALUE}" in execution_output, "Synthetic environment was not propagated to the packaged command.")
            require(f"W17122_RG={expected_rg}" in execution_output, "Packaged runtime selected an unexpected ripgrep path.")
            require("ripgrep" in execution_output.lower(), "Packaged ripgrep did not execute.")

            result_file.write_text(
                json.dumps(
                    {
                        "entrypoint": entrypoint,
                        "entrypoint_path": str(entrypoint_path),
                        "host_path": str(host_path),
                        "host_sha256": host_digest,
                        "argv0": str(host_path),
                        "probe_ok": True,
                    },
                    sort_keys=True,
                ),
                encoding="utf-8",
            )
            config_path.write_text(original_config, encoding="utf-8")


def verify_trace(trace_path: Path, expected_entrypoint: str, expected_host: str) -> None:
    require(trace_path.is_file(), "Code-mode process trace was not written.")
    found = False
    # The closing bracket immediately after argv[0] proves there are no extra
    # arguments, rather than merely checking that the expected argv[0] occurs.
    expected_call = f'execve("{expected_host}", ["{expected_host}"], '
    entrypoint_found = False
    for line in trace_path.read_text(encoding="utf-8", errors="replace").splitlines():
        if expected_call in line and line.rstrip().endswith("= 0"):
            found = True
        if f'execve("{expected_entrypoint}", ["{expected_entrypoint}"' in line and line.rstrip().endswith("= 0"):
            entrypoint_found = True
    require(found, "The packaged host was not selected with the expected executable argv.")
    require(entrypoint_found, "The expected packaged entrypoint was not executed with its real argv.")


def run_default_instruction_consumer(
    *,
    source_directory: Path,
    target: str,
    cli_archive: Path,
    app_server_archive: Path,
) -> dict[str, Any]:
    """Run the pinned nested-spawn smoke test with bundled root and child models."""
    smoke_directory = source_directory / "scripts" / "codex_package" / "smoke_tests"
    test_path = (
        smoke_directory
        / "first_binary"
        / "test_agent_control_model_acceptance.py"
    )
    require(test_path.is_file(), "Pinned nested-spawn smoke test is absent.")

    import_paths = (
        smoke_directory,
        source_directory / "sdk" / "python" / "src",
        source_directory / "sdk" / "python" / "tests",
    )
    for import_path in reversed(import_paths):
        sys.path.insert(0, str(import_path))

    spec = importlib.util.spec_from_file_location(
        "codex_0b_nested_spawn_acceptance", test_path
    )
    require(spec is not None and spec.loader is not None, "Pinned smoke test could not be loaded.")
    test_module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(test_module)

    captured_servers: list[Any] = []
    original_server = test_module.MockResponsesServer

    class CapturedMockResponsesServer(original_server):
        def __enter__(self):
            server = super().__enter__()
            captured_servers.append(self)
            return server

    original_mock_config = test_module._mock_config

    def use_bundled_root_model(home: Path, server: Any, *, agent_tools: bool = False) -> None:
        original_mock_config(home, server, agent_tools=agent_tools)
        config_path = home / "config.toml"
        config = config_path.read_text(encoding="utf-8")
        placeholder = 'model = "package-smoke"'
        require(
            config.count(placeholder) == 1,
            "Nested-spawn fixture root model input was not unique.",
        )
        config_path.write_text(
            config.replace(placeholder, f'model = "{ROOT_MODEL}"', 1),
            encoding="utf-8",
        )

    test_module.MockResponsesServer = CapturedMockResponsesServer
    test_module._mock_config = use_bundled_root_model

    from fixtures import SmokePackage

    with tempfile.TemporaryDirectory(prefix="w17122-default-instructions-") as temp:
        package = SmokePackage.from_archives(
            Path(temp), target, cli_archive, app_server_archive, "gzip"
        )
        try:
            test_module.test_packaged_model_nested_spawn_recovery_and_list_after_resume(
                package
            )
        except AssertionError as error:
            frames = traceback.extract_tb(error.__traceback__)
            test_line = next(
                (frame.lineno for frame in reversed(frames) if frame.filename == str(test_path)),
                0,
            )
            raise RuntimeError(
                f"Default-instruction nested-spawn assertion failed at source line {test_line}."
            ) from error

    require(
        len(captured_servers) == 1,
        "Default-instruction fixture did not capture exactly one server.",
    )
    requests = [
        request
        for request in captured_servers[0].requests()
        if request.path == "/v1/responses"
    ]
    root_requests = [
        request
        for request in requests
        if request.header("x-codex-parent-thread-id") is None
    ]
    child_requests = [
        request
        for request in requests
        if request.header("x-codex-parent-thread-id") is not None
    ]
    require(bool(root_requests), "Default root model request was not captured.")
    require(bool(child_requests), "Default child model requests were not captured.")
    child_parent_ids = {
        request.header("x-codex-parent-thread-id") for request in child_requests
    }
    require(
        len(child_parent_ids) >= 2,
        "Default child requests did not cover both nested levels.",
    )

    for request in root_requests:
        body = request.body_json()
        require(
            body.get("model") == ROOT_MODEL,
            "Root request did not use the bundled model slug.",
        )
        instructions = body.get("instructions")
        require(
            isinstance(instructions, str),
            "Default model instructions were not captured as text.",
        )
        require(
            COMMENTARY_SENTENCE not in instructions,
            "Root instructions still include the commentary cadence clause.",
        )
        require(
            WAIT_SENTENCE not in instructions,
            "Root instructions still include the wait cadence clause.",
        )
        require(
            DEFAULT_INSTRUCTION_SENTINEL in instructions,
            "Root default instruction sentinel was not retained.",
        )

    for request in child_requests:
        body = request.body_json()
        require(
            body.get("model") == CHILD_MODEL,
            "Child request did not use the bundled model slug.",
        )
        instructions = body.get("instructions")
        require(
            isinstance(instructions, str),
            "Default model instructions were not captured as text.",
        )
        require(
            COMMENTARY_SENTENCE not in instructions,
            "Child instructions still include the commentary cadence clause.",
        )
        require(
            WAIT_SENTENCE not in instructions,
            "Child instructions still include the wait cadence clause.",
        )
        require(
            DEFAULT_INSTRUCTION_SENTINEL in instructions,
            "Child default instruction sentinel was not retained.",
        )

    return {
        "test": "test_packaged_model_nested_spawn_recovery_and_list_after_resume",
        "root_model": ROOT_MODEL,
        "child_model": CHILD_MODEL,
        "root_requests_checked": len(root_requests),
        "child_requests_checked": len(child_requests),
        "nested_parent_ids_checked": len(child_parent_ids),
        "cadence_clauses_absent": True,
        "unrelated_default_instruction_retained": True,
    }


def main() -> None:
    if len(sys.argv) == 2 and sys.argv[1] == "--probe":
        run_probe(
            target=env("PRODUCT_TARGET"),
            entrypoint=env("CODE_MODE_ENTRYPOINT"),
            cli_archive=Path(env("CLI_ARCHIVE")),
            app_server_archive=Path(env("APP_SERVER_ARCHIVE")),
            result_file=Path(env("PROBE_RESULT_FILE")),
        )
        return

    if len(sys.argv) == 2 and sys.argv[1] == "--default-instructions":
        architecture = env("PRODUCT_ARCHITECTURE")
        require(architecture in ARCHITECTURES, "Unsupported package architecture.")
        expected = ARCHITECTURES[architecture]
        repo, harness_sha, harness_tree = verify_consumer_checkout()
        verify_producer(repo, architecture)
        artifacts = verify_downloaded_artifact(
            Path(env("PRODUCT_ARTIFACT_DIR")), expected
        )
        os.environ.pop("GH_TOKEN", None)
        source_directory = Path(env("PRODUCT_SOURCE_DIR"))
        require(
            subprocess.check_output(
                ["git", "-C", str(source_directory), "rev-parse", "HEAD"],
                text=True,
            ).strip()
            == PRODUCT_SOURCE,
            "Pinned product source checkout does not match the package source SHA.",
        )
        require(
            subprocess.check_output(
                ["git", "-C", str(source_directory), "rev-parse", "HEAD^{tree}"],
                text=True,
            ).strip()
            == PRODUCT_TREE,
            "Pinned product source checkout does not match the package source tree.",
        )
        result = run_default_instruction_consumer(
            source_directory=source_directory,
            target=expected["target"],
            cli_archive=artifacts["cli"],
            app_server_archive=artifacts["app_server"],
        )
        print(
            json.dumps(
                {
                    "architecture": architecture,
                    "harness_sha": harness_sha,
                    "harness_tree": harness_tree,
                    "product_source": PRODUCT_SOURCE,
                    "product_tree": PRODUCT_TREE,
                    "producer_run": PRODUCT_RUN_ID,
                    "artifact_id": expected["artifact_id"],
                    "artifact_digest": expected["artifact_digest"],
                    **result,
                },
                sort_keys=True,
            )
        )
        return

    architecture = env("PRODUCT_ARCHITECTURE")
    require(architecture in ARCHITECTURES, "Unsupported package architecture.")
    expected = ARCHITECTURES[architecture]
    repo, harness_sha, harness_tree = verify_consumer_checkout()
    verify_producer(repo, architecture)
    artifact_files = verify_downloaded_artifact(Path(env("PRODUCT_ARTIFACT_DIR")), expected)

    # The provenance lookup token is no longer needed. Do not pass it to the
    # packaged process, its child, the trace, or the model-facing tool output.
    os.environ.pop("GH_TOKEN", None)
    os.environ[ENV_WITNESS_NAME] = ENV_WITNESS_VALUE

    source_directory = Path(env("PRODUCT_SOURCE_DIR"))
    smoke_directory = source_directory / "scripts" / "codex_package" / "smoke_tests"
    symbols = artifact_files["symbols"]
    require(smoke_directory.is_dir() and symbols.is_file(), "Pinned package smoke inputs are absent.")
    require(
        subprocess.check_output(["git", "-C", str(source_directory), "rev-parse", "HEAD"], text=True).strip()
        == PRODUCT_SOURCE,
        "Pinned product source checkout does not match the package source SHA.",
    )
    require(
        subprocess.check_output(["git", "-C", str(source_directory), "rev-parse", "HEAD^{tree}"], text=True).strip()
        == PRODUCT_TREE,
        "Pinned product source checkout does not match the package source tree.",
    )

    with tempfile.TemporaryDirectory(prefix="w17122-code-mode-runner-") as temp_directory:
        temp = Path(temp_directory)
        for entrypoint in ("codex", "codex-app-server"):
            trace_path = temp / f"{entrypoint}.execve"
            result_path = temp / f"{entrypoint}.json"
            probe_env = os.environ.copy()
            probe_env.update(
                {
                    "PRODUCT_TARGET": expected["target"],
                    "CODE_MODE_ENTRYPOINT": entrypoint,
                    "CLI_ARCHIVE": str(artifact_files["cli"]),
                    "APP_SERVER_ARCHIVE": str(artifact_files["app_server"]),
                    "PROBE_RESULT_FILE": str(result_path),
                    "PYTHONPATH": os.pathsep.join(
                        [
                            str(smoke_directory),
                            str(source_directory / "sdk" / "python" / "src"),
                            str(source_directory / "sdk" / "python" / "tests"),
                        ]
                    ),
                }
            )
            command = [
                "strace",
                "-f",
                "-e",
                "trace=execve",
                "-o",
                str(trace_path),
                sys.executable,
                str(Path(__file__).resolve()),
                "--probe",
            ]
            completed = subprocess.run(
                command,
                cwd=source_directory,
                env=probe_env,
                capture_output=True,
                text=True,
                timeout=180,
                check=False,
            )
            require(
                completed.returncode == 0,
                f"Packaged SDK code-mode probe failed "
                f"(entrypoint={entrypoint}; child_exit={completed.returncode}; "
                f"{safe_trace_summary(trace_path, entrypoint)}; "
                f"{safe_child_diagnostic(completed.stderr)}).",
            )
            require(result_path.is_file(), f"Packaged {entrypoint} probe result is absent.")
            result = json.loads(result_path.read_text(encoding="utf-8"))
            require(
                result.get("entrypoint") == entrypoint and result.get("probe_ok") is True,
                f"Packaged {entrypoint} probe identity mismatch.",
            )
            verify_trace(trace_path, result["entrypoint_path"], result["host_path"])
            print(
                json.dumps(
                    {
                        "architecture": architecture,
                        "harness_sha": harness_sha,
                        "harness_tree": harness_tree,
                        "product_source": PRODUCT_SOURCE,
                        "product_tree": PRODUCT_TREE,
                        "workflow_host": PRODUCT_WORKFLOW_HOST,
                        "workflow_host_tree": PRODUCT_WORKFLOW_HOST_TREE,
                        "producer_run": PRODUCT_RUN_ID,
                        "artifact_id": expected["artifact_id"],
                        "artifact_digest": expected["artifact_digest"],
                        "entrypoint": entrypoint,
                        "entrypoint_execve": "passed",
                        "code_mode_host_sha256": json.loads(result_path.read_text(encoding="utf-8"))["host_sha256"],
                        "code_mode_host_execve_argv": "exact packaged resource path, argv0 only",
                        "synthetic_env_cwd_stdout_stderr_rg": "passed",
                    },
                    sort_keys=True,
                )
            )


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Never log arbitrary exception text, which can contain request data,
        # credentials, environment values, or runner paths.
        error_type = type(error).__name__
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,63}", error_type):
            error_type = "Exception"
        category = "unclassified_exception"
        fixture_line = None
        if isinstance(error, RuntimeError):
            category = PROBE_FAILURE_CATEGORIES.get(str(error), "unclassified_runtime_error")
            match = re.fullmatch(
                r"Default-instruction nested-spawn assertion failed at source line ([0-9]{1,5})\.",
                str(error),
            )
            if match:
                category = "nested_spawn_fixture_assertion"
                fixture_line = match.group(1)
        http_status, rpc_code = safe_exception_codes(error)
        suffix = ""
        if http_status is not None:
            suffix += f"; http_status={http_status}"
        if rpc_code is not None:
            suffix += f"; rpc_code={rpc_code}"
        if fixture_line is not None:
            suffix += f"; fixture_line={fixture_line}"
        print(
            f"code-mode consumer failed: error_type={error_type}; category={category}{suffix}",
            file=sys.stderr,
        )
        raise SystemExit(1)
