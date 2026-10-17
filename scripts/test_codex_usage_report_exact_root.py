#!/usr/bin/env python3
"""Hosted synthetic qualification for bounded exact-root usage reporting."""

from __future__ import annotations

import hashlib
import io
import json
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import codex_usage_report as report


ROOT_THREAD = "synthetic-root"
START = "1970-01-01T00:00:00Z"
END = "2100-01-01T00:00:00Z"
KNOWN_REPORT_REASON_CODES = frozenset(
    {
        "actual_credit_coverage_incomplete",
        "attribution_text_accounting_unavailable",
        "bounded_text_limit_unavailable",
        "call_limit_exceeded",
        "configured_model_attribution_conflict",
        "cross_ledger_reconciliation_limited_by_source_coverage",
        "database_unavailable",
        "deadline_exceeded",
        "instruction_limit_exceeded",
        "ledger_busy",
        "ledger_schema_mismatch",
        "lineage_cycle",
        "lineage_depth_limit_exceeded",
        "lineage_incomplete",
        "lineage_metadata_changed_during_snapshot",
        "lineage_metadata_duplicate_id",
        "lineage_metadata_invalid",
        "lineage_node_limit_exceeded",
        "lineage_scan_row_limit_exceeded",
        "lineage_scan_text_limit_exceeded",
        "output_group_limit_exceeded",
        "physical_call_deduplication_incomplete",
        "rate_policy_row_limit_exceeded",
        "rate_policy_key_limit_exceeded",
        "read_only_policy_denied_query",
        "required_reporting_index_missing",
        "required_spawn_request_index_missing",
        "selected_attribution_text_limit_exceeded",
        "selected_source_text_limit_exceeded",
        "serialized_error_limit_exceeded",
        "serialized_json_limit_exceeded",
        "source_identity_field_limit_exceeded",
        "source_text_field_limit_exceeded",
        "spawn_request_metadata_limit_exceeded",
        "sqlite_value_limit_exceeded",
        "standard_scenario_coverage_incomplete",
        "thread_metadata_missing",
        "token_coverage_incomplete",
        "unknown_thread_id",
        "unsupported_timestamp_encoding",
        "sqlite_query_failed",
        "sqlite_read_failed",
        "all_time_requires_exact_thread_scope",
    }
)


