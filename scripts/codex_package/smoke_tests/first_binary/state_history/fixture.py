"""Generate privacy-safe complete historical state ledgers and schemas.

Only hosted tests may call this module. It writes exclusively to the explicit
synthetic CODEX_HOME passed by the package harness, never to a real profile.
"""

from __future__ import annotations

import hashlib
import json
import sqlite3
from contextlib import closing
from dataclasses import dataclass
from pathlib import Path

from .sources import OLD_FORK_TO_NEW, SHIFTED_TO_CANONICAL, HistoricalSources

PARENT_ID = "00000000-0000-0000-0000-000000000101"
CHILD_ID = "00000000-0000-0000-0000-000000000102"
STATE_DB = "state_5.sqlite"


@dataclass(frozen=True)
class MigrationStep:
    recorded_version: int
    canonical_version: int
    execute_sql: bool = True


@dataclass(frozen=True)
class FixtureEvidence:
    case_name: str
    codex_home: Path
    state_path: Path
    source_root: Path
    historical_rows: int
    expected_rekeys: tuple[tuple[int, int], ...]
    marked_alias_rows: int
    seed_thread_rows: int
    seed_phase2_baseline_rows: int
    seed_phase2_root_rows: int
    pre_start_semantic_sha256: str | None
    known_bad_collision_detected: bool
    known_bad_collision_rows: int
    negative: bool


def _upstream(last: int) -> list[MigrationStep]:
    return [MigrationStep(version, version) for version in range(1, last + 1)]


def _fork_through(last: int) -> list[MigrationStep]:
    return [
        MigrationStep(old, OLD_FORK_TO_NEW[old])
        for old in (56, 57, 58)
        if old <= last
    ]


def _shifted_through(last: int) -> list[MigrationStep]:
    return _upstream(23) + [
        MigrationStep(old, SHIFTED_TO_CANONICAL[old])
        for old in range(24, last + 1)
    ]


def positive_steps(case_name: str) -> list[MigrationStep]:
    if case_name == "fresh":
        return []
    if case_name == "u23":
        return _upstream(23)
    if case_name == "u55":
        return _upstream(55)
    if case_name in {"u56", "u57", "u58"}:
        return _upstream(int(case_name[1:]))
    if case_name in {"f56", "f57", "f58"}:
        return _upstream(55) + _fork_through(int(case_name[1:]))
    if case_name == "f_full":
        return (
            _upstream(55)
            + _fork_through(58)
            + [MigrationStep(9000, OLD_FORK_TO_NEW[9000])]
            + [
                MigrationStep(9001, OLD_FORK_TO_NEW[9001], execute_sql=False),
                MigrationStep(9002, OLD_FORK_TO_NEW[9002], execute_sql=False),
            ]
            + [MigrationStep(9003, OLD_FORK_TO_NEW[9003])]
        )
    if case_name == "f_alias_pair":
        return _upstream(55) + [
            MigrationStep(9001, OLD_FORK_TO_NEW[9001]),
            MigrationStep(9002, OLD_FORK_TO_NEW[9002]),
        ]
    if case_name.startswith("shift"):
        last = int(case_name.removeprefix("shift"))
        if last in {24, 29, 38, 45, 50}:
            return _shifted_through(last)
    raise ValueError(f"unknown positive historical state case {case_name}")


POSITIVE_CASES = (
    "fresh",
    "u23",
    "u55",
    "u56",
    "u57",
    "u58",
    "f56",
    "f57",
    "f58",
    "f_full",
    "f_alias_pair",
    "shift24",
    "shift29",
    "shift38",
    "shift45",
    "shift50",
)
NEGATIVE_CASES = (
    "bad_checksum",
    "mixed_ids",
    "missing_middle",
    "failed_row",
    "incomplete_f58",
    "partial_alias",
    "partial_upstream_schema",
    "unknown_id",
)


