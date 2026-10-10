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
    def test_local_only_ignores_key_and_preserves_native_windows_config(self) -> None:
        env = {
            "CODEX_BAZEL_LOCAL_ONLY": "1",
            "BUILDBUDDY_API_KEY": "token",
            "RUNNER_OS": "Windows",
        }
        args = [
            "test",
            "--config=ci-windows-local-msvc",
            "--host_platform=//:local_windows_msvc",
            "--",
            "//codex-rs/...",
        ]
        self.assertIsNone(run_bazel_with_buildbuddy.remote_config(args, env))
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_args_with_remote_config(args, env),
            [
                "test",
                "--config=ci-windows-local-msvc",
                "--host_platform=//:local_windows_msvc",
                "--remote_executor=",
                "--remote_cache=",
                "--bes_backend=",
                "--experimental_remote_downloader=",
                "--spawn_strategy=local",
                "--strategy_regexp=.*=local",
                "--jobs=HOST_CPUS",
                "--",
                "//codex-rs/...",
            ],
        )

    def test_local_only_uses_concrete_gnu_and_preserves_genuine_musl(self) -> None:
        env = {"CODEX_BAZEL_LOCAL_ONLY": "1", "RUNNER_OS": "Linux"}
        for triple, platform in (
            ("x86_64-unknown-linux-gnu", "//:linux_x86_64_gnu"),
            ("aarch64-unknown-linux-gnu", "//:linux_aarch64_gnu"),
            (
                "x86_64-unknown-linux-musl",
                "@rules_rs//rs/platforms:x86_64-unknown-linux-musl",
            ),
        ):
            with self.subTest(triple=triple):
                result = run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                    [
                        "test",
                        f"--platforms=@rules_rs//rs/platforms:{triple}",
                        "--",
                        "//codex-rs/...",
                    ],
                    env,
                )
                self.assertIn(f"--platforms={platform}", result)
                self.assertIn("--remote_executor=", result)
                self.assertIn("--strategy_regexp=.*=sandboxed,local", result)

    def test_local_only_preserves_audited_release_configs_without_compute(self) -> None:
        for config in (
            "ci-windows-msvc",
            "ci-macos",
            "ci-v8",
            "release",
            "v8-release-compat",
            "v8-target-x64",
            "v8-target-arm64",
            "rusty-v8-upstream-libcxx",
        ):
            with self.subTest(config=config):
                result = run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                    ["build", f"--config={config}", "--", "//third_party/v8:all"],
                    {"CODEX_BAZEL_LOCAL_ONLY": "1", "RUNNER_OS": "Linux"},
                )
                self.assertIn(f"--config={config}", result)
                self.assertIn("--remote_executor=", result)
                self.assertIn("--jobs=HOST_CPUS", result)

    def test_local_only_refuses_remote_config_endpoint_and_platform(self) -> None:
        env = {"CODEX_BAZEL_LOCAL_ONLY": "1", "BUILDBUDDY_API_KEY": "token"}
        for arg in (
            "--config=ci-windows-cross",
            "--config=ci-linux",
            "--config=buildbuddy-generic-rbe",
            "--config=unvetted",
            "--remote_executor=grpcs://example.invalid",
            "--remote_cache=grpcs://example.invalid",
            "--bes_backend=grpcs://example.invalid",
            "--experimental_remote_downloader=grpcs://example.invalid",
            "--host_platform=//:rbe",
            "--platforms=//:rbe",
            "--extra_execution_platforms=//:windows_x86_64_msvc,//:rbe",
            "--config",
            "--remote_executor",
            "--platforms",
        ):
            with self.subTest(arg=arg), self.assertRaises(ValueError):
                run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                    ["build", arg, "--", "//codex-rs/..."], env
                )

    def test_local_only_preserves_program_arguments_and_analysis_commands(self) -> None:
        env = {"CODEX_BAZEL_LOCAL_ONLY": "1", "BUILDBUDDY_API_KEY": "token"}
        args = ["run", "//codex-rs/cli:codex", "--", "--config=remote"]
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_args_with_remote_config(args, env),
            [
                *args[:2],
                "--remote_executor=",
                "--remote_cache=",
                "--bes_backend=",
                "--experimental_remote_downloader=",
                "--spawn_strategy=sandboxed,local",
                "--strategy_regexp=.*=sandboxed,local",
                "--jobs=HOST_CPUS",
                *args[2:],
            ],
        )
        for command in ("query", "cquery", "aquery", "info"):
            with self.subTest(command=command):
                args = [command, "--config=ci-bazel", "//codex-rs/..."]
                self.assertEqual(
                    run_bazel_with_buildbuddy.bazel_args_with_remote_config(args, env),
                    args,
                )

    def test_local_only_preserves_available_os_sandbox_strategies(self) -> None:
        for runner_os, strategy in (
            ("Linux", "sandboxed,local"),
            ("macOS", "darwin-sandbox,local"),
            ("Windows", "local"),
        ):
            with self.subTest(runner_os=runner_os):
                result = run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                    ["build", "--config=ci-bazel", "--", "//codex-rs/..."],
                    {"CODEX_BAZEL_LOCAL_ONLY": "1", "RUNNER_OS": runner_os},
                )
                self.assertIn(f"--spawn_strategy={strategy}", result)
                self.assertIn(f"--strategy_regexp=.*={strategy}", result)
                self.assertNotIn("remote", strategy.split(","))

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

    def test_program_arguments_after_separator_do_not_select_or_lose_rbe(self) -> None:
        args = ["run", "//codex-rs/cli:codex", "--", "--config=remote"]

        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_args_with_remote_config(args, {}),
            args,
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
