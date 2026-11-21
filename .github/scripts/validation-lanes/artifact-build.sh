#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
commit="$(git rev-parse HEAD)"
tree="$(git rev-parse 'HEAD^{tree}')"
target=x86_64-unknown-linux-gnu
version="0.166.0+sedna.${GITHUB_RUN_NUMBER}.g${commit:0:8}"
dist="${root}/dist"
stage="$(mktemp -d "${RUNNER_TEMP}/codex-identity.XXXXXX")"
archive="${dist}/codex-cli-${target}.tar.gz"

cd "${root}"
mkdir -p "${dist}"
python3 scripts/build_codex_package.py \
  --target "${target}" --variant codex --cargo-profile release \
  --package-version "${version}" --package-dir "${stage}/package" \
  --archive-output "${archive}"

# Reuse the package build profile and target for both focused regressions.
export CODEX_RELEASE_VERSION="${version}"
cd "${root}/codex-rs"
for selector in started_and_targeted_wait_show_resolved_agent_identity live_agent_identity_survives_metadata_refresh_before_wait; do
  cargo test --locked --release --target "${target}" -p codex-tui --lib "${selector}" \
    | tee "${stage}/${selector}.log"
  grep -E "^test .*::${selector} \.\.\. ok$" "${stage}/${selector}.log"
done

mkdir "${stage}/extracted"
tar -xzf "${archive}" -C "${stage}/extracted"
[[ "$("${stage}/extracted/bin/codex" --version)" == "codex-cli ${version}" ]]
binary_hash="$(sha256sum "${stage}/extracted/bin/codex" | cut -d' ' -f1)"
(cd "${dist}" && sha256sum "codex-cli-${target}.tar.gz" > SHA256SUMS.txt)
jq -n \
  --arg repository "${GITHUB_REPOSITORY}" --arg sourceCommit "${commit}" \
  --arg sourceTree "${tree}" --arg target "${target}" \
  --arg packageVersion "${version}" --arg binarySha256 "${binary_hash}" \
  --arg workflowRunId "${GITHUB_RUN_ID}" \
  '$ARGS.named' > "${dist}/release-manifest.json"
