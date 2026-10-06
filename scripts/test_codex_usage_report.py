"""Hosted synthetic consumer and scale qualification for codex_usage_report.py."""

from __future__ import annotations

import argparse
import calendar
import importlib.util
import io
import json
import os
import sqlite3
import struct
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


def _write_melbourne_tzif_fixture(zoneinfo_root: Path) -> None:
    """Write only the transition used by the DST assertion into a temp TZif."""
    zone = zoneinfo_root / "Australia" / "Melbourne"
    zone.parent.mkdir(parents=True, exist_ok=True)
    abbreviations = b"AEST\0AEDT\0"
    header = (
        b"TZif\0"
        + bytes(15)
        + struct.pack(
            ">6I",
            0,  # ttisgmtcnt
            0,  # ttisstdcnt
            0,  # leapcnt
            1,  # timecnt
            2,  # typecnt
            len(abbreviations),
        )
    )
    # The test window straddles Melbourne's 2026 spring transition at 16:00Z.
    transition = calendar.timegm((2026, 10, 3, 16, 0, 0))
    transitions = (
        struct.pack(">l", transition)
        + b"\x01"
        + struct.pack(">lBB", 10 * 60 * 60, 0, 0)
        + struct.pack(">lBB", 11 * 60 * 60, 1, 5)
        + abbreviations
    )
    zone.write_bytes(header + transitions)


def _diagnostic_excerpt(value: str, max_chars: int = 512) -> str:
    if len(value) <= max_chars:
        return value
    prefix_chars = max_chars // 2
    suffix_chars = max_chars - prefix_chars
    omitted_chars = len(value) - max_chars
    return f"{value[:prefix_chars]}...[{omitted_chars} chars omitted]...{value[-suffix_chars:]}"


def _bounded_report_summary(stdout: str) -> str:
    try:
        report = json.loads(stdout)
    except (json.JSONDecodeError, TypeError):
        return "unavailable"
    if not isinstance(report, dict):
        return "unavailable"
    summary = report.get("summary")
    if not isinstance(summary, dict):
        summary = {}

    def count(value: Any) -> int | None:
        return value if type(value) is int and 0 <= value <= 1_000_000 else None

    def reason_counts(value: Any) -> dict[str, int]:
        if not isinstance(value, dict):
            return {}
        result: dict[str, int] = {}
        for key, raw_amount in sorted(value.items())[:8]:
            amount = count(raw_amount)
            if isinstance(key, str) and amount is not None:
                result[key[:80]] = amount
        return result

    def mode_summary(name: str) -> dict[str, Any]:
        mode = summary.get(name)
        if not isinstance(mode, dict):
            return {}
        return {
            "covered_call_count": count(mode.get("covered_call_count")),
            "uncovered_call_count": count(mode.get("uncovered_call_count")),
            "uncovered_reasons": reason_counts(mode.get("uncovered_reasons")),
        }

    incomplete_reasons = report.get("incomplete_reasons")
    if not isinstance(incomplete_reasons, list):
        incomplete_reasons = []
    status = report.get("status")
    context = {
        "status": status
        if isinstance(status, str)
        and status in {"complete", "incomplete", "unavailable"}
        else None,
        "incomplete_reasons": [
            reason[:80] for reason in incomplete_reasons[:8] if isinstance(reason, str)
        ],
        "provider_call_count": count(summary.get("provider_call_count")),
        "token_missing_call_count": reason_counts(
            summary.get("token_missing_call_count")
        ),
        "actual_mode": mode_summary("actual_mode"),
        "standard_scenario": mode_summary("standard_scenario"),
        "lineage_coverage": {
            key: count(report.get("lineage_coverage", {}).get(key))
            for key in (
                "participating_thread_count",
                "unresolved_thread_count",
                "known_zero_call_thread_count",
            )
        }
        if isinstance(report.get("lineage_coverage"), dict)
        else {},
    }
    return json.dumps(context, sort_keys=True, separators=(",", ":"))[:1_024]


