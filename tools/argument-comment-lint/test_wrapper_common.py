#!/usr/bin/env python3

from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import wrapper_common


class WrapperCommonTest(unittest.TestCase):
    def test_prebuilt_cargo_dylint_defaults_to_packaged_executable(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            package_dir = Path(temp_dir) / "argument-comment-lint" / "bin"
            package_dir.mkdir(parents=True)
            package_entrypoint = package_dir / "argument-comment-lint"
            package_entrypoint.touch()
            packaged_cargo_dylint = package_dir / "cargo-dylint"
            packaged_cargo_dylint.touch()

            resolved = wrapper_common.resolve_prebuilt_cargo_dylint(
                package_entrypoint, {}
            )

        self.assertEqual(resolved, packaged_cargo_dylint)

    def test_prebuilt_cargo_dylint_override_uses_absolute_existing_executable(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            override = Path(temp_dir) / "cargo-dylint"
            override.touch()

            resolved = wrapper_common.resolve_prebuilt_cargo_dylint(
                Path("/package/bin/argument-comment-lint"),
                {wrapper_common.PREBUILT_CARGO_DYLINT_ENV: str(override)},
            )

        self.assertEqual(resolved, override.resolve())

    def test_prebuilt_cargo_dylint_override_rejects_missing_executable(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            override = Path(temp_dir) / "missing-cargo-dylint"

            with self.assertRaises(SystemExit):
                wrapper_common.resolve_prebuilt_cargo_dylint(
                    Path("/package/bin/argument-comment-lint"),
                    {wrapper_common.PREBUILT_CARGO_DYLINT_ENV: str(override)},
                )

    def test_defaults_to_workspace_and_all_targets(self) -> None:
        parsed = wrapper_common.parse_wrapper_args([])
        final_args = wrapper_common.build_final_args(parsed, Path("/repo/codex-rs/Cargo.toml"))

        self.assertEqual(
            final_args,
            [
                "--manifest-path",
                "/repo/codex-rs/Cargo.toml",
                "--workspace",
                "--no-deps",
                "--",
                "--all-targets",
            ],
        )

    def test_forwarded_cargo_args_keep_single_separator(self) -> None:
        parsed = wrapper_common.parse_wrapper_args(["-p", "codex-core", "--", "--tests"])
        final_args = wrapper_common.build_final_args(parsed, Path("/repo/codex-rs/Cargo.toml"))

        self.assertEqual(
            final_args,
            [
                "--manifest-path",
                "/repo/codex-rs/Cargo.toml",
                "--no-deps",
                "-p",
                "codex-core",
                "--",
                "--tests",
            ],
        )

    def test_fix_does_not_add_all_targets(self) -> None:
        parsed = wrapper_common.parse_wrapper_args(["--fix", "-p", "codex-core"])
        final_args = wrapper_common.build_final_args(parsed, Path("/repo/codex-rs/Cargo.toml"))

        self.assertEqual(
            final_args,
            [
                "--manifest-path",
                "/repo/codex-rs/Cargo.toml",
                "--no-deps",
                "--fix",
                "-p",
                "codex-core",
            ],
        )

    def test_explicit_manifest_and_workspace_are_preserved(self) -> None:
        parsed = wrapper_common.parse_wrapper_args(
            [
                "--manifest-path",
                "/tmp/custom/Cargo.toml",
                "--workspace",
                "--no-deps",
                "--",
                "--bins",
            ]
        )
        final_args = wrapper_common.build_final_args(parsed, Path("/repo/codex-rs/Cargo.toml"))

        self.assertEqual(
            final_args,
            [
                "--manifest-path",
                "/tmp/custom/Cargo.toml",
                "--workspace",
                "--no-deps",
                "--",
                "--bins",
            ],
        )

    def test_explicit_package_manifest_does_not_force_workspace(self) -> None:
        parsed = wrapper_common.parse_wrapper_args(
            [
                "--manifest-path",
                "/tmp/custom/Cargo.toml",
            ]
        )
        final_args = wrapper_common.build_final_args(parsed, Path("/repo/codex-rs/Cargo.toml"))

        self.assertEqual(
            final_args,
            [
                "--no-deps",
                "--manifest-path",
                "/tmp/custom/Cargo.toml",
                "--",
                "--all-targets",
            ],
        )

    def test_default_lint_env_promotes_both_strict_lints(self) -> None:
        env: dict[str, str] = {}

        wrapper_common.set_default_lint_env(env)

        self.assertEqual(
            env["DYLINT_RUSTFLAGS"],
            "-D argument-comment-mismatch "
            "-D uncommented-anonymous-literal-argument "
            "-A unknown_lints",
        )
        self.assertEqual(env["CARGO_INCREMENTAL"], "0")


if __name__ == "__main__":
    unittest.main()
