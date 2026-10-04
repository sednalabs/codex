"""Exact-artifact checks shared by the additional hosted package smoke cases.

This module deliberately consumes the existing ``SmokePackage`` fixture.  It
does not build Rust, download artifacts, select a latest run, or use a local
profile.  The workflow must resolve and write CODEX_PACKAGE_EVIDENCE from the
GitHub run/artifact APIs before invoking pytest.
"""

import hashlib
import json
import os
import platform
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from app_server_harness import MockResponsesServer, ev_completed, ev_response_created, sse
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage


SHA256 = re.compile(r"[0-9a-f]{64}\Z")
SHA = re.compile(r"[0-9a-f]{40}\Z")
UPSTREAM_BASE_SHA = "76a6e55d5ac69e3ba6b0481f8f3b94256f46dfa7"
UPSTREAM_BASE_TREE = "01cd0e4b1c5aa80b5befbc26c37df47f93129bf8"
TARGET_ARCH = {
    "x86_64-unknown-linux-gnu": "x86_64",
    "aarch64-unknown-linux-gnu": "aarch64",
    "x86_64-unknown-linux-musl": "x86_64",
    "aarch64-unknown-linux-musl": "aarch64",
}


def _required_string(value: Any, name: str) -> str:
    assert isinstance(value, str) and value.strip(), f"missing {name}"
    return value


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


@dataclass(frozen=True)
class ArtifactEvidence:
    artifact_id: int
    sha256: str
    target_sha: str


@dataclass(frozen=True)
class PackageEvidence:
    target_sha: str
    base_ref: str
    base_sha: str
    upstream_sha: str
    upstream_tree: str
    workflow_host_sha: str
    run_id: int
    target: str
    cli: ArtifactEvidence
    app_server: ArtifactEvidence


@dataclass(frozen=True)
class ConsumerContext:
    repository: str
    workflow_path: str
    event: str
    workflow_host_sha: str
    run_id: int
    run_attempt: int
    ref: str
    branch: str
    workflow_ref: str
    target_sha: str
    base_sha: str
    target: str
    architecture: str
    runner_label: str


def read_consumer_context(
    path: Path,
    *,
    target_sha: str,
    base_sha: str,
    target: str,
    producer_run_id: int | None = None,
) -> ConsumerContext:
    """Bind the current Actions API receipt to the untouched consumer env."""
    assert os.environ.get("GITHUB_ACTIONS") == "true", "hosted Actions required"
    assert path.is_absolute() and path.is_file() and not path.is_symlink()
    runner_temp = Path(_required_string(os.environ.get("RUNNER_TEMP"), "RUNNER_TEMP"))
    assert path.resolve(strict=True).is_relative_to(runner_temp.resolve(strict=True))
    data = json.loads(path.read_text(encoding="utf-8"))
    assert isinstance(data, dict) and data.get("schema_version") == "sedna-first-binary-consumer-api-v1"
    repository = "sednalabs/codex"
    workflow_path = ".github/workflows/sedna-branch-build.yml"
    assert data.get("repository") == repository == os.environ.get("GITHUB_REPOSITORY")
    assert data.get("workflow_path") == workflow_path
    assert data.get("event") == "workflow_dispatch" == os.environ.get("GITHUB_EVENT_NAME")
    host_sha = _required_string(data.get("workflow_host_sha"), "consumer.workflow_host_sha")
    assert SHA.fullmatch(host_sha) and host_sha == os.environ.get("GITHUB_SHA")
    run_id = data.get("run_id")
    attempt = data.get("run_attempt")
    assert type(run_id) is int and run_id > 0 and str(run_id) == os.environ.get("GITHUB_RUN_ID")
    assert type(attempt) is int and attempt > 0 and str(attempt) == os.environ.get("GITHUB_RUN_ATTEMPT")
    if producer_run_id is not None:
        assert run_id != producer_run_id, "cross-run consumer reused producer run identity"
    ref = _required_string(data.get("ref"), "consumer.ref")
    branch = _required_string(data.get("branch"), "consumer.branch")
    workflow_ref = _required_string(data.get("workflow_ref"), "consumer.workflow_ref")
    assert ref == os.environ.get("GITHUB_REF")
    assert branch == os.environ.get("GITHUB_REF_NAME")
    assert workflow_ref == os.environ.get("GITHUB_WORKFLOW_REF")
    assert workflow_ref == f"{repository}/{workflow_path}@{ref}"
    assert data.get("product_sha") == target_sha and SHA.fullmatch(target_sha)
    assert data.get("comparison_base_sha") == base_sha and SHA.fullmatch(base_sha)
    assert data.get("target") == target and target in TARGET_ARCH
    architecture = TARGET_ARCH[target]
    runner_label = {
        "x86_64": "ubuntu-24.04", "aarch64": "ubuntu-24.04-arm"
    }[architecture]
    assert data.get("architecture") == architecture
    assert data.get("runner_label") == runner_label
    assert platform.system() == "Linux" and platform.machine().lower() == architecture
    assert os.environ.get("RUNNER_OS") == "Linux"
    assert os.environ.get("RUNNER_ARCH") == {"x86_64": "X64", "aarch64": "ARM64"}[architecture]
    return ConsumerContext(
        repository, workflow_path, "workflow_dispatch", host_sha, run_id, attempt,
        ref, branch, workflow_ref, target_sha, base_sha, target, architecture, runner_label,
    )


