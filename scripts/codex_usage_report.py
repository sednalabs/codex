#!/usr/bin/env python3
"""Bounded, read-only reports over one explicitly selected Codex usage ledger."""

from __future__ import annotations

import argparse
import datetime as dt
import decimal
import json
import math
import re
import sqlite3
import sys
import time
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable, Iterator, Sequence
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

try:
    import resource
except ImportError:  # pragma: no cover - unavailable on Windows
    resource = None


REPORT_VERSION = "codex-local-usage-report/v1"
MAX_SECONDS = 2.0
MAX_VM_INSTRUCTIONS = 8_000_000
MAX_CALLS = 25_000
MAX_LINEAGE_NODES = 4_096
MAX_LINEAGE_DEPTH = 128
MAX_RATE_POLICY_ROWS = 512
MAX_OUTPUT_GROUPS = 1_000
MAX_OUTPUT_BYTES = 16 * 1024 * 1024
MAX_TEXT_FIELD_CHARS = 8_192
MAX_OUTPUT_TEXT_CHARS = 1_000_000
MAX_SQLITE_ROW_BYTES = 1024 * 1024
SQL_CHUNK = 250
PROGRESS_INTERVAL = 1_000

STARTED_INDEX = "usage_provider_calls_started_at_idx"
THREAD_STARTED_INDEX = "usage_provider_calls_thread_started_at_idx"
PARENT_INDEX = "usage_threads_reporting_parent_idx"
PARENT_EXPR = "COALESCE(NULLIF(parent_thread_id,''), NULLIF(fork_parent_thread_id,''))"

CALL_COLUMNS = (
    "provider_call_id, thread_id, provider, requested_model, actual_model_used, "
    "requested_service_tier, actual_service_tier, actual_service_tier_source, "
    "fast_mode_used, billing_surface, account_plan, started_at, completed_at, "
    "input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, "
    "output_tokens, total_tokens, provider_reported_credits, status"
)
THREAD_COLUMNS = (
    "thread_id, parent_thread_id, root_thread_id, fork_parent_thread_id, "
    "agent_nickname, agent_role, source, created_at"
)
RATE_COLUMNS = (
    "rate_id, provider, model, service_tier, speed_mode, rate_card_kind, "
    "credits_per_1m_uncached_input, credits_per_1m_cached_input, "
    "credits_per_1m_output, effective_from, effective_to, source_url, "
    "source_observed_at"
)
POLICY_COLUMNS = (
    "policy_id, provider, billing_surface, account_plan, rate_card_kind, "
    "effective_from, effective_to, source_url, source_observed_at"
)
TOKEN_FIELDS = {
    "uncached_input_tokens": "input_tokens_uncached",
    "cached_input_tokens": "input_tokens_cached",
    "cache_write_tokens": "input_tokens_cache_write",
    "output_tokens": "output_tokens",
    "total_tokens": "total_tokens",
}
DECIMAL_CONTEXT = decimal.Context(prec=38, rounding=decimal.ROUND_HALF_EVEN)
ZERO = decimal.Decimal(0)
UTC = dt.timezone.utc


class ReportProblem(Exception):
    def __init__(self, status: str, reason: str, message: str = "") -> None:
        super().__init__(message or reason)
        self.status = status
        self.reason = reason
        self.message = message or reason


class WorkLimitReached(ReportProblem):
    def __init__(self, reason: str) -> None:
        super().__init__("incomplete", reason)


def _blank_to_none(value: Any) -> str | None:
    if value is None:
        return None
    text = str(value)
    return text if text.strip() else None


def parse_utc(value: str) -> dt.datetime:
    raw = value.strip()
    if raw.endswith("Z"):
        raw = raw[:-1] + "+00:00"
    try:
        parsed = dt.datetime.fromisoformat(raw)
    except ValueError as exc:
        raise ValueError(
            "timestamp must be ISO-8601 with an explicit UTC offset"
        ) from exc
    if parsed.tzinfo is None or parsed.utcoffset() != dt.timedelta(0):
        raise ValueError("timestamp must use UTC")
    return parsed.astimezone(UTC)


def _iso_utc(value: dt.datetime) -> str:
    return (
        value.astimezone(UTC).isoformat(timespec="microseconds").replace("+00:00", "Z")
    )


def _supported_utc_text(value: str) -> dt.datetime:
    if not (value.endswith("Z") or value.endswith("+00:00")):
        raise ValueError("timestamp encoding is not a qualified UTC writer encoding")
    parsed = parse_utc(value)
    return parsed


def _seek_bounds(start: dt.datetime, end: dt.datetime) -> tuple[str, str]:
    # Include a conservative whole-second envelope because qualified writer
    # rows use both Z and +00:00 suffixes and variable fractional precision.
    start_floor = start.replace(microsecond=0) - dt.timedelta(seconds=1)
    end_floor = end.replace(microsecond=0)
    end_ceiling = end_floor + dt.timedelta(seconds=2)
    return (
        start_floor.strftime("%Y-%m-%dT%H:%M:%SZ"),
        end_ceiling.strftime("%Y-%m-%dT%H:%M:%SZ"),
    )


def _decimal(value: Any) -> decimal.Decimal:
    if isinstance(value, decimal.Decimal):
        return value
    if value is None:
        return ZERO
    return DECIMAL_CONTEXT.create_decimal(str(value))


def _nonnegative_decimal(value: Any) -> decimal.Decimal | None:
    if value is None:
        return None
    try:
        result = DECIMAL_CONTEXT.create_decimal(str(value))
    except (decimal.DecimalException, ValueError, TypeError):
        return None
    if not result.is_finite() or result < 0:
        return None
    return result


def _nonnegative_integer(value: Any) -> int | None:
    if value is None or isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value if value >= 0 else None
    if isinstance(value, float):
        return (
            int(value)
            if math.isfinite(value) and value >= 0 and value.is_integer()
            else None
        )
    text = str(value).strip()
    if not re.fullmatch(r"[0-9]+", text):
        return None
    try:
        return int(text)
    except ValueError:
        return None


def _credit_string(value: decimal.Decimal) -> str:
    if not value:
        return "0"
    text = format(value, "f")
    if "." in text:
        text = text.rstrip("0").rstrip(".")
    return text


def _key(value: Any) -> str | None:
    text = _blank_to_none(value)
    return text.lower() if text is not None else None


def _sql_chunks(
    values: Sequence[str], size: int = SQL_CHUNK
) -> Iterator[Sequence[str]]:
    for offset in range(0, len(values), size):
        yield values[offset : offset + size]


def _add_bounded_key(keys: set[Any], key: Any, limit: int, reason: str) -> None:
    if key not in keys and len(keys) >= limit:
        raise WorkLimitReached(reason)
    keys.add(key)


def _sqlite_authorizer(
    action: int,
    arg1: str | None,
    arg2: str | None,
    _database: str | None,
    _trigger: str | None,
) -> int:
    deny_names = (
        "SQLITE_INSERT",
        "SQLITE_UPDATE",
        "SQLITE_DELETE",
        "SQLITE_CREATE_INDEX",
        "SQLITE_CREATE_TABLE",
        "SQLITE_CREATE_TEMP_INDEX",
        "SQLITE_CREATE_TEMP_TABLE",
        "SQLITE_CREATE_TEMP_TRIGGER",
        "SQLITE_CREATE_TEMP_VIEW",
        "SQLITE_CREATE_TRIGGER",
        "SQLITE_CREATE_VIEW",
        "SQLITE_CREATE_VTABLE",
        "SQLITE_DROP_INDEX",
        "SQLITE_DROP_TABLE",
        "SQLITE_DROP_TEMP_INDEX",
        "SQLITE_DROP_TEMP_TABLE",
        "SQLITE_DROP_TEMP_TRIGGER",
        "SQLITE_DROP_TEMP_VIEW",
        "SQLITE_DROP_TRIGGER",
        "SQLITE_DROP_VIEW",
        "SQLITE_DROP_VTABLE",
        "SQLITE_ALTER_TABLE",
        "SQLITE_REINDEX",
        "SQLITE_ANALYZE",
        "SQLITE_ATTACH",
        "SQLITE_DETACH",
        "SQLITE_SAVEPOINT",
    )
    if any(action == getattr(sqlite3, name, object()) for name in deny_names):
        return sqlite3.SQLITE_DENY
    if action == getattr(sqlite3, "SQLITE_PRAGMA", -1):
        return sqlite3.SQLITE_DENY
    if action == getattr(sqlite3, "SQLITE_FUNCTION", -1):
        if (arg2 or arg1 or "").lower() in {"load_extension", "readfile", "writefile"}:
            return sqlite3.SQLITE_DENY
    return sqlite3.SQLITE_OK


