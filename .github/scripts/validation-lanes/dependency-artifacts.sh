#!/usr/bin/env bash
# Produce a reviewable patch; generated files are never pushed or installed here.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
source_sha="$(git rev-parse HEAD)"
source_tree="$(git rev-parse HEAD^{tree})"
(
  cd codex-rs
  cargo update -p rmcp --precise 3.2.0
  cargo fmt --all
)
just bazel-lock-update
mkdir -p dist/dependency-artifacts
git diff --binary -- codex-rs MODULE.bazel.lock > dist/dependency-artifacts/generated.patch
cp codex-rs/Cargo.lock MODULE.bazel.lock dist/dependency-artifacts/
SOURCE_SHA="$source_sha" SOURCE_TREE="$source_tree" python3 - <<'PYTHON'
import hashlib
import json
import os
from pathlib import Path
root = Path("dist/dependency-artifacts")
receipt = {
    "source_sha": os.environ["SOURCE_SHA"],
    "source_tree": os.environ["SOURCE_TREE"],
    "workflow_run_id": os.environ["GITHUB_RUN_ID"],
    "workflow_run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
    "purpose": "reviewable generated artifacts; not compiled or runtime validation",
    "sha256": {
        path.name: hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sorted(root.iterdir()) if path.is_file()
    },
}
(root / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
PYTHON
