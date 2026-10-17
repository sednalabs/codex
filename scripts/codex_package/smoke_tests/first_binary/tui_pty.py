"""Small real-PTY driver for the downloaded Codex TUI on hosted Linux."""

import fcntl
import os
import pty
import re
import select
import signal
import struct
import subprocess
import termios
import time
from collections.abc import Iterator
from contextlib import contextmanager

from fixtures import SmokePackage


ANSI = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)|.)")


def plain(raw: bytes) -> str:
    return ANSI.sub(b"", raw).decode("utf-8", errors="replace")


@contextmanager
def shared_agent_server(package: SmokePackage) -> Iterator[None]:
    """Run /agents against the packaged CLI's daemon in this package home."""
    command = [str(package.cli), "app-server", "daemon"]

    def run_daemon(action: str) -> None:
        try:
            result = subprocess.run(
                [*command, action],
                cwd=package.directory,
                env=package.environment,
                capture_output=True,
                text=True,
                check=False,
                timeout=30,
            )
        except subprocess.TimeoutExpired as error:
            raise AssertionError(
                f"packaged app-server daemon {action} timed out: "
                f"stdout={error.stdout!r}; stderr={error.stderr!r}"
            ) from error
        if result.returncode != 0:
            raise AssertionError(
                f"packaged app-server daemon {action} failed: "
                f"stdout={result.stdout!r}; stderr={result.stderr!r}"
            )

    try:
        run_daemon("start")
    except BaseException as primary_error:
        try:
            run_daemon("stop")
        except BaseException as cleanup_error:
            primary_error.add_note(f"daemon cleanup also failed: {cleanup_error}")
        raise
    try:
        yield
    except BaseException as primary_error:
        try:
            run_daemon("stop")
        except BaseException as cleanup_error:
            primary_error.add_note(f"daemon cleanup also failed: {cleanup_error}")
        raise
    else:
        run_daemon("stop")