def _new_metrics() -> dict[str, Any]:
    return {
        "provider_call_count": 0,
        "token_sums": {name: 0 for name in TOKEN_FIELDS},
        "token_missing_calls": {name: 0 for name in TOKEN_FIELDS},
        "actual_model_evidence": defaultdict(int),
        "model_identity_evidence": defaultdict(int),
        "actual_service_tier_evidence": defaultdict(int),
        "service_tier_source_evidence": defaultdict(int),
        "fast_mode_used_evidence": defaultdict(int),
        "missing_usage_calls": 0,
        "provider_reported": {"call_count": 0, "credits_subtotal": ZERO},
        "actual_mode": {
            "covered_call_count": 0,
            "rate_estimated_call_count": 0,
            "uncovered_call_count": 0,
            "credits_subtotal": ZERO,
            "uncovered_reasons": defaultdict(int),
            "rate_id_counts": defaultdict(int),
            "policy_id_counts": defaultdict(int),
        },
        "standard_scenario": {
            "covered_call_count": 0,
            "uncovered_call_count": 0,
            "credits_subtotal": ZERO,
            "uncovered_reasons": defaultdict(int),
            "rate_id_counts": defaultdict(int),
        },
    }


def _add_call(
    metrics: dict[str, Any],
    call: dict[str, Any],
    actual: dict[str, Any],
    standard: dict[str, Any],
) -> None:
    metrics["provider_call_count"] += 1
    if call.get("status") != "ok":
        metrics["missing_usage_calls"] += 1
    observed_model = _blank_to_none(call.get("actual_model_used"))
    metrics["actual_model_evidence"][observed_model or "<unavailable>"] += 1
    metrics["model_identity_evidence"][
        (
            _blank_to_none(call.get("requested_model")),
            observed_model,
        )
    ] += 1
    observed_tier = _blank_to_none(call.get("actual_service_tier"))
    metrics["actual_service_tier_evidence"][observed_tier or "<unavailable>"] += 1
    tier_source = _blank_to_none(call.get("actual_service_tier_source"))
    metrics["service_tier_source_evidence"][tier_source or "<unavailable>"] += 1
    fast_mode = call.get("fast_mode_used")
    fast_evidence = (
        "fast" if fast_mode == 1 else "standard" if fast_mode == 0 else "<unavailable>"
    )
    metrics["fast_mode_used_evidence"][fast_evidence] += 1
    for output_name, column in TOKEN_FIELDS.items():
        value = call.get(column)
        if value is None:
            metrics["token_missing_calls"][output_name] += 1
        else:
            metrics["token_sums"][output_name] += int(value)

    reported = call.get("provider_reported_credits")
    if reported is not None:
        reported_amount = _nonnegative_decimal(reported)
        if reported_amount is not None:
            metrics["provider_reported"]["call_count"] += 1
            metrics["provider_reported"]["credits_subtotal"] += reported_amount

    if actual.get("covered"):
        metrics["actual_mode"]["covered_call_count"] += 1
        if actual.get("kind") == "rate_estimate":
            metrics["actual_mode"]["rate_estimated_call_count"] += 1
        metrics["actual_mode"]["credits_subtotal"] += actual["credits"]
        if actual.get("kind") == "rate_estimate":
            rate_id = actual["rate"].get("rate_id")
            if rate_id is not None:
                metrics["actual_mode"]["rate_id_counts"][str(rate_id)] += 1
            policy_id = actual["policy"].get("policy_id")
            if policy_id is not None:
                metrics["actual_mode"]["policy_id_counts"][str(policy_id)] += 1
    else:
        metrics["actual_mode"]["uncovered_call_count"] += 1
        metrics["actual_mode"]["uncovered_reasons"][actual["reason"]] += 1

    if standard.get("covered"):
        metrics["standard_scenario"]["covered_call_count"] += 1
        metrics["standard_scenario"]["credits_subtotal"] += standard["credits"]
        rate_id = standard["rate"].get("rate_id")
        if rate_id is not None:
            metrics["standard_scenario"]["rate_id_counts"][str(rate_id)] += 1
    else:
        metrics["standard_scenario"]["uncovered_call_count"] += 1
        metrics["standard_scenario"]["uncovered_reasons"][standard["reason"]] += 1


def _merge_metrics(items: Iterable[dict[str, Any]]) -> dict[str, Any]:
    merged = _new_metrics()
    for item in items:
        merged["provider_call_count"] += item["provider_call_count"]
        merged["missing_usage_calls"] += item["missing_usage_calls"]
        for model, count in item["actual_model_evidence"].items():
            merged["actual_model_evidence"][model] += count
        for identity, count in item["model_identity_evidence"].items():
            merged["model_identity_evidence"][identity] += count
        for evidence_key in (
            "actual_service_tier_evidence",
            "service_tier_source_evidence",
            "fast_mode_used_evidence",
        ):
            for evidence, count in item[evidence_key].items():
                merged[evidence_key][evidence] += count
        for key in merged["token_sums"]:
            merged["token_sums"][key] += item["token_sums"][key]
            merged["token_missing_calls"][key] += item["token_missing_calls"][key]
        merged["provider_reported"]["call_count"] += item["provider_reported"][
            "call_count"
        ]
        merged["provider_reported"]["credits_subtotal"] += item["provider_reported"][
            "credits_subtotal"
        ]
        for mode in ("actual_mode", "standard_scenario"):
            merged[mode]["covered_call_count"] += item[mode]["covered_call_count"]
            merged[mode]["uncovered_call_count"] += item[mode]["uncovered_call_count"]
            merged[mode]["credits_subtotal"] += item[mode]["credits_subtotal"]
            if mode == "actual_mode":
                merged[mode]["rate_estimated_call_count"] += item[mode][
                    "rate_estimated_call_count"
                ]
                for policy_id, count in item[mode]["policy_id_counts"].items():
                    merged[mode]["policy_id_counts"][policy_id] += count
            for rate_id, count in item[mode]["rate_id_counts"].items():
                merged[mode]["rate_id_counts"][rate_id] += count
            for reason, count in item[mode]["uncovered_reasons"].items():
                merged[mode]["uncovered_reasons"][reason] += count
    return merged


