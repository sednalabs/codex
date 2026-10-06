"""Hosted synthetic consumer and scale qualification for codex_usage_report.py."""

from __future__ import annotations

import argparse
import importlib.util
import io
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


START = "2026-09-30T00:00:00Z"
END = "2026-09-30T01:00:00Z"


def _connect_readonly(path: Path) -> sqlite3.Connection:
    return sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)


def _copy_database(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    source_connection = _connect_readonly(source)
    destination_connection = sqlite3.connect(destination)
    try:
        source_connection.backup(destination_connection)
    finally:
        destination_connection.close()
        source_connection.close()


def _report(
    database: Path,
    repo_root: Path,
    *,
    start: str = START,
    end: str = END,
    timezone: str = "UTC",
    scope: str = "all",
    thread_id: str | None = None,
    credit_mode: str = "both",
    diagnostics: bool = False,
    expected_exit: int | None = None,
) -> tuple[dict[str, Any], dict[str, Any]]:
    cli = repo_root / "scripts" / "codex_usage_report.py"
    argv = [
        sys.executable,
        str(cli),
        "--database",
        str(database.resolve()),
        "--start-utc",
        start,
        "--end-utc",
        end,
        "--timezone",
        timezone,
        "--scope",
        scope,
        "--credit-mode",
        credit_mode,
    ]
    if thread_id is not None:
        argv.extend(("--thread-id", thread_id))
    if diagnostics:
        argv.append("--diagnostics")
    env = {
        "PATH": os.environ.get("PATH", ""),
        "PYTHONNOUSERSITE": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
        "TZ": "UTC",
    }
    completed = subprocess.run(
        argv,
        cwd=repo_root,
        env=env,
        check=False,
        capture_output=True,
        text=True,
        timeout=8,
    )
    if expected_exit is not None and completed.returncode != expected_exit:
        raise AssertionError(
            f"reporter exit {completed.returncode} != {expected_exit}: {completed.stdout}\n{completed.stderr}"
        )
    try:
        report = json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        raise AssertionError(f"reporter did not return JSON: {completed.stdout}\n{completed.stderr}") from exc
    diagnostic: dict[str, Any] = {}
    for line in completed.stderr.splitlines():
        prefix = "CODEX_USAGE_REPORT_DIAGNOSTICS="
        if line.startswith(prefix):
            diagnostic = json.loads(line[len(prefix) :])
    return report, diagnostic


def _assert(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def _correctness(database: Path, repo_root: Path, temp_root: Path) -> dict[str, Any]:
    helper_spec = importlib.util.spec_from_file_location(
        "codex_usage_report", repo_root / "scripts" / "codex_usage_report.py"
    )
    if helper_spec is None or helper_spec.loader is None:
        raise AssertionError("could not import the exact tested reporter source")
    helper = importlib.util.module_from_spec(helper_spec)
    helper_spec.loader.exec_module(helper)
    original_output_limit = helper.MAX_OUTPUT_BYTES
    bounded_output = io.StringIO()
    helper.MAX_OUTPUT_BYTES = 128
    try:
        try:
            helper._write_bounded_json({"synthetic": "x" * 1_024}, bounded_output)
        except helper.WorkLimitReached as exc:
            _assert(exc.reason == "output_size_limit_exceeded", "JSON byte overflow must be typed incomplete")
        else:
            raise AssertionError("synthetic oversized JSON unexpectedly passed its output cap")
        _assert(bounded_output.getvalue() == "", "oversized JSON must be rejected before emitting partial output")
    finally:
        helper.MAX_OUTPUT_BYTES = original_output_limit
    writer_guard = helper.UsageReporter(
        database,
        helper.parse_utc(START),
        helper.parse_utc(END),
        helper.ZoneInfo("UTC"),
        "all",
        None,
        "both",
        False,
    )
    text_connection = sqlite3.connect(":memory:")
    try:
        oversized_text = text_connection.execute(
            "SELECT ?", ("x" * (helper.MAX_TEXT_FIELD_CHARS + 1),)
        ).fetchone()
        try:
            writer_guard._account_output_text(oversized_text)
        except helper.WorkLimitReached as exc:
            _assert(exc.reason == "output_size_limit_exceeded", "oversized source text must be typed incomplete")
        else:
            raise AssertionError("synthetic oversized source text unexpectedly passed its field cap")
        original_text_limit = helper.MAX_OUTPUT_TEXT_CHARS
        helper.MAX_OUTPUT_TEXT_CHARS = 3
        writer_guard._account_output_text(text_connection.execute("SELECT ?", ("ab",)).fetchone())
        try:
            writer_guard._account_output_text(text_connection.execute("SELECT ?", ("cd",)).fetchone())
        except helper.WorkLimitReached as exc:
            _assert(exc.reason == "output_size_limit_exceeded", "aggregate report text must be typed incomplete")
        else:
            raise AssertionError("synthetic aggregate output text unexpectedly passed its cap")
        finally:
            helper.MAX_OUTPUT_TEXT_CHARS = original_text_limit
    finally:
        text_connection.close()
    guarded_connection = writer_guard.open()
    try:
        guarded_connection.execute("DELETE FROM usage_provider_calls")
    except sqlite3.DatabaseError:
        pass
    else:
        raise AssertionError("reporter connection authorizer allowed a write")
    finally:
        guarded_connection.rollback()
        guarded_connection.close()

    all_report, all_diagnostic = _report(database, repo_root, diagnostics=True, expected_exit=2)
    _assert(all_report["status"] == "incomplete", "missing usage/model evidence must be incomplete")
    _assert(all_report["summary"]["provider_call_count"] == 4, "all scope must include exactly four in-window calls")
    _assert(all_report["summary"]["provider_reported_credits"]["call_count"] == 1, "reported credit count")
    _assert(all_report["summary"]["provider_reported_credits"]["credits_subtotal"] == "4.25", "reported credit subtotal")
    actual = all_report["summary"]["actual_mode"]
    _assert(actual["covered_call_count"] == 2, "reported and exact-rate actual-mode coverage")
    _assert(actual["rate_estimated_call_count"] == 1, "provider report must take precedence over a rate")
    _assert(actual["credits_subtotal"] == "174.25", "actual-mode subtotal")
    _assert(actual["complete_total"] is None, "partial actual subtotal must not be called a total")
    _assert(actual["rate_id_call_counts"] == {"rate-before": 1}, "actual mode must expose the applied rate id")
    scenario = all_report["summary"]["standard_scenario"]
    _assert(scenario["covered_call_count"] == 2, "standard scenario uses only actual model and supported token rows")
    _assert(scenario["credits_subtotal"] == "255", "standard scenario subtotal")
    _assert(scenario["complete_total"] is None, "partial scenario subtotal must not be called a total")
    _assert(scenario["rate_id_call_counts"] == {"rate-before": 1, "rate-after": 1}, "half-open rate boundary selection")
    _assert(all_report["lineage_coverage"]["unresolved_thread_count"] == 0, "valid parent/fork lineage")
    _assert(all_report["summary"]["token_missing_call_count"]["uncached_input_tokens"] == 1, "missing usage count")
    _assert(
        {entry["actual_model_used"] for entry in all_report["summary"]["actual_model_evidence"]}
        == {"gpt-6-luna", None},
        "report must use and retain response-observed model evidence",
    )
    _assert(
        {entry["actual_model_used"] for entry in all_report["summary"]["requested_to_actual_model_evidence"]}
        == {"gpt-6-luna", None},
        "requested model must remain visibly distinct from the response-observed model",
    )
    _assert(
        {entry["rate_id"] for entry in all_report["credit_evidence"]["rates"]}
        == {"rate-before", "rate-after"},
        "applied rate intervals and sources must be included",
    )
    _assert(any("SEARCH usage_provider_calls USING INDEX usage_provider_calls_started_at_idx" in detail for plan in all_diagnostic["query_plans"] for detail in plan["details"]), "all-window calls must use the started_at index")

    subtree, subtree_diagnostic = _report(
        database,
        repo_root,
        scope="subtree",
        thread_id="root-thread",
        diagnostics=True,
        expected_exit=2,
    )
    contributions = subtree["scope_contributions"]
    _assert(subtree["summary"]["provider_call_count"] == 4, "subtree must include all parent and fork descendants")
    _assert(contributions["own"]["provider_call_count"] == 1, "anchor's own usage")
    _assert(contributions["direct_children"]["provider_call_count"] == 1, "direct child attribution")
    _assert(contributions["descendants_excluding_anchor"]["provider_call_count"] == 3, "all descendants excluding anchor")
    _assert(contributions["tree_including_anchor"]["provider_call_count"] == 4, "tree attribution reconciliation")
    _assert(subtree["lineage_coverage"]["known_zero_call_thread_count"] == 1, "known zero-call descendant retained")
    _assert(any("SEARCH usage_threads USING INDEX usage_threads_reporting_parent_idx" in detail for plan in subtree_diagnostic["query_plans"] for detail in plan["details"]), "subtree must use normalized-parent index")

    thread, _thread_diagnostic = _report(
        database, repo_root, scope="thread", thread_id="child-thread", expected_exit=0
    )
    _assert(thread["summary"]["provider_call_count"] == 1, "exact-thread scope")
    _assert(thread["summary"]["actual_mode"]["complete_total"] == "4.25", "provider credit wins for exact thread")

    wal_database = temp_root / "wal-concurrency.sqlite"
    _copy_database(database, wal_database)
    writer = sqlite3.connect(wal_database, isolation_level=None)
    writer.execute("PRAGMA journal_mode=WAL")
    writer.execute("PRAGMA wal_autocheckpoint=0")
    writer.execute("BEGIN IMMEDIATE")
    writer.execute(
        "INSERT INTO usage_provider_calls(provider_call_id, thread_id, provider, actual_model_used, actual_service_tier, actual_service_tier_source, fast_mode_used, billing_surface, account_plan, started_at, completed_at, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            "wal-uncommitted-call",
            "child-thread",
            "openai",
            "gpt-6-luna",
            "default",
            "runtime_contract",
            0,
            "chatgpt_credits",
            "plus",
            "2026-09-30T00:45:00Z",
            "2026-09-30T00:45:01Z",
            1,
            0,
            0,
            1,
            2,
            "ok",
        ),
    )
    before_commit, _before_commit_diagnostic = _report(wal_database, repo_root, expected_exit=2)
    _assert(before_commit["summary"]["provider_call_count"] == 4, "WAL reader must not expose an uncommitted writer row")
    _assert(
        before_commit["snapshot"]["last_persisted_call_rowid"] == all_report["snapshot"]["last_persisted_call_rowid"],
        "uncommitted call must not advance the snapshot watermark",
    )
    writer.commit()
    after_commit, _after_commit_diagnostic = _report(wal_database, repo_root, expected_exit=2)
    _assert(after_commit["summary"]["provider_call_count"] == 5, "WAL reader must see a committed row on the next snapshot")
    writer.close()

    at_rate_boundary, _boundary_diagnostic = _report(
        database,
        repo_root,
        start=START,
        end="2026-09-30T00:30:00Z",
        expected_exit=0,
    )
    _assert(at_rate_boundary["summary"]["provider_call_count"] == 1, "end is exclusive")
    _assert(at_rate_boundary["summary"]["actual_mode"]["rate_id_call_counts"] == {"rate-before": 1}, "rate interval boundary is half-open")

    dst, _dst_diagnostic = _report(
        database,
        repo_root,
        start="2026-10-03T15:30:00Z",
        end="2026-10-03T16:30:00Z",
        timezone="Australia/Melbourne",
        credit_mode="supplied_standard_scenario",
        expected_exit=0,
    )
    bounds = dst["window"]["presentation_bounds"]
    _assert(bounds["start"].endswith("+10:00"), "DST transition start offset")
    _assert(bounds["end_exclusive"].endswith("+11:00"), "DST transition end offset")

    unknown, _unknown_diagnostic = _report(
        database,
        repo_root,
        scope="thread",
        thread_id="unknown-thread",
        expected_exit=3,
    )
    _assert(unknown["status"] == "unavailable" and unknown["error"]["reason"] == "unknown_thread_id", "unknown id is unavailable, not a zero report")

    missing_index = temp_root / "missing-index.sqlite"
    _copy_database(database, missing_index)
    with sqlite3.connect(missing_index) as connection:
        connection.execute("DROP INDEX usage_provider_calls_started_at_idx")
    missing, _missing_diagnostic = _report(missing_index, repo_root, expected_exit=3)
    _assert(missing["status"] == "unavailable", "required-index loss must fail closed")
    _assert(missing["error"]["reason"] == "required_reporting_index_missing", "missing index must not fall back to a broad query")

    cap, _cap_diagnostic = _report(
        database,
        repo_root,
        start="2020-01-01T00:00:00Z",
        end="2020-01-02T00:00:00Z",
        credit_mode="supplied_standard_scenario",
        expected_exit=0,
    )
    # The base fixture contains no history in 2020. The scale lane below proves
    # the call cap using a separately generated million-row history.
    _assert(cap["status"] == "complete", "empty historical window is a known zero")
    rollback = temp_root / "migration-rollback.sqlite"
    _copy_database(database, rollback)
    rollback_connection = sqlite3.connect(rollback, isolation_level=None)
    rollback_connection.execute("DROP INDEX usage_provider_calls_started_at_idx")
    rollback_connection.execute("DROP INDEX usage_provider_calls_thread_started_at_idx")
    rollback_connection.execute("CREATE INDEX usage_provider_calls_thread_idx ON usage_provider_calls(thread_id)")
    rollback_connection.execute(
        "CREATE INDEX usage_provider_calls_thread_started_at_idx ON usage_provider_calls(provider_call_id)"
    )
    rows_before = rollback_connection.execute("SELECT COUNT(*) FROM usage_provider_calls").fetchone()[0]
    try:
        rollback_connection.executescript(
            "BEGIN IMMEDIATE;\n"
            + (repo_root / "codex-rs" / "state" / "usage_migrations" / "0020_bounded_usage_reporting_indexes.sql").read_text(encoding="utf-8")
            + "\nCOMMIT;"
        )
    except sqlite3.Error:
        if rollback_connection.in_transaction:
            rollback_connection.rollback()
    else:
        raise AssertionError("deliberately conflicting migration unexpectedly succeeded")
    rollback_indexes = {
        row[1]
        for row in rollback_connection.execute("PRAGMA index_list('usage_provider_calls')").fetchall()
    }
    _assert("usage_provider_calls_started_at_idx" not in rollback_indexes, "failed migration must roll back earlier DDL")
    _assert("usage_provider_calls_thread_idx" in rollback_indexes, "failed migration must preserve prior index")
    _assert(
        rollback_connection.execute("SELECT COUNT(*) FROM usage_provider_calls").fetchone()[0] == rows_before,
        "failed migration must preserve usage rows",
    )
    rollback_connection.close()
    return {
        "correctness_status": "passed",
        "all_window_call_count": all_report["summary"]["provider_call_count"],
        "subtree_call_count": subtree["summary"]["provider_call_count"],
        "actual_covered_calls": actual["covered_call_count"],
        "scenario_covered_calls": scenario["covered_call_count"],
        "diagnostic_vm_instructions": all_diagnostic.get("sqlite_vm_instruction_estimate"),
        "wal_writer_visibility": "passed",
        "migration_rollback": "passed",
    }


def _history_rows(offset: int, count: int):
    for index in range(offset, offset + count):
        minute = index % 1_440
        yield (
            f"history-call-{index:07d}",
            "history-thread",
            f"2020-01-01T{minute // 60:02d}:{minute % 60:02d}:00Z",
            "ok",
        )


def _insert_history(connection: sqlite3.Connection, offset: int, count: int) -> None:
    connection.execute("BEGIN IMMEDIATE")
    try:
        for chunk_start in range(offset, offset + count, 10_000):
            chunk_count = min(10_000, offset + count - chunk_start)
            connection.executemany(
                "INSERT INTO usage_provider_calls(provider_call_id, thread_id, started_at, status) VALUES (?, ?, ?, ?)",
                _history_rows(chunk_start, chunk_count),
            )
        connection.commit()
    except sqlite3.Error:
        connection.rollback()
        raise


def _prepare_old_schema_copy(source: Path, destination: Path) -> sqlite3.Connection:
    _copy_database(source, destination)
    connection = sqlite3.connect(destination, isolation_level=None)
    connection.execute("PRAGMA journal_mode=WAL")
    connection.execute("PRAGMA wal_autocheckpoint=0")
    connection.execute("DROP INDEX usage_provider_calls_started_at_idx")
    connection.execute("DROP INDEX usage_provider_calls_thread_started_at_idx")
    connection.execute("DROP INDEX usage_threads_reporting_parent_idx")
    connection.execute("CREATE INDEX usage_provider_calls_thread_idx ON usage_provider_calls(thread_id)")
    return connection


def _migrate_and_measure(connection: sqlite3.Connection, migration_path: Path) -> dict[str, Any]:
    before_rows = connection.execute("SELECT COUNT(*) FROM usage_provider_calls").fetchone()[0]
    before_pages = connection.execute("PRAGMA page_count").fetchone()[0]
    before_bytes = Path(connection.execute("PRAGMA database_list").fetchone()[2]).stat().st_size
    started = time.perf_counter()
    sql = migration_path.read_text(encoding="utf-8")
    try:
        connection.executescript("BEGIN IMMEDIATE;\n" + sql + "\nCOMMIT;")
    except sqlite3.Error:
        if connection.in_transaction:
            connection.rollback()
        raise
    migration_seconds = time.perf_counter() - started
    after_pages = connection.execute("PRAGMA page_count").fetchone()[0]
    database_path = Path(connection.execute("PRAGMA database_list").fetchone()[2])
    wal_path = Path(str(database_path) + "-wal")
    indexes = {
        row[1]
        for row in connection.execute("PRAGMA index_list('usage_provider_calls')").fetchall()
    }
    required = {
        "usage_provider_calls_started_at_idx",
        "usage_provider_calls_thread_started_at_idx",
        "usage_provider_calls_thread_idx",
    }
    _assert(required.intersection(indexes) == required - {"usage_provider_calls_thread_idx"}, "migration index set")
    thread_indexes = {
        row[1]
        for row in connection.execute("PRAGMA index_list('usage_threads')").fetchall()
    }
    _assert("usage_threads_reporting_parent_idx" in thread_indexes, "migration parent index")
    after_rows = connection.execute("SELECT COUNT(*) FROM usage_provider_calls").fetchone()[0]
    _assert(after_rows == before_rows, "migration preserves exact usage row count")
    return {
        "migration_seconds": round(migration_seconds, 6),
        "database_bytes_before": before_bytes,
        "database_bytes_after": database_path.stat().st_size,
        "page_count_before": before_pages,
        "page_count_after": after_pages,
        "usage_rows_preserved": after_rows,
        "wal_bytes_after_migration": wal_path.stat().st_size if wal_path.exists() else 0,
        "index_names": sorted(indexes),
    }


def _scale(database: Path, repo_root: Path, temp_root: Path) -> dict[str, Any]:
    migration = repo_root / "codex-rs" / "state" / "usage_migrations" / "0020_bounded_usage_reporting_indexes.sql"
    full_old = temp_root / "history-old.sqlite"
    connection = _prepare_old_schema_copy(database, full_old)
    snapshots: list[tuple[int, Path]] = []
    for count in (10_000, 90_000, 900_000):
        before = connection.execute("SELECT COUNT(*) FROM usage_provider_calls").fetchone()[0]
        _insert_history(connection, before, count)
        snapshot = temp_root / f"history-{before + count}.sqlite"
        destination = sqlite3.connect(snapshot)
        try:
            connection.backup(destination)
        finally:
            destination.close()
        snapshots.append((before + count, snapshot))
    migration_measurement = _migrate_and_measure(connection, migration)
    connection.close()

    scale_results: list[dict[str, Any]] = []
    instruction_counts: list[int] = []
    for total, path in snapshots:
        if path == snapshots[-1][1]:
            measured_path = full_old
        else:
            measured_path = path
            migration_connection = sqlite3.connect(measured_path, isolation_level=None)
            migration_connection.execute("PRAGMA journal_mode=WAL")
            migration_connection.execute("PRAGMA wal_autocheckpoint=0")
            _migrate_and_measure(migration_connection, migration)
            migration_connection.close()
        report, diagnostic = _report(measured_path, repo_root, diagnostics=True, expected_exit=2)
        vm = diagnostic.get("sqlite_vm_instruction_estimate")
        if vm is None:
            raise AssertionError("diagnostics omitted SQLite work estimate")
        instruction_counts.append(int(vm))
        rss = diagnostic.get("max_rss_platform_units")
        _assert(rss is not None and int(rss) <= 64 * 1024, "hosted Python process must remain within the 64 MiB RSS target")
        plans = [detail for plan in diagnostic["query_plans"] for detail in plan["details"]]
        _assert(any("SEARCH usage_provider_calls USING INDEX usage_provider_calls_started_at_idx" in detail for detail in plans), f"history size {total}: started_at range index")
        _assert(report["summary"]["provider_call_count"] == 4, f"history size {total}: fixed one-hour result cardinality")
        scale_results.append(
            {
                "historical_call_count": total,
                "selected_call_count": diagnostic["selected_call_rows"],
                "sqlite_vm_instruction_estimate": vm,
                "elapsed_seconds": diagnostic["elapsed_seconds"],
                "max_rss_platform_units": diagnostic.get("max_rss_platform_units"),
                "max_rss_unit": "KiB on the standard Ubuntu hosted runner",
            }
        )
    _assert(max(instruction_counts) <= min(instruction_counts) * 1.25 + 5_000, "indexed read work should not grow with out-of-window history")

    cap, _cap_diagnostic = _report(
        full_old,
        repo_root,
        start="2020-01-01T00:00:00Z",
        end="2020-01-02T00:00:00Z",
        credit_mode="supplied_standard_scenario",
        expected_exit=2,
    )
    _assert(cap["status"] == "incomplete", "over-limit window must return typed incomplete")
    _assert(cap["error"]["reason"] == "call_limit_exceeded", "over-limit query must not fall back globally")

    group_limit_database = temp_root / "output-group-limit.sqlite"
    _copy_database(database, group_limit_database)
    group_writer = sqlite3.connect(group_limit_database)
    try:
        group_writer.executemany(
            "INSERT INTO usage_provider_calls(provider_call_id, thread_id, provider, actual_model_used, actual_service_tier, actual_service_tier_source, fast_mode_used, billing_surface, account_plan, started_at, completed_at, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                (
                    f"output-group-call-{index}",
                    f"output-group-thread-{index}",
                    "openai",
                    "gpt-6-luna",
                    "default",
                    "runtime_contract",
                    0,
                    "chatgpt_credits",
                    "plus",
                    "2026-09-30T00:45:00Z",
                    "2026-09-30T00:45:01Z",
                    1,
                    0,
                    0,
                    1,
                    2,
                    "ok",
                )
                for index in range(1_001)
            ),
        )
        group_writer.commit()
    finally:
        group_writer.close()
    group_limit, _group_limit_diagnostic = _report(
        group_limit_database,
        repo_root,
        credit_mode="supplied_standard_scenario",
        expected_exit=2,
    )
    _assert(group_limit["status"] == "incomplete", "excess output groups must return typed incomplete")
    _assert(
        group_limit["error"]["reason"] == "output_group_limit_exceeded",
        "output group overflow must stop before per-thread aggregation without widening the query",
    )

    return {
        "scale_status": "passed",
        "history_queries": scale_results,
        "largest_history_migration": migration_measurement,
        "call_cap_status": cap["error"]["reason"],
        "output_group_cap_status": group_limit["error"]["reason"],
        "qualification_note": "migration/storage measurements are synthetic hosted evidence; writer-index timing is measured separately with the real runtime writer by the Rust consumer test",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--repo-root", type=Path, required=True)
    parser.add_argument("--scale", action="store_true")
    args = parser.parse_args()
    database = args.database.resolve(strict=True)
    repo_root = args.repo_root.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="codex-usage-report-qualification-") as temporary:
        temp_root = Path(temporary)
        result = {"consumer": _correctness(database, repo_root, temp_root)}
        if args.scale:
            result["scale"] = _scale(database, repo_root, temp_root)
        print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
