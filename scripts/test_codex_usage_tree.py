import contextlib
import io
import json
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path
from unittest.mock import patch

from codex_usage_tree import (
    RpcError,
    app_server_argv,
    collect_local_tree,
    collect_tree,
    error_document,
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
                group["tokenTotals"]["output"]["knownSubtotal"]
                for group in tree_groups
            ),
            50,
        )
        self.assertEqual(
            sum(
                (Decimal(group["calculatedCredits"]["credits"]) for group in tree_groups),
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
            with patch(
                "codex_usage_tree.AppServer", side_effect=AssertionError
            ), contextlib.redirect_stdout(output):
                exit_code = main(
                    ["--thread-id", "root", "--rollout-file", str(root_path)]
                )

        self.assertEqual(exit_code, 2)  # Observed TokenUsageRecord rows are best effort.
        report = json.loads(output.getvalue())
        self.assertEqual(report["usage"]["source"], "local TokenUsageRecord rollout items")
        self.assertFalse(report["completeness"]["usageComplete"])


if __name__ == "__main__":
    unittest.main()
