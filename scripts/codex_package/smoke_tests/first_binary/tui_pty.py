"""Small real-PTY driver for the downloaded Codex TUI on hosted Linux."""

import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import termios
import time

from fixtures import SmokePackage


ANSI = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)|.)")


def plain(raw: bytes) -> str:
    return ANSI.sub(b"", raw).decode("utf-8", errors="replace")


class PackagedTui:
    def __init__(self, package: SmokePackage, *arguments: str) -> None:
        self.package = package
        self.arguments = arguments
        self.master: int | None = None
        self.process: subprocess.Popen[bytes] | None = None
        self.last_input: str | None = None
        self.last_received = b""
        self.last_frame = ""

    def __enter__(self) -> "PackagedTui":
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 110, 0, 0))
        env = {**self.package.environment, "TERM": "xterm-256color", "COLUMNS": "110", "LINES": "34"}
        try:
            self.process = subprocess.Popen(
                [str(self.package.cli), "--no-alt-screen", *self.arguments],
                cwd=self.package.directory, env=env,
                stdin=slave, stdout=slave, stderr=slave,
                start_new_session=True, close_fds=True,
            )
        finally:
            os.close(slave)
        self.master = master
        return self

    def __exit__(self, _kind: object, _error: object, _traceback: object) -> None:
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        if self.master is not None:
            os.close(self.master)

    def send(self, value: str) -> None:
        assert self.master is not None
        self.last_input = value
        # Drop unread bytes from the previous frame so each assertion is
        # caused by this input rather than by an old redraw in the PTY queue.
        while select.select([self.master], [], [], 0)[0]:
            try:
                if not os.read(self.master, 65536):
                    break
            except OSError as error:
                raise self._closed_output(
                    f"input {value!r}", self.last_received, f"PTY {error} while preparing input"
                ) from error
        self.last_received = b""
        try:
            os.write(self.master, value.encode("utf-8"))
        except OSError as error:
            raise self._closed_output(
                f"input {value!r}", self.last_received, f"PTY {error} while writing input"
            ) from error

    def _closed_output(self, marker: str, received: bytes, reason: str) -> AssertionError:
        assert self.process is not None
        self.last_received = received
        return AssertionError(
            f"packaged TUI {reason} before {marker!r}; "
            f"exit_status={self.process.poll()!r}; last_input={self.last_input!r}; "
            f"buffered_output={plain(received)!r}; previous_frame={self.last_frame!r}"
        )

    def until(self, marker: str, *, timeout: float = 30) -> str:
        """Require a fresh rendered witness after the preceding input."""
        assert self.master is not None and self.process is not None
        deadline = time.monotonic() + timeout
        received = bytearray()
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise self._closed_output(marker, bytes(received), "exited")
            ready, _, _ = select.select([self.master], [], [], max(0, deadline - time.monotonic()))
            if not ready:
                break
            try:
                chunk = os.read(self.master, 65536)
            except OSError as error:
                raise self._closed_output(marker, bytes(received), f"PTY {error}") from error
            if not chunk:
                raise self._closed_output(marker, bytes(received), "PTY reached EOF")
            received.extend(chunk)
            self.last_received = bytes(received)
            rendered = plain(received)
            if marker in rendered:
                self.last_frame = rendered
                return rendered
        self.last_received = bytes(received)
        raise AssertionError(
            f"packaged TUI did not render {marker!r}; exit_status={self.process.poll()!r}; "
            f"last_input={self.last_input!r}; buffered_output={plain(received)!r}; "
            f"previous_frame={self.last_frame!r}"
        )
