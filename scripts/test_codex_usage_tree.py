import contextlib
import io
import json
import sqlite3
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path
from unittest.mock import patch

from codex_usage_tree import (
    RpcError,
    UsageError,
    _database_compact_aggregate,
    app_server_argv,
    collect_database_tree,
    collect_database_union,
    collect_local_tree,
    collect_tree,
    error_document,
    load_ledger_standard_rates,
    load_rates,
    main,
    rpc_request_document,
)


def usage(thread_id, *, model="gpt-6.1-sol", uncached=0, cached=0, output=0):
    return {
        "threadUsage": {
            "threadId": thread_id,
            "estimatedUsageCreditsMicros": 0,
            "estimatedUsageUsdMicros": None,
            "groups": [
                {
                    "model": model,
                    "reasoningEffort": "high",
                    "speed": "standard",
                    "estimatedUsageCreditsMicros": 1_000_000,
                    "netNewInputTokens": uncached,
                    "cachedInputTokens": cached,
                    "inputTokens": None,
                    "outputTokens": output,
                    "totalTokens": None,
                }
            ],
        }
    }


class SyntheticAppServer:
    def __init__(self):
        self.calls = []
        self.usage_by_id = {
            "root": usage(
                "root", uncached=1_000_000, cached=1_000_000, output=1_000_000
            ),
            "child-a": usage("child-a", uncached=1_000_000, cached=None, output=0),
            "child-b": usage("child-b", model="gpt-unpriced", uncached=1_000_000),
            "child-c": {"threadUsage": None},
        }

    def request(self, method, params):
        self.calls.append((method, params))
        if method == "account/usage/read":
            return self.usage_by_id[params["threadId"]]
        if method == "thread/list":
            if params["archived"] is False and params.get("cursor") is None:
                return {
                    "data": [
                        {"id": "root", "parentThreadId": None},
                        {"id": "child-a", "parentThreadId": "root"},
                    ],
                    "nextCursor": "active-page-2",
                }
            if params["archived"] is False:
                return {
                    "data": [
                        {"id": "child-b", "parentThreadId": "child-a"},
                        {"id": "child-a", "parentThreadId": "root"},
                    ],
                    "nextCursor": None,
                }
            return {
                "data": [
                    {"id": "child-c", "parentThreadId": "child-a"},
                    {"id": "root", "parentThreadId": None},
                ],
                "nextCursor": None,
            }
        raise AssertionError(f"unexpected app-server method: {method}")


class SyntheticErrorAppServer:
    def __init__(self):
        self.calls = []

    def request(self, method, params):
        self.calls.append((method, params))
        raise RpcError(method, -32004)


