#!/usr/bin/env python3
"""Calculate local-only identity hashes for one model-baseline overlay.

The plan file is a TOML fragment with a [model_baseline] table. Feed the exact
saved baseline bytes on stdin; neither input is printed or sent anywhere.
"""

import argparse
import hashlib
import json
import sys
import tomllib
from pathlib import Path

DOMAIN = b"codex-historical-model-baseline-overlay-v1\0"
LIMIT = 256 * 1024


def identity(plan, baseline):
    if len(baseline) > LIMIT:
        raise ValueError("baseline exceeds the size limit")
    if set(plan) - {"on_absent", "models", "selection", "edits"}:
        raise ValueError("unrecognized model_baseline option")
    on_absent = plan.get("on_absent", "error")
    selection = plan.get("selection", "unique")
    models = plan.get("models")
    edits = plan.get("edits", [])
    if on_absent not in ("error", "skip") or selection != "unique":
        raise ValueError("invalid or ineligible adoption plan")
    if models is not None and (
        not isinstance(models, list) or not models
        or any(not isinstance(model, str) or not model for model in models)
    ):
        raise ValueError("invalid model selectors")
    if not isinstance(edits, list) or not 1 <= len(edits) <= 64:
        raise ValueError("invalid edit count")
    normalized_edits = []
    for edit in edits:
        if not isinstance(edit, dict) or set(edit) != {"op", "text", "expected_matches"}:
            raise ValueError("invalid adoption edit")
        if edit["op"] not in ("remove_exact", "remove_exact_if_present"):
            raise ValueError("adoption permits exact removals only")
        if (not isinstance(edit["text"], str) or not edit["text"]
                or len(edit["text"].encode()) > 32 * 1024
                or type(edit["expected_matches"]) is not int or edit["expected_matches"] != 1):
            raise ValueError("invalid exact removal")
        normalized_edits.append({"op": edit["op"], "text": edit["text"], "expected_matches": 1})
    normalized = {"on_absent": on_absent, "models": models, "selection": selection, "edits": normalized_edits}
    encoded = json.dumps(normalized, ensure_ascii=False, separators=(",", ":")).encode()
    return hashlib.sha256(baseline).hexdigest(), hashlib.sha256(DOMAIN + encoded).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan-file", required=True, type=Path)
    args = parser.parse_args()
    if sys.stdin.isatty():
        parser.error("pipe the exact baseline bytes on stdin")
    try:
        with args.plan_file.open("rb") as plan_file:
            plan_bytes = plan_file.read(LIMIT + 1)
        if len(plan_bytes) > LIMIT:
            raise ValueError("plan file exceeds the size limit")
        document = tomllib.loads(plan_bytes.decode("utf-8"))
        baseline = sys.stdin.buffer.read(LIMIT + 1)
        baseline_hash, plan_hash = identity(document["model_baseline"], baseline)
    except (OSError, UnicodeError, KeyError, TypeError, ValueError, tomllib.TOMLDecodeError) as error:
        # Do not echo parser errors, paths, or instruction text.
        print(f"cannot calculate overlay identities ({type(error).__name__})", file=sys.stderr)
        return 1
    print(f"baseline_sha256 = \"{baseline_hash}\"")
    print(f"plan_sha256 = \"{plan_hash}\"")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
