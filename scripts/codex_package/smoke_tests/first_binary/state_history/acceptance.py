"""Assertions for the real packaged-startup/reopen state consumer join."""

from __future__ import annotations

import sqlite3
from contextlib import closing
from dataclasses import dataclass
from pathlib import Path

from .fixture import CHILD_ID, PARENT_ID, FixtureEvidence, semantic_snapshot
from .sources import FORK_BASE, HistoricalSources


@dataclass(frozen=True)
class StateWitness:
    case_name: str
    phase: str
    canonical_ledger_rows: int
    fork_effects: int
    preserved_thread_rows: int
    preserved_dynamic_tools: int
    preserved_spawn_edges: int
    preserved_phase2_baselines: int
    preserved_phase2_roots: int
    rekey_receipts: int
    known_bad_collision_detected: bool
    ledger_identity: tuple[tuple[int, str, bool, str], ...]
    receipt_identity: tuple[tuple[int, int, str], ...]


def _has_table(db: sqlite3.Connection, name: str) -> bool:
    return (
        db.execute(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?", (name,)
        ).fetchone()
        is not None
    )


def _has_index(db: sqlite3.Connection, name: str) -> bool:
    return (
        db.execute(
            "SELECT 1 FROM sqlite_schema WHERE type = 'index' AND name = ?", (name,)
        ).fetchone()
        is not None
    )


def _columns(db: sqlite3.Connection, table: str) -> set[str]:
    return {row[1] for row in db.execute(f"PRAGMA table_info({table})")}


def assert_positive_state(evidence: FixtureEvidence, phase: str) -> StateWitness:
    """Prove the packaged process consumed the pinned historical state DB."""
    if evidence.negative:
        raise AssertionError("positive assertion called on negative fixture")
    if phase not in {"first_open", "second_open"}:
        raise ValueError(phase)
    sources = HistoricalSources(evidence.source_root)
    if not evidence.state_path.is_file():
        raise AssertionError("packaged startup did not create the state DB")
    with closing(
        sqlite3.connect(f"file:{evidence.state_path}?mode=ro", uri=True)
    ) as db:
        ledger_identity = tuple(
            (int(version), str(description), bool(success), bytes(checksum).hex())
            for version, description, success, checksum in db.execute(
                "SELECT version, description, success, checksum "
                "FROM _sqlx_migrations ORDER BY version"
            )
        )
        actual = {row[0]: row for row in ledger_identity}
        expected_versions = set(range(1, 59)) | set(range(FORK_BASE, FORK_BASE + 7))
        if set(actual) != expected_versions:
            raise AssertionError(
                f"state migration set incomplete: missing={sorted(expected_versions - set(actual))}, "
                f"extra={sorted(set(actual) - expected_versions)}"
            )
        for version in expected_versions:
            _, description, success, checksum = actual[version]
            script = sources.script(version)
            if not success or description != script.description or checksum != script.checksum.hex():
                raise AssertionError(f"canonical state migration {version} differs")

        for table in ("phase2_attested_baselines", "phase2_attestation_roots"):
            if not _has_table(db, table):
                raise AssertionError(f"fork effect table missing: {table}")
        baseline_rows = tuple(
            db.execute(
                "SELECT memory_root_key, output_tree_sha256, schema_version, "
                "selection_sha256, prepared_inputs_sha256, consolidator_sha256, "
                "completion_watermark, selected_count, attested_at "
                "FROM phase2_attested_baselines ORDER BY memory_root_key"
            )
        )
        expected_baselines = (
            (
                "synthetic-root", "synthetic-output", 1, "synthetic-selection",
                "synthetic-inputs", "synthetic-consolidator", 7, 2, 1_700_000_002,
            ),
        ) if evidence.seed_phase2_baseline_rows else ()
        if baseline_rows != expected_baselines:
            raise AssertionError("historical phase-two baseline data changed")
        root_rows = tuple(
            db.execute(
                "SELECT memory_root_key, required_since, updated_at "
                "FROM phase2_attestation_roots ORDER BY memory_root_key"
            )
        )
        expected_roots = (
            ("synthetic-root", 1_700_000_000, 1_700_000_002),
        ) if evidence.seed_phase2_root_rows else ()
        if root_rows != expected_roots:
            raise AssertionError("historical phase-two root data changed")
        thread_columns = _columns(db, "threads")
        if not {
            "configured_identity_provenance",
            "creator_user_id",
            "creator_account_id",
        } <= thread_columns:
            raise AssertionError("fork/upstream thread identity columns did not coexist")
        dynamic_columns = _columns(db, "thread_dynamic_tools")
        if not {
            "persist_on_resume",
            "capability_json",
            "namespace_description",
        } <= dynamic_columns:
            raise AssertionError("all three dynamic-tool state columns are required")
        for index in (
            "idx_threads_archive_created_at_ms",
            "idx_threads_archive_updated_at_ms",
            "idx_threads_archive_recency_at_ms",
        ):
            if not _has_index(db, index):
                raise AssertionError(f"upstream archive index missing: {index}")

        preserved_threads = 0
        preserved_tools = 0
        preserved_edges = 0
        if evidence.seed_thread_rows:
            threads = {
                str(thread_id): (
                    str(title),
                    str(first_user_message),
                    int(provenance),
                )
                for thread_id, title, first_user_message, provenance in db.execute(
                    "SELECT id, title, first_user_message, configured_identity_provenance "
                    "FROM threads WHERE id IN (?, ?)",
                    (PARENT_ID, CHILD_ID),
                )
            }
            if threads != {
                PARENT_ID: ("historical parent", "historical parent message", 0),
                CHILD_ID: ("historical child", "historical child message", 0),
            }:
                raise AssertionError(f"historical thread data changed: {threads}")
            preserved_threads = len(threads)
            tool = db.execute(
                "SELECT name, description, input_schema, persist_on_resume, "
                "capability_json, namespace_description "
                "FROM thread_dynamic_tools WHERE thread_id = ? AND position = 0",
                (CHILD_ID,),
            ).fetchone()
            if tool != (
                "historical_tool",
                "synthetic description",
                '{"type":"object"}',
                1,
                None,
                None,
            ):
                raise AssertionError(f"historical dynamic tool changed: {tool}")
            preserved_tools = 1
            edge = db.execute(
                "SELECT status FROM thread_spawn_edges "
                "WHERE parent_thread_id = ? AND child_thread_id = ?",
                (PARENT_ID, CHILD_ID),
            ).fetchone()
            if edge != ("open",):
                raise AssertionError(f"historical spawn edge missing: {edge}")
            preserved_edges = 1

        receipts = ()
        if _has_table(db, "state_migration_rekey_receipts"):
            receipts = tuple(
                (int(old), int(new), bytes(checksum).hex())
                for old, new, checksum in db.execute(
                    "SELECT old_version, new_version, checksum "
                    "FROM state_migration_rekey_receipts ORDER BY old_version"
                )
            )
        receipt_by_old = {old: (new, checksum) for old, new, checksum in receipts}
        for old, target in evidence.expected_rekeys:
            expected_checksum = sources.script(target).checksum.hex()
            if receipt_by_old.get(old) != (target, expected_checksum):
                raise AssertionError(f"historical mapping receipt {old}->{target} missing")
        if len(receipts) != len(evidence.expected_rekeys):
            raise AssertionError(
                f"unexpected rekey receipt count: {len(receipts)} != {len(evidence.expected_rekeys)}"
            )
    return StateWitness(
        case_name=evidence.case_name,
        phase=phase,
        canonical_ledger_rows=len(ledger_identity),
        fork_effects=7,
        preserved_thread_rows=preserved_threads,
        preserved_dynamic_tools=preserved_tools,
        preserved_spawn_edges=preserved_edges,
        preserved_phase2_baselines=len(baseline_rows),
        preserved_phase2_roots=len(root_rows),
        rekey_receipts=len(receipts),
        known_bad_collision_detected=evidence.known_bad_collision_detected,
        ledger_identity=ledger_identity,
        receipt_identity=receipts,
    )


