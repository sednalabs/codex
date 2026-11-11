"""Exercise the retained packaged TUI /agents view through a real Linux PTY."""

from dataclasses import replace
from pathlib import Path

from app_server_harness import MockResponsesServer
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage
from tui_pty import PackagedTui, shared_agent_server


def _mock_config(
    home: Path, server: MockResponsesServer, *, agent_tools: bool = False
) -> None:
    home.mkdir(parents=True, exist_ok=True)
    (home / "config.toml").write_text(
        'model = "package-smoke"\nmodel_provider = "package_smoke"\n'
        'approval_policy = "never"\nsandbox_mode = "workspace-write"\n'
        "[features]\n"
        f"code_mode_only = {str(not agent_tools).lower()}\n"
        "code_mode_host = true\n"
        "multi_agent_v2 = true\nmemories = false\napps = false\nplugins = false\n"
        '[model_providers.package_smoke]\nname = "package smoke"\n'
        f'base_url = "{server.url}/v1"\nwire_api = "responses"\n'
        "request_max_retries = 0\nstream_max_retries = 0\n"
        '[tui]\nstatus_line = ["model-name", "thread-id", "thread-name"]\n',
        encoding="utf-8",
    )


def _isolated(package: SmokePackage, suffix: str) -> tuple[SmokePackage, Path]:
    home = package.directory / suffix
    home.mkdir()
    return replace(
        package, environment={**package.environment, "CODEX_HOME": str(home)}
    ), home


def _sdk(package: SmokePackage) -> Codex:
    return Codex(
        config=CodexConfig(
            codex_bin=str(package.cli),
            cwd=str(package.directory),
            env=package.environment,
        )
    )


def test_actual_tui_agents_entry_has_initial_empty_search(
    package: SmokePackage,
) -> None:
    isolated, home = _isolated(package, "tui-empty-search")
    with MockResponsesServer() as server:
        _mock_config(home, server, agent_tools=True)
        with shared_agent_server(isolated):
            with PackagedTui(isolated) as tui:
                # StartupDraft already renders the banner and composer. The
                # welcome tail is inserted by the actual SessionInfoCell.
                tui.until_all(
                    (
                        "/review - review any changes and find issues",
                        "package-smoke",
                        "Ask Codex",
                    )
                )
                tui.submit_line("/agents", pasted_marker="/agents")
                opened = tui.until("Agent command center")
                assert "Group:" in opened
                tui.send("/")
                tui.resize(111)
                search = tui.until_all(
                    ("Search ›", "Agent command center"), initial=opened
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
                ephemeral=False,
                approval_mode=ApprovalMode.deny_all,
                sandbox=Sandbox.workspace_write,
            )
            root.set_name("root-package-task")
            assert root.run("Complete root seed.").final_response == "root done"
            child = client.thread_fork(root.id, ephemeral=False)
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

        with shared_agent_server(isolated):
            with PackagedTui(isolated, "resume", root_id) as tui:
                tui.until_all(
                    (
                        "/review - review any changes and find issues",
                        root_id,
                        "Ask Codex",
                    )
                )
                tui.submit_line("/agents", pasted_marker="/agents")
                tree = tui.until("nested-package-task")
                assert "Agent command center" in tree
                assert "root-package-task" in tree and "child-package-task" in tree
                tui.send("/")
                assert "Search ›" in tui.until("Search ›")
                tui.send("child-package-task")
                child_row = tui.until("child-package-task")
                assert "child-package-task" in child_row
                tui.send("\x03")
                assert "nested-package-task" in tui.until("nested-package-task")
                tui.send("/")
                assert "Search ›" in tui.until("Search ›")
                tui.send("nested-package-task")
                nested_row = tui.until("nested-package-task")
                assert "nested-package-task" in nested_row
                tui.send("\x03")
                assert "root-package-task" in tui.until("root-package-task")
                tui.send("/")
                assert "Search ›" in tui.until("Search ›")
                tui.send("no-such-synthetic-task")
                assert "No matching tasks" in tui.until("No matching tasks")
                tui.send("\x03")
                assert "nested-package-task" in tui.until("nested-package-task")
                tui.send("r")
                assert "Rename ›" in tui.until("Rename ›")
                tui.send("\x7f" * 80)
                tui.send_paste("live-renamed-package-task")
                # The PTY helper strips cursor moves rather than applying them;
                # require the whole edited value in a fresh viewport, not diffs.
                tui.until("renamed")
                tui.resize(111)
                tui.until("live-renamed-package-task")
                tui.send("\r")
                # A full fresh viewport avoids matching fragmented cell updates.
                tui.resize(109)
                assert "live-renamed-package-task" in tui.until(
                    "live-renamed-package-task"
                )

        with shared_agent_server(isolated):
            with PackagedTui(isolated, "resume", root_id) as tui:
                tui.until_all(
                    (
                        "/review - review any changes and find issues",
                        root_id,
                        "Ask Codex",
                    )
                )
                tui.submit_line("/agents", pasted_marker="/agents")
                tui.until("live-renamed-package-task")
                tui.resize(111)
                replay = tui.until_all(
                    (
                        "Agent command center",
                        "child-package-task",
                        "nested-package-task",
                        "live-renamed-package-task",
                    )
                )
                assert "Agent command center" in replay
                assert "child-package-task" in replay
                assert "nested-package-task" in replay
                assert "live-renamed-package-task" in replay
