#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

archive_file="${VALIDATION_LAB_NEXTEST_ARCHIVE_FILE:-${RUNNER_TEMP:-/tmp}/codex-core-carry-nextest.tar.zst}"
mkdir -p "$(dirname "${archive_file}")"

archive_extract_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/codex-core-carry-archive.XXXXXX")"
trap 'rm -rf "${archive_extract_dir}"' EXIT

# Build the runtime helpers from this exact checkout and target directory. They
# are not test binaries, so cargo-nextest does not include them in its archive.
target="${VALIDATION_LAB_RUST_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"
source "../.github/scripts/validation-lanes/setup-rusty-v8.sh" "${target}"
cargo build \
  -p codex-code-mode-host --bin codex-code-mode-host \
  -p codex-rmcp-client --bin test_stdio_server

cargo nextest archive \
  -p codex-core \
  --test all \
  --archive-file "${archive_file}"

helper_archive_dir="${archive_extract_dir}/validation-lab/helpers"
mkdir -p "${helper_archive_dir}"
for helper in codex-code-mode-host test_stdio_server; do
  source_path="${CARGO_TARGET_DIR:-target}/debug/${helper}"
  if [[ ! -x "${source_path}" ]]; then
    echo "required nextest helper is missing or not executable: ${source_path}" >&2
    exit 1
  fi
  cp --preserve=mode,timestamps "${source_path}" "${helper_archive_dir}/${helper}"
done

validation_sha="$(git rev-parse HEAD)"
cat > "${helper_archive_dir}/manifest" <<EOF
validation_sha=${validation_sha}
codex-code-mode-host=$(sha256sum "${helper_archive_dir}/codex-code-mode-host" | awk '{print $1}')
test_stdio_server=$(sha256sum "${helper_archive_dir}/test_stdio_server" | awk '{print $1}')
EOF

# Repack the nextest archive with the two runtime helpers adjacent to its
# metadata. nextest ignores the validation-lab namespace, while the consumer
# extracts it explicitly and never invokes a consumer-side Cargo build.
tar --zstd -xf "${archive_file}" -C "${archive_extract_dir}"
tar --zstd -cf "${archive_file}.tmp" -C "${archive_extract_dir}" .
mv "${archive_file}.tmp" "${archive_file}"

du -h "${archive_file}"
