#!/usr/bin/env bash
set -euo pipefail

cd sdk/python

repo_root="$(git rev-parse --show-toplevel)"
python_sdk_image="${CODEX_PYTHON_SDK_IMAGE:-python:3.12-slim}"
docker run --rm \
  --user "$(id -u):$(id -g)" \
  -e HOME=/tmp/codex-python-sdk-home \
  -e UV_LINK_MODE=copy \
  -v "${repo_root}:${repo_root}" \
  -w "${repo_root}/sdk/python" \
  "${python_sdk_image}" \
  sh -euxc '
    python -m venv /tmp/uv
    /tmp/uv/bin/python -m pip install uv==0.11.3
    /tmp/uv/bin/uv sync --group dev --frozen
    /tmp/uv/bin/uv run --group dev ruff check --output-format=github .
    /tmp/uv/bin/uv run --group dev ruff format --check .
    /tmp/uv/bin/uv run --group dev pytest \
      tests/test_public_api_signatures.py \
      tests/test_public_api_runtime_behavior.py \
      tests/test_client_rpc_methods.py
  '
