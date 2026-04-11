import importlib.util
import io
import json
import os
import subprocess
import tempfile
import types
import sys
import urllib.error
import unittest
from contextlib import contextmanager
from pathlib import Path
from unittest.mock import Mock, patch


MODULE_PATH = Path(
    os.environ.get(
        "GH_WORKFLOW_RUN_WATCH_MODULE_PATH",
        str(Path(__file__).resolve().parents[1] / "scripts" / "gh_workflow_run_watch.py"),
    )
)
SPEC = importlib.util.spec_from_file_location("gh_workflow_run_watch", MODULE_PATH)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


@contextmanager
def temp_cwd(path):
    previous = Path.cwd()
    os.chdir(path)
    try:
        yield
    finally:
        os.chdir(previous)


class GeminiWatcherTests(unittest.TestCase):
    def test_target_display_key_includes_head_sha(self):
        target = {
            "kind": MODULE.TARGET_KIND_WORKFLOW,
            "workflow": "validation-lab",
            "ref": "integration/test",
            "head_sha": "abc123",
        }
        self.assertEqual(
            MODULE.target_to_display_key(target),
            "workflow:validation-lab|ref:integration/test|head-sha:abc123",
        )

    def test_target_display_key_includes_min_run_id(self):
        target = {
            "kind": MODULE.TARGET_KIND_WORKFLOW,
            "workflow": "validation-lab",
            "ref": "integration/test",
            "head_sha": "abc123",
            "min_run_id": 456,
        }
        self.assertEqual(
            MODULE.target_to_display_key(target),
            "workflow:validation-lab|ref:integration/test|head-sha:abc123|min-run-id:456",
        )

    def test_parse_target_arg_accepts_min_run_id(self):
        target = MODULE.parse_target_arg(
            "workflow=validation-lab,ref=integration/test,head-sha=abc123,min-run-id=456"
        )
        self.assertEqual(target["min_run_id"], 456)

    def test_parse_target_arg_accepts_host_ref(self):
        target = MODULE.parse_target_arg(
            "workflow=validation-lab,ref=validation/w2902,host-ref=main,head-sha=abc123"
        )
        self.assertEqual(target["host_ref"], "main")

    def test_parse_target_arg_rejects_invalid_min_run_id(self):
        with self.assertRaises(MODULE.GhCommandError):
            MODULE.parse_target_arg(
                "workflow=validation-lab,ref=integration/test,min-run-id=not-a-number"
            )

    def test_list_workflow_runs_filters_by_min_run_id(self):
        runs = [
            {
                "databaseId": 100,
                "headBranch": "integration/test",
                "headSha": "abc123def0",
                "status": "completed",
                "conclusion": "failure",
            },
            {
                "databaseId": 200,
                "headBranch": "integration/test",
                "headSha": "abc123dead",
                "status": "queued",
                "conclusion": "",
            },
            {
                "databaseId": 300,
                "headBranch": "other",
                "headSha": "abc123ffff",
                "status": "queued",
                "conclusion": "",
            },
        ]
        with patch.object(MODULE, "gh_json", return_value=runs):
            filtered = MODULE.list_workflow_runs("owner/repo", "validation-lab", "integration/test", "abc123", minimum_run_id=150)

        self.assertEqual(len(filtered), 1)
        self.assertEqual(filtered[0]["databaseId"], 200)

    def test_list_workflow_runs_matches_expected_head_sha_case_insensitively(self):
        runs = [
            {
                "databaseId": 100,
                "headBranch": "integration/test",
                "headSha": "A206CA4957946E4BA491D6C9EAEF4380243C9F07",
                "status": "queued",
                "conclusion": "",
            }
        ]
        with patch.object(MODULE, "gh_json", return_value=runs):
            filtered = MODULE.list_workflow_runs(
                "owner/repo",
                "validation-lab",
                "integration/test",
                "a206ca49",
            )

        self.assertEqual(len(filtered), 1)
        self.assertEqual(filtered[0]["databaseId"], 100)

    def test_list_workflow_runs_accepts_multiple_expected_head_sha_prefixes(self):
        runs = [
            {
                "databaseId": 100,
                "headBranch": "main",
                "headSha": "b3710929c80726c970083486d691d3a5ebd17043",
                "status": "queued",
                "conclusion": "",
            }
        ]
        with patch.object(MODULE, "gh_json", return_value=runs):
            filtered = MODULE.list_workflow_runs(
                "owner/repo",
                "validation-lab",
                "main",
                ["deadbeef", "B3710929"],
            )

        self.assertEqual(len(filtered), 1)
        self.assertEqual(filtered[0]["databaseId"], 100)

    def test_target_state_surfaces_dispatch_host_branch_mismatch(self):
        args = types.SimpleNamespace(
            no_gemini_diagnosis=True,
            gemini_model=MODULE.GEMINI_DEFAULT_MODEL,
            gemini_timeout_seconds=MODULE.GEMINI_DEFAULT_TIMEOUT_SECONDS,
            appearance_timeout_seconds=300,
            poll_seconds=10,
            ack_action=[],
            verbose_details=False,
        )
        target = {
            "kind": MODULE.TARGET_KIND_WORKFLOW,
            "workflow": "validation-lab.yml",
            "ref": "validation/w2902-subagent-confirm",
            "head_sha": "9f95361ef183d194ffcba7c376b3e298d6e49ead",
            "min_run_id": None,
            "spec": "workflow=validation-lab.yml,ref=validation/w2902-subagent-confirm,head-sha=9f95361",
        }
        mismatch_run = {
            "databaseId": 23950570058,
            "headBranch": "main",
            "headSha": "9f95361ef183d194ffcba7c376b3e298d6e49ead",
            "event": "workflow_dispatch",
            "url": "https://github.com/sednalabs/codex/actions/runs/23950570058",
        }

        with patch.object(MODULE, "detect_ref", return_value=target["ref"]), patch.object(
            MODULE, "list_workflow_runs", side_effect=[[], [mismatch_run]]
        ):
            snapshot = MODULE.target_state_from_target(args, target, "sednalabs/codex", {})

        self.assertEqual(snapshot["actions"], ["stop_dispatch_host_branch_mismatch"])
        mismatch = snapshot["appearance_wait"]["dispatch_host_mismatch"]
        self.assertEqual(mismatch["host_branch"], "main")
        self.assertEqual(mismatch["run_id"], 23950570058)
        self.assertIn("host-ref=main", mismatch["suggested_target"])

    def test_target_state_keeps_following_cached_run_without_relisting_each_poll(self):
        args = types.SimpleNamespace(
            no_gemini_diagnosis=True,
            gemini_model=MODULE.GEMINI_DEFAULT_MODEL,
            gemini_timeout_seconds=MODULE.GEMINI_DEFAULT_TIMEOUT_SECONDS,
            appearance_timeout_seconds=300,
            ack_action=[],
            verbose_details=False,
            poll_seconds=10,
        )
        target = {
            "kind": MODULE.TARGET_KIND_WORKFLOW,
            "workflow": "validation-lab.yml",
            "ref": "feature/branch",
            "host_ref": None,
            "head_sha": None,
            "min_run_id": None,
            "spec": "workflow=validation-lab.yml,ref=feature/branch",
        }
        run_view = {
            "databaseId": 101,
            "number": 7,
            "displayTitle": "workflow run",
            "workflowName": "validation-lab.yml",
            "url": "https://example.invalid/run/101",
            "headBranch": "feature/branch",
            "headSha": "abcdef123456",
            "event": "push",
            "status": "in_progress",
            "conclusion": "",
            "createdAt": "2026-03-31T00:00:00Z",
            "updatedAt": "2026-03-31T00:05:00Z",
            "jobs": [],
        }
        newer_run = {
            "databaseId": 202,
            "headBranch": "feature/branch",
            "headSha": "abcdef123456",
            "event": "push",
            "url": "https://example.invalid/run/202",
        }

        remembered = {}
        with patch.object(MODULE, "detect_ref", return_value=target["ref"]), patch.object(
            MODULE,
            "list_workflow_runs",
            side_effect=[[{"databaseId": 101, "headBranch": "feature/branch", "headSha": "abcdef123456"}], [newer_run]],
        ) as list_mock, patch.object(MODULE, "view_run", return_value=run_view) as view_mock, patch.object(
            MODULE.time,
            "time",
            side_effect=[100, 100, 110, 110],
        ):
            snapshot1 = MODULE.target_state_from_target(args, target, "sednalabs/codex", remembered)
            snapshot2 = MODULE.target_state_from_target(args, target, "sednalabs/codex", remembered)

        self.assertEqual(list_mock.call_count, 1)
        self.assertEqual(view_mock.call_args_list[0].args[1], 101)
        self.assertEqual(view_mock.call_args_list[1].args[1], 101)
        self.assertEqual(snapshot1["run"]["id"], 101)
        self.assertEqual(snapshot2["run"]["id"], 101)
        self.assertFalse(snapshot2["followed_newer_run"])

    def test_target_state_rechecks_follow_discovery_after_the_hold_window(self):
        args = types.SimpleNamespace(
            no_gemini_diagnosis=True,
            gemini_model=MODULE.GEMINI_DEFAULT_MODEL,
            gemini_timeout_seconds=MODULE.GEMINI_DEFAULT_TIMEOUT_SECONDS,
            appearance_timeout_seconds=300,
            ack_action=[],
            verbose_details=False,
            poll_seconds=10,
        )
        target = {
            "kind": MODULE.TARGET_KIND_WORKFLOW,
            "workflow": "validation-lab.yml",
            "ref": "feature/branch",
            "host_ref": None,
            "head_sha": None,
            "min_run_id": None,
            "spec": "workflow=validation-lab.yml,ref=feature/branch",
        }
        initial_run = {
            "databaseId": 101,
            "headBranch": "feature/branch",
            "headSha": "abcdef123456",
            "event": "push",
            "url": "https://example.invalid/run/101",
        }
        newer_run_view = {
            "databaseId": 202,
            "number": 8,
            "displayTitle": "workflow run",
            "workflowName": "validation-lab.yml",
            "url": "https://example.invalid/run/202",
            "headBranch": "feature/branch",
            "headSha": "abcdef123456",
            "event": "push",
            "status": "in_progress",
            "conclusion": "",
            "createdAt": "2026-03-31T00:06:00Z",
            "updatedAt": "2026-03-31T00:07:00Z",
            "jobs": [],
        }

        remembered = {}
        with patch.object(MODULE, "detect_ref", return_value=target["ref"]), patch.object(
            MODULE,
            "list_workflow_runs",
            side_effect=[[initial_run], [newer_run_view]],
        ) as list_mock, patch.object(
            MODULE,
            "view_run",
            side_effect=[
                {
                    "databaseId": 101,
                    "number": 7,
                    "displayTitle": "workflow run",
                    "workflowName": "validation-lab.yml",
                    "url": "https://example.invalid/run/101",
                    "headBranch": "feature/branch",
                    "headSha": "abcdef123456",
                    "event": "push",
                    "status": "in_progress",
                    "conclusion": "",
                    "createdAt": "2026-03-31T00:00:00Z",
                    "updatedAt": "2026-03-31T00:05:00Z",
                    "jobs": [],
                },
                newer_run_view,
            ],
        ) as view_mock, patch.object(MODULE.time, "time", side_effect=[100, 100, 170, 170]):
            snapshot1 = MODULE.target_state_from_target(args, target, "sednalabs/codex", remembered)
            snapshot2 = MODULE.target_state_from_target(args, target, "sednalabs/codex", remembered)

        self.assertEqual(list_mock.call_count, 2)
        self.assertEqual(view_mock.call_args_list[1].args[1], 202)
        self.assertEqual(snapshot2["run"]["id"], 202)
        self.assertTrue(snapshot2["followed_newer_run"])

    def test_target_state_throttles_host_mismatch_rechecks_between_no_match_polls(self):
        args = types.SimpleNamespace(
            no_gemini_diagnosis=True,
            gemini_model=MODULE.GEMINI_DEFAULT_MODEL,
            gemini_timeout_seconds=MODULE.GEMINI_DEFAULT_TIMEOUT_SECONDS,
            appearance_timeout_seconds=300,
            ack_action=[],
            verbose_details=False,
            poll_seconds=10,
        )
        target = {
            "kind": MODULE.TARGET_KIND_WORKFLOW,
            "workflow": "validation-lab.yml",
            "ref": "validation/w2902-subagent-confirm",
            "host_ref": None,
            "head_sha": "9f95361ef183d194ffcba7c376b3e298d6e49ead",
            "min_run_id": None,
            "spec": "workflow=validation-lab.yml,ref=validation/w2902-subagent-confirm,head-sha=9f95361",
        }
        mismatch_run = {
            "databaseId": 23950570058,
            "headBranch": "main",
            "headSha": "9f95361ef183d194ffcba7c376b3e298d6e49ead",
            "event": "workflow_dispatch",
            "url": "https://github.com/sednalabs/codex/actions/runs/23950570058",
        }

        remembered = {}
        with patch.object(MODULE, "detect_ref", return_value=target["ref"]), patch.object(
            MODULE, "list_workflow_runs", side_effect=[[], [mismatch_run], []]
        ) as list_mock, patch.object(MODULE.time, "time", side_effect=[100, 100, 110, 110]):
            snapshot1 = MODULE.target_state_from_target(args, target, "sednalabs/codex", remembered)
            snapshot2 = MODULE.target_state_from_target(args, target, "sednalabs/codex", remembered)

        self.assertEqual(list_mock.call_count, 3)
        self.assertEqual(snapshot1["actions"], ["stop_dispatch_host_branch_mismatch"])
        self.assertEqual(snapshot2["actions"], ["stop_dispatch_host_branch_mismatch"])
        self.assertEqual(snapshot2["appearance_wait"]["dispatch_host_mismatch"]["run_id"], 23950570058)

    def test_launcher_runs_without_path_when_python_override_is_set(self):
        launcher = Path(__file__).resolve().parents[1] / "scripts" / "gh_workflow_run_watch"
        env = {"PATH": "", "GH_WORKFLOW_RUN_WATCH_PYTHON": sys.executable}
        result = subprocess.run(
            [str(launcher), "--help"],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, msg=result.stderr)
        self.assertIn("usage:", result.stdout)

    def test_detect_repo_respects_watch_repo_env(self):
        with patch.dict(
            os.environ,
            {"GH_WORKFLOW_RUN_WATCH_REPO": "sednalabs/solar-gravity-lab"},
            clear=True,
        ), patch.object(MODULE, "command_text", return_value=None), patch.object(
            MODULE, "gh_json", return_value=None
        ):
            repo = MODULE.detect_repo()
        self.assertEqual(repo, "sednalabs/solar-gravity-lab")

    def test_load_gemini_api_keys_prefers_env_and_dedupes(self):
        with patch.dict(
            os.environ,
            {"GEMINI_API_KEYS": "one, two, one,   three"},
            clear=True,
        ):
            keys = MODULE._load_gemini_api_keys()
            self.assertEqual(keys, ["one", "two", "three"])
            self.assertEqual(os.environ["GEMINI_API_KEY"], "one")

    def test_load_gemini_api_keys_falls_back_to_env_file(self):
        with tempfile.TemporaryDirectory(prefix="gemini-env-") as tmpdir:
            root = Path(tmpdir) / "repo"
            root.mkdir()
            (root / ".env.local").write_text(
                "export GEMINI_API_KEYS='alpha, beta, alpha'\n",
                encoding="utf-8",
            )
            nested = root / "subdir"
            nested.mkdir()
            with patch.dict(os.environ, {}, clear=True), temp_cwd(nested):
                keys = MODULE._load_gemini_api_keys()
                self.assertEqual(keys, ["alpha", "beta"])
                self.assertEqual(os.environ["GEMINI_API_KEY"], "alpha")

    def test_parse_args_respects_disable_env_but_flag_can_override(self):
        with patch.dict(os.environ, {"GH_WORKFLOW_RUN_WATCH_DISABLE_GEMINI": "1"}, clear=True):
            with patch.object(sys, "argv", ["gh_workflow_run_watch.py", "--once"]):
                args = MODULE.parse_args()
            self.assertTrue(args.no_gemini_diagnosis)

            with patch.object(sys, "argv", ["gh_workflow_run_watch.py", "--once", "--gemini-diagnosis"]):
                args = MODULE.parse_args()
            self.assertFalse(args.no_gemini_diagnosis)

    def test_parse_args_collects_ack_action(self):
        with patch.object(
            sys,
            "argv",
            [
                "gh_workflow_run_watch.py",
                "--once",
                "--ack-action",
                "fingerprint-1",
                "--ack-action",
                "fingerprint-2",
            ],
        ):
            args = MODULE.parse_args()
        self.assertEqual(args.ack_action, ["fingerprint-1", "fingerprint-2"])

    def test_parse_args_watch_until_terminal_implies_require_terminal(self):
        with patch.object(
            sys,
            "argv",
            ["gh_workflow_run_watch.py", "--watch-until-terminal", "--wait-for", "first_action"],
        ):
            args = MODULE.parse_args()
        self.assertTrue(args.watch_until_terminal)
        self.assertTrue(args.watch_until_action)
        self.assertTrue(args.require_terminal_run)

    def test_parse_args_wait_until_terminal_alias(self):
        with patch.object(
            sys,
            "argv",
            ["gh_workflow_run_watch.py", "--wait-until-terminal"],
        ):
            args = MODULE.parse_args()
        self.assertTrue(args.watch_until_terminal)
        self.assertTrue(args.watch_until_action)
        self.assertTrue(args.require_terminal_run)

    def test_normalize_snapshot_marks_in_progress_failed_job_actionable(self):
        run_view = {
            "databaseId": 42,
            "status": "in_progress",
            "conclusion": "",
            "url": "https://example.invalid/runs/42",
            "workflowName": "rust-ci",
            "jobs": [
                {
                    "databaseId": 501,
                    "name": "Tests — ubuntu",
                    "status": "completed",
                    "conclusion": "failure",
                    "url": "https://example.invalid/jobs/501",
                }
            ],
        }
        snapshot = MODULE.normalize_snapshot(
            run_view,
            target={"kind": MODULE.TARGET_KIND_RUN_ID, "run_id": 42, "spec": "run-id=42"},
            repo="sednalabs/codex",
            followed_newer_run=False,
            resolved_ref="integration/test",
            gemini_disabled=True,
        )
        snapshot["diagnostic_evidence"] = {
            "log_sources": [
                {
                    "kind": "failed_job_log",
                    "label": "Tests — ubuntu",
                    "job_id": 501,
                    "chars": 120,
                }
            ]
        }
        snapshot = MODULE._apply_acknowledged_actions(snapshot, [])
        self.assertEqual(snapshot["actions"], ["diagnose_run_failure"])
        self.assertEqual(snapshot["action_triggers"][0]["failure_phase"], "in_progress_failed_job")
        self.assertEqual(snapshot["action_triggers"][0]["job_id"], 501)
        self.assertTrue(snapshot["action_triggers"][0]["logs_available"])

    def test_ack_action_suppresses_repeat_failure(self):
        snapshot = {
            "actions": ["diagnose_run_failure"],
            "target": {"kind": MODULE.TARGET_KIND_RUN_ID, "run_id": 42, "spec": "run-id=42"},
            "run": {
                "id": 42,
                "url": "https://example.invalid/runs/42",
                "status": "completed",
                "conclusion": "failure",
            },
            "failed_jobs": [
                {
                    "id": 501,
                    "name": "Tests — ubuntu",
                    "url": "https://example.invalid/jobs/501",
                }
            ],
            "appearance_wait": None,
            "diagnostic_evidence": {"log_sources": []},
        }
        fingerprint = MODULE._action_descriptors_for_snapshot(snapshot)[0]["fingerprint"]
        snapshot = MODULE._apply_acknowledged_actions(snapshot, [fingerprint])
        self.assertEqual(snapshot["actions"], ["idle"])
        self.assertEqual(snapshot["suppressed_action_fingerprints"], [fingerprint])

    def test_skipped_gemini_diagnosis_is_not_an_error_state(self):
        status = MODULE._build_diagnosis_status(
            actions=["diagnose_run_failure"],
            gemini_diagnosis=None,
            gemini_error="Skipped Gemini diagnosis to avoid low-value token spend.",
            gemini_disabled=False,
        )
        alerts = MODULE._gemini_failure_alert("Skipped Gemini diagnosis to avoid low-value token spend.")
        self.assertEqual(status["state"], "skipped")
        self.assertEqual(alerts, [])

    def test_payload_has_in_progress_failure_even_with_mixed_aggregate_actions(self):
        payload = {
            "actions": ["stop_run_succeeded", "diagnose_run_failure"],
            "targets": [
                {
                    "actions": ["stop_run_succeeded"],
                    "run": {"status": "completed", "conclusion": "success"},
                },
                {
                    "actions": ["diagnose_run_failure"],
                    "run": {"status": "in_progress", "conclusion": ""},
                },
            ],
        }

        self.assertTrue(MODULE._payload_has_in_progress_failure(payload))

    def test_watch_until_action_waits_for_terminal_failures_per_target(self):
        args = types.SimpleNamespace(
            require_terminal_run=True,
            wait_for="all_done",
            poll_seconds=1,
            ack_action=[],
            verbose_details=False,
        )
        early_payload = {
            "actions": ["stop_run_succeeded", "diagnose_run_failure"],
            "summary": {"targets_idle": 0},
            "targets": [
                {
                    "actions": ["stop_run_succeeded"],
                    "run": {"status": "completed", "conclusion": "success"},
                },
                {
                    "actions": ["diagnose_run_failure"],
                    "run": {"status": "in_progress", "conclusion": ""},
                },
            ],
        }
        terminal_payload = {
            "actions": ["stop_run_succeeded", "diagnose_run_failure"],
            "summary": {"targets_idle": 0},
            "targets": [
                {
                    "actions": ["stop_run_succeeded"],
                    "run": {"status": "completed", "conclusion": "success"},
                },
                {
                    "actions": ["diagnose_run_failure"],
                    "run": {"status": "completed", "conclusion": "failure"},
                },
            ],
        }

        with patch.object(MODULE, "build_targets", return_value=[{"kind": "dummy"}]), patch.object(
            MODULE,
            "resolve_snapshot",
            side_effect=[early_payload, terminal_payload],
        ), patch.object(MODULE, "emit") as emit, patch.object(MODULE.time, "sleep", return_value=None) as sleep:
            MODULE.watch_until_action(args, "sednalabs/codex")

        emit.assert_called_once_with(terminal_payload)
        sleep.assert_called_once_with(1)

    def test_watch_until_action_all_done_waits_for_failure_logs_to_be_retrievable(self):
        args = types.SimpleNamespace(
            require_terminal_run=False,
            wait_for="all_done",
            poll_seconds=1,
            ack_action=[],
            verbose_details=False,
        )
        early_payload = {
            "actions": ["stop_run_succeeded", "diagnose_run_failure"],
            "summary": {"targets_idle": 0},
            "targets": [
                {
                    "actions": ["stop_run_succeeded"],
                    "run": {"status": "completed", "conclusion": "success"},
                    "action_triggers": [
                        {
                            "action": "stop_run_succeeded",
                            "fingerprint": "stop_run_succeeded:run:1",
                        }
                    ],
                },
                {
                    "actions": ["diagnose_run_failure"],
                    "run": {"status": "in_progress", "conclusion": ""},
                    "action_triggers": [
                        {
                            "action": "diagnose_run_failure",
                            "fingerprint": "diagnose_run_failure:run:2:phase:in_progress_failed_job:job:501",
                            "run_id": 2,
                            "run_url": "https://example.invalid/runs/2",
                            "job_id": 501,
                            "job_name": "Tests — ubuntu",
                            "failure_phase": "in_progress_failed_job",
                            "logs_available": False,
                        }
                    ],
                },
            ],
        }
        ready_payload = {
            "actions": ["stop_run_succeeded", "diagnose_run_failure"],
            "summary": {"targets_idle": 0},
            "targets": [
                {
                    "actions": ["stop_run_succeeded"],
                    "run": {"status": "completed", "conclusion": "success"},
                    "action_triggers": [
                        {
                            "action": "stop_run_succeeded",
                            "fingerprint": "stop_run_succeeded:run:1",
                        }
                    ],
                },
                {
                    "actions": ["diagnose_run_failure"],
                    "run": {"status": "in_progress", "conclusion": ""},
                    "action_triggers": [
                        {
                            "action": "diagnose_run_failure",
                            "fingerprint": "diagnose_run_failure:run:2:phase:in_progress_failed_job:job:501",
                            "run_id": 2,
                            "run_url": "https://example.invalid/runs/2",
                            "job_id": 501,
                            "job_name": "Tests — ubuntu",
                            "failure_phase": "in_progress_failed_job",
                            "logs_available": True,
                        }
                    ],
                },
            ],
        }

        with patch.object(MODULE, "build_targets", return_value=[{"kind": "dummy"}]), patch.object(
            MODULE,
            "resolve_snapshot",
            side_effect=[early_payload, ready_payload],
        ), patch.object(MODULE, "emit") as emit, patch.object(MODULE.time, "sleep", return_value=None) as sleep:
            MODULE.watch_until_action(args, "sednalabs/codex")

        emit.assert_called_once_with(ready_payload)
        sleep.assert_called_once_with(1)

    def test_watch_until_action_all_done_does_not_keep_polling_after_terminal_failure_without_logs(self):
        args = types.SimpleNamespace(
            require_terminal_run=False,
            wait_for="all_done",
            poll_seconds=1,
            ack_action=[],
            verbose_details=False,
        )
        terminal_payload_without_logs = {
            "actions": ["stop_run_succeeded", "diagnose_run_failure"],
            "summary": {"targets_idle": 0},
            "targets": [
                {
                    "actions": ["stop_run_succeeded"],
                    "run": {"status": "completed", "conclusion": "success"},
                    "action_triggers": [
                        {
                            "action": "stop_run_succeeded",
                            "fingerprint": "stop_run_succeeded:run:1",
                        }
                    ],
                },
                {
                    "actions": ["diagnose_run_failure"],
                    "run": {"status": "completed", "conclusion": "failure"},
                    "action_triggers": [
                        {
                            "action": "diagnose_run_failure",
                            "fingerprint": "diagnose_run_failure:run:2:phase:terminal_failure:job:501",
                            "run_id": 2,
                            "run_url": "https://example.invalid/runs/2",
                            "job_id": 501,
                            "job_name": "Tests — ubuntu",
                            "failure_phase": "terminal_failure",
                            "logs_available": False,
                        }
                    ],
                },
            ],
        }

        with patch.object(MODULE, "build_targets", return_value=[{"kind": "dummy"}]), patch.object(
            MODULE,
            "resolve_snapshot",
            return_value=terminal_payload_without_logs,
        ), patch.object(MODULE, "emit") as emit, patch.object(MODULE.time, "sleep", return_value=None) as sleep:
            MODULE.watch_until_action(args, "sednalabs/codex")

        emit.assert_called_once_with(terminal_payload_without_logs)
        sleep.assert_not_called()

    def test_watch_until_terminal_mode_stays_until_terminal(self):
        with patch.object(
            sys,
            "argv",
            [
                "gh_workflow_run_watch.py",
                "--watch-until-terminal",
                "--wait-for",
                "all_done",
                "--poll-seconds",
                "1",
            ],
        ):
            args = MODULE.parse_args()
        early_payload = {
            "actions": ["stop_run_succeeded", "diagnose_run_failure"],
            "summary": {"targets_idle": 0},
            "targets": [
                {
                    "actions": ["stop_run_succeeded"],
                    "run": {"status": "completed", "conclusion": "success"},
                },
                {
                    "actions": ["diagnose_run_failure"],
                    "run": {"status": "in_progress", "conclusion": ""},
                },
            ],
        }
        terminal_payload = {
            "actions": ["stop_run_succeeded", "diagnose_run_failure"],
            "summary": {"targets_idle": 0},
            "targets": [
                {
                    "actions": ["stop_run_succeeded"],
                    "run": {"status": "completed", "conclusion": "success"},
                },
                {
                    "actions": ["diagnose_run_failure"],
                    "run": {"status": "completed", "conclusion": "failure"},
                },
            ],
        }
        with patch.object(MODULE, "build_targets", return_value=[{"kind": "dummy"}]), patch.object(
            MODULE,
            "resolve_snapshot",
            side_effect=[early_payload, terminal_payload],
        ), patch.object(MODULE, "emit") as emit, patch.object(MODULE.time, "sleep", return_value=None) as sleep:
            MODULE.watch_until_action(args, "sednalabs/codex")

        emit.assert_called_once_with(terminal_payload)
        sleep.assert_called_once_with(1)


    def test_redaction_and_runner_path_mapping(self):
        with tempfile.TemporaryDirectory(prefix="repo-") as tmpdir:
            repo_root = Path(tmpdir) / "repo"
            (repo_root / "src").mkdir(parents=True)
            (repo_root / "src" / "lib.rs").write_text(
                "fn main() {}\nlet answer = 42;\npanic!(\"boom\");\n",
                encoding="utf-8",
            )
            (repo_root / "src" / "main.py").write_text(
                "def run():\n    raise RuntimeError('boom')\n",
                encoding="utf-8",
            )

            runner_path = "/home/runner/work/repo/repo/src/lib.rs"
            self.assertEqual(
                MODULE._normalize_repo_path(repo_root, runner_path),
                (repo_root / "src" / "lib.rs").resolve(),
            )

            log_text = "\n".join(
                [
                    "Traceback (most recent call last):",
                    '  File "src/main.py", line 2, in run',
                    "    raise RuntimeError('boom')",
                    "  at /home/runner/work/repo/repo/src/lib.rs:2:5",
                    "Authorization: Bearer ghp_example",
                    "password=secret",
                ]
            )
            contexts = MODULE._collect_code_context(repo_root, [log_text])
            paths = {item["path"] for item in contexts}
            self.assertIn(str((repo_root / "src" / "main.py").resolve()), paths)
            self.assertIn(str((repo_root / "src" / "lib.rs").resolve()), paths)
            self.assertEqual(MODULE._redact_text(log_text).count("<redacted>"), 2)

    def test_call_gemini_diagnosis_rotates_keys_and_parses_json(self):
        prompt_response = {
            "summary": "primary failure",
            "likely_root_cause": "bad config",
            "confidence": "medium",
            "next_steps": ["fix config", "rerun"],
            "suspect_paths": ["src/main.rs"],
            "evidence_notes": ["log mentions config"],
        }
        gemini_payload = {
            "candidates": [
                {
                    "content": {
                        "parts": [
                            {
                                "text": json.dumps(prompt_response),
                            }
                        ]
                    }
                }
            ]
        }
        gemini_usage = {
            "promptTokenCount": 111,
            "candidatesTokenCount": 22,
            "totalTokenCount": 133,
        }
        gemini_response = {
            "responseId": "resp-123",
            "modelVersion": "gemini-3.1-flash-lite-preview-001",
            "usageMetadata": gemini_usage,
            **gemini_payload,
        }
        calls = []

        class FakeResponse:
            def __init__(self, body):
                self._body = body

            def __enter__(self):
                return self

            def __exit__(self, exc_type, exc, tb):
                return False

            def read(self):
                return self._body

        def fake_urlopen(request, timeout=None):
            headers = {key.lower(): value for key, value in request.header_items()}
            calls.append((request.full_url, headers.get("x-goog-api-key"), timeout))
            if len(calls) == 1:
                raise urllib.error.HTTPError(
                    request.full_url,
                    429,
                    "Too Many Requests",
                    hdrs=None,
                    fp=io.BytesIO(b"rate limit"),
                )
            return FakeResponse(json.dumps(gemini_response).encode("utf-8"))

        with patch.dict(os.environ, {"GEMINI_API_KEYS": "bad-key, good-key"}, clear=True):
            with patch.object(MODULE.urllib.request, "urlopen", side_effect=fake_urlopen), patch.object(
                MODULE.time, "sleep", return_value=None
            ), patch.object(MODULE.time, "perf_counter", side_effect=[100.0, 100.05, 100.125]):
                diagnosis, telemetry = MODULE._call_gemini_diagnosis(
                    model=MODULE.GEMINI_DEFAULT_MODEL,
                    prompt="diagnose me",
                    timeout_seconds=3,
                )

        self.assertEqual(len(calls), 2)
        self.assertTrue(calls[0][0].endswith(f"/models/{MODULE.GEMINI_DEFAULT_MODEL}:generateContent"))
        self.assertEqual(calls[0][1], "bad-key")
        self.assertEqual(calls[1][1], "good-key")
        self.assertEqual(diagnosis["model"], MODULE.GEMINI_DEFAULT_MODEL)
        self.assertEqual(diagnosis["confidence"], "medium")
        self.assertEqual(diagnosis["next_steps"], ["fix config", "rerun"])
        self.assertEqual(telemetry["model"], MODULE.GEMINI_DEFAULT_MODEL)
        self.assertEqual(telemetry["attempts"], 2)
        self.assertEqual(telemetry["latency_ms"], 125)
        self.assertEqual(telemetry["response_id"], "resp-123")
        self.assertEqual(telemetry["model_version"], "gemini-3.1-flash-lite-preview-001")
        self.assertEqual(telemetry["usage_metadata"]["prompt_token_count"], 111)
        self.assertEqual(telemetry["usage_metadata"]["candidates_token_count"], 22)
        self.assertEqual(telemetry["usage_metadata"]["total_token_count"], 133)

    def test_collect_log_sources_focuses_primary_failure_and_skips_cancelled_log_spam(self):
        run_view = {
            "databaseId": 321,
            "jobs": [
                {
                    "databaseId": 10,
                    "name": "Tests - ubuntu",
                    "status": "completed",
                    "conclusion": "failure",
                    "url": "https://example.invalid/job/10",
                    "steps": [
                        {"name": "Set up job", "conclusion": "success"},
                        {"name": "tests", "conclusion": "failure"},
                    ],
                },
                {
                    "databaseId": 11,
                    "name": "CI results (required)",
                    "status": "completed",
                    "conclusion": "failure",
                    "url": "https://example.invalid/job/11",
                    "steps": [
                        {"name": "Summarize", "conclusion": "failure"},
                    ],
                },
                {
                    "databaseId": 12,
                    "name": "Lint/Build - macos",
                    "status": "completed",
                    "conclusion": "cancelled",
                    "url": "https://example.invalid/job/12",
                    "steps": [
                        {"name": "cargo clippy", "conclusion": "cancelled"},
                    ],
                },
            ],
        }

        load_calls = []

        def fake_load_job_log_text(repo, job_id):
            load_calls.append(job_id)
            return (
                "\n".join(
                    [
                        "step header",
                        f"job {job_id} doing work",
                        "tests failed with panic",
                        "error: assertion failed",
                    ]
                ),
                "mocked",
            )

        with patch.object(MODULE, "gh_text", return_value="workflow log failure tail"), patch.object(
            MODULE, "_load_job_log_text", side_effect=fake_load_job_log_text
        ):
            sources = MODULE._collect_log_sources("owner/repo", run_view, validation_summary=None)

        self.assertEqual(load_calls, [10, 11])
        self.assertEqual(sources[0]["kind"], "failed_jobs_overview")
        self.assertNotIn(12, [source.get("job_id") for source in sources if source.get("job_id") is not None])

    def test_no_gemini_mode_keeps_diagnostic_evidence_bundle(self):
        args = types.SimpleNamespace(
            no_gemini_diagnosis=True,
            gemini_model=MODULE.GEMINI_DEFAULT_MODEL,
            gemini_timeout_seconds=MODULE.GEMINI_DEFAULT_TIMEOUT_SECONDS,
            appearance_timeout_seconds=0,
        )
        target = {"kind": MODULE.TARGET_KIND_RUN_ID, "run_id": 321, "spec": "run-id=321"}
        run_view = {
            "databaseId": 321,
            "status": "completed",
            "conclusion": "failure",
            "headBranch": "integration/test",
            "jobs": [
                {
                    "databaseId": 10,
                    "name": "Tests - ubuntu",
                    "status": "completed",
                    "conclusion": "failure",
                    "url": "https://example.invalid/job/10",
                    "steps": [{"name": "tests", "conclusion": "failure"}],
                }
            ],
        }

        with patch.object(MODULE, "view_run", return_value=run_view), patch.object(
            MODULE, "normalize_snapshot",
            return_value={
                "actions": ["diagnose_run_failure"],
                "validation_summary": None,
                "failed_jobs": [{"id": 10, "name": "Tests - ubuntu"}],
            },
        ), patch.object(
            MODULE,
            "_collect_failure_evidence",
            return_value={"evidence": {"failed_job_count": 1, "structured_failure_signals": ["panic"]}},
        ), patch.object(MODULE, "_diagnose_failure") as diagnose_failure:
            snapshot = MODULE.target_state_from_target(args, target, "owner/repo", {})

        diagnose_failure.assert_not_called()
        self.assertEqual(snapshot["diagnosis_status"]["state"], "disabled")
        self.assertEqual(snapshot["gemini_diagnosis"], None)
        self.assertEqual(snapshot["gemini_error"], None)
        self.assertEqual(snapshot["diagnostic_evidence"]["failed_job_count"], 1)

    def test_collect_log_sources_frontier_prefers_summary_and_one_primary_job(self):
        run_view = {
            "databaseId": 321,
            "jobs": [
                {
                    "databaseId": 10,
                    "name": "Tests - ubuntu",
                    "status": "completed",
                    "conclusion": "failure",
                    "url": "https://example.invalid/job/10",
                    "steps": [{"name": "tests", "conclusion": "failure"}],
                },
                {
                    "databaseId": 11,
                    "name": "CI results (required)",
                    "status": "completed",
                    "conclusion": "failure",
                    "url": "https://example.invalid/job/11",
                    "steps": [{"name": "Summarize", "conclusion": "failure"}],
                },
            ],
        }
        validation_summary = {
            "selection": {"profile": "frontier", "lane_set": "subagents"},
            "summary": {
                "failed_lane_count": 2,
                "first_failure": {"lane_id": "lane-a", "signal": "thread panicked"},
                "candidate_next_slices": [{"lane_id": "lane-a", "signal": "thread panicked"}],
            },
        }

        with patch.object(MODULE, "gh_text", return_value="workflow log failure tail"), patch.object(
            MODULE,
            "_load_job_log_text",
            side_effect=lambda repo, job_id: (f"job {job_id}\nthread 'x' panicked at src/main.rs:10:5", "mocked"),
        ):
            sources = MODULE._collect_log_sources(
                "owner/repo",
                run_view,
                validation_summary=validation_summary,
            )

        self.assertEqual(sources[0]["kind"], "failed_jobs_overview")
        self.assertEqual(sources[1]["kind"], "validation_summary")
        detailed_logs = [source for source in sources if source["kind"] == "failed_job_log"]
        self.assertEqual([source["job_id"] for source in detailed_logs], [10])

    def test_focus_job_log_text_includes_step_context_and_highlights(self):
        job = {
            "id": 10,
            "name": "Tests - ubuntu",
            "conclusion": "failure",
            "failed_steps": ["tests"],
        }
        log_text = "\n".join(
            [
                "bootstrap",
                "Running tests",
                "tests",
                "thread 'main' panicked at src/main.rs:10:5",
                "error: assertion failed: left == right",
                "tail",
            ]
        )

        focused = MODULE._focus_job_log_text(job, log_text)

        self.assertIn("== Failed step: tests ==", focused)
        self.assertIn("== Failure highlights ==", focused)
        self.assertIn("assertion failed", focused)

    def test_focus_job_log_text_prefers_exact_failed_step_signal_over_early_step_noise(self):
        job = {
            "id": 10,
            "name": "Tests - ubuntu",
            "conclusion": "failure",
            "failed_steps": ["tests"],
        }
        log_text = "\n".join(
            [
                "Tests - ubuntu\ttests\t2026-03-31T21:00:00Z\tCurrent runner version: '2.333.1'",
                "Tests - ubuntu\ttests\t2026-03-31T21:00:01Z\tsetup still running",
                "Tests - ubuntu\ttests\t2026-03-31T21:03:00Z\ttest suite::v2::review::review_start_runs_review_turn_and_emits_code_review_item ... FAILED",
                "Tests - ubuntu\ttests\t2026-03-31T21:03:01Z\tthread 'suite::v2::review::review_start_runs_review_turn_and_emits_code_review_item' panicked at app-server/tests/suite/v2/review.rs:140:5:",
                "Tests - ubuntu\ttests\t2026-03-31T21:03:02Z\tassertion failed: review.contains(\"Token usage: unavailable\")",
            ]
        )

        focused = MODULE._focus_job_log_text(job, log_text)

        self.assertIn("review_start_runs_review_turn_and_emits_code_review_item", focused)
        self.assertIn("Token usage: unavailable", focused)
        self.assertNotIn("Current runner version", focused)

    def test_extract_structured_failure_signals_finds_test_assertion_and_location(self):
        log_text = "\n".join(
            [
                "Tests - ubuntu\ttests\t2026-03-31T21:03:00Z\ttest suite::v2::review::review_start_runs_review_turn_and_emits_code_review_item ... FAILED",
                "Tests - ubuntu\ttests\t2026-03-31T21:03:01Z\tthread 'suite::v2::review::review_start_runs_review_turn_and_emits_code_review_item' panicked at app-server/tests/suite/v2/review.rs:140:5:",
                "Tests - ubuntu\ttests\t2026-03-31T21:03:02Z\tassertion failed: review.contains(\"Token usage: unavailable\")",
            ]
        )

        signals = MODULE._extract_structured_failure_signals(log_text)

        self.assertIn(
            "suite::v2::review::review_start_runs_review_turn_and_emits_code_review_item",
            signals["failing_tests"][0],
        )
        self.assertIn("Token usage: unavailable", signals["assertions"][0])
        self.assertIn("app-server/tests/suite/v2/review.rs:140", signals["failure_locations"][0])

    def test_build_gemini_prompt_calls_out_causal_analysis_rules(self):
        prompt = MODULE._build_gemini_prompt(
            repo="owner/repo",
            run_view={
                "databaseId": 7,
                "number": 99,
                "workflowName": "rust-ci",
                "headBranch": "main",
                "headSha": "abcdef",
                "status": "completed",
                "conclusion": "failure",
                "url": "https://example.invalid/run/7",
            },
            validation_summary=None,
            log_sources=[],
            code_context=[],
        )

        self.assertIn("Find the earliest causal failure", prompt)
        self.assertIn("The logs below are focused excerpts", prompt)
        self.assertIn("## Analysis priorities", prompt)

    def test_build_gemini_prompt_includes_validation_mode_context(self):
        prompt = MODULE._build_gemini_prompt(
            repo="owner/repo",
            run_view={
                "databaseId": 7,
                "number": 99,
                "workflowName": "validation-lab",
                "headBranch": "main",
                "headSha": "abcdef",
                "status": "completed",
                "conclusion": "failure",
                "url": "https://example.invalid/run/7",
                "jobs": [],
            },
            validation_summary={
                "selection": {"profile": "frontier", "lane_set": "subagents"},
                "summary": {
                    "failed_lane_count": 2,
                    "first_failure": {"lane_id": "lane-a", "signal": "panic"},
                    "candidate_next_slices": [{"lane_id": "lane-b", "signal": "error"}],
                },
            },
            log_sources=[],
            code_context=[],
        )

        self.assertIn("## Validation mode context", prompt)
        self.assertIn("frontier_harvest", prompt)
        self.assertIn("failure_structure", prompt)
        self.assertIn("recommended_follow_up", prompt)

    def test_derive_validation_mode_context_frontier_marks_independent(self):
        context = MODULE._derive_validation_mode_context(
            {
                "selection": {
                    "profile": "frontier",
                    "lane_set": "subagents",
                    "explicit_lanes": ["lane-a"],
                    "baseline_required": True,
                },
                "jobs": {
                    "smoke_gate": {"result": "success"},
                    "downstream_lanes": {"result": "failure"},
                    "artifact": {"result": "skipped"},
                },
                "summary": {
                    "failed_lane_count": 2,
                    "first_failure": {"lane_id": "lane-a", "signal": "thread panicked"},
                    "candidate_next_slices": [
                        {"lane_id": "lane-a", "signal": "thread panicked"},
                        {"lane_id": "lane-b", "signal": "assertion failed"},
                    ],
                },
            },
            failed_jobs=[
                {"id": 10, "name": "lane-a", "conclusion": "failure"},
                {"id": 11, "name": "lane-b", "conclusion": "failure"},
            ],
        )

        self.assertEqual(context["profile"], "frontier")
        self.assertEqual(context["failure_structure"], "independent")
        self.assertEqual(context["recommended_follow_up"], "frontier_harvest")
        self.assertEqual(context["first_blocker"]["lane_id"], "lane-a")

    def test_derive_validation_mode_context_prefers_targeted_repair_for_one_direct_failure(self):
        context = MODULE._derive_validation_mode_context(
            None,
            failed_jobs=[
                {"id": 10, "name": "Tests - ubuntu", "conclusion": "failure"},
                {"id": 11, "name": "CI results (required)", "conclusion": "failure"},
                {"id": 12, "name": "Lint/Build - macos", "conclusion": "cancelled"},
            ],
        )

        self.assertEqual(context["failure_structure"], "cascading")
        self.assertEqual(context["recommended_follow_up"], "targeted_repair")
        self.assertEqual(context["first_blocker"]["job_name"], "Tests - ubuntu")

    def test_target_state_caches_gemini_diagnosis_once(self):
        run_view = {
            "databaseId": 42,
            "number": 7,
            "displayTitle": "workflow run",
            "workflowName": "validation-lab",
            "url": "https://example.invalid/run/42",
            "headBranch": "main",
            "headSha": "abcdef123456",
            "event": "push",
            "status": "completed",
            "conclusion": "failure",
            "createdAt": "2026-03-31T00:00:00Z",
            "updatedAt": "2026-03-31T00:05:00Z",
            "jobs": [
                {
                    "databaseId": 9001,
                    "name": "tests",
                    "status": "completed",
                    "conclusion": "failure",
                    "url": "https://example.invalid/job/9001",
                }
            ],
        }
        target = {"kind": MODULE.TARGET_KIND_RUN_ID, "run_id": 42, "spec": "run-id=42"}
        args = types.SimpleNamespace(
            no_gemini_diagnosis=False,
            gemini_model=MODULE.GEMINI_DEFAULT_MODEL,
            gemini_timeout_seconds=5.0,
            appearance_timeout_seconds=0,
            wait_for="first_action",
        )
        diagnosis = {
            "model": MODULE.GEMINI_DEFAULT_MODEL,
            "summary": "root cause",
            "likely_root_cause": "bad flag",
            "confidence": "high",
            "next_steps": ["fix flag"],
            "suspect_paths": ["src/main.rs"],
            "evidence_notes": ["log line"],
        }
        telemetry = {
            "model": MODULE.GEMINI_DEFAULT_MODEL,
            "attempts": 1,
            "latency_ms": 17,
            "usage_metadata": {
                "prompt_token_count": 42,
                "candidates_token_count": 7,
                "total_token_count": 49,
            },
            "response_id": "resp-cached",
        }
        evidence = {
            "redaction_applied": True,
            "truncated": False,
            "failed_job_count": 1,
            "log_chars_sent": 123,
            "log_sources": [],
            "code_context_paths": ["src/main.rs"],
            "structured_failure_signals": {"failing_tests": ["suite::x"], "assertions": [], "failure_locations": [], "evidence_lines": []},
            "validation_context": {"profile": "targeted"},
        }

        diagnose = Mock(return_value=(diagnosis, evidence, telemetry))
        with patch.object(MODULE, "view_run", return_value=run_view), patch.object(
            MODULE, "load_validation_summary", return_value={"validation": "summary"}
        ), patch.object(MODULE, "_diagnose_failure", diagnose):
            remembered = {}
            snapshot1 = MODULE.target_state_from_target(args, target, "owner/repo", remembered)
            snapshot2 = MODULE.target_state_from_target(args, target, "owner/repo", remembered)

        self.assertEqual(diagnose.call_count, 1)
        self.assertEqual(snapshot1["actions"], ["diagnose_run_failure"])
        self.assertEqual(snapshot1["gemini_diagnosis"], diagnosis)
        self.assertEqual(snapshot1["diagnostic_evidence"], evidence)
        self.assertEqual(snapshot1["gemini_telemetry"], telemetry)
        self.assertEqual(snapshot2["gemini_diagnosis"], diagnosis)
        self.assertEqual(snapshot2["gemini_error"], None)
        self.assertEqual(snapshot2["diagnostic_evidence"], evidence)
        self.assertEqual(snapshot2["gemini_telemetry"], telemetry)
        self.assertEqual(snapshot1["failed_jobs"][0]["id"], 9001)
        self.assertEqual(snapshot1["diagnosis_status"]["state"], "available")

    def test_target_state_skips_gemini_when_disabled(self):
        run_view = {
            "databaseId": 99,
            "number": 12,
            "displayTitle": "workflow run",
            "workflowName": "validation-lab",
            "url": "https://example.invalid/run/99",
            "headBranch": "main",
            "headSha": "abcdef123456",
            "event": "push",
            "status": "completed",
            "conclusion": "failure",
            "createdAt": "2026-03-31T00:00:00Z",
            "updatedAt": "2026-03-31T00:05:00Z",
            "jobs": [],
        }
        target = {"kind": MODULE.TARGET_KIND_RUN_ID, "run_id": 99, "spec": "run-id=99"}
        args = types.SimpleNamespace(
            no_gemini_diagnosis=True,
            gemini_model=MODULE.GEMINI_DEFAULT_MODEL,
            gemini_timeout_seconds=5.0,
            appearance_timeout_seconds=0,
            wait_for="first_action",
        )
        with patch.object(MODULE, "view_run", return_value=run_view), patch.object(
            MODULE, "load_validation_summary", return_value=None
        ), patch.object(MODULE, "_diagnose_failure") as diagnose:
            snapshot = MODULE.target_state_from_target(args, target, "owner/repo", {})

        diagnose.assert_not_called()
        self.assertIsNone(snapshot["gemini_diagnosis"])
        self.assertIsNone(snapshot["gemini_error"])
        self.assertIsInstance(snapshot["diagnostic_evidence"], dict)
        self.assertIsNone(snapshot["gemini_telemetry"])
        self.assertEqual(snapshot["diagnosis_status"]["state"], "disabled")

    def test_target_state_alerts_when_gemini_fails(self):
        run_view = {
            "databaseId": 100,
            "number": 13,
            "displayTitle": "workflow run",
            "workflowName": "validation-lab",
            "url": "https://example.invalid/run/100",
            "headBranch": "main",
            "headSha": "abcdef123456",
            "event": "push",
            "status": "completed",
            "conclusion": "failure",
            "createdAt": "2026-03-31T00:00:00Z",
            "updatedAt": "2026-03-31T00:05:00Z",
            "jobs": [],
        }
        target = {"kind": MODULE.TARGET_KIND_RUN_ID, "run_id": 100, "spec": "run-id=100"}
        args = types.SimpleNamespace(
            no_gemini_diagnosis=False,
            gemini_model=MODULE.GEMINI_DEFAULT_MODEL,
            gemini_timeout_seconds=5.0,
            appearance_timeout_seconds=0,
            wait_for="first_action",
        )
        evidence = {
            "redaction_applied": True,
            "truncated": False,
            "failed_job_count": 0,
            "log_chars_sent": 0,
            "log_sources": [],
            "code_context_paths": [],
            "structured_failure_signals": {"failing_tests": [], "assertions": [], "failure_locations": [], "evidence_lines": []},
            "validation_context": {"profile": "checkpoint"},
        }
        telemetry = {
            "model": MODULE.GEMINI_DEFAULT_MODEL,
            "attempts": 1,
            "latency_ms": 9,
            "usage_metadata": None,
        }

        with patch.object(MODULE, "view_run", return_value=run_view), patch.object(
            MODULE, "load_validation_summary", return_value=None
        ), patch.object(
            MODULE,
            "_diagnose_failure",
            side_effect=MODULE.GeminiDiagnosisError("Gemini down", evidence=evidence, telemetry=telemetry),
        ):
            snapshot = MODULE.target_state_from_target(args, target, "owner/repo", {})

        self.assertEqual(snapshot["actions"], ["diagnose_run_failure"])
        self.assertEqual(snapshot["gemini_error"], "Gemini down")
        self.assertEqual(snapshot["diagnostic_evidence"], evidence)
        self.assertEqual(snapshot["gemini_telemetry"], telemetry)
        self.assertEqual(snapshot["alerts"][0]["kind"], "gemini_diagnosis_failed")
        self.assertIn("Gemini diagnosis failed", snapshot["alerts"][0]["message"])
        self.assertIn("Gemini down", snapshot["alerts"][0]["details"])
        self.assertEqual(snapshot["diagnosis_status"]["state"], "unavailable")

    def test_compact_snapshot_keeps_failure_bundle_and_trims_default_details(self):
        snapshot = {
            "target": {
                "kind": MODULE.TARGET_KIND_WORKFLOW,
                "workflow": "validation-lab",
                "ref": "integration/test",
                "spec": "workflow=validation-lab,ref=integration/test",
            },
            "repo": "sednalabs/codex",
            "resolved_ref": "integration/test",
            "run": {
                "id": 99,
                "number": 7,
                "name": "run name",
                "workflow_name": "rust-ci",
                "url": "https://example.invalid/run/99",
                "head_branch": "integration/test",
                "head_sha": "abcdef123456",
                "event": "workflow_dispatch",
                "status": "completed",
                "conclusion": "failure",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:10:00Z",
            },
            "failed_jobs": [
                {
                    "id": 501,
                    "name": "Tests",
                    "status": "completed",
                    "conclusion": "failure",
                    "url": "https://example.invalid/job/501",
                }
            ],
            "validation_summary": {"large": "payload"},
            "appearance_wait": None,
            "followed_newer_run": False,
            "gemini_diagnosis": None,
            "gemini_error": None,
            "diagnostic_evidence": {"failed_job_count": 1, "structured_failure_signals": {"failing_tests": ["x"]}},
            "gemini_telemetry": None,
            "validation_context": {
                "profile": "targeted",
                "failure_structure": "single_blocker",
                "recommended_follow_up": "targeted_repair",
                "first_blocker": {"job_id": 501},
                "candidate_next_slices": [],
                "failed_lane_count": 1,
                "lane_set": "subagents",
            },
            "diagnosis_status": {"state": "disabled", "summary": "Gemini diagnosis disabled"},
            "alerts": [],
            "actions": ["diagnose_run_failure"],
            "ts": 123,
        }

        compact = MODULE._compact_snapshot(snapshot, verbose_details=False)

        self.assertNotIn("repo", compact)
        self.assertNotIn("validation_summary", compact)
        self.assertEqual(compact["diagnosis_status"], {"state": "disabled"})
        self.assertEqual(compact["failed_jobs"], [{"id": 501, "name": "Tests", "conclusion": "failure"}])
        self.assertEqual(compact["run"]["id"], 99)
        self.assertNotIn("name", compact["run"])
        self.assertEqual(compact["diagnostic_evidence"]["failed_job_count"], 1)
        self.assertEqual(compact["validation_context"]["profile"], "targeted")
        self.assertNotIn("lane_set", compact["validation_context"])

    def test_compact_snapshot_verbose_details_passthrough(self):
        snapshot = {
            "repo": "sednalabs/codex",
            "target": {"kind": MODULE.TARGET_KIND_RUN_ID, "run_id": 42, "spec": "run-id=42"},
            "run": {"id": 42, "name": "full name", "status": "completed", "conclusion": "success"},
            "failed_jobs": [],
            "validation_summary": {"large": "payload"},
            "diagnosis_status": {"state": "not_needed", "summary": "ok"},
            "actions": ["stop_run_succeeded"],
            "ts": 123,
        }

        verbose = MODULE._compact_snapshot(snapshot, verbose_details=True)

        self.assertEqual(verbose, snapshot)


if __name__ == "__main__":
    unittest.main()
