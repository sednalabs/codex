"""Regression tests for the hosted named-Rust-test command catalog."""

from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import run_named_rust_tests as named_tests


class NamedRustTestManifestTests(unittest.TestCase):
    def write_bin_manifest(
        self,
        root: Path,
        target: str = "codex",
        *,
        inventory_argv: list[str] | None = None,
    ) -> None:
        inventory, execution = named_tests.expected_commands(
            "codex-cli", "bin", target
        )
        path = root / ".github" / named_tests.MANIFEST_NAME
        path.parent.mkdir(parents=True)
        path.write_text(
            json.dumps(
                {
                    "schema_version": named_tests.MANIFEST_SCHEMA_VERSION,
                    "targets": [
                        {
                            "package": "codex-cli",
                            "target_kind": "bin",
                            "target": target,
                            "profiles": ["rust_minimal"],
                            "inventory_argv": inventory_argv or inventory,
                            "execution_argv": execution,
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )

    def test_bin_catalog_entry_uses_the_closed_cargo_command(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.write_bin_manifest(root)
            manifest = named_tests.load_manifest(root)
            request = {
                "package": "codex-cli",
                "target_kind": "bin",
                "target": "codex",
                "profile": "rust_minimal",
            }

            record = named_tests.select_target(request, manifest)

            self.assertEqual(
                named_tests.cargo_args(request, list_only=True, command_record=record),
                ["cargo", "test", "--locked", "-p", "codex-cli", "--bin", "codex", "--", "--list"],
            )
            self.assertEqual(
                named_tests.cargo_args(request, list_only=False, command_record=record),
                [
                    "cargo",
                    "test",
                    "--locked",
                    "-p",
                    "codex-cli",
                    "--bin",
                    "codex",
                    "--",
                    "--test-threads=1",
                ],
            )
            with patch.dict(
                os.environ,
                {
                    "RUST_TEST_REQUEST_JSON": json.dumps(
                        {
                            "schema_version": named_tests.SCHEMA_VERSION,
                            **request,
                            "tests": ["codex::tests::version"],
                        }
                    ),
                    "VALIDATION_PROFILE": "rust_minimal",
                },
            ):
                parsed, error = named_tests.load_request()
            self.assertIsNone(error)
            self.assertEqual(parsed["target_kind"], "bin")

    def test_bin_catalog_rejects_unsafe_targets_and_noncanonical_commands(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.write_bin_manifest(root, "codex;--all")
            with self.assertRaisesRegex(ValueError, "bin target is not safe"):
                named_tests.load_manifest(root)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.write_bin_manifest(root, inventory_argv=["cargo", "test", "--lib"])
            with self.assertRaisesRegex(ValueError, "command tuples must exactly match"):
                named_tests.load_manifest(root)


if __name__ == "__main__":
    unittest.main()
