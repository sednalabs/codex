import importlib.util
import io
import json
import os
import sys
import types
import unittest
from pathlib import Path
from unittest.mock import Mock, patch


MODULE_PATH = Path(
    os.environ.get(
        "GH_DISPATCH_AND_WATCH_MODULE_PATH",
        str(Path(__file__).resolve().parents[1] / "scripts" / "gh_dispatch_and_watch.py"),
    )
)
SPEC = importlib.util.spec_from_file_location("gh_dispatch_and_watch", MODULE_PATH)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class DispatchAndWatchTests(unittest.TestCase):
    def test_parse_dispatch_inputs_accepts_key_value_pairs(self):
        parsed = MODULE._parse_dispatch_inputs(["profile=frontier", "lane_set=smoke,core"])
        self.assertEqual(parsed, [("profile", "frontier"), ("lane_set", "smoke,core")])

    def test_parse_dispatch_inputs_rejects_missing_equals(self):
        with self.assertRaises(ValueError):
            MODULE._parse_dispatch_inputs(["profile"])

    def test_parse_wait_until_terminal_alias(self):
        argv = [
            "gh_dispatch_and_watch",
            "--workflow",
            "validation-lab.yml",
            "--ref",
            "main",
            "--head-sha",
            "deadbeef",
            "--wait-until-terminal",
        ]
        with patch.object(sys, "argv", argv):
            parsed = MODULE.parse_args()
        self.assertTrue(parsed.watch_until_terminal)

    def test_parse_legacy_no_gemini_flag_as_safe_no_op(self):
        argv = [
            "gh_dispatch_and_watch",
            "--workflow",
            "validation-lab.yml",
            "--no-gemini-diagnosis",
        ]
        with patch.object(sys, "argv", argv):
            parsed = MODULE.parse_args()
        self.assertTrue(parsed.no_gemini_diagnosis)

    def test_dispatch_workflow_passes_supersession_and_custom_inputs(self):
        captured = {}

        class FakeWatcher:
            def gh_text(self, args, repo=None):
                captured["args"] = list(args)
                captured["repo"] = repo
                return ""

        MODULE._dispatch_workflow(
            FakeWatcher(),
            "owner/repo",
            "validation-lab.yml",
            "feature/branch",
            "auto",
            "checkpoint-a",
            [("profile", "frontier"), ("lane_set", "smoke,core")],
        )

        self.assertEqual(captured["repo"], "owner/repo")
        self.assertEqual(captured["args"][:5], ["workflow", "run", "validation-lab.yml", "--ref", "feature/branch"])
        self.assertIn("supersession_mode=auto", captured["args"])
        self.assertIn("supersession_key=checkpoint-a", captured["args"])
        self.assertIn("profile=frontier", captured["args"])
        self.assertIn("lane_set=smoke,core", captured["args"])

    def test_dispatch_workflow_retries_without_supersession_inputs_when_rejected(self):
        calls = []

        class FakeWatcher:
            def gh_text(self, args, repo=None):
                calls.append((list(args), repo))
                if len(calls) == 1:
                    raise RuntimeError(
                        'GitHub CLI command failed: gh ...\nstderr: could not create workflow dispatch '
                        'event: HTTP 422: Unexpected inputs provided: ["supersession_mode"]'
                    )
                return ""

        MODULE._dispatch_workflow(
            FakeWatcher(),
            "owner/repo",
            "rust-ci-full.yml",
            "feature/branch",
            "auto",
            "",
            [("profile", "frontier")],
        )

        self.assertEqual(len(calls), 2)
        self.assertIn("supersession_mode=auto", calls[0][0])
        self.assertNotIn("supersession_mode=auto", calls[1][0])
        self.assertIn("profile=frontier", calls[1][0])

    def test_emit_expected_head_sha_mismatch_outputs_structured_action(self):
        buffer = io.StringIO()
        with patch("sys.stdout", buffer):
            rc = MODULE._emit_expected_head_sha_mismatch(
                workflow="validation-lab.yml",
                ref="feature/branch",
                expected_sha="deadbeef",
                observed_sha="eedacefde09014c4897744aba5674ebb7c5b2305",
            )
        self.assertEqual(rc, 1)
        payload = json.loads(buffer.getvalue().strip())
        self.assertEqual(payload["actions"], ["stop_expected_head_sha_mismatch"])
        self.assertEqual(payload["expected_head_sha_mismatch"]["expected_head_sha"], "deadbeef")
        self.assertEqual(payload["expected_head_sha_mismatch"]["observed_head_sha"], "eedacefde09014c4897744aba5674ebb7c5b2305")

    def test_resolve_remote_ref_sha_prefers_remote_head_for_branch_refs(self):
        calls = []

        class FakeWatcher:
            def is_sha_like(self, value):
                if not value:
                    return False
                value = str(value)
                return all(ch in "0123456789abcdefABCDEF" for ch in value) and len(value) >= 7

            def gh_json(self, args, repo=None):
                calls.append(list(args))
                return {"object": {"sha": "abcdef1234567890"}}

        resolved = MODULE._resolve_remote_ref_sha(
            FakeWatcher(), "owner/repo", "feature/branch"
        )
        self.assertEqual(calls, [["api", "/repos/owner/repo/git/ref/heads/feature%2Fbranch", "--hostname", "github.com"]])
        self.assertEqual(resolved, "abcdef1234567890")

    def test_validate_expected_head_sha_against_remote_branch_detects_mismatch(self):
        class FakeWatcher:
            def is_sha_like(self, value):
                if not value:
                    return False
                value = str(value)
                return all(ch in "0123456789abcdefABCDEF" for ch in value) and len(value) >= 7

            def gh_json(self, args, repo=None):
                return {"object": {"sha": "eedacefde09014c4897744aba5674ebb7c5b2305"}}

        mismatch = MODULE._validate_expected_head_sha_against_remote_branch(
            FakeWatcher(), "owner/repo", "feature/branch", "badcafe"
        )
        self.assertEqual(mismatch, "eedacefde09014c4897744aba5674ebb7c5b2305")

    def test_validate_expected_head_sha_against_remote_branch_accepts_short_matching_prefix(self):
        class FakeWatcher:
            def is_sha_like(self, value):
                if not value:
                    return False
                value = str(value)
                return all(ch in "0123456789abcdefABCDEF" for ch in value) and len(value) >= 7

            def gh_json(self, args, repo=None):
                return {"object": {"sha": "9f95361ef183d194ffcba7c376b3e298d6e49ead"}}

        mismatch = MODULE._validate_expected_head_sha_against_remote_branch(
            FakeWatcher(), "owner/repo", "feature/branch", "9f953"
        )
        self.assertIsNone(mismatch)

    def test_is_head_sha_prefix_allows_short_hex(self):
        self.assertTrue(MODULE._is_head_sha_prefix("9f953"))

    def test_is_head_sha_prefix_rejects_short_or_nonhex(self):
        self.assertFalse(MODULE._is_head_sha_prefix("abc"))
        self.assertFalse(MODULE._is_head_sha_prefix("zzzzz"))
        self.assertFalse(MODULE._is_head_sha_prefix(""))

    def test_validate_expected_head_sha_against_remote_branch_skips_when_unknown(self):
        class FakeWatcher:
            def is_sha_like(self, value):
                if not value:
                    return False
                value = str(value)
                return all(ch in "0123456789abcdefABCDEF" for ch in value) and len(value) >= 7

            def gh_json(self, args, repo=None):
                raise RuntimeError("no remote ref")

        mismatch = MODULE._validate_expected_head_sha_against_remote_branch(
            FakeWatcher(), "owner/repo", "feature/branch", "deadbeef"
        )
        self.assertIsNone(mismatch)

    def test_validate_expected_head_sha_against_remote_branch_skips_when_expected_sha_missing(self):
        class FakeWatcher:
            def is_sha_like(self, value):
                if not value:
                    return False
                value = str(value)
                return all(ch in "0123456789abcdefABCDEF" for ch in value) and len(value) >= 7

            def gh_json(self, args, repo=None):
                return {"object": {"sha": "eedacefde09014c4897744aba5674ebb7c5b2305"}}

        mismatch = MODULE._validate_expected_head_sha_against_remote_branch(
            FakeWatcher(), "owner/repo", "feature/branch", ""
        )
        self.assertIsNone(mismatch)

    def test_dispatch_url_requires_exact_host_repository_and_single_id(self):
        with patch.dict(os.environ, {"GH_HOST": "github.com"}):
            self.assertEqual(MODULE._dispatch_run_id("https://github.com/owner/repo/actions/runs/501\n", "owner/repo"), 501)
            for value in ("", "204", "https://evil.example/owner/repo/actions/runs/501",
                          "https://github.com/owner/other/actions/runs/501",
                          "https://github.com/owner/repo/actions/runs/501?x=1",
                          "https://github.com/owner/repo/actions/runs/0",
                          "https://github.com/owner/repo/actions/runs/501\nextra"):
                with self.subTest(value=value), self.assertRaises(ValueError):
                    MODULE._dispatch_run_id(value, "owner/repo")

    def test_ambiguous_dispatch_errors_do_not_trigger_fallback(self):
        watcher = Mock()
        watcher.gh_text.side_effect = RuntimeError("transport lost after dispatch")
        with self.assertRaises(RuntimeError):
            MODULE._dispatch_workflow(watcher, "owner/repo", "1", "main", "auto", "", [])
        self.assertEqual(watcher.gh_text.call_count, 1)
        watcher.gh_text.reset_mock(side_effect=True)
        watcher.gh_text.side_effect = RuntimeError('HTTP 422: Unexpected inputs provided: ["supersession_mode"]')
        with self.assertRaises(RuntimeError):
            MODULE._dispatch_workflow(watcher, "owner/repo", "1", "main", "auto", "", [])
        self.assertEqual(watcher.gh_text.call_count, 1)

    def test_rejected_input_fallback_does_not_remove_caller_owned_field(self):
        watcher = Mock()
        watcher.gh_text.side_effect = RuntimeError(
            'GitHub CLI command failed\nstderr: HTTP 422: Unexpected inputs provided: ["supersession_mode"]'
        )
        with self.assertRaises(RuntimeError):
            MODULE._dispatch_workflow(watcher, "owner/repo", "1", "main", "auto", "", [("supersession_mode", "retain")])
        self.assertEqual(watcher.gh_text.call_count, 1)

    def test_workflow_name_requires_complete_unambiguous_catalog(self):
        watcher = Mock()
        watcher.gh_json.return_value = {"total_count": 2, "workflows": [
            {"id": 1, "name": "build", "path": ".github/workflows/one.yml"},
            {"id": 2, "name": "build", "path": ".github/workflows/two.yml"},
        ]}
        with patch.dict(os.environ, {"GH_HOST": "github.com"}), self.assertRaises(ValueError):
            MODULE._workflow_id(watcher, "owner/repo", "build")
        watcher.gh_json.return_value = {"total_count": 1, "workflows": []}
        with patch.dict(os.environ, {"GH_HOST": "github.com"}), self.assertRaises(ValueError):
            MODULE._workflow_id(watcher, "owner/repo", "build")

    def test_nonzero_stale_retry_stops_before_dispatch(self):
        argv = ["helper", "--workflow", "build.yml", "--stale-head-retries", "1"]
        with patch.object(sys, "argv", argv), patch.object(MODULE, "_load_watcher") as load, \
             patch("sys.stdout", new_callable=io.StringIO):
            self.assertEqual(MODULE.main(), 1)
        load.assert_not_called()

    def test_read_only_broker_allowlist_and_dispatch_preflight(self):
        proxy_path = Path(__file__).resolve().parents[2] / "babysit-pr/scripts/github_app_broker_proxy.py"
        spec = importlib.util.spec_from_file_location("actual_github_app_broker_proxy", proxy_path)
        proxy = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(proxy)
        with self.assertRaises(proxy.ProxyError):
            proxy.validate_gh_argv(["-R", "owner/repo", "repo", "view", "--json", "nameWithOwner,url"])
        with self.assertRaises(proxy.ProxyError):
            proxy.validate_gh_argv(["-R", "owner/repo", "workflow", "run", "42", "--ref", "main"])
        argv = ["helper", "--repo", "owner/repo", "--workflow", "build.yml", "--ref", "main"]
        with patch.dict(os.environ, {"GITHUB_APP_BROKER_SOCKET": "/not-opened"}, clear=True), \
             patch.object(sys, "argv", argv), patch.object(MODULE, "_load_watcher") as load, \
             patch.object(proxy, "request") as request, patch("sys.stdout", new_callable=io.StringIO) as out:
            self.assertEqual(MODULE.main(), 1)
        load.assert_not_called()
        request.assert_not_called()
        self.assertIn("read-only", json.loads(out.getvalue())["error"])

    def test_current_repository_context_is_distinct_from_explicit_unqualified_selection(self):
        watcher = Mock()
        def view(args, repo=None):
            host = "github.enterprise.example" if repo is None else "github.com"
            return {"nameWithOwner": "owner/repo", "url": f"https://{host}/owner/repo"}
        watcher.gh_json.side_effect = view
        with patch.dict(os.environ, {}, clear=True):
            current = MODULE._resolve_repo_host(watcher, "owner/repo", current_repo=True)
            explicit = MODULE._resolve_repo_host(watcher, "owner/repo", current_repo=False)
        self.assertEqual(current, "github.enterprise.example/owner/repo")
        self.assertEqual(explicit, "github.com/owner/repo")
        self.assertIsNone(watcher.gh_json.call_args_list[0].kwargs["repo"])
        self.assertEqual(watcher.gh_json.call_args_list[1].kwargs["repo"], "owner/repo")

    def test_whitespace_repository_selectors_stop_before_dispatch(self):
        cases = [
            {"GH_WORKFLOW_RUN_WATCH_REPO": "   "},
            {"GH_REPO": "   "},
            {"GH_WORKFLOW_RUN_WATCH_REPO": "   ", "GH_REPO": "github.com/owner/repo"},
            {"GH_WORKFLOW_RUN_WATCH_REPO": "", "GH_REPO": "github.com/owner/repo"},
            {"GH_WORKFLOW_RUN_WATCH_REPO": "github.com/owner/repo", "GH_REPO": "  "},
        ]
        for env in cases:
            argv = ["helper", "--workflow", "build.yml", "--ref", "main"]
            with self.subTest(env=env), patch.dict(os.environ, env, clear=True), \
                 patch.object(sys, "argv", argv), patch.object(MODULE, "_load_watcher") as load, \
                 patch.object(MODULE, "_dispatch_workflow") as dispatch, patch("sys.stdout", new_callable=io.StringIO) as out:
                self.assertEqual(MODULE.main(), 1)
            load.assert_not_called()
            dispatch.assert_not_called()
            self.assertIn("empty or whitespace", json.loads(out.getvalue())["error"])

    def test_valid_secondary_repository_is_explicit(self):
        sha = "a" * 40
        class FakeWatcher:
            GhCommandError = RuntimeError
            def __init__(self): self.views = []
            def detect_repo(self):
                return (os.environ.get("GH_WORKFLOW_RUN_WATCH_REPO") or os.environ.get("GH_REPO") or "").strip()
            def detect_ref(self, ref): return ref
            def is_sha_like(self, ref): return ref == sha
            def gh_json(self, args, repo=None):
                self.views.append((repo, os.environ.get("GH_REPO")))
                return {"nameWithOwner": "owner/repo", "url": "https://github.com/owner/repo"}
        fake = FakeWatcher()
        env = {"GH_REPO": "github.com/owner/repo"}
        argv = ["helper", "--workflow", "build.yml", "--ref", sha, "--head-sha", sha]
        with patch.dict(os.environ, env, clear=True), patch.object(sys, "argv", argv), \
             patch.object(MODULE, "_load_watcher", return_value=fake), \
             patch.object(MODULE, "_dispatch_workflow") as dispatch, patch("sys.stdout", new_callable=io.StringIO):
            self.assertEqual(MODULE.main(), 1)
        self.assertEqual(fake.views, [("github.com/owner/repo", "github.com/owner/repo")])
        dispatch.assert_not_called()

    def test_main_binds_dispatch_id_not_concurrent_run_and_keeps_target_separate(self):
        sha = "a" * 40
        class FakeWatcher:
            GhCommandError = RuntimeError
            calls = []
            def detect_ref(self, ref): return ref
            def is_sha_like(self, ref): return ref == sha
            def gh_json(self, args, repo=None):
                self.calls.append(args[1])
                if args[0] == "repo": return {"nameWithOwner": "owner/repo", "url": "https://github.com/owner/repo"}
                if "/git/ref/heads/" in args[1]: return {"object": {"sha": sha}}
                if "/actions/workflows/" in args[1]: return {"id": 42}
                if args[1].endswith("/actions/runs/501"):
                    return {"id": 501, "repository": {"full_name": "owner/repo"},
                            "workflow_id": 42, "event": "workflow_dispatch", "head_branch": "main", "head_sha": sha}
                raise AssertionError("Unexpected listing or foreign-run read")
            def gh_text(self, args, repo=None):
                self.calls.append("dispatch")
                return "https://github.com/owner/repo/actions/runs/501\n"
        fake = FakeWatcher()
        argv = ["helper", "--repo", "owner/repo", "--workflow", "build.yml", "--ref", "main",
                "--head-sha", sha, "--validation-target-ref", "candidate", "--validation-target-sha", "b" * 40,
                "--wait-until-terminal"]
        with patch.dict(os.environ, {"GH_HOST": "github.com"}), patch.object(sys, "argv", argv), \
             patch.object(MODULE, "_load_watcher", return_value=fake), \
             patch.object(MODULE, "_run_watcher", return_value=0) as watch, patch("sys.stdout", new_callable=io.StringIO) as out:
            self.assertEqual(MODULE.main(), 0)
        self.assertEqual(fake.calls.count("dispatch"), 1)
        self.assertEqual(watch.call_args.args[1], 501)
        self.assertEqual(watch.call_args.kwargs["validation_target_ref"], "candidate")
        self.assertEqual(watch.call_args.kwargs["validation_target_sha"], "b" * 40)
        self.assertTrue(watch.call_args.kwargs["require_terminal_run"])
        self.assertEqual([json.loads(line)["dispatch_receipt"]["stage"] for line in out.getvalue().splitlines()], ["provisional", "verified"])

    def test_enterprise_host_resolved_before_dispatch_and_pinned_through_watch(self):
        sha = "a" * 40
        host = "github.enterprise.example"
        class FakeWatcher:
            GhCommandError = RuntimeError
            def __init__(self): self.calls = []
            def detect_repo(self): return "owner/repo"  # watcher autodetection strips remote host
            def detect_ref(self, ref): return ref
            def is_sha_like(self, ref): return ref == sha
            def gh_json(self, args, repo=None):
                self.calls.append((list(args), repo))
                if args[0] == "repo": return {"nameWithOwner": "owner/repo", "url": f"https://{host}/owner/repo"}
                if repo != f"{host}/owner/repo" or args[-2:] != ["--hostname", host]:
                    raise AssertionError("API host was not pinned")
                if "/git/ref/heads/" in args[1]: return {"object": {"sha": sha}}
                if "/actions/workflows/" in args[1]: return {"id": 42}
                if args[1].endswith("/actions/runs/501"):
                    return {"id": 501, "repository": {"full_name": "owner/repo"}, "workflow_id": 42,
                            "event": "workflow_dispatch", "head_branch": "main", "head_sha": sha}
                raise AssertionError("Unexpected API read")
            def gh_text(self, args, repo=None):
                self.calls.append((list(args), repo))
                if repo != f"{host}/owner/repo": raise AssertionError("Dispatch host was not pinned")
                return f"https://{host}/owner/repo/actions/runs/501\n"
        for selected_repo, env in ((None, {}), (f"{host}/owner/repo", {}), ("owner/repo", {"GH_HOST": host})):
            fake = FakeWatcher()
            argv = ["helper", "--workflow", "build.yml", "--ref", "main", "--head-sha", sha]
            if selected_repo: argv.extend(["--repo", selected_repo])
            with self.subTest(repo=selected_repo, env=env), patch.dict(os.environ, env, clear=True), \
                 patch.object(sys, "argv", argv), patch.object(MODULE, "_load_watcher", return_value=fake), \
                 patch.object(MODULE, "_run_watcher", return_value=0) as watch, patch("sys.stdout", new_callable=io.StringIO) as out:
                self.assertEqual(MODULE.main(), 0)
            self.assertEqual(watch.call_args.kwargs["repo"], "owner/repo")
            self.assertEqual(watch.call_args.kwargs["host"], host)
            self.assertEqual(watch.call_args.args[1], 501)
            view_call = next(call for call in fake.calls if call[0][0] == "repo")
            self.assertEqual(view_call[1], selected_repo)
            receipts = [json.loads(line)["dispatch_receipt"] for line in out.getvalue().splitlines()]
            self.assertEqual([receipt["stage"] for receipt in receipts], ["provisional", "verified"])
            self.assertEqual(receipts[0]["run_url"], f"https://{host}/owner/repo/actions/runs/501")

    def test_conflicting_malformed_or_unreadable_host_stops_before_dispatch(self):
        cases = [
            ("github.enterprise.example/owner/repo", {"GH_HOST": "github.com"}, None),
            ("owner/repo", {}, {"nameWithOwner": "owner/repo", "url": "https://github.com@evil.example/owner/repo"}),
            ("owner/repo", {}, {"nameWithOwner": "owner/other", "url": "https://github.com/owner/other"}),
            ("owner/repo", {}, {"nameWithOwner": "owner/repo", "url": "https://bad..host/owner/repo"}),
            ("owner/repo", {}, RuntimeError("metadata unavailable")),
        ]
        for repo, env, payload in cases:
            watcher = Mock(GhCommandError=RuntimeError)
            if isinstance(payload, Exception): watcher.gh_json.side_effect = payload
            else: watcher.gh_json.return_value = payload
            argv = ["helper", "--repo", repo, "--workflow", "build.yml", "--ref", "main", "--head-sha", "a" * 40]
            with self.subTest(repo=repo, payload=payload), patch.dict(os.environ, env, clear=True), \
                 patch.object(sys, "argv", argv), patch.object(MODULE, "_load_watcher", return_value=watcher), \
                 patch.object(MODULE, "_dispatch_workflow") as dispatch, patch("sys.stdout", new_callable=io.StringIO):
                self.assertEqual(MODULE.main(), 1)
            dispatch.assert_not_called()
            watcher.detect_ref.assert_not_called()

    def test_sha_only_ref_stops_before_dispatch(self):
        sha = "a" * 40
        watcher = Mock(GhCommandError=RuntimeError)
        watcher.detect_ref.return_value = sha
        watcher.is_sha_like.return_value = True
        watcher.gh_json.return_value = {"nameWithOwner": "owner/repo", "url": "https://github.com/owner/repo"}
        argv = ["helper", "--repo", "owner/repo", "--workflow", "build.yml", "--ref", sha, "--head-sha", sha]
        with patch.dict(os.environ, {}, clear=True), patch.object(sys, "argv", argv), \
             patch.object(MODULE, "_load_watcher", return_value=watcher), \
             patch.object(MODULE, "_dispatch_workflow") as dispatch, patch("sys.stdout", new_callable=io.StringIO):
            self.assertEqual(MODULE.main(), 1)
        dispatch.assert_not_called()

    def test_exact_run_rejects_each_identity_mismatch(self):
        good = {"id": 501, "repository": {"full_name": "owner/repo"}, "workflow_id": 42,
                "event": "workflow_dispatch", "head_branch": "main", "head_sha": "a" * 40}
        for field, value in (("id", 502), ("repository", {"full_name": "owner/other"}),
                             ("workflow_id", 43), ("event", "push"), ("head_branch", "other"),
                             ("head_sha", "b" * 40)):
            run = dict(good, **{field: value})
            watcher = Mock(GhCommandError=RuntimeError)
            watcher.gh_json.return_value = run
            with self.subTest(field=field), patch.dict(os.environ, {"GH_HOST": "github.com"}), self.assertRaises(ValueError):
                MODULE._wait_for_exact_run(watcher, "owner/repo", 501, 42, "main", "a" * 40,
                    start_time=0, attempts=0, max_wait_seconds=0, max_retries=0, poll_seconds=1, appearance_timeout_seconds=1)
            self.assertEqual(watcher.gh_json.call_count, 1)

    def test_missing_malformed_foreign_and_ambiguous_dispatch_never_replay(self):
        sha = "a" * 40
        class FakeWatcher:
            GhCommandError = RuntimeError
            def detect_ref(self, ref): return ref
            def is_sha_like(self, ref): return False
        argv = ["helper", "--repo", "owner/repo", "--workflow", "build.yml", "--ref", "main", "--head-sha", sha]
        for result in ("", "204", "garbage", "https://github.com/other/repo/actions/runs/501", RuntimeError("transport lost")):
            with self.subTest(result=str(result)), patch.dict(os.environ, {"GH_HOST": "github.com"}), \
                 patch.object(sys, "argv", argv), patch.object(MODULE, "_load_watcher", return_value=FakeWatcher()), \
                 patch.object(MODULE, "_resolve_repo_host", return_value="github.com/owner/repo"), \
                 patch.object(MODULE, "_resolve_remote_ref_sha", return_value=sha), \
                 patch.object(MODULE, "_wait_for_ref_to_match_expected", return_value=(True, 0)), \
                 patch.object(MODULE, "_query_remote_ref_sha", return_value=sha), \
                 patch.object(MODULE, "_workflow_id", return_value=42), \
                 patch.object(MODULE, "_dispatch_workflow") as dispatch, \
                 patch.object(MODULE, "_run_watcher") as watch, patch("sys.stdout", new_callable=io.StringIO):
                if isinstance(result, Exception): dispatch.side_effect = result
                else: dispatch.return_value = result
                self.assertEqual(MODULE.main(), 1)
                dispatch.assert_called_once()
                watch.assert_not_called()

    def test_exact_run_appearance_reads_only_returned_id(self):
        good = {"id": 501, "repository": {"full_name": "owner/repo"}, "workflow_id": 42,
                "event": "workflow_dispatch", "head_branch": "main", "head_sha": "a" * 40}
        watcher = Mock(GhCommandError=RuntimeError)
        watcher.gh_json.side_effect = [RuntimeError("HTTP 404"), good]
        with patch.dict(os.environ, {"GH_HOST": "github.com"}), patch.object(MODULE.time, "sleep"):
            self.assertEqual(MODULE._wait_for_exact_run(watcher, "owner/repo", 501, 42, "main", "a" * 40,
                start_time=0, attempts=0, max_wait_seconds=0, max_retries=0, poll_seconds=1, appearance_timeout_seconds=10), good)
        self.assertEqual([call.args[0][1] for call in watcher.gh_json.call_args_list],
                         ["/repos/owner/repo/actions/runs/501"] * 2)

    def test_run_watcher_forwards_separate_validation_target_identity(self):
        completed = types.SimpleNamespace(returncode=0, stdout="", stderr="")
        with patch.object(MODULE.subprocess, "run", return_value=completed) as run:
            rc = MODULE._run_watcher(
                Mock(),
                501,
                repo="owner/repo",
                wait_for="first_action",
                poll_seconds=10,
                appearance_timeout=300,
                require_terminal_run=True,
                validation_target_ref="candidate/exact",
                validation_target_sha="2222222",
                host="github.enterprise.example",
            )

        self.assertEqual(rc, 0)
        self.assertEqual(run.call_args.kwargs["env"]["GH_HOST"], "github.enterprise.example")
        self.assertEqual(run.call_args.kwargs["env"]["GH_REPO"], "github.enterprise.example/owner/repo")
        command = run.call_args.args[0]
        self.assertIn("--validation-target-ref", command)
        self.assertEqual(
            command[command.index("--validation-target-ref") + 1],
            "candidate/exact",
        )
        self.assertIn("--validation-target-sha", command)
        self.assertEqual(
            command[command.index("--validation-target-sha") + 1],
            "2222222",
        )


if __name__ == "__main__":
    unittest.main()
