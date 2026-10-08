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
import shutil
import signal
import struct
import subprocess
import sys
import tarfile
import tempfile
import termios
import time
from pathlib import Path, PurePosixPath


ANSI_ESCAPE = re.compile(
    rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)|.)"
)
SOURCE_SHA = re.compile(r"[0-9a-f]{40}\Z")
SEMVER = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\Z"
)
MAX_ARCHIVE_MEMBERS = 256
MAX_ARCHIVE_UNPACKED_BYTES = 1024 * 1024 * 1024


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="verify_sedna_package_identity")
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--cargo-version", required=True)
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--predecessor-source-sha", required=True)
    parser.add_argument("--workflow-host-sha", required=True)
    parser.add_argument(
        "--validate-inputs-only",
        action="store_true",
        help="validate provenance arguments before an expensive package build",
    )
    return parser.parse_args()


def identity_input_failure(args: argparse.Namespace) -> str | None:
    for field in (
        "source_sha",
        "predecessor_source_sha",
        "workflow_host_sha",
    ):
        if SOURCE_SHA.fullmatch(getattr(args, field)) is None:
            return f"invalid_{field}"
    if SEMVER.fullmatch(args.expected_version) is None:
        return "invalid_expected_version"
    if not args.expected_version.endswith(f"+g{args.source_sha[:8]}"):
        return "expected_version_source_mismatch"
    return None


def archive_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as archive:
        for chunk in iter(lambda: archive.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def extract_canonical_package(archive_path: Path, destination: Path) -> bool:
    """Extract only ordinary package files beneath a fresh temporary root."""
    try:
        with tarfile.open(archive_path, "r:gz") as archive:
            members = []
            for member in archive:
                members.append(member)
                if len(members) > MAX_ARCHIVE_MEMBERS:
                    return False

            seen: set[str] = set()
            unpacked_bytes = 0
            for member in members:
                name = member.name
                relative = PurePosixPath(name)
                if (
                    not name
                    or "\\" in name
                    or relative.is_absolute()
                    or relative.as_posix() != name
                    or any(part in ("", ".", "..") for part in relative.parts)
                    or name in seen
                    or member.type
                    not in (tarfile.DIRTYPE, tarfile.REGTYPE, tarfile.AREGTYPE)
                ):
                    return False
                seen.add(name)
                if member.isfile():
                    if member.size < 0:
                        return False
                    unpacked_bytes += member.size
                    if unpacked_bytes > MAX_ARCHIVE_UNPACKED_BYTES:
                        return False
                elif member.size != 0:
                    return False

            root = destination.resolve(strict=True)
            for member in members:
                relative = PurePosixPath(member.name)
                target = root.joinpath(*relative.parts)
                if not target.resolve(strict=False).is_relative_to(root):
                    return False
                directory = root
                for part in target.parent.relative_to(root).parts:
                    directory = directory / part
                    if directory.exists():
                        if not directory.is_dir() or directory.is_symlink():
                            return False
                    else:
                        directory.mkdir(mode=0o755)

                if member.isdir():
                    if target.exists() or target.is_symlink():
                        if not target.is_dir() or target.is_symlink():
                            return False
                    else:
                        target.mkdir(mode=0o755)
                    continue

                if target.exists() or target.is_symlink():
                    return False
                source = archive.extractfile(member)
                if source is None:
                    return False
                with source, target.open("xb") as output:
                    shutil.copyfileobj(source, output, length=1024 * 1024)
                os.chmod(target, (member.mode & 0o755) or 0o644)
        return True
    except (OSError, tarfile.TarError, ValueError):
        return False


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
    input_failure = identity_input_failure(args)
    if input_failure is not None:
        return fail(input_failure)
    if args.validate_inputs_only:
        print(json.dumps({"outcome": "ok", "identity_inputs": "valid"}))
        return 0
    if args.archive is None or args.report is None:
        return fail("package_archive_and_report_required")

    try:
        archive = args.archive.resolve(strict=True)
        archive_digest = archive_sha256(archive)
    except OSError:
        return fail("package_archive_unavailable")

    try:
        with tempfile.TemporaryDirectory(prefix="codex-package-consumer-") as temp_dir:
            package_dir = Path(temp_dir)
            if not extract_canonical_package(archive, package_dir):
                return fail("package_archive_invalid")
            return verify_extracted_package(args, archive_digest, package_dir)
    except OSError:
        return fail("package_archive_unavailable")


def verify_extracted_package(
    args: argparse.Namespace,
    archive_digest: str,
    package_dir: Path,
) -> int:
    try:
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
        report = {
            "outcome": "success",
            "run_id": os.environ.get("GITHUB_RUN_ID", "unavailable"),
            "workflow_url": (
                f'{os.environ.get("GITHUB_SERVER_URL", "")}/'
                f'{os.environ.get("GITHUB_REPOSITORY", "")}/actions/runs/'
                f'{os.environ.get("GITHUB_RUN_ID", "")}'
            ),
            "artifact_kind": "canonical_codex_package_archive",
            "predecessor_source_sha": args.predecessor_source_sha,
            "source_sha": args.source_sha,
            "workflow_host_sha": args.workflow_host_sha,
            "target": args.target,
            "consumer": "canonical_package_archive",
            "cargo_version": args.cargo_version,
            "preview_version": args.expected_version,
            "manifest_version": manifest["version"],
            "manifest_matches_expected": True,
            "cli_version": expected_cli,
            "cli_version_matches_expected": True,
            "tui_header": expected_header,
            "tui_header_observed": True,
            "components_present_and_executable": [
                "codex",
                "codex-code-mode-host",
                "rg",
                "bwrap",
            ],
            "not_included": ["codex-responses-api-proxy", "separate codex-app-server binary"],
            "other_carry_capabilities": "unverified_by_version_identity_consumer",
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
