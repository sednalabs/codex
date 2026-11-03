#!/usr/bin/env bash
set -euo pipefail

python3 -m py_compile .github/scripts/check_workflow_policy.py
python3 .github/scripts/check_workflow_policy.py
