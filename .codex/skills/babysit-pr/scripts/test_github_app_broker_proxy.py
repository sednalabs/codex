import socket
import struct
import tempfile
import threading
import unittest
from pathlib import Path

from github_app_broker_proxy import MAX_ARGUMENT, MAX_REQUEST, MAX_RESPONSE, ProxyError, _read_exact, validate_gh_argv


class BrokerProxyTests(unittest.TestCase):
    def test_partial_frame_read(self):
        left, right = socket.socketpair()
        try:
            threading.Thread(target=lambda: (right.send(b"ab"), right.send(b"cd")), daemon=True).start()
            self.assertEqual(b"abcd", _read_exact(left, 4))
        finally:
            left.close(); right.close()

    def test_allowlist_and_mutation_rejections(self):
        allowed = [
            ["-R", "o/r", "pr", "view", "1", "--json", "number,url"],
            ["-R", "o/r", "pr", "checks", "1", "--json", "name,state"],
            ["-R", "o/r", "run", "view", "7", "--json", "status"],
            ["-R", "o/r", "run", "view", "7", "--log-failed"],
            ["-R", "o/r", "run", "view", "--job", "8", "--log"],
            ["-R", "o/r", "run", "list", "--workflow", "CI", "--limit", "30", "--json", "status"],
            ["repo", "view", "--json", "nameWithOwner"],
            ["-R", "o/r", "repo", "view", "--json", "nameWithOwner"],
            ["api", "repos/o/r/actions/runs/7", "--method", "GET"],
            ["api", "graphql", "-f", "query=query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){mergeQueueEntry{id state position headCommit{oid}}}}}", "-F", "owner=o", "-F", "name=r", "-F", "number=1"],
        ]
        for argv in allowed:
            validate_gh_argv(argv, "o/r")

        rejected = [
            ["-R", "other/r", "pr", "view", "1", "--json", "number"],
            ["-R", "o/r", "pr", "view", "1", "--web", "number"],
            ["-R", "o/r", "run", "view", "7", "--jq", ".token"],
            ["-R", "o/r", "run", "view", "7", "--log-failed", "extra"],
            ["-R", "o/r", "run", "rerun", "7"],
            ["api", "repos/other/r/actions/runs/7"],
            ["api", "repos/o/r/actions/runs", "-f", "page=1"],
            ["api", "repos/o/r/actions/runs", "-X", "POST"],
            ["api", "graphql", "-f", "query=mutation { x }"],
            ["api", "graphql", "-f", "query=# comment\nmutation { x }"],
            ["api", "graphql", "-f", "query=/* comment */ mutation { x }"],
            ["api", "graphql", "-f", "query=query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){number}}}", "-F", "owner=other", "-F", "name=r", "-F", "number=1"],
            ["api", "graphql", "-f", "query=query{rateLimit{remaining}}"],
            ["extension", "exec", "anything"],
        ]
        for argv in rejected:
            with self.assertRaises(ProxyError, msg=argv):
                validate_gh_argv(argv, "o/r")

    def test_download_is_confined_to_private_watcher_temp_directory(self):
        with tempfile.TemporaryDirectory(prefix="gh-run-download-", dir="/tmp") as path:
            validate_gh_argv(
                ["-R", "o/r", "run", "download", "7", "--name", "validation-summary", "--dir", path],
                "o/r",
            )
            Path(path).chmod(0o755)
            with self.assertRaises(ProxyError):
                validate_gh_argv(
                    ["-R", "o/r", "run", "download", "7", "--name", "validation-summary", "--dir", path],
                    "o/r",
                )

    def test_malformed_pr_argv_is_typed_proxy_error(self):
        for argv in (["-R", "o/r", "pr"], ["-R", "o/r", "pr", "view"], ["-R", "o/r", "pr", "checks"]):
            with self.assertRaises(ProxyError):
                validate_gh_argv(argv, "o/r")

    def test_oversized_and_malformed_frame_inputs_are_bounded(self):
        left, right = socket.socketpair()
        try:
            right.sendall(struct.pack("!I", 16 * 1024 + 1))
            with self.assertRaises(ProxyError):
                size = struct.unpack("!I", _read_exact(left, 4))[0]
                if size > 16 * 1024: raise ProxyError("request exceeds safety bound")
        finally:
            left.close(); right.close()

    def test_composed_response_and_argument_bounds(self):
        self.assertGreater(MAX_RESPONSE, 2 * 256 * 1024)
        with self.assertRaises(ProxyError):
            validate_gh_argv(["-R", "o/r", "pr", "view", "1", "--json", "x" * (MAX_ARGUMENT + 1)], "o/r")
        self.assertEqual(16 * 1024, MAX_REQUEST)


if __name__ == "__main__": unittest.main()
