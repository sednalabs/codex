#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "${repo_root}/codex-rs"
source_status="$(git status --porcelain --untracked-files=normal)"
if [[ -n "${source_status}" ]]; then
  echo "Refusing to build a validation artifact from a dirty source tree." >&2
  printf '%s\n' "${source_status}" >&2
  exit 1
fi

cargo test --locked -p codex-tui --lib side_chat_keeps_configured_guidance_separate_from_retained_policy
cargo build --locked --target x86_64-unknown-linux-gnu --release --bin codex --bin codex-responses-api-proxy

dist_dir="${repo_root}/dist"
stage_dir="${RUNNER_TEMP}/sedna-validation/x86_64-unknown-linux-gnu"
file_version="${CODEX_RELEASE_VERSION//+/__}"
archive_base="codex-sedna-validation-${file_version}-x86_64-unknown-linux-gnu"
archive_path="${dist_dir}/${archive_base}.tar.gz"

mkdir -p "${dist_dir}"
rm -rf "${stage_dir}"
mkdir -p "${stage_dir}"
install -Dm 0755 "target/x86_64-unknown-linux-gnu/release/codex" "${stage_dir}/codex"
install -Dm 0755 \
  "target/x86_64-unknown-linux-gnu/release/codex-responses-api-proxy" \
  "${stage_dir}/codex-responses-api-proxy"

version_output="$("${stage_dir}/codex" --version)"
printf '%s\n' "${version_output}"
if [[ "${version_output}" == *-dirty* ]]; then
  echo "Validation binary unexpectedly contains dirty git provenance." >&2
  exit 1
fi

tar -C "${stage_dir}" -czf "${archive_path}" .
smoke_dir="$(mktemp -d "${RUNNER_TEMP}/sedna-validation-smoke.XXXXXX")"
tar -xzf "${archive_path}" -C "${smoke_dir}"
cmp -- "${stage_dir}/codex" "${smoke_dir}/codex"
python3 "${repo_root}/side_chat_package_smoke.py" "${smoke_dir}/codex"

binary_sha256="$(sha256sum "${smoke_dir}/codex" | cut -d' ' -f1)"
archive_sha256="$(sha256sum "${archive_path}" | cut -d' ' -f1)"
printf '%s  %s\n' "${archive_sha256}" "${archive_path##*/}" \
  > "${archive_path}.sha256"

CODEX_REPOSITORY="${GITHUB_REPOSITORY}" \
  CODEX_COMMIT="$(git -C "${repo_root}" rev-parse HEAD)" \
  CODEX_RELEASE_VERSION="${CODEX_RELEASE_VERSION}" \
  CODEX_BINARY_SHA256="${binary_sha256}" \
  CODEX_ARCHIVE_SHA256="${archive_sha256}" \
  CODEX_WORKFLOW_URL="${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}" \
  python3 - <<'PY' > "${dist_dir}/${archive_base}.json"
import json
import os
import sys

payload = {
    "repository": os.environ["CODEX_REPOSITORY"],
    "commit": os.environ["CODEX_COMMIT"],
    "previewVersion": os.environ["CODEX_RELEASE_VERSION"],
    "artifact": "codex",
    "binarySha256": os.environ["CODEX_BINARY_SHA256"],
    "archiveSha256": os.environ["CODEX_ARCHIVE_SHA256"],
    "workflow": os.environ["CODEX_WORKFLOW_URL"],
}
json.dump(payload, sys.stdout, indent=2)
sys.stdout.write("\n")
PY
