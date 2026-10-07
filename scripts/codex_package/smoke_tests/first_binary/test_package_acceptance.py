"""First-binary artifact consumers; run only on native hosted Linux runners."""

import json
import re
import shutil
import stat
import subprocess
import tomllib
from pathlib import Path
from tempfile import TemporaryDirectory

import pytest
from app_server_harness import MockResponsesServer
from openai_codex import Codex, CodexConfig

from artifact import (
    FirstBinaryEvidence,
    read_first_binary_artifact,
    validate_extracted_layout,
)
from fixtures import SmokePackage
from package_acceptance import _mock_config, open_and_reopen


def _read_again(directory: Path, evidence: FirstBinaryEvidence) -> FirstBinaryEvidence:
    return read_first_binary_artifact(
        directory,
        artifact_id=evidence.artifact_id,
        artifact_name=evidence.artifact_name,
        target_sha=evidence.target_sha,
        base_ref=evidence.base_ref,
        base_sha=evidence.base_sha,
        workflow_host_sha=evidence.workflow_host_sha,
        run_id=evidence.run_id,
    )


def test_exact_single_artifact_and_native_layout(
    artifact_evidence: FirstBinaryEvidence, package: SmokePackage
) -> None:
    assert package.cli_root.is_relative_to(package.directory)
    assert package.cli_root != artifact_evidence.artifact_dir
    assert package.target == artifact_evidence.target
    validate_extracted_layout(package.cli_root, artifact_evidence)
    assert artifact_evidence.archive.name in artifact_evidence.digests
    assert artifact_evidence.proxy.name in artifact_evidence.digests


