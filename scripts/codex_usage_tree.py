#!/usr/bin/env python3
"""Query a Codex tree through app-server, an exact local ledger, or named rollouts."""

from __future__ import annotations

import argparse
import hashlib
import json
import queue
import sqlite3
import subprocess
import threading
import time
from collections import Counter
from datetime import datetime
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any

PAGE_SIZE = 100
DEFAULT_MAX_THREADS_PER_STATE = 100
MAX_THREADS_PER_STATE = 500
MAX_LOCAL_ROLLOUT_BYTES = 10 * 1024 * 1024
MAX_DATABASE_THREADS = 900
MAX_DATABASE_CALLS = 10_000
MAX_DATABASE_UNION_CALLS_PER_SOURCE = 200_000
MAX_DATABASE_UNION_CALLS_TOTAL = 300_000
MAX_DATABASE_UNION_MEMORY_BYTES = 192 * 1024 * 1024
MAX_DATABASE_UNION_GROUPS = 100
MAX_DATABASE_SOURCE_SECONDS = 12.0
MAX_DATABASE_UNION_SECONDS = 24.0
MAX_DATABASE_QUERY_SECONDS = 4.0
UNION_CALL_FIELDS = (
    "role",
    "schemaMissingFields",
    "threadId",
    "turnId",
    "spawnRequestId",
    "toolCallId",
    "requestId",
    "provider",
    "requestedModel",
    "providerEffectiveModel",
    "providerEffectiveModelSource",
    "requestedServiceTier",
    "actualServiceTier",
    "actualServiceTierSource",
    "fastModeRequested",
    "fastModeUsed",
    "billingSurface",
    "accountPlan",
    "providerReportedCost",
    "providerReportedCurrency",
    "providerReportedCredits",
    "startedAt",
    "completedAt",
    "inputTokensUncached",
    "inputTokensCached",
    "cacheWriteInputTokens",
    "outputTokens",
    "totalTokens",
    "status",
)
UNION_FIELD_INDEX = {field: index for index, field in enumerate(UNION_CALL_FIELDS)}
UNION_CONTRIBUTION_FIELDS = (
    "role",
    "requestedModel",
    "providerEffectiveModel",
    "providerEffectiveModelSource",
    "providerReportedCredits",
    "inputTokensUncached",
    "inputTokensCached",
    "cacheWriteInputTokens",
    "outputTokens",
    "totalTokens",
    "status",
)
DATABASE_CALL_COLUMNS = (
    "turn_id",
    "spawn_request_id",
    "tool_call_id",
    "request_id",
    "provider",
    "requested_model",
    "actual_model_used",
    "final_model",
    "requested_service_tier",
    "actual_service_tier",
    "actual_service_tier_source",
    "fast_mode_requested",
    "fast_mode_used",
    "billing_surface",
    "account_plan",
    "provider_reported_cost",
    "provider_reported_currency",
    "provider_reported_credits",
    "started_at",
    "completed_at",
    "input_tokens_uncached",
    "input_tokens_cached",
    "input_tokens_cache_write",
    "output_tokens",
    "total_tokens",
    "status",
)
UNION_REQUIRED_COLUMNS = DATABASE_CALL_COLUMNS
TOKEN_FIELDS = {
    "uncachedInput": ("netNewInputTokens", "input"),
    "cachedInput": ("cachedInputTokens", "cached_input"),
    "output": ("outputTokens", "output"),
}


def app_server_argv(executable: str) -> list[str]:
    """Return the supported normal CLI invocation for the app-server stdio transport."""
    return [executable, "app-server", "--listen", "stdio://"]


def rpc_request_document(
    request_id: int, method: str, params: dict[str, Any]
) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}


class UsageError(Exception):
    pass


class RpcError(UsageError):
    def __init__(self, method: str, code: Any):
        self.method, self.code = method, code
        super().__init__(f"{method} failed (RPC code {code!r})")


class AppServer:
    """One bounded JSON-RPC stdio client; app-server logs are not copied to output."""

    def __init__(self, executable: str, timeout: int, overall_timeout: int):
        self.timeout = timeout
        self.deadline = time.monotonic() + overall_timeout
        self.next_id = 0
        self.messages: queue.Queue[Any] = queue.Queue()
        try:
            self.process = subprocess.Popen(
                app_server_argv(executable),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                encoding="utf-8",
                bufsize=1,
            )
        except OSError as exc:
            raise UsageError("could not start the Codex app-server executable") from exc
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self) -> None:
        assert self.process.stdout is not None
        try:
            for line in self.process.stdout:
                try:
                    self.messages.put(json.loads(line))
                except json.JSONDecodeError:
                    self.messages.put(UsageError("app-server returned invalid JSON"))
        finally:
            self.messages.put(None)

    def notify(self, method: str) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(
            json.dumps({"jsonrpc": "2.0", "method": method, "params": {}}) + "\n"
        )
        self.process.stdin.flush()

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self.next_id += 1
        request_id = self.next_id
        assert self.process.stdin is not None
        self.process.stdin.write(
            json.dumps(rpc_request_document(request_id, method, params)) + "\n"
        )
        self.process.stdin.flush()
        while True:
            remaining = min(self.timeout, self.deadline - time.monotonic())
            if remaining <= 0:
                raise UsageError("overall usage query timed out")
            try:
                message = self.messages.get(timeout=remaining)
            except queue.Empty as exc:
                if time.monotonic() >= self.deadline:
                    raise UsageError("overall usage query timed out") from exc
                raise UsageError(f"timed out waiting for {method}") from exc
            if message is None:
                raise UsageError("app-server closed its output")
            if isinstance(message, Exception):
                raise message
            if not isinstance(message, dict):
                raise UsageError("app-server returned a non-object JSON message")
            if message.get("id") != request_id:
                continue
            if "error" in message:
                error = message["error"]
                raise RpcError(
                    method, error.get("code") if isinstance(error, dict) else None
                )
            result = message.get("result")
            if not isinstance(result, dict):
                raise UsageError(f"{method} returned a non-object result")
            return result

    def initialize(self) -> None:
        self.request(
            "initialize",
            {
                "clientInfo": {"name": "codex-usage-tree", "version": "1"},
                "capabilities": {"experimentalApi": True},
            },
        )
        self.notify("initialized")

    def close(self) -> None:
        if self.process.stdin:
            try:
                self.process.stdin.close()
            except OSError:
                pass
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()


def _default_rates() -> dict[str, Any]:
    return {
        "source": "operator-provided authoritative credit table",
        "date": "2026-09-30",
        "credits_per_million_tokens": {
            "gpt-6.1-sol": {"input": 50, "cached_input": 2.5, "output": 250},
            "gpt-6-sol": {"input": 50, "cached_input": 5, "output": 250},
            "gpt-6-luna": {"input": 2.5, "cached_input": 0.25, "output": 12.5},
            "gpt-6-astra": {"input": 250, "cached_input": 25, "output": 1250},
        },
    }


