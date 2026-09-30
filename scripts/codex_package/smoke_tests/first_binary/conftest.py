"""One-archive first-binary fixtures; the upstream release conftest is excluded.

Run with ``--confcutdir=.../smoke_tests/first_binary``.  The old release suite
requires app-server and symbols archives not produced by this branch workflow.
"""

import json
import os
import sys
import tarfile
from collections.abc import Iterator
from pathlib import Path

import pytest
import zstandard

# Reuse U's smoke fixtures/helper module without loading U's release conftest.
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from app_server_harness import MockResponsesServer
from fixtures import SmokePackage

from artifact import FirstBinaryEvidence, read_first_binary_artifact, validate_extracted_layout


def pytest_addoption(parser: pytest.Parser) -> None:
    group = parser.getgroup("Sedna first binary package")
    for option in ("artifact-dir", "artifact-name", "target-sha", "base-ref", "base-sha", "workflow-host-sha"):
        group.addoption(f"--{option}", required=True)
    group.addoption("--artifact-id", required=True, type=int)
    group.addoption("--run-id", required=True, type=int)


@pytest.fixture(scope="session")
def artifact_evidence(pytestconfig: pytest.Config) -> FirstBinaryEvidence:
    return read_first_binary_artifact(
        Path(pytestconfig.getoption("artifact_dir")),
        artifact_id=pytestconfig.getoption("artifact_id"),
        artifact_name=pytestconfig.getoption("artifact_name"),
        target_sha=pytestconfig.getoption("target_sha"),
        base_ref=pytestconfig.getoption("base_ref"),
        base_sha=pytestconfig.getoption("base_sha"),
        workflow_host_sha=pytestconfig.getoption("workflow_host_sha"),
        run_id=pytestconfig.getoption("run_id"),
    )


@pytest.fixture(scope="session")
def package(
    artifact_evidence: FirstBinaryEvidence,
    tmp_path_factory: pytest.TempPathFactory,
) -> SmokePackage:
    # The evidence fixture verifies the package archive digest before these
    # archive bytes are passed to the tar reader.
    directory = tmp_path_factory.mktemp("sedna-first-binary")
    root = directory / "package"
    root.mkdir()
    with zstandard.open(artifact_evidence.archive, "rb") as source:
        with tarfile.open(fileobj=source, mode="r|") as archive:
            archive.extractall(root, filter="data")
    validate_extracted_layout(root, artifact_evidence)
    cli = root / "bin/codex"
    home = directory / "synthetic-home"
    home.mkdir()
    environment = dict(os.environ)
    environment["CODEX_HOME"] = str(home)
    environment["PATH"] = "/usr/bin:/bin"
    environment["NO_PROXY"] = ",".join(filter(None, (
        environment.get("NO_PROXY", ""), "127.0.0.1", "localhost",
    )))
    environment.pop("BASH_ENV", None)
    environment["ZDOTDIR"] = str(directory)
    # Only the primary CLI archive exists.  The app_server fields satisfy the
    # existing dataclass shape; they are aliases, not another artifact/proof.
    return SmokePackage(
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