def read_evidence(
    package: SmokePackage,
    cli_archive: Path,
    app_archive: Path,
    *,
    expected_producer_identity: tuple[str, int] | None = None,
    consumer_context: ConsumerContext | None = None,
) -> PackageEvidence:
    """Reject missing or cross-source H/T/B/run/artifact/target/digest joins."""
    evidence_path = Path(_required_string(os.environ.get("CODEX_PACKAGE_EVIDENCE"), "CODEX_PACKAGE_EVIDENCE"))
    data = json.loads(evidence_path.read_text(encoding="utf-8"))
    assert isinstance(data, dict)
    target_sha = _required_string(data.get("target_sha"), "target_sha")
    base_ref = _required_string(data.get("base_ref"), "base_ref")
    base_sha = _required_string(data.get("base_sha"), "base_sha")
    upstream_sha = _required_string(data.get("upstream_sha"), "upstream_sha")
    upstream_tree = _required_string(data.get("upstream_tree"), "upstream_tree")
    host_sha = _required_string(data.get("workflow_host_sha"), "workflow_host_sha")
    assert all(SHA.fullmatch(value) for value in (target_sha, base_sha, upstream_sha, upstream_tree, host_sha))
    assert upstream_sha == UPSTREAM_BASE_SHA and upstream_tree == UPSTREAM_BASE_TREE
    run_id = data.get("run_id")
    assert type(run_id) is int and run_id > 0, "missing run_id"
    target = _required_string(data.get("target"), "target")
    assert target == package.target and target in TARGET_ARCH
    assert platform.system() == "Linux"
    assert platform.machine().lower() == TARGET_ARCH[target]
    assert os.environ.get("GITHUB_ACTIONS") == "true", "hosted Actions execution required"
    if expected_producer_identity is None:
        assert os.environ.get("GITHUB_RUN_ID") == str(run_id), "same-run producer mismatch"
        assert os.environ.get("GITHUB_SHA") == host_sha, "same-run host mismatch"
        if consumer_context is not None:
            assert consumer_context.run_id == run_id
            assert consumer_context.workflow_host_sha == host_sha
    else:
        producer_host, producer_run = expected_producer_identity
        assert SHA.fullmatch(producer_host) and type(producer_run) is int and producer_run > 0
        assert host_sha == producer_host and run_id == producer_run
        assert consumer_context is not None, "cross-run consumer context required"
        assert consumer_context.run_id != run_id
        assert str(consumer_context.run_id) == os.environ.get("GITHUB_RUN_ID")
        assert consumer_context.workflow_host_sha == os.environ.get("GITHUB_SHA")
        assert str(consumer_context.run_attempt) == os.environ.get("GITHUB_RUN_ATTEMPT")
        assert consumer_context.ref == os.environ.get("GITHUB_REF")
        assert consumer_context.workflow_ref == os.environ.get("GITHUB_WORKFLOW_REF")
        assert consumer_context.target_sha == target_sha
        assert consumer_context.base_sha == base_sha
        assert consumer_context.target == target
    assert os.environ.get("RUNNER_OS") == "Linux"
    assert os.environ.get("RUNNER_ARCH") == {"x86_64": "X64", "aarch64": "ARM64"}[TARGET_ARCH[target]]
    artifacts = data.get("artifacts")
    assert isinstance(artifacts, dict)

    def artifact(name: str, archive: Path) -> ArtifactEvidence:
        record = artifacts.get(name)
        assert isinstance(record, dict), f"missing {name} artifact evidence"
        artifact_id = record.get("id")
        assert type(artifact_id) is int and artifact_id > 0
        digest = _required_string(record.get("sha256"), f"{name}.sha256")
        assert SHA256.fullmatch(digest)
        artifact_target = _required_string(record.get("target_sha"), f"{name}.target_sha")
        assert artifact_target == target_sha, f"{name} is from another source"
        assert record.get("base_sha") == base_sha, f"{name} base mismatch"
        assert record.get("workflow_host_sha") == host_sha, f"{name} host mismatch"
        assert record.get("run_id") == run_id, f"{name} run mismatch"
        assert record.get("target") == target, f"{name} architecture mismatch"
        assert archive.is_file() and sha256_file(archive) == digest, f"{name} digest mismatch"
        return ArtifactEvidence(artifact_id, digest, artifact_target)

    return PackageEvidence(
        target_sha, base_ref, base_sha, upstream_sha, upstream_tree, host_sha, run_id, target,
        artifact("cli", cli_archive), artifact("app_server", app_archive),
    )