def _json_metrics(metrics: dict[str, Any]) -> dict[str, Any]:
    result: dict[str, Any] = {
        "provider_call_count": metrics["provider_call_count"],
        "token_sums_known_subtotal": metrics["token_sums"],
        "token_missing_call_count": metrics["token_missing_calls"],
        "missing_usage_call_count": metrics["missing_usage_calls"],
        "actual_model_evidence": [
            {
                "actual_model_used": None if model == "<unavailable>" else model,
                "call_count": count,
            }
            for model, count in sorted(metrics["actual_model_evidence"].items())
        ],
        "requested_to_actual_model_evidence": [
            {
                "requested_model": requested_model,
                "actual_model_used": actual_model,
                "call_count": count,
            }
            for (requested_model, actual_model), count in sorted(
                metrics["model_identity_evidence"].items(),
                key=lambda item: (
                    item[0][0] is None,
                    item[0][0] or "",
                    item[0][1] is None,
                    item[0][1] or "",
                ),
            )
        ],
        "actual_service_tier_evidence": dict(
            sorted(metrics["actual_service_tier_evidence"].items())
        ),
        "service_tier_source_evidence": dict(
            sorted(metrics["service_tier_source_evidence"].items())
        ),
        "fast_mode_used_evidence": dict(
            sorted(metrics["fast_mode_used_evidence"].items())
        ),
        "provider_reported_credits": {
            "call_count": metrics["provider_reported"]["call_count"],
            "credits_subtotal": _credit_string(
                metrics["provider_reported"]["credits_subtotal"]
            ),
            "complete": metrics["provider_reported"]["call_count"]
            == metrics["provider_call_count"],
        },
        "actual_mode": {
            "covered_call_count": metrics["actual_mode"]["covered_call_count"],
            "rate_estimated_call_count": metrics["actual_mode"][
                "rate_estimated_call_count"
            ],
            "uncovered_call_count": metrics["actual_mode"]["uncovered_call_count"],
            "credits_subtotal": _credit_string(
                metrics["actual_mode"]["credits_subtotal"]
            ),
            "complete_total": (
                _credit_string(metrics["actual_mode"]["credits_subtotal"])
                if metrics["actual_mode"]["covered_call_count"]
                == metrics["provider_call_count"]
                else None
            ),
            "uncovered_reasons": dict(
                sorted(metrics["actual_mode"]["uncovered_reasons"].items())
            ),
            "rate_id_call_counts": dict(
                sorted(metrics["actual_mode"]["rate_id_counts"].items())
            ),
            "policy_id_call_counts": dict(
                sorted(metrics["actual_mode"]["policy_id_counts"].items())
            ),
        },
        "standard_scenario": {
            "covered_call_count": metrics["standard_scenario"]["covered_call_count"],
            "uncovered_call_count": metrics["standard_scenario"][
                "uncovered_call_count"
            ],
            "credits_subtotal": _credit_string(
                metrics["standard_scenario"]["credits_subtotal"]
            ),
            "complete_total": (
                _credit_string(metrics["standard_scenario"]["credits_subtotal"])
                if metrics["standard_scenario"]["covered_call_count"]
                == metrics["provider_call_count"]
                else None
            ),
            "uncovered_reasons": dict(
                sorted(metrics["standard_scenario"]["uncovered_reasons"].items())
            ),
            "rate_id_call_counts": dict(
                sorted(metrics["standard_scenario"]["rate_id_counts"].items())
            ),
            "assumption": "OpenAI standard/default Codex token-rate scenario; not provider-reported billing, public API spend, or quota debit.",
        },
    }
    return result


def _rate_evidence(rate: dict[str, Any]) -> dict[str, Any]:
    fields = (
        "rate_id",
        "provider",
        "model",
        "service_tier",
        "speed_mode",
        "rate_card_kind",
        "credits_per_1m_uncached_input",
        "credits_per_1m_cached_input",
        "credits_per_1m_output",
        "effective_from",
        "effective_to",
        "source_url",
        "source_observed_at",
    )
    return {field: rate.get(field) for field in fields}


def _policy_evidence(policy: dict[str, Any]) -> dict[str, Any]:
    fields = (
        "policy_id",
        "provider",
        "billing_surface",
        "account_plan",
        "rate_card_kind",
        "effective_from",
        "effective_to",
        "source_url",
        "source_observed_at",
    )
    return {field: policy.get(field) for field in fields}


