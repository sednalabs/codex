"""One-archive first-binary fixtures; the upstream release conftest is excluded.

Run with ``--confcutdir=.../smoke_tests/first_binary``.  The old release suite
requires app-server and symbols archives not produced by this branch workflow.
"""

import os
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
from collections.abc import Iterator
from pathlib import Path

import pytest
import zstandard

# Reuse U's smoke fixtures/helper module without loading U's release conftest.
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from app_server_harness import MockResponsesServer
from fixtures import SmokePackage

from artifact import FirstBinaryEvidence, read_first_binary_artifact, validate_extracted_layout
from package_acceptance import ConsumerContext, read_consumer_context


SHA = re.compile(r"[0-9a-f]{40}\Z")
SOURCE_ROOT = Path(__file__).resolve().parents[4]


def _producer_identity(config: pytest.Config) -> tuple[str, int, bool]:
    legacy = (config.getoption("workflow_host_sha"), config.getoption("run_id"))
    explicit = (
        config.getoption("expected_producer_workflow_host_sha"),
        config.getoption("expected_producer_run_id"),
    )
    has_legacy = any(value is not None for value in legacy)
    has_explicit = any(value is not None for value in explicit)
    if has_legacy == has_explicit:
        raise pytest.UsageError("supply exactly one complete producer H/run pair")
    host, run_id = explicit if has_explicit else legacy
    if not isinstance(host, str) or not SHA.fullmatch(host):
        raise pytest.UsageError("producer workflow host must be a full lowercase SHA")
    if type(run_id) is not int or run_id <= 0:
        raise pytest.UsageError("producer run ID must be positive")
    return host, run_id, has_explicit


def pytest_addoption(parser: pytest.Parser) -> None:
    group = parser.getgroup("Sedna first binary package")
    for option in ("artifact-dir", "artifact-name", "target-sha", "base-ref", "base-sha"):
        group.addoption(f"--{option}", required=True)
    group.addoption("--artifact-id", required=True, type=int)
    group.addoption("--workflow-host-sha")
    group.addoption("--run-id", type=int)
    group.addoption("--expected-producer-workflow-host-sha")
    group.addoption("--expected-producer-run-id", type=int)
    group.addoption("--consumer-context-file", required=True, type=Path)
    group.addoption("--fixture-sha")


def pytest_configure(config: pytest.Config) -> None:
    _, _, cross_run = _producer_identity(config)
    for option in ("target_sha", "base_sha", "fixture_sha"):
        value = config.getoption(option)
        if value is not None and not SHA.fullmatch(value):
            raise pytest.UsageError(f"{option} must be a full lowercase SHA")
    if cross_run:
        fixture_sha = config.getoption("fixture_sha")
        if fixture_sha is None or fixture_sha == config.getoption("target_sha"):
            raise pytest.UsageError("cross-run fixture SHA Q must be explicit and differ from product T")


@pytest.fixture(scope="session")
def fixture_source_sha(pytestconfig: pytest.Config) -> str:
    expected = pytestconfig.getoption("fixture_sha") or pytestconfig.getoption("target_sha")
    actual = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=SOURCE_ROOT,
        capture_output=True, text=True, check=True, timeout=10,
    ).stdout.strip()
    assert actual == expected, "fixture checkout does not match its explicit SHA"
    return expected


@pytest.fixture(scope="session")
def artifact_evidence(
    pytestconfig: pytest.Config, fixture_source_sha: str
) -> FirstBinaryEvidence:
    assert fixture_source_sha
    producer_host, producer_run, _ = _producer_identity(pytestconfig)
    return read_first_binary_artifact(
        Path(pytestconfig.getoption("artifact_dir")),
        artifact_id=pytestconfig.getoption("artifact_id"),
        artifact_name=pytestconfig.getoption("artifact_name"),
        target_sha=pytestconfig.getoption("target_sha"),
        base_ref=pytestconfig.getoption("base_ref"),
        base_sha=pytestconfig.getoption("base_sha"),
        workflow_host_sha=producer_host,
        run_id=producer_run,
    )


@pytest.fixture(scope="session")
def consumer_context(
    pytestconfig: pytest.Config, artifact_evidence: FirstBinaryEvidence
) -> ConsumerContext:
    _, _, cross_run = _producer_identity(pytestconfig)
    return read_consumer_context(
        pytestconfig.getoption("consumer_context_file"),
        target_sha=artifact_evidence.target_sha,
        base_sha=artifact_evidence.base_sha,
        target=artifact_evidence.target,
        producer_run_id=artifact_evidence.run_id if cross_run else None,
    )


@pytest.fixture(scope="session")
def package(
    artifact_evidence: FirstBinaryEvidence,
    consumer_context: ConsumerContext,
) -> Iterator[SmokePackage]:
    # The evidence fixture verifies the package archive digest before these
    # archive bytes are passed to the tar reader.
    assert consumer_context.run_id > 0
    workspace = Path(os.environ["GITHUB_WORKSPACE"]).resolve(strict=True)
    assert workspace.is_dir(), "hosted source checkout missing"
    parent = workspace.parent.resolve(strict=True)
    system_temp = Path(tempfile.gettempdir()).resolve(strict=True)
    assert not parent.is_relative_to(system_temp), "synthetic profile parent is system temp"
    assert not parent.is_relative_to(workspace), "synthetic profile parent is source checkout"
    with tempfile.TemporaryDirectory(prefix="codex-first-binary-", dir=parent) as temporary:
        directory = Path(temporary).resolve(strict=True)
        assert directory.parent == parent
        assert stat.S_IMODE(directory.stat().st_mode) == 0o700
        assert not directory.is_relative_to(system_temp)
        root = directory / "package"
        root.mkdir()
        with zstandard.open(artifact_evidence.archive, "rb") as source:
            with tarfile.open(fileobj=source, mode="r|") as archive:
                archive.extractall(root, filter="data")
        validate_extracted_layout(root, artifact_evidence)
        cli = root / "bin/codex"
        home = directory / "synthetic-home"
        home.mkdir(mode=0o700)
        assert stat.S_IMODE(home.stat().st_mode) == 0o700
        environment = dict(os.environ)
        environment["CODEX_HOME"] = str(home)
        environment["PATH"] = "/usr/bin:/bin"
        environment["NO_PROXY"] = ",".join(filter(None, (
            environment.get("NO_PROXY", ""), "127.0.0.1", "localhost",
        )))
        environment.pop("BASH_ENV", None)
        environment["ZDOTDIR"] = str(directory)
        # Only the primary CLI archive exists.  The app_server fields satisfy
        # the existing dataclass shape, not a second artifact or proof.
        yield SmokePackage(
            target=artifact_evidence.target,
            cli=cli, cli_root=root, cli_path_dir=root / "codex-path",
            app_server=cli, app_server_root=root,
            app_server_path_dir=root / "codex-path",
            directory=directory, environment=environment,
        )


@pytest.fixture
def responses_server() -> Iterator[MockResponsesServer]:
    with MockResponsesServer() as server:
        yield server
