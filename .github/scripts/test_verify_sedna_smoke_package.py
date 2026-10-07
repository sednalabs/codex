#!/usr/bin/env python3
"""Focused offline contract tests for the SmokePackage consumer helpers."""

from __future__ import annotations

import importlib.util
import io
import json
import sys
import tarfile
import tempfile
from pathlib import Path
from unittest import TestCase, main


SCRIPT_DIR = Path(__file__).resolve().parent


def load_module(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPT_DIR / filename)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


VERIFY = load_module("verify_sedna_smoke_package", "verify_sedna_smoke_package.py")
RUN_BROWSER = load_module("run_sedna_browser_smoke", "run_sedna_browser_smoke.py")


def package_archive(path: Path, *, variant: str, extra_member: str | None = None) -> None:
    directories, required_files, optional_files = VERIFY._expected_package_paths(variant)
    entrypoint = "codex" if variant == "codex" else "codex-app-server"
    manifest = {
        "layoutVersion": 1,
        "version": "0.0.0",
        "target": "x86_64-unknown-linux-gnu",
        "variant": variant,
        "entrypoint": f"bin/{entrypoint}",
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
    }
    payloads = {
        name: (json.dumps(manifest).encode() if name == "codex-package.json" else b"binary")
        for name in required_files
    }
    with tarfile.open(path, "w:gz") as archive:
        for name in sorted(directories):
            member = tarfile.TarInfo(name)
            member.type = tarfile.DIRTYPE
            member.mode = 0o755
            archive.addfile(member)
        for name, payload in sorted(payloads.items()):
            member = tarfile.TarInfo(name)
            member.mode = 0o644 if name == "codex-package.json" else 0o755
            member.size = len(payload)
            archive.addfile(member, io.BytesIO(payload))
        if extra_member is not None:
            member = tarfile.TarInfo(extra_member)
            member.mode = 0o644
            member.size = 1
            archive.addfile(member, io.BytesIO(b"x"))


def symbols_archive(path: Path, *, run_id: str, target: str, extra_name: str | None = None) -> None:
    root = f"codex-symbols-sedna-{run_id}-{target}"
    with tarfile.open(path, "w:gz") as archive:
        directory = tarfile.TarInfo(root)
        directory.type = tarfile.DIRTYPE
        directory.mode = 0o755
        archive.addfile(directory)
        names = [
            f"{root}/codex.debug",
            f"{root}/codex-app-server.debug",
            f"{root}/codex-code-mode-host.debug",
        ]
        if extra_name is not None:
            names.append(extra_name)
        for name in names:
            member = tarfile.TarInfo(name)
            member.mode = 0o644
            member.size = 1
            archive.addfile(member, io.BytesIO(b"x"))