def load_rates(path: str | None) -> dict[str, Any]:
    try:
        raw = _default_rates() if path is None else json.loads(Path(path).read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise UsageError("could not read or parse the rate map JSON") from exc
    if not isinstance(raw, dict) or not isinstance(
        raw.get("credits_per_million_tokens"), dict
    ):
        raise UsageError(
            "rate map requires credits_per_million_tokens by exact model name"
        )
    if not all(
        isinstance(raw.get(key), str) and raw[key].strip() for key in ("source", "date")
    ):
        raise UsageError("rate map requires non-empty source and date")
    models = {}
    try:
        for model, entries in raw["credits_per_million_tokens"].items():
            if not isinstance(model, str) or not isinstance(entries, dict):
                raise UsageError(
                    "each rate entry must map an exact model name to token classes"
                )
            models[model] = {}
            for dimension in ("input", "cached_input", "output"):
                if dimension in entries:
                    value = Decimal(str(entries[dimension]))
                    if not value.is_finite() or value < 0:
                        raise UsageError(
                            "rates must be finite, non-negative credits per million"
                        )
                    models[model][dimension] = value
    except (InvalidOperation, TypeError) as exc:
        raise UsageError(
            "rates must be finite, non-negative credits per million"
        ) from exc
    return {"source": raw["source"], "date": raw["date"], "models": models}


def load_ledger_standard_rates(database_path: str, at: str) -> dict[str, Any]:
    """Select an explicit standard-rate scenario, not a provider debit."""
    connection = None
    try:
        instant = datetime.fromisoformat(at.replace("Z", "+00:00"))
        if instant.tzinfo is None or instant.utcoffset() is None:
            raise UsageError("ledger rate scenario needs a timestamp with a timezone")
        uri = Path(database_path).resolve(strict=True).as_uri() + "?mode=ro"
        connection = sqlite3.connect(uri, uri=True, timeout=5.0)
        connection.execute("PRAGMA query_only = ON")
        if connection.execute("SELECT julianday(?)", (at,)).fetchone()[0] is None:
            raise UsageError("ledger rate scenario needs a valid SQLite date/time")
        rows = connection.execute(
            """SELECT model, credits_per_1m_uncached_input,
                      credits_per_1m_cached_input, credits_per_1m_output
               FROM usage_codex_credit_rates
               WHERE provider = 'openai' AND service_tier = 'default'
                 AND speed_mode = 'standard' AND rate_card_kind = 'codex_token_based'
                 AND julianday(effective_from) <= julianday(?)
                 AND (effective_to IS NULL OR julianday(effective_to) > julianday(?))
               LIMIT 1001""",
            (at, at),
        ).fetchall()
        if len(rows) > 1000:
            raise UsageError("ledger rate scenario exceeds the 1,000-row bound")
        candidates: dict[str, set[tuple[Decimal, Decimal, Decimal]]] = {}
        for model, uncached, cached, output in rows:
            if not isinstance(model, str) or not model.strip():
                continue
            values = tuple(Decimal(str(value)) for value in (uncached, cached, output))
            if any(not value.is_finite() or value < 0 for value in values):
                raise UsageError("ledger scenario contains an invalid rate")
            candidates.setdefault(model, set()).add(values)
        models = {}
        for model, choices in candidates.items():
            if len(choices) != 1:
                continue  # Never choose between conflicting overlapping rates.
            uncached, cached, output = next(iter(choices))
            models[model] = {
                "input": uncached,
                "cached_input": cached,
                "output": output,
            }
        return {
            "source": (
                "local ledger standard-rate scenario; effective call tier, speed and "
                "debit are not established; historical calls may use different rates"
            ),
            "date": at,
            "models": models,
        }
    except (OSError, sqlite3.Error, InvalidOperation, TypeError, ValueError) as exc:
        raise UsageError("could not read the requested ledger rate scenario") from exc
    finally:
        if connection is not None:
            connection.close()


def _optional_int(value: Any, field: str) -> int | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise UsageError(f"usage field {field} was not a non-negative integer or null")
    return value


def _optional_text(value: Any, field: str) -> str | None:
    if value is not None and not isinstance(value, str):
        raise UsageError(f"usage field {field} was not text or null")
    return value


def _optional_decimal(value: Any, field: str) -> Decimal | None:
    if value is None:
        return None
    try:
        result = Decimal(str(value))
    except InvalidOperation as exc:
        raise UsageError(
            f"usage field {field} was not a finite decimal or null"
        ) from exc
    if not result.is_finite():
        raise UsageError(f"usage field {field} was not a finite decimal or null")
    return result


def _normalize(response: dict[str, Any], thread_id: str) -> dict[str, Any] | None:
    usage = response.get("threadUsage")
    if usage is None:
        return None
    if not isinstance(usage, dict) or usage.get("threadId") != thread_id:
        raise UsageError(
            "account/usage/read returned malformed or mismatched thread usage"
        )
    groups = usage.get("groups")
    if not isinstance(groups, list):
        raise UsageError("account/usage/read returned malformed groups")
    normalized = []
    for group in groups:
        if not isinstance(group, dict):
            raise UsageError("account/usage/read returned a malformed group")
        normalized.append(
            {
                "model": _optional_text(group.get("model"), "model"),
                "reasoningEffort": _optional_text(
                    group.get("reasoningEffort"), "reasoningEffort"
                ),
                "speed": _optional_text(group.get("speed"), "speed"),
                "netNewInputTokens": _optional_int(
                    group.get("netNewInputTokens"), "netNewInputTokens"
                ),
                "cachedInputTokens": _optional_int(
                    group.get("cachedInputTokens"), "cachedInputTokens"
                ),
                "outputTokens": _optional_int(
                    group.get("outputTokens"), "outputTokens"
                ),
                "estimatedUsageCreditsMicros": _optional_int(
                    group.get("estimatedUsageCreditsMicros"),
                    "estimatedUsageCreditsMicros",
                ),
            }
        )
    return {"groups": normalized}


def _decimal_text(value: Decimal) -> str:
    normalized = value.normalize()
    return format(normalized, "f") if normalized else "0"


def _cost(group: dict[str, Any], rates: dict[str, Any]) -> dict[str, Any]:
    known = Decimal(0)
    missing_tokens, missing_rates = [], []
    model_rates = rates["models"].get(group["model"], {}) if group["model"] else {}
    for public_name, (field, rate_name) in TOKEN_FIELDS.items():
        tokens = group[field]
        if tokens is None:
            missing_tokens.append(public_name)
        elif tokens:
            rate = model_rates.get(rate_name)
            if rate is None:
                missing_rates.append(
                    f"{group['model'] or '<unknown model>'}:{rate_name}"
                )
            else:
                known += Decimal(tokens) * rate / Decimal(1_000_000)
    missing_tokens.extend(group.get("unpricedTokenDimensions", []))
    complete = not missing_tokens and not missing_rates
    return {
        "credits": _decimal_text(known) if complete else None,
        "knownSubtotal": _decimal_text(known),
        "complete": complete,
        "missingTokenDimensions": missing_tokens,
        "missingRates": missing_rates,
    }


def _key(group: dict[str, Any]) -> tuple[Any, Any, Any]:
    return (group["model"], group["reasoningEffort"], group["speed"])


def aggregate(rows: list[dict[str, Any]], rates: dict[str, Any]) -> dict[str, Any]:
    missing_ids = [row["threadId"] for row in rows if row["usage"] is None]
    grouped: dict[tuple[Any, Any, Any], list[dict[str, Any]]] = {}
    for row in rows:
        if row["usage"] is not None:
            for group in row["usage"]["groups"]:
                grouped.setdefault(_key(group), []).append(group)

    totals = []
    for key, groups in sorted(
        grouped.items(), key=lambda item: tuple(x or "" for x in item[0])
    ):
        tokens = {}
        for name, (field, _) in TOKEN_FIELDS.items():
            values = [group[field] for group in groups]
            subtotal = sum(value for value in values if value is not None)
            missing = sum(value is None for value in values) + len(missing_ids)
            tokens[name] = {
                "value": subtotal if not missing else None,
                "knownSubtotal": subtotal,
                "complete": not missing,
                "missingValues": missing,
            }
        if any("cacheWriteInputTokens" in group for group in groups):
            cache_write_values = [
                group.get("cacheWriteInputTokens") for group in groups
            ]
            cache_write_subtotal = sum(
                value for value in cache_write_values if value is not None
            )
            cache_write_missing = sum(
                value is None for value in cache_write_values
            ) + len(missing_ids)
            tokens["cacheWriteInput"] = {
                "value": cache_write_subtotal if not cache_write_missing else None,
                "knownSubtotal": cache_write_subtotal,
                "complete": not cache_write_missing,
                "missingValues": cache_write_missing,
            }
        cost_parts = [_cost(group, rates) for group in groups]
        known_credits = sum(
            (Decimal(part["knownSubtotal"]) for part in cost_parts), start=Decimal(0)
        )
        estimated = [group["estimatedUsageCreditsMicros"] for group in groups]
        missing_estimated = sum(value is None for value in estimated) + len(missing_ids)
        cost_complete = not missing_ids and all(part["complete"] for part in cost_parts)
        known_estimated = sum(value for value in estimated if value is not None)
        totals.append(
            {
                "model": key[0],
                "reasoningEffort": key[1],
                "speed": key[2],
                "tokenTotals": tokens,
                "estimatedUsageCreditsMicros": {
                    "value": known_estimated if not missing_estimated else None,
                    "knownSubtotal": known_estimated,
                    "complete": not missing_estimated,
                    "missingValues": missing_estimated,
                },
                "calculatedCredits": {
                    "credits": _decimal_text(known_credits) if cost_complete else None,
                    "knownSubtotal": _decimal_text(known_credits),
                    "complete": cost_complete,
                    "missingUsageThreadIds": missing_ids,
                    "missingTokenDimensions": sorted(
                        {
                            item
                            for part in cost_parts
                            for item in part["missingTokenDimensions"]
                        }
                    ),
                    "missingRates": sorted(
                        {item for part in cost_parts for item in part["missingRates"]}
                    ),
                },
            }
        )
    return {
        "threadIds": [row["threadId"] for row in rows],
        "missingUsageThreadIds": missing_ids,
        "groups": totals,
    }


def _read_local_rollout(path: str) -> dict[str, Any]:
    """Read only native usage/context metadata from one explicitly named rollout."""
    rollout_path = Path(path)
    try:
        if rollout_path.stat().st_size > MAX_LOCAL_ROLLOUT_BYTES:
            raise UsageError("local rollout exceeds the 10 MiB read limit")
        handle = rollout_path.open("rb")
    except UsageError:
        raise
    except OSError as exc:
        raise UsageError("could not open an explicitly supplied local rollout") from exc

    session_meta: dict[str, Any] | None = None
    contexts: dict[str, list[dict[str, Any]]] = {}
    records: list[dict[str, Any]] = []
    response_ids: set[str] = set()
    bytes_read = 0
    try:
        with handle:
            while True:
                line = handle.readline(MAX_LOCAL_ROLLOUT_BYTES - bytes_read + 1)
                if not line:
                    break
                bytes_read += len(line)
                if bytes_read > MAX_LOCAL_ROLLOUT_BYTES:
                    raise UsageError("local rollout exceeds the 10 MiB read limit")
                try:
                    item = json.loads(line.decode("utf-8"))
                except (json.JSONDecodeError, UnicodeDecodeError) as exc:
                    raise UsageError("local rollout contains malformed JSON") from exc
                if not isinstance(item, dict):
                    continue
                item_type = item.get("type")
                if item_type not in {
                    "session_meta",
                    "turn_context",
                    "token_usage_record",
                }:
                    continue
                payload = item.get("payload")
                if not isinstance(payload, dict):
                    raise UsageError("local rollout usage metadata is malformed")
                if item_type == "session_meta":
                    if session_meta is not None:
                        raise UsageError("local rollout has duplicate session metadata")
                    thread_id = payload.get("id")
                    parent_thread_id = payload.get("parent_thread_id")
                    if not isinstance(thread_id, str) or not thread_id:
                        raise UsageError("local rollout has no thread identity")
                    if parent_thread_id is not None and not isinstance(
                        parent_thread_id, str
                    ):
                        raise UsageError("local rollout has malformed parent identity")
                    session_meta = {
                        "threadId": thread_id,
                        "parentThreadId": parent_thread_id,
                    }
                elif item_type == "turn_context":
                    turn_id = payload.get("turn_id")
                    if not isinstance(turn_id, str) or not turn_id:
                        raise UsageError("local rollout has malformed turn context")
                    model = _optional_text(payload.get("model"), "turn_context.model")
                    effort = _optional_text(
                        payload.get("effort"), "turn_context.effort"
                    )
                    contexts.setdefault(turn_id, []).append(
                        {"model": model, "reasoningEffort": effort}
                    )
                else:
                    thread_id = payload.get("thread_id")
                    turn_id = payload.get("turn_id")
                    root_turn_id = payload.get("root_turn_id")
                    response_id = payload.get("response_id")
                    usage = payload.get("usage")
                    if (
                        not isinstance(thread_id, str)
                        or not isinstance(turn_id, str)
                        or not isinstance(root_turn_id, str)
                        or not isinstance(response_id, str)
                        or not isinstance(usage, dict)
                    ):
                        raise UsageError("local rollout has malformed response usage")
                    if response_id in response_ids:
                        raise UsageError(
                            "local rollout repeats a response usage record"
                        )
                    response_ids.add(response_id)
                    records.append(
                        {
                            "threadId": thread_id,
                            "turnId": turn_id,
                            "rootTurnId": root_turn_id,
                            "usage": {
                                **{
                                    name: _optional_int(usage.get(field), field)
                                    for name, field in (
                                        ("input", "input_tokens"),
                                        ("cachedInput", "cached_input_tokens"),
                                        ("output", "output_tokens"),
                                    )
                                },
                                "cacheWriteInput": _optional_int(
                                    usage.get("cache_write_input_tokens", 0),
                                    "cache_write_input_tokens",
                                ),
                            },
                        }
                    )
    except OSError as exc:
        raise UsageError("could not read an explicitly supplied local rollout") from exc

    if session_meta is None:
        raise UsageError("local rollout has no session metadata")
    if any(record["threadId"] != session_meta["threadId"] for record in records):
        raise UsageError("local rollout usage does not match its thread identity")
    return {**session_meta, "contexts": contexts, "records": records}


def _local_groups(rollout: dict[str, Any]) -> tuple[list[dict[str, Any]], bool]:
    grouped: dict[tuple[Any, Any], list[dict[str, Any]]] = {}
    model_context_complete = True
    for record in rollout["records"]:
        contexts = rollout["contexts"].get(record["turnId"], [])
        context_pairs = {
            (context["model"], context["reasoningEffort"]) for context in contexts
        }
        model, effort = (
            next(iter(context_pairs)) if len(context_pairs) == 1 else (None, None)
        )
        if model is None:
            model_context_complete = False
        values = record["usage"]
        input_tokens, cached_tokens = values["input"], values["cachedInput"]
        if (
            input_tokens is not None
            and cached_tokens is not None
            and cached_tokens > input_tokens
        ):
            raise UsageError("local rollout cached input exceeds total input")
        cache_write_tokens = values["cacheWriteInput"]
        uncached_tokens = (
            None
            if input_tokens is None or cached_tokens is None or cache_write_tokens != 0
            else input_tokens - cached_tokens
        )
        key = (model, effort)
        grouped.setdefault(key, []).append(
            {
                "netNewInputTokens": uncached_tokens,
                "cachedInputTokens": cached_tokens,
                "outputTokens": values["output"],
                "cacheWriteInputTokens": cache_write_tokens,
                "unpricedTokenDimensions": (
                    ["cacheWriteInputTokens"] if cache_write_tokens != 0 else []
                ),
            }
        )

    groups = []
    for (model, effort), items in sorted(
        grouped.items(), key=lambda item: tuple(value or "" for value in item[0])
    ):

        def total(field: str) -> int | None:
            values = [item[field] for item in items]
            return (
                sum(value for value in values if value is not None)
                if all(value is not None for value in values)
                else None
            )

        groups.append(
            {
                "model": model,
                "reasoningEffort": effort,
                "speed": None,
                "netNewInputTokens": total("netNewInputTokens"),
                "cachedInputTokens": total("cachedInputTokens"),
                "outputTokens": total("outputTokens"),
                "estimatedUsageCreditsMicros": None,
                "cacheWriteInputTokens": total("cacheWriteInputTokens"),
                "unpricedTokenDimensions": sorted(
                    {
                        dimension
                        for item in items
                        for dimension in item["unpricedTokenDimensions"]
                    }
                ),
            }
        )
    return groups, model_context_complete


def collect_local_tree(
    root_id: str,
    rollout_paths: list[str],
    expected_child_ids: list[str],
    rates: dict[str, Any],
) -> dict[str, Any]:
    """Aggregate explicit local rollout files without starting app-server or scanning state."""
    if not isinstance(root_id, str) or not root_id.strip():
        raise UsageError("root thread ID must be non-empty")
    if not rollout_paths:
        raise UsageError("local rollout mode requires at least one --rollout-file")
    if len(rollout_paths) > MAX_THREADS_PER_STATE * 2 + 1:
        raise UsageError("too many explicit local rollout files")
    if any(
        not isinstance(thread_id, str) or not thread_id.strip()
        for thread_id in expected_child_ids
    ):
        raise UsageError("expected child thread IDs must be non-empty")
    if (
        len(set(expected_child_ids)) != len(expected_child_ids)
        or root_id in expected_child_ids
    ):
        raise UsageError(
            "expected local thread IDs must be distinct descendants, excluding root"
        )

    rollouts = [_read_local_rollout(path) for path in rollout_paths]
    by_id = {row["threadId"]: row for row in rollouts}
    if len(by_id) != len(rollouts):
        raise UsageError("local rollout files repeat a thread identity")
    if root_id not in by_id:
        raise UsageError("local rollout files do not include the root thread")
    root_turn_ids = set(by_id[root_id]["contexts"])
    for rollout in rollouts:
        if any(
            record["rootTurnId"] not in root_turn_ids for record in rollout["records"]
        ):
            raise UsageError(
                "usage references a turn outside the supplied root rollout"
            )
    unexpected = set(by_id) - {root_id, *expected_child_ids}
    if unexpected:
        raise UsageError("local rollout files include an unexpected thread")

    expected_ids = {root_id, *expected_child_ids} if expected_child_ids else set(by_id)
    missing_files = expected_ids - set(by_id)
    rows = []
    model_context_complete = not missing_files
    for thread_id in sorted(expected_ids, key=lambda value: (value != root_id, value)):
        rollout = by_id.get(thread_id)
        if rollout is None:
            rows.append(
                {
                    "threadId": thread_id,
                    "parentThreadId": None,
                    "role": "root" if thread_id == root_id else "child",
                    "usageStatus": "rollout_not_supplied",
                    "usage": None,
                }
            )
            continue
        groups, has_model_context = _local_groups(rollout)
        model_context_complete = model_context_complete and has_model_context
        rows.append(
            {
                "threadId": thread_id,
                "parentThreadId": rollout["parentThreadId"],
                "role": "root" if thread_id == root_id else "child",
                "usageStatus": (
                    "observed_local_records"
                    if rollout["records"]
                    else "no_usage_records"
                ),
                "usage": {"groups": groups} if groups else None,
                "responseUsageRecordCount": len(rollout["records"]),
                "modelSource": "persisted_turn_context",
            }
        )

    parents = {row["threadId"]: row["parentThreadId"] for row in rows}
    lineage_complete = bool(expected_child_ids) and not missing_files
    for row in rows:
        if row["threadId"] == root_id:
            lineage_complete = lineage_complete and row["parentThreadId"] is None
            continue
        parent = parents.get(row["threadId"])
        seen: set[str] = set()
        while (
            parent is not None
            and parent != root_id
            and parent in parents
            and parent not in seen
        ):
            seen.add(parent)
            parent = parents[parent]
        lineage_complete = lineage_complete and parent == root_id

    root_rows = [rows[0]]
    child_rows = rows[1:]
    root_total = aggregate(root_rows, rates)
    child_total = aggregate(child_rows, rates)
    tree_total = aggregate(rows, rates)
    pricing_complete = any(
        summary["groups"] for summary in (root_total, child_total, tree_total)
    ) and all(
        group["calculatedCredits"]["complete"]
        for summary in (root_total, child_total, tree_total)
        for group in summary["groups"]
    )
    return {
        "schema": "codex-usage-tree/v1",
        "status": "partial",
        "scope": "root_and_spawned_descendants",
        "rootThreadId": root_id,
        "lineage": {
            "source": "local_rollout_session_meta",
            "expectedThreadIdsSource": (
                "caller-supplied IDs from a previously validated complete thread/list"
                if expected_child_ids
                else None
            ),
            "expectedThreadIds": sorted(expected_ids),
            "suppliedThreadIds": sorted(by_id),
            "missingRolloutFiles": sorted(missing_files),
            "complete": lineage_complete,
        },
        "usage": {
            "source": "local TokenUsageRecord rollout items",
            "availableThreadCount": sum(row["usage"] is not None for row in rows),
            "requestedThreadCount": len(rows),
            "responseUsageRecordCount": sum(
                row.get("responseUsageRecordCount", 0) for row in rows
            ),
            "bestEffort": True,
            "coverageNote": (
                "sums observed usage records only; this source has no independent "
                "response denominator"
            ),
            "nativeEstimatedCreditsAvailable": False,
        },
        "rateMap": {
            "source": rates["source"],
            "date": rates["date"],
            "unit": "credits per 1,000,000 exact-model tokens",
            "creditsPerMillionTokens": {
                model: {name: _decimal_text(rate) for name, rate in entries.items()}
                for model, entries in sorted(rates["models"].items())
            },
        },
        "threads": rows,
        "totals": {"root": root_total, "children": child_total, "tree": tree_total},
        "completeness": {
            "lineageComplete": lineage_complete,
            "usageComplete": False,
            "modelContextComplete": model_context_complete,
            "pricingComplete": pricing_complete,
        },
    }


def _database_total(
    values: list[int | None], *, coverage_truncated: bool
) -> dict[str, Any]:
    known = sum(value for value in values if value is not None)
    missing = sum(value is None for value in values)
    complete = missing == 0 and not coverage_truncated
    return {
        "value": known if complete else None,
        "knownSubtotal": known,
        "completeForReturnedRows": complete,
        "missingValues": missing,
    }


def _database_call_cost(
    call: dict[str, Any], rates: dict[str, Any]
) -> tuple[str | None, str, dict[str, Any]]:
    effective_model = call["providerEffectiveModel"]
    requested_model = call["requestedModel"]
    rate_model = effective_model or requested_model
    estimate_basis = (
        "provider_effective_model"
        if effective_model
        else "requested_model_fallback"
        if requested_model
        else "unknown_model"
    )
    cache_write = call["cacheWriteInputTokens"]
    cost = _cost(
        {
            "model": rate_model,
            "netNewInputTokens": (
                call["inputTokensUncached"] if cache_write == 0 else None
            ),
            "cachedInputTokens": call["inputTokensCached"],
            "outputTokens": call["outputTokens"],
            "unpricedTokenDimensions": (
                [] if cache_write == 0 else ["cacheWriteInputTokens"]
            ),
        },
        rates,
    )
    return rate_model, estimate_basis, cost


def _database_model_groups(
    calls: list[dict[str, Any]],
    rates: dict[str, Any],
    *,
    coverage_truncated: bool,
) -> list[dict[str, Any]]:
    grouped: dict[tuple[str | None, str | None], list[dict[str, Any]]] = {}
    for call in calls:
        key = (call["requestedModel"], call["providerEffectiveModel"])
        grouped.setdefault(key, []).append(call)

    groups = []
    for (requested_model, effective_model), items in sorted(
        grouped.items(), key=lambda item: tuple(value or "" for value in item[0])
    ):
        status_counts: dict[str, int] = {}
        for item in items:
            status = item["status"] or "unknown"
            status_counts[status] = status_counts.get(status, 0) + 1

        token_totals = {
            field: _database_total(
                [item[field] for item in items],
                coverage_truncated=coverage_truncated,
            )
            for field in (
                "inputTokensUncached",
                "inputTokensCached",
                "cacheWriteInputTokens",
                "outputTokens",
                "totalTokens",
            )
        }
        rate_model = effective_model or requested_model
        estimate_basis = (
            "provider_effective_model"
            if effective_model
            else "requested_model_fallback"
            if requested_model
            else "unknown_model"
        )
        cost_parts = []
        reported_credit_values: list[Decimal | None] = []
        for item in items:
            cost_parts.append(_database_call_cost(item, rates)[2])
            reported_credit_values.append(item["providerReportedCredits"])

        estimate_subtotal = sum(
            (Decimal(part["knownSubtotal"]) for part in cost_parts),
            start=Decimal(0),
        )
        estimate_complete = not coverage_truncated and all(
            part["complete"] for part in cost_parts
        )
        reported_subtotal = sum(
            (value for value in reported_credit_values if value is not None),
            start=Decimal(0),
        )
        reported_complete = not coverage_truncated and all(
            value is not None for value in reported_credit_values
        )
        groups.append(
            {
                "requestedModel": requested_model,
                "providerEffectiveModel": effective_model,
                "providerEffectiveModelSource": next(
                    (
                        item["providerEffectiveModelSource"]
                        for item in items
                        if item["providerEffectiveModelSource"] is not None
                    ),
                    None,
                ),
                "responseUsageRecordCount": len(items),
                "statusCounts": dict(sorted(status_counts.items())),
                "tokenTotals": token_totals,
                "calculatedCreditEstimate": {
                    "value": _decimal_text(estimate_subtotal)
                    if estimate_complete
                    else None,
                    "knownSubtotal": _decimal_text(estimate_subtotal),
                    "completeForReturnedRows": estimate_complete,
                    "rateModel": rate_model,
                    "rateModelBasis": estimate_basis,
                    "missingTokenDimensions": sorted(
                        {
                            name
                            for part in cost_parts
                            for name in part["missingTokenDimensions"]
                        }
                    ),
                    "missingRates": sorted(
                        {name for part in cost_parts for name in part["missingRates"]}
                    ),
                },
                "providerReportedCredits": {
                    "value": _decimal_text(reported_subtotal)
                    if reported_complete
                    else None,
                    "knownSubtotal": _decimal_text(reported_subtotal),
                    "completeForReturnedRows": reported_complete,
                    "missingValues": sum(
                        value is None for value in reported_credit_values
                    ),
                    "note": (
                        "reported ledger field; not independently verified "
                        "as an account debit"
                    ),
                },
                "actualDebit": {
                    "value": None,
                    "status": "not_observed",
                },
            }
        )
    return groups


def _database_deadline(overall_deadline: float | None = None) -> float:
    local_deadline = time.monotonic() + MAX_DATABASE_QUERY_SECONDS
    return min(local_deadline, overall_deadline) if overall_deadline else local_deadline


def _assert_indexed_thread_call_lookup(
    connection: sqlite3.Connection,
    thread_ids: list[str],
    *,
    sql: str | None = None,
    parameters: tuple[Any, ...] | list[str] | None = None,
) -> None:
    if sql is None:
        placeholders = ", ".join("?" for _ in thread_ids)
        sql = (
            "SELECT provider_call_id FROM usage_provider_calls "
            f"WHERE thread_id IN ({placeholders}) LIMIT 1"
        )
        parameters = thread_ids
    plan = connection.execute("EXPLAIN QUERY PLAN " + sql, parameters or ()).fetchall()
    details = [str(row[3]).upper() for row in plan]
    if not any(
        "SEARCH" in detail and "INDEX" in detail and "THREAD_ID" in detail
        for detail in details
    ):
        raise UsageError(
            "usage database has no indexed exact-thread provider-call lookup"
        )


def _normalize_database_call(
    row: sqlite3.Row,
    thread_roles: dict[str, str],
    call_columns: set[str],
) -> dict[str, Any]:
    thread_id = _optional_text(row["thread_id"], "thread_id")
    if not thread_id:
        raise UsageError("usage database contains a provider call without a thread ID")
    requested_model = _optional_text(row["requested_model"], "requested_model") or None
    actual_model = _optional_text(row["actual_model_used"], "actual_model_used") or None
    final_model = _optional_text(row["final_model"], "final_model") or None
    effective_model = final_model or actual_model
    effective_model_source = (
        "final_model" if final_model else "actual_model_used" if actual_model else None
    )
    reported_cost = _optional_decimal(
        row["provider_reported_cost"], "provider_reported_cost"
    )
    reported_credits = _optional_decimal(
        row["provider_reported_credits"], "provider_reported_credits"
    )
    return {
        "role": thread_roles.get(thread_id, "other_descendant"),
        "schemaMissingFields": tuple(
            field for field in UNION_REQUIRED_COLUMNS if field not in call_columns
        ),
        "threadId": thread_id,
        "providerCallId": _optional_text(row["provider_call_id"], "provider_call_id"),
        "turnId": _optional_text(row["turn_id"], "turn_id"),
        "spawnRequestId": _optional_text(row["spawn_request_id"], "spawn_request_id"),
        "toolCallId": _optional_text(row["tool_call_id"], "tool_call_id"),
        "requestId": _optional_text(row["request_id"], "request_id"),
        "provider": _optional_text(row["provider"], "provider"),
        "requestedModel": requested_model,
        "providerEffectiveModel": effective_model,
        "providerEffectiveModelSource": effective_model_source,
        "requestedServiceTier": _optional_text(
            row["requested_service_tier"], "requested_service_tier"
        ),
        "actualServiceTier": _optional_text(
            row["actual_service_tier"], "actual_service_tier"
        ),
        "actualServiceTierSource": _optional_text(
            row["actual_service_tier_source"], "actual_service_tier_source"
        ),
        "fastModeRequested": _optional_int(
            row["fast_mode_requested"], "fast_mode_requested"
        ),
        "fastModeUsed": _optional_int(row["fast_mode_used"], "fast_mode_used"),
        "billingSurface": _optional_text(row["billing_surface"], "billing_surface"),
        "accountPlan": _optional_text(row["account_plan"], "account_plan"),
        "providerReportedCost": reported_cost,
        "providerReportedCurrency": _optional_text(
            row["provider_reported_currency"], "provider_reported_currency"
        ),
        "providerReportedCredits": reported_credits,
        "startedAt": _optional_text(row["started_at"], "started_at"),
        "completedAt": _optional_text(row["completed_at"], "completed_at"),
        "inputTokensUncached": _optional_int(
            row["input_tokens_uncached"], "input_tokens_uncached"
        ),
        "inputTokensCached": _optional_int(
            row["input_tokens_cached"], "input_tokens_cached"
        ),
        "cacheWriteInputTokens": _optional_int(
            row["input_tokens_cache_write"], "input_tokens_cache_write"
        ),
        "outputTokens": _optional_int(row["output_tokens"], "output_tokens"),
        "totalTokens": _optional_int(row["total_tokens"], "total_tokens"),
        "status": _optional_text(row["status"], "status"),
    }


def _union_call_values(call: dict[str, Any]) -> tuple[Any, ...]:
    provider_call_id = call["providerCallId"]
    if provider_call_id is not None and len(provider_call_id.encode("utf-8")) > 256:
        raise ValueError("cross-ledger identity fields exceed their safe size bound")
    values = tuple(call[field] for field in UNION_CALL_FIELDS)
    text_bytes = 0
    for field, value in zip(UNION_CALL_FIELDS, values):
        if not isinstance(value, str):
            continue
        encoded_length = len(value.encode("utf-8"))
        text_bytes += encoded_length
        if encoded_length > 1024 or text_bytes > 4096:
            raise ValueError(
                "cross-ledger identity fields exceed their safe size bound"
            )
    return values


def _union_contribution(values: tuple[Any, ...]) -> tuple[Any, ...]:
    return tuple(
        values[UNION_FIELD_INDEX[field]] for field in UNION_CONTRIBUTION_FIELDS
    )


def _union_call_fingerprint(values: tuple[Any, ...]) -> bytes:
    serializable = list(values)
    for field in ("providerReportedCost", "providerReportedCredits"):
        value_index = UNION_FIELD_INDEX[field]
        value = serializable[value_index]
        if isinstance(value, Decimal):
            serializable[value_index] = _decimal_text(value)
    payload = json.dumps(serializable, ensure_ascii=True, separators=(",", ":"))
    return hashlib.sha256(payload.encode("utf-8")).digest()


class _MergedUsageTotals:
    """Compact totals for the keyed rows that survive cross-ledger comparison."""

    def __init__(self, rates: dict[str, Any]):
        self.rates = rates
        self.groups: dict[tuple[str, str | None, str | None], dict[str, Any]] = {}

    @staticmethod
    def group_key(values: tuple[Any, ...]) -> tuple[str, str | None, str | None]:
        call = dict(zip(UNION_CONTRIBUTION_FIELDS, values))
        return (
            call["role"],
            call["requestedModel"],
            call["providerEffectiveModel"],
        )

    def add(self, values: tuple[Any, ...]) -> None:
        call = dict(zip(UNION_CONTRIBUTION_FIELDS, values))
        group_key = self.group_key(values)
        state = self.groups.setdefault(
            group_key,
            {
                "count": 0,
                "statusCounts": Counter(),
                "effectiveSources": Counter(),
                "tokenSums": Counter(),
                "tokenMissing": Counter(),
                "estimateSubtotal": Decimal(0),
                "estimateIncomplete": 0,
                "missingTokenDimensions": Counter(),
                "missingRates": Counter(),
                "reportedSubtotal": Decimal(0),
                "reportedMissing": 0,
            },
        )
        state["count"] += 1
        state["statusCounts"][call["status"] or "unknown"] += 1
        if call["providerEffectiveModelSource"]:
            state["effectiveSources"][call["providerEffectiveModelSource"]] += 1
        for field in (
            "inputTokensUncached",
            "inputTokensCached",
            "cacheWriteInputTokens",
            "outputTokens",
            "totalTokens",
        ):
            value = call[field]
            if value is None:
                state["tokenMissing"][field] += 1
            else:
                state["tokenSums"][field] += value

        cost = _database_call_cost(call, self.rates)[2]
        state["estimateSubtotal"] += Decimal(cost["knownSubtotal"])
        if not cost["complete"]:
            state["estimateIncomplete"] += 1
        for field in cost["missingTokenDimensions"]:
            state["missingTokenDimensions"][field] += 1
        for field in cost["missingRates"]:
            state["missingRates"][field] += 1

        reported = call["providerReportedCredits"]
        if reported is None:
            state["reportedMissing"] += 1
        else:
            state["reportedSubtotal"] += reported

    def output(
        self, *, complete_scan: bool, unresolved_rows: int
    ) -> list[dict[str, Any]]:
        groups = []
        for (role, requested, effective), state in sorted(
            self.groups.items(),
            key=lambda item: tuple(value or "" for value in item[0]),
        ):
            token_totals = {}
            for field in (
                "inputTokensUncached",
                "inputTokensCached",
                "cacheWriteInputTokens",
                "outputTokens",
                "totalTokens",
            ):
                missing = state["tokenMissing"][field]
                subtotal = state["tokenSums"][field]
                field_complete = missing == 0
                token_totals[field] = {
                    "value": subtotal
                    if complete_scan and unresolved_rows == 0 and field_complete
                    else None,
                    "knownSubtotal": subtotal,
                    "completeForIncludedRows": field_complete,
                    "missingValues": missing,
                }

            rate_model = effective or requested
            rate_basis = (
                "provider_effective_model"
                if effective
                else "requested_model_fallback"
                if requested
                else "unknown_model"
            )
            estimate_complete = state["estimateIncomplete"] == 0
            reported_complete = state["reportedMissing"] == 0
            effective_sources = sorted(state["effectiveSources"])
            groups.append(
                {
                    "role": role,
                    "requestedModel": requested,
                    "providerEffectiveModel": effective,
                    "providerEffectiveModelSources": effective_sources,
                    "responseUsageRecordCount": state["count"],
                    "statusCounts": dict(sorted(state["statusCounts"].items())),
                    "tokenTotals": token_totals,
                    "calculatedCreditEstimate": {
                        "estimateType": "model_rate_scenario_not_verified_billing",
                        "value": _decimal_text(state["estimateSubtotal"])
                        if complete_scan and unresolved_rows == 0 and estimate_complete
                        else None,
                        "knownSubtotal": _decimal_text(state["estimateSubtotal"]),
                        "completeForIncludedRows": estimate_complete,
                        "rateModel": rate_model,
                        "rateModelBasis": rate_basis,
                        "missingTokenDimensions": sorted(
                            state["missingTokenDimensions"]
                        ),
                        "missingRates": sorted(state["missingRates"]),
                    },
                    "providerReportedCredits": {
                        "value": _decimal_text(state["reportedSubtotal"])
                        if complete_scan and unresolved_rows == 0 and reported_complete
                        else None,
                        "knownSubtotal": _decimal_text(state["reportedSubtotal"]),
                        "completeForIncludedRows": reported_complete,
                        "missingValues": state["reportedMissing"],
                        "note": (
                            "reported ledger field; not independently verified "
                            "as an account debit"
                        ),
                    },
                    "actualDebit": {"value": None, "status": "not_observed"},
                }
            )
        return groups


class _CrossLedgerUnion:
    def __init__(self):
        self.calls_by_id: dict[str, tuple[bytes, tuple[Any, ...], int, int, bool]] = {}
        self.group_counts: Counter = Counter()
        self.conflicting_ids: set[str] = set()
        self.schema_unverifiable_ids: set[str] = set()
        self.exact_duplicate_rows = 0
        self.exact_duplicate_rows_by_source = [0, 0]
        self.conflict_rows = 0
        self.conflict_rows_by_source = [0, 0]
        self.schema_unverifiable_rows = 0
        self.schema_unverifiable_rows_by_source = [0, 0]
        self.unkeyed_rows = [0, 0]
        self.source_rows = [0, 0]
        self.rate_context_missing_by_source = [Counter(), Counter()]
        self.estimated_memory_bytes = 0
        self.rejection_reason = None

    def add(self, source_index: int, call: dict[str, Any]) -> bool:
        self.source_rows[source_index] += 1
        for field, label in (
            ("providerEffectiveModel", "provider_effective_model"),
            ("provider", "provider"),
            ("actualServiceTier", "actual_service_tier"),
            ("actualServiceTierSource", "actual_service_tier_source"),
            ("fastModeUsed", "fast_mode_used"),
            ("billingSurface", "billing_surface"),
            ("accountPlan", "account_plan"),
            ("startedAt", "started_at"),
        ):
            value = call[field]
            if value is None or (isinstance(value, str) and not value.strip()):
                self.rate_context_missing_by_source[source_index][label] += 1
        provider_call_id = call["providerCallId"]
        if not provider_call_id or not provider_call_id.strip():
            self.unkeyed_rows[source_index] += 1
            return True
        try:
            values = _union_call_values(call)
        except ValueError:
            self.rejection_reason = "identity_field_size_limit"
            return False
        schema_complete = not call["schemaMissingFields"]
        fingerprint = _union_call_fingerprint(values)
        contribution = _union_contribution(values)
        existing = self.calls_by_id.get(provider_call_id)
        if provider_call_id in self.conflicting_ids:
            self.conflict_rows += 1
            self.conflict_rows_by_source[source_index] += 1
            return True
        if provider_call_id in self.schema_unverifiable_ids:
            self.schema_unverifiable_rows += 1
            self.schema_unverifiable_rows_by_source[source_index] += 1
            return True
        if existing is None:
            group_key = _MergedUsageTotals.group_key(contribution)
            if (
                group_key not in self.group_counts
                and len(self.group_counts) >= MAX_DATABASE_UNION_GROUPS
            ):
                self.rejection_reason = "group_limit"
                return False
            key_bytes = len(provider_call_id.encode("utf-8"))
            contribution_bytes = sum(
                len(value.encode("utf-8"))
                for value in contribution
                if isinstance(value, str)
            )
            entry_bytes = 768 + key_bytes + contribution_bytes
            if (
                self.estimated_memory_bytes + entry_bytes
                > MAX_DATABASE_UNION_MEMORY_BYTES
            ):
                self.rejection_reason = "memory_limit"
                return False
            self.calls_by_id[provider_call_id] = (
                fingerprint,
                contribution,
                entry_bytes,
                key_bytes,
                schema_complete,
            )
            self.estimated_memory_bytes += entry_bytes
            self.group_counts[group_key] += 1
            return True
        if existing[4] and schema_complete and existing[0] == fingerprint:
            self.exact_duplicate_rows += 1
            self.exact_duplicate_rows_by_source[source_index] += 1
            return True
        if existing[4] and schema_complete:
            self.conflicting_ids.add(provider_call_id)
            self.conflict_rows += 2
            self.conflict_rows_by_source[0] += 1
            self.conflict_rows_by_source[source_index] += 1
        else:
            self.schema_unverifiable_ids.add(provider_call_id)
            self.schema_unverifiable_rows += 2
            self.schema_unverifiable_rows_by_source[0] += 1
            self.schema_unverifiable_rows_by_source[source_index] += 1
        self.calls_by_id.pop(provider_call_id, None)
        self.estimated_memory_bytes -= existing[2] - (128 + existing[3])
        group_key = _MergedUsageTotals.group_key(existing[1])
        self.group_counts[group_key] -= 1
        if self.group_counts[group_key] == 0:
            del self.group_counts[group_key]
        return True


def _database_compact_aggregate(
    connection: sqlite3.Connection,
    thread_ids: list[str],
    root_id: str,
    rates: dict[str, Any],
    call_columns: set[str],
    thread_columns: set[str],
    overall_deadline: float | None = None,
) -> dict[str, Any]:
    """Bound a summary independently from the number of detailed call records."""

    def call_field(name: str) -> str:
        return f'c."{name}"' if name in call_columns else "NULL"

    source = 't."source"' if "source" in thread_columns else "NULL"
    kind = 't."thread_source"' if "thread_source" in thread_columns else "NULL"
    fields = (
        "input_tokens_uncached",
        "input_tokens_cached",
        "input_tokens_cache_write",
        "output_tokens",
        "total_tokens",
    )
    dimensions = []
    for field in fields:
        value = call_field(field)
        dimensions.extend(
            [
                f"SUM({value}) AS {field}",
                f"SUM(CASE WHEN {value} IS NULL THEN 1 ELSE 0 END) AS {field}_missing",
                f"SUM(CASE WHEN {value} IS NOT NULL AND "
                f"(typeof({value}) != 'integer' OR {value} < 0) THEN 1 ELSE 0 END) AS {field}_invalid",
            ]
        )
    uncached = call_field("input_tokens_uncached")
    cache_write = call_field("input_tokens_cache_write")
    requested = call_field("requested_model")
    actual = call_field("actual_model_used")
    final = call_field("final_model")
    placeholders = ", ".join("?" for _ in thread_ids)
    sql = f"""
        SELECT CASE WHEN c.thread_id = ? THEN 'root'
                    WHEN {kind} = 'subagent' OR lower(COALESCE({source}, '')) LIKE '%subagent%'
                    THEN 'subagent' ELSE 'other_descendant' END AS role,
               {requested} AS requested_model, {actual} AS actual_model,
               {final} AS final_model, COUNT(*) AS calls,
               {", ".join(dimensions)},
               SUM(CASE WHEN {cache_write} = 0 THEN {uncached} ELSE 0 END) AS priced_uncached,
               SUM(CASE WHEN {cache_write} IS NULL OR {cache_write} != 0 THEN 1 ELSE 0 END) AS unpriced_cache_write
        FROM usage_provider_calls c JOIN usage_threads t ON t.thread_id = c.thread_id
        WHERE c.thread_id IN ({placeholders})
        GROUP BY role, requested_model, actual_model, final_model
        LIMIT 101
    """
    deadline = _database_deadline(overall_deadline)
    parameters = (root_id, *thread_ids)
    _assert_indexed_thread_call_lookup(
        connection, thread_ids, sql=sql, parameters=parameters
    )
    connection.set_progress_handler(lambda: int(time.monotonic() >= deadline), 10000)
    try:
        rows = connection.execute(sql, parameters).fetchall()
    except sqlite3.OperationalError as exc:
        if "interrupt" not in str(exc).lower():
            raise
        return {"status": "unavailable", "reason": "aggregate_time_limit"}
    finally:
        connection.set_progress_handler(None, 0)

    groups = []
    for row in rows[:100]:
        if any(row[f"{field}_invalid"] for field in fields):
            raise UsageError("usage database contains invalid aggregate token values")
        requested_model = (
            _optional_text(row["requested_model"], "requested_model") or None
        )
        actual_model = _optional_text(row["actual_model"], "actual_model_used") or None
        final_model = _optional_text(row["final_model"], "final_model") or None
        effective = final_model or actual_model
        model = effective or requested_model
        missing = [field for field in fields if row[f"{field}_missing"]]
        if row["unpriced_cache_write"]:
            missing.append("cache_write_pricing")
        cost = _cost(
            {
                "model": model,
                "netNewInputTokens": row["priced_uncached"] or 0,
                "cachedInputTokens": row["input_tokens_cached"] or 0,
                "outputTokens": row["output_tokens"] or 0,
            },
            rates,
        )
        complete_estimate = not missing and cost["complete"]
        groups.append(
            {
                "role": row["role"],
                "requestedModel": requested_model,
                "providerEffectiveModel": effective,
                "providerEffectiveModelSource": "final_model"
                if final_model
                else "actual_model_used"
                if actual_model
                else None,
                "responseUsageRecordCount": row["calls"],
                "tokenTotals": {
                    field: {
                        "knownSubtotal": row[field] or 0,
                        "missingValues": row[f"{field}_missing"],
                    }
                    for field in fields
                },
                "calculatedCreditEstimate": {
                    "value": cost["credits"] if complete_estimate else None,
                    "knownSubtotal": cost["knownSubtotal"],
                    "rateModel": model,
                    "completeForSelectedRecordedRows": complete_estimate,
                    "rateModelBasis": "provider_effective_model"
                    if effective
                    else "requested_model_fallback"
                    if requested_model
                    else "unknown_model",
                    "missingTokenDimensions": missing,
                    "missingRates": cost["missingRates"],
                },
            }
        )
    return {
        "status": "partial" if len(rows) > 100 else "recorded_rows_only",
        "groups": groups,
        "groupsTruncated": len(rows) > 100,
        "actualDebitObserved": False,
        "note": "Only selected recorded threads; historical lineage and actual debits remain unproven.",
    }


def collect_database_tree(
    root_id: str,
    database_path: str,
    rates: dict[str, Any],
    *,
    aggregate: bool = False,
    aggregate_only: bool = False,
    union_callback: Any | None = None,
    union_row_limit: int | None = None,
    overall_deadline: float | None = None,
) -> dict[str, Any]:
    """Read an explicitly named usage database without startup, migration, or writes."""
    if not isinstance(root_id, str) or not root_id.strip():
        raise UsageError("root thread ID must be non-empty")
    source_deadline = time.monotonic() + MAX_DATABASE_SOURCE_SECONDS
    if overall_deadline is not None:
        source_deadline = min(source_deadline, overall_deadline)
    connection = None
    try:
        uri = Path(database_path).resolve(strict=True).as_uri() + "?mode=ro"
        connection = sqlite3.connect(uri, uri=True, timeout=5.0)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA query_only = ON")
        tables = {
            row["name"]
            for row in connection.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table'"
            )
        }
        if not {"usage_threads", "usage_provider_calls"}.issubset(tables):
            raise UsageError("database does not contain the Codex usage ledger tables")

        def columns(table: str) -> set[str]:
            return {
                row["name"]
                for row in connection.execute(f'PRAGMA table_info("{table}")')
            }

        thread_columns = columns("usage_threads")
        call_columns = columns("usage_provider_calls")
        if not {
            "thread_id",
            "parent_thread_id",
            "root_thread_id",
            "fork_parent_thread_id",
        }.issubset(thread_columns):
            raise UsageError("usage database has no supported thread lineage schema")
        if not {"provider_call_id", "thread_id", "started_at"}.issubset(call_columns):
            raise UsageError("usage database has no supported provider-call schema")
        if (
            connection.execute(
                "SELECT 1 FROM usage_threads WHERE thread_id = ?", (root_id,)
            ).fetchone()
            is None
        ):
            raise UsageError("root thread is not present in usage_threads")

        thread_source = (
            'thread.source AS "source"'
            if "source" in thread_columns
            else 'NULL AS "source"'
        )
        thread_kind = (
            'thread.thread_source AS "thread_source"'
            if "thread_source" in thread_columns
            else 'NULL AS "thread_source"'
        )
        # Keep the recursive row outermost and remove its TEXT affinity: SQLite
        # can then search the existing parent/fork expression index per child.
        deadline = _database_deadline(source_deadline)
        connection.set_progress_handler(
            lambda: int(time.monotonic() >= deadline), 10000
        )
        thread_rows = connection.execute(
            f"""
            WITH RECURSIVE lineage(thread_id, depth, visited) AS (
                SELECT thread_id, 0, ',' || thread_id || ','
                FROM usage_threads WHERE thread_id = ?
                UNION ALL
                SELECT child.thread_id, lineage.depth + 1,
                       lineage.visited || child.thread_id || ','
                FROM lineage
                CROSS JOIN usage_threads AS child
                WHERE COALESCE(
                      NULLIF(child.parent_thread_id, ''),
                      NULLIF(child.fork_parent_thread_id, '')
                  ) = +lineage.thread_id
                  AND instr(lineage.visited, ',' || child.thread_id || ',') = 0
                  AND lineage.depth < ?
                LIMIT ?
            ),
            selected(thread_id) AS (
                SELECT thread_id FROM lineage
                UNION
                SELECT thread_id FROM (
                    SELECT thread_id FROM usage_threads
                    WHERE root_thread_id = ?
                    LIMIT ?
                )
            )
            SELECT thread.thread_id, thread.parent_thread_id,
                   thread.fork_parent_thread_id, thread.root_thread_id,
                   {thread_source}, {thread_kind},
                   COALESCE((
                       SELECT MIN(lineage.depth) FROM lineage
                       WHERE lineage.thread_id = thread.thread_id
                   ), 1) AS depth
            FROM usage_threads AS thread
            JOIN selected ON selected.thread_id = thread.thread_id
            ORDER BY CASE WHEN thread.thread_id = ? THEN 0 ELSE 1 END,
                     depth, thread.thread_id
            LIMIT ?
            """,
            (
                root_id,
                MAX_DATABASE_THREADS,
                MAX_DATABASE_THREADS + 1,
                root_id,
                MAX_DATABASE_THREADS + 1,
                root_id,
                MAX_DATABASE_THREADS + 1,
            ),
        ).fetchall()
        connection.set_progress_handler(None, 0)
        threads_truncated = len(thread_rows) > MAX_DATABASE_THREADS
        thread_rows = thread_rows[:MAX_DATABASE_THREADS]
        thread_ids = [row["thread_id"] for row in thread_rows]
        if root_id not in thread_ids:
            raise UsageError("root thread was omitted by the bounded lineage query")
        _assert_indexed_thread_call_lookup(connection, thread_ids)

        selected_fields = DATABASE_CALL_COLUMNS
        expressions = [
            f'"{field}" AS "{field}"' if field in call_columns else f'NULL AS "{field}"'
            for field in selected_fields
        ]
        placeholders = ", ".join("?" for _ in thread_ids)
        aggregate_result = (
            _database_compact_aggregate(
                connection,
                thread_ids,
                root_id,
                rates,
                call_columns,
                thread_columns,
                source_deadline,
            )
            if aggregate or aggregate_only
            else None
        )
        if aggregate_only:
            union_scan = None
            if union_callback is not None:
                if (
                    union_row_limit is None
                    or not 0 <= union_row_limit <= MAX_DATABASE_UNION_CALLS_PER_SOURCE
                ):
                    raise UsageError("cross-ledger row limit is outside its safe bound")
                thread_roles = {}
                for thread_row in thread_rows:
                    thread_id = thread_row["thread_id"]
                    source = _optional_text(thread_row["source"], "thread source")
                    thread_source = _optional_text(
                        thread_row["thread_source"], "thread source kind"
                    )
                    thread_roles[thread_id] = (
                        "root"
                        if thread_id == root_id
                        else "subagent"
                        if thread_source == "subagent"
                        or (source is not None and "subagent" in source.lower())
                        else "other_descendant"
                    )
                selected_fields = DATABASE_CALL_COLUMNS
                expressions = [
                    f'"{field}" AS "{field}"'
                    if field in call_columns
                    else f'NULL AS "{field}"'
                    for field in selected_fields
                ]
                placeholders = ", ".join("?" for _ in thread_ids)
                row_limit = union_row_limit or 0
                query_deadline = _database_deadline(source_deadline)
                call_sql = f"""
                    SELECT provider_call_id, thread_id, {", ".join(expressions)}
                    FROM usage_provider_calls
                    WHERE thread_id IN ({placeholders})
                    LIMIT ?
                """
                call_parameters = (*thread_ids, row_limit + 1)
                _assert_indexed_thread_call_lookup(
                    connection,
                    thread_ids,
                    sql=call_sql,
                    parameters=call_parameters,
                )
                connection.set_progress_handler(
                    lambda: int(time.monotonic() >= query_deadline), 10000
                )
                rows_scanned = 0
                minimum_started_at = None
                maximum_started_at = None
                truncated = False
                extra_row_observed = False
                truncation_reason = None
                try:
                    cursor = connection.execute(call_sql, call_parameters)
                    for row in cursor:
                        if rows_scanned >= row_limit:
                            truncated = True
                            extra_row_observed = True
                            truncation_reason = (
                                "per_source_or_whole_operation_row_limit"
                            )
                            break
                        if time.monotonic() >= query_deadline:
                            truncated = True
                            extra_row_observed = True
                            truncation_reason = "query_time_limit"
                            break
                        call = _normalize_database_call(row, thread_roles, call_columns)
                        accepted = union_callback(call)
                        rows_scanned += 1
                        if accepted is False:
                            truncated = True
                            truncation_reason = "union_state_limit"
                            break
                        started_at = call["startedAt"]
                        if started_at is not None:
                            minimum_started_at = (
                                started_at
                                if minimum_started_at is None
                                else min(minimum_started_at, started_at)
                            )
                            maximum_started_at = (
                                started_at
                                if maximum_started_at is None
                                else max(maximum_started_at, started_at)
                            )
                except sqlite3.OperationalError as exc:
                    if (
                        getattr(exc, "sqlite_errorcode", None)
                        != sqlite3.SQLITE_INTERRUPT
                    ):
                        raise
                    truncated = True
                    truncation_reason = "query_time_limit"
                finally:
                    connection.set_progress_handler(None, 0)
                union_scan = {
                    "rowsScanned": rows_scanned,
                    "rowsScannedAtLeast": rows_scanned + 1
                    if extra_row_observed
                    else rows_scanned,
                    "complete": not truncated,
                    "truncated": truncated,
                    "reason": truncation_reason,
                    "minimumStartedAtObserved": minimum_started_at,
                    "maximumStartedAtObserved": maximum_started_at,
                    "timestampOrder": "SQLite text order; timestamps are not normalized",
                }
            result = {
                "schema": "codex-usage-tree/v1",
                "status": "partial",
                "scope": "root_and_known_descendants",
                "rootThreadId": root_id,
                "lineage": {
                    "knownThreadCount": len(thread_rows),
                    "truncated": threads_truncated,
                    "complete": False,
                    "coverageNote": "Unrecorded or historical descendants are unknown.",
                },
                "rateMap": {
                    "source": rates["source"],
                    "date": rates["date"],
                    "unit": "credits per 1,000,000 exact-model tokens",
                },
                "columnCoverage": {
                    "usage_threads": {
                        field: field in thread_columns
                        for field in (
                            "parent_thread_id",
                            "root_thread_id",
                            "fork_parent_thread_id",
                            "source",
                            "thread_source",
                        )
                    },
                    "usage_provider_calls": {
                        field: field in call_columns
                        for field in (
                            "provider_call_id",
                            "thread_id",
                            *selected_fields,
                        )
                    },
                    "indexedExactThreadLookup": True,
                },
                "recordedAggregate": aggregate_result,
                "actualDebitObserved": False,
            }
            if union_scan is not None:
                result["_unionScan"] = union_scan
            return result
        deadline = _database_deadline(source_deadline)
        call_sql = f"""
            SELECT provider_call_id, thread_id, {", ".join(expressions)}
            FROM usage_provider_calls
            WHERE thread_id IN ({placeholders})
            ORDER BY started_at DESC, provider_call_id DESC
            LIMIT ?
        """
        call_parameters = (*thread_ids, MAX_DATABASE_CALLS + 1)
        _assert_indexed_thread_call_lookup(
            connection,
            thread_ids,
            sql=call_sql,
            parameters=call_parameters,
        )
        connection.set_progress_handler(
            lambda: int(time.monotonic() >= deadline), 10000
        )
        call_rows = connection.execute(call_sql, call_parameters).fetchall()
        connection.set_progress_handler(None, 0)
        calls_truncated = len(call_rows) > MAX_DATABASE_CALLS
        call_rows = call_rows[:MAX_DATABASE_CALLS]
    except UsageError:
        raise
    except (OSError, sqlite3.Error, ValueError) as exc:
        if getattr(exc, "sqlite_errorcode", None) == sqlite3.SQLITE_INTERRUPT:
            raise UsageError("usage database query exceeded its time limit") from exc
        raise UsageError(
            "could not read the explicitly supplied usage database in read-only mode"
        ) from exc
    finally:
        if connection is not None:
            connection.close()

    calls = []
    for row in call_rows:
        requested_model = (
            _optional_text(row["requested_model"], "requested_model") or None
        )
        actual_model = (
            _optional_text(row["actual_model_used"], "actual_model_used") or None
        )
        final_model = _optional_text(row["final_model"], "final_model") or None
        effective_model = final_model or actual_model
        effective_model_source = (
            "final_model"
            if final_model
            else "actual_model_used"
            if actual_model
            else None
        )
        reported_credits = row["provider_reported_credits"]
        if reported_credits is not None:
            try:
                reported_credits = Decimal(str(reported_credits))
            except InvalidOperation as exc:
                raise UsageError("usage database has invalid reported credits") from exc
            if not reported_credits.is_finite():
                raise UsageError("usage database has non-finite reported credits")
        calls.append(
            {
                "threadId": row["thread_id"],
                "providerCallId": row["provider_call_id"],
                "turnId": _optional_text(row["turn_id"], "turn_id"),
                "requestId": _optional_text(row["request_id"], "request_id"),
                "provider": _optional_text(row["provider"], "provider"),
                "requestedModel": requested_model,
                "providerEffectiveModel": effective_model,
                "providerEffectiveModelSource": effective_model_source,
                "providerReportedCredits": reported_credits,
                "startedAt": _optional_text(row["started_at"], "started_at"),
                "completedAt": _optional_text(row["completed_at"], "completed_at"),
                "inputTokensUncached": _optional_int(
                    row["input_tokens_uncached"], "input_tokens_uncached"
                ),
                "inputTokensCached": _optional_int(
                    row["input_tokens_cached"], "input_tokens_cached"
                ),
                "cacheWriteInputTokens": _optional_int(
                    row["input_tokens_cache_write"], "input_tokens_cache_write"
                ),
                "outputTokens": _optional_int(row["output_tokens"], "output_tokens"),
                "totalTokens": _optional_int(row["total_tokens"], "total_tokens"),
                "status": _optional_text(row["status"], "status"),
            }
        )

    calls_by_thread: dict[str, list[dict[str, Any]]] = {}
    for call in calls:
        calls_by_thread.setdefault(call["threadId"], []).append(call)
    truncated = threads_truncated or calls_truncated

    def response_usage_record(call: dict[str, Any]) -> dict[str, Any]:
        rate_model, estimate_basis, cost = _database_call_cost(call, rates)
        return {
            "providerCallId": call["providerCallId"],
            "requestId": call["requestId"],
            "turnId": call["turnId"],
            "provider": call["provider"],
            "requestedModel": call["requestedModel"],
            "providerEffectiveModel": call["providerEffectiveModel"],
            "providerEffectiveModelSource": call["providerEffectiveModelSource"],
            "startedAt": call["startedAt"],
            "completedAt": call["completedAt"],
            "tokenUsage": {
                "inputTokensUncached": call["inputTokensUncached"],
                "inputTokensCached": call["inputTokensCached"],
                "cacheWriteInputTokens": call["cacheWriteInputTokens"],
                "outputTokens": call["outputTokens"],
                "totalTokens": call["totalTokens"],
            },
            "status": call["status"],
            "calculatedCreditEstimate": {
                "value": cost["credits"],
                "knownSubtotal": cost["knownSubtotal"],
                "rateModel": rate_model,
                "rateModelBasis": estimate_basis,
                "missingTokenDimensions": cost["missingTokenDimensions"],
                "missingRates": cost["missingRates"],
            },
            "providerReportedCredits": (
                _decimal_text(call["providerReportedCredits"])
                if call["providerReportedCredits"] is not None
                else None
            ),
            "actualDebit": {"value": None, "status": "not_observed"},
        }

    thread_output = []
    for row in thread_rows:
        thread_id = row["thread_id"]
        thread_calls = calls_by_thread.get(thread_id, [])
        source = _optional_text(row["source"], "thread source")
        thread_source = _optional_text(row["thread_source"], "thread source kind")
        role = (
            "root"
            if thread_id == root_id
            else "subagent"
            if thread_source == "subagent"
            or (source is not None and "subagent" in source.lower())
            else "descendant"
        )
        thread_output.append(
            {
                "threadId": thread_id,
                "parentThreadId": row["parent_thread_id"],
                "forkParentThreadId": row["fork_parent_thread_id"],
                "rootThreadId": row["root_thread_id"],
                "depth": row["depth"],
                "role": role,
                "source": source,
                "threadSource": thread_source,
                "usageStatus": "observed_response_records"
                if thread_calls
                else "no_recorded_response_usage",
                "responseUsageRecordCount": len(thread_calls),
                "responseUsageRecords": [
                    response_usage_record(call) for call in thread_calls
                ],
                "models": _database_model_groups(
                    thread_calls, rates, coverage_truncated=truncated
                ),
            }
        )

    root_calls = [call for call in calls if call["threadId"] == root_id]
    child_calls = [call for call in calls if call["threadId"] != root_id]
    return {
        "schema": "codex-usage-tree/v1",
        "status": "partial",
        "scope": "root_and_known_descendants",
        "rootThreadId": root_id,
        "recordedAggregate": aggregate_result,
        "lineage": {
            "source": "usage_threads parent, fork-parent, and root fields",
            "knownThreadCount": len(thread_rows),
            "truncated": threads_truncated,
            "complete": False,
            "coverageNote": (
                "unrecorded or historical descendants cannot be enumerated "
                "from this ledger"
            ),
        },
        "usage": {
            "source": "explicit read-only usage_provider_calls rows",
            "responseUsageRecordCount": len(calls),
            "truncated": calls_truncated,
            "bestEffort": True,
            "coverageNote": (
                "records only usage-bearing responses successfully projected "
                "to this database; no independent expected-response denominator "
                "exists"
            ),
            "timingNote": (
                "new response projections store observation time; provider "
                "start time and latency are not independently measured"
            ),
        },
        "rateMap": {
            "source": rates["source"],
            "date": rates["date"],
            "unit": "credits per 1,000,000 exact-model tokens",
            "creditsPerMillionTokens": {
                model: {name: _decimal_text(rate) for name, rate in entries.items()}
                for model, entries in sorted(rates["models"].items())
            },
        },
        "threads": thread_output,
        "totals": {
            "root": {
                "groups": _database_model_groups(
                    root_calls, rates, coverage_truncated=truncated
                )
            },
            "descendants": {
                "groups": _database_model_groups(
                    child_calls, rates, coverage_truncated=truncated
                )
            },
            "tree": {
                "groups": _database_model_groups(
                    calls, rates, coverage_truncated=truncated
                )
            },
        },
        "completeness": {
            "historicalCoverageComplete": False,
            "lineageTruncated": threads_truncated,
            "responseRowsTruncated": calls_truncated,
            "selectedLedgerRowsComplete": not truncated,
            "actualDebitObserved": False,
        },
        "bounds": {
            "maxThreads": MAX_DATABASE_THREADS,
            "maxResponseUsageRecords": MAX_DATABASE_CALLS,
        },
    }


