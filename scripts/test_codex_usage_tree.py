import unittest

from codex_usage_tree import (
    RpcError,
    app_server_argv,
    collect_tree,
    error_document,
    load_rates,
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


if __name__ == "__main__":
    unittest.main()
