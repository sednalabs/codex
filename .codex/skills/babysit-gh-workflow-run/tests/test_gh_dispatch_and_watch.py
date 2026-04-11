import importlib.util
import io
import json
import os
import sys
import unittest
from pathlib import Path
from unittest.mock import patch


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
                        'GitHub CLI command failed: gh ... stderr: could not create workflow dispatch '
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

    def test_select_newest_matching_run_returns_stale_head_when_only_wrong_sha_exists(self):
        class FakeWatcher:
            def __init__(self):
                self.calls = 0

            def list_workflow_runs(self, repo, workflow, ref, expected_head_sha=None, minimum_run_id=None):
                self.calls += 1
                if expected_head_sha:
                    return []
                return [
                    {
                        "databaseId": 200,
                        "headSha": "oldoldold1234567",
                        "url": "https://example.invalid/runs/200",
                    }
                ]

        watcher = FakeWatcher()
        selection, attempts = MODULE._select_newest_matching_run(
            watcher,
            "owner/repo",
            "validation-lab.yml",
            "feature/branch",
            expected_head_sha="newnewnewabcdef",
            minimum_run_id=150,
            start_time=0.0,
            attempts=0,
            max_wait_seconds=30,
            max_retries=0,
            poll_seconds=1,
            dispatch_start_time=0.0,
            appearance_timeout_seconds=0,
        )
        self.assertEqual(attempts, 0)
        self.assertEqual(selection["kind"], "stale_head")
        self.assertEqual(selection["run"]["databaseId"], 200)

    def test_select_newest_matching_run_does_not_mark_stale_when_head_sha_not_visible_yet(self):
        class FakeWatcher:
            def __init__(self):
                self.calls = 0

            def list_workflow_runs(self, repo, workflow, ref, expected_head_sha=None, minimum_run_id=None):
                self.calls += 1
                if expected_head_sha:
                    if self.calls < 3:
                        return []
                    return [
                        {
                            "databaseId": 201,
                            "headSha": "newnewnewabcdef1234567890",
                            "url": "https://example.invalid/runs/201",
                        }
                    ]
                return [
                    {
                        "databaseId": 201,
                        "headSha": "",
                        "url": "https://example.invalid/runs/201",
                    }
                ]

        with patch.object(MODULE.time, "monotonic", side_effect=[1.0, 1.1]), patch.object(
            MODULE.time, "sleep"
        ) as sleep_mock:
            selection, attempts = MODULE._select_newest_matching_run(
                FakeWatcher(),
                "owner/repo",
                "validation-lab.yml",
                "feature/branch",
                expected_head_sha="newnewnewabcdef",
                minimum_run_id=150,
                start_time=1.0,
                attempts=0,
                max_wait_seconds=30,
                max_retries=0,
                poll_seconds=1,
                dispatch_start_time=1.0,
                appearance_timeout_seconds=0,
            )

        sleep_mock.assert_called_once_with(1)
        self.assertEqual(attempts, 1)
        self.assertEqual(selection["kind"], "matched")
        self.assertEqual(selection["run"]["databaseId"], 201)

    def test_select_newest_matching_run_waits_when_newest_run_head_sha_is_unhydrated(self):
        class FakeWatcher:
            def __init__(self):
                self.calls = 0

            def list_workflow_runs(self, repo, workflow, ref, expected_head_sha=None, minimum_run_id=None):
                self.calls += 1
                if expected_head_sha:
                    if self.calls < 3:
                        return []
                    return [
                        {
                            "databaseId": 201,
                            "headSha": "newnewnewabcdef1234567890",
                            "url": "https://example.invalid/runs/201",
                        }
                    ]
                return [
                    {
                        "databaseId": 201,
                        "headSha": "",
                        "url": "https://example.invalid/runs/201",
                    }
                ]

        with patch.object(MODULE.time, "monotonic", side_effect=[1.0, 1.2, 1.3]), patch.object(
            MODULE.time, "sleep"
        ) as sleep_mock:
            selection, attempts = MODULE._select_newest_matching_run(
                FakeWatcher(),
                "owner/repo",
                "validation-lab.yml",
                "feature/branch",
                expected_head_sha="newnewnewabcdef",
                minimum_run_id=150,
                start_time=1.0,
                attempts=0,
                max_wait_seconds=30,
                max_retries=0,
                poll_seconds=1,
                dispatch_start_time=1.0,
                appearance_timeout_seconds=1,
            )

        sleep_mock.assert_called_once_with(1)
        self.assertEqual(attempts, 1)
        self.assertEqual(selection["kind"], "matched")
        self.assertEqual(selection["run"]["databaseId"], 201)

    def test_select_newest_matching_run_returns_appearance_timeout_when_no_run_visible(self):
        class FakeWatcher:
            def list_workflow_runs(self, repo, workflow, ref, expected_head_sha=None, minimum_run_id=None):
                return []

        with patch.object(MODULE.time, "monotonic", side_effect=[1.0, 1.1, 11.5]), patch.object(
            MODULE.time, "sleep"
        ) as sleep_mock:
            selection, attempts = MODULE._select_newest_matching_run(
                FakeWatcher(),
                "owner/repo",
                "validation-lab.yml",
                "feature/branch",
                expected_head_sha="newsha123",
                minimum_run_id=150,
                start_time=0.0,
                attempts=0,
                max_wait_seconds=3600,
                max_retries=0,
                poll_seconds=1,
                dispatch_start_time=1.0,
                appearance_timeout_seconds=10,
            )

        sleep_mock.assert_called_once_with(1)
        self.assertEqual(attempts, 1)
        self.assertEqual(selection["kind"], "appearance_timed_out")

    def test_emit_stale_head_timeout_outputs_structured_action(self):
        buffer = io.StringIO()
        with patch("sys.stdout", buffer):
            rc = MODULE._emit_stale_head_timeout(
                workflow="validation-lab.yml",
                ref="feature/branch",
                expected_sha="newsha123",
                attempts_used=2,
                retries_allowed=1,
                stale_runs=[
                    {
                        "dispatch_attempt": 1,
                        "run_id": 111,
                        "run_url": "https://example.invalid/runs/111",
                        "run_head_sha": "oldsha999",
                        "expected_head_sha": "newsha123",
                    }
                ],
            )
        self.assertEqual(rc, 1)
        payload = json.loads(buffer.getvalue().strip())
        self.assertEqual(payload["actions"], ["stop_stale_head_dispatch_detected"])
        self.assertEqual(payload["stale_head_dispatch"]["expected_head_sha"], "newsha123")
        self.assertEqual(payload["stale_head_dispatch"]["latest_observed"]["run_id"], 111)

    def test_emit_dispatch_appearance_timeout_outputs_structured_action(self):
        buffer = io.StringIO()
        with patch("sys.stdout", buffer):
            rc = MODULE._emit_dispatch_appearance_timeout(
                workflow="validation-lab.yml",
                ref="feature/branch",
                expected_sha="newsha123",
                appearance_timeout_seconds=60,
                attempts_used=1,
            )
        self.assertEqual(rc, 1)
        payload = json.loads(buffer.getvalue().strip())
        self.assertEqual(payload["actions"], ["stop_dispatch_run_not_visible"])
        self.assertEqual(payload["dispatch_visibility"]["appearance_timeout_seconds"], 60)
        self.assertEqual(payload["dispatch_visibility"]["attempts_used"], 1)

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
        self.assertEqual(calls, [["api", "/repos/owner/repo/git/ref/heads/feature%2Fbranch"]])
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

    def test_resolve_dispatch_ref_uses_default_branch_for_downstream_ref_input(self):
        class FakeWatcher:
            def __init__(self):
                self.calls = []

            def detect_ref(self, ref):
                self.calls.append(("detect_ref", ref))
                return "feature/branch"

            def gh_json(self, args, repo=None):
                self.calls.append((tuple(args), repo))
                return {"defaultBranchRef": {"name": "main"}}

        watcher = FakeWatcher()
        self.assertEqual(
            MODULE._resolve_dispatch_ref(watcher, "owner/repo", "auto", []),
            "feature/branch",
        )
        self.assertIn(("detect_ref", "auto"), watcher.calls)

        watcher = FakeWatcher()
        self.assertEqual(
            MODULE._resolve_dispatch_ref(watcher, "owner/repo", "auto", [("ref", "target-branch")]),
            "main",
        )
        self.assertIn((("repo", "view", "--json", "defaultBranchRef"), "owner/repo"), watcher.calls)
        self.assertNotIn(("detect_ref", "auto"), watcher.calls)

    def test_main_dispatches_from_default_branch_for_downstream_ref_input(self):
        calls = {"list_workflow_runs": []}

        class FakeWatcher:
            GhCommandError = RuntimeError

            def detect_repo(self):
                return "owner/repo"

            def detect_ref(self, ref):
                raise AssertionError("detect_ref should not be used when auto resolves downstream ref input")

            def is_sha_like(self, value):
                return False

            def list_workflow_runs(self, repo, workflow, ref, expected_head_sha=None, minimum_run_id=None):
                calls["list_workflow_runs"].append(
                    (repo, workflow, ref, expected_head_sha, minimum_run_id)
                )
                return []

            def command_text(self, *args, **kwargs):
                raise AssertionError("command_text should not be used when head-sha is provided")

            def gh_json(self, args, repo=None):
                if args == ["repo", "view", "--json", "defaultBranchRef"]:
                    return {"defaultBranchRef": {"name": "main"}}
                raise AssertionError(f"unexpected gh_json call: {args!r}")

        watcher = FakeWatcher()
        argv = [
            "gh_dispatch_and_watch",
            "--workflow",
            "validation-lab.yml",
            "--head-sha",
            "a206ca4957946e4ba491d6c9eaef4380243c9f07",
            "--input",
            "ref=w3710-route-coverage-20260411",
            "--input",
            "profile=targeted",
            "--input",
            "lane_set=mcp",
            "--input",
            "lanes=ops-mcp-http",
        ]

        with patch.object(sys, "argv", argv), patch.object(
            MODULE, "_load_watcher", return_value=watcher
        ), patch.object(
            MODULE, "_validate_expected_head_sha_against_remote_branch", return_value=None
        ) as validate_mock, patch.object(
            MODULE, "_wait_for_ref_to_match_expected", return_value=(True, 0)
        ) as wait_mock, patch.object(
            MODULE, "_dispatch_workflow"
        ) as dispatch_mock, patch.object(
            MODULE,
            "_select_newest_matching_run",
            return_value=({"kind": "matched", "run": {"databaseId": 501}}, 0),
        ) as select_mock, patch.object(MODULE, "_run_watcher", return_value=0) as run_mock:
            rc = MODULE.main()

        self.assertEqual(rc, 0)
        validate_mock.assert_called_once_with(
            watcher,
            "owner/repo",
            "w3710-route-coverage-20260411",
            "a206ca4957946e4ba491d6c9eaef4380243c9f07",
        )
        wait_mock.assert_called_once()
        self.assertEqual(wait_mock.call_args.args[1:4], (
            "owner/repo",
            "w3710-route-coverage-20260411",
            "a206ca4957946e4ba491d6c9eaef4380243c9f07",
        ))
        self.assertEqual(dispatch_mock.call_args.args[3], "main")
        self.assertIn(("profile", "targeted"), dispatch_mock.call_args.args[6])
        self.assertIn(("lane_set", "mcp"), dispatch_mock.call_args.args[6])
        self.assertIn(("lanes", "ops-mcp-http"), dispatch_mock.call_args.args[6])
        self.assertEqual(select_mock.call_args.kwargs["appearance_timeout_seconds"], 300)
        self.assertEqual(calls["list_workflow_runs"][0][2], "main")
        self.assertIsNone(select_mock.call_args.kwargs["expected_head_sha"])
        self.assertEqual(select_mock.call_args.args[3], "main")
        self.assertEqual(run_mock.call_args.kwargs["appearance_timeout"], 300)
        self.assertEqual(run_mock.call_args.args[1], 501)

    def test_effective_minimum_run_id_applies_override(self):
        self.assertEqual(MODULE._effective_minimum_run_id(100, None), 101)
        self.assertEqual(MODULE._effective_minimum_run_id(100, 50), 101)
        self.assertEqual(MODULE._effective_minimum_run_id(100, 500), 500)

    @patch.object(MODULE, "_run_watcher")
    @patch.object(MODULE, "_dispatch_workflow")
    @patch.object(MODULE, "_select_newest_matching_run")
    @patch.object(MODULE, "_load_watcher")
    def test_min_run_id_passed_to_select(self, load_mock, select_mock, dispatch_mock, run_mock):
        select_record = {}

        def fake_select(*args, **kwargs):
            select_record["minimum_run_id"] = kwargs.get("minimum_run_id")
            return {"kind": "matched", "run": {"databaseId": 501}}, 0

        select_mock.side_effect = fake_select
        run_mock.return_value = 0
        dispatch_mock.return_value = None

        class FakeWatcher:
            GhCommandError = RuntimeError

            def detect_repo(self):
                return "owner/repo"

            def detect_ref(self, ref):
                return ref

            def is_sha_like(self, value):
                return bool(value)

            def list_workflow_runs(self, *args, **kwargs):
                return []

            def command_text(self, *args, **kwargs):
                return ""

        load_mock.return_value = FakeWatcher()

        argv = [
            "gh_dispatch_and_watch",
            "--workflow",
            "validation-lab.yml",
            "--ref",
            "deadbeef123456789abcdef",
            "--head-sha",
            "deadbeef123456789abcdef",
            "--min-run-id",
            "500",
        ]
        with patch.object(sys, "argv", argv):
            rc = MODULE.main()

        self.assertEqual(rc, 0)
        self.assertEqual(select_record["minimum_run_id"], 500)


if __name__ == "__main__":
    unittest.main()
