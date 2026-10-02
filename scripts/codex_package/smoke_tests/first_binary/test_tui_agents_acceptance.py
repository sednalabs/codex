"""Exercise the actual packaged TUI /agents view through a real Linux PTY."""

import re
from dataclasses import replace
from pathlib import Path

from app_server_harness import MockResponsesServer
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage
from package_acceptance import _mock_config
from tui_pty import PackagedTui


def _assert_thread_identity_rendered(frame: str, thread_id: str) -> None:
    lines = [line.strip() for line in frame.replace("\r", "").splitlines() if line.strip()]
    label_index = lines.index("Thread ID:")
    assert lines[label_index + 1] == thread_id, lines
    assert re.fullmatch(r"[0-9a-f-]{36}", lines[label_index + 1])


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


def test_actual_tui_agents_entry_has_initial_empty_search(package: SmokePackage) -> None:
    isolated, home = _isolated(package, "tui-empty-search")
    with MockResponsesServer() as server:
        _mock_config(home, server, agent_tools=True)
        with PackagedTui(isolated) as tui:
            _open_agents(tui)
            opened = tui.until("Agent command center")
            assert "Group:" in opened
            tui.send("f")
            search = tui.until("Search ›")
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
            assert "Search ›" in tui.until("Search ›")
            tui.send("child-package-task")
            tui.until("Thread ID:")
            tui.refresh()
            child_detail = tui.until(child_id)
            _assert_thread_identity_rendered(child_detail, child_id)
            assert f"Parent thread ID: {root_id}" in child_detail
            tui.send("\x03")
            assert "nested-package-task" in tui.until("nested-package-task")
            tui.send("f")
            assert "Search ›" in tui.until("Search ›")
            tui.send("nested-package-task")
            tui.until("Thread ID:")
            tui.refresh()
            nested_detail = tui.until(nested_id)
            _assert_thread_identity_rendered(nested_detail, nested_id)
            assert f"Parent thread ID: {child_id}" in nested_detail
            tui.send("\x03")
            assert "root-package-task" in tui.until("root-package-task")
            tui.send("f")
            assert "Search ›" in tui.until("Search ›")
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