class SmokePackageVerifierTests(TestCase):
    def test_exact_package_inventory_extracts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "package.tar.gz"
            package_archive(archive, variant="codex")
            destination = root / "package"

            manifest = VERIFY._safe_extract_package(
                archive, destination, variant="codex"
            )

            self.assertEqual(manifest["variant"], "codex")
            self.assertTrue((destination / "bin/codex").is_file())
            self.assertTrue((destination / "bin/codex").stat().st_mode & 0o111)

    def test_exact_symbols_inventory_is_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "symbols.tar.gz"
            symbols_archive(
                archive,
                run_id="123456789",
                target="x86_64-unknown-linux-gnu",
            )

            VERIFY._validate_symbols_archive(
                archive,
                run_id="123456789",
                target="x86_64-unknown-linux-gnu",
            )

    def test_symbols_inventory_rejects_unlisted_members(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "symbols.tar.gz"
            symbols_archive(
                archive,
                run_id="123456789",
                target="x86_64-unknown-linux-gnu",
                extra_name="codex-symbols-sedna-123456789-x86_64-unknown-linux-gnu/private.txt",
            )

            with self.assertRaises(VERIFY.SmokePackageFailure) as failure:
                VERIFY._validate_symbols_archive(
                    archive,
                    run_id="123456789",
                    target="x86_64-unknown-linux-gnu",
                )

            self.assertEqual(failure.exception.code, "symbols_archive_member_rejected")

    def test_package_parent_traversal_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "package.tar.gz"
            package_archive(archive, variant="codex", extra_member="../escaped")

            with self.assertRaises(VERIFY.SmokePackageFailure) as failure:
                VERIFY._safe_extract_package(archive, root / "package", variant="codex")

            self.assertEqual(failure.exception.code, "package_archive_member_rejected")
            self.assertFalse((root / "escaped").exists())

    def test_unknown_package_member_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "package.tar.gz"
            package_archive(archive, variant="codex", extra_member="bin/unlisted")

            with self.assertRaises(VERIFY.SmokePackageFailure) as failure:
                VERIFY._safe_extract_package(archive, root / "package", variant="codex")

            self.assertEqual(failure.exception.code, "package_archive_member_rejected")

    def test_cli_version_classification_is_exact_and_bounded(self) -> None:
        source_sha = "0123456789abcdef" * 2 + "01234567"
        self.assertEqual(
            VERIFY._classify_cli_version(
                "codex 0.143.0-alpha.10-dev.sedna.4+g01234567 (git:01234567)",
                "0.0.0",
                "0.143.0-alpha.10-dev.sedna.4+g01234567",
                source_sha,
            ),
            "preview_version_with_source_sha",
        )
        self.assertEqual(
            VERIFY._classify_cli_version(
                "codex 0.0.0 (Sedna dev g01234567)",
                "0.0.0",
                "0.143.0-alpha.10-dev.sedna.4+g01234567",
                source_sha,
            ),
            "progressive_source_identity",
        )
        self.assertEqual(
            VERIFY._classify_cli_version(
                "unexpected private output",
                "0.0.0",
                "0.143.0-alpha.10-dev.sedna.4+g01234567",
                source_sha,
            ),
            "unrecognized",
        )

    def test_package_manifest_requires_integer_layout_version(self) -> None:
        manifest = {
            "layoutVersion": 1,
            "version": "0.0.0",
            "target": "x86_64-unknown-linux-gnu",
            "variant": "codex",
            "entrypoint": "bin/codex",
            "resourcesDir": "codex-resources",
            "pathDir": "codex-path",
        }
        self.assertTrue(
            VERIFY._manifest_for_variant(
                manifest,
                variant="codex",
                target="x86_64-unknown-linux-gnu",
                version="0.0.0",
            )
        )
        manifest["layoutVersion"] = True
        self.assertFalse(
            VERIFY._manifest_for_variant(
                manifest,
                variant="codex",
                target="x86_64-unknown-linux-gnu",
                version="0.0.0",
            )
        )

    def test_junit_failure_summary_discards_raw_failure_text(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            junit = Path(temporary) / "junit.xml"
            junit.write_text(
                "<testsuite><testcase name=\"%s\"><failure message=\"private\">secret payload</failure></testcase></testsuite>"
                % RUN_BROWSER.EXPECTED_TEST,
                encoding="utf-8",
            )

            result = RUN_BROWSER._junit_result(junit, return_code=1)
            serialized = json.dumps(result)

            self.assertEqual(result["result"], "failed")
            self.assertEqual(result["failed"], 1)
            self.assertNotIn("secret payload", serialized)
            self.assertNotIn("message", serialized)

    def test_browser_diagnostic_emits_only_allowlisted_status_scalars(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            junit = Path(temporary) / "junit.xml"
            diagnostic = {
                "fixture_function_call_emitted": True,
                "responses_request_count": 2,
                "second_request_observed": True,
                "second_request_input_available": True,
                "function_call_output_count": 1,
                "function_call_output_call_ids": ["private-call-id"],
                "function_call_output_tool_names": ["private-tool-name"],
                "synthetic_browser_provider_invoked": True,
                "synthetic_browser_provider_tool_name": "private-tool-name",
                "source_stage_observations": [
                    {
                        "stage": "browser_provider",
                        "call_id_matches_fixture": True,
                        "provider_process_exit_success": True,
                        "provider_json_parse_success": True,
                        "provider_content_item_count": 3,
                    }
                ],
            }
            junit.write_text(
                "<testsuite><testcase name=\"%s\"><properties><property "
                "name=\"browser_output_diagnostic_json\" value='%s'/>"
                "</properties></testcase></testsuite>"
                % (RUN_BROWSER.EXPECTED_TEST, json.dumps(diagnostic)),
                encoding="utf-8",
            )

            result = RUN_BROWSER._junit_result(junit, return_code=0)
            serialized = json.dumps(result)

            self.assertEqual(result["result"], "passed")
            safe = result["diagnostic"]
            self.assertEqual(safe["function_call_output_count"], 1)
            self.assertTrue(safe["source_stage_observations"][0]["provider_json_parse_success"])
            self.assertNotIn("private-call-id", serialized)
            self.assertNotIn("private-tool-name", serialized)

    def test_browser_diagnostic_suppresses_unexpected_fields(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            junit = Path(temporary) / "junit.xml"
            diagnostic = {
                "fixture_function_call_emitted": True,
                "responses_request_count": 2,
                "second_request_observed": True,
                "second_request_input_available": True,
                "function_call_output_count": 1,
                "function_call_output_call_ids": ["fixture"],
                "function_call_output_tool_names": ["browser_step"],
                "synthetic_browser_provider_invoked": True,
                "synthetic_browser_provider_tool_name": "browser_step",
                "source_stage_observations": [],
                "unreviewed_text": "not safe to publish",
            }
            junit.write_text(
                "<testsuite><testcase name=\"%s\"><properties><property "
                "name=\"browser_output_diagnostic_json\" value='%s'/>"
                "</properties></testcase></testsuite>"
                % (RUN_BROWSER.EXPECTED_TEST, json.dumps(diagnostic)),
                encoding="utf-8",
            )

            result = RUN_BROWSER._junit_result(junit, return_code=0)

            self.assertEqual(result["diagnostic_status"], "suppressed_or_unavailable")


if __name__ == "__main__":
    main()
