#!/usr/bin/env python3
"""Bounded, read-only reports over explicitly selected Codex usage ledgers."""

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
from typing import Any, Callable, Iterable, Iterator, Sequence
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

try:
    import resource
except ImportError:  # pragma: no cover - unavailable on Windows
    resource = None


REPORT_VERSION = "codex-local-usage-report/v1"
# Keep one dual-ledger read within a short wall-clock bound; row and VM caps
# independently bound the amount of SQLite work and retained report data.
MAX_SECONDS = 10.0
MAX_VM_INSTRUCTIONS = 8_000_000
MAX_CALLS = 25_000
MAX_LINEAGE_NODES = 4_096
MAX_LINEAGE_DEPTH = 128
MAX_RATE_POLICY_ROWS = 512
MAX_OUTPUT_GROUPS = 1_000
MAX_OUTPUT_BYTES = 16 * 1024 * 1024
MAX_TEXT_FIELD_CHARS = 8_192
MAX_SELECTED_TEXT_CHARS = 8_000_000
MAX_MULTI_LEDGER_ATTRIBUTION_TEXT_CHARS = 1_000_000
MAX_MULTI_LEDGER_INTERNAL_TEXT_CHARS = {
    "provider_call_id": 128,
    "thread_id": 128,
    "spawn_request_id": 128,
    "parent_thread_id": 128,
    "root_thread_id": 128,
    "fork_parent_thread_id": 128,
    "provider": 128,
    "started_at": 64,
    "completed_at": 64,
    "status": 32,
}
MULTI_LEDGER_ATTRIBUTION_TEXT_COLUMNS = frozenset(
    {
        "requested_model",
        "actual_model_used",
        "requested_reasoning_effort",
        "_configured_model",
        "_configured_effort",
    }
)
MAX_SQLITE_ROW_BYTES = 1024 * 1024
SQL_CHUNK = 250
PROGRESS_INTERVAL = 1_000

STARTED_INDEX = "usage_provider_calls_started_at_idx"
THREAD_STARTED_INDEX = "usage_provider_calls_thread_started_at_idx"