class UsageReporter:
    def __init__(
        self,
        database: Path,
        start: dt.datetime,
        end: dt.datetime,
        timezone: dt.tzinfo,
        timezone_name: str,
        scope: str,
        thread_id: str | None,
        credit_mode: str,
        diagnostics: bool,
    ) -> None:
        self.database = database
        self.start = start
        self.end = end
        self.timezone = timezone
        self.timezone_name = timezone_name
        self.scope = scope
        self.thread_id = thread_id
        self.credit_mode = credit_mode
        self.diagnostics_enabled = diagnostics
        self.deadline = time.monotonic() + MAX_SECONDS
        self.vm_instructions = 0
        self.vm_stop_reason: str | None = None
        self.query_plans: list[dict[str, Any]] = []
        self.selected_rows = 0
        self.rate_policy_rows = 0
        self.lineage_nodes = 0
        self.output_text_chars = 0
        self.conn: sqlite3.Connection | None = None

    def check_deadline(self) -> None:
        if time.monotonic() >= self.deadline:
            self.vm_stop_reason = "deadline_exceeded"
            raise WorkLimitReached("deadline_exceeded")

    def progress(self) -> int:
        self.vm_instructions += PROGRESS_INTERVAL
        if self.vm_instructions >= MAX_VM_INSTRUCTIONS:
            self.vm_stop_reason = "instruction_limit_exceeded"
            return 1
        if time.monotonic() >= self.deadline:
            self.vm_stop_reason = "deadline_exceeded"
            return 1
        return 0

    def open(self) -> sqlite3.Connection:
        try:
            uri = self.database.resolve(strict=True).as_uri() + "?mode=ro"
        except OSError as exc:
            raise ReportProblem("unavailable", "database_unavailable") from exc
        try:
            conn = sqlite3.connect(uri, uri=True, timeout=0.25, isolation_level=None)
        except sqlite3.Error as exc:
            raise ReportProblem("unavailable", "database_unavailable") from exc
        set_limit = getattr(conn, "setlimit", None)
        length_limit = getattr(sqlite3, "SQLITE_LIMIT_LENGTH", None)
        if set_limit is None or length_limit is None:
            conn.close()
            raise ReportProblem("unavailable", "bounded_text_limit_unavailable")
        try:
            # Bound SQLite values/rows before Python materializes result rows.
            set_limit(length_limit, MAX_SQLITE_ROW_BYTES)
        except sqlite3.Error as exc:
            conn.close()
            raise ReportProblem(
                "unavailable", "bounded_text_limit_unavailable"
            ) from exc
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA query_only=ON")
        conn.set_authorizer(_sqlite_authorizer)
        conn.set_progress_handler(self.progress, PROGRESS_INTERVAL)
        conn.execute("BEGIN")
        self.conn = conn
        return conn

    def _execute(
        self, name: str, sql: str, params: Sequence[Any] = ()
    ) -> sqlite3.Cursor:
        self.check_deadline()
        assert self.conn is not None
        if self.diagnostics_enabled:
            plan_rows = self.conn.execute(
                "EXPLAIN QUERY PLAN " + sql, params
            ).fetchall()
            self.query_plans.append(
                {"query": name, "details": [str(row["detail"]) for row in plan_rows]}
            )
        return self.conn.execute(sql, params)

    def _account_output_text(self, row: sqlite3.Row) -> None:
        for value in row:
            if not isinstance(value, str):
                continue
            if len(value) > MAX_TEXT_FIELD_CHARS:
                raise WorkLimitReached("output_size_limit_exceeded")
            self.output_text_chars += len(value)
            if self.output_text_chars > MAX_OUTPUT_TEXT_CHARS:
                raise WorkLimitReached("output_size_limit_exceeded")

    def _fetch_threads(self, thread_ids: Sequence[str]) -> dict[str, dict[str, Any]]:
        result: dict[str, dict[str, Any]] = {}
        ids = sorted(set(thread_ids))
        for batch in _sql_chunks(ids):
            self.check_deadline()
            placeholders = ",".join("?" for _ in batch)
            sql = f"SELECT {THREAD_COLUMNS} FROM usage_threads WHERE thread_id IN ({placeholders})"
            for row in self._execute("thread_metadata", sql, batch):
                self._account_output_text(row)
                result[row["thread_id"]] = dict(row)
        return result

    def _fetch_anchor(self, thread_id: str) -> dict[str, Any] | None:
        sql = f"SELECT {THREAD_COLUMNS} FROM usage_threads WHERE thread_id = ?"
        row = self._execute("thread_anchor", sql, (thread_id,)).fetchone()
        if row is not None:
            self._account_output_text(row)
        return dict(row) if row is not None else None

    def _fetch_children(
        self, parent_ids: Sequence[str], remaining: int
    ) -> list[dict[str, Any]]:
        children: list[dict[str, Any]] = []
        for batch in _sql_chunks(sorted(set(parent_ids))):
            if not batch:
                continue
            self.check_deadline()
            batch_count = 0
            placeholders = ",".join("?" for _ in batch)
            sql = (
                f"SELECT {THREAD_COLUMNS} FROM usage_threads INDEXED BY {PARENT_INDEX} "
                f"WHERE {PARENT_EXPR} IN ({placeholders}) LIMIT ?"
            )
            cursor = self._execute("lineage_children", sql, (*batch, remaining + 1))
            for row in cursor:
                self.check_deadline()
                if batch_count >= remaining:
                    raise WorkLimitReached("lineage_node_limit_exceeded")
                self._account_output_text(row)
                batch_count += 1
                children.append(dict(row))
            remaining -= batch_count
        return children

    def _walk_subtree(
        self, anchor: dict[str, Any]
    ) -> tuple[dict[str, dict[str, Any]], bool]:
        nodes = {anchor["thread_id"]: anchor}
        frontier = [anchor["thread_id"]]
        cycle_seen = False
        depth = 0
        while frontier:
            self.check_deadline()
            children = self._fetch_children(frontier, MAX_LINEAGE_NODES - len(nodes))
            next_frontier: list[str] = []
            for child in children:
                child_id = child["thread_id"]
                if child_id in nodes:
                    cycle_seen = True
                    continue
                if len(nodes) >= MAX_LINEAGE_NODES:
                    raise WorkLimitReached("lineage_node_limit_exceeded")
                nodes[child_id] = child
                next_frontier.append(child_id)
            if not next_frontier:
                break
            depth += 1
            if depth > MAX_LINEAGE_DEPTH:
                raise WorkLimitReached("lineage_depth_limit_exceeded")
            frontier = next_frontier
        return nodes, cycle_seen

    def _fetch_calls(
        self,
        scope_thread_ids: Sequence[str] | None,
        seek_start: str,
        seek_end: str,
    ) -> list[dict[str, Any]]:
        calls: list[dict[str, Any]] = []
        if scope_thread_ids is None:
            sql = (
                f"SELECT {CALL_COLUMNS} FROM usage_provider_calls INDEXED BY {STARTED_INDEX} "
                "WHERE started_at >= ? AND started_at < ? LIMIT ?"
            )
            cursor = self._execute(
                "window_calls", sql, (seek_start, seek_end, MAX_CALLS + 1)
            )
            for row in cursor:
                self._account_output_text(row)
                if len(calls) >= MAX_CALLS:
                    raise WorkLimitReached("call_limit_exceeded")
                calls.append(dict(row))
            self.selected_rows = len(calls)
            return calls

        ids = sorted(set(scope_thread_ids))
        for batch in _sql_chunks(ids):
            if not batch:
                continue
            remaining = MAX_CALLS - len(calls)
            placeholders = ",".join("?" for _ in batch)
            sql = (
                f"SELECT {CALL_COLUMNS} FROM usage_provider_calls INDEXED BY {THREAD_STARTED_INDEX} "
                f"WHERE thread_id IN ({placeholders}) AND started_at >= ? AND started_at < ? LIMIT ?"
            )
            params = (*batch, seek_start, seek_end, remaining + 1)
            for row in self._execute("thread_window_calls", sql, params):
                self._account_output_text(row)
                if len(calls) >= MAX_CALLS:
                    raise WorkLimitReached("call_limit_exceeded")
                calls.append(dict(row))
        self.selected_rows = len(calls)
        return calls

    def _fetch_ancestors(
        self,
        initial_ids: Sequence[str],
        existing: dict[str, dict[str, Any]],
    ) -> tuple[dict[str, dict[str, Any]], set[str]]:
        metadata = dict(existing)
        missing: set[str] = set()
        frontier_depths = {
            thread_id: 0 for thread_id in set(initial_ids) - set(metadata)
        }
        for row in metadata.values():
            parent, _parent_source = self._effective_parent(row)
            if parent is not None and parent not in metadata and parent not in missing:
                previous_depth = frontier_depths.get(parent)
                if previous_depth is None or previous_depth > 1:
                    frontier_depths[parent] = 1

        while frontier_depths:
            self.check_deadline()
            depth = min(frontier_depths.values())
            if depth > MAX_LINEAGE_DEPTH:
                raise WorkLimitReached("lineage_depth_limit_exceeded")
            if len(metadata) + len(missing) + len(frontier_depths) > MAX_LINEAGE_NODES:
                raise WorkLimitReached("lineage_node_limit_exceeded")
            frontier = sorted(
                thread_id
                for thread_id, candidate_depth in frontier_depths.items()
                if candidate_depth == depth
            )
            for thread_id in frontier:
                del frontier_depths[thread_id]
            fetched = self._fetch_threads(frontier)
            missing.update(set(frontier) - set(fetched))
            metadata.update(fetched)
            for row in fetched.values():
                parent, _parent_source = self._effective_parent(row)
                if (
                    parent is not None
                    and parent not in metadata
                    and parent not in missing
                ):
                    next_depth = depth + 1
                    previous_depth = frontier_depths.get(parent)
                    if previous_depth is None or next_depth < previous_depth:
                        frontier_depths[parent] = next_depth
            if len(metadata) + len(missing) + len(frontier_depths) > MAX_LINEAGE_NODES:
                raise WorkLimitReached("lineage_node_limit_exceeded")
        self.lineage_nodes = len(metadata)
        return metadata, missing

    @staticmethod
    def _effective_parent(row: dict[str, Any]) -> tuple[str | None, str]:
        parent = _blank_to_none(row.get("parent_thread_id"))
        if parent is not None:
            return parent, "parent"
        fork_parent = _blank_to_none(row.get("fork_parent_thread_id"))
        if fork_parent is not None:
            return fork_parent, "fork_parent"
        return None, "none"

    def _resolve_lineage(
        self, thread_id: str, metadata: dict[str, dict[str, Any]], missing: set[str]
    ) -> dict[str, Any]:
        current: str | None = thread_id
        visited: set[str] = set()
        recorded_roots: set[str] = set()
        source = "unknown"
        depth = 0
        while current is not None:
            self.check_deadline()
            if current in visited:
                return {
                    "resolved_root_thread_id": None,
                    "lineage_status": "cycle",
                    "lineage_source": source,
                }
            visited.add(current)
            row = metadata.get(current)
            if row is None:
                status = (
                    "missing_thread_metadata"
                    if current == thread_id
                    else "missing_ancestor"
                )
                return {
                    "resolved_root_thread_id": None,
                    "lineage_status": status,
                    "lineage_source": source,
                }
            recorded = _blank_to_none(row.get("root_thread_id"))
            if recorded is not None:
                recorded_roots.add(recorded)
            parent, parent_source = self._effective_parent(row)
            if current == thread_id:
                source = parent_source
            if len(recorded_roots) > 1:
                return {
                    "resolved_root_thread_id": None,
                    "lineage_status": "conflicting_recorded_roots",
                    "lineage_source": source,
                }
            if parent is None:
                if recorded == current and (
                    not recorded_roots or recorded_roots == {current}
                ):
                    return {
                        "resolved_root_thread_id": current,
                        "lineage_status": "resolved_self_root",
                        "lineage_source": "recorded_self_root",
                    }
                if recorded is not None:
                    return {
                        "resolved_root_thread_id": None,
                        "lineage_status": "recorded_root_unverified",
                        "lineage_source": source,
                    }
                return {
                    "resolved_root_thread_id": None,
                    "lineage_status": "root_unrecorded",
                    "lineage_source": source,
                }
            current = parent
            depth += 1
            if depth > MAX_LINEAGE_DEPTH:
                return {
                    "resolved_root_thread_id": None,
                    "lineage_status": "depth_limit_exceeded",
                    "lineage_source": source,
                }
        return {
            "resolved_root_thread_id": None,
            "lineage_status": "root_unrecorded",
            "lineage_source": source,
        }

    def _load_policies(
        self, calls: Sequence[dict[str, Any]]
    ) -> dict[tuple[str, str, str], list[dict[str, Any]]]:
        keys: set[tuple[str, str, str]] = set()
        for call in calls:
            provider = _key(call.get("provider"))
            billing_surface = _key(call.get("billing_surface"))
            account_plan = _key(call.get("account_plan"))
            if provider and billing_surface and account_plan:
                _add_bounded_key(
                    keys,
                    (provider, billing_surface, account_plan),
                    MAX_RATE_POLICY_ROWS,
                    "rate_policy_key_limit_exceeded",
                )
        result: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
        total = 0
        for key in sorted(keys):
            self.check_deadline()
            sql = (
                f"SELECT {POLICY_COLUMNS} FROM usage_codex_credit_policies "
                "INDEXED BY usage_codex_credit_policies_lookup_idx "
                "WHERE provider = ? AND billing_surface = ? AND account_plan = ? LIMIT ?"
            )
            rows = []
            for row in self._execute(
                "credit_policies", sql, (*key, MAX_RATE_POLICY_ROWS - total + 1)
            ):
                self._account_output_text(row)
                if total >= MAX_RATE_POLICY_ROWS:
                    raise WorkLimitReached("rate_policy_row_limit_exceeded")
                total += 1
                rows.append(dict(row))
            result[key] = rows
        self.rate_policy_rows = total
        return result

    def _load_rates(
        self,
        calls: Sequence[dict[str, Any]],
        policies: dict[tuple[str, str, str], list[dict[str, Any]]],
    ) -> dict[tuple[str, str, str, str, str], list[dict[str, Any]]]:
        keys: set[tuple[str, str, str, str, str]] = set()
        for call in calls:
            if (
                self.credit_mode in {"observed_or_effective_rate", "both"}
                and call.get("provider_reported_credits") is None
            ):
                provider = _key(call.get("provider"))
                model = _key(call.get("actual_model_used"))
                tier = _key(call.get("actual_service_tier"))
                speed = (
                    "fast"
                    if call.get("fast_mode_used") == 1
                    else "standard"
                    if call.get("fast_mode_used") == 0
                    else None
                )
                policy_key = (
                    _key(call.get("provider")),
                    _key(call.get("billing_surface")),
                    _key(call.get("account_plan")),
                )
                for policy in policies.get(policy_key, []):
                    if provider and model and tier and speed:
                        _add_bounded_key(
                            keys,
                            (
                                provider,
                                model,
                                tier,
                                speed,
                                _key(policy.get("rate_card_kind")) or "",
                            ),
                            MAX_RATE_POLICY_ROWS,
                            "rate_policy_key_limit_exceeded",
                        )
            if self.credit_mode in {"supplied_standard_scenario", "both"}:
                model = _key(call.get("actual_model_used"))
                if model:
                    _add_bounded_key(
                        keys,
                        ("openai", model, "default", "standard", "codex_token_based"),
                        MAX_RATE_POLICY_ROWS,
                        "rate_policy_key_limit_exceeded",
                    )
        result: dict[tuple[str, str, str, str, str], list[dict[str, Any]]] = {}
        total = self.rate_policy_rows
        for key in sorted(keys):
            self.check_deadline()
            sql = (
                f"SELECT {RATE_COLUMNS} FROM usage_codex_credit_rates "
                "INDEXED BY usage_codex_credit_rates_lookup_idx "
                "WHERE provider = ? AND model = ? AND service_tier = ? "
                "AND speed_mode = ? AND rate_card_kind = ? LIMIT ?"
            )
            rows = []
            for row in self._execute(
                "credit_rates", sql, (*key, MAX_RATE_POLICY_ROWS - total + 1)
            ):
                self._account_output_text(row)
                if total >= MAX_RATE_POLICY_ROWS:
                    raise WorkLimitReached("rate_policy_row_limit_exceeded")
                total += 1
                rows.append(dict(row))
            result[key] = rows
        self.rate_policy_rows = total
        return result

    @staticmethod
    def _effective_rows(
        rows: Sequence[dict[str, Any]], at: dt.datetime
    ) -> tuple[list[dict[str, Any]], bool]:
        matches: list[dict[str, Any]] = []
        invalid_interval = False
        for row in rows:
            try:
                start = _supported_utc_text(str(row["effective_from"]))
                end_text = row.get("effective_to")
                end = (
                    _supported_utc_text(str(end_text)) if end_text is not None else None
                )
            except (ValueError, TypeError):
                invalid_interval = True
                continue
            if end is not None and end <= start:
                invalid_interval = True
                continue
            if start <= at and (end is None or at < end):
                matches.append(row)
        return matches, invalid_interval

    def _actual_price(
        self,
        call: dict[str, Any],
        policies: dict[tuple[str, str, str], list[dict[str, Any]]],
        rates: dict[tuple[str, str, str, str, str], list[dict[str, Any]]],
        at: dt.datetime,
    ) -> dict[str, Any]:
        reported = call.get("provider_reported_credits")
        if reported is not None:
            amount = _nonnegative_decimal(reported)
            if amount is None:
                return {"covered": False, "reason": "provider_reported_credit_invalid"}
            return {"covered": True, "kind": "provider_reported", "credits": amount}
        if call.get("status") != "ok":
            return {"covered": False, "reason": "provider_usage_missing"}
        dimensions = [
            call.get("input_tokens_uncached"),
            call.get("input_tokens_cached"),
            call.get("input_tokens_cache_write"),
            call.get("output_tokens"),
            call.get("total_tokens"),
        ]
        if any(value is None for value in dimensions):
            return {"covered": False, "reason": "token_breakdown_incomplete"}
        if int(call["input_tokens_cache_write"]) != 0:
            return {"covered": False, "reason": "cache_write_unsupported"}
        provider = _key(call.get("provider"))
        billing = _key(call.get("billing_surface"))
        plan = _key(call.get("account_plan"))
        model = _key(call.get("actual_model_used"))
        tier = _key(call.get("actual_service_tier"))
        tier_source = _blank_to_none(call.get("actual_service_tier_source"))
        fast = call.get("fast_mode_used")
        if not provider or not billing or not plan:
            return {"covered": False, "reason": "credit_policy_dimensions_missing"}
        policy_matches, invalid_policy_interval = self._effective_rows(
            policies.get((provider, billing, plan), []), at
        )
        if invalid_policy_interval:
            return {"covered": False, "reason": "credit_policy_interval_invalid"}
        if len(policy_matches) > 1:
            return {"covered": False, "reason": "ambiguous_credit_policy"}
        if not policy_matches:
            return {
                "covered": False,
                "reason": "credit_policy_missing_or_not_effective",
            }
        if not model:
            return {"covered": False, "reason": "actual_model_missing"}
        if not tier or not tier_source:
            return {"covered": False, "reason": "actual_service_tier_missing"}
        if fast not in (0, 1):
            return {"covered": False, "reason": "actual_speed_mode_missing"}
        speed = "fast" if fast == 1 else "standard"
        policy = policy_matches[0]
        key = (provider, model, tier, speed, _key(policy.get("rate_card_kind")) or "")
        matches, invalid_rate_interval = self._effective_rows(rates.get(key, []), at)
        if invalid_rate_interval:
            return {"covered": False, "reason": "effective_rate_interval_invalid"}
        if len(matches) > 1:
            return {"covered": False, "reason": "ambiguous_effective_rate"}
        if not matches:
            return {"covered": False, "reason": "effective_rate_missing"}
        rate = matches[0]
        rate_amounts = [
            _nonnegative_decimal(rate.get("credits_per_1m_uncached_input")),
            _nonnegative_decimal(rate.get("credits_per_1m_cached_input")),
            _nonnegative_decimal(rate.get("credits_per_1m_output")),
        ]
        if any(amount is None for amount in rate_amounts):
            return {"covered": False, "reason": "effective_rate_invalid"}
        amount = (
            _decimal(call["input_tokens_uncached"]) * rate_amounts[0]
            + _decimal(call["input_tokens_cached"]) * rate_amounts[1]
            + _decimal(call["output_tokens"]) * rate_amounts[2]
        ) / decimal.Decimal(1_000_000)
        return {
            "covered": True,
            "kind": "rate_estimate",
            "credits": DECIMAL_CONTEXT.create_decimal(amount),
            "rate": rate,
            "policy": policy,
        }

    def _standard_price(
        self,
        call: dict[str, Any],
        rates: dict[tuple[str, str, str, str, str], list[dict[str, Any]]],
        at: dt.datetime,
    ) -> dict[str, Any]:
        if call.get("status") != "ok":
            return {"covered": False, "reason": "provider_usage_missing"}
        dimensions = [
            call.get("input_tokens_uncached"),
            call.get("input_tokens_cached"),
            call.get("input_tokens_cache_write"),
            call.get("output_tokens"),
            call.get("total_tokens"),
        ]
        if any(value is None for value in dimensions):
            return {"covered": False, "reason": "token_breakdown_incomplete"}
        if int(call["input_tokens_cache_write"]) != 0:
            return {"covered": False, "reason": "cache_write_unsupported"}
        if _key(call.get("provider")) != "openai":
            return {
                "covered": False,
                "reason": "standard_scenario_provider_unsupported",
            }
        model = _key(call.get("actual_model_used"))
        if not model:
            return {"covered": False, "reason": "actual_model_missing"}
        key = ("openai", model, "default", "standard", "codex_token_based")
        rows = rates.get(key, [])
        matches, invalid_rate_interval = self._effective_rows(rows, at)
        if invalid_rate_interval:
            return {"covered": False, "reason": "standard_rate_interval_invalid"}
        if len(matches) > 1:
            return {"covered": False, "reason": "ambiguous_standard_rate"}
        if not matches:
            reason = (
                "standard_rate_not_effective" if rows else "standard_model_rate_missing"
            )
            return {"covered": False, "reason": reason}
        rate = matches[0]
        rate_amounts = [
            _nonnegative_decimal(rate.get("credits_per_1m_uncached_input")),
            _nonnegative_decimal(rate.get("credits_per_1m_cached_input")),
            _nonnegative_decimal(rate.get("credits_per_1m_output")),
        ]
        if any(amount is None for amount in rate_amounts):
            return {"covered": False, "reason": "standard_rate_invalid"}
        amount = (
            _decimal(call["input_tokens_uncached"]) * rate_amounts[0]
            + _decimal(call["input_tokens_cached"]) * rate_amounts[1]
            + _decimal(call["output_tokens"]) * rate_amounts[2]
        ) / decimal.Decimal(1_000_000)
        return {
            "covered": True,
            "credits": DECIMAL_CONTEXT.create_decimal(amount),
            "rate": rate,
        }

    def _serialize_metric(self, metrics: dict[str, Any]) -> dict[str, Any]:
        result = _json_metrics(metrics)
        if self.credit_mode == "observed_or_effective_rate":
            result.pop("standard_scenario")
        elif self.credit_mode == "supplied_standard_scenario":
            result.pop("actual_mode")
            result.pop("provider_reported_credits")
        return result

    def run(self) -> dict[str, Any]:
        started = time.monotonic()
        self.conn = self.open()
        try:
            watermark_row = self._execute(
                "call_watermark",
                "SELECT rowid AS last_call_rowid FROM usage_provider_calls ORDER BY rowid DESC LIMIT 1",
            ).fetchone()
            watermark = int(watermark_row["last_call_rowid"]) if watermark_row else None
            scope_nodes: dict[str, dict[str, Any]] = {}
            cycle_seen = False
            if self.scope == "subtree":
                anchor = self._fetch_anchor(self.thread_id or "")
                if anchor is not None:
                    scope_nodes, cycle_seen = self._walk_subtree(anchor)
            elif self.scope == "thread":
                anchor = self._fetch_anchor(self.thread_id or "")
                if anchor is not None:
                    scope_nodes = {anchor["thread_id"]: anchor}

            if self.scope == "all":
                scope_ids: Sequence[str] | None = None
            elif scope_nodes:
                scope_ids = sorted(scope_nodes)
            else:
                scope_ids = [self.thread_id or ""]
            seek_start, seek_end = _seek_bounds(self.start, self.end)
            calls = self._fetch_calls(scope_ids, seek_start, seek_end)
            filtered_calls: list[dict[str, Any]] = []
            unsupported_timestamps = 0
            for call in calls:
                self.check_deadline()
                for column in TOKEN_FIELDS.values():
                    call[column] = _nonnegative_integer(call.get(column))
                fast_mode = _nonnegative_integer(call.get("fast_mode_used"))
                call["fast_mode_used"] = fast_mode if fast_mode in (0, 1) else None
                try:
                    at = _supported_utc_text(str(call["started_at"]))
                except (ValueError, TypeError):
                    unsupported_timestamps += 1
                    continue
                if self.start <= at < self.end:
                    call["_started_at_parsed"] = at
                    filtered_calls.append(call)
            if unsupported_timestamps:
                raise WorkLimitReached("unsupported_timestamp_encoding")
            calls = filtered_calls
            if self.scope in {"thread", "subtree"} and not scope_nodes and not calls:
                raise ReportProblem("unavailable", "unknown_thread_id")
            if len(scope_nodes) > MAX_OUTPUT_GROUPS:
                raise WorkLimitReached("output_group_limit_exceeded")
            participating = set(scope_nodes) if self.scope == "subtree" else set()
            if (
                self.scope == "thread"
                and self.thread_id
                and self.thread_id not in participating
            ):
                if len(participating) >= MAX_OUTPUT_GROUPS:
                    raise WorkLimitReached("output_group_limit_exceeded")
                participating.add(self.thread_id)
            call_thread_ids: set[str] = set()
            for call in calls:
                thread_id = str(call["thread_id"])
                if thread_id in call_thread_ids:
                    continue
                if thread_id not in participating:
                    if len(participating) >= MAX_OUTPUT_GROUPS:
                        raise WorkLimitReached("output_group_limit_exceeded")
                    participating.add(thread_id)
                call_thread_ids.add(thread_id)

            initial_metadata = dict(scope_nodes)
            missing_anchor = self.scope in {"thread", "subtree"} and not scope_nodes
            metadata, missing_metadata = self._fetch_ancestors(
                sorted(call_thread_ids | set(scope_nodes)), initial_metadata
            )
            if len(metadata) + len(missing_metadata) > MAX_LINEAGE_NODES:
                raise WorkLimitReached("lineage_node_limit_exceeded")

            policies: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
            rates: dict[tuple[str, str, str, str, str], list[dict[str, Any]]] = {}
            if self.credit_mode in {"observed_or_effective_rate", "both"}:
                policies = self._load_policies(calls)
            rates = self._load_rates(calls, policies)

            thread_metrics: dict[str, dict[str, Any]] = {}
            lineage: dict[str, dict[str, Any]] = {}
            for thread_id in sorted(participating):
                thread_metrics[thread_id] = _new_metrics()
                lineage[thread_id] = self._resolve_lineage(
                    thread_id, metadata, missing_metadata
                )

            all_calls_metrics = _new_metrics()
            rate_evidence: dict[str, dict[str, Any]] = {}
            policy_evidence: dict[str, dict[str, Any]] = {}
            call_parent: dict[str, str | None] = {}
            for thread_id, row in metadata.items():
                call_parent[thread_id] = self._effective_parent(row)[0]
            for call in calls:
                self.check_deadline()
                at = call["_started_at_parsed"]
                actual = (
                    self._actual_price(call, policies, rates, at)
                    if self.credit_mode in {"observed_or_effective_rate", "both"}
                    else {"covered": False, "reason": "not_requested"}
                )
                standard = (
                    self._standard_price(call, rates, at)
                    if self.credit_mode in {"supplied_standard_scenario", "both"}
                    else {"covered": False, "reason": "not_requested"}
                )
                _add_call(all_calls_metrics, call, actual, standard)
                for mode_name, priced in (
                    ("actual_mode", actual),
                    ("supplied_standard_scenario", standard),
                ):
                    rate = priced.get("rate")
                    if (
                        priced.get("covered")
                        and rate is not None
                        and rate.get("rate_id") is not None
                    ):
                        rate_id = str(rate["rate_id"])
                        evidence = rate_evidence.setdefault(
                            rate_id,
                            {**_rate_evidence(rate), "used_by_modes": defaultdict(int)},
                        )
                        evidence["used_by_modes"][mode_name] += 1
                    policy = priced.get("policy")
                    if (
                        priced.get("covered")
                        and policy is not None
                        and policy.get("policy_id") is not None
                    ):
                        policy_id = str(policy["policy_id"])
                        evidence = policy_evidence.setdefault(
                            policy_id,
                            {**_policy_evidence(policy), "used_call_count": 0},
                        )
                        evidence["used_call_count"] += 1
                thread_id = str(call["thread_id"])
                if thread_id not in thread_metrics:
                    if (
                        thread_id not in participating
                        and len(participating) >= MAX_OUTPUT_GROUPS
                    ):
                        raise WorkLimitReached("output_group_limit_exceeded")
                    thread_metrics[thread_id] = _new_metrics()
                    lineage[thread_id] = self._resolve_lineage(
                        thread_id, metadata, missing_metadata
                    )
                    participating.add(thread_id)
                _add_call(thread_metrics[thread_id], call, actual, standard)

            if not calls and self.scope == "all":
                all_calls_metrics = _new_metrics()
            if self.scope in {"thread", "subtree"} and not scope_nodes and calls:
                lineage[self.thread_id or ""] = {
                    "resolved_root_thread_id": None,
                    "lineage_status": "missing_thread_metadata",
                    "lineage_source": "unknown",
                }

            thread_rows: list[dict[str, Any]] = []
            for thread_id in sorted(participating):
                row = metadata.get(thread_id)
                thread_lineage = lineage.get(thread_id) or self._resolve_lineage(
                    thread_id, metadata, missing_metadata
                )
                metric = thread_metrics.get(thread_id, _new_metrics())
                parent, parent_source = (
                    self._effective_parent(row) if row else (None, "unknown")
                )
                thread_rows.append(
                    {
                        "thread_id": thread_id,
                        "parent_thread_id": _blank_to_none(row.get("parent_thread_id"))
                        if row
                        else None,
                        "fork_parent_thread_id": _blank_to_none(
                            row.get("fork_parent_thread_id")
                        )
                        if row
                        else None,
                        "effective_parent_thread_id": parent,
                        "lineage_source": thread_lineage["lineage_source"]
                        if row
                        else "unknown",
                        "lineage_status": thread_lineage["lineage_status"],
                        "recorded_root_thread_id": _blank_to_none(
                            row.get("root_thread_id")
                        )
                        if row
                        else None,
                        "resolved_root_thread_id": thread_lineage[
                            "resolved_root_thread_id"
                        ],
                        "agent_nickname": _blank_to_none(row.get("agent_nickname"))
                        if row
                        else None,
                        "agent_role": _blank_to_none(row.get("agent_role"))
                        if row
                        else None,
                        "source": _blank_to_none(row.get("source")) if row else None,
                        "created_at": row.get("created_at") if row else None,
                        "metrics": self._serialize_metric(metric),
                    }
                )

            root_buckets: dict[str, list[dict[str, Any]]] = defaultdict(list)
            for thread_id in sorted(participating):
                root_id = lineage.get(thread_id, {}).get("resolved_root_thread_id")
                if root_id is None:
                    root_buckets["unresolved"].append(
                        thread_metrics.get(thread_id, _new_metrics())
                    )
                else:
                    root_buckets[root_id].append(
                        thread_metrics.get(thread_id, _new_metrics())
                    )
            disjoint_roots = {
                root: self._serialize_metric(_merge_metrics(items))
                for root, items in sorted(root_buckets.items())
            }
            if self.scope in {"thread", "subtree"} and self.thread_id:
                anchor_id = self.thread_id
                direct_ids = [
                    thread_id
                    for thread_id in participating
                    if thread_id != anchor_id
                    and call_parent.get(thread_id) == anchor_id
                ]
                descendant_ids = [
                    thread_id for thread_id in participating if thread_id != anchor_id
                ]
                own = thread_metrics.get(anchor_id, _new_metrics())
                direct = _merge_metrics(
                    thread_metrics.get(thread_id, _new_metrics())
                    for thread_id in direct_ids
                )
                descendants = _merge_metrics(
                    thread_metrics.get(thread_id, _new_metrics())
                    for thread_id in descendant_ids
                )
                tree = _merge_metrics(thread_metrics.values())
                scope_contributions = {
                    "anchor_thread_id": anchor_id,
                    "own": self._serialize_metric(own),
                    "direct_children": self._serialize_metric(direct),
                    "descendants_excluding_anchor": self._serialize_metric(descendants),
                    "tree_including_anchor": self._serialize_metric(tree),
                    "disjoint_call_counts_reconcile": tree["provider_call_count"]
                    == all_calls_metrics["provider_call_count"],
                }
            else:
                scope_contributions = None

            unresolved_lineage = sum(
                1
                for thread_id in participating
                if lineage.get(thread_id, {}).get("resolved_root_thread_id") is None
            )
            report_status = "complete"
            reasons: list[str] = []
            if unresolved_lineage:
                report_status = "incomplete"
                reasons.append("lineage_incomplete")
            if missing_anchor:
                report_status = "incomplete"
                reasons.append("thread_metadata_missing")
            if cycle_seen:
                report_status = "incomplete"
                reasons.append("lineage_cycle")
            if any(
                metrics["actual_mode"]["uncovered_call_count"] > 0
                for metrics in [all_calls_metrics]
            ) and self.credit_mode in {"observed_or_effective_rate", "both"}:
                report_status = "incomplete"
                reasons.append("actual_credit_coverage_incomplete")
            if all_calls_metrics["token_missing_calls"] and any(
                all_calls_metrics["token_missing_calls"].values()
            ):
                report_status = "incomplete"
                reasons.append("token_coverage_incomplete")
            if (
                self.credit_mode in {"supplied_standard_scenario", "both"}
                and all_calls_metrics["standard_scenario"]["uncovered_call_count"]
            ):
                report_status = "incomplete"
                reasons.append("standard_scenario_coverage_incomplete")
            window_local = {
                "start": self.start.astimezone(self.timezone).isoformat(),
                "end_exclusive": self.end.astimezone(self.timezone).isoformat(),
            }
            report = {
                "schema_version": REPORT_VERSION,
                "status": report_status,
                "incomplete_reasons": sorted(set(reasons)),
                "scope": {"kind": self.scope, "thread_id": self.thread_id},
                "window": {
                    "start_utc_inclusive": _iso_utc(self.start),
                    "end_utc_exclusive": _iso_utc(self.end),
                    "presentation_timezone": self.timezone_name,
                    "presentation_bounds": window_local,
                },
                "snapshot": {
                    "captured_at_utc": _iso_utc(dt.datetime.now(UTC)),
                    "last_persisted_call_rowid": watermark,
                    "provider_freshness": "not_asserted",
                },
                "database_profile": "explicit_local_usage_ledger",
                "timestamp_encoding_coverage": "qualified UTC Z and +00:00 rows only; arbitrary imported encodings are not globally certified",
                "recorder_completeness": "unknown; report covers recorded ledger rows only",
                "summary": self._serialize_metric(all_calls_metrics),
                "threads": thread_rows,
                "resolved_root_buckets": disjoint_roots,
                "credit_evidence": {
                    "rates": [
                        {
                            **value,
                            "used_by_modes": dict(
                                sorted(value["used_by_modes"].items())
                            ),
                        }
                        for _key_id, value in sorted(rate_evidence.items())
                    ],
                    "policies": [
                        value for _key_id, value in sorted(policy_evidence.items())
                    ],
                },
                "scope_contributions": scope_contributions,
                "lineage_coverage": {
                    "participating_thread_count": len(participating),
                    "unresolved_thread_count": unresolved_lineage,
                    "known_zero_call_thread_count": sum(
                        1
                        for thread_id in participating
                        if thread_metrics.get(thread_id, _new_metrics())[
                            "provider_call_count"
                        ]
                        == 0
                    ),
                },
                "credit_mode": self.credit_mode,
            }
            self.check_deadline()
            if self.diagnostics_enabled:
                self._write_diagnostics(started)
            return report
        except WorkLimitReached:
            raise
        except sqlite3.OperationalError as exc:
            message = str(exc).lower()
            if getattr(exc, "sqlite_errorcode", None) == getattr(
                sqlite3, "SQLITE_TOOBIG", -1
            ):
                raise WorkLimitReached("output_size_limit_exceeded") from exc
            if self.vm_stop_reason:
                raise WorkLimitReached(self.vm_stop_reason) from exc
            if "no such index" in message or "no query solution" in message:
                raise ReportProblem(
                    "unavailable", "required_reporting_index_missing"
                ) from exc
            if "no such table" in message or "no such column" in message:
                raise ReportProblem("unavailable", "ledger_schema_mismatch") from exc
            if "not authorized" in message:
                raise ReportProblem(
                    "unavailable", "read_only_policy_denied_query"
                ) from exc
            if "locked" in message or "busy" in message:
                raise ReportProblem("unavailable", "ledger_busy") from exc
            raise ReportProblem("unavailable", "sqlite_query_failed") from exc
        except sqlite3.Error as exc:
            if getattr(exc, "sqlite_errorcode", None) == getattr(
                sqlite3, "SQLITE_TOOBIG", -1
            ):
                raise WorkLimitReached("output_size_limit_exceeded") from exc
            if self.vm_stop_reason:
                raise WorkLimitReached(self.vm_stop_reason) from exc
            raise ReportProblem("unavailable", "sqlite_read_failed") from exc
        finally:
            if self.conn is not None:
                try:
                    self.conn.rollback()
                except sqlite3.Error:
                    # Closing below discards this read-only transaction; avoid
                    # replacing the report's primary result with cleanup noise.
                    pass
                self.conn.close()
                self.conn = None

    def _write_diagnostics(self, started: float) -> None:
        max_rss_kib = None
        if resource is not None:
            max_rss = int(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)
            # Linux reports ru_maxrss in KiB; macOS reports bytes.
            if sys.platform == "darwin":
                max_rss_kib = (max_rss + 1023) // 1024
            else:
                max_rss_kib = max_rss
        payload = {
            "selected_call_rows": self.selected_rows,
            "lineage_nodes": self.lineage_nodes,
            "rate_policy_rows": self.rate_policy_rows,
            "sqlite_vm_instruction_estimate": self.vm_instructions,
            "elapsed_seconds": round(time.monotonic() - started, 6),
            "max_rss_kib": max_rss_kib,
            "query_plans": self.query_plans,
        }
        print(
            "CODEX_USAGE_REPORT_DIAGNOSTICS=" + json.dumps(payload, sort_keys=True),
            file=sys.stderr,
        )


