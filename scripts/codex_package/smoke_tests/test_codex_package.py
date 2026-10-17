"""Exercise each assembled package in the order its shipped files appear.

ROOT_OF_EXTRACTED_PACKAGE
├── bin
│   ├── codex[.exe]                       # CLI package only
│   ├── codex-app-server[.exe]            # app-server package only
│   └── codex-code-mode-host[.exe]
├── codex-package.json
├── codex-path
│   └── rg[.exe]
└── codex-resources
    ├── bwrap                             # Linux only
    ├── codex-command-runner.exe          # Windows only
    ├── codex-windows-sandbox-setup.exe   # Windows only
    └── zsh/bin/zsh                       # supported Unix targets only

Debug symbols for all shipped binaries arrive in a separate companion archive.
Each package contains one entrypoint, not both codex and codex-app-server.
"""

import hashlib
import json
import shutil
import subprocess
import sys
import tomllib
from dataclasses import replace
from pathlib import Path

import pytest
from app_server_harness import (
    MockResponsesServer,
    ev_completed,
    ev_response_created,
    sse,
)
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage


@pytest.mark.parametrize(
    ("arguments", "expected"),
    [
        pytest.param(("--help",), "Usage:", id="help"),
        pytest.param(("features", "list"), "code_mode", id="features"),
        pytest.param(("completion", "bash"), "codex", id="completion"),
    ],
)
def test_cli_public_commands(
    package: SmokePackage,
    arguments: tuple[str, ...],
    expected: str,
) -> None:
    """Packaged CLI remains usable for discovery, features, and completions."""
    output = package.run(*arguments).stdout
    assert expected in output