def assert_rejected_without_mutation(evidence: FixtureEvidence) -> str:
    """Negative startup must reject before any state schema or user-data write."""
    if not evidence.negative or evidence.pre_start_semantic_sha256 is None:
        raise AssertionError("a negative fixture with a persisted preimage is required")
    current = semantic_snapshot(evidence.state_path)
    if current != evidence.pre_start_semantic_sha256:
        raise AssertionError(
            f"negative {evidence.case_name} modified state: "
            f"{evidence.pre_start_semantic_sha256} -> {current}"
        )
    return current


def assert_idempotent_reopen(first: StateWitness, second: StateWitness) -> None:
    if first.case_name != second.case_name:
        raise AssertionError("reopen witnesses are from different fixtures")
    if first.phase != "first_open" or second.phase != "second_open":
        raise AssertionError("reopen witnesses are not ordered")
    if first.ledger_identity != second.ledger_identity:
        raise AssertionError("second packaged open changed migration ledger")
    if first.receipt_identity != second.receipt_identity:
        raise AssertionError("second packaged open changed rekey mapping receipts")
    if first.preserved_thread_rows != second.preserved_thread_rows:
        raise AssertionError("second packaged open lost historical threads")
    if first.preserved_dynamic_tools != second.preserved_dynamic_tools:
        raise AssertionError("second packaged open lost historical dynamic tool")
    if first.preserved_spawn_edges != second.preserved_spawn_edges:
        raise AssertionError("second packaged open lost historical spawn edge")
    if first.preserved_phase2_baselines != second.preserved_phase2_baselines:
        raise AssertionError("second packaged open lost phase-two baseline data")
    if first.preserved_phase2_roots != second.preserved_phase2_roots:
        raise AssertionError("second packaged open lost phase-two root data")
