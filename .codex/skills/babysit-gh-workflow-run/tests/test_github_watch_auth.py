import importlib.util
import os
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch


MODULE_PATH = Path(__file__).resolve().parents[2] / "github_watch_auth.py"
SPEC = importlib.util.spec_from_file_location("github_watch_auth", MODULE_PATH)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class AuthHelperTests(unittest.TestCase):
    def test_helper_preferred_and_repository_context_passed(self):
        state = MODULE.AuthState()
        with patch.dict(os.environ, {MODULE.TOKEN_COMMAND_ENV: "token-helper", "GH_TOKEN": "oauth"}, clear=False), patch.object(
            MODULE.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "app-token\n", "")
        ) as run:
            env = {"GH_TOKEN": "oauth"}
            self.assertTrue(state.apply(env, repo="sednalabs/codex"))
            self.assertEqual(env["GH_TOKEN"], "app-token")
            self.assertEqual(run.call_args.kwargs["env"][MODULE.REPOSITORY_ENV], "sednalabs/codex")

    def test_configured_helper_failure_fails_closed(self):
        state = MODULE.AuthState()
        with patch.dict(os.environ, {MODULE.TOKEN_COMMAND_ENV: "token-helper"}, clear=False), patch.object(
            MODULE.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "secret", "error")
        ):
            with self.assertRaises(RuntimeError):
                state.apply({}, repo="sednalabs/codex")

    def test_absent_helper_preserves_existing_fallback(self):
        state = MODULE.AuthState()
        with patch.dict(os.environ, {}, clear=True):
            env = {"GITHUB_TOKEN": "oauth"}
            self.assertFalse(state.apply(env))
            self.assertEqual(env["GITHUB_TOKEN"], "oauth")
            self.assertEqual(state.source, "GITHUB_TOKEN")

    def test_refresh_is_single_attempt_and_redaction_is_safe(self):
        state = MODULE.AuthState()
        with patch.dict(os.environ, {MODULE.TOKEN_COMMAND_ENV: "token-helper"}, clear=False), patch.object(
            MODULE.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "new-token", "")
        ) as run:
            env = {}
            self.assertTrue(state.refresh(env))
            self.assertFalse(state.refresh(env))
            self.assertEqual(run.call_count, 1)
            self.assertEqual(MODULE.redact("new-token failed", ("new-token",)), "<redacted> failed")

    def test_rate_reset_wait_is_bounded_and_once(self):
        state = MODULE.AuthState()
        clock = [1_000]
        with patch.object(MODULE, "read_rate_limit_reset", return_value=1_030), patch.object(
            MODULE.time, "time", side_effect=lambda: clock[0]
        ), patch.object(
            MODULE.time, "monotonic", side_effect=lambda: clock[0]
        ):
            sleeps = []
            def sleep(seconds):
                sleeps.append(seconds)
                clock[0] += seconds
            self.assertTrue(MODULE.wait_for_reset(state, {}, "API rate limit exceeded", sleep=sleep))
            self.assertFalse(MODULE.wait_for_reset(state, {}, "API rate limit exceeded", sleep=sleep))
            self.assertGreater(len(sleeps), 1)
            self.assertLessEqual(sleeps[0], MODULE.MAX_RESET_SLEEP_SECONDS)

    def test_resource_selection_never_substitutes_core(self):
        self.assertEqual(MODULE.rate_resource(["api", "graphql"]), "graphql")
        self.assertEqual(MODULE.rate_resource(["api", "repos/x/search/issues"]), "search")
        self.assertEqual(MODULE.rate_resource(["api", "search/issues"]), "search")
        self.assertEqual(MODULE.rate_resource(["api", "repos/x/actions/runs"]), "core")
        self.assertTrue(MODULE.is_retry_safe(["api", "rate_limit"]))
        self.assertFalse(MODULE.is_retry_safe(["api", "repos/x/issues", "--method", "POST"]))
        self.assertFalse(MODULE.is_retry_safe(["api", "repos/x/issues", "-X", "POST"]))
        self.assertFalse(MODULE.is_retry_safe(["api", "repos/x/issues", "-f", "title=x"]))
        self.assertTrue(MODULE.is_retry_safe(["api", "graphql", "-f", "query=query { viewer { login } }"]))
        self.assertFalse(MODULE.is_retry_safe(["api", "graphql", "-f", "query=mutation { x } "]))

    def test_secondary_limit_without_evidence_does_not_query_or_wait(self):
        state = MODULE.AuthState()
        with patch.object(MODULE, "read_rate_limit_reset", side_effect=AssertionError("must not query")):
            sleeps = []
            self.assertFalse(MODULE.wait_for_reset(state, {}, "secondary rate limit", sleep=sleeps.append))
            self.assertEqual(sleeps, [])

    def test_retry_after_is_authoritative_and_budgeted(self):
        state = MODULE.AuthState()
        state.deadline = 1_010
        clock = [1_000]
        with patch.object(MODULE.time, "time", side_effect=lambda: clock[0]), patch.object(
            MODULE.time, "monotonic", side_effect=lambda: clock[0]
        ):
            sleeps = []
            def sleep(seconds):
                sleeps.append(seconds)
                clock[0] += seconds
            self.assertTrue(MODULE.wait_for_reset(state, {}, "secondary rate limit Retry-After: 5", sleep=sleep))
            self.assertEqual(sum(sleeps), 6)

    def test_expired_monotonic_budget_does_not_sleep_or_retry(self):
        state = MODULE.AuthState()
        state.deadline = 99
        with patch.object(MODULE.time, "monotonic", return_value=100), patch.object(
            MODULE, "read_rate_limit_reset", return_value=200
        ):
            sleeps = []
            self.assertFalse(MODULE.wait_for_reset(state, {}, "API rate limit exceeded", sleep=sleeps.append))
            self.assertEqual(sleeps, [])

    def test_reset_beyond_budget_sleeps_to_budget_then_returns_false(self):
        state = MODULE.AuthState()
        state.deadline = 1_005
        clock = [1_000]
        with patch.object(MODULE, "read_rate_limit_reset", return_value=1_030), patch.object(
            MODULE.time, "time", side_effect=lambda: clock[0]
        ), patch.object(MODULE.time, "monotonic", side_effect=lambda: clock[0]):
            sleeps = []
            def sleep(seconds):
                sleeps.append(seconds)
                clock[0] += seconds
            self.assertFalse(MODULE.wait_for_reset(state, {}, "API rate limit exceeded", sleep=sleep))
            self.assertEqual(sum(sleeps), 5)
            self.assertIsNone(state._slept_reset)

    def test_rate_detection_does_not_treat_generic_403_as_quota(self):
        self.assertFalse(MODULE.is_rate_limited("HTTP 403 forbidden"))
        self.assertTrue(MODULE.is_rate_limited("API rate limit exceeded"))


if __name__ == "__main__":
    unittest.main()
