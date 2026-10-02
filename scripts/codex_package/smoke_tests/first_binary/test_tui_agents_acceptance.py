"""Exercise the actual packaged TUI /agents view through a real Linux PTY."""

import re
from dataclasses import replace
from pathlib import Path

from app_server_harness import MockResponsesServer
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage
from package_acceptance import _mock_config
from tui_pty import PackagedTui, TerminalScreen


def _field_value_is_rendered(frame: str, label: str, value: str) -> bool:
    lines = [line.strip() for line in frame.replace("\r", "").splitlines()]
    try:
        label_index = lines.index(label)
    except ValueError:
        return False
    return (
        label_index + 1 < len(lines)
        and lines[label_index + 1] == value
        and re.fullmatch(r"[0-9a-f-]{36}", value) is not None
    )


def _thread_identity_is_rendered(frame: str, thread_id: str) -> bool:
    return _field_value_is_rendered(frame, "Thread ID:", thread_id)


def _assert_thread_identity_rendered(frame: str, thread_id: str) -> None:
    lines = [line.strip() for line in frame.replace("\r", "").splitlines()]
    assert _thread_identity_is_rendered(frame, thread_id), lines


def _open_agents(tui: PackagedTui) -> None:
    # Establish readiness, then wait for the actual `/agents` popup entry
    # before Enter so paste-burst handling cannot turn the command into text.
    tui.until("Ask Codex to do anything")
    tui.send("/agents")
    popup = tui.until("open the agent command center")
    assert "/agents" in popup, popup
    tui.send("\r")


def _isolated(package: SmokePackage, suffix: str) -> tuple[SmokePackage, Path]:
    home = package.directory / suffix
    home.mkdir()
    return replace(
        package, environment={**package.environment, "CODEX_HOME": str(home)}
    ), home


def _sdk(package: SmokePackage) -> Codex:
    return Codex(config=CodexConfig(
        codex_bin=str(package.cli), cwd=str(package.directory), env=package.environment,
    ))


def _assert_screen_identity_oracle() -> None:
    correct_id = "11111111-1111-4111-8111-111111111111"
    stale_id = "22222222-2222-4222-8222-222222222222"
    wrong_id = "33333333-3333-4333-8333-333333333333"
    screen = TerminalScreen(rows=34, columns=110)

    # Treat the initial chunks as output drained before the input under test.
    for chunk in (
        b"\x1b[1;",
        b"21r\x1b[1;1HThread ID:",
        b"\x1b[2;1H" + stale_id.encode() + b" ",
        b"\x1b[3;1H",
        b"\xe2",
        b"\x80\xba",
        b"\x1b[1;",
        b"0r\x1b[r",
    ):
        screen.feed(chunk)
    before_input = screen.text()
    assert _thread_identity_is_rendered(before_input, stale_id)

    # A fragmented cursor/erase repaint replaces the stale UUID rather than
    # accepting an old label/value or a substring from the raw byte stream.
    for chunk in (
        b"\x1b[2;",
        b"1H\x1b[2",
        b"K" + correct_id.encode()[:9],
        correct_id.encode()[9:],
    ):
        screen.feed(chunk)
    after_input = screen.text()
    assert after_input != before_input
    assert _thread_identity_is_rendered(after_input, correct_id)
    assert not _thread_identity_is_rendered(after_input, wrong_id)
    assert not _thread_identity_is_rendered(after_input, stale_id)

    separated = TerminalScreen(rows=34, columns=110)
    separated.feed(
        b"\x1b[1;1HThread ID:\x1b[3;1H" + stale_id.encode() + b" "
    )
    assert not _thread_identity_is_rendered(separated.text(), stale_id)

    # T1's emitted reverse-index scrolls only within the configured region.
    region = TerminalScreen(rows=4, columns=20)
    region.feed(b"\x1b[2;3r\x1b[2;1Htop\x1bM")
    assert region.text().splitlines()[1] == ""
    assert region.text().splitlines()[2] == "top"


