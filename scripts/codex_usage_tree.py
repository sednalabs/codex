#!/usr/bin/env python3
"""Query a Codex thread tree through app-server or explicitly named local rollouts."""

from __future__ import annotations

import argparse
import json
import queue
import subprocess
import threading
import time
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any

PAGE_SIZE = 100
DEFAULT_MAX_THREADS_PER_STATE = 100
MAX_THREADS_PER_STATE = 500
MAX_LOCAL_ROLLOUT_BYTES = 10 * 1024 * 1024
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
        "--max-threads-per-state", type=int, default=DEFAULT_MAX_THREADS_PER_STATE
    )
    parser.add_argument("--timeout-seconds", type=int, default=20)
    parser.add_argument("--overall-timeout-seconds", type=int, default=300)
    args = parser.parse_args(argv)
    if not args.thread_id.strip():
        parser.error("--thread-id cannot be empty")
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
    if not args.rollout_file and not args.allow_local_app_server_startup:
        parser.error(
            "starting a local app-server is not strictly read-only; pass "
            "--allow-local-app-server-startup only if its normal state initialization is acceptable"
        )
    client = None
    try:
        rates = load_rates(args.rates_json)
        if args.rollout_file:
            result = collect_local_tree(
                args.thread_id,
                args.rollout_file,
                args.expected_child_id,
                rates,
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
