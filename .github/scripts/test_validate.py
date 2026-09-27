"""Security regression tests for the hosted validation dispatcher.

These tests are intentionally negative as well as positive: the dispatcher
must keep the executable and option names fixed while rejecting values that
could otherwise be interpreted as options or shell syntax.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
from unittest import TestCase, main


VALIDATE_PATH = Path(__file__).parents[2] / "scripts" / "validate"
SPEC = importlib.util.spec_from_file_location("codex_validate", VALIDATE_PATH)
if SPEC is None or SPEC.loader is None:  # pragma: no cover - import failure
    raise RuntimeError(f"cannot load validation dispatcher: {VALIDATE_PATH}")
VALIDATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VALIDATE)


class ValidateArgvTests(TestCase):
    def request(self, **overrides: object) -> str:
        payload = {
            "schema_version": "rust-tests-v1",
            "profile": "rust_minimal",
            "package": "codex-core",
            "target_kind": "lib",
            "target": "",
            "tests": ["suite::known"],
        }
        payload.update(overrides)
        return json.dumps(payload)

    def args(self) -> object:
        return type(
            "Args",
            (),
            {
                "repo": "sednalabs/codex",
                "workflow_host_ref": "main",
                "target_sha": "a" * 40,
                "base_ref": "main",
                "profile": "rust_minimal",
            },
        )()

    def test_safe_values_remain_separate_argv_values(self) -> None:
        command = VALIDATE.gh_command(self.args(), self.request())

        self.assertEqual(command[:4], ["gh", "workflow", "run", "validation-named-tests.yml"])
        self.assertEqual(command[4], "--repo")
        self.assertEqual(command[5], "sednalabs/codex")
        self.assertNotIn(";", "".join(command))
        self.assertIn("request_json=", command[-1])

    def test_rejects_untrusted_command_inputs(self) -> None:
        for field, value in {
            "repo": "sednalabs/codex;echo nope",
            "workflow_host_ref": "--evil",
            "base_ref": "main && echo nope",
            "target_sha": "a" * 39 + ";",
        }.items():
            with self.subTest(field=field):
                args = self.args()
                setattr(args, field, value)
                with self.assertRaises(SystemExit):
                    VALIDATE.gh_command(args, self.request())

    def test_rejects_unapproved_profile(self) -> None:
        args = self.args()
        args.profile = "rust_minimal; echo nope"

        with self.assertRaises(SystemExit):
            VALIDATE.gh_command(args, self.request())

    def test_watch_validation_does_not_require_dispatch_sha(self) -> None:
        args = self.args()
        args.target_sha = None

        VALIDATE.validate_command_inputs(args, require_target_sha=False)

    def test_rejects_untrusted_typed_request_values(self) -> None:
        for overrides in (
            {"package": "--workspace"},
            {"target_kind": "integration", "target": "$(touch nope)"},
            {"tests": ["suite::known; echo nope"]},
            {"tests": ["suite::known", "suite::known"]},
        ):
            with self.subTest(overrides=overrides):
                with self.assertRaises(SystemExit):
                    VALIDATE.normalize_request(self.request(**overrides), "rust_minimal")


if __name__ == "__main__":
    main()