def test_actual_tui_agents_entry_has_initial_empty_search(package: SmokePackage) -> None:
    _assert_screen_identity_oracle()
    isolated, home = _isolated(package, "tui-empty-search")
    with MockResponsesServer() as server:
        _mock_config(home, server, agent_tools=True)
        with PackagedTui(isolated) as tui:
            _open_agents(tui)
            opened = tui.until("Agent command center")
            assert "Group:" in opened
            tui.send("f")
            search = tui.until_screen(
                "Search ›", required_markers=("Agent command center",)
            )
            assert "Search ›" in search and "Agent command center" in search


def test_actual_tui_nested_filter_clear_live_rename_and_replay(
    package: SmokePackage,
) -> None:
    isolated, home = _isolated(package, "tui-tree-replay")
    with MockResponsesServer() as server:
        _mock_config(home, server, agent_tools=True)
        for index, message in enumerate(("root done", "child done", "nested done")):
            server.enqueue_assistant_message(message, response_id=f"tui-seed-{index}")
        with _sdk(isolated) as client:
            root = client.thread_start(
                ephemeral=False, approval_mode=ApprovalMode.deny_all,
                sandbox=Sandbox.workspace_write,
            )
            root.set_name("root-package-task")
            assert root.run("Complete root seed.").final_response == "root done"
            child = client.thread_fork(
                root.id, ephemeral=False, include_turns=False,
            )
            child.set_name("child-package-task")
            assert child.run("Complete child seed.").final_response == "child done"
            nested = client.thread_fork(child.id, ephemeral=False)
            nested.set_name("nested-package-task")
            assert nested.run("Complete nested seed.").final_response == "nested done"
            root_id, child_id, nested_id = root.id, child.id, nested.id

        # A second packaged process must reopen the exact child identity.
        with _sdk(isolated) as client:
            assert client.thread_resume(child_id).id == child_id
            assert client.thread_resume(root_id).id == root_id

        with PackagedTui(isolated, "resume", root_id) as tui:
            _open_agents(tui)
            tree = tui.until("nested-package-task")
            assert "Agent command center" in tree
            assert "root-package-task" in tree and "child-package-task" in tree
            tui.send("f")
            assert "Search ›" in tui.until_screen(
                "Search ›", required_markers=("Agent command center",)
            )
            tui.send("child-package-task")
            # The helper applies actual incremental TUI cell updates, so the
            # label row must remain adjacent to this exact child ID.
            child_detail = tui.until_screen(
                child_id,
                value_label="Thread ID:",
            )
            _assert_thread_identity_rendered(child_detail, child_id)
            assert not _thread_identity_is_rendered(
                child_detail, "00000000-0000-0000-0000-000000000000"
            ), child_detail
            # `thread_fork` records an independent session, not a V2 subagent
            # source; its detail view must not be assigned a synthetic parent.
            tui.send("\x03")
            assert "nested-package-task" in tui.until("nested-package-task")
            tui.send("f")
            assert "Search ›" in tui.until_screen(
                "Search ›", required_markers=("Agent command center",)
            )
            tui.send("nested-package-task")
            nested_detail = tui.until_screen(
                nested_id,
                value_label="Thread ID:",
            )
            _assert_thread_identity_rendered(nested_detail, nested_id)
            assert not _thread_identity_is_rendered(nested_detail, child_id), nested_detail
            assert not _thread_identity_is_rendered(
                nested_detail, "00000000-0000-0000-0000-000000000000"
            ), nested_detail
            # This session was likewise created with `thread_fork`; the
            # separate V2 subagent acceptance verifies parent lineage.
            tui.send("\x03")
            assert "root-package-task" in tui.until("root-package-task")
            tui.send("f")
            assert "Search ›" in tui.until_screen(
                "Search ›", required_markers=("Agent command center",)
            )
            tui.send("no-such-synthetic-task")
            assert "No matching tasks" in tui.until("No matching tasks")
            tui.send("\x03")
            assert "nested-package-task" in tui.until("nested-package-task")
            tui.send("r")
            assert "Rename ›" in tui.until("Rename ›")
            tui.send("\x7f" * 80 + "live-renamed-package-task")
            tui.send("\r")
            assert "live-renamed-package-task" in tui.until("live-renamed-package-task")

        with PackagedTui(isolated, "resume", root_id) as tui:
            _open_agents(tui)
            replay = tui.until("live-renamed-package-task")
            assert "Agent command center" in replay
            assert "root-package-task" in replay and "child-package-task" in replay
