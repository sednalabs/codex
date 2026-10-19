#!/usr/bin/env python3

import json
import os
import subprocess
import sys
from collections.abc import Mapping
from collections.abc import Sequence
from pathlib import Path
from tempfile import TemporaryDirectory


OPENAI_REPOSITORY = "openai/codex"
# Remote configurations select cache/BES/download endpoints. Their -rbe forms
# also select the matching remote executor endpoint.
GENERIC_REMOTE_CONFIG = "buildbuddy-generic"
OPENAI_REMOTE_CONFIG = "buildbuddy-openai"
# These CI configurations require remote build execution. The wrapper supplies
# an RBE configuration, which also includes the common `remote` settings.
REMOTE_EXECUTION_CONFIGS = {
    "--config=ci-linux",
    "--config=ci-macos",
    "--config=ci-v8",
    "--config=ci-windows-cross",
}
LOCAL_EXECUTION_CONFIGS = {
    "ci-bazel",
    "ci-windows",
    "ci-windows-local-msvc",
    "ci-windows-msvc",
    "ci-macos",
    "ci-v8",
    "argument-comment-lint",
    "clippy",
    "release",
    "v8-release-compat",
    "v8-target-x64",
    "v8-target-arm64",
    "rusty-v8-upstream-libcxx",
}
LOCAL_ENDPOINT_ARGS = [
    "--remote_executor=",
    "--remote_cache=",
    "--bes_backend=",
    "--experimental_remote_downloader=",
]
# Honor either explicit setting so the wrapper never overrides the caller's
# choice when it supplies the CI default below.
REMOTE_REPO_CONTENTS_CACHE_STARTUP_OPTIONS = {
    "--experimental_remote_repo_contents_cache",
    "--noexperimental_remote_repo_contents_cache",
}


def startup_args(args: Sequence[str], env: Mapping[str, str]) -> list[str]:
    """Return shared startup options that are missing from a Bazel invocation.

    Bazel startup options must precede the command, and changing them restarts
    the server and discards its analysis cache. GitHub Actions invokes Bazel
    through several helpers, so normalize their startup options here while
    preserving any explicit choice made by the caller.
    """
    command_idx = next(
        (idx for idx, arg in enumerate(args) if not arg.startswith("-")),
        len(args),
    )
    configured_startup_args = args[:command_idx]
    injected_args = []

    output_user_root = env.get("BAZEL_OUTPUT_USER_ROOT")
    if output_user_root and not any(
        arg.startswith("--output_user_root=") for arg in configured_startup_args
    ):
        injected_args.append(f"--output_user_root={output_user_root}")

    if env.get("GITHUB_ACTIONS") == "true" and not any(
        arg in REMOTE_REPO_CONTENTS_CACHE_STARTUP_OPTIONS
        for arg in configured_startup_args
    ):
        # Work around Bazel 9 overlay materialization failures seen in CI. This
        # disables only the startup-level repo contents cache; keyed runs still
        # use BuildBuddy.
        injected_args.append("--noexperimental_remote_repo_contents_cache")

    return injected_args