@pytest.mark.skipif(
    sys.platform != "linux",
    reason="Instruction consumer is scoped to the disposable Linux first-package runner",
)
def test_cli_instruction_inspection_with_synthetic_linux_configuration(
    package: SmokePackage,
    tmp_path: Path,
) -> None:
    """Exercise the shipped CLI in the disposable Linux first-package consumer."""
    home = tmp_path / "home"
    config_home = home / "codex"
    workspace = tmp_path / "workspace"
    temporary = tmp_path / "temporary"
    for directory in (config_home, workspace, temporary):
        directory.mkdir(parents=True)
    # The resolver recognizes .git/HEAD as a checkout boundary. Keep project
    # discovery inside this fresh workspace without invoking an external git.
    (workspace / ".git").mkdir()
    (workspace / ".git" / "HEAD").write_text(
        "ref: refs/heads/main\n", encoding="utf-8"
    )

    # Keep the exact package entrypoint, but do not inherit credentials, provider
    # settings, remote-control selectors, or runtime-proof configuration.
    environment = {
        key: package.environment[key]
        for key in ("PATH", "SystemRoot", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT")
        if key in package.environment
    }
    environment.update(
        CODEX_HOME=str(config_home),
        HOME=str(home),
        USERPROFILE=str(home),
        XDG_CONFIG_HOME=str(home / "xdg-config"),
        APPDATA=str(home / "app-data"),
        LOCALAPPDATA=str(home / "local-app-data"),
        PROGRAMDATA=str(home / "program-data"),
        TMPDIR=str(temporary),
        TMP=str(temporary),
        TEMP=str(temporary),
    )
    isolated = replace(package, directory=workspace, environment=environment)
    config_path = config_home / "config.toml"
    unopened_path = workspace / "missing-instruction-target.md"
    configuration = (
        'instructions = "PACKAGE_BASE_MARKER"\n'
        'developer_instructions = "PACKAGE_DEVELOPER_MARKER"\n'
        f"model_instructions_file = {json.dumps(str(unopened_path))}\n"
        "[features.multi_agent_v2]\n"
        'subagent_developer_instructions = "PACKAGE_CHILD_MARKER"\n'
    )
    config_path.write_text(configuration, encoding="utf-8")

    def run_report(*arguments: str, stdin: str | None = None) -> tuple[dict, str]:
        # Synthetic HOME does not redirect Unix system configuration. This
        # canonical Linux consumer must fail closed before either invocation;
        # lstat also rejects dangling symlinks without reading their targets.
        for name in ("config.toml", "requirements.toml", "managed_config.toml"):
            try:
                (Path("/etc/codex") / name).lstat()
            except FileNotFoundError:
                continue
            except OSError:
                pytest.fail("System configuration absence is unverified", pytrace=False)
            pytest.fail(
                "Instruction consumer requires absent system configuration", pytrace=False
            )
        try:
            completed = isolated.run("debug", "instructions", *arguments, stdin=stdin)
            report = json.loads(completed.stdout)
        except (OSError, subprocess.SubprocessError, ValueError, UnicodeError):
            pytest.fail("Packaged instruction inspection failed", pytrace=False)
        if not isinstance(report, dict):
            pytest.fail("Packaged instruction report has an invalid shape", pytrace=False)
        return report, completed.stdout

    def inline_fields(report: dict) -> dict:
        values = report.get("inline_configuration")
        if not isinstance(values, list):
            return {}
        return {
            value["key"]: value
            for value in values
            if isinstance(value, dict) and isinstance(value.get("key"), str)
        }

    def section(report: dict, key: str) -> dict:
        value = report.get(key)
        return value if isinstance(value, dict) else {}

    default, default_output = run_report()
    default_fields = inline_fields(default)
    base = default_fields.get("instructions", {})
    origin = base.get("origin")
    model_file = default_fields.get("model_instructions_file", {})
    default_checks = (
        isinstance(origin, dict)
        and origin.get("kind") == "user"
        and origin.get("file") == str(config_path),
        base.get("present") is True,
        default_fields.get("developer_instructions", {}).get("present") is True,
        default_fields.get(
            "features.multi_agent_v2.subagent_developer_instructions", {}
        ).get("present") is True,
        len(default_fields) == 8,
        all("content" not in field for field in default_fields.values()),
        not any(
            marker in default_output
            for marker in (
                "PACKAGE_BASE_MARKER",
                "PACKAGE_DEVELOPER_MARKER",
                "PACKAGE_CHILD_MARKER",
            )
        ),
        model_file.get("configured_path") == str(unopened_path),
        model_file.get("content_status") == "not_opened; configured path only",
        section(default, "caller_supplied_text").get("origin") == "not read",
        default.get("scope")
        == "selected local merged configuration; not a complete effective session prompt",
        default.get("unknown_or_not_inspected")
        == [
            "cloud-managed additions (cloud callback is no-op)",
            "stored thread history, current model template, runtime conversation and provider-hidden instructions",
            "AGENTS.md and user/provider instruction discovery",
            "model_instructions_file target contents and final baseline fallback",
        ],
    )
    if not all(default_checks):
        pytest.fail("Default packaged instruction projection failed", pytrace=False)

    explicit, explicit_output = run_report(
        "--show-content",
        "--client-instructions-stdin",
        stdin="PACKAGE_CALLER_MARKER\npassword=PACKAGE_PRIVATE_MARKER\n",
    )
    explicit_fields = inline_fields(explicit)
    caller = section(explicit, "caller_supplied_text")
    managed = section(explicit, "local_managed_additional_developer_instructions")
    warning = explicit.get("publication_warning")
    explicit_checks = (
        explicit_fields.get("instructions", {}).get("content")
        == "PACKAGE_BASE_MARKER",
        explicit_fields.get("developer_instructions", {}).get("content")
        == "PACKAGE_DEVELOPER_MARKER",
        caller.get("origin") == "caller-supplied; unverified",
        caller.get("content") == "PACKAGE_CALLER_MARKER\n[REDACTED]\n",
        caller.get("redacted_lines") == 1,
        "PACKAGE_PRIVATE_MARKER" not in explicit_output,
        managed.get("content")
        == "excluded; local-managed instruction content is never displayed",
        isinstance(warning, str)
        and "heuristic" in warning
        and "not guaranteed" in warning,
    )
    if not all(explicit_checks):
        pytest.fail("Explicit packaged instruction projection failed", pytrace=False)

    state_created = any(
        path.name in {"auth.json", "history.jsonl", "sessions"}
        or ".sqlite" in path.name
        or path.name.endswith(".db")
        for path in config_home.rglob("*")
    )
    config_unchanged = config_path.read_text(encoding="utf-8") == configuration
    target_unopened = not unopened_path.exists()
    assert not state_created, "Instruction inspection created auth or session state"
    assert config_unchanged, "Instruction inspection changed its synthetic config"
    assert target_unopened, "Instruction inspection created its configured target"


def test_cli_version_matches_release_sources(package: SmokePackage) -> None:
    """Verify the compiled banner against the sources used to package the release."""
    root = next(
        parent
        for parent in Path(__file__).resolve().parents
        if (parent / "codex-rs/Cargo.toml").is_file()
    )
    manifest = tomllib.loads((root / "codex-rs/Cargo.toml").read_text())
    version = manifest["workspace"]["package"]["version"]
    assert package.run("--version").stdout == f"codex-cli {version}\n"


def test_package_manifests_bind_all_extracted_payloads(package: SmokePackage) -> None:
    """Each downloaded package manifest hashes its exact extracted payload."""
    for package_root in (package.cli_root, package.app_server_root):
        manifest = json.loads(
            (package_root / "codex-package.json").read_text(encoding="utf-8")
        )
        payload_hashes: dict[str, str] = {}
        for path in sorted(package_root.rglob("*")):
            relative_path = path.relative_to(package_root).as_posix()
            if not path.is_file() or relative_path == "codex-package.json":
                continue
            digest = hashlib.sha256()
            with path.open("rb") as payload:
                for chunk in iter(lambda: payload.read(1024 * 1024), b""):
                    digest.update(chunk)
            payload_hashes[relative_path] = digest.hexdigest()

        assert manifest.get("payloadSha256") == payload_hashes, package_root


@pytest.mark.parametrize("entrypoint", ["codex", "codex-app-server"])
def test_app_server_runs_code_mode_through_python_sdk(
    package: SmokePackage,
    responses_server: MockResponsesServer,
    entrypoint: str,
) -> None:
    """Both packages run their own ripgrep through sandboxed SDK code mode."""
    windows = "windows" in package.target
    sandbox_config = ("--config", 'windows.sandbox="unelevated"') if windows else ()
    if entrypoint == "codex":
        executable = package.cli
        package_root = package.cli_root
        package_path_dir = package.cli_path_dir
        config = CodexConfig(
            codex_bin=str(executable),
            config_overrides=sandbox_config[1:],
            cwd=str(package.directory),
            env=package.environment,
        )
    else:
        executable = package.app_server
        package_root = package.app_server_root
        package_path_dir = package.app_server_path_dir
        config = CodexConfig(
            launch_args_override=(str(executable), *sandbox_config),
            cwd=str(package.directory),
            env=package.environment,
        )
    if windows:
        # TODO(anp): Assert resolved rg paths once Windows sandbox PATH finds them.
        ripgrep = str(package_path_dir / "rg.exe").replace("'", "''")
        command = (
            f"$rg = '{ripgrep}'; Write-Output $rg; & $rg --version; "
            "if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }"
        )
    else:
        command = "command -v rg && rg --version"
    exec_arguments = {"cmd": command, "login": False, "yield_time_ms": 10_000}
    responses_server.enqueue_sse(
        sse(
            [
                ev_response_created("code-mode"),
                {
                    "type": "response.output_item.done",
                    "item": {
                        "type": "custom_tool_call",
                        "call_id": "package-smoke",
                        "name": "exec",
                        "input": (
                            "text(JSON.stringify(await tools.exec_command("
                            f"{json.dumps(exec_arguments)})))"
                        ),
                    },
                },
                ev_completed("code-mode"),
            ]
        )
    )
    responses_server.enqueue_assistant_message("Done", response_id="code-mode-done")
    with Codex(config=config) as client:
        turn = client.thread_start(
            ephemeral=True,
            approval_mode=ApprovalMode.deny_all,
            sandbox=Sandbox.workspace_write,
        ).run("run package smoke")
        assert turn.final_response == "Done", turn
    # The mock also records analytics POSTs, which have no Responses input list.
    output = next(
        item
        for request in responses_server.requests()
        if request.path == "/v1/responses"
        for item in request.input()
        if item.get("type") == "custom_tool_call_output"
        and item.get("call_id") == "package-smoke"
    )["output"]
    if not isinstance(output, str):
        output = next(
            item["text"] for item in output if item.get("text", "").startswith("{")
        )
    execution = json.loads(output)
    execution_output = execution["output"]
    assert execution["exit_code"] == 0, execution_output
    assert "ripgrep" in execution_output, execution_output

    # Each entrypoint must execute the ripgrep bundled in its own package.
    reported_ripgrep_path = Path(execution_output.splitlines()[0].strip())
    assert reported_ripgrep_path.is_relative_to(package_root), execution_output


def _code_mode_host_symbol_address(
    package: SmokePackage,
    symbols: Path,
    *arguments: str,
) -> str:
    symbol_output = subprocess.run(
        ["nm", *arguments, str(symbols)],
        cwd=package.directory,
        env=package.environment,
        capture_output=True,
        text=True,
        check=True,
        timeout=45,
    ).stdout
    return next(
        parts[0]
        for line in symbol_output.splitlines()
        if len(parts := line.split()) == 3
        and parts[1].lower() == "t"
        and "codex_code_mode_host" in parts[2]
    )


@pytest.mark.skipif(sys.platform != "linux", reason="requires Linux debug symbols")
def test_linux_debug_symbols_resolve_packaged_code(
    package: SmokePackage,
    code_mode_host_debug_symbols: Path,
) -> None:
    """Linux host symbols resolve a packaged function and its source location."""
    address = _code_mode_host_symbol_address(
        package, code_mode_host_debug_symbols, "--defined-only", "--extern-only"
    )
    resolved = subprocess.run(
        [
            "addr2line",
            "-e",
            str(code_mode_host_debug_symbols),
            "-f",
            "-C",
            f"0x{address}",
        ],
        cwd=package.directory,
        env=package.environment,
        capture_output=True,
        text=True,
        check=True,
        timeout=45,
    ).stdout
    assert "codex_code_mode_host" in resolved, resolved
    source = resolved.splitlines()[-1]
    assert source != "??:?" and source.rsplit(":", 1)[-1].isdigit(), source


@pytest.mark.skipif(sys.platform != "darwin", reason="requires macOS debug symbols")
def test_macos_debug_symbols_resolve_packaged_code(
    package: SmokePackage,
    code_mode_host_debug_symbols: Path,
) -> None:
    """macOS host symbols resolve a packaged function."""
    address = _code_mode_host_symbol_address(package, code_mode_host_debug_symbols)
    resolved = subprocess.run(
        ["atos", "-o", str(code_mode_host_debug_symbols), f"0x{address}"],
        cwd=package.directory,
        env=package.environment,
        capture_output=True,
        text=True,
        check=True,
        timeout=45,
    ).stdout
    assert "codex_code_mode_host" in resolved, resolved


@pytest.mark.skipif(sys.platform != "win32", reason="requires Windows debug symbols")
def test_windows_debug_symbols_resolve_packaged_code(
    package: SmokePackage,
    code_mode_host_debug_symbols: Path,
) -> None:
    """Windows host symbols match the packaged executable's debug signature."""
    # Cargo embeds an underscored PDB name; Bazel uses the binary's name.
    # Offer the archived PDB under both names so dumpbin still verifies that
    # its signature matches the packaged executable, regardless of builder.
    cargo_symbols = code_mode_host_debug_symbols.with_name("codex_code_mode_host.pdb")
    shutil.copy2(code_mode_host_debug_symbols, cargo_symbols)
    symbols = (code_mode_host_debug_symbols, cargo_symbols)
    host = code_mode_host_debug_symbols.with_suffix(".exe")
    shutil.copy2(package.cli.with_name("codex-code-mode-host.exe"), host)
    result = subprocess.run(
        ["dumpbin", "/PDBPATH", str(host)],
        cwd=package.directory,
        env=package.environment,
        capture_output=True,
        text=True,
        check=True,
        timeout=45,
    )
    assert any(str(path).casefold() in result.stdout.casefold() for path in symbols), (
        result.stdout
    )
