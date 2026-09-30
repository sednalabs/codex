#!/usr/bin/env python3
"""Serialize the accepted pre-migration-24 state fixture for package smoke."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sqlite3
from pathlib import Path


FIXTURE_SOURCE = (
    "codex-rs/state/src/runtime/migration_repair.rs::"
    "repairs_known_good_pre_migration_24_state_and_reopens_idempotently"
)
TARGET_MIGRATIONS = [9000, 24, 25, 9001, 9002, 26, 27, 28, 29]
HISTORICAL_ROWS = [
    (24, "phase2 attestation roots", 9000),
    (25, "remote control enrollments", 24),
    (26, "thread timestamps millis", 25),
    (27, "thread dynamic tools persist on resume", 9001),
    (28, "thread dynamic tools capability json", 9002),
    (29, "thread dynamic tools namespace", 26),
    (30, "threads cwd sort indexes", 27),
    (31, "device key bindings", 28),
    (32, "thread goals", 29),
]
MIGRATION_PATTERN = re.compile(r"^(\d+)_([a-z0-9_]+)\.sql$")


def migration_files(directory: Path) -> dict[int, tuple[str, bytes]]:
    files: dict[int, tuple[str, bytes]] = {}
    for path in directory.glob("*.sql"):
        match = MIGRATION_PATTERN.fullmatch(path.name)
        if not match:
            raise ValueError(f"unexpected state migration filename: {path.name}")
        version = int(match.group(1))
        if version in files:
            raise ValueError(f"duplicate state migration version: {version}")
        files[version] = (match.group(2).replace("_", " "), path.read_bytes())
    return files


def create_fixture(migrations_dir: Path, database_path: Path) -> dict[str, object]:
    migrations = migration_files(migrations_dir)
    required_versions = set(range(1, 24)) | set(TARGET_MIGRATIONS)
    missing = sorted(required_versions - migrations.keys())
    if missing:
        raise ValueError(f"required state migrations are missing: {missing}")
    if database_path.exists():
        raise FileExistsError(f"refusing to overwrite fixture database: {database_path}")
    database_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)

    connection = sqlite3.connect(database_path)
    try:
        # Mirrors ensure_migrations_table() in migration_repair.rs.
        connection.execute(
            """
            CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            )
            """
        )
        for version in sorted(v for v in migrations if v <= 23):
            description, sql = migrations[version]
            connection.executescript(sql.decode("utf-8"))
            connection.execute(
                """
                INSERT INTO _sqlx_migrations
                    (version, description, success, checksum, execution_time)
                VALUES (?, ?, TRUE, ?, 0)
                """,
                (version, description, hashlib.sha256(sql).digest()),
            )

        # These rows and values are copied from the immutable Rust fixture.
        connection.execute(
            """
            INSERT INTO threads (
                id, rollout_path, created_at, updated_at, source, model_provider, cwd,
                title, sandbox_policy, approval_mode, first_user_message
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                "thread-preserved",
                "/tmp/legacy.jsonl",
                1_700_000_000,
                1_700_000_001,
                "cli",
                "openai",
                "/tmp",
                "legacy title",
                "read-only",
                "on-request",
                "legacy first message",
            ),
        )
        connection.execute(
            """
            INSERT INTO thread_dynamic_tools (
                thread_id, position, name, description, input_schema, defer_loading
            ) VALUES (?, ?, ?, ?, ?, ?)
            """,
            (
                "thread-preserved",
                0,
                "legacy-tool",
                "legacy description",
                '{"type":"object"}',
                0,
            ),
        )

        for version in TARGET_MIGRATIONS:
            _description, sql = migrations[version]
            connection.executescript(sql.decode("utf-8"))
        for source_version, description, target_version in HISTORICAL_ROWS:
            _target_description, target_sql = migrations[target_version]
            connection.execute(
                """
                INSERT INTO _sqlx_migrations
                    (version, description, success, checksum, execution_time)
                VALUES (?, ?, TRUE, ?, 0)
                """,
                (
                    source_version,
                    description,
                    hashlib.sha256(target_sql).digest(),
                ),
            )
        connection.commit()

        actual_versions = [
            row[0]
            for row in connection.execute(
                "SELECT version FROM _sqlx_migrations ORDER BY version"
            )
        ]
        expected_versions = list(range(1, 24)) + [row[0] for row in HISTORICAL_ROWS]
        if actual_versions != expected_versions:
            raise AssertionError("generated legacy migration ledger differs from fixture")
        thread = connection.execute(
            "SELECT title, first_user_message FROM threads WHERE id = ?",
            ("thread-preserved",),
        ).fetchone()
        if thread != ("legacy title", "legacy first message"):
            raise AssertionError("generated legacy thread differs from fixture")
        return {
            "source": FIXTURE_SOURCE,
            "migration_versions_before_startup": actual_versions,
            "database_sha256": hashlib.sha256(database_path.read_bytes()).hexdigest(),
        }
    finally:
        connection.close()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--migrations-dir", type=Path, required=True)
    parser.add_argument("--database-path", type=Path, required=True)
    parser.add_argument("--receipt-path", type=Path, required=True)
    args = parser.parse_args()
    receipt = create_fixture(args.migrations_dir, args.database_path)
    args.receipt_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    args.receipt_path.write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n")


if __name__ == "__main__":
    main()
