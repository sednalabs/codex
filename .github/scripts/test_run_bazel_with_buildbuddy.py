#!/usr/bin/env python3

import json
import os
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import run_bazel_with_buildbuddy


class RunBazelWithBuildBuddyTest(unittest.TestCase):
    def github_env(
        self,
        temp_dir: str,
        *,
        repository: str = "openai/codex",
        fork: bool = False,
        event_name: str = "pull_request",
    ) -> dict[str, str]:
        event_path = Path(temp_dir) / "event.json"
        event_path.write_text(
            json.dumps({"pull_request": {"head": {"repo": {"fork": fork}}}}),
            encoding="utf-8",
        )
        return {
            "BUILDBUDDY_API_KEY": "token",
            "GITHUB_ACTIONS": "true",
            "GITHUB_EVENT_NAME": event_name,
            "GITHUB_EVENT_PATH": str(event_path),
            "GITHUB_REPOSITORY": repository,
        }

    def test_keyless_invocation_drops_remote_ci_configuration(self) -> None:
        self.assertIsNone(
            run_bazel_with_buildbuddy.remote_config(
                ["build", "--config=ci-linux", "//codex-rs/cli:codex"],
                {},
            )
        )
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                ["build", "--config=ci-linux", "--", "//codex-rs/cli:codex"],
                {},
            ),
            ["build", "--", "//codex-rs/cli:codex"],
        )

    def test_hosted_only_fork_callers_withhold_optional_remote_credentials(self) -> None:
        workflow_root = Path(__file__).resolve().parents[1] / "workflows"
        expected = (
            "BUILDBUDDY_API_KEY: ${{ github.repository != 'sednalabs/codex' "
            "&& secrets.BUILDBUDDY_API_KEY || '' }}"
        )
        for workflow in (
            "bazel.yml",
            "native-windows-bazel-health.yml",
            "rust-ci-full.yml",
            "sdk.yml",
            "v8-canary.yml",
        ):
            with self.subTest(workflow=workflow):
                source = (workflow_root / workflow).read_text(encoding="utf-8")
                assignments = [
                    line.strip()
                    for line in source.splitlines()
                    if line.strip().startswith("BUILDBUDDY_API_KEY:")
                ]
                self.assertTrue(assignments)
                self.assertTrue(all(line == expected for line in assignments))

    def test_keyless_hosted_fallback_preserves_every_remote_ci_target(self) -> None:
        for config in run_bazel_with_buildbuddy.REMOTE_EXECUTION_CONFIGS:
            with self.subTest(config=config):
                self.assertEqual(
                    run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                        ["test", config, "--", "//codex-rs/cli:codex", "//..."], {}
                    ),
                    ["test", "--", "//codex-rs/cli:codex", "//..."],
                )

    def test_keyless_hosted_fork_retains_only_shared_local_ci_settings(self) -> None:
        env = {
            "GITHUB_ACTIONS": "true",
            "GITHUB_REPOSITORY": "sednalabs/codex",
            "RUNNER_ENVIRONMENT": "github-hosted",
        }
        for config in run_bazel_with_buildbuddy.REMOTE_EXECUTION_CONFIGS:
            with self.subTest(config=config):
                self.assertEqual(
                    run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                        [
                            "--output_user_root=/tmp/test-bazel-output",
                            "test",
                            config,
                            "--test_tag_filters=-argument-comment-lint",
                            "--",
                            "//codex-rs/cli:codex",
                            "//...",
                        ],
                        env,
                    ),
                    [
                        "--output_user_root=/tmp/test-bazel-output",
                        "test",
                        "--config=ci-bazel",
                        "--test_tag_filters=-argument-comment-lint",
                        "--",
                        "//codex-rs/cli:codex",
                        "//...",
                    ],
                )

    def test_shared_local_ci_fallback_is_limited_to_this_hosted_fork(self) -> None:
        for env in (
            {},
            {"GITHUB_REPOSITORY": "sednalabs/codex"},
            {
                "GITHUB_REPOSITORY": "sednalabs/codex",
                "RUNNER_ENVIRONMENT": "github-hosted",
            },
            {
                "GITHUB_ACTIONS": "false",
                "GITHUB_REPOSITORY": "sednalabs/codex",
                "RUNNER_ENVIRONMENT": "github-hosted",
            },
            {
                "GITHUB_ACTIONS": "true",
                "GITHUB_REPOSITORY": "openai/codex",
                "RUNNER_ENVIRONMENT": "github-hosted",
            },
            {
                "GITHUB_ACTIONS": "true",
                "GITHUB_REPOSITORY": "example/codex",
                "RUNNER_ENVIRONMENT": "github-hosted",
            },
            {"GITHUB_ACTIONS": "true", "GITHUB_REPOSITORY": "sednalabs/codex"},
            {
                "GITHUB_ACTIONS": "true",
                "GITHUB_REPOSITORY": "sednalabs/codex",
                "RUNNER_ENVIRONMENT": "self-hosted",
            },
        ):
            with self.subTest(env=env):
                self.assertEqual(
                    run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                        ["build", "--config=ci-linux", "--", "//codex-rs/cli:codex"],
                        env,
                    ),
                    ["build", "--", "//codex-rs/cli:codex"],
                )

    def test_shared_ci_config_is_lean_without_remote_endpoints_or_disk_cas(self) -> None:
        bazelrc = Path(__file__).resolve().parents[2] / ".bazelrc"
        shared_ci = [
            line
            for line in bazelrc.read_text(encoding="utf-8").splitlines()
            if line.startswith(("common:ci ", "common:ci-bazel "))
        ]
        self.assertIn("common:ci-bazel --config=ci", shared_ci)
        self.assertIn("common:ci --disk_cache=", shared_ci)
        self.assertIn(
            "common:ci --@rules_rust//rust/settings:extra_rustc_flag=-Cdebuginfo=0",
            shared_ci,
        )
        self.assertIn(
            "common:ci --@rules_rust//rust/settings:extra_exec_rustc_flag=-Cdebuginfo=0",
            shared_ci,
        )
        for forbidden in (
            "--config=buildbuddy-",
            "--config=remote",
            "--remote_executor=",
            "--remote_cache=",
            "--bes_backend=",
            "--experimental_remote_downloader=",
            "--strategy=remote",
            "--platforms=//:rbe",
            "--extra_execution_platforms=",
        ):
            with self.subTest(forbidden=forbidden):
                self.assertFalse(any(forbidden in line for line in shared_ci))

    def test_program_arguments_after_separator_do_not_select_or_lose_rbe(self) -> None:
        args = ["run", "//codex-rs/cli:codex", "--", "--config=remote"]

        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_args_with_remote_config(args, {}),
            args,
        )
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                ["run", "//codex-rs/cli:codex", "--", "--config=ci-linux"],
                {
                    "GITHUB_ACTIONS": "true",
                    "GITHUB_REPOSITORY": "sednalabs/codex",
                    "RUNNER_ENVIRONMENT": "github-hosted",
                },
            ),
            ["run", "//codex-rs/cli:codex", "--", "--config=ci-linux"],
        )
        self.assertEqual(
            run_bazel_with_buildbuddy.remote_config(
                args, {"BUILDBUDDY_API_KEY": "fork-token"}
            ),
            "buildbuddy-generic",
        )

    def test_upstream_push_selects_openai_rbe_before_target_separator(self) -> None:
        with TemporaryDirectory() as temp_dir:
            env = self.github_env(temp_dir, event_name="push")

            self.assertEqual(
                run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                    ["build", "--config=ci-linux", "--", "//codex-rs/cli:codex"],
                    env,
                ),
                [
                    "build",
                    "--config=buildbuddy-openai-rbe",
                    "--remote_header=x-buildbuddy-api-key=token",
                    "--config=ci-linux",
                    "--",
                    "//codex-rs/cli:codex",
                ],
            )

    def test_windows_cross_ci_configuration_follows_remote_configuration(self) -> None:
        env = {"BUILDBUDDY_API_KEY": "fork-token"}

        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                ["build", "--config=ci-windows-cross", "//codex-rs/cli:codex"],
                env,
            ),
            [
                "build",
                "--config=buildbuddy-generic-rbe",
                "--remote_header=x-buildbuddy-api-key=fork-token",
                "--config=ci-windows-cross",
                "//codex-rs/cli:codex",
            ],
        )

    def test_query_remote_configuration_is_inserted_before_expression(self) -> None:
        expression = 'kind("rust_library rule", //codex-rs/...)'
        env = {"BUILDBUDDY_API_KEY": "fork-token"}

        for command in ("query", "cquery", "aquery"):
            with self.subTest(command=command):
                self.assertEqual(
                    run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                        [
                            command,
                            "--config=ci-windows-cross",
                            "--output=label",
                            expression,
                        ],
                        env,
                    ),
                    [
                        command,
                        "--config=buildbuddy-generic-rbe",
                        "--remote_header=x-buildbuddy-api-key=fork-token",
                        "--config=ci-windows-cross",
                        "--output=label",
                        expression,
                    ],
                )

    def test_same_repository_pull_request_selects_openai_host(self) -> None:
        with TemporaryDirectory() as temp_dir:
            self.assertEqual(
                run_bazel_with_buildbuddy.remote_config(
                    ["build", "--config=ci-v8"], self.github_env(temp_dir)
                ),
                "buildbuddy-openai-rbe",
            )

    def test_fork_pull_request_cannot_select_openai_host(self) -> None:
        with TemporaryDirectory() as temp_dir:
            env = self.github_env(temp_dir, fork=True)

            self.assertEqual(
                run_bazel_with_buildbuddy.remote_config(
                    ["build", "--config=ci-v8"], env
                ),
                "buildbuddy-generic-rbe",
            )

    def test_run_in_fork_repository_cannot_select_openai_host(self) -> None:
        with TemporaryDirectory() as temp_dir:
            env = self.github_env(temp_dir, repository="contributor/codex")

            self.assertEqual(
                run_bazel_with_buildbuddy.remote_config(
                    ["build", "--config=ci-v8"], env
                ),
                "buildbuddy-generic-rbe",
            )

    def test_pull_request_without_readable_event_payload_fails_closed(self) -> None:
        for event_path in (None, "missing-event.json"):
            env = {
                "BUILDBUDDY_API_KEY": "token",
                "GITHUB_ACTIONS": "true",
                "GITHUB_EVENT_NAME": "pull_request",
                "GITHUB_REPOSITORY": "openai/codex",
            }
            if event_path is not None:
                env["GITHUB_EVENT_PATH"] = event_path

            with self.subTest(event_path=event_path):
                self.assertEqual(
                    run_bazel_with_buildbuddy.remote_config(["build"], env),
                    "buildbuddy-generic",
                )

    def test_bazel_command_uses_configured_binary_locally(self) -> None:
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command(
                "info",
                "execution_root",
                env={"CODEX_BAZEL_BIN": "fake-bazel"},
            ),
            ["fake-bazel", "info", "execution_root"],
        )

    def test_bazel_command_normalizes_github_actions_startup_options(self) -> None:
        env = {
            "BAZEL_OUTPUT_USER_ROOT": "/tmp/bazel-output",
            "GITHUB_ACTIONS": "true",
        }

        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command("build", "//codex-rs/...", env=env),
            [
                "bazel",
                "--output_user_root=/tmp/bazel-output",
                "--noexperimental_remote_repo_contents_cache",
                "build",
                "//codex-rs/...",
            ],
        )
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command(
                "--experimental_remote_repo_contents_cache",
                "build",
                "//codex-rs/...",
                env=env,
            ),
            [
                "bazel",
                "--output_user_root=/tmp/bazel-output",
                "--experimental_remote_repo_contents_cache",
                "build",
                "//codex-rs/...",
            ],
        )

    def test_bazel_command_uses_configured_local_caches(self) -> None:
        env = {
            "BAZEL_REPO_CONTENTS_CACHE": "/tmp/bazel-repo-contents",
            "BAZEL_REPOSITORY_CACHE": "/tmp/bazel-repository",
        }

        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command(
                "build",
                "--config=local",
                "//codex-rs/...",
                env=env,
            ),
            [
                "bazel",
                "build",
                "--config=local",
                "//codex-rs/...",
                "--repo_contents_cache=/tmp/bazel-repo-contents",
                "--repository_cache=/tmp/bazel-repository",
            ],
        )

    def test_aquery_command_uses_configured_local_caches(self) -> None:
        env = {
            "BAZEL_REPO_CONTENTS_CACHE": "/tmp/bazel-repo-contents",
            "BAZEL_REPOSITORY_CACHE": "/tmp/bazel-repository",
        }

        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command(
                "aquery",
                "--output=text",
                "//codex-rs/otel:otel",
                env=env,
            ),
            [
                "bazel",
                "aquery",
                "--output=text",
                "//codex-rs/otel:otel",
                "--repo_contents_cache=/tmp/bazel-repo-contents",
                "--repository_cache=/tmp/bazel-repository",
            ],
        )

    def test_bazel_command_adds_local_caches_before_separator(self) -> None:
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command(
                "build",
                "//codex-rs/...",
                "--",
                "--program-arg",
                env={"BAZEL_REPOSITORY_CACHE": "/tmp/bazel-repository"},
            ),
            [
                "bazel",
                "build",
                "//codex-rs/...",
                "--repository_cache=/tmp/bazel-repository",
                "--",
                "--program-arg",
            ],
        )

    def test_failed_sharded_attempt_logs_survive_interrupted_build_reporting(self) -> None:
        repo_root = Path(__file__).resolve().parents[2]
        with TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            testlogs = root / "testlogs"
            markers = []
            for shard in (2, 3, 5):
                attempt = (
                    testlogs
                    / "codex-rs/core/core-unit-tests"
                    / f"shard_{shard}_of_8/test_attempts/attempt_1.log"
                )
                attempt.parent.mkdir(parents=True)
                marker = f"failed-shard-{shard}-evidence"
                attempt.write_text(marker + "\n", encoding="utf-8")
                markers.append(marker)

            fake_bazel = root / "fake-bazel"
            fake_bazel.write_text(
                f"#!{sys.executable}\n"
                "import os, sys\n"
                "if 'info' in sys.argv:\n"
                "    print(os.environ['TEST_BAZEL_TESTLOGS_DIR'])\n"
                "    sys.exit(0)\n"
                "for shard in (2, 3, 5):\n"
                "    path = os.environ['TEST_BAZEL_TESTLOGS_DIR'] + "
                "f'/codex-rs/core/core-unit-tests/shard_{shard}_of_8/' "
                "+ 'test_attempts/attempt_1.log'\n"
                "    print(f'FAIL: //codex-rs/core:core-unit-tests "
                "(shard {shard} of 8) (Exit 101) (see {path})')\n"
                "sys.exit(1)\n",
                encoding="utf-8",
            )
            fake_bazel.chmod(0o755)
            env = os.environ.copy()
            env.pop("BUILDBUDDY_API_KEY", None)
            env.update(
                CODEX_BAZEL_BIN=str(fake_bazel),
                TEST_BAZEL_TESTLOGS_DIR=str(testlogs),
                RUNNER_TEMP=str(root),
            )
            result = subprocess.run(
                [
                    "bash",
                    str(repo_root / ".github/scripts/run-bazel-ci.sh"),
                    "--print-failed-test-logs",
                    "--",
                    "test",
                    "--",
                    "//codex-rs/core:core-unit-tests",
                ],
                cwd=repo_root,
                env=env,
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            for marker in markers:
                self.assertIn(marker, result.stdout)
            self.assertNotIn("Missing test log:", result.stdout)

    def test_main_preserves_spaced_argument_and_child_exit_status(self) -> None:
        spaced_arg = (
            r"--test_env=PATH=C:\Program Files\PowerShell\7;C:\Program Files\Git\bin"
        )
        child_code = (
            f"import sys; sys.exit(37 if sys.argv[1] == {spaced_arg!r} else 91)"
        )
        env = os.environ.copy()
        env["CODEX_BAZEL_BIN"] = sys.executable
        env.pop("BAZEL_OUTPUT_USER_ROOT", None)
        env.pop("BUILDBUDDY_API_KEY", None)
        env.pop("GITHUB_ACTIONS", None)

        result = subprocess.run(
            [
                sys.executable,
                str(Path(run_bazel_with_buildbuddy.__file__)),
                "-c",
                child_code,
                spaced_arg,
            ],
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 37, result.stderr)


if __name__ == "__main__":
    unittest.main()
