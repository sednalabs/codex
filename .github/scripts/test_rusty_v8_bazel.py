#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import textwrap
import unittest
from os import environ
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

import rusty_v8_bazel
import rusty_v8_module_bazel


class SetupRustyV8ActionTests(unittest.TestCase):
    target = "x86_64-unknown-linux-gnu"
    version = "150.4.0"
    profile = "ptrcomp_sandbox_release"
    archive_name = f"librusty_v8_{profile}_{target}.a.gz"
    binding_name = f"src_binding_{profile}_{target}.rs"
    checksums_name = f"rusty_v8_{profile}_{target}.sha256"

    def _action_script(self) -> str:
        action = (
            rusty_v8_bazel.ROOT
            / ".github"
            / "actions"
            / "setup-rusty-v8"
            / "action.yml"
        ).read_text(encoding="utf-8")
        lines = action.splitlines()
        marker = next(
            index for index, line in enumerate(lines) if line.strip() == "run: |"
        )
        indentation = len(lines[marker]) - len(lines[marker].lstrip()) + 2
        body = []
        for line in lines[marker + 1 :]:
            if line.strip() and len(line) - len(line.lstrip()) < indentation:
                break
            body.append(
                line[indentation:] if line.startswith(" " * indentation) else ""
            )
        return "\n".join(body) + "\n"

    def _fixture(self, directory: str) -> dict[str, str | Path]:
        root = Path(directory)
        bin_dir = root / "mock-bin"
        bin_dir.mkdir()
        workspace = root / "workflow-workspace"
        trusted_root = root / "trusted-harness"
        target_root = root / "validation-target"
        runner_temp = root / "runner-temp"
        for path in (
            workspace,
            trusted_root / "codex-rs",
            target_root / "codex-rs",
            runner_temp,
        ):
            path.mkdir(parents=True)
        helper_dir = trusted_root / ".github" / "scripts"
        helper_dir.mkdir(parents=True)
        source_scripts = rusty_v8_bazel.ROOT / ".github" / "scripts"
        for name in (
            "rusty_v8_bazel.py",
            "rusty_v8_module_bazel.py",
            "run_bazel_with_buildbuddy.py",
        ):
            shutil.copyfile(source_scripts / name, helper_dir / name)

        cargo_lock = target_root / "codex-rs" / "Cargo.lock"
        cargo_lock.write_text(
            f'[[package]]\nname = "v8"\nversion = "{self.version}"\n',
            encoding="utf-8",
        )
        trusted_cargo_lock = trusted_root / "codex-rs" / "Cargo.lock"
        trusted_cargo_lock.write_text(
            f'[[package]]\nname = "v8"\nversion = "{self.version}"\n',
            encoding="utf-8",
        )
        (trusted_root / "MODULE.bazel").write_text(
            textwrap.dedent(
                f"""\
                http_file(
                    name = "rusty_v8_150_4_0_x86_64_unknown_linux_gnu_archive",
                    downloaded_file_path = "{self.archive_name}",
                    urls = [
                        "https://static.crates.io/crates/v8/v8-150.4.0.crate",
                    ],
                )
                """
            ),
            encoding="utf-8",
        )

        archive = b"synthetic archive bytes"
        binding = b"synthetic Rust binding"
        checksums = (
            f"{hashlib.sha256(archive).hexdigest()}  {self.archive_name}\n"
            f"{hashlib.sha256(binding).hexdigest()}  {self.binding_name}\n"
        ).encode()
        trusted_manifest_dir = trusted_root / "third_party" / "v8"
        trusted_manifest_dir.mkdir(parents=True)
        trusted_manifest = (
            trusted_manifest_dir / "rusty_v8_150_4_0_release_manifests.sha256"
        )
        trusted_manifest.write_text(
            f"{hashlib.sha256(checksums).hexdigest()}  {self.checksums_name}\n",
            encoding="utf-8",
        )

        real_python = shutil.which("python3")
        self.assertIsNotNone(real_python)
        (bin_dir / "python3").write_text(
            "#!/usr/bin/env bash\n"
            "set -euo pipefail\n"
            "if [[ \"${1:-}\" == */.github/scripts/rusty_v8_bazel.py ]]; then\n"
            "  printf '%s\\n' \"$@\" > \"${HELPER_CALL_LOG}\"\n"
            f"  exec \"${{REAL_PYTHON}}\" \"$@\"\n"
            "fi\n"
            "exec \"${REAL_PYTHON}\" \"$@\"\n",
            encoding="utf-8",
        )
        (bin_dir / "python3").chmod(0o755)
        (bin_dir / "curl").write_text(
            "#!/usr/bin/env bash\n"
            "set -euo pipefail\n"
            "destination=\nurl=\n"
            "while (($#)); do\n"
            "  case \"$1\" in\n"
            "    -o) destination=\"$2\"; shift 2 ;;\n"
            "    -*) shift ;;\n"
            "    *) url=\"$1\"; shift ;;\n"
            "  esac\n"
            "done\n"
            "printf '%s\\n' \"${url##*/}\" >> \"${CURL_LOG}\"\n"
            "case \"${url##*/}\" in\n"
            "  \"${CHECKSUMS_NAME}\") printf '%s' \"${CHECKSUMS_PAYLOAD}\" "
            "> \"${destination}\" ;;\n"
            "  \"${ARCHIVE_NAME}\") printf '%s' 'synthetic archive bytes' "
            "> \"${destination}\" ;;\n"
            "  \"${BINDING_NAME}\") printf '%s' 'synthetic Rust binding' "
            "> \"${destination}\" ;;\n"
            "  *) exit 31 ;;\n"
            "esac\n",
            encoding="utf-8",
        )
        (bin_dir / "curl").chmod(0o755)
        helper_log = root / "helper-argv.txt"
        curl_log = root / "curl-names.txt"
        github_env = root / "github-env.txt"
        github_env.touch()
        return {
            "root": root,
            "workspace": workspace,
            "trusted_root": trusted_root,
            "target_root": target_root,
            "cargo_lock": cargo_lock,
            "trusted_cargo_lock": trusted_cargo_lock,
            "runner_temp": runner_temp,
            "bin_dir": bin_dir,
            "helper_log": helper_log,
            "curl_log": curl_log,
            "github_env": github_env,
            "trusted_manifest": trusted_manifest,
            "checksums": checksums,
            "real_python": str(real_python),
        }

    def _run_action(
        self,
        fixture: dict[str, str | Path],
        *,
        explicit_roots: bool = True,
        checksum_payload: bytes | None = None,
    ) -> subprocess.CompletedProcess[str]:
        root = Path(fixture["root"])
        workspace = Path(fixture["workspace"])
        trusted_root = Path(fixture["trusted_root"])
        runner_temp = Path(fixture["runner_temp"])
        github_env = Path(fixture["github_env"])
        if not explicit_roots:
            # The legacy action contract defaults to the workflow checkout and
            # discovers V8 from MODULE.bazel without an explicit Cargo.lock.
            workspace = trusted_root
        env = {
            **os.environ,
            "PATH": f"{fixture['bin_dir']}:{os.environ['PATH']}",
            "REAL_PYTHON": str(fixture["real_python"]),
            "HELPER_CALL_LOG": str(fixture["helper_log"]),
            "CURL_LOG": str(fixture["curl_log"]),
            "CHECKSUMS_NAME": self.checksums_name,
            "ARCHIVE_NAME": self.archive_name,
            "BINDING_NAME": self.binding_name,
            "CHECKSUMS_PAYLOAD": (
                checksum_payload
                if checksum_payload is not None
                else fixture["checksums"]
            ).decode(),
            "GITHUB_WORKSPACE": str(workspace),
            "RUNNER_TEMP": str(runner_temp),
            "GITHUB_ENV": str(github_env),
            "TARGET": self.target,
            "TRUSTED_ROOT": str(trusted_root) if explicit_roots else "",
            "CARGO_LOCK": str(fixture["cargo_lock"]) if explicit_roots else "",
        }
        return subprocess.run(
            ["bash", "-euo", "pipefail", "-c", self._action_script()],
            cwd=root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_action_preserves_omitted_root_and_lock_defaults(self) -> None:
        with TemporaryDirectory() as directory:
            fixture = self._fixture(directory)
            completed = self._run_action(fixture, explicit_roots=False)

            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(
                Path(fixture["helper_log"]).read_text().splitlines(),
                [
                    str(
                        Path(fixture["trusted_root"])
                        / ".github/scripts/rusty_v8_bazel.py"
                    ),
                    "resolved-v8-crate-version",
                ],
            )
            self.assertEqual(
                Path(fixture["curl_log"]).read_text().splitlines(),
                [self.checksums_name, self.archive_name, self.binding_name],
            )
            expected_archive = (
                Path(fixture["runner_temp"]) / "rusty_v8" / self.archive_name
            )
            expected_binding = (
                Path(fixture["runner_temp"]) / "rusty_v8" / self.binding_name
            )
            self.assertEqual(
                Path(fixture["github_env"]).read_text(),
                f"RUSTY_V8_ARCHIVE={expected_archive}\n"
                f"RUSTY_V8_SRC_BINDING_PATH={expected_binding}\n",
            )

    def test_action_routes_trusted_helper_and_target_lock_and_exports_verified_pair(
        self,
    ) -> None:
        with TemporaryDirectory() as directory:
            fixture = self._fixture(directory)
            completed = self._run_action(fixture)

            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(
                Path(fixture["helper_log"]).read_text().splitlines(),
                [
                    str(
                        Path(fixture["trusted_root"])
                        / ".github/scripts/rusty_v8_bazel.py"
                    ),
                    "resolved-v8-crate-version",
                    "--cargo-lock",
                    str(fixture["cargo_lock"]),
                ],
            )
            expected_archive = (
                Path(fixture["runner_temp"]) / "rusty_v8" / self.archive_name
            )
            expected_binding = (
                Path(fixture["runner_temp"]) / "rusty_v8" / self.binding_name
            )
            self.assertEqual(
                Path(fixture["github_env"]).read_text(),
                f"RUSTY_V8_ARCHIVE={expected_archive}\n"
                f"RUSTY_V8_SRC_BINDING_PATH={expected_binding}\n",
            )

    def test_action_rejects_missing_and_symlink_helpers_before_curl_or_export(
        self,
    ) -> None:
        for symlinked in (False, True):
            with self.subTest(symlinked=symlinked), TemporaryDirectory() as directory:
                fixture = self._fixture(directory)
                helper = (
                    Path(fixture["trusted_root"])
                    / ".github/scripts/rusty_v8_bazel.py"
                )
                helper.unlink()
                if symlinked:
                    target = Path(fixture["root"]) / "outside-helper.py"
                    target.write_text("# synthetic helper\n", encoding="utf-8")
                    helper.symlink_to(target)
                completed = self._run_action(fixture)

                self.assertNotEqual(completed.returncode, 0)
                self.assertIn("trusted V8 helper", completed.stderr)
                self.assertFalse(Path(fixture["curl_log"]).exists())
                self.assertEqual(Path(fixture["github_env"]).read_text(), "")
                self.assertFalse(Path(fixture["helper_log"]).exists())

    def test_action_rejects_unsafe_lock_version_before_artifact_download_or_export(
        self,
    ) -> None:
        with TemporaryDirectory() as directory:
            fixture = self._fixture(directory)
            Path(fixture["cargo_lock"]).write_text(
                '[[package]]\nname = "v8"\nversion = "../150.4.0"\n',
                encoding="utf-8",
            )
            completed = self._run_action(fixture)

            self.assertNotEqual(completed.returncode, 0)
            self.assertIn("invalid resolved v8 version", completed.stderr)
            self.assertFalse(Path(fixture["curl_log"]).exists())
            self.assertEqual(Path(fixture["github_env"]).read_text(), "")

    def test_action_rejects_checksum_mismatch_before_pair_download_or_export(
        self,
    ) -> None:
        with TemporaryDirectory() as directory:
            fixture = self._fixture(directory)
            changed_checksums = b"synthetic but untrusted checksum file\n"
            completed = self._run_action(fixture, checksum_payload=changed_checksums)

            self.assertNotEqual(completed.returncode, 0)
            self.assertIn("Checksum mismatch", completed.stderr)
            self.assertEqual(
                Path(fixture["curl_log"]).read_text().splitlines(),
                [self.checksums_name],
            )
            self.assertEqual(Path(fixture["github_env"]).read_text(), "")


class RustyV8BazelTest(unittest.TestCase):
    def test_resolved_v8_version_can_use_an_explicit_cargo_lock(self) -> None:
        with TemporaryDirectory() as directory:
            cargo_lock = Path(directory) / "Cargo.lock"
            cargo_lock.write_text(
                textwrap.dedent(
                    """\
                    [[package]]
                    name = "v8"
                    version = "150.4.0"
                    """
                ),
                encoding="utf-8",
            )

            self.assertEqual(
                "150.4.0", rusty_v8_bazel.resolved_v8_crate_version(cargo_lock)
            )

    def test_explicit_lock_without_v8_does_not_fall_back(self) -> None:
        with TemporaryDirectory() as directory:
            cargo_lock = Path(directory) / "Cargo.lock"
            cargo_lock.write_text(
                '[[package]]\nname = "unrelated"\nversion = "1.0.0"\n',
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                SystemExit, "expected exactly one resolved v8 version"
            ):
                rusty_v8_bazel.resolved_v8_crate_version(cargo_lock)

    def test_explicit_lock_rejects_non_numeric_v8_version(self) -> None:
        with TemporaryDirectory() as directory:
            cargo_lock = Path(directory) / "Cargo.lock"
            cargo_lock.write_text(
                textwrap.dedent(
                    """\
                    [[package]]
                    name = "v8"
                    version = "../150.4.0"
                    """
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(SystemExit, "invalid resolved v8 version"):
                rusty_v8_bazel.resolved_v8_crate_version(cargo_lock)

    def test_consumer_selectors_track_resolved_crate_version(self) -> None:
        build_bazel = (
            rusty_v8_bazel.ROOT / "third_party" / "v8" / "BUILD.bazel"
        ).read_text()
        version_suffix = rusty_v8_bazel.resolved_v8_crate_version().replace(".", "_")

        for selector in [
            "aarch64_apple_darwin_bazel",
            "aarch64_pc_windows_gnullvm",
            "aarch64_pc_windows_msvc",
            "aarch64_unknown_linux_gnu_bazel",
            "aarch64_unknown_linux_musl_release_base",
            "x86_64_apple_darwin_bazel",
            "x86_64_pc_windows_gnullvm",
            "x86_64_pc_windows_msvc",
            "x86_64_unknown_linux_gnu_bazel",
            "x86_64_unknown_linux_musl_release",
        ]:
            self.assertIn(
                f":v8_{version_suffix}_{selector}",
                build_bazel,
            )

        for selector in [
            "aarch64_apple_darwin",
            "aarch64_pc_windows_gnullvm",
            "aarch64_pc_windows_msvc",
            "aarch64_unknown_linux_gnu",
            "aarch64_unknown_linux_musl",
            "x86_64_apple_darwin",
            "x86_64_pc_windows_gnullvm",
            "x86_64_pc_windows_msvc",
            "x86_64_unknown_linux_gnu",
            "x86_64_unknown_linux_musl",
        ]:
            self.assertIn(
                f":src_binding_release_{selector}_{version_suffix}_release",
                build_bazel,
            )

    def test_command_version_tracks_remaining_http_file_assets(self) -> None:
        with TemporaryDirectory() as temp_dir:
            module_bazel = Path(temp_dir) / "MODULE.bazel"
            module_bazel.write_text(
                textwrap.dedent(
                    """\
                    http_file(
                        name = "rusty_v8_146_4_0_x86_64_unknown_linux_gnu_archive",
                        downloaded_file_path = "librusty_v8_release_x86_64-unknown-linux-gnu.a.gz",
                        urls = ["https://example.test/archive.gz"],
                    )
                    """
                )
            )

            with patch.object(rusty_v8_bazel, "MODULE_BAZEL", module_bazel):
                self.assertEqual("146.4.0", rusty_v8_bazel.command_version(None))

    def test_artifact_bazel_configs_always_enable_upstream_libcxx(self) -> None:
        self.assertEqual(
            ["rusty-v8-upstream-libcxx"],
            rusty_v8_bazel.artifact_bazel_configs(),
        )
        self.assertEqual(
            ["rusty-v8-upstream-libcxx", "v8-release-compat"],
            rusty_v8_bazel.artifact_bazel_configs(["v8-release-compat"]),
        )
        self.assertEqual(
            ["rusty-v8-upstream-libcxx", "v8-release-compat"],
            rusty_v8_bazel.artifact_bazel_configs(
                ["rusty-v8-upstream-libcxx", "v8-release-compat"]
            ),
        )

    def test_bazel_commands_use_shared_buildbuddy_remote_config_library(self) -> None:
        with patch.dict(environ, {}, clear=True):
            self.assertEqual(
                [
                    "bazel",
                    "build",
                    "//third_party/v8:release",
                ],
                rusty_v8_bazel.bazel_command(
                    "build",
                    "--config=ci-v8",
                    "//third_party/v8:release",
                ),
            )
        with patch.dict(environ, {"BUILDBUDDY_API_KEY": "token"}, clear=True):
            self.assertEqual(
                [
                    "bazel",
                    "build",
                    "--config=buildbuddy-generic-rbe",
                    "--remote_header=x-buildbuddy-api-key=token",
                    "--config=ci-v8",
                    "//third_party/v8:release",
                ],
                rusty_v8_bazel.bazel_command(
                    "build",
                    "--config=ci-v8",
                    "//third_party/v8:release",
                ),
            )

    def test_release_pair_labels_and_staged_names_distinguish_sandbox_artifacts(
        self,
    ) -> None:
        self.assertEqual(
            "//third_party/v8:rusty_v8_release_pair_x86_64_unknown_linux_musl",
            rusty_v8_bazel.release_pair_label("x86_64-unknown-linux-musl"),
        )
        self.assertEqual(
            "//third_party/v8:rusty_v8_sandbox_release_pair_x86_64_unknown_linux_musl",
            rusty_v8_bazel.release_pair_label(
                "x86_64-unknown-linux-musl", sandbox=True
            ),
        )
        self.assertEqual(
            "//third_party/v8:rusty_v8_sandbox_release_pair_x86_64_apple_darwin",
            rusty_v8_bazel.release_pair_label("x86_64-apple-darwin", sandbox=True),
        )
        self.assertEqual(
            "librusty_v8_release_x86_64-unknown-linux-musl.a.gz",
            rusty_v8_bazel.staged_archive_name(
                "x86_64-unknown-linux-musl",
                Path("libv8.a"),
                rusty_v8_bazel.RELEASE_ARTIFACT_PROFILE,
            ),
        )
        self.assertEqual(
            "rusty_v8_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.lib.gz",
            rusty_v8_bazel.staged_archive_name(
                "x86_64-pc-windows-msvc",
                Path("v8.a"),
                rusty_v8_bazel.SANDBOX_ARTIFACT_PROFILE,
            ),
        )
        self.assertEqual(
            "src_binding_ptrcomp_sandbox_release_x86_64-unknown-linux-musl.rs",
            rusty_v8_bazel.staged_binding_name(
                "x86_64-unknown-linux-musl",
                rusty_v8_bazel.SANDBOX_ARTIFACT_PROFILE,
            ),
        )
        self.assertEqual(
            "rusty_v8_ptrcomp_sandbox_release_x86_64-unknown-linux-musl.sha256",
            rusty_v8_bazel.staged_checksums_name(
                "x86_64-unknown-linux-musl",
                rusty_v8_bazel.SANDBOX_ARTIFACT_PROFILE,
            ),
        )

    def test_stage_artifacts(self) -> None:
        with TemporaryDirectory() as source_dir, TemporaryDirectory() as output_dir:
            source_root = Path(source_dir)
            archive = source_root / "librusty_v8.a"
            binding = source_root / "src_binding.rs"
            archive.write_bytes(b"archive")
            binding.write_text("binding")

            rusty_v8_bazel.stage_artifacts(
                "aarch64-apple-darwin",
                archive,
                binding,
                Path(output_dir),
                sandbox=True,
            )

            self.assertEqual(
                {
                    "librusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.a.gz",
                    "src_binding_ptrcomp_sandbox_release_aarch64-apple-darwin.rs",
                    "rusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.sha256",
                },
                {path.name for path in Path(output_dir).iterdir()},
            )
            checksums = (
                Path(output_dir)
                / "rusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.sha256"
            ).read_bytes()
            self.assertNotIn(b"\r", checksums)

    def test_upstream_release_pair_paths(self) -> None:
        self.assertEqual(
            (
                Path(
                    "/tmp/rusty_v8/target/x86_64-apple-darwin/release/gn_out/obj/"
                    "librusty_v8.a"
                ),
                Path(
                    "/tmp/rusty_v8/target/x86_64-apple-darwin/release/gn_out/"
                    "src_binding.rs"
                ),
            ),
            rusty_v8_bazel.upstream_release_pair_paths(
                "x86_64-apple-darwin",
                Path("/tmp/rusty_v8/target"),
            ),
        )
        self.assertEqual(
            (
                Path(
                    "/tmp/rusty_v8/target/x86_64-pc-windows-msvc/release/gn_out/"
                    "obj/rusty_v8.lib"
                ),
                Path(
                    "/tmp/rusty_v8/target/x86_64-pc-windows-msvc/release/gn_out/"
                    "src_binding.rs"
                ),
            ),
            rusty_v8_bazel.upstream_release_pair_paths(
                "x86_64-pc-windows-msvc",
                Path("/tmp/rusty_v8/target"),
            ),
        )

    def test_stage_upstream_release_pair(self) -> None:
        with (
            TemporaryDirectory() as target_dir,
            TemporaryDirectory() as output_dir,
        ):
            gn_out = Path(target_dir) / "x86_64-pc-windows-msvc" / "release" / "gn_out"
            (gn_out / "obj").mkdir(parents=True)
            (gn_out / "obj" / "rusty_v8.lib").write_bytes(b"archive")
            (gn_out / "src_binding.rs").write_text("binding")

            rusty_v8_bazel.stage_upstream_release_pair(
                "x86_64-pc-windows-msvc",
                Path(output_dir),
                Path(target_dir),
                sandbox=True,
            )

            self.assertEqual(
                {
                    "rusty_v8_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.lib.gz",
                    "src_binding_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.rs",
                    "rusty_v8_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.sha256",
                },
                {path.name for path in Path(output_dir).iterdir()},
            )

    def test_ensure_bazel_output_files_rebuilds_existing_outputs(self) -> None:
        with TemporaryDirectory() as output_dir:
            output = Path(output_dir) / "libv8.a"
            output.write_bytes(b"archive")

            with (
                patch.object(rusty_v8_bazel, "bazel_build") as bazel_build,
                patch.object(
                    rusty_v8_bazel,
                    "bazel_output_files",
                    return_value=[output],
                ) as bazel_output_files,
            ):
                self.assertEqual(
                    [output],
                    rusty_v8_bazel.ensure_bazel_output_files(
                        "macos_arm64",
                        ["//third_party/v8:pair"],
                        "opt",
                        ["rusty-v8-upstream-libcxx"],
                    ),
                )

            bazel_build.assert_called_once_with(
                "macos_arm64",
                ["//third_party/v8:pair"],
                "opt",
                ["rusty-v8-upstream-libcxx"],
                download_toplevel=True,
            )
            bazel_output_files.assert_called_once_with(
                "macos_arm64",
                ["//third_party/v8:pair"],
                "opt",
                ["rusty-v8-upstream-libcxx"],
            )

    def test_update_module_bazel_replaces_and_inserts_sha256(self) -> None:
        module_bazel = textwrap.dedent(
            """\
            http_file(
                name = "rusty_v8_146_4_0_x86_64_unknown_linux_gnu_archive",
                downloaded_file_path = "librusty_v8_release_x86_64-unknown-linux-gnu.a.gz",
                sha256 = "0000000000000000000000000000000000000000000000000000000000000000",
                urls = [
                    "https://example.test/librusty_v8_release_x86_64-unknown-linux-gnu.a.gz",
                ],
            )

            http_file(
                name = "rusty_v8_146_4_0_x86_64_unknown_linux_musl_binding",
                downloaded_file_path = "src_binding_release_x86_64-unknown-linux-musl.rs",
                urls = [
                    "https://example.test/src_binding_release_x86_64-unknown-linux-musl.rs",
                ],
            )

            http_file(
                name = "rusty_v8_145_0_0_x86_64_unknown_linux_gnu_archive",
                downloaded_file_path = "librusty_v8_release_x86_64-unknown-linux-gnu.a.gz",
                sha256 = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                urls = [
                    "https://example.test/old.gz",
                ],
            )
            """
        )
        checksums = {
            "librusty_v8_release_x86_64-unknown-linux-gnu.a.gz": (
                "1111111111111111111111111111111111111111111111111111111111111111"
            ),
            "src_binding_release_x86_64-unknown-linux-musl.rs": (
                "2222222222222222222222222222222222222222222222222222222222222222"
            ),
        }

        updated = rusty_v8_module_bazel.update_module_bazel_text(
            module_bazel,
            checksums,
            "146.4.0",
        )

        self.assertEqual(
            textwrap.dedent(
                """\
                http_file(
                    name = "rusty_v8_146_4_0_x86_64_unknown_linux_gnu_archive",
                    downloaded_file_path = "librusty_v8_release_x86_64-unknown-linux-gnu.a.gz",
                    sha256 = "1111111111111111111111111111111111111111111111111111111111111111",
                    urls = [
                        "https://example.test/librusty_v8_release_x86_64-unknown-linux-gnu.a.gz",
                    ],
                )

                http_file(
                    name = "rusty_v8_146_4_0_x86_64_unknown_linux_musl_binding",
                    downloaded_file_path = "src_binding_release_x86_64-unknown-linux-musl.rs",
                    sha256 = "2222222222222222222222222222222222222222222222222222222222222222",
                    urls = [
                        "https://example.test/src_binding_release_x86_64-unknown-linux-musl.rs",
                    ],
                )

                http_file(
                    name = "rusty_v8_145_0_0_x86_64_unknown_linux_gnu_archive",
                    downloaded_file_path = "librusty_v8_release_x86_64-unknown-linux-gnu.a.gz",
                    sha256 = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                    urls = [
                        "https://example.test/old.gz",
                    ],
                )
                """
            ),
            updated,
        )
        rusty_v8_module_bazel.check_module_bazel_text(updated, checksums, "146.4.0")

    def test_check_module_bazel_rejects_manifest_drift(self) -> None:
        module_bazel = textwrap.dedent(
            """\
            http_file(
                name = "rusty_v8_146_4_0_x86_64_unknown_linux_gnu_archive",
                downloaded_file_path = "librusty_v8_release_x86_64-unknown-linux-gnu.a.gz",
                sha256 = "1111111111111111111111111111111111111111111111111111111111111111",
                urls = [
                    "https://example.test/librusty_v8_release_x86_64-unknown-linux-gnu.a.gz",
                ],
            )
            """
        )
        checksums = {
            "librusty_v8_release_x86_64-unknown-linux-gnu.a.gz": (
                "1111111111111111111111111111111111111111111111111111111111111111"
            ),
            "orphan.gz": (
                "2222222222222222222222222222222222222222222222222222222222222222"
            ),
        }

        with self.assertRaisesRegex(
            rusty_v8_module_bazel.RustyV8ChecksumError,
            "manifest has orphan.gz",
        ):
            rusty_v8_module_bazel.check_module_bazel_text(
                module_bazel,
                checksums,
                "146.4.0",
            )

    def test_rusty_v8_http_file_versions(self) -> None:
        module_bazel = textwrap.dedent(
            """\
            http_file(
                name = "rusty_v8_146_4_0_x86_64_unknown_linux_gnu_archive",
                downloaded_file_path = "archive.gz",
                urls = ["https://example.test/archive.gz"],
            )

            http_file(
                name = "rusty_v8_147_4_0_x86_64_unknown_linux_gnu_archive",
                downloaded_file_path = "new-archive.gz",
                urls = ["https://example.test/new-archive.gz"],
            )

            http_file(
                name = "unrelated_archive",
                downloaded_file_path = "other.gz",
                urls = ["https://example.test/other.gz"],
            )
            """
        )

        self.assertEqual(
            ["146.4.0", "147.4.0"],
            rusty_v8_module_bazel.rusty_v8_http_file_versions(module_bazel),
        )


if __name__ == "__main__":
    unittest.main()
