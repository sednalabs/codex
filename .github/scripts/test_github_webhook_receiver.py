#!/usr/bin/env python3
"""Tests for the GitHub webhook receiver helper."""

from __future__ import annotations

import hashlib
import hmac
import importlib.util
import os
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
RECEIVER_PATH = REPO_ROOT / "scripts/github_webhook_receiver.py"

spec = importlib.util.spec_from_file_location("github_webhook_receiver", RECEIVER_PATH)
assert spec is not None
receiver = importlib.util.module_from_spec(spec)
assert spec.loader is not None
sys.modules["github_webhook_receiver"] = receiver
spec.loader.exec_module(receiver)


class GitHubWebhookReceiverTests(unittest.TestCase):
    def test_signature_verification_requires_sha256_hmac(self) -> None:
        body = b'{"zen":"keep it logically scoped"}'
        secret = b"test-secret"
        signature = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()

        self.assertTrue(receiver.constant_time_signature_ok(secret, body, signature))
        self.assertFalse(receiver.constant_time_signature_ok(secret, body, "sha1=bad"))
        self.assertFalse(receiver.constant_time_signature_ok(secret, body + b"!", signature))

    def test_route_matching_is_event_action_and_repository_scoped(self) -> None:
        route = {
            "event": "release",
            "actions": ["published"],
            "repository": "sednalabs/codex",
        }

        self.assertTrue(receiver.route_matches(route, "release", "published", "sednalabs/codex"))
        self.assertTrue(receiver.route_matches(route, "release", "published", "SednaLabs/Codex"))
        self.assertFalse(receiver.route_matches(route, "release", "created", "sednalabs/codex"))
        self.assertFalse(receiver.route_matches(route, "push", "published", "sednalabs/codex"))
        self.assertFalse(receiver.route_matches(route, "release", "published", "example/repo"))

    def test_command_expansion_allows_only_context_and_payload_fields(self) -> None:
        payload = {
            "repository": {"full_name": "sednalabs/codex"},
            "release": {"tag_name": "v0.126.0-alpha.4-sedna.1"},
        }
        context = {
            "event": "release",
            "action": "published",
            "delivery": "delivery-id",
            "repository": "sednalabs/codex",
        }
        route = {
            "command": [
                "scripts/install_sedna_release_asset",
                "--repository",
                "{repository}",
                "--release-tag",
                "{payload.release.tag_name}",
                "--allow-prerelease",
            ],
        }

        self.assertEqual(
            receiver.build_command(route, payload, context),
            [
                "scripts/install_sedna_release_asset",
                "--repository",
                "sednalabs/codex",
                "--release-tag",
                "v0.126.0-alpha.4-sedna.1",
                "--allow-prerelease",
            ],
        )
        with self.assertRaisesRegex(receiver.WebhookError, "unsupported command placeholder"):
            receiver.expand_token("{HOME}", payload, context)

    def test_config_loads_secret_from_file_and_lock_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            secret_path = root / "secret"
            secret_path.write_text("top-secret\n", encoding="utf-8")
            config_path = root / "config.json"
            config_path.write_text(
                """
{
  "lock_path": "install.lock",
  "timeout_seconds": 42,
  "routes": [
    {
      "id": "release-install",
      "event": "release",
      "actions": ["published"],
      "command": ["true"]
    }
  ]
}
""".strip(),
                encoding="utf-8",
            )

            old_secret = os.environ.pop("GITHUB_WEBHOOK_SECRET", None)
            old_secret_file = os.environ.get("GITHUB_WEBHOOK_SECRET_FILE")
            os.environ["GITHUB_WEBHOOK_SECRET_FILE"] = str(secret_path)
            try:
                config = receiver.load_config(config_path)
            finally:
                if old_secret is not None:
                    os.environ["GITHUB_WEBHOOK_SECRET"] = old_secret
                if old_secret_file is not None:
                    os.environ["GITHUB_WEBHOOK_SECRET_FILE"] = old_secret_file
                else:
                    os.environ.pop("GITHUB_WEBHOOK_SECRET_FILE", None)

        self.assertEqual(config.secret, b"top-secret")
        self.assertEqual(config.timeout_seconds, 42)
        self.assertEqual(config.lock_path, Path("install.lock"))


if __name__ == "__main__":
    unittest.main()
