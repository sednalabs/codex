#!/usr/bin/env python3
"""Focused offline contract tests for the SmokePackage consumer helpers."""

from __future__ import annotations

import importlib.util
import io
import json
import os
import sys
import tarfile
import tempfile
from subprocess import CompletedProcess
from pathlib import Path
from unittest import TestCase, main
from unittest.mock import patch


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
FINALIZE = load_module("finalize_sedna_smoke_consumer", "finalize_sedna_smoke_consumer.py")


def package_archive(
    path: Path,
    *,
    variant: str,
    extra_member: str | None = None,
    manifest_payload: bytes | None = None,
    pax_member: bool = False,
) -> None:
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
    if manifest_payload is not None:
        payloads["codex-package.json"] = manifest_payload
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
            if pax_member and name == "bin/codex":
                member.pax_headers = {"comment": "bounded fixture"}
            archive.addfile(member, io.BytesIO(payload))
        if extra_member is not None:
            member = tarfile.TarInfo(extra_member)
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

    def test_oversized_manifest_is_rejected_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "package.tar.gz"
            package_archive(
                archive,
                variant="codex",
                manifest_payload=b"x" * (VERIFY.MAX_MANIFEST_BYTES + 1),
            )
            destination = root / "extracted"

            with self.assertRaises(VERIFY.SmokePackageFailure) as failure:
                VERIFY._safe_extract_package(archive, destination, variant="codex")

            self.assertEqual(failure.exception.code, "package_manifest_invalid")
            self.assertFalse(destination.exists())

    def test_tar_extension_is_rejected_before_tarfile_parses_payload(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "package.tar.gz"
            package_archive(archive, variant="codex", pax_member=True)

            with self.assertRaises(VERIFY.SmokePackageFailure) as failure:
                VERIFY._safe_extract_package(archive, root / "extracted", variant="codex")

            self.assertEqual(
                failure.exception.code,
                "package_archive_header_extension_rejected",
            )
            self.assertFalse((root / "extracted").exists())

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
                "codex 0.0.0 (Sedna dev 01234567)",
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

    def test_browser_child_does_not_receive_host_output_paths(self) -> None:
        with patch.dict(
            os.environ,
            {
                "RESULT_PATH": "/private/result.json",
                "JUNIT_PATH": "/private/results.xml",
                "ARTIFACT_DIR": "/private/artifact",
                "RUNNER_TEMP": "/private/temp",
                "CODEX_BROWSER_PRIVATE": "must-not-pass",
            },
            clear=False,
        ):
            child = RUN_BROWSER._child_environment(Path("/source"))

        self.assertEqual(child["PYTHONPATH"], os.pathsep.join(("/source/sdk/python/src", "/source/sdk/python/tests")))
        self.assertNotIn("RESULT_PATH", child)
        self.assertNotIn("JUNIT_PATH", child)
        self.assertNotIn("ARTIFACT_DIR", child)
        self.assertNotIn("RUNNER_TEMP", child)
        self.assertNotIn("CODEX_BROWSER_PRIVATE", child)

    def test_package_result_writer_refuses_symlink_overwrite(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target.json"
            target.write_text("unchanged", encoding="utf-8")
            result = root / "result.json"
            result.symlink_to(target)

            with self.assertRaises(VERIFY.SmokePackageFailure) as failure:
                VERIFY._write_result_exclusive(result, {"status": "safe"})

            self.assertEqual(failure.exception.code, "result_write_failed")
            self.assertEqual(target.read_text(encoding="utf-8"), "unchanged")

    def test_finalizer_result_writer_refuses_symlink_overwrite(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target.json"
            target.write_text("unchanged", encoding="utf-8")
            result = root / "result.json"
            result.symlink_to(target)

            with self.assertRaises(OSError):
                FINALIZE._write_result_exclusive(result, {"status": "safe"})

            self.assertEqual(target.read_text(encoding="utf-8"), "unchanged")

    def test_finalizer_requires_the_one_exact_browser_test_status(self) -> None:
        result = {
            "schema_version": "sedna-browser-smoke-result-v1",
            "source_sha": "0" * 40,
            "workflow_sha": "1" * 40,
            "run_id": "123",
            "target": "x86_64-unknown-linux-gnu",
            "result": "passed",
            "failure_code": None,
            "observed_test_count": 1,
            "passed": 1,
            "failed": 0,
            "errors": 0,
            "skipped": 0,
        }
        self.assertTrue(FINALIZE._browser_passed(result))
        result["failed"] = 1
        self.assertFalse(FINALIZE._browser_passed(result))
        result["failed"] = 0
        result["unexpected_payload"] = "not allowed"
        self.assertFalse(FINALIZE._browser_passed(result))

    def test_app_server_consumer_invokes_packaged_executable(self) -> None:
        app_server = Path("/runner/temp/app-server/bin/codex-app-server")
        with patch.object(
            VERIFY,
            "_run_packaged_command",
            return_value=CompletedProcess([str(app_server), "--help"], 0, "", ""),
        ) as command:
            self.assertTrue(VERIFY._invoke_app_server_help(app_server, cwd=Path("/runner/temp")))

        command.assert_called_once_with([str(app_server), "--help"], cwd=Path("/runner/temp"))


if __name__ == "__main__":
    main()
