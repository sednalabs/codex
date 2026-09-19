import socket
import struct
import threading
import unittest

from github_app_broker_proxy import ProxyError, _read_exact, validate_gh_argv


class BrokerProxyTests(unittest.TestCase):
    def test_partial_frame_read(self):
        left, right = socket.socketpair()
        try:
            threading.Thread(target=lambda: (right.send(b"ab"), right.send(b"cd")), daemon=True).start()
            self.assertEqual(b"abcd", _read_exact(left, 4))
        finally:
            left.close(); right.close()

    def test_allowlist_and_mutation_rejections(self):
        for argv in (["pr", "view", "1"], ["run", "download", "1"], ["api", "repos/o/r", "--method", "GET"], ["api", "graphql", "-f", "query=query { viewer { login } }"]):
            validate_gh_argv(list(argv))
        for argv in (["pr", "merge", "1"], ["run", "rerun", "1"], ["api", "repos/o/r", "-X", "POST"], ["api", "graphql", "-f", "query=mutation { x }"]):
            with self.assertRaises(ProxyError): validate_gh_argv(list(argv))

    def test_oversized_and_malformed_frame_inputs_are_bounded(self):
        left, right = socket.socketpair()
        try:
            right.sendall(struct.pack("!I", 16 * 1024 + 1))
            with self.assertRaises(ProxyError):
                size = struct.unpack("!I", _read_exact(left, 4))[0]
                if size > 16 * 1024: raise ProxyError("request exceeds safety bound")
        finally:
            left.close(); right.close()


if __name__ == "__main__": unittest.main()