def _assert(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def _safe_report_process_diagnostic(
    returncode: int, stdout: str, stderr: str
) -> str:
    status = "unavailable"
    reason_codes: tuple[str, ...] = ()
    error_reason = "none"
    try:
        parsed = json.loads(stdout)
    except json.JSONDecodeError:
        parsed = None
    if isinstance(parsed, dict):
        candidate_status = parsed.get("status")
        if candidate_status in {"complete", "incomplete", "unavailable"}:
            status = str(candidate_status)
        reasons = parsed.get("incomplete_reasons")
        if isinstance(reasons, list):
            reason_codes = tuple(
                value
                for value in reasons
                if isinstance(value, str) and value in KNOWN_REPORT_REASON_CODES
            )[:8]
        error = parsed.get("error")
        if isinstance(error, dict):
            candidate_reason = error.get("reason")
            if (
                isinstance(candidate_reason, str)
                and candidate_reason in KNOWN_REPORT_REASON_CODES
            ):
                error_reason = candidate_reason

    source_line = "none"
    for line in stderr.splitlines():
        marker = "scripts/codex_usage_report.py\", line "
        if marker not in line:
            continue
        suffix = line.rsplit(marker, 1)[1]
        candidate = suffix.split(",", 1)[0]
        if candidate.isdigit():
            source_line = candidate

    exception_class = "none"
    for line in reversed(stderr.splitlines()):
        candidate = line.strip().split(":", 1)[0]
        if candidate.isidentifier() and (
            candidate.endswith("Error")
            or candidate.endswith("Exception")
            or candidate in {"AssertionError", "KeyboardInterrupt"}
        ):
            exception_class = candidate
            break
    return (
        f"stage=single_ledger_cli expected_exit=2 observed_exit={returncode} "
        f"report_status={status} error_reason={error_reason} "
        f"reason_codes={','.join(reason_codes) or 'none'} "
        f"source_line={source_line} exception_class={exception_class}"
    )


def _create_database(path: Path, *, current_indexes: bool) -> None:
    connection = sqlite3.connect(path)
    try:
        migrations = (
            Path(__file__).resolve().parent.parent
            / "codex-rs"
            / "state"
            / "usage_migrations"
        )
        for migration in sorted(migrations.glob("00*.sql")):
            if migration.name.startswith("0015_") or migration.name.startswith("0020_"):
                continue
            connection.executescript(migration.read_text(encoding="utf-8"))
        if current_indexes:
            connection.executescript(
                (migrations / "0015_bounded_usage_reporting_indexes.sql").read_text(
                    encoding="utf-8"
                )
            )
        connection.executemany(
            """
            INSERT INTO usage_threads(
                thread_id, parent_thread_id, root_thread_id, fork_parent_thread_id
            ) VALUES (?, ?, ?, ?)
            """,
            (
                (ROOT_THREAD, None, ROOT_THREAD, None),
                ("synthetic-child", ROOT_THREAD, ROOT_THREAD, None),
                ("synthetic-root-tagged-detached-child", None, ROOT_THREAD, None),
                ("synthetic-grandchild", None, ROOT_THREAD, "synthetic-child"),
                ("synthetic-zero-call-leaf", "synthetic-child", ROOT_THREAD, None),
            ),
        )
        connection.executemany(
            """
            INSERT INTO usage_spawn_requests(
                spawn_request_id, parent_thread_id, child_thread_id, status
            ) VALUES (?, ?, ?, 'complete')
            """,
            (
                ("spawn-sentinel", ROOT_THREAD, "synthetic-child"),
                ("spawn-child-grandchild", "synthetic-child", "synthetic-grandchild"),
                ("spawn-child-zero-leaf", "synthetic-child", "synthetic-zero-call-leaf"),
            ),
        )
        connection.executemany(
            """
            INSERT INTO usage_provider_calls(
                provider_call_id, thread_id, provider, requested_model,
                actual_model_used, actual_service_tier, actual_service_tier_source,
                fast_mode_used, billing_surface, account_plan, started_at,
                completed_at, input_tokens_uncached, input_tokens_cached,
                input_tokens_cache_write, output_tokens, total_tokens, status
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                (
                    "root-alpha",
                    ROOT_THREAD,
                    "openai",
                    "request-alpha",
                    "synthetic-alpha",
                    "default",
                    "fixture",
                    0,
                    "chatgpt_credits",
                    "test",
                    "2026-09-30T00:01:00Z",
                    "2026-09-30T00:01:01Z",
                    7,
                    3,
                    2,
                    5,
                    17,
                    "ok",
                ),
                (
                    "child-beta",
                    "synthetic-child",
                    "openai",
                    "request-beta",
                    "synthetic-beta",
                    "default",
                    "fixture",
                    0,
                    "chatgpt_credits",
                    "test",
                    "2026-09-30T00:02:00Z",
                    "2026-09-30T00:02:01Z",
                    11,
                    2,
                    0,
                    4,
                    17,
                    "ok",
                ),
                (
                    "child-sentinel-literal",
                    "synthetic-child",
                    "openai",
                    "request-sentinel",
                    "<unavailable>",
                    "default",
                    "fixture",
                    0,
                    "chatgpt_credits",
                    "test",
                    "2026-09-30T00:02:30Z",
                    "2026-09-30T00:02:31Z",
                    1,
                    0,
                    0,
                    1,
                    2,
                    "ok",
                ),
                (
                    "grandchild-alpha",
                    "synthetic-grandchild",
                    "openai",
                    "request-alpha",
                    "synthetic-alpha",
                    "default",
                    "fixture",
                    0,
                    "chatgpt_credits",
                    "test",
                    "2026-09-30T00:03:00Z",
                    "2026-09-30T00:03:01Z",
                    13,
                    1,
                    0,
                    6,
                    20,
                    "ok",
                ),
                (
                    "grandchild-unavailable",
                    "synthetic-grandchild",
                    "openai",
                    "request-unknown",
                    None,
                    "default",
                    "fixture",
                    0,
                    "chatgpt_credits",
                    "test",
                    "2026-09-30T00:04:00Z",
                    "2026-09-30T00:04:01Z",
                    2,
                    0,
                    0,
                    1,
                    3,
                    "ok",
                ),
                (
                    "root-tagged-alpha",
                    "synthetic-root-tagged-detached-child",
                    "openai",
                    "request-alpha",
                    "synthetic-alpha",
                    "default",
                    "fixture",
                    0,
                    "chatgpt_credits",
                    "test",
                    "2026-09-30T00:05:00Z",
                    "2026-09-30T00:05:01Z",
                    3,
                    0,
                    0,
                    2,
                    5,
                    "ok",
                ),
            ),
        )
        for model in ("synthetic-alpha", "synthetic-beta"):
            connection.execute(
                """
                INSERT INTO usage_codex_credit_rates(
                    rate_id, provider, model, service_tier, speed_mode,
                    rate_card_kind, credits_per_1m_uncached_input,
                    credits_per_1m_cached_input, credits_per_1m_output,
                    effective_from, effective_to, source_url, source_observed_at
                ) VALUES (
                    ?, 'openai', ?, 'default', 'standard', 'codex_token_based',
                    1, 1, 1, '2020-01-01T00:00:00Z', NULL,
                    'https://example.invalid/fixture', '2026-01-01T00:00:00Z'
                )
                """,
                (f"fixture-{model}", model),
            )
        connection.commit()
    finally:
        connection.close()


def _run_report(
    database: Path, *, diagnostics: bool = True
) -> tuple[dict[str, Any], dict[str, Any]]:
    script = Path(__file__).resolve().with_name("codex_usage_report.py")
    command = [
        sys.executable,
        str(script),
        "--database",
        str(database),
        "--start-utc",
        START,
        "--end-utc",
        END,
        "--timezone",
        "UTC",
        "--scope",
        "subtree",
        "--thread-id",
        ROOT_THREAD,
        "--credit-mode",
        "supplied_standard_scenario",
    ]
    if diagnostics:
        command.append("--diagnostics")
    completed = subprocess.run(
        command,
        check=False,
        shell=False,
        capture_output=True,
        text=True,
        timeout=8,
    )
    _assert(
        completed.returncode == 2,
        "expected incomplete only for missing actual-model rate; "
        + _safe_report_process_diagnostic(
            completed.returncode, completed.stdout, completed.stderr
        ),
    )
    parsed = json.loads(completed.stdout)
    diagnostic: dict[str, Any] = {}
    for line in completed.stderr.splitlines():
        prefix = "CODEX_USAGE_REPORT_DIAGNOSTICS="
        if line.startswith(prefix):
            diagnostic = json.loads(line[len(prefix) :])
    return parsed, diagnostic


def _run_multi_report(current: Path, historical: Path) -> dict[str, Any]:
    script = Path(__file__).resolve().with_name("codex_usage_report.py")
    completed = subprocess.run(
        [
            sys.executable,
            str(script),
            "--database",
            str(current),
            "--historical-database",
            str(historical),
            "--all-time",
            "--timezone",
            "UTC",
            "--scope",
            "subtree",
            "--thread-id",
            ROOT_THREAD,
            "--credit-mode",
            "none",
        ],
        check=False,
        shell=False,
        capture_output=True,
        text=True,
        timeout=8,
    )
    _assert(completed.returncode == 2, "known duplicate conflict remains incomplete")
    return json.loads(completed.stdout)


def _by_model(metrics: dict[str, Any]) -> dict[str | None, dict[str, Any]]:
    return {item["actual_model_used"]: item for item in metrics["by_actual_model"]}


def _check_shared_text_pool() -> None:
    shared_text_pool: dict[str, str] = {}
    shared_attribution_text: set[str] = set()
    current = report.UsageReporter(
        Path("unused"),
        report.parse_utc(START),
        report.parse_utc(END),
        report.UTC,
        "UTC",
        "subtree",
        ROOT_THREAD,
        "none",
        False,
        token_only_projection=True,
        max_selected_text_chars=5,
        shared_text_pool=shared_text_pool,
        shared_attribution_text=shared_attribution_text,
    )
    historical = report.UsageReporter(
        Path("unused"),
        report.parse_utc(START),
        report.parse_utc(END),
        report.UTC,
        "UTC",
        "subtree",
        ROOT_THREAD,
        "none",
        False,
        token_only_projection=True,
        max_selected_text_chars=0,
        shared_text_pool=shared_text_pool,
        shared_attribution_text=shared_attribution_text,
    )
    first = current._account_output_text(
        {
            "thread_id": "root",
            "provider_call_id": "call-1",
            "provider": "m",
            "requested_model": "m",
        }
    )
    _assert(
        current.output_text_chars == 1,
        "only model-attribution text consumes the shared output budget",
    )
    mirrored = historical._account_output_text(
        {
            "thread_id": "root",
            "provider_call_id": "call-1",
            "provider": "m",
            "requested_model": "m",
        }
    )
    _assert(
        historical.output_text_chars == 0
        and mirrored["thread_id"] is first["thread_id"]
        and mirrored["provider_call_id"] is first["provider_call_id"]
        and mirrored["requested_model"] is first["requested_model"],
        "mirrored identity and model strings reuse the shared pool",
    )
    try:
        historical._account_output_text({"actual_model_used": "x"})
    except report.WorkLimitReached as exc:
        _assert(
            exc.reason == "selected_attribution_text_limit_exceeded",
            "new model attribution beyond the remaining budget fails closed",
        )
    else:
        raise AssertionError("shared attribution budget was exceeded")
    try:
        current._account_output_text({"thread_id": "t" * 129})
    except report.WorkLimitReached as exc:
        _assert(
            exc.reason == "source_identity_field_limit_exceeded",
            "internal identifiers remain bounded without charging output text",
        )
    else:
        raise AssertionError("internal identity field exceeded its fixed bound")


def _check_report_shape(database: Path) -> dict[str, Any]:
    before = hashlib.sha256(database.read_bytes()).hexdigest()
    result, diagnostic = _run_report(database)
    after = hashlib.sha256(database.read_bytes()).hexdigest()
    _assert(before == after, "read-only report changed its selected ledger")
    _assert(result["status"] == "incomplete", "unknown actual model remains explicit")
    _assert(
        result["summary"]["provider_call_count"] == 5,
        "only spawn-confirmed tree calls are included",
    )
    _assert(
        result["scope_contributions"]["own"]["provider_call_count"] == 1,
        "root own-call count",
    )
    _assert(
        result["scope_contributions"]["descendants_excluding_anchor"][
            "provider_call_count"
        ]
        == 4,
        "all descendant-call count",
    )
    _assert(
        result["lineage_coverage"]["unresolved_thread_count"] == 0,
        "only exact spawn-confirmed descendants participate",
    )
    _assert(
        result["lineage_coverage"]["known_zero_call_thread_count"] == 1,
        "zero-call leaf remains visible",
    )
    serialized = json.dumps(result, sort_keys=True)
    _assert(
        ROOT_THREAD in serialized,
        "the exact requested anchor remains identifiable",
    )
    for descendant_id in (
        "synthetic-child",
        "synthetic-grandchild",
        "synthetic-zero-call-leaf",
    ):
        _assert(descendant_id not in serialized, "descendant identifiers are omitted")
    _assert(
        len(result["threads"]) == 1
        and result["threads"][0]["thread_role"] == "requested_root",
        "subtree output exposes only the requested root row",
    )
    _assert(
        "synthetic-root-tagged-detached-child" not in serialized,
        "detached root-tagged rows are not included without a spawn edge",
    )
    _assert(
        result["resolved_root_buckets"]["requested_root"]["provider_call_count"] == 5,
        "resolved root and descendant totals retain their safe aggregate",
    )
    model_split = result["scope_contributions"]["root_vs_descendants_by_actual_model"]
    root_by_model = {
        item["actual_model_used"]: item for item in model_split["requested_root"]
    }
    descendants_by_model = {
        item["actual_model_used"]: item for item in model_split["all_descendants"]
    }
    _assert(
        all(
            "rate_id_call_counts" not in item.get("actual_mode", {})
            and "policy_id_call_counts" not in item.get("actual_mode", {})
            and "rate_id_call_counts" not in item.get("standard_scenario", {})
            for item in (*root_by_model.values(), *descendants_by_model.values())
        ),
        "root and descendant model partitions remain compact",
    )
    _assert(
        root_by_model["synthetic-alpha"]["provider_call_count"] == 1
        and root_by_model["synthetic-alpha"]["token_sums_known_subtotal"][
            "uncached_input_tokens"
        ]
        == 7,
        "requested root usage remains separated by actual model",
    )
    _assert(
        descendants_by_model["synthetic-alpha"]["provider_call_count"] == 2
        and descendants_by_model["synthetic-alpha"]["token_sums_known_subtotal"][
            "uncached_input_tokens"
        ]
        == 16,
        "all nested descendants aggregate by actual model",
    )
    _assert(
        descendants_by_model["synthetic-beta"]["token_sums_known_subtotal"][
            "uncached_input_tokens"
        ]
        == 11
        and descendants_by_model[None]["provider_call_count"] == 1,
        "descendant model partitions preserve distinct and unavailable identities",
    )

    summary_models = _by_model(result["summary"])
    _assert(
        all(
            "rate_id_call_counts" not in item.get("actual_mode", {})
            and "policy_id_call_counts" not in item.get("actual_mode", {})
            and "rate_id_call_counts" not in item.get("standard_scenario", {})
            for item in summary_models.values()
        ),
        "per-model partitions omit repeated rate identifiers",
    )
    _assert(
        bool(result["credit_evidence"]["rates"]),
        "global rate evidence remains available once",
    )
    _assert(
        summary_models["synthetic-alpha"]["token_sums_known_subtotal"][
            "uncached_input_tokens"
        ]
        == 23,
        "alpha model tokens across root and descendants",
    )
    _assert(
        summary_models["synthetic-beta"]["token_sums_known_subtotal"][
            "uncached_input_tokens"
        ]
        == 11,
        "beta model tokens",
    )
    _assert(
        summary_models[None]["token_sums_known_subtotal"]["uncached_input_tokens"] == 2,
        "missing actual model has its own bucket",
    )
    _assert(
        summary_models["<unavailable>"]["token_sums_known_subtotal"][
            "uncached_input_tokens"
        ]
        == 1,
        "a literal model name cannot collide with the missing-model bucket",
    )
    model_evidence = {
        item["actual_model_used"]: item["call_count"]
        for item in result["summary"]["actual_model_evidence"]
    }
    _assert(
        model_evidence[None] == 1,
        "missing actual-model evidence remains a distinct null bucket",
    )
    _assert(
        model_evidence["<unavailable>"] == 1,
        "literal model names are not folded into a missing bucket",
    )
    own_models = _by_model(result["scope_contributions"]["own"])
    descendant_models = _by_model(
        result["scope_contributions"]["descendants_excluding_anchor"]
    )
    _assert(
        own_models["synthetic-alpha"]["token_sums_known_subtotal"][
            "uncached_input_tokens"
        ]
        == 7,
        "root model split",
    )
    _assert(
        descendant_models["synthetic-alpha"]["token_sums_known_subtotal"][
            "uncached_input_tokens"
        ]
        == 16,
        "nested and root-tagged descendant model split",
    )
    _assert(
        sum(
            item["token_sums_known_subtotal"]["uncached_input_tokens"]
            for item in summary_models.values()
        )
        == result["summary"]["token_sums_known_subtotal"]["uncached_input_tokens"],
        "per-model token totals reconcile",
    )

    coverage = result["source_coverage"]
    _assert(
        coverage["other_ledgers"] == "not queried or merged",
        "older ledgers are not silently claimed",
    )
    _assert(
        coverage["physical_call_deduplication"]
        == "rows in the selected ledger are counted as stored; no content-based or cross-ledger mirror reconciliation is attempted",
        "dedupe boundary is explicit",
    )
    _assert(
        "provider-call requested model and effective actual model are distinct"
        in coverage["model_identity"],
        "requested/effective identity distinction is explicit",
    )
    _assert(
        coverage["requested_window_utc"]["start_inclusive"]
        == "1970-01-01T00:00:00.000000Z",
        "requested report start is explicit",
    )
    _assert(
        coverage["selected_window_provider_call_bounds_utc"]["earliest_started_at_utc"]
        == "2026-09-30T00:01:00.000000Z",
        "observed lower date bound in selected window",
    )
    _assert(
        coverage["selected_window_provider_call_bounds_utc"]["latest_started_at_utc"]
        == "2026-09-30T00:04:00.000000Z",
        "detached root-tag-only calls are excluded from the exact tree",
    )
    _assert(
        diagnostic.get("selected_call_rows") == 5,
        "diagnostics report selected row count",
    )
    indexed_plans = [
        detail
        for query in diagnostic.get("query_plans", [])
        for detail in query.get("details", [])
    ]
    _assert(
        any(
            "USING INDEX usage_spawn_requests_parent_idx" in detail
            for detail in indexed_plans
        ),
        "normal family membership uses the exact root index",
    )
    return result


def _check_indexed_spawn_walk_without_optional_call_indexes(temporary: Path) -> None:
    legacy = temporary / "legacy-no-reporting-indexes.sqlite"
    _create_database(legacy, current_indexes=False)
    result, diagnostic = _run_report(legacy)
    _assert(
        result["summary"]["provider_call_count"] == 5,
        "spawn-index walk retains exact-root fixture calls",
    )
    _assert(
        result["lineage_coverage"]["strategy"] == "indexed_spawn_request_breadth_first",
        "lineage uses the indexed spawn-request frontier",
    )
    plans = [
        detail
        for query in diagnostic.get("query_plans", [])
        for detail in query.get("details", [])
    ]
    _assert(
        not any("SCAN usage_threads" in detail for detail in plans),
        "lineage never falls back to scanning usage_threads",
    )
    _assert(
        any(
            "USING INDEX usage_provider_calls_thread_idx" in detail for detail in plans
        ),
        "exact-thread call fallback uses the existing thread index",
    )



def _check_call_limit_partial(temporary: Path) -> None:
    database = temporary / "call-limit-partial.sqlite"
    _create_database(database, current_indexes=True)
    reporter = report.UsageReporter(
        database,
        report.parse_utc(START),
        report.parse_utc(END),
        report.UTC,
        "UTC",
        "subtree",
        ROOT_THREAD,
        "none",
        False,
        max_calls=2,
    )
    result = reporter.run()
    _assert(
        result["status"] == "incomplete"
        and "call_limit_exceeded" in result["incomplete_reasons"],
        "selected-call cap is visible without discarding its bounded subtotal",
    )
    _assert(
        reporter.selected_rows == 2
        and result["summary"]["provider_call_count"] == 2,
        "only rows below the fixed call limit contribute to a partial report",
    )
    _assert(
        result["source_coverage"]["call_selection_complete"] is False,
        "single-ledger output distinguishes partial rows from complete selection",
    )


def _check_cycle_is_incomplete(temporary: Path) -> None:
    cycle = temporary / "lineage-cycle.sqlite"
    _create_database(cycle, current_indexes=True)
    connection = sqlite3.connect(cycle)
    try:
        connection.execute(
            """
            INSERT INTO usage_threads(
                thread_id, parent_thread_id, root_thread_id
            ) VALUES ('cycle-a', ?, ?)
            """,
            (ROOT_THREAD, ROOT_THREAD),
        )
        connection.execute(
            """
            INSERT INTO usage_threads(
                thread_id, parent_thread_id, root_thread_id
            ) VALUES ('cycle-b', 'cycle-a', ?)
            """,
            (ROOT_THREAD,),
        )
        connection.execute(
            "UPDATE usage_threads SET parent_thread_id = 'cycle-b' WHERE thread_id = ?",
            (ROOT_THREAD,),
        )
        connection.executemany(
            "INSERT INTO usage_spawn_requests(spawn_request_id, parent_thread_id, child_thread_id, status) VALUES (?, ?, ?, 'complete')",
            (
                ("cycle-root-a", ROOT_THREAD, "cycle-a"),
                ("cycle-a-b", "cycle-a", "cycle-b"),
                ("cycle-b-root", "cycle-b", ROOT_THREAD),
            ),
        )
        connection.commit()
    finally:
        connection.close()
    result, _diagnostic = _run_report(cycle)
    _assert(
        "lineage_cycle" in result["incomplete_reasons"],
        "cycles remain an explicit incomplete condition",
    )


def _check_streaming_query_indexes(temporary: Path) -> None:
    database = temporary / "streaming-index-plans.sqlite"
    _create_database(database, current_indexes=True)
    reporter = report.UsageReporter(
        database,
        report.parse_utc(START),
        report.parse_utc(END),
        report.UTC,
        "UTC",
        "subtree",
        ROOT_THREAD,
        "none",
        True,
        all_time=True,
        token_only_projection=True,
    )
    try:
        state = _prepare_stream_reporter(reporter)
        scope_ids = sorted(state["scope_nodes"])
        ids = reporter._selected_token_call_ids(scope_ids)
        _assert("root-alpha" in ids, "exact thread selection collects stable IDs only")
        row = next(reporter._iter_token_calls(scope_ids, reporter.selected_rows))
        probe = reporter._probe_token_call(str(row["provider_call_id"]))
        _assert(probe is not None, "stable call ID probe finds one exact row")
        details = [
            detail
            for query in reporter.query_plans
            for detail in query["details"]
        ]
        _assert(
            any("SEARCH" in detail and "usage_provider_calls" in detail for detail in details),
            "the selected-call stream uses an exact thread index",
        )
        _assert(
            any("SEARCH" in detail and "provider_call_id" in detail for detail in details),
            "cross-ledger reconciliation uses an indexed provider-call ID probe",
        )
    finally:
        if reporter.conn is not None:
            reporter.conn.rollback()
            reporter.conn.close()
            reporter.conn = None


def _check_streaming_missing_call_id(temporary: Path) -> None:
    database = temporary / "streaming-missing-call-id.sqlite"
    _create_database(database, current_indexes=True)
    connection = sqlite3.connect(database)
    try:
        connection.execute(
            "UPDATE usage_provider_calls SET provider_call_id = NULL WHERE provider_call_id = ?",
            ("root-alpha",),
        )
        connection.commit()
    finally:
        connection.close()
    result = report._run_multi_ledger_report(
        SimpleNamespace(
            database=database,
            historical_database=None,
            all_time=True,
            start=report.parse_utc(START),
            end=report.parse_utc(END),
            timezone="UTC",
            timezone_value=report.UTC,
            scope="subtree",
            thread_id=ROOT_THREAD,
            credit_mode="none",
            diagnostics=False,
        )
    )
    _assert(
        result["status"] == "incomplete"
        and "physical_call_deduplication_incomplete" in result["incomplete_reasons"],
        "a selected call with no stable ID is reported as incomplete",
    )
    _assert(
        result["deduplication"]["rows_excluded_for_missing_call_id"] == 1,
        "missing stable IDs remain an explicit exclusion count",
    )


def _prepare_stream_reporter(reporter: report.UsageReporter) -> dict[str, Any]:
    return report._prepare_exact_root_reporter(reporter, ROOT_THREAD)


def _check_multi_ledger_reconciliation(temporary: Path) -> None:
    current = temporary / "current-all-time.sqlite"
    historical = temporary / "historical-all-time.sqlite"
    _create_database(current, current_indexes=True)
    _create_database(historical, current_indexes=False)
    for path in (current, historical):
        connection = sqlite3.connect(path)
        try:
            connection.execute(
                "UPDATE usage_provider_calls SET spawn_request_id = ? WHERE provider_call_id = ?",
                ("spawn-sentinel", "child-sentinel-literal"),
            )
            connection.execute(
                "UPDATE usage_spawn_requests SET requested_model = ?, requested_reasoning_effort = ? WHERE spawn_request_id = ?",
                ("configured-beta", "medium", "spawn-sentinel"),
            )
            connection.commit()
        finally:
            connection.close()
    # Billing-only text is deliberately above the selected-text field limit;
    # the token projection must not materialize it.
    for path in (current, historical):
        wide_row_connection = sqlite3.connect(path)
        try:
            wide_row_connection.execute(
                "UPDATE usage_provider_calls SET account_plan = ? WHERE provider_call_id = ?",
                ("x" * 9_000, "root-alpha"),
            )
            wide_row_connection.execute(
                "UPDATE usage_threads SET agent_nickname = ? WHERE thread_id = ?",
                ("x" * 9_000, ROOT_THREAD),
            )
            wide_row_connection.commit()
        finally:
            wide_row_connection.close()
    historical_connection = sqlite3.connect(historical)
    try:
        # Five exact mirrors, one conflicting call ID, and one genuinely new call.
        historical_connection.execute(
            "UPDATE usage_provider_calls SET thread_id = ? WHERE provider_call_id = ?",
            ("synthetic-root-tagged-detached-child", "root-alpha"),
        )
        historical_connection.execute(
            "UPDATE usage_provider_calls SET output_tokens = 99 WHERE provider_call_id = ?",
            ("child-beta",),
        )
        historical_connection.execute(
            "UPDATE usage_spawn_requests SET requested_model = ? WHERE spawn_request_id = ?",
            ("legacy-configured-beta", "spawn-sentinel"),
        )
        historical_connection.execute(
            "INSERT INTO usage_provider_calls(provider_call_id, thread_id, spawn_request_id, provider, requested_model, actual_model_used, requested_service_tier, actual_service_tier, actual_service_tier_source, fast_mode_used, billing_surface, account_plan, started_at, completed_at, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, provider_reported_credits, status) SELECT ?, thread_id, spawn_request_id, provider, requested_model, actual_model_used, requested_service_tier, actual_service_tier, actual_service_tier_source, fast_mode_used, billing_surface, account_plan, started_at, completed_at, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, provider_reported_credits, status FROM usage_provider_calls WHERE provider_call_id = ?",
            ("historical-only", "grandchild-alpha"),
        )
        historical_connection.commit()
    finally:
        historical_connection.close()

    before = {
        path: hashlib.sha256(path.read_bytes()).hexdigest()
        for path in (current, historical)
    }
    result = _run_multi_report(current, historical)
    after = {
        path: hashlib.sha256(path.read_bytes()).hexdigest()
        for path in (current, historical)
    }
    _assert(before == after, "multi-ledger report leaves both inputs unchanged")
    _assert(result["status"] == "incomplete", "call-ID conflict is not guessed")
    _assert(
        result["window"]["kind"] == "all_time",
        "the report uses unbounded exact-subtree selection",
    )
    _assert(
        [source["source"] for source in result["source_coverage"]["ledger_sources"]]
        == ["current", "historical"],
        "the two explicit source roles are preserved",
    )
    dedupe = result["deduplication"]
    _assert(dedupe["selected_provider_call_rows"] == 10, "both indexed selections are counted")
    _assert(
        dedupe["identical_mirrored_rows_collapsed"] == 3,
        "three exact in-scope mirrored rows are each counted once",
    )
    _assert(dedupe["conflicting_call_id_count"] == 1, "conflicting call IDs are surfaced")
    _assert(dedupe["rows_excluded_for_conflicting_call_ids"] == 2, "both conflict copies are excluded")
    _assert(
        dedupe["configured_attribution_conflict_provider_call_count"] == 1,
        "conflicting spawn configuration is counted without selecting either value",
    )
    _assert(
        dedupe["source_selection_complete"] is True,
        "cross-ledger reconciliation is complete when both bounded sources are complete",
    )
    _assert(
        all(
            "source_text_field_limit_exceeded" not in source["incomplete_reasons"]
            for source in result["source_coverage"]["ledger_sources"]
        ),
        "unused billing text is not selected by the all-time token projection",
    )
    _assert(
        result["source_coverage"]["limits"]["selected_attribution_text_characters"]
        == 1_000_000,
        "the dual-ledger projection retains its original small attribution budget",
    )
    _assert(
        result["source_coverage"]["limits"]["selected_attribution_text_accounting"]
        == (
            "unique requested, actual, configured model and effort strings "
            "are interned and budgeted once; identity, lineage, provider, "
            "timestamp and status strings use fixed per-field bounds and "
            "selected-row limits"
        ),
        "the shared budget measures only output model attribution strings",
    )
    _assert(result["summary"]["provider_call_count"] == 5, "only unambiguous calls contribute")
    _assert(
        result["summary"]["token_sums_known_subtotal"]["uncached_input_tokens"] == 36,
        "exact mirror is counted once and conflicting call tokens are excluded",
    )
    _assert(result["scope_totals"]["requested_root"]["provider_call_count"] == 1, "root usage stays separate")
    _assert(
        result["scope_totals"]["all_descendants"]["provider_call_count"] == 4,
        "only lineage-resolved nested descendants are included",
    )
    _assert(
        result["scope_totals"]["conflicting_root"]["provider_call_count"] == 0,
        "detached root-tag-only records are excluded",
    )
    _assert(
        result["scope_totals"]["unresolved_lineage"]["provider_call_count"] == 0,
        "recorded-root conflicts are distinct from missing lineage",
    )
    by_model = {
        item["actual_model_used"]: item for item in result["by_actual_model"]
    }
    sentinel = by_model["<unavailable>"]["configured_model_effort_evidence"]
    _assert(
        sentinel[0]["configured_model_from_spawn_request"] is None
        and sentinel[0]["provider_requested_model"] == "request-sentinel"
        and sentinel[0]["provider_actual_model"] == "<unavailable>",
        "configured, provider-requested, and effective models stay distinct without guessing conflicting configuration",
    )
    serialized = json.dumps(result, sort_keys=True)
    for value in ("root-alpha", "child-beta", "historical-only", "synthetic-child"):
        _assert(value not in serialized, "call and descendant identifiers stay private")


def _check_multi_ledger_postprocessing_deadline() -> None:
    checks = 0

    def deadline_after_one_step() -> None:
        nonlocal checks
        checks += 1
        if checks > 5:
            raise report.WorkLimitReached("deadline_exceeded")

    try:
        report._merge_exact_root_ledgers(
            [
                (
                    "current",
                    [
                        {
                            "provider_call_id": "synthetic-call",
                            "thread_id": ROOT_THREAD,
                        },
                        {
                            "provider_call_id": "synthetic-call",
                            "thread_id": ROOT_THREAD,
                        },
                    ],
                )
            ],
            ROOT_THREAD,
            check_deadline=deadline_after_one_step,
        )
    except report.WorkLimitReached as exc:
        _assert(
            exc.reason == "deadline_exceeded",
            "deadline interrupts mirrored-call reconciliation",
        )
    else:
        raise AssertionError("multi-ledger merge ignored its shared deadline")

    checks = 0
    output = io.StringIO()
    try:
        report._write_bounded_json(
            {"synthetic": [1, 2, 3]},
            output,
            deadline_after_one_step,
        )
    except report.WorkLimitReached as exc:
        _assert(exc.reason == "deadline_exceeded", "serialization deadline is explicit")
        _assert(output.getvalue() == "", "deadline failure does not emit partial JSON")
    else:
        raise AssertionError("multi-ledger serialization ignored its shared deadline")


def _check_multi_ledger_partial_selection(temporary: Path) -> None:
    current = temporary / "current-partial.sqlite"
    historical = temporary / "historical-partial.sqlite"
    _create_database(current, current_indexes=True)
    _create_database(historical, current_indexes=False)
    original_call_limit = report.MAX_CALLS
    report.MAX_CALLS = 4
    try:
        result = report._run_multi_ledger_report(
            SimpleNamespace(
                database=current,
                historical_database=historical,
                all_time=True,
                start=report.parse_utc(START),
                end=report.parse_utc(END),
                timezone="UTC",
                timezone_value=report.UTC,
                scope="subtree",
                thread_id=ROOT_THREAD,
                credit_mode="none",
                diagnostics=False,
            )
        )
    finally:
        report.MAX_CALLS = original_call_limit
    sources = result["source_coverage"]["ledger_sources"]
    _assert(
        result["status"] == "incomplete"
        and "cross_ledger_reconciliation_limited_by_source_coverage"
        in result["incomplete_reasons"],
        "partial source selections cannot be presented as reconciled totals",
    )
    _assert(
        [source["selected_provider_call_rows"] for source in sources] == [2, 2],
        "the strict aggregate call ceiling is split across explicit ledger sources",
    )
    _assert(
        all(
            source["call_selection_complete"] is False
            and source["status"] == "incomplete"
            for source in sources
        ),
        "each truncated source exposes its own incomplete selection",
    )
    _assert(
        result["deduplication"]["source_selection_complete"] is False,
        "cross-ledger deduplication completeness is explicit",
    )


def _check_requested_subtree_without_recorded_root(temporary: Path) -> None:
    database = temporary / "subtree-without-recorded-root.sqlite"
    _create_database(database, current_indexes=True)
    connection = sqlite3.connect(database)
    try:
        connection.execute("UPDATE usage_threads SET root_thread_id = NULL")
        connection.execute(
            "UPDATE usage_threads SET root_thread_id = ? WHERE thread_id = ?",
            (ROOT_THREAD, "synthetic-root-tagged-detached-child"),
        )
        connection.commit()
    finally:
        connection.close()

    result = report._run_multi_ledger_report(
        SimpleNamespace(
            database=database,
            historical_database=None,
            all_time=True,
            start=report.parse_utc(START),
            end=report.parse_utc(END),
            timezone="UTC",
            timezone_value=report.UTC,
            scope="subtree",
            thread_id=ROOT_THREAD,
            credit_mode="none",
            diagnostics=False,
        )
    )
    _assert(
        result["status"] == "complete"
        and "lineage_incomplete" not in result["incomplete_reasons"],
        "the indexed spawn path proves descendants without recorded root tags",
    )
    _assert(
        result["scope_totals"]["requested_root"]["provider_call_count"] == 1,
        "the requested anchor remains separate without a stored self-root",
    )
    _assert(
        result["scope_totals"]["all_descendants"]["provider_call_count"] == 4,
        "spawn-confirmed nested descendants remain separate from the anchor",
    )
    _assert(
        result["scope_totals"]["unresolved_lineage"]["provider_call_count"] == 0,
        "unrecorded reachable descendants are separated from resolved role buckets",
    )
    _assert(
        result["scope_totals"]["conflicting_root"]["provider_call_count"] == 0,
        "a disconnected root-tagged record is not misattributed as a descendant",
    )
    serialized = json.dumps(result, sort_keys=True)
    for call_id in (
        "root-alpha",
        "child-beta",
        "child-sentinel-literal",
        "grandchild-alpha",
        "grandchild-unavailable",
    ):
        _assert(call_id not in serialized, "private provider-call identifiers stay omitted")


def _check_large_subtree_uses_compact_projection(temporary: Path) -> None:
    database = temporary / "large-subtree-compact-projection.sqlite"
    _create_database(database, current_indexes=True)
    connection = sqlite3.connect(database)
    try:
        children = tuple(
            (f"synthetic-detached-{index:04d}", ROOT_THREAD, ROOT_THREAD)
            for index in range(report.MAX_OUTPUT_GROUPS + 1)
        )
        connection.executemany(
            "INSERT INTO usage_threads(thread_id, parent_thread_id, root_thread_id) VALUES (?, ?, ?)",
            children,
        )
        connection.executemany(
            "INSERT INTO usage_spawn_requests(spawn_request_id, parent_thread_id, child_thread_id, status) VALUES (?, ?, ?, 'complete')",
            (
                (f"spawn-large-{index:04d}", ROOT_THREAD, f"synthetic-detached-{index:04d}")
                for index in range(report.MAX_OUTPUT_GROUPS + 1)
            ),
        )
        connection.commit()
    finally:
        connection.close()

    result = report._run_multi_ledger_report(
        SimpleNamespace(
            database=database,
            historical_database=None,
            all_time=True,
            start=report.parse_utc(START),
            end=report.parse_utc(END),
            timezone="UTC",
            timezone_value=report.UTC,
            scope="subtree",
            thread_id=ROOT_THREAD,
            credit_mode="none",
            diagnostics=False,
        )
    )
    source = result["source_coverage"]["ledger_sources"][0]
    _assert(
        source["thread_count"] > report.MAX_OUTPUT_GROUPS
        and source["lineage_snapshot_complete"] is True,
        "bounded token projection accepts more participants than output-group limit",
    )
    _assert(
        "output_group_limit_exceeded" not in result["incomplete_reasons"],
        "compact root/subagent aggregation does not fail on omitted per-thread rows",
    )
    serialized = json.dumps(result, sort_keys=True)
    _assert(
        "synthetic-detached-0000" not in serialized
        and "synthetic-detached-1000" not in serialized,
        "large subtree output does not serialize participant identifiers",
    )


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="codex-usage-exact-root-") as directory:
        temporary = Path(directory)
        current = temporary / "current-indexes.sqlite"
        _create_database(current, current_indexes=True)
        _check_report_shape(current)
        _check_indexed_spawn_walk_without_optional_call_indexes(temporary)
        _check_call_limit_partial(temporary)
        _check_cycle_is_incomplete(temporary)
        _check_streaming_query_indexes(temporary)
        _check_streaming_missing_call_id(temporary)
        _check_shared_text_pool()
        _check_multi_ledger_reconciliation(temporary)
        _check_multi_ledger_postprocessing_deadline()
        _check_multi_ledger_partial_selection(temporary)
        _check_requested_subtree_without_recorded_root(temporary)
        _check_large_subtree_uses_compact_projection(temporary)
    print(json.dumps({"status": "passed", "cases": 10}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