def collect_database_union(
    root_id: str,
    database_paths: list[str],
    rates: dict[str, Any],
) -> dict[str, Any]:
    """Read two explicitly named ledgers and conservatively deduplicate their calls."""
    if len(database_paths) != 2:
        raise UsageError("cross-ledger aggregate requires exactly two databases")
    try:
        resolved_paths = [Path(path).resolve(strict=True) for path in database_paths]
    except (OSError, ValueError) as exc:
        raise UsageError(
            "could not resolve both explicitly supplied usage databases"
        ) from exc
    if resolved_paths[0] == resolved_paths[1]:
        raise UsageError("cross-ledger aggregate requires two distinct databases")

    overall_deadline = time.monotonic() + MAX_DATABASE_UNION_SECONDS
    union = _CrossLedgerUnion()
    source_results = []
    all_source_scans_complete = True
    for source_index, database_path in enumerate(resolved_paths):
        label = "primary" if source_index == 0 else "comparison"
        processed_before = sum(union.source_rows)
        remaining_total = max(0, MAX_DATABASE_UNION_CALLS_TOTAL - processed_before)
        row_limit = min(MAX_DATABASE_UNION_CALLS_PER_SOURCE, remaining_total)
        callback = lambda call, index=source_index: union.add(index, call)
        try:
            result = collect_database_tree(
                root_id,
                str(database_path),
                rates,
                aggregate_only=True,
                union_callback=callback,
                union_row_limit=row_limit,
                overall_deadline=overall_deadline,
            )
        except (UsageError, OSError, ValueError) as exc:
            all_source_scans_complete = False
            detail = (
                str(exc)
                if isinstance(exc, UsageError)
                else "read-only source open failed"
            )
            source_results.append(
                {
                    "source": label,
                    "status": "unavailable",
                    "reason": detail,
                    "lineage": None,
                    "recordedAggregate": None,
                    "rowScan": {
                        "rowsScanned": union.source_rows[source_index],
                        "complete": False,
                        "truncated": True,
                        "reason": "source_query_failed",
                    },
                    "unkeyedProviderCallRowsScanned": union.unkeyed_rows[source_index],
                    "conflictingRowsWithheldObserved": union.conflict_rows_by_source[
                        source_index
                    ],
                    "exactDuplicateRowsOmittedObserved": union.exact_duplicate_rows_by_source[
                        source_index
                    ],
                }
            )
            continue

        scan = result.pop("_unionScan")
        if scan["reason"] == "union_state_limit":
            scan["reason"] = union.rejection_reason or "union_state_limit"
        if (
            not scan["complete"]
            or result["lineage"]["truncated"]
            or result["recordedAggregate"]["status"] == "unavailable"
        ):
            all_source_scans_complete = False
        source_results.append(
            {
                "source": label,
                "status": "partial",
                "scope": "recorded rows in bounded known lineage",
                "lineage": result["lineage"],
                "columnCoverage": result["columnCoverage"],
                "recordedAggregate": result["recordedAggregate"],
                "rowScan": scan,
                "unkeyedProviderCallRowsScanned": union.unkeyed_rows[source_index],
                "conflictingRowsWithheldObserved": union.conflict_rows_by_source[
                    source_index
                ],
                "exactDuplicateRowsOmittedObserved": union.exact_duplicate_rows_by_source[
                    source_index
                ],
            }
        )

    for source_index, source in enumerate(source_results):
        source["unkeyedProviderCallRowsScanned"] = union.unkeyed_rows[source_index]
        source["conflictingRowsWithheldObserved"] = union.conflict_rows_by_source[
            source_index
        ]
        source["schemaUnverifiableRowsWithheldObserved"] = (
            union.schema_unverifiable_rows_by_source[source_index]
        )
        source["rateContextMissingRowsObserved"] = dict(
            sorted(union.rate_context_missing_by_source[source_index].items())
        )
        source["exactDuplicateRowsOmittedObserved"] = (
            union.exact_duplicate_rows_by_source[source_index]
        )

    unresolved_rows = (
        sum(union.unkeyed_rows) + union.conflict_rows + union.schema_unverifiable_rows
    )
    merge_totals_available = all_source_scans_complete
    merged_totals_accumulator = None
    if all_source_scans_complete:
        merged_totals_accumulator = _MergedUsageTotals(rates)
        for index, (_, contribution, _, _, _) in enumerate(union.calls_by_id.values()):
            if index % 128 == 0 and time.monotonic() >= overall_deadline:
                merge_totals_available = False
                break
            merged_totals_accumulator.add(contribution)
        if time.monotonic() >= overall_deadline:
            merge_totals_available = False

    if merge_totals_available:
        assert merged_totals_accumulator is not None
        merged_totals = {
            "status": "partial_excluding_unresolved"
            if unresolved_rows
            else "selected_recorded_rows_deduplicated",
            "scope": "unique keyed rows in the selected recorded lineages only",
            "responseUsageRecordCount": len(union.calls_by_id),
            "excludedUnresolvedRows": unresolved_rows,
            "groups": merged_totals_accumulator.output(
                complete_scan=True, unresolved_rows=unresolved_rows
            ),
            "note": (
                "Unkeyed rows, every row for conflicting provider_call_id values, and "
                "duplicate IDs whose source schemas cannot verify all compared fields "
                "are excluded. When any are excluded, group values are known subtotals "
                "only. Historical descendants and actual debits remain unknown."
            ),
        }
    else:
        operation_timeout = (
            all_source_scans_complete and time.monotonic() >= overall_deadline
        )
        merged_totals = {
            "status": "unavailable_operation_time_limit"
            if operation_timeout
            else "unavailable_incomplete_source_scan",
            "scope": "no merged total claimed",
            "responseUsageRecordCount": None,
            "excludedUnresolvedRows": unresolved_rows,
            "groups": [],
            "note": (
                "The operation exceeded its total time bound or a source scan was "
                "incomplete; per-source aggregates remain visible, but no merged "
                "total is offered."
            ),
        }

    return {
        "schema": "codex-usage-tree/v2",
        "status": "partial",
        "scope": "two_explicit_read_only_ledgers_for_root_and_known_descendants",
        "rootThreadId": root_id,
        "sources": source_results,
        "crossLedgerDeduplication": {
            "status": "complete_for_selected_rows"
            if all_source_scans_complete
            else "incomplete",
            "key": "non-empty provider_call_id",
            "matchRequires": [
                "matching thread, turn, request and provider identity",
                "matching timestamps, requested/effective model provenance, status, token fields and reported credits",
                "matching root/subagent/other-descendant role",
                "all compared source columns present in both ledgers",
            ],
            "rowsScanned": sum(union.source_rows),
            "rowsScannedBySource": union.source_rows,
            "estimatedRetainedBytes": union.estimated_memory_bytes,
            "exactDuplicateRowsOmittedObserved": union.exact_duplicate_rows,
            "conflictingProviderCallIdsObserved": len(union.conflicting_ids),
            "conflictingRowsWithheldObserved": union.conflict_rows,
            "schemaUnverifiableProviderCallIdsObserved": len(
                union.schema_unverifiable_ids
            ),
            "schemaUnverifiableRowsWithheldObserved": union.schema_unverifiable_rows,
            "unkeyedRowsWithheldObserved": sum(union.unkeyed_rows),
            "unresolvedRowsWithheldObserved": unresolved_rows,
            "mergedTotals": merged_totals,
        },
        "rateMap": {
            "source": rates["source"],
            "date": rates["date"],
            "unit": "credits per 1,000,000 exact-model tokens",
        },
        "pricingCoverage": {
            "rateMapAppliedAs": "same selected model-rate scenario for both sources",
            "effectiveDatedPerCallRateMatchPerformed": False,
            "requiredPerCallContext": [
                "provider and billing policy",
                "effective model and call timestamp",
                "actual service tier and its source",
                "actual fast/standard mode",
                "account plan and billing surface",
            ],
            "missingContextRowsObservedBySource": [
                {
                    "source": "primary",
                    "missing": dict(
                        sorted(union.rate_context_missing_by_source[0].items())
                    ),
                },
                {
                    "source": "comparison",
                    "missing": dict(
                        sorted(union.rate_context_missing_by_source[1].items())
                    ),
                },
            ],
            "note": (
                "Calculated credits are model-rate scenario estimates, not an "
                "effective-dated per-call billing match. Missing call context and "
                "rate gaps remain explicit; provider-reported credits are not "
                "verified account debits, and actual debit is not observed."
            ),
        },
        "completeness": {
            "historicalCoverageComplete": False,
            "crossLedgerScanCompleteForSelectedLineage": all_source_scans_complete,
            "unresolvedRowsExcludedFromMergedTotals": True,
            "actualDebitObserved": False,
            "coverageNote": (
                "The result covers only recorded calls in the bounded known lineage; "
                "unrecorded or historical descendants are not discoverable here."
            ),
        },
        "bounds": {
            "maxThreadsPerSource": MAX_DATABASE_THREADS,
            "maxRowsPerSource": MAX_DATABASE_UNION_CALLS_PER_SOURCE,
            "maxRowsTotal": MAX_DATABASE_UNION_CALLS_TOTAL,
            "maxEstimatedRetainedBytes": MAX_DATABASE_UNION_MEMORY_BYTES,
            "maxMergedGroups": MAX_DATABASE_UNION_GROUPS,
            "maxSourceSeconds": MAX_DATABASE_SOURCE_SECONDS,
            "maxQuerySeconds": MAX_DATABASE_QUERY_SECONDS,
            "maxOperationSeconds": MAX_DATABASE_UNION_SECONDS,
        },
    }


