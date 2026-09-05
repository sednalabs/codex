#!/usr/bin/env python3
"""Secret-free, mock-only contract tests for github_app_installation_broker."""

import base64
import json
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from github_app_installation_broker import (
    ALLOWED_PERMISSIONS,
    BrokerError,
    GitHubAppBroker,
    HttpResult,
    MAX_HTTP_BODY,
    _run_child_bounded,
    _production_credentials_directory,
    _validate_private_key,
    build_jwt_claims,
    fingerprint_command,
    main,
)


class FakeClock:
    def __init__(self, value=1_700_000_000):
        self.value = value

    def __call__(self):
        return self.value


class FakeGitHub:
    def __init__(self, *, token="opaque-installation-token", expiry="2030-01-01T00:00:00Z"):
        self.calls = []
        self.token = token
        self.expiry = expiry
        self.installation = {
            "id": 42,
            "app_id": 7,
            "app_slug": "sedna-codex-delivery-coordinator",
            "account": {"login": "example-org"},
            "target_type": "Organization",
            "repository_selection": "selected",
            "suspended_at": None,
            "events": [],
            "permissions": {name: "read" for name in ALLOWED_PERMISSIONS},
        }

    def __call__(self, method, url, headers, data):
        self.calls.append((method, url, dict(headers), data))
        if method == "GET" and url.endswith("/app"):
            return HttpResult(
                200,
                {"X-RateLimit-Limit": "5000"},
                json.dumps(
                    {
                        "id": 7,
                        "slug": "sedna-codex-delivery-coordinator",
                        "owner": {"login": "example-org", "type": "Organization"},
                        "permissions": {name: "read" for name in ALLOWED_PERMISSIONS},
                        "events": [],
                    }
                ).encode(),
                url,
            )
        if method == "GET":
            return HttpResult(200, {"X-RateLimit-Limit": "5000"}, json.dumps(self.installation).encode(), url)
        if method == "POST":
            return HttpResult(
                201,
                {"X-RateLimit-Remaining": "4999"},
                json.dumps(
                    {
                        "token": self.token,
                        "expires_at": self.expiry,
                        "repository_selection": "selected",
                        "repositories": [{"full_name": "example-org/codex"}],
                        "permissions": {"metadata": "read", "contents": "read"},
                    }
                ).encode(),
                url,
            )
        if method == "DELETE":
            return HttpResult(204, {}, b"", url)
        raise AssertionError(method)


def make_broker(fake, clock=None):
    directory = tempfile.TemporaryDirectory()
    key = Path(directory.name) / "github-app-private-key.pem"
    key.write_bytes(b"test fixture key material")
    key.chmod(0o600)
    with mock.patch("github_app_installation_broker._production_credentials_directory", return_value=Path(directory.name)):
        broker = GitHubAppBroker(
            app_id=7,
            app_slug="sedna-codex-delivery-coordinator",
            installation_id=42,
            account="example-org",
            repository="example-org/codex",
            permissions={"metadata": "read", "contents": "read"},
            requester=fake,
            clock=clock or FakeClock(),
        )
    broker._jwt = mock.Mock(return_value="header.claim.signature")
    return directory, broker


