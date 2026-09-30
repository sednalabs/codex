#!/usr/bin/env bash
set -euo pipefail

bash -n scripts/install/install.sh
just install-branch-artifact --help >/dev/null
python3 scripts/install/test_branch_artifact_installer.py
python3 -m py_compile   scripts/stage_npm_packages.py   .github/scripts/verify_bazel_clippy_lints.py   .github/scripts/verify_cargo_workspace_manifests.py
python3 .github/scripts/verify_bazel_clippy_lints.py
python3 .github/scripts/verify_cargo_workspace_manifests.py