def _report(
    database: Path,
    repo_root: Path,
    *,
    start: str = START,
    end: str = END,
    timezone: str = "UTC",
    zoneinfo_path: Path | None = None,
    scope: str = "all",
    thread_id: str | None = None,
    credit_mode: str = "both",
    diagnostics: bool = False,
    expected_exit: int | None = None,
) -> tuple[dict[str, Any], dict[str, Any]]:
    cli = Path(__file__).resolve().with_name("codex_usage_report.py")
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
    if zoneinfo_path is not None:
        env["PYTHONTZPATH"] = str(zoneinfo_path.resolve())
    completed = subprocess.run(
        argv,
        cwd=repo_root,
        env=env,
        check=False,
        shell=False,
        capture_output=True,
        text=True,
        timeout=8,
    )
    if expected_exit is not None and completed.returncode != expected_exit:
        raise AssertionError(
            "reporter exit mismatch: "
            f"actual={completed.returncode} expected={expected_exit} "
            f"stdout_bytes={len(completed.stdout.encode('utf-8'))} "
            f"stdout_excerpt={_diagnostic_excerpt(completed.stdout)!r} "
            f"stderr_bytes={len(completed.stderr.encode('utf-8'))} "
            f"stderr_excerpt={_diagnostic_excerpt(completed.stderr)!r} "
            f"report_summary={_bounded_report_summary(completed.stdout)}"
        )
    try:
        report = json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        raise AssertionError(
            f"reporter did not return JSON at char {exc.pos}: {exc.msg}; "
            f"stdout_bytes={len(completed.stdout.encode('utf-8'))} "
            f"stdout_excerpt={_diagnostic_excerpt(completed.stdout)!r} "
            f"stderr_bytes={len(completed.stderr.encode('utf-8'))} "
            f"stderr_excerpt={_diagnostic_excerpt(completed.stderr)!r}"
        ) from exc
    diagnostic: dict[str, Any] = {}
    for line in completed.stderr.splitlines():
        prefix = "CODEX_USAGE_REPORT_DIAGNOSTICS="
        if line.startswith(prefix):
            diagnostic = json.loads(line[len(prefix) :])
    return report, diagnostic