def fetch_usage(api: Any, thread_id: str) -> dict[str, Any] | None:
    return _normalize(
        api.request("account/usage/read", {"threadId": thread_id}), thread_id
    )


def fetch_descendants(api: Any, root_id: str, max_per_state: int):
    descendants: dict[str, dict[str, Any]] = {}
    conflicts = set()
    pages, truncated_states = 0, []
    for archived in (False, True):
        cursor, seen, received, truncated = None, set(), 0, False
        for _ in range(max_per_state):
            if cursor in seen:
                truncated = True
                break
            seen.add(cursor)
            limit = min(PAGE_SIZE, max_per_state - received)
            params = {
                "limit": limit,
                "sourceKinds": ["subAgentThreadSpawn"],
                "archived": archived,
                "useStateDbOnly": True,
                "ancestorThreadId": root_id,
                "sortDirection": "desc",
            }
            if cursor is not None:
                params["cursor"] = cursor
            page = api.request("thread/list", params)
            data = page.get("data")
            if not isinstance(data, list) or len(data) > limit:
                raise UsageError("thread/list returned malformed or oversized data")
            pages += 1
            received += len(data)
            for item in data:
                if not isinstance(item, dict) or not isinstance(item.get("id"), str):
                    raise UsageError("thread/list returned a malformed thread identity")
                thread_id = item["id"]
                if thread_id == root_id:
                    continue
                parent_id = item.get("parentThreadId")
                if parent_id is not None and not isinstance(parent_id, str):
                    raise UsageError("thread/list returned a malformed parent identity")
                prior = descendants.get(thread_id)
                if prior and prior["parentThreadId"] != parent_id:
                    conflicts.add(thread_id)
                else:
                    descendants.setdefault(
                        thread_id,
                        {"threadId": thread_id, "parentThreadId": parent_id},
                    )
            next_cursor = page.get("nextCursor")
            if next_cursor is None:
                break
            if not isinstance(next_cursor, str) or not next_cursor:
                raise UsageError("thread/list returned a malformed cursor")
            if received >= max_per_state:
                truncated = True
                break
            cursor = next_cursor
        else:
            truncated = True
        truncated_states.append(truncated)
    meta = {
        "source": "thread/list",
        "sourceKinds": ["subAgentThreadSpawn"],
        "useStateDbOnly": True,
        "archivedStatesQueried": [False, True],
        "pagesRead": pages,
        "maxPagesPerArchivedState": max_per_state,
        "maxThreadsPerArchivedState": max_per_state,
        "truncated": any(truncated_states),
        "truncatedArchivedStates": [
            state
            for state, truncated in zip((False, True), truncated_states)
            if truncated
        ],
        "conflictingParentThreadIds": sorted(conflicts),
    }
    return sorted(descendants.values(), key=lambda row: row["threadId"]), meta


