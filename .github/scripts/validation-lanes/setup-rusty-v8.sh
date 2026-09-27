#!/usr/bin/env bash
set -euo pipefail

target="${1:?usage: setup-rusty-v8.sh <target> }"
repo_root="${GITHUB_WORKSPACE:-$(git rev-parse --show-toplevel)}"
version="$(python3 "${repo_root}/.github/scripts/rusty_v8_bazel.py" resolved-v8-crate-version)"
release_tag="rusty-v8-v${version}"
base_url="https://github.com/openai/codex/releases/download/${release_tag}"
binding_dir="${RUNNER_TEMP:-/tmp}/rusty_v8"
profile="ptrcomp_sandbox_release"

if [[ "${target}" == *-pc-windows-msvc ]]; then
  archive_name="rusty_v8_${profile}_${target}.lib.gz"
else
  archive_name="librusty_v8_${profile}_${target}.a.gz"
fi
binding_name="src_binding_${profile}_${target}.rs"
checksums_name="rusty_v8_${profile}_${target}.sha256"

archive_path="${binding_dir}/${archive_name}"
binding_path="${binding_dir}/${binding_name}"
checksums_path="${binding_dir}/${checksums_name}"
trusted_checksums="${repo_root}/third_party/v8/rusty_v8_${version//./_}_release_manifests.sha256"

if command -v sha256sum >/dev/null 2>&1; then
  checksum_command=(sha256sum --check -)
else
  checksum_command=(shasum -a 256 --check -)
fi

mkdir -p "${binding_dir}"
curl -fsSL "${base_url}/${checksums_name}" -o "${checksums_path}"
expected_manifest_checksum="$(grep -F "  ${checksums_name}" "${trusted_checksums}" | cut -d ' ' -f 1)"
actual_manifest_checksum="$(python3 -c 'import hashlib, pathlib, sys; print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())' "${checksums_path}")"
if [[ "${actual_manifest_checksum}" != "${expected_manifest_checksum}" ]]; then
  echo "Checksum mismatch for ${checksums_name}: expected ${expected_manifest_checksum}, got ${actual_manifest_checksum}" >&2
  exit 1
fi
curl -fsSL "${base_url}/${archive_name}" -o "${archive_path}"
curl -fsSL "${base_url}/${binding_name}" -o "${binding_path}"

if [[ "$(wc -l < "${checksums_path}")" -ne 2 ]]; then
  echo "Expected exactly two checksums for ${target} in ${checksums_path}" >&2
  exit 1
fi
(cd "${binding_dir}" && tr -d '\r' < "${checksums_path}" | "${checksum_command[@]}")

export RUSTY_V8_ARCHIVE="${archive_path}"
export RUSTY_V8_SRC_BINDING_PATH="${binding_path}"
if [[ -n "${GITHUB_ENV:-}" ]]; then
  echo "RUSTY_V8_ARCHIVE=${archive_path}" >> "${GITHUB_ENV}"
  echo "RUSTY_V8_SRC_BINDING_PATH=${binding_path}" >> "${GITHUB_ENV}"
fi