class PackagedTui:
    def __init__(self, package: SmokePackage, *arguments: str) -> None:
        self.package = package
        self.arguments = arguments
        self.master: int | None = None
        self.process: subprocess.Popen[bytes] | None = None
        self._last_wait_started_at: float | None = None
        self._last_wait_received_bytes = 0

    def __enter__(self) -> "PackagedTui":
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 110, 0, 0))
        env = {
            **self.package.environment,
            "TERM": "xterm-256color",
            "COLUMNS": "110",
            "LINES": "34",
        }
        try:
            self.process = subprocess.Popen(
                [str(self.package.cli), "--no-alt-screen", *self.arguments],
                cwd=self.package.directory,
                env=env,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                start_new_session=True,
                close_fds=True,
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
        # Drop unread bytes from the previous frame so each assertion is
        # caused by this input rather than by an old redraw in the PTY queue.
        while select.select([self.master], [], [], 0)[0]:
            try:
                if not os.read(self.master, 65536):
                    break
            except OSError:
                break
        os.write(self.master, value.encode("utf-8"))

    def resize(self, columns: int) -> None:
        """Resize the owned PTY to force a fresh TUI viewport render."""
        assert self.master is not None and self.process is not None
        self.send("")
        fcntl.ioctl(
            self.master,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", 34, columns, 0, 0),
        )
        self.process.send_signal(signal.SIGWINCH)

    def _failure_context(
        self,
        *,
        stage: str,
        action: str,
        started_at: float,
        marker_presence: tuple[bool, ...],
    ) -> str:
        process_status = "unavailable"
        exit_code = "unavailable"
        if self.process is not None:
            exit_code = self.process.poll()
            process_status = "alive" if exit_code is None else "exited"
            exit_code = "none" if exit_code is None else str(exit_code)
        presence = ",".join(
            "true" if present else "false" for present in marker_presence
        )
        return (
            f"stage={stage}; action={action}; process={process_status}; "
            f"exit_code={exit_code}; elapsed_ms="
            f"{max(0, int((time.monotonic() - started_at) * 1000))}; "
            f"received_pty_bytes={self._last_wait_received_bytes}; "
            f"marker_presence=[{presence}]"
        )

    def require_condition(
        self,
        condition: bool,
        *,
        stage: str,
        action: str,
        marker_presence: tuple[bool, ...],
        started_at: float | None = None,
    ) -> None:
        """Fail with value-free PTY state rather than fixture or terminal text."""
        if condition:
            return
        started_at = started_at or self._last_wait_started_at or time.monotonic()
        raise AssertionError(
            self._failure_context(
                stage=stage,
                action=action,
                started_at=started_at,
                marker_presence=marker_presence,
            )
        )

    def assert_markers(
        self,
        received: str,
        markers: tuple[str, ...],
        stage: str,
        action: str,
    ) -> None:
        self.assert_marker_states(
            received,
            markers,
            expected_presence=(True,) * len(markers),
            stage=stage,
            action=action,
        )

    def assert_marker_states(
        self,
        received: str,
        markers: tuple[str, ...],
        *,
        expected_presence: tuple[bool, ...],
        stage: str,
        action: str,
    ) -> None:
        presence = tuple(marker in received for marker in markers)
        self.require_condition(
            presence == expected_presence,
            stage=stage,
            action=action,
            marker_presence=presence,
        )

    def until_output(
        self,
        *,
        timeout: float = 30,
        stage: str = "unspecified",
        action: str = "wait-for-redraw",
        _received: bytearray | None = None,
        _markers: tuple[str, ...] = (),
        _started_at: float | None = None,
    ) -> bytes:
        """Wait for a fresh PTY redraw after input that has no stable text marker."""
        assert self.master is not None and self.process is not None
        started_at = _started_at or time.monotonic()
        if _started_at is None:
            self._last_wait_received_bytes = 0
        self._last_wait_started_at = started_at
        deadline = started_at + timeout
        received = _received if _received is not None else bytearray()
        start_length = len(received)
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                for _ in range(64):
                    if not select.select([self.master], [], [], 0)[0]:
                        break
                    try:
                        chunk = os.read(self.master, 65536)
                    except OSError:
                        break
                    if not chunk:
                        break
                    received.extend(chunk)
                    self._last_wait_received_bytes += len(chunk)
                raise AssertionError(
                    self._failure_context(
                        stage=stage,
                        action=action,
                        started_at=started_at,
                        marker_presence=tuple(
                            marker in plain(received) for marker in _markers
                        ),
                    )
                )
            ready, _, _ = select.select(
                [self.master], [], [], max(0, deadline - time.monotonic())
            )
            if not ready:
                break
            try:
                chunk = os.read(self.master, 65536)
            except OSError as error:
                raise AssertionError(
                    self._failure_context(
                        stage=stage,
                        action=action,
                        started_at=started_at,
                        marker_presence=tuple(
                            marker in plain(received) for marker in _markers
                        ),
                    )
                ) from error
            if not chunk:
                break
            received.extend(chunk)
            self._last_wait_received_bytes += len(chunk)
            return bytes(received[start_length:])
        raise AssertionError(
            self._failure_context(
                stage=stage,
                action=action,
                started_at=started_at,
                marker_presence=tuple(
                    marker in plain(received) for marker in _markers
                ),
            )
        )

    def until_all(
        self,
        markers: tuple[str, ...],
        *,
        initial: str = "",
        timeout: float = 30,
        stage: str = "unspecified",
        action: str = "wait-for-markers",
    ) -> str:
        """Wait for all rendered markers, retaining an already captured first frame."""
        received = bytearray(initial.encode("utf-8"))
        started_at = time.monotonic()
        self._last_wait_received_bytes = 0
        self._last_wait_started_at = started_at
        deadline = started_at + timeout
        while not all(marker in plain(received) for marker in markers):
            if deadline - time.monotonic() <= 0:
                raise AssertionError(
                    self._failure_context(
                        stage=stage,
                        action=action,
                        started_at=started_at,
                        marker_presence=tuple(
                            marker in plain(received) for marker in markers
                        ),
                    )
                )
            self.until_output(
                timeout=timeout,
                stage=stage,
                action=action,
                _received=received,
                _markers=markers,
                _started_at=started_at,
            )
        return plain(received)

    def send_paste(self, value: str) -> None:
        """Send one explicit bracketed-paste event to the TUI."""
        self.send(f"\x1b[200~{value}\x1b[201~")

    def submit_line(
        self,
        value: str,
        *,
        pasted_marker: str,
        stage: str = "unspecified",
        action: str = "paste-before-submit",
    ) -> str:
        """Paste a line, witness it in the composer, and submit with a key event."""
        self.send_paste(value)
        rendered = self.until(pasted_marker, stage=stage, action=action)
        self.send("\r")
        return rendered

    def until(
        self,
        marker: str,
        *,
        timeout: float = 30,
        stage: str = "unspecified",
        action: str = "wait-for-marker",
    ) -> str:
        """Require a fresh rendered witness after the preceding input."""
        assert self.master is not None and self.process is not None
        started_at = time.monotonic()
        self._last_wait_received_bytes = 0
        self._last_wait_started_at = started_at
        deadline = started_at + timeout
        received = bytearray()
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                # The child can exit before select sees bytes already queued by
                # the PTY. Drain a bounded amount so early failures can report
                # received-byte and marker-presence evidence.
                for _ in range(64):
                    if not select.select([self.master], [], [], 0)[0]:
                        break
                    try:
                        chunk = os.read(self.master, 65536)
                    except OSError:
                        break
                    if not chunk:
                        break
                    received.extend(chunk)
                    self._last_wait_received_bytes += len(chunk)
                raise AssertionError(
                    self._failure_context(
                        stage=stage,
                        action=action,
                        started_at=started_at,
                        marker_presence=(marker in plain(received),),
                    )
                )
            self.until_output(
                timeout=timeout,
                stage=stage,
                action=action,
                _received=received,
                _markers=(marker,),
                _started_at=started_at,
            )
            rendered = plain(received)
            if marker in rendered:
                return rendered
        raise AssertionError(
            self._failure_context(
                stage=stage,
                action=action,
                started_at=started_at,
                marker_presence=(marker in plain(received),),
            )
        )
