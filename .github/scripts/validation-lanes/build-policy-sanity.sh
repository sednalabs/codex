#!/usr/bin/env bash
set -euo pipefail

bash -n scripts/install/install.sh
python3 -m py_compile   scripts/stage_npm_packages.py   .github/scripts/verify_bazel_clippy_lints.py   .github/scripts/verify_cargo_workspace_manifests.py
python3 .github/scripts/verify_bazel_clippy_lints.py
python3 .github/scripts/verify_cargo_workspace_manifests.py
