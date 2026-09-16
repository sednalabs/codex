#!/usr/bin/env python3
"""Reconcile the candidate workspace lockfile from one immutable upstream commit.

This is intentionally a small hosted-only helper.  It fetches one public commit,
checks that Git resolved the requested object, seeds the candidate lockfile with
the upstream lock, and asks Cargo's ordinary metadata resolution to validate or
update it. This deliberately does not invoke generate-lockfile or update-all.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from collections import Counter
from pathlib import Path
from typing import Any

SHA40 = re.compile(r"^[0-9a-fA-F]{40}$")
UPSTREAM_URL = "https://github.com/openai/codex.git"


class ReconcileError(Exception):
    pass


def run(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    env = dict(os.environ)
    env["GIT_TERMINAL_PROMPT"] = "0"
    process = subprocess.run(
        ["git", "-C", str(repo), *args],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        check=False,
    )
    if check and process.returncode:
        raise ReconcileError(f"git {args[0]} failed (exit {process.returncode})")
    return process


def sha(value: str, label: str) -> str:
    if not SHA40.fullmatch(value):
        raise ReconcileError(f"invalid {label} SHA: expected 40 hexadecimal characters")
    return value.lower()


def lock_path(repo: Path) -> Path:
    for relative in (Path("codex-rs/Cargo.lock"), Path("Cargo.lock")):
        if (repo / relative).is_file() or (repo / relative.parent / "Cargo.toml").is_file():
            return repo / relative
    raise ReconcileError("candidate workspace has no Cargo.toml/Cargo.lock target")


def package_entries(path: Path) -> list[tuple[str, str, str]]:
    # Cargo.lock is TOML; tomllib is in the hosted Python used by ubuntu-latest.
    try:
        import tomllib
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise ReconcileError(f"unable to parse Cargo.lock: {error}") from error
    entries: list[tuple[str, str, str]] = []
    for package in data.get("package", []):
        name, source, version = package.get("name"), package.get("source", ""), package.get("version")
        if isinstance(name, str) and isinstance(version, str):
            entries.append((name, version, str(source)))
    return sorted(entries)


def cargo_metadata_command(manifest: Path) -> list[str]:
    return ["cargo", "metadata", "--format-version", "1", "--manifest-path", str(manifest)]


def write_report(path: Path, report: dict[str, Any]) -> None:
    path.write_text(json.dumps(report, sort_keys=True, indent=2) + "\n", encoding="utf-8")


def diagnostic(stderr: str) -> str:
    lines = stderr.strip().splitlines()
    if len(lines) <= 8:
        return " ".join(lines)
    return " ".join(lines[:4] + ["...", *lines[-4:]])


def reconcile(args: argparse.Namespace) -> dict[str, Any]:
    repo = Path(args.workspace).resolve()
    if not (repo / ".git").exists():
        raise ReconcileError("candidate workspace is not a Git checkout")
    candidate = sha(args.candidate, "candidate")
    upstream = sha(args.upstream, "upstream")
    actual_candidate = run(repo, "rev-parse", "--verify", "HEAD").stdout.strip().lower()
    if actual_candidate != candidate:
        raise ReconcileError(f"candidate checkout mismatch: expected {candidate}, got {actual_candidate}")
    target = lock_path(repo)
    upstream_lock = Path(args.temp_dir).resolve() / "upstream-Cargo.lock"
    fetch = run(repo, "fetch", "--no-tags", "--depth=1", UPSTREAM_URL, upstream, check=False)
    if fetch.returncode:
        raise ReconcileError(f"unable to retrieve requested upstream commit (exit {fetch.returncode})")
    actual_upstream = run(repo, "rev-parse", "--verify", "FETCH_HEAD").stdout.strip().lower()
    if actual_upstream != upstream:
        raise ReconcileError(f"upstream source mismatch: expected {upstream}, got {actual_upstream}")
    source_spec = f"{upstream}:codex-rs/Cargo.lock"
    source = run(repo, "show", source_spec, check=False)
    if source.returncode:
        source_spec = f"{upstream}:Cargo.lock"
        source = run(repo, "show", source_spec, check=False)
    if source.returncode or not source.stdout:
        raise ReconcileError("requested upstream commit does not contain Cargo.lock")
    upstream_lock.write_text(source.stdout, encoding="utf-8")
    source_digest = hashlib.sha256(source.stdout.encode()).hexdigest()
    before = package_entries(upstream_lock)
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(upstream_lock, target)
    cargo = subprocess.run(
        cargo_metadata_command(target.parent / "Cargo.toml"),
        cwd=target.parent,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if cargo.returncode:
        detail = diagnostic(cargo.stderr)
        raise ReconcileError(
            f"cargo metadata failed (exit {cargo.returncode}): {detail or 'no diagnostic output'}"
        )
    after = package_entries(target)
    before_counts, after_counts = Counter(before), Counter(after)
    retained = sorted(entry for entry in before_counts if entry in after_counts)
    removed = sorted(entry for entry, count in before_counts.items() if count > after_counts.get(entry, 0))
    added = sorted(entry for entry, count in after_counts.items() if count > before_counts.get(entry, 0))
    return {
        "version": 1,
        "status": "reconciled",
        "candidate": candidate,
        "upstream": upstream,
        "upstream_source": {"repository": UPSTREAM_URL, "commit": actual_upstream, "path": source_spec.split(":", 1)[1], "sha256": source_digest},
        "candidate_lockfile": str(target.relative_to(repo)),
        "candidate_lock_sha256": hashlib.sha256(target.read_bytes()).hexdigest(),
        "cargo": {"command": cargo_metadata_command(target.parent / "Cargo.toml"), "exit_code": cargo.returncode},
        "package_entries": {
            "retained": [{"name": n, "version": v, "source": s, "count": min(before_counts[(n, v, s)], after_counts[(n, v, s)])} for n, v, s in retained],
            "removed": [{"name": n, "version": v, "source": s, "count": before_counts[(n, v, s)] - after_counts[(n, v, s)]} for n, v, s in removed],
            "added": [{"name": n, "version": v, "source": s, "count": after_counts[(n, v, s)] - before_counts[(n, v, s)]} for n, v, s in added],
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--upstream", required=True)
    parser.add_argument("--workspace", default=os.getcwd())
    parser.add_argument("--report", default="upstream-lock-reconciliation.json")
    args = parser.parse_args()
    report_path = Path(args.report)
    temp_dir = Path(args.workspace) / ".upstream-lock-reconcile"
    temp_dir.mkdir(exist_ok=True)
    args.temp_dir = str(temp_dir)
    try:
        report = reconcile(args)
        write_report(report_path, report)
        return 0
    except ReconcileError as error:
        write_report(report_path, {"version": 1, "status": "failed", "error": str(error)})
        print(f"upstream lock reconciliation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
