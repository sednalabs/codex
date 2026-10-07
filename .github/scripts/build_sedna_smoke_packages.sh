#!/usr/bin/env bash
set -euo pipefail

target="${1:?target required}"
release_dir="${2:?release directory required}"

case "${target}" in
  x86_64-unknown-linux-gnu)
    expected_machine="x86_64"
    ;;
  aarch64-unknown-linux-gnu)
    expected_machine="aarch64"
    ;;
  *)
    echo "unsupported smoke-package target." >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  x86_64|amd64) actual_machine="x86_64" ;;
  aarch64|arm64) actual_machine="aarch64" ;;
  *) actual_machine="unknown" ;;
esac
if [[ "${actual_machine}" != "${expected_machine}" ]]; then
  echo "smoke-package build runner does not match the native target." >&2
  exit 1
fi

source_root="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
workflow_root="${source_root}/.workflow-src"
output_dir="${source_root}/smoke-output/${target}"
preview_version="${CODEX_RELEASE_VERSION:?CODEX_RELEASE_VERSION is required}"
run_id="${GITHUB_RUN_ID:?GITHUB_RUN_ID is required}"
repository="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
server_url="${GITHUB_SERVER_URL:?GITHUB_SERVER_URL is required}"

source_sha="$(git -C "${source_root}" rev-parse HEAD)"
source_tree="$(git -C "${source_root}" rev-parse 'HEAD^{tree}')"
workflow_sha="$(git -C "${workflow_root}" rev-parse HEAD)"
if [[ ! "${source_sha}" =~ ^[0-9a-f]{40}$ || ! "${source_tree}" =~ ^[0-9a-f]{40}$ || ! "${workflow_sha}" =~ ^[0-9a-f]{40}$ || "${workflow_sha}" != "${GITHUB_SHA:?GITHUB_SHA is required}" ]]; then
  echo "source or trusted workflow identity is invalid." >&2
  exit 1
fi
if [[ -n "$(git -C "${source_root}" status --porcelain --untracked-files=normal)" ]]; then
  echo "refusing to package from a dirty source checkout." >&2
  exit 1
fi

mkdir -p "${output_dir}"
input_dir="$(mktemp -d "${RUNNER_TEMP:?RUNNER_TEMP is required}/sedna-smoke-input-${target}.XXXXXX")"
package_dir="$(mktemp -d "${RUNNER_TEMP}/sedna-smoke-packages-${target}.XXXXXX")"
cleanup() {
  rm -rf -- "${input_dir}" "${package_dir}"
}
trap cleanup EXIT

for binary in codex codex-app-server codex-code-mode-host bwrap; do
  source_binary="${release_dir%/}/${binary}"
  if [[ ! -f "${source_binary}" ]]; then
    echo "required package input is missing." >&2
    exit 1
  fi
  install -m 0755 "${source_binary}" "${input_dir}/${binary}"
done

symbols_name="sedna-${run_id}-${target}"
bash "${workflow_root}/.github/scripts/archive-release-symbols-and-strip-binaries.sh" \
  --target "${target}" \
  --artifact-name "${symbols_name}" \
  --release-dir "${input_dir}" \
  --archive-dir "${output_dir}" \
  --binaries "codex codex-app-server codex-code-mode-host"
mv "${output_dir}/codex-symbols-${symbols_name}.tar.gz" "${output_dir}/codex-symbols.tar.gz"

python3 "${source_root}/scripts/build_codex_package.py" \
  --target "${target}" \
  --variant codex \
  --entrypoint-bin "${input_dir}/codex" \
  --code-mode-host-bin "${input_dir}/codex-code-mode-host" \
  --bwrap-bin "${input_dir}/bwrap" \
  --cargo-profile release \
  --package-dir "${package_dir}/cli" \
  --archive-output "${output_dir}/codex-package.tar.gz" \
  --force

python3 "${source_root}/scripts/build_codex_package.py" \
  --target "${target}" \
  --variant codex-app-server \
  --entrypoint-bin "${input_dir}/codex-app-server" \
  --code-mode-host-bin "${input_dir}/codex-code-mode-host" \
  --bwrap-bin "${input_dir}/bwrap" \
  --cargo-profile release \
  --package-dir "${package_dir}/app-server" \
  --archive-output "${output_dir}/codex-app-server-package.tar.gz" \
  --force

SOURCE_SHA="${source_sha}" \
SOURCE_TREE="${source_tree}" \
WORKFLOW_SHA="${workflow_sha}" \
PREVIEW_VERSION="${preview_version}" \
RUN_ID="${run_id}" \
REPOSITORY="${repository}" \
SERVER_URL="${server_url}" \
TARGET="${target}" \
CLI_PACKAGE_DIR="${package_dir}/cli" \
APP_SERVER_PACKAGE_DIR="${package_dir}/app-server" \
OUTPUT_DIR="${output_dir}" \
SYMBOLS_NAME="codex-symbols.tar.gz" \
python3 - <<'PY'
import hashlib
import json
import os
import re
import tomllib
from pathlib import Path

root = Path(os.environ["GITHUB_WORKSPACE"])
output_dir = Path(os.environ["OUTPUT_DIR"])
target = os.environ["TARGET"]
source_manifest = tomllib.loads((root / "codex-rs/Cargo.toml").read_text(encoding="utf-8"))
package_version = source_manifest["workspace"]["package"]["version"]
if not isinstance(package_version, str) or not re.fullmatch(r"[0-9A-Za-z.+-]{1,128}", package_version):
    raise SystemExit("source package version is invalid")

expected_manifests = (
    (Path(os.environ["CLI_PACKAGE_DIR"]) / "codex-package.json", "codex", "bin/codex"),
    (
        Path(os.environ["APP_SERVER_PACKAGE_DIR"]) / "codex-package.json",
        "codex-app-server",
        "bin/codex-app-server",
    ),
)
for path, variant, entrypoint in expected_manifests:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload != {
        "layoutVersion": 1,
        "version": package_version,
        "target": target,
        "variant": variant,
        "entrypoint": entrypoint,
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
    }:
        raise SystemExit("package builder emitted unexpected manifest metadata")

archives = {
    "cli": "codex-package.tar.gz",
    "app_server": "codex-app-server-package.tar.gz",
    "symbols": os.environ["SYMBOLS_NAME"],
}
archive_inventory = {}
for key, name in archives.items():
    path = output_dir / name
    if not path.is_file() or path.is_symlink():
        raise SystemExit("expected smoke-package archive is missing")
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    archive_inventory[key] = {
        "name": name,
        "size_bytes": path.stat().st_size,
        "sha256": digest.hexdigest(),
    }

identity = {
    "schema_version": "sedna-smoke-package-v1",
    "repository": os.environ["REPOSITORY"],
    "run_id": os.environ["RUN_ID"],
    "workflow_url": (
        f'{os.environ["SERVER_URL"].rstrip("/")}/{os.environ["REPOSITORY"]}'
        f'/actions/runs/{os.environ["RUN_ID"]}'
    ),
    "source_sha": os.environ["SOURCE_SHA"],
    "source_tree": os.environ["SOURCE_TREE"],
    "workflow_sha": os.environ["WORKFLOW_SHA"],
    "target": target,
    "preview_version": os.environ["PREVIEW_VERSION"],
    "package_version": package_version,
    "archives": archive_inventory,
}
(output_dir / "smoke-package.json").write_text(
    json.dumps(identity, sort_keys=True, indent=2) + "\n", encoding="utf-8"
)
PY
