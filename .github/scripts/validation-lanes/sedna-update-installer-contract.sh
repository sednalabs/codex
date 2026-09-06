#!/usr/bin/env bash
set -euo pipefail

python3 scripts/install/test_sedna_release_lower_bound.py
bash -n scripts/install_sedna_release_asset
