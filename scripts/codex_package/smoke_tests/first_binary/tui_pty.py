"""Small real-PTY driver for the downloaded Codex TUI on hosted Linux."""

import codecs
import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import termios
import time
import unicodedata

from fixtures import SmokePackage


ANSI = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)|.)")


def plain(raw: bytes) -> str:
    return ANSI.sub(b"", raw).decode("utf-8", errors="replace")


class TerminalScreen:
    """Fixed-size emitted TUI subset: cursor positions, erase, style/modes, and OSC.

    Any other escape/control sequence is a diagnostic failure, not ignored screen state.
    """

    def __init__(self, rows: int, columns: int) -> None:
        self.rows = rows
        self.columns = columns
        self.cells = [[" "] * columns for _ in range(rows)]
        self.row = self.column = 0
        self.top_margin = 0
        self.bottom_margin = rows - 1
        self._decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        self._state = "text"
        self._csi = ""
        self.unsupported: list[str] = []

    def feed(self, data: bytes) -> None:
        for char in self._decoder.decode(data):
            if self._state == "osc":
                if char == "\x07":
                    self._state = "text"
                elif char == "\x1b":
                    self._state = "osc-escape"
                continue
            if self._state == "osc-escape":
                self._state = "text" if char == "\\" else "osc"
                continue
            if self._state == "csi":
                if "@" <= char <= "~":
                    self._apply_csi(char, self._csi)
                    self._csi = ""
                    self._state = "text"
                else:
                    self._csi += char
                continue
            if self._state == "escape":
                if char == "[":
                    self._state = "csi"
                elif char == "]":
                    self._state = "osc"
                elif char == "M":
                    if self.row == self.top_margin:
                        self._scroll_down()
                    else:
                        self.row = max(0, self.row - 1)
                    self._state = "text"
                else:
                    self.unsupported.append(f"ESC {char!r}")
                    self._state = "text"
                continue
            if char == "\x1b":
                self._state = "escape"
            elif char == "\r":
                self.column = 0
            elif char == "\n":
                self._line_feed()
            elif char == "\b":
                self.column = max(0, self.column - 1)
            elif char == "\t":
                self.column = min(self.columns - 1, (self.column // 8 + 1) * 8)
            elif char == "\x07":
                continue
            elif ord(char) < 32:
                self.unsupported.append(f"control {ord(char):#04x}")
            elif char >= " ":
                self._put(char)

    def text(self) -> str:
        if self.unsupported:
            raise AssertionError(
                "terminal screen observer encountered unsupported controls: "
                f"{self.unsupported[-20:]!r}"
            )
        return "\n".join("".join(line).rstrip() for line in self.cells)

    def _line_feed(self) -> None:
        if self.top_margin <= self.row <= self.bottom_margin:
            if self.row == self.bottom_margin:
                self._scroll_up()
            else:
                self.row += 1
        else:
            self.row = min(self.rows - 1, self.row + 1)

    def _scroll_up(self) -> None:
        self.cells.pop(self.top_margin)
        self.cells.insert(self.bottom_margin, [" "] * self.columns)

    def _scroll_down(self) -> None:
        self.cells.pop(self.bottom_margin)
        self.cells.insert(self.top_margin, [" "] * self.columns)

    def _put(self, char: str) -> None:
        if self.column >= self.columns:
            self.column = 0
            self._line_feed()
        width = 2 if unicodedata.east_asian_width(char) in {"W", "F"} else 1
        self.cells[self.row][self.column] = char
        if width == 2 and self.column + 1 < self.columns:
            self.cells[self.row][self.column + 1] = " "
        self.column += width

    def _apply_csi(self, command: str, raw_parameters: str) -> None:
        parameters = raw_parameters.lstrip("?=>!")
        values = [int(part) if part.isdigit() else 0 for part in parameters.split(";")]

        def value(index: int, default: int = 1) -> int:
            return values[index] if index < len(values) and values[index] else default

        if command in {"H", "f"}:
            self.row = min(self.rows - 1, value(0) - 1)
            self.column = min(self.columns - 1, value(1) - 1)
        elif command == "r":
            parts = raw_parameters.split(";") if raw_parameters else []
            if len(parts) > 2 or any(part and not part.isdigit() for part in parts):
                self.unsupported.append(f"CSI {raw_parameters!r}{command}")
                return
            top = int(parts[0]) if parts and parts[0] else 1
            bottom = int(parts[1]) if len(parts) > 1 and parts[1] else self.rows
            top = top or 1
            bottom = bottom or self.rows
            if not 1 <= top < bottom <= self.rows:
                self.unsupported.append(f"CSI {raw_parameters!r}{command}")
                return
            self.top_margin = top - 1
            self.bottom_margin = bottom - 1
            self.row = self.column = 0
        elif command == "J":
            self._erase_display(values[0] if values else 0)
        elif command == "K":
            self._erase_line(values[0] if values else 0)
        elif command in {"m", "n", "q"}:
            # Ratatui styling, cursor shape, and status queries do not change cells.
            return
        elif command == "u" and raw_parameters in {"?", "<", ">5", ">7"}:
            # Query, restore, and enable terminal keyboard-reporting modes only.
            return
        elif command == "c" and raw_parameters in {"", "0"}:
            # T1's DA1 startup probe has no effect on the cell screen.
            return
        elif command in {"h", "l"} and raw_parameters.startswith("?") and set(values) <= {
            25, 1000, 1002, 1004, 1006, 1007, 2004, 2026,
        }:
            # Cursor visibility, mouse/focus, alternate-scroll, paste, sync modes only.
            return
        else:
            self.unsupported.append(f"CSI {raw_parameters!r}{command}")

    def _erase_display(self, mode: int) -> None:
        if mode in {2, 3}:
            self.cells = [[" "] * self.columns for _ in range(self.rows)]
        elif mode == 0:
            self.cells[self.row][self.column:] = [" "] * (self.columns - self.column)
            for row in range(self.row + 1, self.rows):
                self.cells[row] = [" "] * self.columns
        elif mode == 1:
            for row in range(self.row):
                self.cells[row] = [" "] * self.columns
            self.cells[self.row][: self.column + 1] = [" "] * (self.column + 1)

    def _erase_line(self, mode: int) -> None:
        if mode == 2:
            self.cells[self.row] = [" "] * self.columns
        elif mode == 0:
            self.cells[self.row][self.column:] = [" "] * (self.columns - self.column)
        elif mode == 1:
            self.cells[self.row][: self.column + 1] = [" "] * (self.column + 1)


class PackagedTui:
    def __init__(
        self,
        package: SmokePackage,
        *arguments: str,
        columns: int = 110,
    ) -> None:
        self.package = package
        self.arguments = arguments
        self.rows = 34
        self.columns = columns
        self.master: int | None = None
        self.process: subprocess.Popen[bytes] | None = None
        self.last_input: str | None = None
        self.last_received = b""
        self.last_frame = ""
        self.interactions: list[dict[str, object]] = []
        self.screen = TerminalScreen(rows=self.rows, columns=self.columns)
        self.last_input_screen = self.screen.text()

    def __enter__(self) -> "PackagedTui":
        master, slave = pty.openpty()
        fcntl.ioctl(
            slave,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", self.rows, self.columns, 0, 0),
        )
        env = {
            **self.package.environment,
            "TERM": "xterm-256color",
            "COLUMNS": str(self.columns),
            "LINES": str(self.rows),
        }
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
        self.interactions.append({
            "input": value,
            "before_exit_status": self.process.poll() if self.process else None,
        })
        drained = bytearray()
        # Drop unread bytes from the previous frame so each assertion is
        # caused by this input rather than by an old redraw in the PTY queue.
        while select.select([self.master], [], [], 0)[0]:
            try:
                chunk = os.read(self.master, 65536)
                if not chunk:
                    break
                drained.extend(chunk)
                self.screen.feed(chunk)
            except OSError as error:
                raise self._closed_output(
                    f"input {value!r}", bytes(drained), f"PTY {error} while preparing input"
                ) from error
        self.last_received = b""
        try:
            self.last_input_screen = self.screen.text()
        except AssertionError as error:
            raise self._closed_output(
                f"input {value!r}", bytes(drained), f"unsupported terminal output: {error}"
            ) from error
        try:
            os.write(self.master, value.encode("utf-8"))
        except OSError as error:
            raise self._closed_output(
                f"input {value!r}", bytes(drained), f"PTY {error} while writing input"
            ) from error

    def _closed_output(self, marker: str, received: bytes, reason: str) -> AssertionError:
        assert self.process is not None
        self.last_received = received
        return AssertionError(
            f"packaged TUI {reason} before {marker!r}; "
            f"exit_status={self.process.poll()!r}; last_input={self.last_input!r}; "
            f"buffered_output={plain(received)!r}; previous_frame={self.last_frame!r}; "
            f"screen={self._screen_snapshot()!r}; interactions={self.interactions[-20:]!r}"
        )

    def _screen_snapshot(self) -> str:
        try:
            return self.screen.text()
        except AssertionError as error:
            return f"<unavailable: {error}>"

    def until(self, marker: str, *, timeout: float = 30) -> str:
        """Require a fresh rendered witness after the preceding input."""
        assert self.master is not None and self.process is not None
        self.interactions.append({
            "await_marker": marker,
            "after_input": self.last_input,
            "before_exit_status": self.process.poll(),
        })
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
            self.screen.feed(chunk)
            try:
                self.screen.text()
            except AssertionError as error:
                raise self._closed_output(
                    marker, bytes(received), f"unsupported terminal output: {error}"
                ) from error
            rendered = plain(received)
            if self.interactions:
                self.interactions[-1]["last_rendered_bytes"] = len(received)
            if marker in rendered:
                self.last_frame = rendered
                self.interactions.append({"witness": marker, "exit_status": self.process.poll()})
                return rendered
        self.last_received = bytes(received)
        self.interactions[-1]["timeout_exit_status"] = self.process.poll()
        self.interactions[-1]["buffered_output"] = plain(received)
        current_screen = self._screen_snapshot()
        screen_changed = current_screen != self.last_input_screen
        self.interactions[-1]["timeout_screen"] = current_screen
        self.interactions[-1]["screen_changed_since_input"] = screen_changed
        raise AssertionError(
            f"packaged TUI did not render {marker!r}; exit_status={self.process.poll()!r}; "
            f"last_input={self.last_input!r}; buffered_output={plain(received)!r}; "
            f"screen_changed_since_input={screen_changed!r}; current_screen={current_screen!r}; "
            f"previous_frame={self.last_frame!r}; interactions={self.interactions[-20:]!r}"
        )

    def until_screen(
        self,
        marker: str,
        *,
        value_label: str | None = None,
        required_markers: tuple[str, ...] = (),
        timeout: float = 30,
    ) -> str:
        """Require a changed current screen containing marker, not historical bytes."""
        assert self.master is not None and self.process is not None
        self.interactions.append({
            "await_screen_marker": marker,
            "after_input": self.last_input,
            "before_exit_status": self.process.poll(),
        })
        deadline = time.monotonic() + timeout
        received = bytearray()

        def matches(rendered: str) -> bool:
            # The details pane is preceded by its visible `│` gutter. Treat
            # that pane chrome as framing while still requiring the exact
            # label and UUID on neighboring rendered rows.
            lines = [line.strip().strip("│").strip() for line in rendered.splitlines()]
            adjacent_value = value_label is None or any(
                lines[index] == value_label and lines[index + 1] == marker
                for index in range(len(lines) - 1)
            )
            return (
                rendered != self.last_input_screen
                and marker in rendered
                and all(required in rendered for required in required_markers)
                and adjacent_value
            )

        # PTYs need not redraw an unchanged screen after input. Evaluate the
        # maintained frame first; raw-byte freshness is not screen-state
        # freshness. `send()` snapshots the screen after draining pre-input
        # bytes, so this still requires a post-input transition.
        try:
            rendered = self.screen.text()
        except AssertionError as error:
            raise self._closed_output(
                marker, bytes(received), f"unsupported terminal output: {error}"
            ) from error
        if self.process.poll() is not None:
            raise self._closed_output(marker, bytes(received), "exited before screen witness")
        if matches(rendered):
            self.last_frame = rendered
            self.interactions.append({
                "screen_witness": marker,
                "source": "maintained-screen",
                "exit_status": self.process.poll(),
            })
            return rendered

        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise self._closed_output(marker, bytes(received), "exited before screen witness")
            ready, _, _ = select.select([self.master], [], [], max(0, deadline - time.monotonic()))
            if not ready:
                break
            try:
                chunk = os.read(self.master, 65536)
            except OSError as error:
                raise self._closed_output(
                    marker, bytes(received), f"PTY {error} before screen witness"
                ) from error
            if not chunk:
                raise self._closed_output(
                    marker, bytes(received), "PTY reached EOF before screen witness"
                )
            received.extend(chunk)
            self.last_received = bytes(received)
            self.screen.feed(chunk)
            try:
                rendered = self.screen.text()
            except AssertionError as error:
                raise self._closed_output(
                    marker, bytes(received), f"unsupported terminal output: {error}"
                ) from error
            if matches(rendered):
                self.last_frame = rendered
                self.interactions.append({
                    "screen_witness": marker,
                    "source": "pty-update",
                    "exit_status": self.process.poll(),
                })
                return rendered
        self.last_received = bytes(received)
        rendered = self._screen_snapshot()
        self.interactions[-1]["timeout_exit_status"] = self.process.poll()
        self.interactions[-1]["buffered_output"] = plain(received)
        self.interactions[-1]["screen"] = rendered
        self.interactions[-1]["changed_since_input"] = rendered != self.last_input_screen
        self.interactions[-1]["required_markers"] = required_markers
        self.interactions[-1]["value_label"] = value_label
        raise AssertionError(
            f"packaged TUI screen did not change to contain {marker!r}; "
            f"exit_status={self.process.poll()!r}; last_input={self.last_input!r}; "
            f"buffered_output={plain(received)!r}; screen={rendered!r}; "
            f"interactions={self.interactions[-20:]!r}"
        )
