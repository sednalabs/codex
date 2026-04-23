#!/usr/bin/env bash
set -euo pipefail

cd codex-rs
mkdir -p ../dist
cargo build --locked --target x86_64-unknown-linux-gnu --release --bin codex --bin codex-responses-api-proxy

stage_dir="${RUNNER_TEMP}/sedna-validation/x86_64-unknown-linux-gnu"
file_version="${CODEX_RELEASE_VERSION//+/__}"
archive_base="codex-sedna-validation-${file_version}-x86_64-unknown-linux-gnu"

rm -rf "${stage_dir}"
mkdir -p "${stage_dir}"

install -Dm 0755 "target/x86_64-unknown-linux-gnu/release/codex" "${stage_dir}/codex"
install -Dm 0755 "target/x86_64-unknown-linux-gnu/release/codex-responses-api-proxy" "${stage_dir}/codex-responses-api-proxy"

tar -C "${stage_dir}" -czf "../dist/${archive_base}.tar.gz" .

cat > "../dist/${archive_base}.json" <<EOF
{
  "previewVersion": "${CODEX_RELEASE_VERSION}",
  "source": "validation-lab"
}
EOF
