#!/usr/bin/env python3
"""Fail closed on a changed frozen state-migration identity or namespace map.

Run this on the exact candidate in standard GitHub-hosted validation. It is a
source guard, not a substitute for SQLx migration and reopen fixtures.
"""

import hashlib
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent
MIGRATIONS = ROOT / "migrations"
MANIFEST = ROOT / "migration_namespace_manifest.txt"
BRIDGE = ROOT / "src" / "runtime" / "migration_repair.rs"
FORK_BASE = 8_000_000_000_000_000
EXPECTED_OLD = (56, 57, 58, 9000, 9001, 9002, 9003)


def git_blob_sha1(data: bytes) -> str:
    return hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()


def main() -> None:
    expected: dict[str, tuple[str, str]] = {}
    versions: dict[int, str] = {}
    for raw_line in MANIFEST.read_text().splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        kind, name, digest = line.split()
        if kind not in {"u", "f"} or name in expected:
            raise ValueError(f"duplicate or invalid manifest entry: {name}")
        match = re.fullmatch(r"(\d+)_.*\.sql", name)
        if match is None:
            raise ValueError(f"invalid migration filename: {name}")
        version = int(match.group(1))
        if version in versions:
            raise ValueError(f"duplicate state migration version {version}")
        if kind == "u" and not 1 <= version <= 60:
            raise ValueError(f"upstream migration outside frozen range: {name}")
        if kind == "f" and not FORK_BASE <= version <= FORK_BASE + 6:
            raise ValueError(f"fork migration outside reviewed namespace: {name}")
        versions[version] = name
        expected[name] = (kind, digest)
    if set(versions) != set(range(1, 61)) | set(range(FORK_BASE, FORK_BASE + 7)):
        raise ValueError("frozen upstream or seven-fork migration mapping is incomplete")
    actual = {path.name for path in MIGRATIONS.glob("*.sql")}
    if actual != set(expected):
        raise ValueError(
            f"state migration file set differs: missing={sorted(set(expected) - actual)}, "
            f"extra={sorted(actual - set(expected))}"
        )
    for name, (kind, expected_digest) in expected.items():
        data = (MIGRATIONS / name).read_bytes()
        digest = git_blob_sha1(data) if kind == "u" else hashlib.sha384(data).hexdigest()
        if digest != expected_digest:
            raise ValueError(f"historical SQL bytes changed: {name}")
    source = BRIDGE.read_text()
    if "const FORK_BASE: i64 = 8_000_000_000_000_000;" not in source:
        raise ValueError("bridge fork namespace constant changed")
    for offset, old in enumerate(EXPECTED_OLD):
        symbol = f"FORK_{old}"
        definition = (
            f"const {symbol}: i64 = FORK_BASE;"
            if offset == 0
            else f"const {symbol}: i64 = FORK_BASE + {offset};"
        )
        if definition not in source or f"({old}, {symbol})" not in source:
            raise ValueError(f"missing bridge mapping for historical migration {old}")
    print("state migration namespace and historical SQL identities: OK")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError) as error:
        print(f"state migration namespace guard: {error}", file=sys.stderr)
        raise SystemExit(1) from error