def _args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--database", required=True, help="exact SQLite usage-ledger file path"
    )
    parser.add_argument(
        "--start-utc", required=True, help="inclusive ISO-8601 UTC timestamp"
    )
    parser.add_argument(
        "--end-utc", required=True, help="exclusive ISO-8601 UTC timestamp"
    )
    parser.add_argument("--timezone", required=True, help="IANA presentation timezone")
    parser.add_argument("--scope", required=True, choices=("all", "thread", "subtree"))
    parser.add_argument("--thread-id")
    parser.add_argument(
        "--credit-mode",
        required=True,
        choices=("observed_or_effective_rate", "supplied_standard_scenario", "both"),
    )
    parser.add_argument("--diagnostics", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.scope == "all" and args.thread_id:
        parser.error("--thread-id is only valid for thread or subtree scope")
    if args.scope != "all" and not args.thread_id:
        parser.error("--thread-id is required for thread or subtree scope")
    if args.thread_id is not None:
        args.thread_id = args.thread_id.strip()
        if not args.thread_id:
            parser.error("--thread-id must not be blank")
    try:
        args.start = parse_utc(args.start_utc)
        args.end = parse_utc(args.end_utc)
    except ValueError as exc:
        parser.error(str(exc))
    if args.start >= args.end:
        parser.error("start must be earlier than end")
    try:
        args.timezone_value = ZoneInfo(args.timezone)
    except ZoneInfoNotFoundError:
        if args.timezone == "UTC":
            args.timezone_value = UTC
        else:
            parser.error("--timezone must name an installed IANA timezone")
    return args


def _error_report(
    problem: ReportProblem, args: argparse.Namespace | None
) -> dict[str, Any]:
    scope = None
    if args is not None:
        thread_id_omitted = (
            args.thread_id is not None and len(args.thread_id) > MAX_TEXT_FIELD_CHARS
        )
        scope = {
            "kind": args.scope,
            "thread_id": None if thread_id_omitted else args.thread_id,
        }
        if thread_id_omitted:
            scope["thread_id_omitted"] = True
    return {
        "schema_version": REPORT_VERSION,
        "status": problem.status,
        "error": {"reason": problem.reason, "message": problem.message},
        "scope": scope,
        "window": (
            {
                "start_utc_inclusive": _iso_utc(args.start),
                "end_utc_exclusive": _iso_utc(args.end),
                "presentation_timezone": args.timezone,
            }
            if args is not None
            else None
        ),
    }


def _write_error_json(problem: ReportProblem, args: argparse.Namespace | None) -> None:
    try:
        _write_bounded_json(_error_report(problem, args), sys.stdout)
    except WorkLimitReached:
        fallback = WorkLimitReached("output_size_limit_exceeded")
        _write_bounded_json(_error_report(fallback, None), sys.stdout)


def _write_bounded_json(
    report: dict[str, Any], output: Any, check_deadline: Any | None = None
) -> None:
    encoder = json.JSONEncoder(
        sort_keys=True, separators=(",", ":"), default=str, ensure_ascii=True
    )
    encoded_chars = 0
    chunks: list[str] = []
    for chunk in encoder.iterencode(report):
        # ensure_ascii makes every encoded character one UTF-8 byte.
        encoded_chars += len(chunk)
        if encoded_chars > MAX_OUTPUT_BYTES:
            raise WorkLimitReached("output_size_limit_exceeded")
        if check_deadline is not None:
            check_deadline()
        chunks.append(chunk)
    if check_deadline is not None:
        check_deadline()
    for chunk in chunks:
        output.write(chunk)
    output.write("\n")


def main(argv: Sequence[str] | None = None) -> int:
    args: argparse.Namespace | None = None
    try:
        args = _args(argv)
        reporter = UsageReporter(
            database=Path(args.database),
            start=args.start,
            end=args.end,
            timezone=args.timezone_value,
            timezone_name=args.timezone,
            scope=args.scope,
            thread_id=args.thread_id,
            credit_mode=args.credit_mode,
            diagnostics=args.diagnostics,
        )
        with decimal.localcontext(DECIMAL_CONTEXT):
            report = reporter.run()
        reporter.check_deadline()
        status = report["status"]
        _write_bounded_json(report, sys.stdout, reporter.check_deadline)
        return 0 if status == "complete" else 2
    except WorkLimitReached as exc:
        _write_error_json(exc, args)
        return 2
    except ReportProblem as exc:
        _write_error_json(exc, args)
        return 3


if __name__ == "__main__":
    raise SystemExit(main())
