#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

archive_file="${VALIDATION_LAB_NEXTEST_ARCHIVE_FILE:-${RUNNER_TEMP:-/tmp}/codex-core-carry-nextest.tar.zst}"
mkdir -p "$(dirname "${archive_file}")"

archive_extract_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/codex-core-carry-archive.XXXXXX")"
helper_stage_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/codex-core-carry-helpers.XXXXXX")"
trap 'rm -rf "${archive_extract_dir}" "${helper_stage_dir}"' EXIT

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

helper_archive_dir="${helper_stage_dir}/target/validation-lab/helpers"
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
# metadata. Stage helpers separately and install them only after extracting the
# original archive so an archive-owned path cannot overwrite the helpers.
tar --zstd -xf "${archive_file}" -C "${archive_extract_dir}"
mkdir -p "${archive_extract_dir}/target/validation-lab/helpers"
cp --preserve=mode,timestamps "${helper_archive_dir}/"* "${archive_extract_dir}/target/validation-lab/helpers/"
tar --zstd -cf "${archive_file}.tmp" -C "${archive_extract_dir}" target
mv "${archive_file}.tmp" "${archive_file}"

member_list="${archive_extract_dir}/members"
tar --zstd -tf "${archive_file}" > "${member_list}"
for helper in manifest codex-code-mode-host test_stdio_server; do
  member="target/validation-lab/helpers/${helper}"
  if ! grep -Fxq "${member}" "${member_list}"; then
    echo "repacked nextest archive is missing ${member}" >&2
    exit 1
  fi
done

du -h "${archive_file}"
