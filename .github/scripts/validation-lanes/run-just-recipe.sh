#!/usr/bin/env bash
set -euo pipefail
if [[ $# -lt 1 ]]; then
  echo 'usage: run-with-v8.sh <recipe> [args...]' >&2
  exit 64
fi
export TARGET=x86_64-unknown-linux-gnu
setup_script="${RUNNER_TEMP}/w14939-setup-rusty-v8.sh"
sed -n '/^        set -euo pipefail$/,$p' .github/actions/setup-rusty-v8/action.yml | sed 's/^        //' > "${setup_script}"
bash "${setup_script}"
export RUSTY_V8_ARCHIVE="${RUNNER_TEMP}/rusty_v8/librusty_v8_ptrcomp_sandbox_release_${TARGET}.a.gz"
export RUSTY_V8_SRC_BINDING_PATH="${RUNNER_TEMP}/rusty_v8/src_binding_ptrcomp_sandbox_release_${TARGET}.rs"
exec just "$@"