def test_wrong_source_manifest_is_rejected_before_unpack(
    artifact_evidence: FirstBinaryEvidence, tmp_path: Path
) -> None:
    for name in artifact_evidence.digests:
        (tmp_path / name).symlink_to(artifact_evidence.artifact_dir / name)
    manifest = json.loads(artifact_evidence.manifest_path.read_text(encoding="utf-8"))
    manifest["product_sha"] = "0" * 40
    (tmp_path / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
    with pytest.raises(AssertionError):
        _read_again(tmp_path, artifact_evidence)


def test_wrong_target_and_missing_helper_are_rejected(
    artifact_evidence: FirstBinaryEvidence, package: SmokePackage, tmp_path: Path
) -> None:
    bad = tmp_path / "bad-package"
    shutil.copytree(package.cli_root, bad)
    manifest_path = bad / "codex-package.json"
    metadata = json.loads(manifest_path.read_text(encoding="utf-8"))
    metadata["target"] = (
        "aarch64-unknown-linux-gnu"
        if artifact_evidence.architecture == "x86_64"
        else "x86_64-unknown-linux-gnu"
    )
    manifest_path.write_text(json.dumps(metadata), encoding="utf-8")
    with pytest.raises(AssertionError):
        validate_extracted_layout(bad, artifact_evidence)
    manifest_path.write_text(
        (package.cli_root / "codex-package.json").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    (bad / "bin/codex-code-mode-host").unlink()
    with pytest.raises(AssertionError):
        validate_extracted_layout(bad, artifact_evidence)


def test_banner_native_client_and_same_source_proxy_execution(
    artifact_evidence: FirstBinaryEvidence, package: SmokePackage, tmp_path: Path
) -> None:
    source_root = Path(__file__).resolve().parents[4]
    with (source_root / "codex-rs/Cargo.toml").open("rb") as manifest_file:
        cargo_version = tomllib.load(manifest_file)["workspace"]["package"]["version"]
    cargo_base = re.fullmatch(
        r"(?P<base>\d+\.\d+\.\d+)(?:-dev\.sedna\.\d+)?",
        cargo_version,
    )
    assert cargo_base is not None
    producer_manifest = json.loads(
        artifact_evidence.manifest_path.read_text(encoding="utf-8")
    )
    progressive_iteration = producer_manifest.get("progressive_iteration")
    assert type(progressive_iteration) is int and progressive_iteration > 0
    expected_package_version = (
        f"{cargo_base.group('base')}-dev.sedna.{progressive_iteration}"
        f"+g{artifact_evidence.target_sha[:8]}"
    )
    assert artifact_evidence.version == expected_package_version

    banner = package.run("--version").stdout.strip()
    assert banner == f"codex Sedna v{expected_package_version}", banner
    with Codex(config=CodexConfig(
        codex_bin=str(package.cli), cwd=str(package.directory), env=package.environment,
    )) as client:
        native_ua = client.metadata.userAgent
        assert native_ua and cargo_version in native_ua
        assert expected_package_version not in native_ua

    # Actions artifact ZIPs need not preserve Unix execute bits.  Execute a
    # digest-verified copy of the separately built, same-T proxy sidecar.
    proxy = tmp_path / "codex-responses-api-proxy"
    shutil.copy2(artifact_evidence.proxy, proxy)
    proxy.chmod(proxy.stat().st_mode | stat.S_IXUSR)
    help_result = subprocess.run(
        [str(proxy), "--help"], cwd=package.directory, env=package.environment,
        capture_output=True, text=True, timeout=15, check=False,
    )
    assert help_result.returncode == 0 and "Usage:" in help_result.stdout
    parsed = subprocess.run(
        [str(proxy), "--upstream-url", "not-a-url"], input="synthetic-test-only\n",
        cwd=package.directory, env=package.environment,
        capture_output=True, text=True, timeout=15, check=False,
    )
    assert parsed.returncode != 0
    assert "parsing --upstream-url" in parsed.stderr


def test_packaged_model_list_exposes_bundled_gpt6_descriptors(
    package: SmokePackage, responses_server: MockResponsesServer
) -> None:
    expected_models = {
        "gpt-6.1-sol": (
            "GPT-6.1-Sol",
            "Latest workhorse model for coding and everyday work.",
            "low",
            (
                ("low", "Fast responses with lighter reasoning"),
                ("medium", "Balances speed and reasoning depth for everyday tasks"),
                ("high", "Greater reasoning depth for complex problems"),
                ("xhigh", "Extra high reasoning depth for complex problems"),
                ("max", "Maximum reasoning depth for the hardest problems"),
                ("ultra", "Maximum reasoning with automatic task delegation"),
            ),
            "v2",
            ("text", "image"),
            True,
        ),
        "gpt-6-sol": (
            "GPT-6-Sol",
            "Previous generation workhorse model.",
            "medium",
            (
                ("low", "Fast responses with lighter reasoning"),
                ("medium", "Balances speed and reasoning depth for everyday tasks"),
                ("high", "Greater reasoning depth for complex problems"),
                ("xhigh", "Extra high reasoning depth for complex problems"),
                ("max", "Maximum reasoning depth for the hardest problems"),
                ("ultra", "Maximum reasoning with automatic task delegation"),
            ),
            "v2",
            ("text", "image"),
            False,
        ),
        "gpt-6-luna": (
            "GPT-6-Luna",
            "Fast and affordable model for easier tasks.",
            "medium",
            (
                ("low", "Fast responses with lighter reasoning"),
                ("medium", "Balances speed and reasoning depth for everyday tasks"),
                ("high", "Greater reasoning depth for complex problems"),
                ("xhigh", "Extra high reasoning depth for complex problems"),
                ("max", "Maximum reasoning depth for the hardest problems"),
            ),
            "v2",
            ("text", "image"),
            False,
        ),
    }

    with TemporaryDirectory(prefix="model-list-", dir=package.directory) as home_path:
        home = Path(home_path)
        assert home.is_relative_to(package.directory)
        _mock_config(home, responses_server)
        environment = {**package.environment, "CODEX_HOME": str(home)}
        environment.pop("OPENAI_API_KEY", None)
        environment.pop("CODEX_API_KEY", None)
        with Codex(config=CodexConfig(
            codex_bin=str(package.cli), cwd=str(package.directory), env=environment,
        )) as client:
            response = client.models(include_hidden=False)

    model_ids = [model.id for model in response.data]
    assert response.next_cursor is None
    assert [model_id for model_id in model_ids if model_id in expected_models] == list(
        expected_models
    )
    for slug, expected in expected_models.items():
        model = next(model for model in response.data if model.id == slug)
        actual = (
            model.id,
            model.model,
            model.display_name,
            model.description,
            model.default_reasoning_effort,
            tuple(
                (option.reasoning_effort, option.description)
                for option in model.supported_reasoning_efforts
            ),
            model.multi_agent_version,
            tuple(model.input_modalities or ()),
            model.hidden,
            model.is_default,
        )
        assert actual == (
            slug,
            slug,
            *expected[:4],
            expected[4],
            expected[5],
            False,
            expected[6],
        )
    assert not any(
        request.path == "/v1/responses" for request in responses_server.requests()
    )


def test_persistent_code_mode_and_reopen_from_real_package(
    package: SmokePackage, responses_server: MockResponsesServer
) -> None:
    witness = open_and_reopen(
        package, Path(package.environment["CODEX_HOME"]), responses_server
    )
    assert witness["first_thread_id"] == witness["second_thread_id"]
    assert Path(witness["packaged_rg_path"]).is_relative_to(package.cli_root)
    assert witness["first_user_agent"] == witness["second_user_agent"]
    request = next(
        request for request in responses_server.requests()
        if request.path == "/v1/responses" and request.header("user-agent")
    )
    assert request.header("user-agent"), "native outbound User-Agent absent"


def test_missing_host_cannot_qualify_code_mode(
    package: SmokePackage, responses_server: MockResponsesServer
) -> None:
    copied = package.directory / "missing-host-control"
    shutil.copytree(package.cli_root, copied)
    (copied / "bin/codex-code-mode-host").unlink()
    broken = SmokePackage(
        target=package.target,
        cli=copied / "bin/codex", cli_root=copied,
        cli_path_dir=copied / "codex-path",
        app_server=copied / "bin/codex", app_server_root=copied,
        app_server_path_dir=copied / "codex-path",
        directory=package.directory,
        environment={**package.environment, "CODEX_HOME": str(package.directory / "missing-host-home")},
    )
    with pytest.raises(json.JSONDecodeError) as error:
        open_and_reopen(broken, package.directory / "missing-host-home", responses_server)
    assert "failed to spawn code-mode host" in error.value.doc
    assert "missing-host-control/bin/codex-code-mode-host" in error.value.doc
    assert "No such file or directory" in error.value.doc
    assert any(req.path == "/v1/responses" for req in responses_server.requests()), (
        "negative control never reached the local mock provider"
    )