class UsageTreeTests(unittest.TestCase):
    def _write_union_ledger(self, database, calls):
        connection = sqlite3.connect(database)
        connection.executescript(
            """
            CREATE TABLE usage_threads (
                thread_id TEXT PRIMARY KEY,
                parent_thread_id TEXT,
                root_thread_id TEXT,
                fork_parent_thread_id TEXT,
                source TEXT,
                thread_source TEXT,
                created_at TEXT NOT NULL
            );
            CREATE INDEX usage_threads_reporting_parent_idx ON usage_threads(
                COALESCE(NULLIF(parent_thread_id, ''), NULLIF(fork_parent_thread_id, ''))
            );
            CREATE TABLE usage_provider_calls (
                provider_call_id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL,
                turn_id TEXT,
                spawn_request_id TEXT,
                tool_call_id TEXT,
                request_id TEXT,
                provider TEXT,
                requested_model TEXT,
                actual_model_used TEXT,
                final_model TEXT,
                requested_service_tier TEXT,
                actual_service_tier TEXT,
                actual_service_tier_source TEXT,
                fast_mode_requested INTEGER,
                fast_mode_used INTEGER,
                billing_surface TEXT,
                account_plan TEXT,
                provider_reported_cost REAL,
                provider_reported_currency TEXT,
                provider_reported_credits REAL,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                input_tokens_uncached INTEGER,
                input_tokens_cached INTEGER,
                input_tokens_cache_write INTEGER,
                output_tokens INTEGER,
                total_tokens INTEGER,
                status TEXT
            );
            CREATE INDEX usage_provider_calls_thread_idx
                ON usage_provider_calls(thread_id);
            INSERT INTO usage_threads VALUES
                ('root', NULL, 'root', NULL, 'cli', 'user', '2026-10-01'),
                ('child', 'root', 'root', NULL,
                 '{"subagent":{"other":"worker"}}', 'subagent', '2026-10-02');
            """
        )
        connection.executemany(
            """
            INSERT INTO usage_provider_calls (
                provider_call_id, thread_id, turn_id, request_id, provider,
                requested_model, actual_model_used, final_model,
                provider_reported_credits, started_at, completed_at,
                input_tokens_uncached, input_tokens_cached,
                input_tokens_cache_write, output_tokens, total_tokens, status
            ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
            """,
            calls,
        )
        connection.commit()
        connection.close()

    def _write_legacy_union_ledger(self, database, call):
        connection = sqlite3.connect(database)
        connection.executescript(
            """
            CREATE TABLE usage_threads (
                thread_id TEXT PRIMARY KEY,
                parent_thread_id TEXT,
                root_thread_id TEXT,
                fork_parent_thread_id TEXT,
                created_at TEXT NOT NULL
            );
            CREATE INDEX usage_threads_reporting_parent_idx ON usage_threads(
                COALESCE(NULLIF(parent_thread_id, ''), NULLIF(fork_parent_thread_id, ''))
            );
            CREATE TABLE usage_provider_calls (
                provider_call_id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL,
                turn_id TEXT,
                spawn_request_id TEXT,
                tool_call_id TEXT,
                request_id TEXT,
                provider TEXT,
                requested_model TEXT,
                actual_model_used TEXT,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                input_tokens_uncached INTEGER,
                input_tokens_cached INTEGER,
                output_tokens INTEGER,
                total_tokens INTEGER,
                provider_reported_cost REAL,
                provider_reported_currency TEXT,
                status TEXT
            );
            CREATE INDEX usage_provider_calls_thread_idx
                ON usage_provider_calls(thread_id);
            INSERT INTO usage_threads VALUES ('root', NULL, 'root', NULL, '2026-10-01');
            """
        )
        connection.execute(
            """
            INSERT INTO usage_provider_calls (
                provider_call_id, thread_id, turn_id, request_id, provider,
                requested_model, actual_model_used, started_at, input_tokens_uncached,
                input_tokens_cached, output_tokens, total_tokens, status
            ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)
            """,
            call,
        )
        connection.commit()
        connection.close()

    def test_recorded_aggregate_preserves_unknowns_and_other_descendants(self):
        connection = sqlite3.connect(":memory:")
        connection.row_factory = sqlite3.Row
        connection.executescript(
            """
            CREATE TABLE usage_threads (thread_id TEXT PRIMARY KEY);
            CREATE TABLE usage_provider_calls (
                thread_id TEXT, requested_model TEXT, input_tokens_uncached INTEGER,
                input_tokens_cached INTEGER, input_tokens_cache_write INTEGER,
                output_tokens INTEGER
            );
            INSERT INTO usage_threads VALUES ('other');
            INSERT INTO usage_provider_calls VALUES
                ('other', 'gpt-6-luna', 100, NULL, 0, 1);
            """
        )
        result = _database_compact_aggregate(
            connection,
            ["other"],
            "root",
            load_rates(None),
            {
                "requested_model",
                "input_tokens_uncached",
                "input_tokens_cached",
                "input_tokens_cache_write",
                "output_tokens",
            },
            {"thread_id"},
        )
        connection.close()
        group = result["groups"][0]
        self.assertEqual(group["role"], "other_descendant")
        self.assertIsNone(group["providerEffectiveModel"])
        self.assertEqual(
            group["tokenTotals"]["input_tokens_cached"]["missingValues"], 1
        )
        self.assertIsNone(group["calculatedCreditEstimate"]["value"])
        self.assertEqual(
            group["calculatedCreditEstimate"]["knownSubtotal"], "0.0002625"
        )
        self.assertFalse(result["actualDebitObserved"])

    def test_ledger_rates_are_dated_standard_and_skip_ambiguous_models(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            database = Path(temp_dir) / "usage.sqlite"
            connection = sqlite3.connect(database)
            connection.executescript(
                """
                CREATE TABLE usage_codex_credit_rates (
                    model TEXT, provider TEXT, service_tier TEXT, speed_mode TEXT,
                    rate_card_kind TEXT, credits_per_1m_uncached_input REAL,
                    credits_per_1m_cached_input REAL, credits_per_1m_output REAL,
                    effective_from TEXT, effective_to TEXT
                );
                INSERT INTO usage_codex_credit_rates VALUES
                    ('priced', 'openai', 'default', 'standard', 'codex_token_based',
                     10, 1, 50, '2026-01-01', '2026-10-01'),
                    ('priced', 'openai', 'default', 'standard', 'codex_token_based',
                     20, 2, 100, '2026-10-01', NULL),
                    ('priced', 'openai', 'priority', 'fast', 'codex_token_based',
                     40, 4, 200, '2026-10-01', NULL),
                    ('ambiguous', 'openai', 'default', 'standard', 'codex_token_based',
                     1, 1, 1, '2026-01-01', NULL),
                    ('ambiguous', 'openai', 'default', 'standard', 'codex_token_based',
                     2, 2, 2, '2026-01-01', NULL),
                    ('image', 'openai', 'default', 'standard', 'codex_token_based_image',
                     3, 3, 3, '2026-01-01', NULL);
                """
            )
            connection.commit()
            connection.close()
            before = database.read_bytes()
            rates = load_ledger_standard_rates(str(database), "2026-10-15T00:00:00Z")
            self.assertEqual(rates["models"]["priced"]["input"], Decimal("20.0"))
            self.assertNotIn("ambiguous", rates["models"])
            self.assertNotIn("image", rates["models"])
            self.assertIn("not established", rates["source"])
            earlier = load_ledger_standard_rates(str(database), "2026-09-15T00:00:00Z")
            self.assertEqual(earlier["models"]["priced"]["input"], Decimal("10.0"))
            with self.assertRaises(UsageError):
                load_ledger_standard_rates(str(database), "2026-10-15")
            self.assertEqual(database.read_bytes(), before)

    def test_supported_cli_argv_and_wire_field_names(self):
        self.assertEqual(
            app_server_argv("codex"),
            ["codex", "app-server", "--listen", "stdio://"],
        )
        self.assertEqual(
            rpc_request_document(
                7,
                "thread/list",
                {
                    "limit": 100,
                    "sourceKinds": ["subAgentThreadSpawn"],
                    "archived": False,
                    "useStateDbOnly": True,
                    "ancestorThreadId": "root",
                },
            ),
            {
                "jsonrpc": "2.0",
                "id": 7,
                "method": "thread/list",
                "params": {
                    "limit": 100,
                    "sourceKinds": ["subAgentThreadSpawn"],
                    "archived": False,
                    "useStateDbOnly": True,
                    "ancestorThreadId": "root",
                },
            },
        )
        self.assertEqual(
            rpc_request_document(8, "account/usage/read", {"threadId": "root"}),
            {
                "jsonrpc": "2.0",
                "id": 8,
                "method": "account/usage/read",
                "params": {"threadId": "root"},
            },
        )

    def test_root_child_totals_deduplicate_and_preserve_nulls_and_missing_rates(self):
        api = SyntheticAppServer()
        result = collect_tree(api, "root", load_rates(None), max_per_state=4)

        self.assertEqual(result["status"], "partial")
        self.assertTrue(result["lineage"]["useStateDbOnly"])
        self.assertEqual(result["lineage"]["pagesRead"], 3)
        self.assertEqual(result["usage"]["requestedThreadCount"], 4)
        self.assertEqual(
            [row["threadId"] for row in result["threads"]],
            ["root", "child-a", "child-b", "child-c"],
        )
        usage_calls = [
            params["threadId"]
            for method, params in api.calls
            if method == "account/usage/read"
        ]
        self.assertEqual(usage_calls, ["root", "child-a", "child-b", "child-c"])
        list_calls = [params for method, params in api.calls if method == "thread/list"]
        self.assertTrue(
            all(params["ancestorThreadId"] == "root" for params in list_calls)
        )
        self.assertTrue(
            all(
                params["sourceKinds"] == ["subAgentThreadSpawn"]
                for params in list_calls
            )
        )
        self.assertTrue(all(params["useStateDbOnly"] for params in list_calls))
        self.assertEqual(result["lineage"]["unresolvedParentThreadIds"], [])

        root_group = result["totals"]["root"]["groups"][0]
        self.assertEqual(root_group["calculatedCredits"]["credits"], "302.5")
        self.assertEqual(root_group["tokenTotals"]["cachedInput"]["value"], 1_000_000)

        child_sol = next(
            group
            for group in result["totals"]["children"]["groups"]
            if group["model"] == "gpt-6.1-sol"
        )
        self.assertIsNone(child_sol["tokenTotals"]["cachedInput"]["value"])
        self.assertFalse(child_sol["tokenTotals"]["cachedInput"]["complete"])
        self.assertIsNone(child_sol["calculatedCredits"]["credits"])
        self.assertEqual(child_sol["calculatedCredits"]["knownSubtotal"], "50")
        self.assertEqual(
            result["totals"]["children"]["missingUsageThreadIds"], ["child-c"]
        )

        unpriced = next(
            group
            for group in result["totals"]["tree"]["groups"]
            if group["model"] == "gpt-unpriced"
        )
        self.assertIsNone(unpriced["calculatedCredits"]["credits"])
        self.assertEqual(unpriced["calculatedCredits"]["knownSubtotal"], "0")
        self.assertEqual(
            unpriced["calculatedCredits"]["missingRates"], ["gpt-unpriced:input"]
        )

    def test_read_only_database_mode_separates_root_subagent_models_and_credits(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            database = Path(temp_dir) / "usage_1.sqlite"
            connection = sqlite3.connect(database)
            connection.executescript(
                """
                CREATE TABLE usage_threads (
                    thread_id TEXT PRIMARY KEY,
                    parent_thread_id TEXT,
                    root_thread_id TEXT,
                    fork_parent_thread_id TEXT,
                    source TEXT,
                    thread_source TEXT,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX usage_threads_reporting_parent_idx ON usage_threads(
                    COALESCE(NULLIF(parent_thread_id, ''), NULLIF(fork_parent_thread_id, ''))
                );
                CREATE TABLE usage_provider_calls (
                    provider_call_id TEXT PRIMARY KEY,
                    thread_id TEXT NOT NULL,
                    turn_id TEXT,
                    request_id TEXT,
                    provider TEXT,
                    requested_model TEXT,
                    actual_model_used TEXT,
                    final_model TEXT,
                    provider_reported_credits REAL,
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    input_tokens_uncached INTEGER,
                    input_tokens_cached INTEGER,
                    input_tokens_cache_write INTEGER,
                    output_tokens INTEGER,
                    total_tokens INTEGER,
                    status TEXT
                );
                CREATE INDEX usage_provider_calls_thread_idx
                    ON usage_provider_calls(thread_id);
                INSERT INTO usage_threads VALUES
                    ('root', NULL, 'root', NULL, 'cli', 'user', '2026-10-01'),
                    ('child', 'root', 'root', NULL,
                     '{"subagent":{"other":"worker"}}', 'subagent', '2026-10-02');
                INSERT INTO usage_provider_calls VALUES
                    ('root-call', 'root', 'turn-root', 'response-root', 'openai',
                     'gpt-6.1-sol', NULL, NULL, 7.5, '2026-10-01', '2026-10-01',
                     1000000, 1000000, 0, 1000000, 3000000, 'ok'),
                    ('child-call', 'child', 'turn-child', 'response-child', 'openai',
                     'gpt-6.1-sol', 'gpt-6-sol', NULL, NULL, '2026-10-02', NULL,
                     1000000, 1000000, 5, 1000000, 3000000, 'incomplete');
                """
            )
            connection.commit()
            connection.close()
            original_bytes = database.read_bytes()

            plans = []

            class TracingConnection(sqlite3.Connection):
                def execute(self, sql, parameters=()):
                    if "WITH RECURSIVE lineage" in sql:
                        plans.extend(
                            row[3]
                            for row in super().execute(
                                "EXPLAIN QUERY PLAN " + sql, parameters
                            )
                        )
                    return super().execute(sql, parameters)

            real_connect = sqlite3.connect

            def connect_with_plan(*args, **kwargs):
                return real_connect(*args, factory=TracingConnection, **kwargs)

            with patch(
                "codex_usage_tree.sqlite3.connect", side_effect=connect_with_plan
            ):
                result = collect_database_tree("root", str(database), load_rates(None))

            self.assertTrue(
                any(
                    "SEARCH child USING INDEX usage_threads_reporting_parent_idx" in p
                    for p in plans
                ),
                plans,
            )

            self.assertEqual(result["status"], "partial")
            self.assertFalse(result["completeness"]["historicalCoverageComplete"])
            self.assertFalse(result["completeness"]["actualDebitObserved"])
            self.assertTrue(result["completeness"]["selectedLedgerRowsComplete"])
            self.assertEqual(
                [row["role"] for row in result["threads"]], ["root", "subagent"]
            )
            root_record = result["threads"][0]["responseUsageRecords"][0]
            self.assertEqual(root_record["requestedModel"], "gpt-6.1-sol")
            self.assertIsNone(root_record["providerEffectiveModel"])
            self.assertIsNone(root_record["actualDebit"]["value"])
            root_group = result["totals"]["root"]["groups"][0]
            self.assertEqual(root_group["requestedModel"], "gpt-6.1-sol")
            self.assertIsNone(root_group["providerEffectiveModel"])
            self.assertEqual(
                root_group["calculatedCreditEstimate"]["rateModelBasis"],
                "requested_model_fallback",
            )
            self.assertEqual(root_group["calculatedCreditEstimate"]["value"], "302.5")
            self.assertEqual(root_group["providerReportedCredits"]["value"], "7.5")
            self.assertIsNone(root_group["actualDebit"]["value"])
            child_group = result["totals"]["descendants"]["groups"][0]
            self.assertEqual(child_group["requestedModel"], "gpt-6.1-sol")
            self.assertEqual(child_group["providerEffectiveModel"], "gpt-6-sol")
            self.assertEqual(child_group["statusCounts"], {"incomplete": 1})
            self.assertIsNone(child_group["calculatedCreditEstimate"]["value"])
            self.assertIn(
                "cacheWriteInputTokens",
                child_group["calculatedCreditEstimate"]["missingTokenDimensions"],
            )
            with patch("codex_usage_tree.MAX_DATABASE_CALLS", 1):
                bounded = collect_database_tree(
                    "root", str(database), load_rates(None), aggregate=True
                )
            self.assertTrue(bounded["usage"]["truncated"])
            aggregate_groups = {
                group["role"]: group for group in bounded["recordedAggregate"]["groups"]
            }
            self.assertEqual(set(aggregate_groups), {"root", "subagent"})
            self.assertEqual(
                aggregate_groups["root"]["calculatedCreditEstimate"]["value"], "302.5"
            )
            self.assertIsNone(
                aggregate_groups["subagent"]["calculatedCreditEstimate"]["value"]
            )
            self.assertIn(
                "cache_write_pricing",
                aggregate_groups["subagent"]["calculatedCreditEstimate"][
                    "missingTokenDimensions"
                ],
            )
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                exit_code = main(["--thread-id", "root", "--database", str(database)])
            self.assertEqual(exit_code, 2)
            self.assertEqual(json.loads(output.getvalue())["status"], "partial")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                exit_code = main(
                    [
                        "--thread-id",
                        "root",
                        "--database",
                        str(database),
                        "--aggregate-recorded",
                    ]
                )
            aggregate_only = json.loads(output.getvalue())
            self.assertEqual(exit_code, 2)
            self.assertNotIn("threads", aggregate_only)
            self.assertEqual(len(aggregate_only["recordedAggregate"]["groups"]), 2)

            class InterruptedConnection(sqlite3.Connection):
                def set_progress_handler(self, callback, count):
                    return super().set_progress_handler(
                        (lambda: 1) if callback is not None else None, 1
                    )

            def connect_interrupted(*args, **kwargs):
                return real_connect(*args, factory=InterruptedConnection, **kwargs)

            with patch(
                "codex_usage_tree.sqlite3.connect", side_effect=connect_interrupted
            ):
                with self.assertRaisesRegex(UsageError, "exceeded its time limit"):
                    collect_database_tree("root", str(database), load_rates(None))
            self.assertEqual(database.read_bytes(), original_bytes)

    def test_cross_ledger_union_deduplicates_and_withholds_unresolved_rows(self):
        def call(call_id, thread, request, uncached, credits, *, status="ok"):
            return (
                call_id,
                thread,
                f"turn-{request}",
                request,
                "openai",
                "gpt-6.1-sol",
                "gpt-6-sol",
                None,
                credits,
                "2026-10-0{}T00:00:00Z".format(1 if thread == "root" else 2),
                None,
                uncached,
                10,
                0,
                5,
                uncached + 15,
                status,
            )

        with tempfile.TemporaryDirectory() as temp_dir:
            primary = Path(temp_dir) / "primary.sqlite"
            comparison = Path(temp_dir) / "comparison.sqlite"
            self._write_union_ledger(
                primary,
                [
                    call("pcid-secret-shared", "root", "shared-request", 100, 1.25),
                    call("pcid-secret-conflict", "child", "conflict-primary", 20, 0.5),
                    call(None, "root", "unkeyed-primary", 30, 0.25),
                ],
            )
            self._write_union_ledger(
                comparison,
                [
                    call("pcid-secret-shared", "root", "shared-request", 100, 1.25),
                    call("pcid-secret-conflict", "child", "conflict-comparison", 200, 9.5),
                    call("pcid-secret-secondary", "child", "secondary-request", 40, 0.75),
                    call(None, "root", "unkeyed-comparison", 50, 0.125),
                ],
            )
            original_primary = primary.read_bytes()
            original_comparison = comparison.read_bytes()

            result = collect_database_union(
                "root", [str(primary), str(comparison)], load_rates(None)
            )

            dedupe = result["crossLedgerDeduplication"]
            self.assertEqual(result["status"], "partial")
            self.assertFalse(result["completeness"]["historicalCoverageComplete"])
            self.assertTrue(result["completeness"]["unresolvedRowsExcludedFromMergedTotals"])
            self.assertFalse(
                result["pricingCoverage"]["effectiveDatedPerCallRateMatchPerformed"]
            )
            self.assertGreater(
                result["pricingCoverage"]["missingContextRowsObservedBySource"][0][
                    "missing"
                ]["actual_service_tier"],
                0,
            )
            self.assertEqual(dedupe["status"], "complete_for_selected_rows")
            self.assertEqual(dedupe["rowsScanned"], 7)
            self.assertEqual(dedupe["exactDuplicateRowsOmittedObserved"], 1)
            self.assertEqual(dedupe["conflictingProviderCallIdsObserved"], 1)
            self.assertEqual(dedupe["conflictingRowsWithheldObserved"], 2)
            self.assertEqual(dedupe["unkeyedRowsWithheldObserved"], 2)
            merged = dedupe["mergedTotals"]
            self.assertEqual(merged["status"], "partial_excluding_unresolved")
            self.assertEqual(merged["responseUsageRecordCount"], 2)
            self.assertEqual(merged["excludedUnresolvedRows"], 4)
            self.assertEqual(
                sum(
                    group["tokenTotals"]["inputTokensUncached"]["knownSubtotal"]
                    for group in merged["groups"]
                ),
                140,
            )
            self.assertEqual(
                sum(
                    (
                        Decimal(group["providerReportedCredits"]["knownSubtotal"])
                        for group in merged["groups"]
                    ),
                    start=Decimal(0),
                ),
                Decimal("2"),
            )
            self.assertEqual(
                {group["role"] for group in merged["groups"]}, {"root", "subagent"}
            )
            self.assertTrue(
                all(
                    group["tokenTotals"]["inputTokensUncached"]["value"] is None
                    for group in merged["groups"]
                )
            )
            self.assertTrue(
                all(
                    group["calculatedCreditEstimate"]["estimateType"]
                    == "model_rate_scenario_not_verified_billing"
                    and Decimal(
                        group["calculatedCreditEstimate"]["knownSubtotal"]
                    )
                    > 0
                    for group in merged["groups"]
                )
            )
            self.assertTrue(
                all(
                    group["actualDebit"]["status"] == "not_observed"
                    for group in merged["groups"]
                )
            )
            self.assertEqual(len(result["sources"]), 2)
            self.assertTrue(
                all(
                    source["recordedAggregate"]["groups"]
                    for source in result["sources"]
                )
            )
            self.assertTrue(
                all(source["rowScan"]["complete"] for source in result["sources"])
            )
            serialized = json.dumps(result)
            self.assertNotIn("pcid-secret", serialized)
            self.assertNotIn(str(primary), serialized)
            self.assertNotIn(str(comparison), serialized)
            self.assertEqual(primary.read_bytes(), original_primary)
            self.assertEqual(comparison.read_bytes(), original_comparison)

            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                exit_code = main(
                    [
                        "--thread-id",
                        "root",
                        "--database",
                        str(primary),
                        "--also-database",
                        str(comparison),
                        "--aggregate-recorded",
                    ]
                )
            self.assertEqual(exit_code, 2)
            self.assertIn("crossLedgerDeduplication", json.loads(output.getvalue()))

    def test_cross_ledger_bounds_withhold_merged_totals_but_keep_source_summaries(self):
        def call(call_id, uncached):
            return (
                call_id,
                "root",
                "turn-" + call_id,
                "request-" + call_id,
                "openai",
                "gpt-6.1-sol",
                None,
                None,
                None,
                "2026-10-01T00:00:00Z",
                None,
                uncached,
                0,
                0,
                0,
                uncached,
                "ok",
            )

        with tempfile.TemporaryDirectory() as temp_dir:
            primary = Path(temp_dir) / "primary.sqlite"
            comparison = Path(temp_dir) / "comparison.sqlite"
            self._write_union_ledger(primary, [call("a1", 10), call("a2", 20)])
            self._write_union_ledger(comparison, [call("b1", 30), call("b2", 40)])
            with patch("codex_usage_tree.MAX_DATABASE_UNION_CALLS_PER_SOURCE", 1), patch(
                "codex_usage_tree.MAX_DATABASE_UNION_CALLS_TOTAL", 2
            ):
                result = collect_database_union(
                    "root", [str(primary), str(comparison)], load_rates(None)
                )

        dedupe = result["crossLedgerDeduplication"]
        self.assertEqual(dedupe["status"], "incomplete")
        self.assertEqual(
            dedupe["mergedTotals"]["status"], "unavailable_incomplete_source_scan"
        )
        self.assertEqual(dedupe["mergedTotals"]["groups"], [])
        self.assertTrue(
            all(source["recordedAggregate"]["groups"] for source in result["sources"])
        )
        self.assertTrue(all(source["rowScan"]["truncated"] for source in result["sources"]))

    def test_legacy_schema_duplicate_is_unverifiable_not_silently_deduplicated(self):
        current_call = (
            "pcid-secret-legacy",
            "root",
            "turn-legacy",
            "request-legacy",
            "openai",
            "gpt-6.1-sol",
            "gpt-6-sol",
            None,
            1.0,
            "2026-10-01T00:00:00Z",
            None,
            10,
            0,
            0,
            5,
            15,
            "ok",
        )
        legacy_call = (
            "pcid-secret-legacy",
            "root",
            "turn-legacy",
            "request-legacy",
            "openai",
            "gpt-6.1-sol",
            "gpt-6-sol",
            "2026-10-01T00:00:00Z",
            10,
            0,
            5,
            15,
            "ok",
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            current = Path(temp_dir) / "current.sqlite"
            legacy = Path(temp_dir) / "legacy.sqlite"
            self._write_union_ledger(current, [current_call])
            self._write_legacy_union_ledger(legacy, legacy_call)

            result = collect_database_union(
                "root", [str(current), str(legacy)], load_rates(None)
            )

        dedupe = result["crossLedgerDeduplication"]
        self.assertEqual(dedupe["exactDuplicateRowsOmittedObserved"], 0)
        self.assertEqual(dedupe["conflictingProviderCallIdsObserved"], 0)
        self.assertEqual(dedupe["schemaUnverifiableProviderCallIdsObserved"], 1)
        self.assertEqual(dedupe["schemaUnverifiableRowsWithheldObserved"], 2)
        self.assertEqual(dedupe["mergedTotals"]["responseUsageRecordCount"], 0)
        self.assertFalse(
            result["sources"][1]["columnCoverage"]["usage_provider_calls"][
                "input_tokens_cache_write"
            ]
        )

    def test_cross_ledger_memory_bound_withholds_merge_without_losing_source_summary(self):
        call = (
            "a1",
            "root",
            "turn-a1",
            "request-a1",
            "openai",
            "gpt-6.1-sol",
            None,
            None,
            None,
            "2026-10-01T00:00:00Z",
            None,
            10,
            0,
            0,
            0,
            10,
            "ok",
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            primary = Path(temp_dir) / "primary.sqlite"
            comparison = Path(temp_dir) / "comparison.sqlite"
            self._write_union_ledger(primary, [call])
            self._write_union_ledger(comparison, [call])
            with patch("codex_usage_tree.MAX_DATABASE_UNION_MEMORY_BYTES", 1):
                result = collect_database_union(
                    "root", [str(primary), str(comparison)], load_rates(None)
                )

        dedupe = result["crossLedgerDeduplication"]
        self.assertEqual(dedupe["status"], "incomplete")
        self.assertEqual(
            dedupe["mergedTotals"]["status"], "unavailable_incomplete_source_scan"
        )
        self.assertEqual(result["sources"][0]["rowScan"]["reason"], "memory_limit")
        self.assertTrue(result["sources"][0]["recordedAggregate"]["groups"])

    def test_database_mode_fails_closed_without_thread_index(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            database = Path(temp_dir) / "usage.sqlite"
            self._write_union_ledger(
                database,
                [
                    (
                        "call",
                        "root",
                        "turn",
                        "request",
                        "openai",
                        "gpt-6.1-sol",
                        None,
                        None,
                        None,
                        "2026-10-01T00:00:00Z",
                        None,
                        10,
                        0,
                        0,
                        5,
                        15,
                        "ok",
                    )
                ],
            )
            connection = sqlite3.connect(database)
            connection.execute("DROP INDEX usage_provider_calls_thread_idx")
            connection.commit()
            connection.close()

            with self.assertRaisesRegex(UsageError, "indexed exact-thread"):
                collect_database_tree(
                    "root", str(database), load_rates(None), aggregate_only=True
                )

    def test_a_list_cap_marks_lineage_incomplete_instead_of_claiming_full_tree(self):
        class CappedAppServer(SyntheticAppServer):
            def request(self, method, params):
                self.calls.append((method, params))
                if method == "account/usage/read":
                    return self.usage_by_id[params["threadId"]]
                if method == "thread/list" and params["archived"] is False:
                    return {
                        "data": [{"id": "child-a", "parentThreadId": "root"}],
                        "nextCursor": "more-active-threads",
                    }
                if method == "thread/list":
                    return {"data": [], "nextCursor": None}
                raise AssertionError(f"unexpected app-server method: {method}")

        result = collect_tree(
            CappedAppServer(), "root", load_rates(None), max_per_state=1
        )

        self.assertEqual(result["status"], "partial")
        self.assertFalse(result["completeness"]["lineageComplete"])
        self.assertEqual(result["lineage"]["truncatedArchivedStates"], [False])

    def test_native_account_usage_error_is_not_reported_as_zero(self):
        api = SyntheticErrorAppServer()
        with self.assertRaises(RpcError) as caught:
            collect_tree(api, "root", load_rates(None), max_per_state=4)
        self.assertEqual(caught.exception.method, "account/usage/read")
        self.assertEqual([method for method, _ in api.calls], ["account/usage/read"])
        failure = error_document("root", caught.exception)
        self.assertEqual(failure["status"], "unavailable")
        self.assertEqual(failure["failure"]["method"], "account/usage/read")

    @staticmethod
    def write_rollout(path, thread_id, parent_thread_id, contexts, records):
        events = [
            {
                "type": "session_meta",
                "payload": {"id": thread_id, "parent_thread_id": parent_thread_id},
            }
        ]
        events.extend(
            {
                "type": "turn_context",
                "payload": {"turn_id": turn_id, "model": model, "effort": effort},
            }
            for turn_id, model, effort in contexts
        )
        events.extend(
            {
                "type": "token_usage_record",
                "payload": {
                    "thread_id": thread_id,
                    "turn_id": turn_id,
                    "root_turn_id": root_turn_id,
                    "response_id": response_id,
                    "usage": {
                        "input_tokens": input_tokens,
                        "cached_input_tokens": cached_tokens,
                        "cache_write_input_tokens": cache_write_tokens,
                        "output_tokens": output_tokens,
                    },
                },
            }
            for (
                turn_id,
                root_turn_id,
                response_id,
                input_tokens,
                cached_tokens,
                cache_write_tokens,
                output_tokens,
            ) in records
        )
        path.write_text("".join(json.dumps(event) + "\n" for event in events))

    def test_local_rollouts_join_models_and_sum_root_child_response_records(self):
        with tempfile.TemporaryDirectory() as directory:
            root_path = Path(directory) / "root.jsonl"
            child_path = Path(directory) / "child.jsonl"
            self.write_rollout(
                root_path,
                "root",
                None,
                [("root-turn", "gpt-6-luna", "xhigh")],
                [
                    ("root-turn", "root-turn", "root-r1", 100, 40, 0, 20),
                    ("root-turn", "root-turn", "root-r2", 50, 10, 0, 10),
                ],
            )
            self.write_rollout(
                child_path,
                "child",
                "root",
                [("child-turn", "gpt-6-luna", "medium")],
                [("child-turn", "root-turn", "child-r1", 200, 60, 0, 20)],
            )

            result = collect_local_tree(
                "root", [str(root_path), str(child_path)], ["child"], load_rates(None)
            )

        self.assertEqual(result["status"], "partial")
        self.assertTrue(result["lineage"]["complete"])
        self.assertFalse(result["completeness"]["usageComplete"])
        self.assertEqual(result["usage"]["responseUsageRecordCount"], 3)
        self.assertEqual(
            [row["threadId"] for row in result["threads"]], ["root", "child"]
        )
        root_group = result["totals"]["root"]["groups"][0]
        child_group = result["totals"]["children"]["groups"][0]
        self.assertEqual(
            root_group["tokenTotals"]["uncachedInput"]["knownSubtotal"], 100
        )
        self.assertEqual(root_group["tokenTotals"]["cachedInput"]["value"], 50)
        self.assertEqual(root_group["tokenTotals"]["output"]["value"], 30)
        self.assertEqual(root_group["tokenTotals"]["cacheWriteInput"]["value"], 0)
        self.assertEqual(child_group["tokenTotals"]["uncachedInput"]["value"], 140)
        tree_groups = result["totals"]["tree"]["groups"]
        self.assertEqual(
            sum(
                group["tokenTotals"]["uncachedInput"]["knownSubtotal"]
                for group in tree_groups
            ),
            240,
        )
        self.assertEqual(
            sum(
                group["tokenTotals"]["cachedInput"]["knownSubtotal"]
                for group in tree_groups
            ),
            110,
        )
        self.assertEqual(
            sum(
                group["tokenTotals"]["output"]["knownSubtotal"] for group in tree_groups
            ),
            50,
        )
        self.assertEqual(
            sum(
                (
                    Decimal(group["calculatedCredits"]["credits"])
                    for group in tree_groups
                ),
                start=Decimal(0),
            ),
            Decimal("0.0012525"),
        )
        self.assertEqual(root_group["calculatedCredits"]["credits"], "0.0006375")
        self.assertEqual(child_group["calculatedCredits"]["credits"], "0.000615")

    def test_local_rollout_missing_file_and_cache_write_remain_incomplete(self):
        with tempfile.TemporaryDirectory() as directory:
            root_path = Path(directory) / "root.jsonl"
            self.write_rollout(
                root_path,
                "root",
                None,
                [("root-turn", "gpt-6-luna", "high")],
                [("root-turn", "root-turn", "root-r1", 20, 8, 2, 3)],
            )
            result = collect_local_tree(
                "root", [str(root_path)], ["missing-child"], load_rates(None)
            )

        missing = result["threads"][1]
        self.assertEqual(missing["usageStatus"], "rollout_not_supplied")
        self.assertIsNone(missing["usage"])
        self.assertFalse(result["lineage"]["complete"])
        self.assertFalse(result["completeness"]["modelContextComplete"])
        group = result["totals"]["root"]["groups"][0]
        self.assertIsNone(group["tokenTotals"]["uncachedInput"]["value"])
        self.assertEqual(group["tokenTotals"]["uncachedInput"]["knownSubtotal"], 0)
        self.assertEqual(group["tokenTotals"]["cacheWriteInput"]["value"], 2)
        self.assertIsNone(group["calculatedCredits"]["credits"])
        self.assertIn(
            "cacheWriteInputTokens",
            group["calculatedCredits"]["missingTokenDimensions"],
        )

    def test_local_null_token_fields_remain_null(self):
        with tempfile.TemporaryDirectory() as directory:
            root_path = Path(directory) / "root.jsonl"
            self.write_rollout(
                root_path,
                "root",
                None,
                [("root-turn", "gpt-6-luna", "high")],
                [("root-turn", "root-turn", "root-r1", None, 8, 0, 3)],
            )
            result = collect_local_tree("root", [str(root_path)], [], load_rates(None))

        group = result["totals"]["root"]["groups"][0]
        self.assertIsNone(group["tokenTotals"]["uncachedInput"]["value"])
        self.assertFalse(group["tokenTotals"]["uncachedInput"]["complete"])
        self.assertEqual(group["tokenTotals"]["cachedInput"]["value"], 8)
        self.assertEqual(group["tokenTotals"]["output"]["value"], 3)
        self.assertIsNone(group["calculatedCredits"]["credits"])

    def test_local_rollout_cli_does_not_start_app_server(self):
        with tempfile.TemporaryDirectory() as directory:
            root_path = Path(directory) / "root.jsonl"
            self.write_rollout(
                root_path,
                "root",
                None,
                [("root-turn", "gpt-6-luna", "high")],
                [("root-turn", "root-turn", "root-r1", 20, 8, 0, 3)],
            )
            output = io.StringIO()
            with (
                patch("codex_usage_tree.AppServer", side_effect=AssertionError),
                contextlib.redirect_stdout(output),
            ):
                exit_code = main(
                    ["--thread-id", "root", "--rollout-file", str(root_path)]
                )

        self.assertEqual(
            exit_code, 2
        )  # Observed TokenUsageRecord rows are best effort.
        report = json.loads(output.getvalue())
        self.assertEqual(
            report["usage"]["source"], "local TokenUsageRecord rollout items"
        )
        self.assertFalse(report["completeness"]["usageComplete"])


if __name__ == "__main__":
    unittest.main()