class BrokerTests(unittest.TestCase):
    def test_permission_reduction_and_selected_repository_binding(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        record = broker.get_installation_token()
        self.assertEqual({"metadata": "read", "contents": "read"}, dict(record.permissions))
        post = next(call for call in fake.calls if call[0] == "POST")
        request_payload = json.loads(post[3].decode("utf-8"))
        self.assertEqual({"repositories": ["codex"], "permissions": {"contents": "read", "metadata": "read"}}, request_payload)
        self.assertNotIn("write", json.dumps(request_payload))

    def test_app_and_installation_identity_and_read_only_ceiling_are_verified(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        broker.get_installation_token()
        self.assertEqual({name: "read" for name in ALLOWED_PERMISSIONS}, fake.installation["permissions"])
        self.assertEqual("sedna-codex-delivery-coordinator", fake.installation["app_slug"])
        self.assertTrue(all(call[2]["X-GitHub-Api-Version"] == "2022-11-28" for call in fake.calls))

    def test_identity_or_grant_drift_fails_closed(self):
        fake = FakeGitHub()
        fake.installation["app_slug"] = "other-app"
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        with self.assertRaises(BrokerError):
            broker.get_installation_token()

        fake = FakeGitHub()
        fake.installation["permissions"]["contents"] = "write"
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        with self.assertRaises(BrokerError):
            broker.get_installation_token()

        fake = FakeGitHub()
        del fake.installation["permissions"]["administration"]
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        with self.assertRaises(BrokerError):
            broker.get_installation_token()

    def test_jwt_claims_are_bounded_without_material(self):
        header, claims = build_jwt_claims(7, 1_700_000_000)
        decoded_header = json.loads(base64.urlsafe_b64decode(header + "=="))
        decoded_claims = json.loads(base64.urlsafe_b64decode(claims + "=="))
        self.assertEqual({"alg": "RS256", "typ": "JWT"}, decoded_header)
        self.assertEqual({"iss": 7, "iat": 1_699_999_940, "exp": 1_700_000_540}, decoded_claims)
        self.assertNotIn("token", header + claims)

    def test_cache_reuse_and_near_expiry_refresh(self):
        fake = FakeGitHub()
        clock = FakeClock()
        directory, broker = make_broker(fake, clock)
        self.addCleanup(directory.cleanup)
        first = broker.get_installation_token()
        self.assertIs(first, broker.get_installation_token())
        self.assertEqual(1, len([call for call in fake.calls if call[0] == "POST"]))
        clock.value = first.expires_epoch - 119
        broker.get_installation_token()
        self.assertEqual(2, len([call for call in fake.calls if call[0] == "POST"]))

    def test_refresh_revokes_before_minting_again(self):
        fake = FakeGitHub()
        clock = FakeClock()
        directory, broker = make_broker(fake, clock)
        self.addCleanup(directory.cleanup)
        first = broker.get_installation_token()
        clock.value = first.expires_epoch - 119
        broker.get_installation_token()
        methods = [call[0] for call in fake.calls]
        self.assertLess(methods.index("DELETE"), len(methods) - 1)
        self.assertEqual("DELETE", methods[3])
        self.assertEqual("POST", methods[-1])

    def test_failed_refresh_revocation_is_sticky_and_terminal(self):
        fake = FakeGitHub()
        clock = FakeClock()
        directory, broker = make_broker(fake, clock)
        self.addCleanup(directory.cleanup)
        first = broker.get_installation_token()
        clock.value = first.expires_epoch - 119
        delete_attempts = 0

        def revoke_fails_then_succeeds(method, url, headers, data):
            nonlocal delete_attempts
            if method == "DELETE":
                delete_attempts += 1
                if delete_attempts == 1:
                    return HttpResult(500, {}, b"{}", url)
            return fake(method, url, headers, data)

        broker._requester = revoke_fails_then_succeeds
        with self.assertRaises(BrokerError):
            broker.get_installation_token()
        with self.assertRaises(BrokerError):
            broker.get_installation_token()
        command = ["/bin/echo", "x"]
        fingerprint = fingerprint_command(broker.repository, broker.permissions, command)
        with self.assertRaises(BrokerError):
            broker.execute(command, fingerprint)
        self.assertEqual(1, len([call for call in fake.calls if call[0] == "POST"]))
        receipt = broker.close()
        self.assertTrue(receipt["revoked"])
        self.assertEqual(2, delete_attempts)
        with self.assertRaises(BrokerError):
            broker.get_installation_token()

    def test_invalid_post_with_failed_cleanup_is_sticky_and_nonsecret(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)

        def invalid_post_cleanup_fails(method, url, headers, data):
            if method == "DELETE":
                return HttpResult(500, {}, b"{}", url)
            response = fake(method, url, headers, data)
            if method == "POST":
                payload = json.loads(response.body)
                payload["repository_selection"] = "all"
                return HttpResult(response.status, response.headers, json.dumps(payload).encode(), response.url)
            return response

        broker._requester = invalid_post_cleanup_fails
        with self.assertRaises(BrokerError):
            broker.get_installation_token()
        with self.assertRaises(BrokerError):
            broker.get_installation_token()
        command = ["/bin/echo", "x"]
        fingerprint = fingerprint_command(broker.repository, broker.permissions, command)
        with self.assertRaises(BrokerError):
            broker.execute(command, fingerprint)
        self.assertEqual(1, len([call for call in fake.calls if call[0] == "POST"]))
        receipt = broker.close()
        self.assertFalse(receipt["revoked"])
        self.assertEqual(1, len([call for call in fake.calls if call[0] == "POST"]))

    def test_invalid_minted_token_is_revoked_before_failure(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        original = fake.__call__

        def invalid_response(method, url, headers, data):
            response = original(method, url, headers, data)
            if method == "POST":
                payload = json.loads(response.body)
                payload["repository_selection"] = "all"
                response = HttpResult(response.status, response.headers, json.dumps(payload).encode(), response.url)
            return response

        broker._requester = invalid_response
        with self.assertRaises(BrokerError):
            broker.get_installation_token()
        self.assertEqual([call[0] for call in fake.calls][-1], "DELETE")

    def test_ambient_tokens_are_stripped_and_output_redacted(self):
        fake = FakeGitHub(token="opaque-installation-token")
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        command = ["/bin/echo", "opaque-installation-token"]
        fingerprint = fingerprint_command(broker.repository, broker.permissions, command)
        captured = {}

        def fake_child_runner(argv, environment):
            captured["argv"] = list(argv)
            captured["env"] = dict(environment)
            return 0, b"opaque-installation-token\n", b"Bearer opaque-installation-token"

        broker._child_runner = fake_child_runner
        with mock.patch.dict(os.environ, {"GH_TOKEN": "ambient-pat", "GITHUB_TOKEN": "ambient-pat"}, clear=False):
            result = broker.execute(command, fingerprint)
        env = captured["env"]
        self.assertEqual("opaque-installation-token", env["GH_TOKEN"])
        self.assertNotIn("GITHUB_TOKEN", env)
        self.assertNotIn("opaque-installation-token", result["stdout"] + result["stderr"])

    def test_fingerprint_mismatch_and_unsafe_sources(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        with self.assertRaises(BrokerError):
            broker.execute(["/bin/echo", "different"], "wrong-fingerprint")
        with tempfile.TemporaryDirectory() as temp:
            writable = Path(temp) / "run.sh"
            writable.write_text("#!/bin/sh\n")
            writable.chmod(0o664)
            with self.assertRaises(BrokerError):
                fingerprint_command("example-org/codex", {"metadata": "read"}, [str(writable)])
            link = Path(temp) / "link.sh"
            link.symlink_to(writable)
            with self.assertRaises(BrokerError):
                fingerprint_command("example-org/codex", {"metadata": "read"}, [str(link)])

    def test_body_redirect_and_host_bounds(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        with self.assertRaises(BrokerError):
            broker._coerce_response(HttpResult(200, {}, b"{}", "https://evil.example/redirect"), "https://api.github.com/app")
        with self.assertRaises(BrokerError):
            broker._coerce_response(HttpResult(200, {}, b"x" * (MAX_HTTP_BODY + 1), "https://api.github.com/app"), "https://api.github.com/app")

    def test_fixed_production_origin_and_key_basename(self):
        fake = FakeGitHub()
        directory, _ = make_broker(fake)
        self.addCleanup(directory.cleanup)
        with self.assertRaises(BrokerError):
            with mock.patch("github_app_installation_broker._production_credentials_directory", return_value=Path(directory.name)):
                GitHubAppBroker(app_id=7, app_slug="sedna-codex-delivery-coordinator", installation_id=42, account="example-org", repository="example-org/codex", permissions={"metadata": "read"}, api_base_url="https://evil.example", requester=None)
        with self.assertRaises(BrokerError):
            with mock.patch("github_app_installation_broker._production_credentials_directory", return_value=Path(directory.name)):
                GitHubAppBroker(app_id=7, app_slug="sedna-codex-delivery-coordinator", installation_id=42, account="example-org", repository="example-org/codex", permissions={"metadata": "read"}, key_basename="other.pem", requester=fake)

    def test_production_credentials_root_boundary_precedes_filesystem_validation(self):
        kwargs = dict(app_id=7, app_slug="sedna-codex-delivery-coordinator", installation_id=42, account="example-org", repository="example-org/codex", permissions={"metadata": "read"})
        for value in ("/tmp/credentials", "/run/credentials/../etc", "/run/credentials-unit"):
            with mock.patch.dict(os.environ, {"CREDENTIALS_DIRECTORY": value}, clear=False), mock.patch("github_app_installation_broker._validate_credentials_directory") as validate:
                with self.assertRaises(BrokerError):
                    GitHubAppBroker(**kwargs)
                validate.assert_not_called()

    def test_canonical_production_credentials_path_reaches_validation(self):
        kwargs = dict(app_id=7, app_slug="sedna-codex-delivery-coordinator", installation_id=42, account="example-org", repository="example-org/codex", permissions={"metadata": "read"})
        with mock.patch.dict(os.environ, {"CREDENTIALS_DIRECTORY": "/run/credentials/unit.service"}, clear=False), mock.patch("github_app_installation_broker._validate_credentials_directory") as validate:
            broker = GitHubAppBroker(**kwargs)
        validate.assert_called_once()
        self.assertEqual("/run/credentials/unit.service", str(broker.credentials_directory))
        self.assertEqual("/run/credentials/unit.service", str(_production_credentials_directory("/run/credentials/unit.service")))

    def test_private_key_mode_owner_and_fd_identity_are_checked_without_signing_secret(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        info = _validate_private_key(broker.key_path)
        self.assertEqual(0, info.st_mode & 0o077)
        self.assertEqual(os.stat(broker.key_path).st_ino, info.st_ino)
        completed = mock.Mock(returncode=0, stdout=b"signature", stderr=b"")
        broker._jwt = GitHubAppBroker._jwt.__get__(broker, GitHubAppBroker)
        with mock.patch("subprocess.run", return_value=completed) as run:
            jwt = broker._jwt()
        self.assertTrue(jwt.count(".") == 2)
        self.assertEqual("/usr/bin/openssl", run.call_args.args[0][0])
        self.assertEqual(1, len(run.call_args.kwargs["pass_fds"]))

    def test_close_is_terminal(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        broker.close()
        with self.assertRaises(BrokerError):
            broker.get_installation_token()
        with self.assertRaises(BrokerError):
            broker.execute(["/bin/echo", "x"], "wrong")

    def test_source_identity_change_after_mint_rejects_child(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        with tempfile.TemporaryDirectory() as temp:
            script = Path(temp) / "run.sh"
            script.write_text("#!/bin/sh\necho ok\n")
            script.chmod(0o755)
            command = [str(Path("/bin/sh").resolve(strict=True)), str(script)]
            fingerprint = fingerprint_command(broker.repository, broker.permissions, command)
            original_get = broker.get_installation_token

            def mint_then_mutate():
                record = original_get()
                script.write_text("#!/bin/sh\necho changed\n")
                return record

            broker.get_installation_token = mint_then_mutate
            child = mock.Mock(return_value=(0, b"", b""))
            broker._child_runner = child
            with self.assertRaises(BrokerError):
                broker.execute(command, fingerprint)
            child.assert_not_called()

    def test_source_mode_change_changes_fingerprint_without_content_change(self):
        with tempfile.TemporaryDirectory() as temp:
            script = Path(temp) / "run.sh"
            script.write_text("#!/bin/sh\necho ok\n")
            script.chmod(0o700)
            command = [str(Path("/bin/sh").resolve(strict=True)), str(script)]
            before = fingerprint_command("example-org/codex", {"metadata": "read"}, command)
            script.chmod(0o755)
            after = fingerprint_command("example-org/codex", {"metadata": "read"}, command)
            self.assertNotEqual(before, after)

    def test_revocation_success_and_failure_are_nonsecret(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        broker.get_installation_token()
        receipt = broker.close()
        self.assertEqual({"attempted": True, "revoked": True, "status": 204}, receipt)
        self.assertNotIn("opaque-installation-token", json.dumps(receipt))

        def failing(*args, **kwargs):
            raise BrokerError("authorization Bearer opaque-installation-token failed")

        fake2 = FakeGitHub()
        fake2.__call__ = failing
        directory2, broker2 = make_broker(fake2)
        self.addCleanup(directory2.cleanup)
        broker2.get_installation_token()
        broker2._requester = failing
        failure = broker2.close()
        self.assertFalse(failure["revoked"])
        self.assertNotIn("opaque-installation-token", json.dumps(failure))

    def test_no_pat_fallback(self):
        fake = FakeGitHub()
        directory, broker = make_broker(fake)
        self.addCleanup(directory.cleanup)
        with mock.patch.dict(os.environ, {"GH_TOKEN": "pat-value"}, clear=False):
            with self.assertRaises(BrokerError):
                broker.execute(["/bin/echo", "x"], "wrong")
        self.assertFalse(any("pat-value" in str(call) for call in fake.calls))

    def test_requester_failure_is_not_retried(self):
        calls = []

        def requester(*args):
            calls.append(args)
            raise TypeError("ambiguous requester failure")

        directory, broker = make_broker(requester)
        self.addCleanup(directory.cleanup)
        with self.assertRaises(BrokerError):
            broker._request_json("GET", "/app", "jwt")
        self.assertEqual(1, len(calls))

    def test_bounded_child_output_terminates(self):
        with self.assertRaises(BrokerError):
            _run_child_bounded(["/bin/sh", "-c", "yes x"], {})

    def test_cli_returns_nonzero_when_revocation_is_unproven(self):
        fake_result = {"returncode": 0, "stdout": "", "stderr": ""}

        class UnrevokedBroker:
            def __init__(self, **kwargs):
                pass

            def execute(self, argv, fingerprint):
                return dict(fake_result)

            def close(self):
                return {"attempted": True, "revoked": False}

        with mock.patch("github_app_installation_broker.GitHubAppBroker", UnrevokedBroker):
            result = main(["exec", "--app-id", "7", "--app-slug", "sedna-codex-delivery-coordinator", "--installation-id", "42", "--account", "example-org", "--repo", "example-org/codex", "--permissions", '{"metadata":"read"}', "--fingerprint", "fp", "--", "/bin/echo", "x"])
        self.assertNotEqual(0, result)


if __name__ == "__main__":
    unittest.main()
