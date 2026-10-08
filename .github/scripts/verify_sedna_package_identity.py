#!/usr/bin/env python3
"""Verify the packaged CLI and TUI expose the package's progressive identity."""

import argparse
import fcntl
import hashlib
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path


ANSI_ESCAPE = re.compile(
    rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)|.)"
)
SOURCE_SHA = re.compile(r"[0-9a-f]{40}\Z")
SEMVER = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\Z"
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="verify_sedna_package_identity")
    parser.add_argument("--package-dir", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--cargo-version", required=True)
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--workflow-host-sha", required=True)
    return parser.parse_args()


def archive_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as archive:
        for chunk in iter(lambda: archive.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def tui_header_is_visible(cli: Path, package_dir: Path, expected_header: str) -> bool:
    master, slave = pty.openpty()
    process: subprocess.Popen[bytes] | None = None
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 110, 0, 0))
        with tempfile.TemporaryDirectory(prefix="codex-package-identity-") as home:
            config_dir = Path(home)
            (config_dir / "config.toml").write_text(
                'approval_policy = "never"\nsandbox_mode = "read-only"\n'
                "check_for_update_on_startup = false\n\n"
                "[analytics]\nenabled = false\n\n"
                '[otel]\nexporter = "none"\ntrace_exporter = "none"\n'
                'metrics_exporter = "none"\n',
                encoding="utf-8",
            )
            env = os.environ.copy()
            for key in (
                "OPENAI_API_KEY",
                "CODEX_API_KEY",
                "CODEX_AUTH_JSON",
                "CODEX_REMOTE_AUTH_TOKEN",
                "GITHUB_TOKEN",
                "GH_TOKEN",
                "ACTIONS_RUNTIME_TOKEN",
                "CODEX_MANAGED_BY_NPM",
                "CODEX_MANAGED_BY_BUN",
                "CODEX_MANAGED_BY_PNPM",
                "CODEX_MANAGED_BY_VITE_PLUS",
            ):
                env.pop(key, None)
            env.update(
                {
                    "CODEX_HOME": str(config_dir),
                    "TERM": "xterm-256color",
                    "COLUMNS": "110",
                    "LINES": "34",
                }
            )
            process = subprocess.Popen(
                [str(cli), "--no-alt-screen"],
                cwd=package_dir,
                env=env,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                start_new_session=True,
                close_fds=True,
            )
            os.close(slave)
            slave = -1
            output = bytearray()
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    return False
                ready, _, _ = select.select([master], [], [], min(1, deadline - time.monotonic()))
                if not ready:
                    continue
                chunk = os.read(master, 8192)
                if not chunk:
                    return False
                output.extend(chunk)
                if len(output) > 262144:
                    del output[:-131072]
                rendered = ANSI_ESCAPE.sub(b"", bytes(output)).decode(
                    "utf-8", errors="replace"
                )
                if expected_header in rendered:
                    return True
            return False
    except (OSError, subprocess.SubprocessError, ValueError):
        return False
    finally:
        if process is not None and process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired):
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except OSError:
                    pass
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
        if slave >= 0:
            os.close(slave)
        os.close(master)


def fail(code: str) -> int:
    print(json.dumps({"outcome": "failure", "failure_code": code}), file=sys.stderr)
    return 1


def main() -> int:
    args = parse_args()
    if (
        SOURCE_SHA.fullmatch(args.source_sha) is None
        or SOURCE_SHA.fullmatch(args.workflow_host_sha) is None
        or SEMVER.fullmatch(args.expected_version) is None
        or not args.expected_version.endswith(f"+g{args.source_sha[:8]}")
    ):
        return fail("invalid_identity_input")

    try:
        package_dir = args.package_dir.resolve(strict=True)
        archive = args.archive.resolve(strict=True)
        manifest = json.loads(
            (package_dir / "codex-package.json").read_text(encoding="utf-8")
        )
    except (OSError, ValueError, json.JSONDecodeError):
        return fail("package_manifest_unavailable")

    if not isinstance(manifest, dict):
        return fail("package_manifest_mismatch")
    if (
        manifest.get("layoutVersion") != 1
        or manifest.get("variant") != "codex"
        or manifest.get("target") != args.target
        or manifest.get("entrypoint") != "bin/codex"
        or manifest.get("version") != args.expected_version
    ):
        return fail("package_manifest_mismatch")

    try:
        cli = (package_dir / manifest["entrypoint"]).resolve(strict=True)
    except OSError:
        return fail("packaged_cli_unavailable")
    if not cli.is_file() or not os.access(cli, os.X_OK):
        return fail("packaged_cli_unavailable")

    for relative in (
        "bin/codex-code-mode-host",
        "codex-path/rg",
        "codex-resources/bwrap",
    ):
        helper = package_dir / relative
        if not helper.is_file() or not os.access(helper, os.X_OK):
            return fail("packaged_helper_missing")

    expected_cli = (
        f"codex {args.cargo_version} (Sedna v{args.expected_version})"
        if args.cargo_version == "0.0.0"
        else f"codex {args.cargo_version}"
    )
    try:
        version_env = os.environ.copy()
        for key in (
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "CODEX_AUTH_JSON",
            "CODEX_REMOTE_AUTH_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "ACTIONS_RUNTIME_TOKEN",
        ):
            version_env.pop(key, None)
        with tempfile.TemporaryDirectory(prefix="codex-package-version-") as home:
            version_env["CODEX_HOME"] = home
            result = subprocess.run(
                [str(cli), "--version"],
                cwd=package_dir,
                env=version_env,
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )
    except (OSError, subprocess.SubprocessError):
        return fail("packaged_cli_version_unavailable")
    if result.returncode != 0 or result.stdout.strip() != expected_cli:
        return fail("packaged_cli_version_mismatch")

    expected_header = f"OpenAI Codex (v{args.expected_version})"
    if not tui_header_is_visible(cli, package_dir, expected_header):
        return fail("packaged_tui_version_missing")

    try:
        archive_digest = archive_sha256(archive)
        report = {
            "outcome": "success",
            "run_id": os.environ.get("GITHUB_RUN_ID", "unavailable"),
            "workflow_url": (
                f'{os.environ.get("GITHUB_SERVER_URL", "")}/'
                f'{os.environ.get("GITHUB_REPOSITORY", "")}/actions/runs/'
                f'{os.environ.get("GITHUB_RUN_ID", "")}'
            ),
            "source_sha": args.source_sha,
            "workflow_host_sha": args.workflow_host_sha,
            "target": args.target,
            "cargo_version": args.cargo_version,
            "manifest_version": manifest["version"],
            "manifest_matches_expected": True,
            "cli_version": expected_cli,
            "cli_version_matches_expected": True,
            "tui_header": expected_header,
            "tui_header_observed": True,
            "package_archive_sha256": archive_digest,
        }
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    except OSError:
        return fail("package_identity_report_unavailable")

    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        result = main()
    except Exception:
        result = fail("identity_verification_error")
    raise SystemExit(result)