CALL_COLUMNS = (
    "provider_call_id, thread_id, spawn_request_id, provider, requested_model, actual_model_used, "
    "requested_service_tier, actual_service_tier, actual_service_tier_source, "
    "fast_mode_used, billing_surface, account_plan, started_at, completed_at, "
    "input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, "
    "output_tokens, total_tokens, provider_reported_credits, status"
)
TOKEN_CALL_COLUMNS = (
    "provider_call_id, thread_id, spawn_request_id, provider, requested_model, "
    "actual_model_used, started_at, completed_at, input_tokens_uncached, "
    "input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status"
)
THREAD_COLUMNS = (
    "thread_id, parent_thread_id, root_thread_id, fork_parent_thread_id, "
    "agent_nickname, agent_role, source, created_at"
)
LINEAGE_THREAD_COLUMNS = (
    "thread_id, parent_thread_id, root_thread_id, fork_parent_thread_id"
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
SPAWN_COLUMNS = (
    "spawn_request_id, requested_model, requested_reasoning_effort"
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


def _deadline_checker(deadline: float) -> Callable[[], None]:
    def check() -> None:
        if time.monotonic() >= deadline:
            raise WorkLimitReached("deadline_exceeded")

    return check


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


def _new_metrics(include_model_breakdown: bool = True) -> dict[str, Any]:
    metrics = {
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
    if include_model_breakdown:
        metrics["by_actual_model"] = {}
    return metrics


def _add_call_totals(
    metrics: dict[str, Any],
    call: dict[str, Any],
    actual: dict[str, Any],
    standard: dict[str, Any],
) -> None:
    metrics["provider_call_count"] += 1
    if call.get("status") != "ok":
        metrics["missing_usage_calls"] += 1
    observed_model = _blank_to_none(call.get("actual_model_used"))
    metrics["actual_model_evidence"][observed_model] += 1
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


def _add_call(
    metrics: dict[str, Any],
    call: dict[str, Any],
    actual: dict[str, Any],
    standard: dict[str, Any],
) -> None:
    _add_call_totals(metrics, call, actual, standard)
    by_model = metrics.get("by_actual_model")
    if by_model is None:
        return
    observed_model = _blank_to_none(call.get("actual_model_used"))
    model_key = observed_model
    if model_key not in by_model:
        if len(by_model) >= MAX_OUTPUT_GROUPS:
            raise WorkLimitReached("output_group_limit_exceeded")
        by_model[model_key] = _new_metrics(include_model_breakdown=False)
    _add_call_totals(by_model[model_key], call, actual, standard)


def _merge_metrics(
    items: Iterable[dict[str, Any]], include_model_breakdown: bool = True
) -> dict[str, Any]:
    items = list(items)
    merged = _new_metrics(include_model_breakdown=include_model_breakdown)
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
    if include_model_breakdown:
        model_keys = {
            model for item in items for model in item.get("by_actual_model", {})
        }
        if len(model_keys) > MAX_OUTPUT_GROUPS:
            raise WorkLimitReached("output_group_limit_exceeded")
        for model in sorted(model_keys, key=lambda value: (value is None, value or "")):
            merged["by_actual_model"][model] = _merge_metrics(
                (
                    item["by_actual_model"][model]
                    for item in items
                    if model in item.get("by_actual_model", {})
                ),
                include_model_breakdown=False,
            )
    return merged


def _json_metrics(metrics: dict[str, Any]) -> dict[str, Any]:
    result: dict[str, Any] = {
        "provider_call_count": metrics["provider_call_count"],
        "token_sums_known_subtotal": metrics["token_sums"],
        "token_missing_call_count": metrics["token_missing_calls"],
        "missing_usage_call_count": metrics["missing_usage_calls"],
        "actual_model_evidence": [
            {
                "actual_model_used": model,
                "call_count": count,
            }
            for model, count in sorted(
                metrics["actual_model_evidence"].items(),
                key=lambda item: (item[0] is None, item[0] or ""),
            )
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
    if "by_actual_model" in metrics:
        result["by_actual_model"] = [
            {
                "actual_model_used": model,
                **_json_metrics(model_metrics),
            }
            for model, model_metrics in sorted(
                metrics["by_actual_model"].items(),
                key=lambda item: (item[0] is None, item[0] or ""),
            )
        ]
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
        *,
        all_time: bool = False,
        token_only_projection: bool = False,
        deadline: float | None = None,
        max_calls: int = MAX_CALLS,
        max_selected_text_chars: int = MAX_SELECTED_TEXT_CHARS,
        max_vm_instructions: int = MAX_VM_INSTRUCTIONS,
        shared_text_pool: dict[str, str] | None = None,
        shared_attribution_text: set[str] | None = None,
        shared_text_counter: dict[str, int] | None = None,
        shared_vm_counter: dict[str, int] | None = None,
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
        self.all_time = all_time
        self.token_only_projection = token_only_projection
        self.call_columns = (
            TOKEN_CALL_COLUMNS if token_only_projection else CALL_COLUMNS
        )
        self.thread_columns = (
            LINEAGE_THREAD_COLUMNS if token_only_projection else THREAD_COLUMNS
        )
        self.deadline = deadline if deadline is not None else time.monotonic() + MAX_SECONDS
        self.max_calls = max_calls
        self.max_selected_text_chars = max_selected_text_chars
        self.max_vm_instructions = max_vm_instructions
        self.shared_text_pool = shared_text_pool
        self.shared_attribution_text = shared_attribution_text
        self.shared_text_counter = shared_text_counter
        self.shared_vm_counter = shared_vm_counter
        self.vm_instructions = 0
        self.vm_stop_reason: str | None = None
        self.query_plans: list[dict[str, Any]] = []
        self.selected_rows = 0
        self.rate_policy_rows = 0
        self.lineage_nodes = 0
        self.lineage_strategy = "exact_thread_metadata"
        self.lineage_scan_rows = 0
        self.lineage_scan_text_chars = 0
        self.lineage_snapshot_truncated = False
        self.lineage_walk_incomplete = False
        self.call_selection_truncated = False
        self.output_text_chars = 0
        self.selected_call_rows: list[dict[str, Any]] = []
        self.spawn_request_metadata: dict[str, dict[str, Any]] = {}
        self.conn: sqlite3.Connection | None = None
        self._verified_call_thread_index: str | None = None
        self._verified_call_id_index = False

    def check_deadline(self) -> None:
        if time.monotonic() >= self.deadline:
            self.vm_stop_reason = "deadline_exceeded"
            raise WorkLimitReached("deadline_exceeded")

    def progress(self) -> int:
        self.vm_instructions += PROGRESS_INTERVAL
        if self.shared_vm_counter is not None:
            self.shared_vm_counter["instructions"] += PROGRESS_INTERVAL
        instruction_count = (
            self.shared_vm_counter["instructions"]
            if self.shared_vm_counter is not None
            else self.vm_instructions
        )
        if instruction_count >= self.max_vm_instructions:
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

    def _account_output_text(self, row: sqlite3.Row) -> dict[str, Any]:
        values: dict[str, Any] = {}
        for column in row.keys():
            value = row[column]
            if not isinstance(value, str):
                values[column] = value
                continue
            pooled_multi_ledger = self.shared_text_pool is not None
            field_limit = (
                MAX_MULTI_LEDGER_INTERNAL_TEXT_CHARS.get(
                    column, MAX_TEXT_FIELD_CHARS
                )
                if pooled_multi_ledger
                else MAX_TEXT_FIELD_CHARS
            )
            if len(value) > field_limit:
                reason = (
                    "source_identity_field_limit_exceeded"
                    if pooled_multi_ledger
                    and column in MAX_MULTI_LEDGER_INTERNAL_TEXT_CHARS
                    else "source_text_field_limit_exceeded"
                )
                raise WorkLimitReached(reason)
            if self.shared_text_pool is None:
                self.output_text_chars += len(value)
                if self.output_text_chars > self.max_selected_text_chars:
                    raise WorkLimitReached("selected_source_text_limit_exceeded")
                values[column] = value
                continue
            shared_value = self.shared_text_pool.get(value)
            if shared_value is None:
                self.shared_text_pool[value] = value
                shared_value = value
            if column in MULTI_LEDGER_ATTRIBUTION_TEXT_COLUMNS:
                if self.shared_attribution_text is None:
                    raise ReportProblem(
                        "unavailable", "attribution_text_accounting_unavailable"
                    )
                if value not in self.shared_attribution_text:
                    self.output_text_chars += len(value)
                    if self.shared_text_counter is not None:
                        self.shared_text_counter["characters"] += len(value)
                        text_count = self.shared_text_counter["characters"]
                    else:
                        text_count = self.output_text_chars
                    if text_count > self.max_selected_text_chars:
                        raise WorkLimitReached(
                            "selected_attribution_text_limit_exceeded"
                        )
                    self.shared_attribution_text.add(value)
            values[column] = shared_value
        return values

    def _fetch_threads(self, thread_ids: Sequence[str]) -> dict[str, dict[str, Any]]:
        result: dict[str, dict[str, Any]] = {}
        ids = sorted(set(thread_ids))
        for batch in _sql_chunks(ids):
            self.check_deadline()
            placeholders = ",".join("?" for _ in batch)
            sql = f"SELECT {self.thread_columns} FROM usage_threads WHERE thread_id IN ({placeholders})"
            for row in self._execute("thread_metadata", sql, batch):
                values = self._account_output_text(row)
                result[values["thread_id"]] = values
        return result

    def _fetch_anchor(self, thread_id: str) -> dict[str, Any] | None:
        sql = f"SELECT {self.thread_columns} FROM usage_threads WHERE thread_id = ?"
        row = self._execute("thread_anchor", sql, (thread_id,)).fetchone()
        return self._account_output_text(row) if row is not None else None

    def _walk_subtree(
        self, anchor: dict[str, Any]
    ) -> tuple[dict[str, dict[str, Any]], bool]:
        """Walk only spawn-confirmed descendants from the exact requested root."""
        root_id = anchor["thread_id"]
        if any(
            anchor.get(column) is not None and not isinstance(anchor.get(column), str)
            for column in (
                "thread_id",
                "root_thread_id",
                "parent_thread_id",
                "fork_parent_thread_id",
            )
        ):
            self.lineage_walk_incomplete = True
        nodes = {root_id: anchor}
        seen_requests: set[str] = set()
        seen_children: dict[str, str] = {}
        parent_by_child: dict[str, str] = {}
        frontier = [root_id]
        depth = 0
        cycle_seen = False
        while frontier:
            self.check_deadline()
            unique_frontier = sorted(set(frontier))
            bounded_frontier = [
                thread_id
                for thread_id in unique_frontier
                if len(thread_id) <= MAX_MULTI_LEDGER_INTERNAL_TEXT_CHARS["thread_id"]
            ]
            if len(bounded_frontier) != len(unique_frontier):
                self.lineage_walk_incomplete = True
            batch_ids = bounded_frontier
            next_frontier: list[str] = []
            for batch in _sql_chunks(batch_ids):
                placeholders = ",".join("?" for _ in batch)
                sql = (
                    "SELECT spawn_request_id, parent_thread_id, child_thread_id, status "
                    "FROM usage_spawn_requests INDEXED BY usage_spawn_requests_parent_idx "
                    f"WHERE parent_thread_id IN ({placeholders}) LIMIT ?"
                )
                params = (*batch, MAX_LINEAGE_NODES - len(seen_requests) + 1)
                try:
                    cursor = self._execute("lineage_spawn_edges", sql, params)
                except sqlite3.OperationalError as exc:
                    diagnostic = str(exc).lower()
                    if any(
                        marker in diagnostic
                        for marker in (
                            "no such index",
                            "no such table",
                            "no such column",
                        )
                    ):
                        raise ReportProblem(
                            "incomplete", "required_spawn_request_index_missing"
                        ) from exc
                    raise
                for edge in cursor:
                    self.check_deadline()
                    raw_request_id = edge["spawn_request_id"]
                    raw_child_id = edge["child_thread_id"]
                    raw_parent_id = edge["parent_thread_id"]
                    if any(
                        value is not None and not isinstance(value, str)
                        for value in (raw_request_id, raw_parent_id, raw_child_id)
                    ):
                        # Do not stringify SQLite BLOB/numeric identifiers:
                        # their size and identity are not covered by text caps.
                        self.lineage_walk_incomplete = True
                        continue
                    if any(
                        isinstance(value, str)
                        and len(value) > MAX_MULTI_LEDGER_INTERNAL_TEXT_CHARS[column]
                        for column, value in (
                            ("spawn_request_id", raw_request_id),
                            ("child_thread_id", raw_child_id),
                            ("parent_thread_id", raw_parent_id),
                        )
                    ):
                        self.lineage_walk_incomplete = True
                        continue
                    request_id = _blank_to_none(raw_request_id)
                    child_id = _blank_to_none(raw_child_id)
                    parent_id = _blank_to_none(raw_parent_id)
                    if request_id is None or parent_id is None or request_id in seen_requests:
                        self.lineage_walk_incomplete = True
                        continue
                    if child_id is not None and edge["status"] != "Completed(None)":
                        # A child ID on a non-successful or unknown terminal
                        # status is not enough to establish reportable lineage.
                        self.lineage_walk_incomplete = True
                        continue
                    if len(seen_requests) >= MAX_LINEAGE_NODES:
                        raise WorkLimitReached("lineage_node_limit_exceeded")
                    seen_requests.add(request_id)
                    if child_id is None:
                        # Pending/failed requests are evidence that lineage may
                        # be incomplete; they never identify a reportable child.
                        self.lineage_walk_incomplete = True
                        continue
                    previous_parent = seen_children.get(child_id)
                    if previous_parent is not None and previous_parent != parent_id:
                        self.lineage_walk_incomplete = True
                        continue
                    seen_children[child_id] = parent_id
                    if child_id in nodes:
                        self.lineage_walk_incomplete = True
                        ancestor: str | None = parent_id
                        while ancestor is not None:
                            if ancestor == child_id:
                                cycle_seen = True
                                break
                            ancestor = parent_by_child.get(ancestor)
                        continue
                    if len(nodes) >= MAX_LINEAGE_NODES:
                        raise WorkLimitReached("lineage_node_limit_exceeded")
                    # Fetch each edge's child by exact primary-key ID, then
                    # require recorded parent/root/fork metadata to agree.
                    child = self._fetch_threads([child_id]).get(child_id)
                    if child is None:
                        self.lineage_walk_incomplete = True
                        continue
                    if any(
                        child.get(column) is not None
                        and not isinstance(child.get(column), str)
                        for column in (
                            "thread_id",
                            "root_thread_id",
                            "parent_thread_id",
                            "fork_parent_thread_id",
                        )
                    ):
                        # Metadata identifiers must remain exact text; in
                        # particular, never stringify a BLOB into a thread ID.
                        self.lineage_walk_incomplete = True
                        continue
                    metadata_parent, _source = self._effective_parent(child)
                    recorded_root = _blank_to_none(child.get("root_thread_id"))
                    recorded_parent = _blank_to_none(child.get("parent_thread_id"))
                    recorded_fork_parent = _blank_to_none(
                        child.get("fork_parent_thread_id")
                    )
                    if (
                        metadata_parent != parent_id
                        or (recorded_parent is not None and recorded_parent != parent_id)
                        or (
                            recorded_fork_parent is not None
                            and recorded_fork_parent != parent_id
                        )
                        or (recorded_root is not None and recorded_root != root_id)
                    ):
                        self.lineage_walk_incomplete = True
                        continue
                    nodes[child_id] = child
                    parent_by_child[child_id] = parent_id
                    next_frontier.append(child_id)
            if not next_frontier:
                break
            depth += 1
            if depth > MAX_LINEAGE_DEPTH:
                self.lineage_walk_incomplete = True
                break
            frontier = next_frontier
        self.lineage_strategy = "indexed_spawn_request_breadth_first"
        self.lineage_nodes = len(nodes)
        return nodes, cycle_seen

    def _fetch_calls(
        self,
        scope_thread_ids: Sequence[str] | None,
        seek_start: str | None,
        seek_end: str | None,
    ) -> list[dict[str, Any]]:
        calls: list[dict[str, Any]] = []
        if scope_thread_ids is None:
            if seek_start is None or seek_end is None:
                raise ReportProblem("unavailable", "all_time_requires_exact_thread_scope")
            sql = (
                f"SELECT {self.call_columns} FROM usage_provider_calls INDEXED BY {STARTED_INDEX} "
                "WHERE started_at >= ? AND started_at < ? LIMIT ?"
            )
            cursor = self._execute(
                "window_calls", sql, (seek_start, seek_end, self.max_calls + 1)
            )
            for row in cursor:
                if len(calls) >= self.max_calls:
                    self.call_selection_truncated = True
                    break
                values = self._account_output_text(row)
                calls.append(values)
                self.selected_rows = len(calls)
            self.selected_rows = len(calls)
            return calls

        ids = sorted(set(scope_thread_ids))
        for batch in _sql_chunks(ids):
            if not batch:
                continue
            remaining = self.max_calls - len(calls)
            placeholders = ",".join("?" for _ in batch)
            if seek_start is None or seek_end is None:
                sql = (
                    f"SELECT {self.call_columns} FROM usage_provider_calls INDEXED BY {THREAD_STARTED_INDEX} "
                    f"WHERE thread_id IN ({placeholders}) LIMIT ?"
                )
                params = (*batch, remaining + 1)
            else:
                sql = (
                    f"SELECT {self.call_columns} FROM usage_provider_calls INDEXED BY {THREAD_STARTED_INDEX} "
                    f"WHERE thread_id IN ({placeholders}) AND started_at >= ? AND started_at < ? LIMIT ?"
                )
                params = (*batch, seek_start, seek_end, remaining + 1)
            try:
                cursor = self._execute("thread_window_calls", sql, params)
            except sqlite3.OperationalError as exc:
                # The original thread-only index remains a safe access path for
                # exact thread IDs when the optional composite time index has not
                # been installed. Never broaden this fallback to an all-call scan.
                if "no such index" not in str(
                    exc
                ).lower() or THREAD_STARTED_INDEX not in str(exc):
                    raise
                if seek_start is None or seek_end is None:
                    fallback_sql = (
                        f"SELECT {self.call_columns} FROM usage_provider_calls INDEXED BY usage_provider_calls_thread_idx "
                        f"WHERE thread_id IN ({placeholders}) LIMIT ?"
                    )
                else:
                    fallback_sql = (
                        f"SELECT {self.call_columns} FROM usage_provider_calls INDEXED BY usage_provider_calls_thread_idx "
                        f"WHERE thread_id IN ({placeholders}) AND started_at >= ? AND started_at < ? LIMIT ?"
                    )
                cursor = self._execute(
                    "thread_window_calls_by_thread_index", fallback_sql, params
                )
            for row in cursor:
                if len(calls) >= self.max_calls:
                    self.call_selection_truncated = True
                    break
                values = self._account_output_text(row)
                calls.append(values)
                self.selected_rows = len(calls)
            if self.call_selection_truncated:
                break
        self.selected_rows = len(calls)
        return calls

    def _token_call_select(self, alias: str = "call") -> str:
        columns = ", ".join(
            f"{alias}.{column.strip()}" for column in TOKEN_CALL_COLUMNS.split(",")
        )
        return (
            f"{columns}, spawn.requested_model AS _configured_model, "
            "spawn.requested_reasoning_effort AS _configured_effort"
        )

    def _token_call_from(self, index: str) -> str:
        return (
            "FROM usage_provider_calls AS call INDEXED BY "
            f"{index} LEFT JOIN usage_spawn_requests AS spawn "
            "ON spawn.spawn_request_id = call.spawn_request_id"
        )

    def _token_call_thread_sql(
        self, thread_ids: Sequence[str], *, limit: int, columns_only: bool = False
    ) -> tuple[str, tuple[Any, ...]]:
        placeholders = ",".join("?" for _ in thread_ids)
        select = "call.provider_call_id, call.thread_id" if columns_only else self._token_call_select()
        params: tuple[Any, ...] = (*thread_ids, limit)
        for index in (THREAD_STARTED_INDEX, "usage_provider_calls_thread_idx"):
            sql = (
                f"SELECT {select} {self._token_call_from(index)} "
                f"WHERE call.thread_id IN ({placeholders}) "
                "ORDER BY call.started_at, call.provider_call_id LIMIT ?"
            )
            assert self.conn is not None
            try:
                plan = self.conn.execute("EXPLAIN QUERY PLAN " + sql, params).fetchall()
            except sqlite3.OperationalError as exc:
                if "no such index" in str(exc).lower() and index == THREAD_STARTED_INDEX:
                    continue
                raise
            details = [str(row["detail"]) for row in plan]
            if not any("SEARCH" in detail and "usage_provider_calls" in detail for detail in details):
                raise ReportProblem("unavailable", "required_reporting_index_missing")
            if self.diagnostics_enabled:
                self.query_plans.append(
                    {"query": "exact_root_thread_call_stream", "details": details}
                )
            self._verified_call_thread_index = index
            return sql, params
        raise ReportProblem("incomplete", "required_reporting_index_missing")

    def _selected_token_call_ids(
        self, scope_thread_ids: Sequence[str]
    ) -> set[str]:
        """Retain only the bounded ID frontier, never selected call rows."""
        selected: set[str] = set()
        rows_seen = 0
        remaining = self.max_calls
        for batch in _sql_chunks(sorted(set(scope_thread_ids))):
            if not batch:
                break
            self.check_deadline()
            sql, params = self._token_call_thread_sql(
                batch, limit=max(1, remaining + 1), columns_only=True
            )
            cursor = self._execute("exact_root_thread_call_ids", sql, params)
            for row in cursor:
                self.check_deadline()
                if rows_seen >= self.max_calls:
                    self.call_selection_truncated = True
                    break
                values = self._account_output_text(row)
                rows_seen += 1
                remaining -= 1
                call_id = _blank_to_none(values.get("provider_call_id"))
                if call_id is not None:
                    selected.add(call_id)
            if self.call_selection_truncated:
                break
        self.selected_rows = rows_seen
        return selected

    def _probe_token_call(self, provider_call_id: str) -> dict[str, Any] | None:
        columns = self._token_call_select()
        sql = (
            f"SELECT {columns} FROM usage_provider_calls AS call "
            "LEFT JOIN usage_spawn_requests AS spawn "
            "ON spawn.spawn_request_id = call.spawn_request_id "
            "WHERE call.provider_call_id = ?"
        )
        assert self.conn is not None
        if not self._verified_call_id_index:
            plan = self.conn.execute("EXPLAIN QUERY PLAN " + sql, (provider_call_id,)).fetchall()
            if not any(
                "SEARCH" in str(row["detail"])
                and "provider_call_id" in str(row["detail"])
                for row in plan
            ):
                raise ReportProblem("unavailable", "required_reporting_index_missing")
            if self.diagnostics_enabled:
                self.query_plans.append(
                    {
                        "query": "exact_provider_call_id_probe",
                        "details": [str(row["detail"]) for row in plan],
                    }
                )
            self._verified_call_id_index = True
        row = self._execute("exact_provider_call_id_probe", sql, (provider_call_id,)).fetchone()
        return self._account_output_text(row) if row is not None else None

    def _iter_token_calls(
        self, scope_thread_ids: Sequence[str], selected_rows: int
    ) -> Iterator[dict[str, Any]]:
        rows_yielded = 0
        remaining = selected_rows
        for batch in _sql_chunks(sorted(set(scope_thread_ids))):
            if not batch or remaining <= 0:
                break
            self.check_deadline()
            sql, params = self._token_call_thread_sql(batch, limit=remaining)
            cursor = self._execute("exact_root_thread_call_rows", sql, params)
            for row in cursor:
                self.check_deadline()
                values = self._account_output_text(row)
                for column in TOKEN_FIELDS.values():
                    values[column] = _nonnegative_integer(values.get(column))
                try:
                    values["_started_at_parsed"] = _supported_utc_text(
                        str(values.get("started_at"))
                    )
                except (ValueError, TypeError):
                    raise WorkLimitReached("unsupported_timestamp_encoding")
                rows_yielded += 1
                remaining -= 1
                yield values
        self.selected_rows = rows_yielded

    def _fetch_spawn_requests(
        self, calls: Sequence[dict[str, Any]]
    ) -> dict[str, dict[str, Any]]:
        request_ids = sorted(
            {
                str(call["spawn_request_id"])
                for call in calls
                if _blank_to_none(call.get("spawn_request_id")) is not None
            }
        )
        if len(request_ids) > MAX_LINEAGE_NODES:
            raise WorkLimitReached("spawn_request_metadata_limit_exceeded")
        result: dict[str, dict[str, Any]] = {}
        for batch in _sql_chunks(request_ids):
            self.check_deadline()
            placeholders = ",".join("?" for _ in batch)
            sql = (
                f"SELECT {SPAWN_COLUMNS} FROM usage_spawn_requests "
                f"WHERE spawn_request_id IN ({placeholders}) LIMIT ?"
            )
            params = (*batch, len(batch) + 1)
            assert self.conn is not None
            plan = self.conn.execute("EXPLAIN QUERY PLAN " + sql, params).fetchall()
            if not any(
                "SEARCH usage_spawn_requests" in str(row["detail"])
                for row in plan
            ):
                raise ReportProblem(
                    "unavailable", "required_spawn_request_index_missing"
                )
            for row in self._execute("spawn_request_metadata", sql, params):
                self.check_deadline()
                if len(result) >= MAX_LINEAGE_NODES:
                    raise WorkLimitReached("spawn_request_metadata_limit_exceeded")
                values = self._account_output_text(row)
                result[str(values["spawn_request_id"])] = values
        return result

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
        if self.credit_mode == "none":
            result.pop("actual_mode", None)
            result.pop("standard_scenario", None)
            result.pop("provider_reported_credits", None)
        elif self.credit_mode == "observed_or_effective_rate":
            result.pop("standard_scenario")
        elif self.credit_mode == "supplied_standard_scenario":
            result.pop("actual_mode")
            result.pop("provider_reported_credits")
        return result

    def _serialize_subtree_metric(
        self, metrics: dict[str, Any], *, compact_aggregate: bool = False
    ) -> dict[str, Any]:
        """Keep model partitions useful without repeating global rate evidence."""
        result = self._serialize_metric(metrics)
        model_rows = result.get("by_actual_model")
        if model_rows is not None:
            compact_rows: list[dict[str, Any]] = []
            for row in model_rows:
                compact: dict[str, Any] = {
                    key: row[key]
                    for key in (
                        "actual_model_used",
                        "provider_call_count",
                        "token_sums_known_subtotal",
                        "token_missing_call_count",
                        "missing_usage_call_count",
                        "requested_to_actual_model_evidence",
                        "provider_reported_credits",
                    )
                    if key in row
                }
                for mode_name in ("actual_mode", "standard_scenario"):
                    mode = row.get(mode_name)
                    if mode is None:
                        continue
                    compact[mode_name] = {
                        key: mode[key]
                        for key in (
                            "covered_call_count",
                            "rate_estimated_call_count",
                            "uncovered_call_count",
                            "credits_subtotal",
                            "complete_total",
                            "uncovered_reasons",
                        )
                        if key in mode
                    }
                compact_rows.append(compact)
            result["by_actual_model"] = compact_rows

        if compact_aggregate:
            for key in (
                "actual_model_evidence",
                "requested_to_actual_model_evidence",
                "actual_service_tier_evidence",
                "service_tier_source_evidence",
                "fast_mode_used_evidence",
            ):
                result.pop(key, None)
            for mode_name in ("actual_mode", "standard_scenario"):
                mode = result.get(mode_name)
                if mode is not None:
                    mode.pop("rate_id_call_counts", None)
                    mode.pop("policy_id_call_counts", None)
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
            seek_start, seek_end = (
                (None, None)
                if self.all_time
                else _seek_bounds(self.start, self.end)
            )
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
                if self.all_time or self.start <= at < self.end:
                    call["_started_at_parsed"] = at
                    filtered_calls.append(call)
            if unsupported_timestamps:
                raise WorkLimitReached("unsupported_timestamp_encoding")
            calls = filtered_calls
            self.spawn_request_metadata = self._fetch_spawn_requests(calls)
            if self.scope in {"thread", "subtree"} and not scope_nodes and not calls:
                raise ReportProblem("unavailable", "unknown_thread_id")
            if len(scope_nodes) > MAX_OUTPUT_GROUPS and not self.token_only_projection:
                raise WorkLimitReached("output_group_limit_exceeded")
            participant_limit = (
                MAX_LINEAGE_NODES
                if self.token_only_projection
                else MAX_OUTPUT_GROUPS
            )
            participating = set(scope_nodes) if self.scope == "subtree" else set()
            if (
                self.scope == "thread"
                and self.thread_id
                and self.thread_id not in participating
            ):
                if len(participating) >= participant_limit:
                    raise WorkLimitReached("output_group_limit_exceeded")
                participating.add(self.thread_id)
            call_thread_ids: set[str] = set()
            for call in calls:
                thread_id = str(call["thread_id"])
                if thread_id in call_thread_ids:
                    continue
                if thread_id not in participating:
                    if len(participating) >= participant_limit:
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
            requested_subtree_nodes: set[str] = set()
            if self.scope == "subtree" and self.thread_id:
                children_by_parent: dict[str, list[str]] = defaultdict(list)
                for thread_id in scope_nodes:
                    parent = call_parent.get(thread_id)
                    if parent in scope_nodes:
                        children_by_parent[parent].append(thread_id)
                frontier = [self.thread_id]
                while frontier:
                    self.check_deadline()
                    parent = frontier.pop()
                    if parent in requested_subtree_nodes:
                        continue
                    requested_subtree_nodes.add(parent)
                    frontier.extend(children_by_parent.get(parent, ()))
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
                thread_lineage = lineage.get(thread_id) or self._resolve_lineage(
                    thread_id, metadata, missing_metadata
                )
                spawn_request = self.spawn_request_metadata.get(
                    str(call.get("spawn_request_id"))
                )
                self.selected_call_rows.append(
                    {
                        **{
                            column: call.get(column)
                            for column in self.call_columns.split(", ")
                        },
                        "_requested_subtree_member": (
                            thread_id in requested_subtree_nodes
                        ),
                        "_resolved_root_thread_id": thread_lineage.get(
                            "resolved_root_thread_id"
                        ),
                        "_lineage_status": thread_lineage.get("lineage_status"),
                        "_configured_model": (
                            _blank_to_none(spawn_request.get("requested_model"))
                            if spawn_request
                            else None
                        ),
                        "_configured_effort": (
                            _blank_to_none(
                                spawn_request.get("requested_reasoning_effort")
                            )
                            if spawn_request
                            else None
                        ),
                    }
                )
                if thread_id not in thread_metrics:
                    if (
                        thread_id not in participating
                        and len(participating) >= participant_limit
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
            cycle_seen = cycle_seen or any(
                item.get("lineage_status") == "cycle" for item in lineage.values()
            )

            thread_rows: list[dict[str, Any]] = []
            for thread_id in (
                sorted(participating) if self.scope != "subtree" else ()
            ):
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
            disjoint_roots = (
                {
                    root: self._serialize_metric(_merge_metrics(items))
                    for root, items in sorted(root_buckets.items())
                }
                if self.scope != "subtree"
                else {}
            )
            if self.scope == "subtree":
                safe_root_groups: dict[str, list[dict[str, Any]]] = defaultdict(list)
                safe_root_buckets: dict[str, dict[str, Any]] = {}
                for root_id, items in root_buckets.items():
                    label = (
                        "requested_root"
                        if root_id == self.thread_id
                        else "unresolved"
                        if root_id == "unresolved"
                        else "conflicting_root"
                    )
                    safe_root_groups[label].extend(items)
                safe_root_buckets = {
                    label: self._serialize_subtree_metric(
                        _merge_metrics(items), compact_aggregate=True
                    )
                    for label, items in sorted(safe_root_groups.items())
                }
                safe_thread_rows = [
                    {
                        "thread_id": self.thread_id,
                        "thread_role": "requested_root",
                        "metrics": self._serialize_subtree_metric(
                            thread_metrics.get(self.thread_id or "", _new_metrics()),
                            compact_aggregate=True,
                        ),
                    }
                ]
            else:
                safe_root_buckets = disjoint_roots
                safe_thread_rows = thread_rows
            if (
                self.scope in {"thread", "subtree"}
                and self.thread_id
                and not self.token_only_projection
            ):
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
                    "own": self._serialize_subtree_metric(own, compact_aggregate=True)
                    if self.scope == "subtree"
                    else self._serialize_metric(own),
                    "direct_children": self._serialize_subtree_metric(
                        direct, compact_aggregate=True
                    )
                    if self.scope == "subtree"
                    else self._serialize_metric(direct),
                    "descendants_excluding_anchor": self._serialize_subtree_metric(
                        descendants, compact_aggregate=True
                    )
                    if self.scope == "subtree"
                    else self._serialize_metric(descendants),
                    "tree_including_anchor": self._serialize_subtree_metric(
                        tree, compact_aggregate=True
                    )
                    if self.scope == "subtree"
                    else self._serialize_metric(tree),
                    "disjoint_call_counts_reconcile": tree["provider_call_count"]
                    == all_calls_metrics["provider_call_count"],
                }
                if self.scope == "subtree":
                    scope_contributions["root_vs_descendants_by_actual_model"] = {
                        "requested_root": self._serialize_subtree_metric(own)[
                            "by_actual_model"
                        ],
                        "all_descendants": self._serialize_subtree_metric(descendants)[
                            "by_actual_model"
                        ],
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
            if self.lineage_walk_incomplete:
                report_status = "incomplete"
                reasons.append("lineage_incomplete")
            if self.lineage_snapshot_truncated:
                report_status = "incomplete"
                reasons.append("lineage_scan_row_limit_exceeded")
            if self.call_selection_truncated:
                report_status = "incomplete"
                reasons.append("call_limit_exceeded")
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
            window_local = (
                None
                if self.all_time
                else {
                    "start": self.start.astimezone(self.timezone).isoformat(),
                    "end_exclusive": self.end.astimezone(self.timezone).isoformat(),
                }
            )
            selected_window_call_bounds = (
                {
                    "earliest_started_at_utc": _iso_utc(
                        min(call["_started_at_parsed"] for call in calls)
                    ),
                    "latest_started_at_utc": _iso_utc(
                        max(call["_started_at_parsed"] for call in calls)
                    ),
                }
                if calls
                else None
            )
            report = {
                "schema_version": REPORT_VERSION,
                "status": report_status,
                "incomplete_reasons": sorted(set(reasons)),
                "scope": {"kind": self.scope, "thread_id": self.thread_id},
                "window": {
                    "kind": "all_time" if self.all_time else "bounded",
                    "start_utc_inclusive": (
                        None if self.all_time else _iso_utc(self.start)
                    ),
                    "end_utc_exclusive": None if self.all_time else _iso_utc(self.end),
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
                "source_coverage": {
                    "selected_ledger_scope": "only the exact --database input",
                    "selected_provider_call_rows": self.selected_rows,
                    "call_selection_complete": not self.call_selection_truncated,
                    "lineage_snapshot_complete": not (
                        self.lineage_snapshot_truncated or self.lineage_walk_incomplete
                    ),
                    "requested_window_utc": {
                        "kind": "all_time" if self.all_time else "bounded",
                        "start_inclusive": (
                            None if self.all_time else _iso_utc(self.start)
                        ),
                        "end_exclusive": None if self.all_time else _iso_utc(self.end),
                    },
                    "selected_window_provider_call_bounds_utc": selected_window_call_bounds,
                    "other_ledgers": "not queried or merged",
                    "physical_call_deduplication": "rows in the selected ledger are counted as stored; no content-based or cross-ledger mirror reconciliation is attempted",
                    "model_identity": "provider-call requested model and effective actual model are distinct; configured model and reasoning effort are sourced only from an exact spawn_request_id match in usage_spawn_requests, otherwise unrecorded",
                },
                "summary": (
                    self._serialize_subtree_metric(all_calls_metrics)
                    if self.scope == "subtree"
                    else self._serialize_metric(all_calls_metrics)
                ),
                "threads": safe_thread_rows,
                "resolved_root_buckets": safe_root_buckets,
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
                    "descendant_thread_count": (
                        max(0, len(participating) - 1)
                        if self.scope == "subtree"
                        else None
                    ),
                    "unresolved_thread_count": unresolved_lineage,
                    "lineage_status_counts": {
                        status: sum(
                            1
                            for item in lineage.values()
                            if item.get("lineage_status") == status
                        )
                        for status in sorted(
                            {
                                str(item.get("lineage_status"))
                                for item in lineage.values()
                            }
                        )
                    },
                    "known_zero_call_thread_count": sum(
                        1
                        for thread_id in participating
                        if thread_metrics.get(thread_id, _new_metrics())[
                            "provider_call_count"
                        ]
                        == 0
                    ),
                    "strategy": self.lineage_strategy,
                    "metadata_snapshot_rows": self.lineage_scan_rows,
                    "metadata_snapshot_text_chars": self.lineage_scan_text_chars,
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
                raise WorkLimitReached("sqlite_value_limit_exceeded") from exc
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
                raise WorkLimitReached("sqlite_value_limit_exceeded") from exc
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
            "lineage_strategy": self.lineage_strategy,
            "lineage_scan_rows": self.lineage_scan_rows,
            "lineage_scan_text_chars": self.lineage_scan_text_chars,
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
        "--historical-database",
        help="optional exact second ledger to reconcile with --database",
    )
    parser.add_argument(
        "--all-time",
        action="store_true",
        help="read all supported UTC timestamps for an exact thread/subtree scope",
    )
    parser.add_argument("--start-utc", help="inclusive ISO-8601 UTC timestamp")
    parser.add_argument("--end-utc", help="exclusive ISO-8601 UTC timestamp")
    parser.add_argument("--timezone", required=True, help="IANA presentation timezone")
    parser.add_argument("--scope", required=True, choices=("all", "thread", "subtree"))
    parser.add_argument("--thread-id")
    parser.add_argument(
        "--credit-mode",
        required=True,
        choices=("none", "observed_or_effective_rate", "supplied_standard_scenario", "both"),
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
    if args.all_time:
        if args.start_utc is not None or args.end_utc is not None:
            parser.error("--all-time cannot be combined with --start-utc or --end-utc")
        if args.scope != "subtree":
            parser.error("--all-time requires exact subtree scope")
        args.start = dt.datetime.min.replace(tzinfo=UTC)
        args.end = dt.datetime.max.replace(tzinfo=UTC)
    else:
        if args.start_utc is None or args.end_utc is None:
            parser.error("--start-utc and --end-utc are required unless --all-time is set")
        try:
            args.start = parse_utc(args.start_utc)
            args.end = parse_utc(args.end_utc)
        except ValueError as exc:
            parser.error(str(exc))
        if args.start >= args.end:
            parser.error("start must be earlier than end")
    if args.historical_database and args.scope != "subtree":
        parser.error("--historical-database is only supported for subtree scope")
    if (args.historical_database or args.all_time) and args.credit_mode != "none":
        parser.error("all-time and multi-ledger reports require --credit-mode none")
    if args.historical_database and Path(args.database).resolve() == Path(
        args.historical_database
    ).resolve():
        parser.error("current and historical database paths must differ")
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
                "kind": "all_time" if args.all_time else "bounded",
                "start_utc_inclusive": None if args.all_time else _iso_utc(args.start),
                "end_utc_exclusive": None if args.all_time else _iso_utc(args.end),
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
        fallback = WorkLimitReached("serialized_error_limit_exceeded")
        _write_bounded_json(_error_report(fallback, None), sys.stdout)


def _write_bounded_json(
    report: dict[str, Any],
    output: Any,
    check_deadline: Callable[[], None] | None = None,
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
            raise WorkLimitReached("serialized_json_limit_exceeded")
        if check_deadline is not None:
            check_deadline()
        chunks.append(chunk)
    if check_deadline is not None:
        check_deadline()
    for chunk in chunks:
        output.write(chunk)
    output.write("\n")


DEDUP_FIELDS = tuple(
    field.strip()
    for field in TOKEN_CALL_COLUMNS.split(",")
    if field.strip() != "provider_call_id"
)


def _dedupe_value(field: str, value: Any) -> Any:
    if field in TOKEN_FIELDS.values() or field == "fast_mode_used":
        return _nonnegative_integer(value)
    if field == "provider_reported_credits":
        if value is None:
            return None
        try:
            return str(DECIMAL_CONTEXT.create_decimal(str(value)).normalize())
        except (decimal.DecimalException, ValueError, TypeError):
            return ("invalid_numeric", type(value).__name__, str(value))
    if value is None:
        return None
    return (type(value).__name__, str(value))


def _call_signature(row: dict[str, Any]) -> tuple[Any, ...]:
    return tuple(_dedupe_value(field, row.get(field)) for field in DEDUP_FIELDS)


def _new_token_metrics() -> dict[str, Any]:
    return {
        "provider_call_count": 0,
        "token_sums_known_subtotal": {name: 0 for name in TOKEN_FIELDS},
        "token_missing_call_count": {name: 0 for name in TOKEN_FIELDS},
        "actual_model_evidence": defaultdict(int),
        "requested_to_actual_model_evidence": defaultdict(int),
        "configured_model_effort_evidence": defaultdict(int),
    }


def _add_token_row(metrics: dict[str, Any], row: dict[str, Any]) -> None:
    metrics["provider_call_count"] += 1
    requested = _blank_to_none(row.get("requested_model"))
    actual = _blank_to_none(row.get("actual_model_used"))
    configured = _blank_to_none(row.get("_configured_model"))
    effort = _blank_to_none(row.get("_configured_effort"))
    metrics["actual_model_evidence"][actual] += 1
    metrics["requested_to_actual_model_evidence"][(requested, actual)] += 1
    metrics["configured_model_effort_evidence"][(configured, effort, requested, actual)] += 1
    for output_name, column in TOKEN_FIELDS.items():
        value = _nonnegative_integer(row.get(column))
        if value is None:
            metrics["token_missing_call_count"][output_name] += 1
        else:
            metrics["token_sums_known_subtotal"][output_name] += value


def _serialize_token_metrics(metrics: dict[str, Any]) -> dict[str, Any]:
    return {
        "provider_call_count": metrics["provider_call_count"],
        "token_sums_known_subtotal": metrics["token_sums_known_subtotal"],
        "token_missing_call_count": metrics["token_missing_call_count"],
        "actual_model_evidence": [
            {"actual_model_used": model, "call_count": count}
            for model, count in sorted(
                metrics["actual_model_evidence"].items(),
                key=lambda item: (item[0] is None, item[0] or ""),
            )
        ],
        "requested_to_actual_model_evidence": [
            {
                "requested_model": requested,
                "actual_model_used": actual,
                "call_count": count,
            }
            for (requested, actual), count in sorted(
                metrics["requested_to_actual_model_evidence"].items(),
                key=lambda item: (
                    item[0][0] is None,
                    item[0][0] or "",
                    item[0][1] is None,
                    item[0][1] or "",
                ),
            )
        ],
        "configured_model_effort_evidence": [
            {
                "configured_model_from_spawn_request": configured,
                "configured_effort_from_spawn_request": effort,
                "provider_requested_model": requested,
                "provider_actual_model": actual,
                "call_count": count,
            }
            for (configured, effort, requested, actual), count in sorted(
                metrics["configured_model_effort_evidence"].items(),
                key=lambda item: tuple(
                    (value is None, value or "") for value in item[0]
                ),
            )
        ],
        "configured_model_unrecorded_call_count": sum(
            count
            for (configured, _effort, _requested, _actual), count in metrics[
                "configured_model_effort_evidence"
            ].items()
            if configured is None
        ),
        "configured_effort_unrecorded_call_count": sum(
            count
            for (_configured, effort, _requested, _actual), count in metrics[
                "configured_model_effort_evidence"
            ].items()
            if effort is None
        ),
    }


def _merge_exact_root_ledgers(
    ledger_rows: Sequence[tuple[str, Sequence[dict[str, Any]]]],
    root_thread_id: str,
    check_deadline: Callable[[], None] | None = None,
) -> dict[str, Any]:
    if check_deadline is not None:
        check_deadline()
    by_call_id: dict[str, list[tuple[str, dict[str, Any]]]] = defaultdict(list)
    unidentified_rows = 0
    for source_name, rows in ledger_rows:
        for row in rows:
            if check_deadline is not None:
                check_deadline()
            call_id = _blank_to_none(row.get("provider_call_id"))
            if call_id is None:
                unidentified_rows += 1
            else:
                by_call_id[call_id].append((source_name, row))

    canonical_rows: list[dict[str, Any]] = []
    identical_duplicate_rows = 0
    conflict_ids = 0
    conflicting_rows = 0
    configured_attribution_conflicts = 0
    for matches in by_call_id.values():
        if check_deadline is not None:
            check_deadline()
        if len(matches) == 1:
            canonical_rows.append(matches[0][1])
            continue
        signatures: set[tuple[Any, ...]] = set()
        for _source_name, row in matches:
            if check_deadline is not None:
                check_deadline()
            signatures.add(_call_signature(row))
        if len(signatures) == 1:
            canonical = dict(matches[0][1])
            known_configurations: set[tuple[Any, Any]] = set()
            for _source_name, row in matches:
                if check_deadline is not None:
                    check_deadline()
                configured_model = row.get("_configured_model")
                configured_effort = row.get("_configured_effort")
                if configured_model is not None or configured_effort is not None:
                    known_configurations.add((configured_model, configured_effort))
            if len(known_configurations) > 1:
                configured_attribution_conflicts += 1
                canonical["_configured_model"] = None
                canonical["_configured_effort"] = None
                canonical["_configured_attribution_conflict"] = True
            elif known_configurations:
                configured_model, configured_effort = next(iter(known_configurations))
                canonical["_configured_model"] = configured_model
                canonical["_configured_effort"] = configured_effort
            canonical_rows.append(canonical)
            identical_duplicate_rows += len(matches) - 1
        else:
            conflict_ids += 1
            conflicting_rows += len(matches)

    overall = _new_token_metrics()
    roles = {
        "requested_root": _new_token_metrics(),
        "all_descendants": _new_token_metrics(),
        "unresolved_lineage": _new_token_metrics(),
        "conflicting_root": _new_token_metrics(),
    }
    model_metrics: dict[str | None, dict[str, Any]] = defaultdict(_new_token_metrics)
    role_model_metrics: dict[str, dict[str | None, dict[str, Any]]] = {
        role: defaultdict(_new_token_metrics) for role in roles
    }
    earliest_started_at: dt.datetime | None = None
    latest_started_at: dt.datetime | None = None
    for row in canonical_rows:
        if check_deadline is not None:
            check_deadline()
        _add_token_row(overall, row)
        actual = _blank_to_none(row.get("actual_model_used"))
        _add_token_row(model_metrics[actual], row)
        resolved_root = _blank_to_none(row.get("_resolved_root_thread_id"))
        lineage_status = row.get("_lineage_status")
        thread_id = _blank_to_none(row.get("thread_id"))
        if resolved_root is not None and resolved_root != root_thread_id:
            role = "conflicting_root"
        elif lineage_status in {
            "conflicting_recorded_roots",
            "recorded_root_unverified",
        }:
            role = "conflicting_root"
        elif thread_id == root_thread_id:
            role = "requested_root"
        elif resolved_root == root_thread_id:
            role = "all_descendants"
        elif (
            row.get("_requested_subtree_member")
            and lineage_status == "root_unrecorded"
        ):
            # The exact subtree walk establishes membership relative to the
            # requested anchor even when legacy rows lack a recorded self-root.
            role = "all_descendants"
        elif resolved_root is None:
            role = "unresolved_lineage"
        else:
            role = "conflicting_root"
        _add_token_row(roles[role], row)
        _add_token_row(role_model_metrics[role][actual], row)
        try:
            started_at = _supported_utc_text(str(row.get("started_at")))
            if earliest_started_at is None or started_at < earliest_started_at:
                earliest_started_at = started_at
            if latest_started_at is None or started_at > latest_started_at:
                latest_started_at = started_at
        except (ValueError, TypeError):
            # Per-ledger reporters already fail closed on unsupported encodings.
            pass

    def model_rows(source: dict[str | None, dict[str, Any]]) -> list[dict[str, Any]]:
        if check_deadline is not None:
            check_deadline()
        sorted_metrics = sorted(
            source.items(), key=lambda item: (item[0] is None, item[0] or "")
        )
        if check_deadline is not None:
            check_deadline()
        rows: list[dict[str, Any]] = []
        for model, metrics in sorted_metrics:
            if check_deadline is not None:
                check_deadline()
            rows.append(
                {"actual_model_used": model, **_serialize_token_metrics(metrics)}
            )
        return rows

    selected_call_rows = 0
    for _source_name, rows in ledger_rows:
        if check_deadline is not None:
            check_deadline()
        selected_call_rows += len(rows)

    result = {
        "summary": _serialize_token_metrics(overall),
        "by_actual_model": model_rows(model_metrics),
        "root_vs_descendants_by_actual_model": {
            role: model_rows(role_model_metrics[role]) for role in roles
        },
        "scope_totals": {
            role: _serialize_token_metrics(metrics) for role, metrics in roles.items()
        },
        "selected_provider_call_rows": selected_call_rows,
        "deduplicated_provider_call_count": len(canonical_rows),
        "identical_mirrored_rows_collapsed": identical_duplicate_rows,
        "conflicting_call_id_count": conflict_ids,
        "rows_excluded_for_conflicting_call_ids": conflicting_rows,
        "rows_excluded_for_missing_call_id": unidentified_rows,
        "configured_attribution_conflict_provider_call_count": configured_attribution_conflicts,
        "selected_window_provider_call_bounds_utc": (
            {
                "earliest_started_at_utc": _iso_utc(earliest_started_at),
                "latest_started_at_utc": _iso_utc(latest_started_at),
            }
            if earliest_started_at is not None and latest_started_at is not None
            else None
        ),
        "physical_call_deduplication": (
            "same stable provider_call_id counted once only when every selected token-report provider-call field, including exact thread/provider/time/model/token identity, matches; conflicting or unidentified rows are excluded"
        ),
        "configured_model_source": "usage_spawn_requests.requested_model and requested_reasoning_effort joined only by exact provider-call spawn_request_id; missing values remain unrecorded, and cross-ledger disagreement is reported without choosing a value",
    }
    if check_deadline is not None:
        check_deadline()
    return result


class _ExactRootTokenAccumulator:
    def __init__(self, root_thread_id: str) -> None:
        self.root_thread_id = root_thread_id
        self.overall = _new_token_metrics()
        self.roles = {
            "requested_root": _new_token_metrics(),
            "all_descendants": _new_token_metrics(),
            "unresolved_lineage": _new_token_metrics(),
            "conflicting_root": _new_token_metrics(),
        }
        self.models: dict[str | None, dict[str, Any]] = defaultdict(_new_token_metrics)
        self.role_models: dict[str, dict[str | None, dict[str, Any]]] = {
            role: defaultdict(_new_token_metrics) for role in self.roles
        }
        self.earliest: dt.datetime | None = None
        self.latest: dt.datetime | None = None
        self.count = 0

    def add(self, row: dict[str, Any]) -> None:
        _add_token_row(self.overall, row)
        actual = _blank_to_none(row.get("actual_model_used"))
        _add_token_row(self.models[actual], row)
        resolved_root = _blank_to_none(row.get("_resolved_root_thread_id"))
        lineage_status = row.get("_lineage_status")
        thread_id = _blank_to_none(row.get("thread_id"))
        if resolved_root is not None and resolved_root != self.root_thread_id:
            role = "conflicting_root"
        elif lineage_status in {"conflicting_recorded_roots", "recorded_root_unverified"}:
            role = "conflicting_root"
        elif thread_id == self.root_thread_id:
            role = "requested_root"
        elif resolved_root == self.root_thread_id:
            role = "all_descendants"
        elif row.get("_requested_subtree_member") and lineage_status == "root_unrecorded":
            role = "all_descendants"
        elif resolved_root is None:
            role = "unresolved_lineage"
        else:
            role = "conflicting_root"
        _add_token_row(self.roles[role], row)
        _add_token_row(self.role_models[role][actual], row)
        at = row.get("_started_at_parsed")
        if isinstance(at, dt.datetime):
            if self.earliest is None or at < self.earliest:
                self.earliest = at
            if self.latest is None or at > self.latest:
                self.latest = at
        self.count += 1

    @staticmethod
    def _model_rows(source: dict[str | None, dict[str, Any]]) -> list[dict[str, Any]]:
        return [
            {"actual_model_used": model, **_serialize_token_metrics(metrics)}
            for model, metrics in sorted(
                source.items(), key=lambda item: (item[0] is None, item[0] or "")
            )
        ]

    def report(self, counters: dict[str, Any]) -> dict[str, Any]:
        return {
            "summary": _serialize_token_metrics(self.overall),
            "by_actual_model": self._model_rows(self.models),
            "root_vs_descendants_by_actual_model": {
                role: self._model_rows(metrics)
                for role, metrics in self.role_models.items()
            },
            "scope_totals": {
                role: _serialize_token_metrics(metrics)
                for role, metrics in self.roles.items()
            },
            "selected_provider_call_rows": counters["selected_provider_call_rows"],
            "deduplicated_provider_call_count": self.count,
            "selected_window_provider_call_bounds_utc": (
                {
                    "earliest_started_at_utc": _iso_utc(self.earliest),
                    "latest_started_at_utc": _iso_utc(self.latest),
                }
                if self.earliest is not None and self.latest is not None
                else None
            ),
            **counters,
            "physical_call_deduplication": (
                "same stable provider_call_id counted once only when every selected token-report provider-call field, including exact thread/provider/time/model/token identity, matches; conflicting or unidentified rows are excluded"
            ),
            "configured_model_source": (
                "usage_spawn_requests.requested_model and requested_reasoning_effort joined only by exact provider-call spawn_request_id; missing values remain unrecorded, and cross-ledger disagreement is reported without choosing a value"
            ),
        }


def _prepare_exact_root_reporter(
    reporter: UsageReporter, root_thread_id: str
) -> dict[str, Any]:
    reporter.open()
    anchor = reporter._fetch_anchor(root_thread_id)
    if anchor is None:
        raise ReportProblem("unavailable", "unknown_thread_id")
    nodes, cycle_seen = reporter._walk_subtree(anchor)
    reporter.lineage_nodes = len(nodes)
    return {
        "scope_nodes": nodes,
        "cycle_seen": cycle_seen,
    }


def _annotate_exact_root_call(
    row: dict[str, Any], reporter: UsageReporter, root_thread_id: str
) -> dict[str, Any]:
    call = dict(row)
    thread_id = str(call.get("thread_id"))
    call["_requested_subtree_member"] = thread_id != root_thread_id
    call["_resolved_root_thread_id"] = root_thread_id
    call["_lineage_status"] = (
        "resolved_self_root" if thread_id == root_thread_id else "resolved_via_spawn_path"
    )
    return call


def _merge_configured_attribution(
    first: dict[str, Any], second: dict[str, Any]
) -> tuple[dict[str, Any], bool]:
    merged = dict(first)
    configurations = {
        (row.get("_configured_model"), row.get("_configured_effort"))
        for row in (first, second)
        if row.get("_configured_model") is not None
        or row.get("_configured_effort") is not None
    }
    if len(configurations) > 1:
        merged["_configured_model"] = None
        merged["_configured_effort"] = None
        merged["_configured_attribution_conflict"] = True
        return merged, True
    if configurations:
        model, effort = next(iter(configurations))
        merged["_configured_model"] = model
        merged["_configured_effort"] = effort
    return merged, False


def _run_exact_root_stream_report(
    args: argparse.Namespace,
    *,
    deadline: float | None = None,
    check_deadline: Callable[[], None] | None = None,
) -> dict[str, Any]:
    """Reconcile an exact-root all-time report without retaining call rows."""
    deadline = deadline if deadline is not None else time.monotonic() + MAX_SECONDS
    check_deadline = check_deadline or _deadline_checker(deadline)
    ledger_inputs = [("current", Path(args.database))]
    if args.historical_database:
        ledger_inputs.append(("historical", Path(args.historical_database)))
    shared_text_pool: dict[str, str] = {}
    shared_attribution_text: set[str] = set()
    shared_text_counter = {"characters": 0}
    shared_vm_counter = {"instructions": 0}
    reporters: list[UsageReporter] = []
    prepared: list[dict[str, Any]] = []
    selected_ids: list[set[str]] = []
    source_rows: list[int] = []
    remaining_calls = MAX_CALLS
    ledgers: list[dict[str, Any]] = []
    try:
        for index, (source_name, database) in enumerate(ledger_inputs):
            check_deadline()
            remaining_sources = len(ledger_inputs) - index
            source_call_budget = (remaining_calls + remaining_sources - 1) // remaining_sources
            reporter = UsageReporter(
                database=database,
                start=args.start,
                end=args.end,
                timezone=args.timezone_value,
                timezone_name=args.timezone,
                scope="subtree",
                thread_id=args.thread_id,
                credit_mode="none",
                diagnostics=args.diagnostics,
                all_time=True,
                token_only_projection=True,
                deadline=deadline,
                max_calls=source_call_budget,
                max_selected_text_chars=MAX_MULTI_LEDGER_ATTRIBUTION_TEXT_CHARS,
                max_vm_instructions=MAX_VM_INSTRUCTIONS,
                shared_text_pool=shared_text_pool,
                shared_attribution_text=shared_attribution_text,
                shared_text_counter=shared_text_counter,
                shared_vm_counter=shared_vm_counter,
            )
            reporters.append(reporter)
            state = _prepare_exact_root_reporter(reporter, args.thread_id)
            scope_ids = sorted(state["scope_nodes"])
            ids = reporter._selected_token_call_ids(scope_ids)
            selected_rows = reporter.selected_rows
            prepared.append(state)
            selected_ids.append(ids)
            source_rows.append(selected_rows)
            remaining_calls = max(0, remaining_calls - selected_rows)
            reasons: list[str] = []
            status = "complete"
            if reporter.lineage_walk_incomplete:
                status = "incomplete"
                reasons.append("lineage_incomplete")
            if state["cycle_seen"]:
                status = "incomplete"
                reasons.append("lineage_cycle")
            if reporter.call_selection_truncated:
                status = "incomplete"
                reasons.append("call_limit_exceeded")
            ledgers.append(
                {
                    "source": source_name,
                    "status": status,
                    "incomplete_reasons": reasons,
                    "selected_provider_call_rows": selected_rows,
                    "call_selection_complete": not reporter.call_selection_truncated,
                    "lineage_snapshot_complete": not (
                        reporter.lineage_walk_incomplete or state["cycle_seen"]
                    ),
                    "thread_count": len(scope_ids),
                    "descendant_thread_count": max(0, len(scope_ids) - 1),
                    "unresolved_thread_count": 0,
                    "lineage_strategy": reporter.lineage_strategy,
                    "metadata_snapshot_rows": 0,
                    "metadata_snapshot_text_chars": 0,
                    "selected_window_provider_call_bounds_utc": None,
                }
            )

        counters: dict[str, Any] = {
            "selected_provider_call_rows": sum(source_rows),
            "identical_mirrored_rows_collapsed": 0,
            "conflicting_call_id_count": 0,
            "rows_excluded_for_conflicting_call_ids": 0,
            "rows_excluded_for_missing_call_id": 0,
            "configured_attribution_conflict_provider_call_count": 0,
        }
        accumulator = _ExactRootTokenAccumulator(args.thread_id)

        def process_source(index: int) -> None:
            reporter = reporters[index]
            scope_ids = sorted(prepared[index]["scope_nodes"])
            other_index = 1 - index if len(reporters) == 2 else None
            earliest: dt.datetime | None = None
            latest: dt.datetime | None = None
            for raw_row in reporter._iter_token_calls(scope_ids, source_rows[index]):
                check_deadline()
                at = raw_row.get("_started_at_parsed")
                if isinstance(at, dt.datetime):
                    if earliest is None or at < earliest:
                        earliest = at
                    if latest is None or at > latest:
                        latest = at
                row = _annotate_exact_root_call(raw_row, reporter, args.thread_id)
                call_id = _blank_to_none(row.get("provider_call_id"))
                if call_id is None:
                    counters["rows_excluded_for_missing_call_id"] += 1
                    continue
                if (
                    other_index is not None
                    and index == 0
                    and call_id in selected_ids[other_index]
                ):
                    counterpart = reporters[other_index]._probe_token_call(call_id)
                    if (
                        counterpart is not None
                        and str(counterpart.get("thread_id"))
                        in prepared[other_index]["scope_nodes"]
                    ):
                        counterpart = _annotate_exact_root_call(
                            counterpart, reporters[other_index], args.thread_id
                        )
                        if _call_signature(row) != _call_signature(counterpart):
                            counters["conflicting_call_id_count"] += 1
                            counters["rows_excluded_for_conflicting_call_ids"] += 2
                            continue
                        canonical, configured_conflict = _merge_configured_attribution(
                            row, counterpart
                        )
                        if configured_conflict:
                            counters[
                                "configured_attribution_conflict_provider_call_count"
                            ] += 1
                        counters["identical_mirrored_rows_collapsed"] += 1
                        accumulator.add(canonical)
                        continue
                elif (
                    other_index is not None
                    and index == 1
                    and call_id in selected_ids[0]
                ):
                    counterpart = reporters[0]._probe_token_call(call_id)
                    if (
                        counterpart is not None
                        and str(counterpart.get("thread_id"))
                        in prepared[0]["scope_nodes"]
                    ):
                        continue
                accumulator.add(row)
            if earliest is not None and latest is not None:
                ledgers[index]["selected_window_provider_call_bounds_utc"] = {
                    "earliest_started_at_utc": _iso_utc(earliest),
                    "latest_started_at_utc": _iso_utc(latest),
                }

        process_source(0)
        if len(reporters) == 2:
            process_source(1)

        if shared_text_counter["characters"] > MAX_MULTI_LEDGER_ATTRIBUTION_TEXT_CHARS:
            raise WorkLimitReached("selected_attribution_text_limit_exceeded")
        if shared_vm_counter["instructions"] > MAX_VM_INSTRUCTIONS:
            raise WorkLimitReached("instruction_limit_exceeded")

        merged = accumulator.report(counters)
        reasons = sorted(
            {reason for ledger in ledgers for reason in ledger["incomplete_reasons"]}
        )
        status = (
            "incomplete"
            if any(ledger["status"] != "complete" for ledger in ledgers)
            else "complete"
        )
        source_selection_complete = all(
            ledger["call_selection_complete"] and ledger["lineage_snapshot_complete"]
            for ledger in ledgers
        )
        if not source_selection_complete:
            status = "incomplete"
            reasons.append("cross_ledger_reconciliation_limited_by_source_coverage")
        if merged["conflicting_call_id_count"] or merged["rows_excluded_for_missing_call_id"]:
            status = "incomplete"
            reasons.append("physical_call_deduplication_incomplete")
        if merged["configured_attribution_conflict_provider_call_count"]:
            status = "incomplete"
            reasons.append("configured_model_attribution_conflict")
        if any(value > 0 for value in merged["summary"]["token_missing_call_count"].values()):
            status = "incomplete"
            reasons.append("token_coverage_incomplete")
        check_deadline()
        return {
            "schema_version": "codex-local-usage-report/v2",
            "status": status,
            "incomplete_reasons": sorted(set(reasons)),
            "scope": {"kind": "subtree", "thread_id": args.thread_id},
            "window": {
                "kind": "all_time",
                "start_utc_inclusive": None,
                "end_utc_exclusive": None,
                "presentation_timezone": args.timezone,
            },
            "snapshot": {
                "captured_at_utc": _iso_utc(dt.datetime.now(UTC)),
                "provider_freshness": "not_asserted",
            },
            "source_coverage": {
                "ledger_sources": ledgers,
                "recorder_completeness": "unknown; reports cover recorded ledger rows only",
                "timestamp_encoding_coverage": "all selected rows must use supported UTC Z or +00:00 encodings; unsupported rows fail closed",
                "configured_model_and_effort": merged["configured_model_source"],
                "deduplication": merged["physical_call_deduplication"],
                "limits": {
                    "combined_selected_call_rows": MAX_CALLS,
                    "selected_attribution_text_characters": MAX_MULTI_LEDGER_ATTRIBUTION_TEXT_CHARS,
                    "selected_attribution_text_accounting": "unique requested, actual, configured model and effort strings are interned and budgeted once; identity, lineage, provider, timestamp and status strings use fixed per-field bounds and selected-row limits",
                    "sqlite_vm_instructions": MAX_VM_INSTRUCTIONS,
                    "elapsed_seconds": MAX_SECONDS,
                    "lineage_nodes_per_ledger": MAX_LINEAGE_NODES,
                },
            },
            "summary": merged["summary"],
            "by_actual_model": merged["by_actual_model"],
            "root_vs_descendants_by_actual_model": merged[
                "root_vs_descendants_by_actual_model"
            ],
            "scope_totals": merged["scope_totals"],
            "deduplication": {
                "source_selection_complete": source_selection_complete,
                **{
                    key: merged[key]
                    for key in (
                        "selected_provider_call_rows",
                        "deduplicated_provider_call_count",
                        "identical_mirrored_rows_collapsed",
                        "conflicting_call_id_count",
                        "rows_excluded_for_conflicting_call_ids",
                        "rows_excluded_for_missing_call_id",
                        "configured_attribution_conflict_provider_call_count",
                        "physical_call_deduplication",
                    )
                },
            },
            "selected_window_provider_call_bounds_utc": merged[
                "selected_window_provider_call_bounds_utc"
            ],
        }
    except sqlite3.Error as exc:
        stop_reason = next(
            (reporter.vm_stop_reason for reporter in reporters if reporter.vm_stop_reason),
            None,
        )
        if stop_reason is not None:
            raise WorkLimitReached(stop_reason) from exc
        message = str(exc).lower()
        if "no such index" in message or "no query solution" in message:
            raise ReportProblem(
                "incomplete", "required_reporting_index_missing"
            ) from exc
        if "no such table" in message or "no such column" in message:
            raise ReportProblem(
                "incomplete", "required_spawn_request_index_missing"
            ) from exc
        if "locked" in message or "busy" in message:
            raise ReportProblem("unavailable", "ledger_busy") from exc
        raise ReportProblem("unavailable", "sqlite_read_failed") from exc
    finally:
        for reporter in reporters:
            if reporter.conn is not None:
                try:
                    reporter.conn.rollback()
                except sqlite3.Error:
                    pass
                reporter.conn.close()
                reporter.conn = None


def _run_multi_ledger_report(
    args: argparse.Namespace,
    *,
    deadline: float | None = None,
    check_deadline: Callable[[], None] | None = None,
) -> dict[str, Any]:
    if args.all_time and args.scope == "subtree":
        return _run_exact_root_stream_report(
            args, deadline=deadline, check_deadline=check_deadline
        )
    deadline = deadline if deadline is not None else time.monotonic() + MAX_SECONDS
    check_deadline = check_deadline or _deadline_checker(deadline)
    ledger_inputs = [("current", Path(args.database))]
    if args.historical_database:
        ledger_inputs.append(("historical", Path(args.historical_database)))
    ledgers: list[dict[str, Any]] = []
    rows_by_source: list[tuple[str, Sequence[dict[str, Any]]]] = []
    remaining_calls = MAX_CALLS
    remaining_text = MAX_MULTI_LEDGER_ATTRIBUTION_TEXT_CHARS
    remaining_vm = MAX_VM_INSTRUCTIONS
    shared_text_pool: dict[str, str] = {}
    shared_attribution_text: set[str] = set()
    for source_index, (source_name, database) in enumerate(ledger_inputs):
        check_deadline()
        remaining_sources = len(ledger_inputs) - source_index
        source_call_budget = (
            remaining_calls + remaining_sources - 1
        ) // remaining_sources
        reporter = UsageReporter(
            database=database,
            start=args.start,
            end=args.end,
            timezone=args.timezone_value,
            timezone_name=args.timezone,
            scope=args.scope,
            thread_id=args.thread_id,
            credit_mode="none",
            diagnostics=args.diagnostics,
            all_time=args.all_time,
            token_only_projection=True,
            deadline=deadline,
            max_calls=source_call_budget,
            max_selected_text_chars=remaining_text,
            max_vm_instructions=remaining_vm,
            shared_text_pool=shared_text_pool,
            shared_attribution_text=shared_attribution_text,
        )
        try:
            report = reporter.run()
            rows_by_source.append((source_name, reporter.selected_call_rows))
            ledgers.append(
                {
                    "source": source_name,
                    "status": report["status"],
                    "incomplete_reasons": report["incomplete_reasons"],
                    "selected_provider_call_rows": reporter.selected_rows,
                    "thread_count": report["lineage_coverage"][
                        "participating_thread_count"
                    ],
                    "call_selection_complete": not reporter.call_selection_truncated,
                    "lineage_snapshot_complete": not (
                        reporter.lineage_snapshot_truncated
                        or reporter.lineage_walk_incomplete
                    ),
                    "descendant_thread_count": report["lineage_coverage"][
                        "descendant_thread_count"
                    ],
                    "unresolved_thread_count": report["lineage_coverage"][
                        "unresolved_thread_count"
                    ],
                    "lineage_strategy": report["lineage_coverage"]["strategy"],
                    "metadata_snapshot_rows": report["lineage_coverage"][
                        "metadata_snapshot_rows"
                    ],
                    "metadata_snapshot_text_chars": report["lineage_coverage"][
                        "metadata_snapshot_text_chars"
                    ],
                    "selected_window_provider_call_bounds_utc": report[
                        "source_coverage"
                    ]["selected_window_provider_call_bounds_utc"],
                }
            )
        except ReportProblem as exc:
            ledgers.append(
                {
                    "source": source_name,
                    "status": exc.status,
                    "incomplete_reasons": [exc.reason],
                    "selected_provider_call_rows": reporter.selected_rows,
                    "call_selection_complete": not reporter.call_selection_truncated,
                    "lineage_snapshot_complete": not (
                        reporter.lineage_snapshot_truncated
                        or reporter.lineage_walk_incomplete
                    ),
                    "thread_count": reporter.lineage_nodes,
                    "descendant_thread_count": None,
                    "unresolved_thread_count": None,
                    "lineage_strategy": reporter.lineage_strategy,
                    "metadata_snapshot_rows": reporter.lineage_scan_rows,
                    "metadata_snapshot_text_chars": reporter.lineage_scan_text_chars,
                    "selected_window_provider_call_bounds_utc": None,
                }
            )
        remaining_calls = max(0, remaining_calls - reporter.selected_rows)
        remaining_text = max(0, remaining_text - reporter.output_text_chars)
        remaining_vm = max(0, remaining_vm - reporter.vm_instructions)

    check_deadline()
    merged = _merge_exact_root_ledgers(
        rows_by_source,
        args.thread_id,
        check_deadline=check_deadline,
    )
    reasons = sorted(
        {
            reason
            for ledger in ledgers
            for reason in ledger["incomplete_reasons"]
        }
    )
    if any(ledger["status"] != "complete" for ledger in ledgers):
        status = "incomplete"
    else:
        status = "complete"
    source_selection_complete = all(
        ledger.get("call_selection_complete", False)
        and ledger.get("lineage_snapshot_complete", False)
        for ledger in ledgers
    )
    if not source_selection_complete:
        status = "incomplete"
        reasons.append("cross_ledger_reconciliation_limited_by_source_coverage")
    if merged["conflicting_call_id_count"] or merged["rows_excluded_for_missing_call_id"]:
        status = "incomplete"
        reasons.append("physical_call_deduplication_incomplete")
    if merged["configured_attribution_conflict_provider_call_count"]:
        status = "incomplete"
        reasons.append("configured_model_attribution_conflict")
    if any(
        value > 0
        for value in merged["summary"]["token_missing_call_count"].values()
    ):
        status = "incomplete"
        reasons.append("token_coverage_incomplete")
    check_deadline()
    incomplete_reasons = sorted(set(reasons))
    check_deadline()
    result = {
        "schema_version": "codex-local-usage-report/v2",
        "status": status,
        "incomplete_reasons": incomplete_reasons,
        "scope": {"kind": "subtree", "thread_id": args.thread_id},
        "window": {
            "kind": "all_time" if args.all_time else "bounded",
            "start_utc_inclusive": None if args.all_time else _iso_utc(args.start),
            "end_utc_exclusive": None if args.all_time else _iso_utc(args.end),
            "presentation_timezone": args.timezone,
        },
        "snapshot": {
            "captured_at_utc": _iso_utc(dt.datetime.now(UTC)),
            "provider_freshness": "not_asserted",
        },
        "source_coverage": {
            "ledger_sources": ledgers,
            "recorder_completeness": "unknown; reports cover recorded ledger rows only",
            "timestamp_encoding_coverage": "all selected rows must use supported UTC Z or +00:00 encodings; unsupported rows fail closed",
            "configured_model_and_effort": merged["configured_model_source"],
            "deduplication": merged["physical_call_deduplication"],
            "limits": {
                "combined_selected_call_rows": MAX_CALLS,
                "selected_attribution_text_characters": MAX_MULTI_LEDGER_ATTRIBUTION_TEXT_CHARS,
                "selected_attribution_text_accounting": (
                    "unique requested, actual, configured model and effort strings "
                    "are interned and budgeted once; identity, lineage, provider, "
                    "timestamp and status strings use fixed per-field bounds and "
                    "selected-row limits"
                ),
                "sqlite_vm_instructions": MAX_VM_INSTRUCTIONS,
                "elapsed_seconds": MAX_SECONDS,
                "lineage_nodes_per_ledger": MAX_LINEAGE_NODES,
            },
        },
        "summary": merged["summary"],
        "by_actual_model": merged["by_actual_model"],
        "root_vs_descendants_by_actual_model": merged[
            "root_vs_descendants_by_actual_model"
        ],
        "scope_totals": merged["scope_totals"],
        "deduplication": {
            "source_selection_complete": source_selection_complete,
            **{
                key: merged[key]
                for key in (
                    "selected_provider_call_rows",
                    "deduplicated_provider_call_count",
                    "identical_mirrored_rows_collapsed",
                    "conflicting_call_id_count",
                    "rows_excluded_for_conflicting_call_ids",
                    "rows_excluded_for_missing_call_id",
                    "configured_attribution_conflict_provider_call_count",
                    "physical_call_deduplication",
                )
            }
        },
        "selected_window_provider_call_bounds_utc": merged[
            "selected_window_provider_call_bounds_utc"
        ],
    }
    check_deadline()
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args: argparse.Namespace | None = None
    try:
        args = _args(argv)
        output_deadline_check = None
        with decimal.localcontext(DECIMAL_CONTEXT):
            if args.historical_database or (args.all_time and args.scope == "subtree"):
                deadline = time.monotonic() + MAX_SECONDS
                output_deadline_check = _deadline_checker(deadline)
                report = _run_multi_ledger_report(
                    args,
                    deadline=deadline,
                    check_deadline=output_deadline_check,
                )
                status = report["status"]
            else:
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
                    all_time=args.all_time,
                )
                report = reporter.run()
                reporter.check_deadline()
                output_deadline_check = reporter.check_deadline
                status = report["status"]
        _write_bounded_json(report, sys.stdout, output_deadline_check)
        return 0 if status == "complete" else 2
    except WorkLimitReached as exc:
        _write_error_json(exc, args)
        return 2
    except ReportProblem as exc:
        _write_error_json(exc, args)
        return 3


if __name__ == "__main__":
    raise SystemExit(main())