def validate_layout(package: SmokePackage) -> dict[str, dict[str, Any]]:
    """Check the canonical v1 metadata and executable files, not mere names."""
    result = {}
    for name, root, executable, path_dir in (
        ("codex", package.cli_root, package.cli, package.cli_path_dir),
        ("codex-app-server", package.app_server_root, package.app_server, package.app_server_path_dir),
    ):
        metadata = json.loads((root / "codex-package.json").read_text(encoding="utf-8"))
        assert metadata["layoutVersion"] == 1
        assert metadata["target"] == package.target
        assert metadata["variant"] == name
        assert metadata["entrypoint"] == f"bin/{name}"
        assert metadata["pathDir"] == "codex-path"
        assert metadata["resourcesDir"] == "codex-resources"
        assert root / metadata["entrypoint"] == executable
        assert root / metadata["pathDir"] == path_dir
        for binary in (executable, root / "bin/codex-code-mode-host", path_dir / "rg"):
            assert binary.is_file() and os.access(binary, os.X_OK), binary
        assert (root / "codex-resources/bwrap").is_file()
        result[name] = metadata
    assert result["codex"]["version"] == result["codex-app-server"]["version"]
    return result


def _mock_config(
    home: Path, server: MockResponsesServer, *, agent_tools: bool = False
) -> None:
    home.mkdir(parents=True, exist_ok=True)
    (home / "config.toml").write_text(
        'model = "package-smoke"\nmodel_provider = "package_smoke"\n'
        'approval_policy = "never"\nsandbox_mode = "workspace-write"\n'
        '[sandbox_workspace_write]\nnetwork_access = true\n'
        '[features]\n'
        f'code_mode_only = {str(not agent_tools).lower()}\n'
        'code_mode_host = true\n'
        'multi_agent_v2 = true\nmemories = false\napps = false\nplugins = false\n'
        '[model_providers.package_smoke]\nname = "package smoke"\n'
        f'base_url = "{server.url}/v1"\nwire_api = "responses"\n'
        'request_max_retries = 0\nstream_max_retries = 0\n',
        encoding="utf-8",
    )