# Only authenticated workflow runs executing trusted upstream code may use the
# OpenAI BuildBuddy host. A pull request event without proof that its head is
# in the upstream repository fails closed to the generic host.
def is_trusted_upstream_run(env: Mapping[str, str]) -> bool:
    # `GITHUB_REPOSITORY` is easy to set locally. Requiring GitHub's workflow
    # marker prevents a local command from opting itself into the OpenAI host.
    if (
        env.get("GITHUB_ACTIONS") != "true"
        or env.get("GITHUB_REPOSITORY") != OPENAI_REPOSITORY
    ):
        return False
    # Non-PR workflow runs in `openai/codex` execute upstream refs, so they are
    # trusted. Fork code reaches these workflows only through pull requests.
    if env.get("GITHUB_EVENT_NAME") != "pull_request":
        return True

    event_path = env.get("GITHUB_EVENT_PATH")
    if not event_path:
        return False
    try:
        event = json.loads(Path(event_path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return False

    try:
        return event["pull_request"]["head"]["repo"]["fork"] is False
    except (KeyError, TypeError):
        return False


def uses_openai_host(env: Mapping[str, str]) -> bool:
    return bool(env.get("BUILDBUDDY_API_KEY")) and is_trusted_upstream_run(env)


def uses_remote_execution(args: Sequence[str]) -> bool:
    try:
        separator_idx = args.index("--")
    except ValueError:
        separator_idx = len(args)
    return any(arg in REMOTE_EXECUTION_CONFIGS for arg in args[:separator_idx])


def remote_config(args: Sequence[str], env: Mapping[str, str]) -> str | None:
    if env.get("CODEX_BAZEL_LOCAL_ONLY") == "1" or not env.get("BUILDBUDDY_API_KEY"):
        return None

    config = OPENAI_REMOTE_CONFIG if uses_openai_host(env) else GENERIC_REMOTE_CONFIG
    if uses_remote_execution(args):
        config += "-rbe"
    return config


def local_execution_args(args: Sequence[str], env: Mapping[str, str]) -> list[str]:
    """Keep vetted target configs, refusing an explicit remote execution route."""
    try:
        separator_idx = args.index("--")
    except ValueError:
        separator_idx = len(args)
    # rules_rs intentionally leaves GNU's concrete libc to the workspace.
    # Select the reviewed GNU platforms without changing musl's Rust/LLVM ABI.
    local_platforms = {
        "--platforms=@rules_rs//rs/platforms:x86_64-unknown-linux-gnu": (
            "--platforms=//:linux_x86_64_gnu"
        ),
        "--platforms=@rules_rs//rs/platforms:aarch64-unknown-linux-gnu": (
            "--platforms=//:linux_aarch64_gnu"
        ),
    }
    bazel_args = [local_platforms.get(arg, arg) for arg in args[:separator_idx]]
    for arg in bazel_args:
        if arg in {
            "--config",
            "--remote_executor",
            "--remote_cache",
            "--bes_backend",
            "--experimental_remote_downloader",
            "--host_platform",
            "--platforms",
            "--extra_execution_platforms",
        }:
            raise ValueError("local-only CI requires explicit option=value syntax")
        if arg.startswith("--config="):
            config = arg.removeprefix("--config=")
            if config not in LOCAL_EXECUTION_CONFIGS:
                raise ValueError(
                    f"configuration is not vetted for local-only CI: {config}"
                )
        if (
            arg.startswith(
                (
                    "--remote_executor=",
                    "--remote_cache=",
                    "--bes_backend=",
                    "--experimental_remote_downloader=",
                )
            )
            and arg.partition("=")[2]
        ):
            raise ValueError("remote endpoint is not permitted in local-only CI")
        if arg.startswith(
            ("--host_platform=", "--platforms=", "--extra_execution_platforms=")
        ):
            if "//:rbe" in arg.partition("=")[2].split(","):
                raise ValueError("RBE platform is not permitted in local-only CI")

    command = next((arg for arg in bazel_args if not arg.startswith("-")), None)
    # Analysis/query/info commands do not execute build or test actions and
    # cannot all accept build-strategy flags. Execution commands enforce these
    # last, after any command-line or user-RC settings, before the target/argv
    # separator. Repository downloads and GitHub Actions caches stay local.
    guard_args = []
    if command in {"build", "test", "run", "coverage"}:
        local_strategy = {
            "Windows": "local",
            "macOS": "darwin-sandbox,local",
        }.get(env.get("RUNNER_OS"), "sandboxed,local")
        # Preserve an explicit sequential cap without raising the host-CPU ceiling.
        job_cap = "--jobs=1" if "--jobs=1" in bazel_args else "--jobs=HOST_CPUS"
        guard_args = [
            *LOCAL_ENDPOINT_ARGS,
            f"--spawn_strategy={local_strategy}",
            f"--strategy_regexp=.*={local_strategy}",
            job_cap,
        ]
    return [*bazel_args, *guard_args, *args[separator_idx:]]


def bazel_args_without_remote_execution(args: Sequence[str]) -> list[str]:
    # Remote CI configs require BuildBuddy credentials. Removing them preserves
    # the local fallback used for fork pull requests.
    try:
        separator_idx = args.index("--")
    except ValueError:
        separator_idx = len(args)
    return [
        *(arg for arg in args[:separator_idx] if arg not in REMOTE_EXECUTION_CONFIGS),
        *args[separator_idx:],
    ]


def bazel_args_with_remote_config(
    args: Sequence[str], env: Mapping[str, str]
) -> list[str]:
    command_idx = next(
        (idx for idx, arg in enumerate(args) if not arg.startswith("-")),
        None,
    )
    if command_idx is None:
        raise ValueError("expected a Bazel command")

    config = remote_config(args, env)
    if env.get("CODEX_BAZEL_LOCAL_ONLY") == "1":
        configured_args = local_execution_args(args, env)
    elif config is None:
        configured_args = bazel_args_without_remote_execution(args)
    else:
        # `remote_config()` returns a configuration only when this key is present.
        api_key = env["BUILDBUDDY_API_KEY"]
        remote_args = [
            f"--config={config}",
            f"--remote_header=x-buildbuddy-api-key={api_key}",
        ]

        # Insert immediately after the Bazel command. This keeps wrapper-added
        # options out of positional payloads and lets later CI configs override
        # shared RBE defaults such as the Windows cross-compilation exec platforms.
        configured_args = [
            *args[: command_idx + 1],
            *remote_args,
            *args[command_idx + 1 :],
        ]

    try:
        separator_idx = configured_args.index("--")
    except ValueError:
        separator_idx = len(configured_args)

    cache_args = [
        f"{option_prefix}{env[env_name]}"
        for env_name, option_prefix in (
            ("BAZEL_REPO_CONTENTS_CACHE", "--repo_contents_cache="),
            ("BAZEL_REPOSITORY_CACHE", "--repository_cache="),
        )
        if env.get(env_name)
        and not any(
            arg.startswith(option_prefix) for arg in configured_args[:separator_idx]
        )
    ]
    return [
        *configured_args[:separator_idx],
        *cache_args,
        *configured_args[separator_idx:],
    ]


def bazel_command(*args: str, env: Mapping[str, str] | None = None) -> list[str]:
    env = os.environ if env is None else env
    bazel = env.get("CODEX_BAZEL_BIN", "bazel")
    return [bazel, *startup_args(args, env), *bazel_args_with_remote_config(args, env)]


def workspace_status_diagnostic(
    events_path: Path, expected_command: str = "build"
) -> dict[str, int | bool]:
    """Classify private BEP records without publishing their values."""
    summary: dict[str, int | bool] = {
        "canonical_events": 0,
        "canonical_build_sections": 0,
        "canonical_test_sections": 0,
        "command_option_sections": 0,
        "status_option_count": 0,
        "workspace_status_events": 0,
        "producer_entry_count": 0,
        "commit_key_count": 0,
        "parse_failures": 0,
        "read_failed": False,
    }
    status_options = []
    try:
        with events_path.open(encoding="utf-8") as events:
            for line in events:
                try:
                    event = json.loads(line)
                    if not isinstance(event, dict):
                        raise ValueError
                    canonical = event.get("structuredCommandLine", {})
                    if canonical.get("commandLineLabel") == "canonical":
                        summary["canonical_events"] += 1
                        sections = canonical.get("sections", [])
                        if not isinstance(sections, list):
                            raise ValueError
                        for section in sections:
                            if section.get("sectionLabel") == "command":
                                chunks = section["chunkList"].get("chunk", [])
                                if chunks == ["build"]:
                                    summary["canonical_build_sections"] += 1
                                elif chunks == ["test"]:
                                    summary["canonical_test_sections"] += 1
                                else:
                                    raise ValueError
                            if section.get("sectionLabel") == "command options":
                                options = section["optionList"].get("option", [])
                                if not isinstance(options, list):
                                    raise ValueError
                                summary["command_option_sections"] += 1
                                for option in options:
                                    name = option["optionName"]
                                    value = option.get("optionValue", "")
                                    if not isinstance(name, str) or not isinstance(
                                        value, str
                                    ):
                                        raise ValueError
                                    if name == "workspace_status_command":
                                        summary["status_option_count"] += 1
                                        status_options.append(value)
                    if "workspaceStatus" in event:
                        items = event["workspaceStatus"].get("item", [])
                        if not isinstance(items, list):
                            raise ValueError
                        summary["workspace_status_events"] += 1
                        for item in items:
                            key = item["key"]
                            value = item.get("value", "")
                            if not isinstance(key, str) or not isinstance(value, str):
                                raise ValueError
                            summary["commit_key_count"] += key == "STABLE_GIT_COMMIT"
                            summary["producer_entry_count"] += (
                                key == "STABLE_WINDOWS_STATUS_CMD_ENTERED"
                                and value == "1"
                            )
                except (AttributeError, KeyError, TypeError, ValueError):
                    summary["parse_failures"] += 1
    except OSError:
        summary["read_failed"] = True
    evidence_valid = not summary["read_failed"] and summary["parse_failures"] == 0
    canonical_command_sections = (
        summary["canonical_test_sections"]
        if expected_command == "test"
        else summary["canonical_build_sections"]
    )
    option_known = evidence_valid and (
        summary["canonical_events"] == 1
        and canonical_command_sections == 1
        and summary["command_option_sections"] == 1
        and summary["status_option_count"] <= 1
    )
    summary["effective_option_known"] = option_known
    summary["effective_option_empty"] = option_known and status_options in ([], [""])
    summary["effective_option_selects_cmd"] = (
        option_known
        and len(status_options) == 1
        and status_options[0].replace("\\", "/").removeprefix("./")
        == "scripts/workspace-status.cmd"
    )
    summary["workspace_status_known"] = (
        evidence_valid and summary["workspace_status_events"] == 1
    )
    return summary


def print_workspace_status_diagnostic(summary: dict[str, int | bool]) -> None:
    try:
        print("Workspace status diagnostic: " + json.dumps(summary, sort_keys=True))
    except (OSError, TypeError, ValueError):
        pass


def run_with_workspace_status_diagnostic(
    command: list[str], expected_command: str = "build"
) -> int:
    """Use the same Windows build; preserve its exit status on diagnostic failure."""
    separator = command.index("--") if "--" in command else len(command)
    if any(arg.startswith("--build_event_json_file") for arg in command[:separator]):
        print_workspace_status_diagnostic({"existing_bep": True})
        return subprocess.run(command, check=False).returncode
    try:
        # Private, temporary, and outside every execution-log/artifact upload path.
        temporary = TemporaryDirectory(
            prefix="bazel-workspace-status-", dir=os.environ["RUNNER_TEMP"]
        )
    except (KeyError, OSError):
        print_workspace_status_diagnostic({"setup_failed": True})
        return subprocess.run(command, check=False).returncode
    try:
        events_path = Path(temporary.name) / "events.json"
        diagnostic_command = [
            *command[:separator],
            f"--build_event_json_file={events_path}",
            *command[separator:],
        ]
        child_env = dict(os.environ, CODEX_BAZEL_STATUS_DIAGNOSTIC="1")
        result = subprocess.run(diagnostic_command, env=child_env, check=False)
        try:
            print_workspace_status_diagnostic(
                workspace_status_diagnostic(events_path, expected_command)
            )
        except (OSError, TypeError, ValueError):
            print_workspace_status_diagnostic({"summary_failed": True})
        return result.returncode
    finally:
        try:
            temporary.cleanup()
        except OSError:
            print_workspace_status_diagnostic({"cleanup_failed": True})


def main() -> None:
    config = remote_config(sys.argv[1:], os.environ)
    if os.environ.get("CODEX_BAZEL_LOCAL_ONLY") == "1":
        print(
            "Runner-local Bazel execution is required; BuildBuddy is disabled.",
            file=sys.stderr,
        )
    elif config is None:
        print(
            "BuildBuddy key unavailable; using local Bazel configuration.",
            file=sys.stderr,
        )
    else:
        host_description = (
            "OpenAI tenant" if uses_openai_host(os.environ) else "generic"
        )
        print(
            f"Using {host_description} BuildBuddy configuration: {config}.",
            file=sys.stderr,
        )

    command = bazel_command(*sys.argv[1:])
    if os.name == "nt":
        # Windows CRT exec can split arguments containing spaces and lose the
        # eventual child exit status. Wait for Bazel and propagate its status.
        separator = command.index("--") if "--" in command else len(command)
        bazel_command_name = next(
            (arg for arg in command[1:separator] if not arg.startswith("-")), None
        )
        if should_run_workspace_status_diagnostic(command, os.environ):
            assert bazel_command_name is not None
            raise SystemExit(
                run_with_workspace_status_diagnostic(command, bazel_command_name)
            )
        result = subprocess.run(command, check=False)
        raise SystemExit(result.returncode)

    os.execvp(command[0], command)


def should_run_workspace_status_diagnostic(
    command: list[str], env: Mapping[str, str]
) -> bool:
    separator = command.index("--") if "--" in command else len(command)
    bazel_command = next(
        (arg for arg in command[1:separator] if not arg.startswith("-")), None
    )
    if (
        env.get("CODEX_BAZEL_LOCAL_ONLY") != "1"
        or env.get("CODEX_BAZEL_WINDOWS_VOICE_TOOLS") != "1"
    ):
        return False
    if bazel_command == "test":
        return True
    return (
        bazel_command == "build"
        and "--config=argument-comment-lint" in command[1:separator]
    )


if __name__ == "__main__":
    main()