def collect_tree(api: Any, root_id: str, rates: dict[str, Any], max_per_state: int):
    root = {
        "threadId": root_id,
        "parentThreadId": None,
        "role": "root",
        "usage": fetch_usage(api, root_id),
    }
    try:
        children, lineage = fetch_descendants(api, root_id, max_per_state)
        lineage_error = None
    except RpcError as exc:
        children, lineage = (
            [],
            {
                "source": "thread/list",
                "sourceKinds": ["subAgentThreadSpawn"],
                "useStateDbOnly": True,
                "archivedStatesQueried": [False, True],
                "pagesRead": 0,
                "maxPagesPerArchivedState": max_per_state,
                "maxThreadsPerArchivedState": max_per_state,
                "truncated": True,
                "errorRpcCode": exc.code,
                "conflictingParentThreadIds": [],
            },
        )
        lineage_error = {"method": exc.method, "rpcCode": exc.code}

    known_ids = {root_id, *(row["threadId"] for row in children)}
    unresolved = [
        row["threadId"]
        for row in children
        if row["parentThreadId"] is None or row["parentThreadId"] not in known_ids
    ]
    lineage["unresolvedParentThreadIds"] = unresolved
    rows, errors, stop = [root], [], False
    for child in children:
        status = "available"
        try:
            if stop:
                usage, status = None, "not_requested_after_error"
            else:
                usage = fetch_usage(api, child["threadId"])
                if usage is None:
                    status = "no_thread_usage"
        except RpcError as exc:
            usage, status, stop = None, "rpc_error", True
            errors.append(
                {
                    "threadId": child["threadId"],
                    "method": exc.method,
                    "rpcCode": exc.code,
                }
            )
        rows.append({**child, "role": "child", "usageStatus": status, "usage": usage})

    public_rows = []
    for row in rows:
        groups = [] if row["usage"] is None else row["usage"]["groups"]
        public_rows.append(
            {
                "threadId": row["threadId"],
                "parentThreadId": row["parentThreadId"],
                "role": row["role"],
                "usageStatus": row.get(
                    "usageStatus", "available" if row["usage"] else "no_thread_usage"
                ),
                "groups": [
                    {
                        **group,
                        "calculatedCredits": _cost(group, rates),
                    }
                    for group in groups
                ],
            }
        )

    root_total, child_total, tree_total = (
        aggregate([root], rates),
        aggregate(rows[1:], rates),
        aggregate(rows, rates),
    )
    lineage_complete = (
        not lineage["truncated"]
        and not lineage["conflictingParentThreadIds"]
        and not unresolved
        and lineage_error is None
    )
    usage_complete = all(row["usage"] is not None for row in rows) and not errors
    pricing_complete = all(
        group["calculatedCredits"]["complete"]
        for summary in (root_total, child_total, tree_total)
        for group in summary["groups"]
    )
    complete = lineage_complete and usage_complete and pricing_complete
    return {
        "schema": "codex-usage-tree/v1",
        "status": "complete" if complete else "partial",
        "scope": "root_and_spawned_descendants",
        "rootThreadId": root_id,
        "lineage": lineage,
        "lineageError": lineage_error,
        "usage": {
            "source": "account/usage/read",
            "usageErrors": errors,
            "availableThreadCount": sum(row["usage"] is not None for row in rows),
            "requestedThreadCount": len(rows),
        },
        "rateMap": {
            "source": rates["source"],
            "date": rates["date"],
            "unit": "credits per 1,000,000 exact-model tokens",
            "creditsPerMillionTokens": {
                model: {name: _decimal_text(rate) for name, rate in entries.items()}
                for model, entries in sorted(rates["models"].items())
            },
        },
        "threads": public_rows,
        "totals": {"root": root_total, "children": child_total, "tree": tree_total},
        "completeness": {
            "lineageComplete": lineage_complete,
            "usageComplete": usage_complete,
            "pricingComplete": pricing_complete,
        },
    }