def open_and_reopen(
    package: SmokePackage,
    home: Path,
    server: MockResponsesServer,
    on_first_close: Callable[[], None] | None = None,
) -> dict[str, str]:
    """Run packaged code mode in a persistent thread, then resume in another process."""
    assert home.is_relative_to(package.directory), "only isolated synthetic homes"
    _mock_config(home, server)
    env = {**package.environment, "CODEX_HOME": str(home)}
    config = CodexConfig(codex_bin=str(package.cli), cwd=str(package.directory), env=env)
    server.enqueue_sse(sse([
        ev_response_created("package-persistent-code-mode"),
        {
            "type": "response.output_item.done",
            "item": {
                "type": "custom_tool_call", "call_id": "package-persistent-exec",
                "name": "exec",
                "input": (
                    "text(JSON.stringify(await tools.exec_command("
                    '{"cmd":"command -v rg && rg --version",'
                    '"login":false,"yield_time_ms":10000})))'
                ),
            },
        },
        ev_completed("package-persistent-code-mode"),
    ]))
    server.enqueue_assistant_message("first synthetic turn", response_id="package-persistent-first")
    with Codex(config=config) as client:
        first_agent = client.thread_start(
            ephemeral=False, approval_mode=ApprovalMode.deny_all,
            sandbox=Sandbox.workspace_write,
        )
        first = first_agent.run("Complete one harmless synthetic turn.")
        assert first.final_response == "first synthetic turn"
        thread_id = first_agent.id
        first_ua = client.metadata.userAgent
    assert thread_id
    outputs = [
        item["output"]
        for request in server.requests() if request.path == "/v1/responses"
        for item in request.input()
        if item.get("type") == "custom_tool_call_output"
        and item.get("call_id") == "package-persistent-exec"
    ]
    assert len(outputs) == 1, "packaged code-mode call did not execute"
    output = outputs[0]
    if not isinstance(output, str):
        output = next(part["text"] for part in output if part.get("text", "").startswith("{"))
    executed = json.loads(output)
    assert executed["exit_code"] == 0, executed
    lines = executed["output"].splitlines()
    assert lines and Path(lines[0]).is_relative_to(package.cli_root), executed
    assert "ripgrep" in executed["output"], executed
    if on_first_close is not None:
        on_first_close()
    with Codex(config=config) as client:
        resumed = client.thread_resume(thread_id)
        assert resumed.id == thread_id
        second_ua = client.metadata.userAgent
    return {
        "first_thread_id": thread_id,
        "second_thread_id": resumed.id,
        "first_user_agent": first_ua,
        "second_user_agent": second_ua,
        "packaged_rg_path": lines[0],
    }


def start_once_expect_failure(
    package: SmokePackage, home: Path, server: MockResponsesServer
) -> dict[str, str | int | None]:
    """Return an actual packaged-process startup error for a known-bad state.

    The caller must additionally match the expected migration error and prove
    its database preimage is unchanged; any exception alone is not acceptance.
    """
    assert home.is_relative_to(package.directory), "only isolated synthetic homes"
    _mock_config(home, server)
    env = {**package.environment, "CODEX_HOME": str(home)}
    config = CodexConfig(codex_bin=str(package.cli), cwd=str(package.directory), env=env)
    try:
        with Codex(config=config) as client:
            client.thread_start(ephemeral=False, approval_mode=ApprovalMode.deny_all)
    except Exception as error:
        return {
            "executable": str(package.cli),
            "error_type": type(error).__name__,
            "error": str(error),
            "returncode": None,
        }
    raise AssertionError("known-bad state unexpectedly started a packaged thread")
