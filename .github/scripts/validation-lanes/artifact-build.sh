#!/usr/bin/env bash
set -euo pipefail

cd codex-rs
source_status="$(git status --porcelain --untracked-files=normal)"
if [[ -n "${source_status}" ]]; then
  echo "Refusing to build a validation artifact from a dirty source tree." >&2
  printf '%s\n' "${source_status}" >&2
  exit 1
fi
cargo build --locked --target x86_64-unknown-linux-gnu --release --bin codex --bin codex-responses-api-proxy
mkdir -p ../dist

stage_dir="${RUNNER_TEMP}/sedna-validation/x86_64-unknown-linux-gnu"
file_version="${CODEX_RELEASE_VERSION//+/__}"
archive_base="codex-sedna-validation-${file_version}-x86_64-unknown-linux-gnu"

rm -rf "${stage_dir}"
mkdir -p "${stage_dir}"

install -Dm 0755 "target/x86_64-unknown-linux-gnu/release/codex" "${stage_dir}/codex"
install -Dm 0755 "target/x86_64-unknown-linux-gnu/release/codex-responses-api-proxy" "${stage_dir}/codex-responses-api-proxy"

version_output="$("${stage_dir}/codex" --version)"
printf '%s\n' "${version_output}"
if [[ "${version_output}" == *-dirty* ]]; then
  echo "Validation binary unexpectedly contains dirty git provenance." >&2
  exit 1
fi

tar -C "${stage_dir}" -czf "../dist/${archive_base}.tar.gz" .

cat > "../dist/${archive_base}.json" <<EOF
{
  "previewVersion": "${CODEX_RELEASE_VERSION}",
  "source": "validation-lab"
}
EOF