def error_document(root_id: str, exc: UsageError):
    failure = {"message": str(exc)}
    if isinstance(exc, RpcError):
        failure = {
            "method": exc.method,
            "rpcCode": exc.code,
            "message": (
                "native usage/lineage API request failed; endpoint or account access "
                "may be unavailable"
            ),
        }
    return {
        "schema": "codex-usage-tree/v1",
        "status": "unavailable",
        "scope": "root_and_spawned_descendants",
        "rootThreadId": root_id,
        "failure": failure,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--thread-id", required=True, help="root thread UUID")
    parser.add_argument(
        "--rollout-file",
        action="append",
        help="read this local rollout JSONL file only (repeat for each known tree thread)",
    )
    parser.add_argument(
        "--database",
        help=(
            "read this exact local usage_1.sqlite path read-only "
            "(no app-server startup or migration)"
        ),
    )
    parser.add_argument(
        "--also-database",
        help=(
            "read one additional exact ledger for a bounded aggregate-only union; "
            "requires --database and --aggregate-recorded"
        ),
    )
    parser.add_argument(
        "--aggregate-recorded",
        action="store_true",
        help="show a time-bounded aggregate without loading detailed call records",
    )
    parser.add_argument(
        "--expected-child-id",
        action="append",
        default=[],
        help="descendant ID from a previously complete thread/list result (repeat as needed)",
    )
    parser.add_argument("--codex-bin", default="codex", help="Codex executable")
    parser.add_argument(
        "--allow-local-app-server-startup",
        action="store_true",
        help="allow normal app-server startup, which can initialize SQLite and backfill rollouts",
    )
    parser.add_argument(
        "--rates-json", help="rate map JSON replacing the embedded dated map"
    )
    parser.add_argument(
        "--standard-ledger-rates-at",
        help="use ledger default/standard rates at this date/time as an explicit scenario",
    )
    parser.add_argument(
        "--max-threads-per-state", type=int, default=DEFAULT_MAX_THREADS_PER_STATE
    )
    parser.add_argument("--timeout-seconds", type=int, default=20)
    parser.add_argument("--overall-timeout-seconds", type=int, default=300)
    args = parser.parse_args(argv)
    if not args.thread_id.strip():
        parser.error("--thread-id cannot be empty")
    if args.database and args.rollout_file:
        parser.error("--database and --rollout-file select different local query modes")
    if args.also_database and not (args.database and args.aggregate_recorded):
        parser.error("--also-database requires --database and --aggregate-recorded")
    if args.database and args.allow_local_app_server_startup:
        parser.error("read-only database mode does not start an app-server")
    if args.aggregate_recorded and not args.database:
        parser.error("--aggregate-recorded requires --database")
    if args.standard_ledger_rates_at and (not args.database or args.rates_json):
        parser.error(
            "--standard-ledger-rates-at requires --database and no --rates-json"
        )
    if args.rollout_file and args.allow_local_app_server_startup:
        parser.error("local rollout mode does not start an app-server")
    if args.expected_child_id and not args.rollout_file:
        parser.error("--expected-child-id requires --rollout-file")
    if not 1 <= args.max_threads_per_state <= MAX_THREADS_PER_STATE:
        parser.error(f"--max-threads-per-state must be in 1..{MAX_THREADS_PER_STATE}")
    if not 1 <= args.timeout_seconds <= 120:
        parser.error("--timeout-seconds must be in 1..120")
    if not 1 <= args.overall_timeout_seconds <= 3600:
        parser.error("--overall-timeout-seconds must be in 1..3600")
    if (
        not args.rollout_file
        and not args.database
        and not args.allow_local_app_server_startup
    ):
        parser.error(
            "starting a local app-server is not strictly read-only; pass "
            "--allow-local-app-server-startup only if its normal state initialization is acceptable"
        )
    client = None
    try:
        rates = (
            load_ledger_standard_rates(args.database, args.standard_ledger_rates_at)
            if args.standard_ledger_rates_at
            else load_rates(args.rates_json)
        )
        if args.rollout_file:
            result = collect_local_tree(
                args.thread_id,
                args.rollout_file,
                args.expected_child_id,
                rates,
            )
            print(json.dumps(result, indent=2, sort_keys=True))
            return 0 if result["status"] == "complete" else 2
        if args.database:
            result = (
                collect_database_union(
                    args.thread_id,
                    [args.database, args.also_database],
                    rates,
                )
                if args.also_database
                else collect_database_tree(
                    args.thread_id,
                    args.database,
                    rates,
                    aggregate_only=args.aggregate_recorded,
                )
            )
            print(json.dumps(result, indent=2, sort_keys=True))
            return 0 if result["status"] == "complete" else 2
        client = AppServer(
            args.codex_bin, args.timeout_seconds, args.overall_timeout_seconds
        )
        client.initialize()
        result = collect_tree(client, args.thread_id, rates, args.max_threads_per_state)
        print(json.dumps(result, indent=2, sort_keys=True))
        return 0 if result["status"] == "complete" else 2
    except (UsageError, OSError, ValueError) as exc:
        safe = (
            exc
            if isinstance(exc, UsageError)
            else UsageError("local command or configuration error")
        )
        print(
            json.dumps(error_document(args.thread_id, safe), indent=2, sort_keys=True)
        )
        return 2
    finally:
        if client is not None:
            client.close()


if __name__ == "__main__":
    raise SystemExit(main())
