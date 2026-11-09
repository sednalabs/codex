#!/usr/bin/env python3
"""Construct disposable state schemas from public migration SQL.

This checks fixture provenance and schema effects. It does not invoke the
Rust migrator or qualify a historical upgrade.
"""

import hashlib
import json
from pathlib import Path
import sqlite3
import unittest


HERE = Path(__file__).resolve().parent
UPSTREAM_MIGRATIONS = HERE.parent.parent / "migrations"
SEDNA = json.loads((HERE / "w14939-sedna-migrations.json").read_text())
SOURCES = {name: json.loads((HERE / f"w14939-{name}-migrations.json").read_text())
           for name in ("oci-source", "workstation-source")}
THREAD_HISTORY = json.loads((HERE / "w14939-thread-history-migrations.json").read_text())


def entries(source):
    if source == "sedna":
        return SEDNA["migrations"]
    if source in SOURCES:
        return SOURCES[source]["migrations"]
    return [
        {"filename": path.name, "sha384": hashlib.sha384(path.read_bytes()).hexdigest(),
         "sql": path.read_text()}
        for path in sorted(UPSTREAM_MIGRATIONS.glob("*.sql"))
    ]


def history(source, through):
    db = sqlite3.connect(":memory:")
    db.execute("""CREATE TABLE _sqlx_migrations (
        version BIGINT PRIMARY KEY, description TEXT NOT NULL,
        installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
        success BOOLEAN NOT NULL, checksum BLOB NOT NULL,
        execution_time BIGINT NOT NULL)""")
    for entry in entries(source):
        filename = entry["filename"]
        version = int(filename.split("_", 1)[0])
        if version > through:
            break
        sql = entry["sql"].encode()
        assert hashlib.sha384(sql).hexdigest() == entry["sha384"]
        db.executescript(entry["sql"])
        description = Path(filename).stem.split("_", 1)[1].replace("_", " ")
        db.execute("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 0)",
                   (version, description, bytes.fromhex(entry["sha384"])))
        db.commit()
    db.execute("""INSERT INTO threads
        (id, rollout_path, created_at, updated_at, source, model_provider, cwd,
         title, sandbox_policy, approval_mode)
        VALUES ('synthetic-thread', '/synthetic.jsonl', 1, 2, 'cli', 'test',
                '/synthetic', 'preserve-me', 'read-only', 'never')""")
    db.commit()
    return db


class MigrationHistories(unittest.TestCase):
    def test_recorded_source_lineages_are_upstream_prefixes(self):
        expected = {
            "oci-source": "1dea691446732a0ab03836f40f932387d9b1e087",
            "workstation-source": "e03a40398272ee4b0e6cdb694e390498c86ac7d9",
        }
        for source, commit in expected.items():
            with self.subTest(source=source):
                self.assertEqual(SOURCES[source]["source_commit"], commit)
                self.assertEqual(len(entries(source)), 62)
                for through in (39, 45, 62):
                    actual = history(source, through)
                    upstream = history("upstream", through)
                    query = "SELECT version, description, success, checksum FROM _sqlx_migrations ORDER BY version"
                    self.assertEqual(actual.execute(query).fetchall(), upstream.execute(query).fetchall())
                    schema_query = "SELECT type, name, sql FROM sqlite_master ORDER BY type, name"
                    self.assertEqual(actual.execute(schema_query).fetchall(), upstream.execute(schema_query).fetchall())
                    self.assertEqual(actual.execute("SELECT title FROM threads WHERE id = 'synthetic-thread'").fetchone(), ("preserve-me",))
                    actual.close()
                    upstream.close()
                by_version = {int(m["filename"].split("_")[0]): m for m in entries(source)}
                self.assertEqual(by_version[39]["filename"], "0039_threads_recency_at.sql")
                self.assertEqual(by_version[45]["filename"], "0045_threads_section.sql")
                self.assertTrue(by_version[45]["sha384"].startswith("a7fadc8c"))

    def test_recorded_thread_history_schemas_and_rows_survive_migrations(self):
        self.assertEqual(THREAD_HISTORY["source_commits"], [
            "1dea691446732a0ab03836f40f932387d9b1e087",
            "e03a40398272ee4b0e6cdb694e390498c86ac7d9",
        ])
        entries = THREAD_HISTORY["migrations"]
        self.assertEqual(len(entries), 8)
        for entry in entries:
            sql = entry["sql"].encode()
            self.assertEqual(hashlib.sha384(sql).hexdigest(), entry["sha384"])
            self.assertEqual(sql, (HERE.parent.parent / "thread_history_migrations" / entry["filename"]).read_bytes())
        for through in (4, 8):
            with self.subTest(through=through):
                db = sqlite3.connect(":memory:")
                for entry in entries[:through]:
                    db.executescript(entry["sql"])
                db.execute("INSERT INTO thread_turns (thread_id, turn_id, rollout_ordinal, status) VALUES ('synthetic-thread', 'turn-1', 10, 'completed')")
                db.execute("INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, updated_at_ordinal, created_at_ms, item_type, item_json) VALUES ('synthetic-thread', 'turn-1', 'item-1', 11, 11, 1000, 'userMessage', '{\"text\":\"preserve-me\"}')")
                db.execute("INSERT INTO thread_history_projection_state VALUES ('synthetic-thread', 200, 12)")
                for entry in entries[through:]:
                    db.executescript(entry["sql"])
                self.assertEqual(db.execute("SELECT turn_id, status FROM thread_turns").fetchone(), ("turn-1", "completed"))
                self.assertEqual(db.execute("SELECT item_id, item_json FROM thread_items").fetchone(), ("item-1", '{"text":"preserve-me"}'))
                self.assertEqual(db.execute("SELECT next_rollout_byte_offset, next_rollout_ordinal FROM thread_history_projection_state").fetchone(), (200, 12))
                db.close()

    def test_collision_histories_and_schema_effects(self):
        self.assertEqual(SEDNA["source_commit"], "97f6c6b811cbda5aa618356e6761c541da2b5032")
        expected_fork = {
            39: "d7b56acb07c96858bdc7ca4c53db51eb71c6a19ef2c524ff68c2660a6fe0d23211a6d39cb905f01c42ffb7c8ee913bbc",
            45: "dd42ba9960d4e01c554bfac6a27c67f3c21e80bdc8e0dd928a90d208e1e680976a3ea356a9f9360f9760a5ca42659053",
        }
        for version, expected in expected_fork.items():
            with self.subTest(version=version):
                fork = history("sedna", version)
                upstream = history("upstream", version)
                fork_rows = dict(fork.execute("SELECT version, checksum FROM _sqlx_migrations"))
                upstream_rows = dict(upstream.execute("SELECT version, checksum FROM _sqlx_migrations"))
                self.assertEqual(fork_rows[version].hex(), expected)
                self.assertNotEqual(fork_rows[version], upstream_rows[version])
                self.assertEqual(next(v for v in sorted(fork_rows) if fork_rows[v] != upstream_rows[v]), 24)
                self.assertEqual(fork.execute("SELECT title FROM threads WHERE id = 'synthetic-thread'").fetchone(), ("preserve-me",))
                columns = {row[1] for row in fork.execute("PRAGMA table_info(threads)")}
                if version == 39:
                    self.assertNotIn("recency_at", columns)
                else:
                    self.assertIn("configured_identity_provenance", columns)
                    self.assertIn("recency_at", columns)
                    self.assertNotIn("thread_section_id", columns)
                fork.close()
                upstream.close()


if __name__ == "__main__":
    unittest.main()
