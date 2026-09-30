"""Read the exact first-binary Actions artifact before unpacking its package.

The producer writes one codex archive, a standalone same-source Responses
proxy, codex-package.json and manifest.json.  A downloader must additionally
bind the Actions artifact ID/name to the specified run through the GitHub API;
the payload cannot authenticate its own upload identity.
"""

import json
import os
import platform
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from package_acceptance import sha256_file


SHA = re.compile(r"[0-9a-f]{40}\Z")
DIGEST = re.compile(r"[0-9a-f]{64}\Z")
BASE_REF = re.compile(r"[A-Za-z0-9][A-Za-z0-9._/-]{0,199}\Z")
EXPECTED = {
    "x86_64-unknown-linux-gnu": ("x86_64", "X64", "ubuntu-24.04"),
    "aarch64-unknown-linux-gnu": ("aarch64", "ARM64", "ubuntu-24.04-arm"),
}


@dataclass(frozen=True)
class FirstBinaryEvidence:
    artifact_dir: Path
    archive: Path
    package_metadata: Path
    proxy: Path
    manifest_path: Path
    artifact_id: int
    artifact_name: str
    run_id: int
    workflow_host_sha: str
    target_sha: str
    base_ref: str
    base_sha: str
    target: str
    architecture: str
    version: str
    digests: dict[str, str]


def _required(data: dict[str, Any], name: str) -> str:
    value = data.get(name)
    assert isinstance(value, str) and value.strip(), f"missing {name}"
    return value


def read_first_binary_artifact(
    artifact_dir: Path,
    *,
    artifact_id: int,
    artifact_name: str,
    target_sha: str,
    base_ref: str,
    base_sha: str,
    workflow_host_sha: str,
    run_id: int,
) -> FirstBinaryEvidence:
    """Check H/T/B/run/name/arch and every payload digest before extraction."""
    assert os.environ.get("GITHUB_ACTIONS") == "true", "hosted Actions required"
    assert os.environ.get("RUNNER_OS") == "Linux"
    assert type(artifact_id) is int and artifact_id > 0
    assert type(run_id) is int and run_id > 0
    assert all(SHA.fullmatch(s) for s in (target_sha, base_sha, workflow_host_sha))
    assert BASE_REF.fullmatch(base_ref), "invalid base ref"
    directory = artifact_dir.resolve(strict=True)
    assert directory.is_dir()
    workspace = os.environ.get("GITHUB_WORKSPACE")
    if workspace:
        assert not directory.is_relative_to(Path(workspace).resolve()), (
            "downloaded package must be outside source checkout"
        )
    manifest_path = directory / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    assert manifest["schema_version"] == "sedna-first-binary-v1"
    assert manifest["repository"] == "sednalabs/codex"
    assert manifest["workflow"] == "sedna-branch-build"
    assert manifest["workflow_host_sha"] == workflow_host_sha
    assert manifest["product_sha"] == target_sha
    assert manifest["comparison_base_sha"] == base_sha
    assert manifest["workflow_run"].endswith(
        f"/sednalabs/codex/actions/runs/{run_id}"
    )
    target = _required(manifest, "target")
    assert target in EXPECTED, f"unsupported target {target}"
    architecture, runner_arch, runner_label = EXPECTED[target]
    assert manifest["architecture"] == architecture
    assert manifest["runner_label"] == runner_label
    assert platform.system() == "Linux" and platform.machine().lower() == architecture
    assert os.environ.get("RUNNER_ARCH") == runner_arch
    assert artifact_name == f"sedna-first-binary-{target_sha}-{architecture}-{run_id}"

    archive = directory / f"codex-package-{target}.tar.zst"
    package_metadata = directory / "codex-package.json"
    proxy = directory / "codex-responses-api-proxy"
    files = (archive, package_metadata, proxy)
    digests = manifest["artifacts_sha256"]
    assert isinstance(digests, dict) and set(digests) == {file.name for file in files}
    for file in files:
        expected = digests[file.name]
        assert isinstance(expected, str) and DIGEST.fullmatch(expected)
        assert file.is_file() and sha256_file(file) == expected, file.name
    # Actions artifact ZIP extraction need not preserve Unix mode bits.  The
    # proxy consumer restores only the execute bit on a digest-verified copy.

    metadata = json.loads(package_metadata.read_text(encoding="utf-8"))
    assert metadata["layoutVersion"] == 1
    assert metadata["variant"] == "codex"
    assert metadata["target"] == target
    assert metadata["entrypoint"] == "bin/codex"
    assert metadata["pathDir"] == "codex-path"
    assert metadata["resourcesDir"] == "codex-resources"
    version = _required(metadata, "version")
    assert manifest["package_version"] == version
    return FirstBinaryEvidence(
        directory, archive, package_metadata, proxy, manifest_path,
        artifact_id, artifact_name, run_id, workflow_host_sha, target_sha,
        base_ref, base_sha, target, architecture, version, digests,
    )


def validate_extracted_layout(root: Path, evidence: FirstBinaryEvidence) -> None:
    """Check the actual unpacked primary package against the outer metadata."""
    embedded = json.loads((root / "codex-package.json").read_text(encoding="utf-8"))
    outer = json.loads(evidence.package_metadata.read_text(encoding="utf-8"))
    assert embedded == outer, "archived layout metadata differs from uploaded manifest"
    assert embedded["target"] == evidence.target
    assert embedded["version"] == evidence.version
    for relative in (
        "bin/codex", "bin/codex-code-mode-host", "codex-path/rg",
        "codex-resources/bwrap",
    ):
        binary = root / relative
        assert binary.is_file() and os.access(binary, os.X_OK), binary