def _assert(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def _correctness(
    database: Path,
    repo_root: Path,
    temp_root: Path,
    root_thread_id: str,
    child_thread_id: str,
) -> dict[str, Any]:
    helper_spec = importlib.util.spec_from_file_location(
        "codex_usage_report", repo_root / "scripts" / "codex_usage_report.py"
    )
    if helper_spec is None or helper_spec.loader is None:
        raise AssertionError("could not import the exact tested reporter source")
    helper = importlib.util.module_from_spec(helper_spec)
    helper_spec.loader.exec_module(helper)
    bounded_keys: set[str] = {"first"}
    try:
        helper._add_bounded_key(
            bounded_keys, "overflow", 1, "rate_policy_key_limit_exceeded"
        )
    except helper.WorkLimitReached as exc:
        _assert(
            exc.reason == "rate_policy_key_limit_exceeded",
            "bounded key overflow must be typed incomplete",
        )
    else:
        raise AssertionError("bounded key accumulator retained an over-limit key")
    _assert(
        bounded_keys == {"first"}, "over-limit key must be rejected before retention"
    )
    original_output_limit = helper.MAX_OUTPUT_BYTES
    bounded_output = io.StringIO()
    helper.MAX_OUTPUT_BYTES = 128
    try:
        try:
            helper._write_bounded_json({"synthetic": "x" * 1_024}, bounded_output)
        except helper.WorkLimitReached as exc:
            _assert(
                exc.reason == "output_size_limit_exceeded",
                "JSON byte overflow must be typed incomplete",
            )
        else:
            raise AssertionError(
                "synthetic oversized JSON unexpectedly passed its output cap"
            )
        _assert(
            bounded_output.getvalue() == "",
            "oversized JSON must be rejected before emitting partial output",
        )
    finally:
        helper.MAX_OUTPUT_BYTES = original_output_limit
    writer_guard = helper.UsageReporter(
        database,
        helper.parse_utc(START),
        helper.parse_utc(END),
        helper.UTC,
        "UTC",
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
            _assert(
                exc.reason == "output_size_limit_exceeded",
                "oversized source text must be typed incomplete",
            )
        else:
            raise AssertionError(
                "synthetic oversized source text unexpectedly passed its field cap"
            )
        original_text_limit = helper.MAX_OUTPUT_TEXT_CHARS
        helper.MAX_OUTPUT_TEXT_CHARS = 3
        writer_guard._account_output_text(
            text_connection.execute("SELECT ?", ("ab",)).fetchone()
        )
        try:
            writer_guard._account_output_text(
                text_connection.execute("SELECT ?", ("cd",)).fetchone()
            )
        except helper.WorkLimitReached as exc:
            _assert(
                exc.reason == "output_size_limit_exceeded",
                "aggregate report text must be typed incomplete",
            )
        else:
            raise AssertionError(
                "synthetic aggregate output text unexpectedly passed its cap"
            )
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

    oversized_database = temp_root / "output-text-limit.sqlite"
    _copy_database(database, oversized_database)
    oversized_writer = sqlite3.connect(oversized_database)
    try:
        oversized_writer.execute(
            "INSERT INTO usage_provider_calls(provider_call_id, thread_id, provider, actual_model_used, actual_service_tier, actual_service_tier_source, fast_mode_used, billing_surface, account_plan, started_at, completed_at, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                "oversized-output-text-call",
                child_thread_id,
                "openai",
                "x" * (2 * helper.MAX_SQLITE_ROW_BYTES),
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
        oversized_writer.commit()
    finally:
        oversized_writer.close()
    oversized_text, _oversized_text_diagnostic = _report(
        oversized_database,
        repo_root,
        credit_mode="supplied_standard_scenario",
        expected_exit=2,
    )
    _assert(
        oversized_text["status"] == "incomplete",
        "oversized source text must return typed incomplete",
    )
    _assert(
        oversized_text["error"]["reason"] == "output_size_limit_exceeded",
        "SQLite text-value limit must not allocate or emit an oversized report",
    )
    long_thread_id = "overlimit-thread-" + ("x" * (helper.MAX_TEXT_FIELD_CHARS + 1))
    long_thread_database = temp_root / "long-thread-error.sqlite"
    _copy_database(database, long_thread_database)
    long_thread_writer = sqlite3.connect(long_thread_database)
    try:
        long_thread_writer.execute(
            "INSERT INTO usage_provider_calls(provider_call_id, thread_id, provider, actual_model_used, actual_service_tier, actual_service_tier_source, fast_mode_used, billing_surface, account_plan, started_at, completed_at, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                "long-thread-id-call",
                long_thread_id,
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
        long_thread_writer.commit()
    finally:
        long_thread_writer.close()
    long_thread_error, _long_thread_diagnostic = _report(
        long_thread_database,
        repo_root,
        scope="thread",
        thread_id=long_thread_id,
        credit_mode="supplied_standard_scenario",
        expected_exit=2,
    )
    _assert(
        long_thread_error["status"] == "incomplete",
        "long-thread error must remain incomplete",
    )
    _assert(
        long_thread_error["error"]["reason"] == "output_size_limit_exceeded",
        "long thread id must trigger bounded text accounting",
    )
    _assert(
        long_thread_error["scope"]["thread_id"] is None
        and long_thread_error["scope"]["thread_id_omitted"] is True,
        "error JSON must disclose that an overlong thread id was omitted",
    )
    bounded_error_bytes = (
        len(
            json.dumps(
                long_thread_error,
                sort_keys=True,
                separators=(",", ":"),
                ensure_ascii=True,
            ).encode("utf-8")
        )
        + 1
    )
    _assert(
        bounded_error_bytes <= helper.MAX_OUTPUT_BYTES,
        "CLI error response must stay within the JSON output cap",
    )

    subtree_text_database = temp_root / "subtree-output-text-limit.sqlite"
    _copy_database(database, subtree_text_database)
    subtree_root_id = "subtree-output-budget-root"
    subtree_writer = sqlite3.connect(subtree_text_database)
    try:
        thread_sql = "INSERT INTO usage_threads(thread_id, parent_thread_id, root_thread_id, fork_parent_thread_id, agent_nickname, agent_role, source, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        subtree_writer.execute(
            thread_sql,
            (
                subtree_root_id,
                None,
                subtree_root_id,
                None,
                "root",
                "agent",
                "synthetic",
                "2026-09-30T00:00:00Z",
            ),
        )
        subtree_writer.executemany(
            thread_sql,
            (
                (
                    f"subtree-output-child-{index}",
                    subtree_root_id,
                    subtree_root_id,
                    None,
                    "n" * helper.MAX_TEXT_FIELD_CHARS,
                    "agent",
                    "synthetic",
                    "2026-09-30T00:00:00Z",
                )
                for index in range(256)
            ),
        )
        subtree_writer.commit()
    finally:
        subtree_writer.close()
    subtree_text_limit, _subtree_text_diagnostic = _report(
        subtree_text_database,
        repo_root,
        scope="subtree",
        thread_id=subtree_root_id,
        credit_mode="supplied_standard_scenario",
        expected_exit=2,
    )
    _assert(
        subtree_text_limit["status"] == "incomplete",
        "subtree text overflow must return typed incomplete",
    )
    _assert(
        subtree_text_limit["error"]["reason"] == "output_size_limit_exceeded",
        "subtree metadata text overflow must stop during row streaming without global fallback",
    )

    all_report, all_diagnostic = _report(
        database, repo_root, diagnostics=True, expected_exit=2
    )
    _assert(
        all_report["status"] == "incomplete",
        "missing usage/model evidence must be incomplete",
    )
    _assert(
        all_report["summary"]["provider_call_count"] == 4,
        "all scope must include exactly four in-window calls",
    )
    _assert(
        all_report["window"]["presentation_timezone"] == "UTC",
        "UTC presentation timezone must be preserved",
    )
    _assert(
        all_report["summary"]["provider_reported_credits"]["call_count"] == 1,
        "reported credit count",
    )
    _assert(
        all_report["summary"]["provider_reported_credits"]["credits_subtotal"]
        == "4.25",
        "reported credit subtotal",
    )
    actual = all_report["summary"]["actual_mode"]
    _assert(
        actual["covered_call_count"] == 2,
        "reported and exact-rate actual-mode coverage",
    )
    _assert(
        actual["rate_estimated_call_count"] == 1,
        "provider report must take precedence over a rate",
    )
    _assert(actual["credits_subtotal"] == "174.25", "actual-mode subtotal")
    _assert(
        actual["complete_total"] is None,
        "partial actual subtotal must not be called a total",
    )
    _assert(
        actual["rate_id_call_counts"] == {"rate-before": 1},
        "actual mode must expose the applied rate id",
    )
    scenario = all_report["summary"]["standard_scenario"]
    _assert(
        scenario["covered_call_count"] == 2,
        "standard scenario uses only actual model and supported token rows",
    )
    _assert(scenario["credits_subtotal"] == "255", "standard scenario subtotal")
    _assert(
        scenario["complete_total"] is None,
        "partial scenario subtotal must not be called a total",
    )
    _assert(
        scenario["rate_id_call_counts"] == {"rate-before": 1, "rate-after": 1},
        "half-open rate boundary selection",
    )
    _assert(
        all_report["lineage_coverage"]["unresolved_thread_count"] == 0,
        "valid parent/fork lineage",
    )
    _assert(
        all_report["summary"]["token_missing_call_count"]["uncached_input_tokens"] == 1,
        "missing usage count",
    )
    _assert(
        {
            entry["actual_model_used"]
            for entry in all_report["summary"]["actual_model_evidence"]
        }
        == {"gpt-6-luna", None},
        "report must use and retain response-observed model evidence",
    )
    _assert(
        {
            entry["actual_model_used"]
            for entry in all_report["summary"]["requested_to_actual_model_evidence"]
        }
        == {"gpt-6-luna", None},
        "requested model must remain visibly distinct from the response-observed model",
    )
    _assert(
        {entry["rate_id"] for entry in all_report["credit_evidence"]["rates"]}
        == {"rate-before", "rate-after"},
        "applied rate intervals and sources must be included",
    )
    _assert(
        any(
            "SEARCH usage_provider_calls USING INDEX usage_provider_calls_started_at_idx"
            in detail
            for plan in all_diagnostic["query_plans"]
            for detail in plan["details"]
        ),
        "all-window calls must use the started_at index",
    )

    subtree, subtree_diagnostic = _report(
        database,
        repo_root,
        scope="subtree",
        thread_id=root_thread_id,
        diagnostics=True,
        expected_exit=2,
    )
    contributions = subtree["scope_contributions"]
    _assert(
        subtree["summary"]["provider_call_count"] == 4,
        "subtree must include all parent and fork descendants",
    )
    _assert(contributions["own"]["provider_call_count"] == 1, "anchor's own usage")
    _assert(
        contributions["direct_children"]["provider_call_count"] == 1,
        "direct child attribution",
    )
    _assert(
        contributions["descendants_excluding_anchor"]["provider_call_count"] == 3,
        "all descendants excluding anchor",
    )
    _assert(
        contributions["tree_including_anchor"]["provider_call_count"] == 4,
        "tree attribution reconciliation",
    )
    _assert(
        subtree["lineage_coverage"]["known_zero_call_thread_count"] == 1,
        "known zero-call descendant retained",
    )
    _assert(
        any(
            "SEARCH usage_threads USING INDEX usage_threads_reporting_parent_idx"
            in detail
            for plan in subtree_diagnostic["query_plans"]
            for detail in plan["details"]
        ),
        "subtree must use normalized-parent index",
    )

    thread, _thread_diagnostic = _report(
        database, repo_root, scope="thread", thread_id=child_thread_id, expected_exit=0
    )
    _assert(thread["summary"]["provider_call_count"] == 1, "exact-thread scope")
    _assert(
        thread["summary"]["actual_mode"]["complete_total"] == "4.25",
        "provider credit wins for exact thread",
    )
    _assert(
        thread["status"] == "complete",
        "thread scope must resolve the preloaded child anchor's ancestry",
    )
    _assert(
        thread["lineage_coverage"]["unresolved_thread_count"] == 0,
        "child-thread ancestor is complete",
    )
    _assert(
        thread["threads"][0]["resolved_root_thread_id"] == root_thread_id,
        "thread scope must follow the persisted parent to its recorded root",
    )

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
            child_thread_id,
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
    before_commit, _before_commit_diagnostic = _report(
        wal_database, repo_root, expected_exit=2
    )
    _assert(
        before_commit["summary"]["provider_call_count"] == 4,
        "WAL reader must not expose an uncommitted writer row",
    )
    _assert(
        before_commit["snapshot"]["last_persisted_call_rowid"]
        == all_report["snapshot"]["last_persisted_call_rowid"],
        "uncommitted call must not advance the snapshot watermark",
    )
    writer.commit()
    after_commit, _after_commit_diagnostic = _report(
        wal_database, repo_root, expected_exit=2
    )
    _assert(
        after_commit["summary"]["provider_call_count"] == 5,
        "WAL reader must see a committed row on the next snapshot",
    )
    writer.close()

    at_rate_boundary, _boundary_diagnostic = _report(
        database,
        repo_root,
        start=START,
        end="2026-09-30T00:30:00Z",
        expected_exit=0,
    )
    _assert(at_rate_boundary["summary"]["provider_call_count"] == 1, "end is exclusive")
    _assert(
        at_rate_boundary["summary"]["actual_mode"]["rate_id_call_counts"]
        == {"rate-before": 1},
        "rate interval boundary is half-open",
    )

    zoneinfo_path = None
    try:
        helper.ZoneInfo("Australia/Melbourne")
    except helper.ZoneInfoNotFoundError:
        zoneinfo_path = temp_root / "melbourne-tzdata"
        _write_melbourne_tzif_fixture(zoneinfo_path)
        dst_timezone_status = "synthetic_tzif_fixture"
    else:
        dst_timezone_status = "host_iana_tzdata"
    dst, _dst_diagnostic = _report(
        database,
        repo_root,
        start="2026-10-03T15:30:00Z",
        end="2026-10-03T16:30:00Z",
        timezone="Australia/Melbourne",
        zoneinfo_path=zoneinfo_path,
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
    _assert(
        unknown["status"] == "unavailable"
        and unknown["error"]["reason"] == "unknown_thread_id",
        "unknown id is unavailable, not a zero report",
    )

    missing_index = temp_root / "missing-index.sqlite"
    _copy_database(database, missing_index)
    with sqlite3.connect(missing_index) as connection:
        connection.execute("DROP INDEX usage_provider_calls_started_at_idx")
    missing, _missing_diagnostic = _report(missing_index, repo_root, expected_exit=3)
    _assert(missing["status"] == "unavailable", "required-index loss must fail closed")
    _assert(
        missing["error"]["reason"] == "required_reporting_index_missing",
        "missing index must not fall back to a broad query",
    )

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
    rollback_connection.execute(
        "CREATE INDEX usage_provider_calls_thread_idx ON usage_provider_calls(thread_id)"
    )
    rollback_connection.execute(
        "CREATE INDEX usage_provider_calls_thread_started_at_idx ON usage_provider_calls(provider_call_id)"
    )
    rows_before = rollback_connection.execute(
        "SELECT COUNT(*) FROM usage_provider_calls"
    ).fetchone()[0]
    try:
        rollback_connection.executescript(
            "BEGIN IMMEDIATE;\n"
            + (
                repo_root
                / "codex-rs"
                / "state"
                / "usage_migrations"
                / "0015_bounded_usage_reporting_indexes.sql"
            ).read_text(encoding="utf-8")
            + "\nCOMMIT;"
        )
    except sqlite3.Error:
        if rollback_connection.in_transaction:
            rollback_connection.rollback()
    else:
        raise AssertionError(
            "deliberately conflicting migration unexpectedly succeeded"
        )
    rollback_indexes = {
        row[1]
        for row in rollback_connection.execute(
            "PRAGMA index_list('usage_provider_calls')"
        ).fetchall()
    }
    _assert(
        "usage_provider_calls_started_at_idx" not in rollback_indexes,
        "failed migration must roll back earlier DDL",
    )
    _assert(
        "usage_provider_calls_thread_idx" in rollback_indexes,
        "failed migration must preserve prior index",
    )
    _assert(
        rollback_connection.execute(
            "SELECT COUNT(*) FROM usage_provider_calls"
        ).fetchone()[0]
        == rows_before,
        "failed migration must preserve usage rows",
    )
    rollback_connection.close()
    return {
        "correctness_status": "passed",
        "dst_timezone_status": dst_timezone_status,
        "all_window_call_count": all_report["summary"]["provider_call_count"],
        "subtree_call_count": subtree["summary"]["provider_call_count"],
        "actual_covered_calls": actual["covered_call_count"],
        "scenario_covered_calls": scenario["covered_call_count"],
        "diagnostic_vm_instructions": all_diagnostic.get(
            "sqlite_vm_instruction_estimate"
        ),
        "wal_writer_visibility": "passed",
        "migration_rollback": "passed",
        "oversized_text_status": oversized_text["error"]["reason"],
        "long_thread_error_status": long_thread_error["status"],
        "subtree_text_limit_status": subtree_text_limit["error"]["reason"],
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
    connection.execute(
        "CREATE INDEX usage_provider_calls_thread_idx ON usage_provider_calls(thread_id)"
    )
    return connection


def _migrate_and_measure(
    connection: sqlite3.Connection, migration_path: Path
) -> dict[str, Any]:
    before_rows = connection.execute(
        "SELECT COUNT(*) FROM usage_provider_calls"
    ).fetchone()[0]
    before_pages = connection.execute("PRAGMA page_count").fetchone()[0]
    before_bytes = (
        Path(connection.execute("PRAGMA database_list").fetchone()[2]).stat().st_size
    )
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
        for row in connection.execute(
            "PRAGMA index_list('usage_provider_calls')"
        ).fetchall()
    }
    required = {
        "usage_provider_calls_started_at_idx",
        "usage_provider_calls_thread_started_at_idx",
        "usage_provider_calls_thread_idx",
    }
    _assert(
        required.intersection(indexes)
        == required - {"usage_provider_calls_thread_idx"},
        "migration index set",
    )
    thread_indexes = {
        row[1]
        for row in connection.execute("PRAGMA index_list('usage_threads')").fetchall()
    }
    _assert(
        "usage_threads_reporting_parent_idx" in thread_indexes,
        "migration parent index",
    )
    after_rows = connection.execute(
        "SELECT COUNT(*) FROM usage_provider_calls"
    ).fetchone()[0]
    _assert(after_rows == before_rows, "migration preserves exact usage row count")
    return {
        "migration_seconds": round(migration_seconds, 6),
        "database_bytes_before": before_bytes,
        "database_bytes_after": database_path.stat().st_size,
        "page_count_before": before_pages,
        "page_count_after": after_pages,
        "usage_rows_preserved": after_rows,
        "wal_bytes_after_migration": wal_path.stat().st_size
        if wal_path.exists()
        else 0,
        "index_names": sorted(indexes),
    }


def _scale(database: Path, repo_root: Path, temp_root: Path) -> dict[str, Any]:
    helper_spec = importlib.util.spec_from_file_location(
        "codex_usage_report", repo_root / "scripts" / "codex_usage_report.py"
    )
    if helper_spec is None or helper_spec.loader is None:
        raise AssertionError("could not import the exact tested reporter source")
    helper = importlib.util.module_from_spec(helper_spec)
    helper_spec.loader.exec_module(helper)
    migration = (
        repo_root
        / "codex-rs"
        / "state"
        / "usage_migrations"
        / "0015_bounded_usage_reporting_indexes.sql"
    )
    full_old = temp_root / "history-old.sqlite"
    connection = _prepare_old_schema_copy(database, full_old)
    snapshots: list[tuple[int, Path]] = []
    for count in (10_000, 90_000, 900_000):
        before = connection.execute(
            "SELECT COUNT(*) FROM usage_provider_calls"
        ).fetchone()[0]
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
        report, diagnostic = _report(
            measured_path, repo_root, diagnostics=True, expected_exit=2
        )
        vm = diagnostic.get("sqlite_vm_instruction_estimate")
        if vm is None:
            raise AssertionError("diagnostics omitted SQLite work estimate")
        instruction_counts.append(int(vm))
        rss = diagnostic.get("max_rss_kib")
        if rss is None:
            _assert(
                sys.platform == "win32",
                "hosted Python process must expose RSS outside Windows",
            )
            rss_observation = "unavailable_windows_resource_module"
        else:
            _assert(
                int(rss) <= 64 * 1024,
                "hosted Python process must remain within the 64 MiB RSS target",
            )
            rss_observation = "measured"
        plans = [
            detail for plan in diagnostic["query_plans"] for detail in plan["details"]
        ]
        _assert(
            any(
                "SEARCH usage_provider_calls USING INDEX usage_provider_calls_started_at_idx"
                in detail
                for detail in plans
            ),
            f"history size {total}: started_at range index",
        )
        _assert(
            report["summary"]["provider_call_count"] == 4,
            f"history size {total}: fixed one-hour result cardinality",
        )
        scale_results.append(
            {
                "historical_call_count": total,
                "selected_call_count": diagnostic["selected_call_rows"],
                "sqlite_vm_instruction_estimate": vm,
                "elapsed_seconds": diagnostic["elapsed_seconds"],
                "max_rss_kib": rss,
                "max_rss_observation": rss_observation,
            }
        )
    _assert(
        max(instruction_counts) <= min(instruction_counts) * 1.25 + 5_000,
        "indexed read work should not grow with out-of-window history",
    )

    call_limit_database = temp_root / "call-limit.sqlite"
    _copy_database(database, call_limit_database)
    call_limit_writer = sqlite3.connect(call_limit_database)
    try:
        call_limit_writer.executemany(
            "INSERT INTO usage_provider_calls(provider_call_id, thread_id, started_at, status) VALUES (?, ?, ?, ?)",
            (
                (f"c{index:07d}", "x", "2020-01-01T00:00:00Z", "ok")
                for index in range(helper.MAX_CALLS + 1)
            ),
        )
        call_limit_writer.commit()
    finally:
        call_limit_writer.close()
    cap, _cap_diagnostic = _report(
        call_limit_database,
        repo_root,
        start="2020-01-01T00:00:00Z",
        end="2020-01-02T00:00:00Z",
        credit_mode="supplied_standard_scenario",
        expected_exit=2,
    )
    _assert(
        cap["status"] == "incomplete", "over-limit window must return typed incomplete"
    )
    _assert(
        cap["error"]["reason"] == "call_limit_exceeded",
        "over-limit query must not fall back globally",
    )

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
    _assert(
        group_limit["status"] == "incomplete",
        "excess output groups must return typed incomplete",
    )
    _assert(
        group_limit["error"]["reason"] == "output_group_limit_exceeded",
        "output group overflow must stop before per-thread aggregation without widening the query",
    )

    pricing_key_limit_database = temp_root / "pricing-key-limit.sqlite"
    _copy_database(database, pricing_key_limit_database)
    pricing_key_writer = sqlite3.connect(pricing_key_limit_database)
    try:
        pricing_key_writer.executemany(
            "INSERT INTO usage_provider_calls(provider_call_id, thread_id, provider, actual_model_used, actual_service_tier, actual_service_tier_source, fast_mode_used, billing_surface, account_plan, started_at, completed_at, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                (
                    f"pricing-key-call-{index}",
                    "child-thread",
                    "openai",
                    f"synthetic-model-{index}",
                    "default",
                    "runtime_contract",
                    0,
                    "chatgpt_credits",
                    f"synthetic-plan-{index}",
                    "2026-09-30T00:45:00Z",
                    "2026-09-30T00:45:01Z",
                    1,
                    0,
                    0,
                    1,
                    2,
                    "ok",
                )
                for index in range(helper.MAX_RATE_POLICY_ROWS + 1)
            ),
        )
        pricing_key_writer.commit()
    finally:
        pricing_key_writer.close()
    for credit_mode in ("observed_or_effective_rate", "supplied_standard_scenario"):
        pricing_key_limit, _pricing_key_diagnostic = _report(
            pricing_key_limit_database,
            repo_root,
            credit_mode=credit_mode,
            expected_exit=2,
        )
        _assert(
            pricing_key_limit["status"] == "incomplete",
            f"{credit_mode} key overflow must be incomplete",
        )
        _assert(
            pricing_key_limit["error"]["reason"] == "rate_policy_key_limit_exceeded",
            f"{credit_mode} key overflow must stop without widening the query",
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
    parser.add_argument("--root-thread-id", default="root-thread")
    parser.add_argument("--child-thread-id", default="child-thread")
    parser.add_argument("--scale", action="store_true")
    args = parser.parse_args()
    database = args.database.resolve(strict=True)
    repo_root = Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory(
        prefix="codex-usage-report-qualification-"
    ) as temporary:
        temp_root = Path(temporary)
        result = {
            "consumer": _correctness(
                database,
                repo_root,
                temp_root,
                args.root_thread_id,
                args.child_thread_id,
            )
        }
        if args.scale:
            result["scale"] = _scale(database, repo_root, temp_root)
        print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