def _negative_baseline(case_name: str) -> list[MigrationStep]:
    if case_name in {"bad_checksum", "missing_middle", "failed_row"}:
        return _upstream(58)
    if case_name == "mixed_ids":
        return _upstream(55) + _fork_through(56) + [MigrationStep(57, 57)]
    if case_name == "incomplete_f58":
        return (
            _upstream(55)
            + _fork_through(57)
            + [MigrationStep(58, OLD_FORK_TO_NEW[58], execute_sql=False)]
            + [
                MigrationStep(9001, OLD_FORK_TO_NEW[9001]),
                MigrationStep(9002, OLD_FORK_TO_NEW[9002]),
            ]
        )
    if case_name == "partial_alias":
        return _upstream(55) + [
            MigrationStep(9001, OLD_FORK_TO_NEW[9001], execute_sql=False)
        ]
    if case_name in {"partial_upstream_schema", "unknown_id"}:
        return _upstream(55)
    raise ValueError(f"unknown negative historical state case {case_name}")


def _create_ledger(db: sqlite3.Connection) -> None:
    db.execute(
        """CREATE TABLE _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        )"""
    )
    db.commit()


def _seed_rows(db: sqlite3.Connection) -> None:
    for thread_id, title, source in [
        (PARENT_ID, "historical parent", "cli"),
        (
            CHILD_ID,
            "historical child",
            json.dumps(
                {"subagent": {"thread_spawn": {"parent_thread_id": PARENT_ID}}},
                separators=(",", ":"),
            ),
        ),
    ]:
        db.execute(
            """INSERT INTO threads (
                id, rollout_path, created_at, updated_at, source,
                model_provider, cwd, title, sandbox_policy, approval_mode,
                first_user_message
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
            (
                thread_id,
                f"/synthetic/{thread_id}.jsonl",
                1_700_000_000,
                1_700_000_001,
                source,
                "synthetic-provider",
                "/synthetic",
                title,
                "read-only",
                "on-request",
                f"{title} message",
            ),
        )
    db.execute(
        """INSERT INTO thread_dynamic_tools
           (thread_id, position, name, description, input_schema)
           VALUES (?, 0, 'historical_tool', 'synthetic description', '{"type":"object"}')""",
        (CHILD_ID,),
    )
    db.commit()


def _record(db: sqlite3.Connection, step: MigrationStep, sources: HistoricalSources) -> None:
    script = sources.script(step.canonical_version)
    if step.execute_sql:
        db.executescript(script.content.decode("utf-8"))
        if step.canonical_version == OLD_FORK_TO_NEW[56]:
            db.execute(
                """INSERT INTO phase2_attested_baselines (
                    memory_root_key, output_tree_sha256, schema_version,
                    selection_sha256, prepared_inputs_sha256, consolidator_sha256,
                    completion_watermark, selected_count, attested_at
                ) VALUES ('synthetic-root', 'synthetic-output', 1, 'synthetic-selection',
                          'synthetic-inputs', 'synthetic-consolidator', 7, 2, 1700000002)"""
            )
        if step.canonical_version == OLD_FORK_TO_NEW[9000]:
            db.execute(
                """INSERT INTO phase2_attestation_roots
                   (memory_root_key, required_since, updated_at)
                   VALUES ('synthetic-root', 1700000000, 1700000002)"""
            )
    db.execute(
        """INSERT INTO _sqlx_migrations
           (version, description, success, checksum, execution_time)
           VALUES (?, ?, TRUE, ?, 0)""",
        (step.recorded_version, script.description, script.checksum),
    )
    db.commit()


def _damage(db: sqlite3.Connection, case_name: str, sources: HistoricalSources) -> None:
    if case_name == "bad_checksum":
        db.execute(
            "UPDATE _sqlx_migrations SET checksum = X'01' WHERE version = 56"
        )
    elif case_name == "missing_middle":
        db.execute("DELETE FROM _sqlx_migrations WHERE version = 27")
    elif case_name == "failed_row":
        db.execute("UPDATE _sqlx_migrations SET success = FALSE WHERE version = 56")
    elif case_name == "partial_upstream_schema":
        db.execute("ALTER TABLE threads ADD COLUMN creator_user_id TEXT")
    elif case_name == "unknown_id":
        db.execute(
            """INSERT INTO _sqlx_migrations
               (version, description, success, checksum, execution_time)
               VALUES (9004, 'unrecognized synthetic migration', TRUE, X'01', 0)"""
        )
    elif case_name in {"mixed_ids", "incomplete_f58", "partial_alias"}:
        # The baseline sequence itself is the controlled inconsistent state.
        pass
    else:
        raise ValueError(case_name)
    db.commit()


def semantic_snapshot(state_path: Path) -> str | None:
    """Hash schema and user/ledger rows, excluding SQLite journal representation."""
    if not state_path.exists():
        return None
    with closing(sqlite3.connect(f"file:{state_path}?mode=ro", uri=True)) as db:
        dump = "\n".join(db.iterdump()).encode()
    return hashlib.sha256(dump).hexdigest()


def _known_bad_collision_rows(
    state_path: Path,
    steps: list[MigrationStep],
    sources: HistoricalSources,
) -> int:
    """Prove actual fixture ledger rows mismatch the unbridged U versions."""
    if not state_path.exists():
        return 0
    count = 0
    with closing(sqlite3.connect(f"file:{state_path}?mode=ro", uri=True)) as db:
        for step in steps:
            if not 1 <= step.recorded_version <= 58:
                continue
            if step.recorded_version == step.canonical_version:
                continue
            row = db.execute(
                "SELECT checksum FROM _sqlx_migrations WHERE version = ?",
                (step.recorded_version,),
            ).fetchone()
            if row is None:
                # A negative missing-row fixture is checked by its own control.
                continue
            actual = bytes(row[0])
            historical = sources.script(step.canonical_version).checksum
            upstream = sources.script(step.recorded_version).checksum
            if actual == historical and actual != upstream:
                count += 1
    return count


def prepare_case(
    source_root: Path,
    codex_home: Path,
    case_name: str,
) -> FixtureEvidence:
    """Create exactly one new synthetic state DB, or an absent DB for fresh."""
    negative = case_name in NEGATIVE_CASES
    if case_name not in POSITIVE_CASES and not negative:
        raise ValueError(case_name)
    sources = HistoricalSources(source_root)
    codex_home = codex_home.resolve()
    codex_home.mkdir(parents=True, exist_ok=True)
    state_path = codex_home / STATE_DB
    if state_path.exists():
        raise AssertionError(f"refusing to overwrite existing state DB: {state_path}")
    steps = _negative_baseline(case_name) if negative else positive_steps(case_name)
    if steps:
        with closing(sqlite3.connect(state_path)) as db:
            _create_ledger(db)
            for step in steps:
                _record(db, step, sources)
                if step.recorded_version == 23:
                    _seed_rows(db)
            if negative:
                _damage(db, case_name, sources)
    elif negative:
        raise AssertionError("negative fixture must have a complete baseline")
    expected_rekeys = tuple(
        (step.recorded_version, step.canonical_version)
        for step in steps
        if step.recorded_version != step.canonical_version
    )
    collision_rows = _known_bad_collision_rows(state_path, steps, sources)
    has_unrekeyed_low_history = any(
        1 <= step.recorded_version <= 58
        and step.recorded_version != step.canonical_version
        for step in steps
    )
    if has_unrekeyed_low_history and collision_rows == 0 and not negative:
        raise AssertionError("known-bad unbridged collision detector did not fire")
    return FixtureEvidence(
        case_name=case_name,
        codex_home=codex_home,
        state_path=state_path,
        source_root=source_root.resolve(),
        historical_rows=len(steps),
        expected_rekeys=expected_rekeys,
        marked_alias_rows=sum(not step.execute_sql for step in steps),
        seed_thread_rows=0 if not steps else 2,
        seed_phase2_baseline_rows=sum(
            step.canonical_version == OLD_FORK_TO_NEW[56] and step.execute_sql
            for step in steps
        ),
        seed_phase2_root_rows=sum(
            step.canonical_version == OLD_FORK_TO_NEW[9000] and step.execute_sql
            for step in steps
        ),
        pre_start_semantic_sha256=semantic_snapshot(state_path),
        known_bad_collision_detected=collision_rows > 0,
        known_bad_collision_rows=collision_rows,
        negative=negative,
    )
