"""Exercise packaged double-Esc interruption and persisted turn identity in a real PTY."""

import json
import shutil
import threading
import time
from contextlib import contextmanager
from dataclasses import replace
from pathlib import Path
from urllib.parse import urlsplit

from app_server_harness import (
    CapturedResponsesRequest,
    MockResponsesServer,
    ev_assistant_message,
    ev_completed,
    ev_response_created,
    sse,
)
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox
from openai_codex.generated.v2_all import TurnStatus

from fixtures import SmokePackage
from package_acceptance import _mock_config
from tui_pty import PackagedTui


INTERRUPTED_NOTICE = "Conversation interrupted - use /feedback if something went wrong"
ARMED_HINT = "esc again to interrupt"


@contextmanager
def _release_gates_on_exit(gates: list[threading.Event]):
    try:
        yield
    finally:
        for gate in gates:
            gate.set()


def _isolated(package: SmokePackage) -> tuple[SmokePackage, Path]:
    home = package.directory / "double-esc-acceptance"
    home.mkdir()
    secret_names = (
        "API_KEY", "ACCESS_TOKEN", "AUTH_TOKEN", "BEARER_TOKEN", "PASSWORD",
        "SECRET", "CREDENTIAL", "AUTHORIZATION",
    )
    environment = {
        name: value for name, value in package.environment.items()
        if not any(secret_name in name.upper() for secret_name in secret_names)
    }
    environment["CODEX_HOME"] = str(home)
    return replace(package, environment=environment), home


def _sdk(package: SmokePackage) -> Codex:
    return Codex(
        config=CodexConfig(
            codex_bin=str(package.cli),
            cwd=str(package.directory),
            env=package.environment,
        )
    )


def _turn_metadata(request: CapturedResponsesRequest) -> tuple[str, str]:
    body = request.body_json()
    metadata = body.get("client_metadata")
    assert isinstance(metadata, dict), body
    thread_id = metadata.get("thread_id")
    raw_turn = metadata.get("x-codex-turn-metadata")
    if isinstance(raw_turn, str):
        raw_turn = json.loads(raw_turn)
    assert isinstance(thread_id, str) and isinstance(raw_turn, dict), metadata
    turn_id = raw_turn.get("turn_id")
    assert isinstance(turn_id, str) and turn_id, raw_turn
    return thread_id, turn_id


def _matches_prompt(
    request: CapturedResponsesRequest, thread_id: str, marker: str
) -> bool:
    if request.method != "POST" or request.path != "/v1/responses":
        return False
    metadata = request.body_json().get("client_metadata")
    if not isinstance(metadata, dict) or metadata.get("thread_id") != thread_id:
        return False
    user_messages = request.message_input_texts("user")
    return bool(user_messages) and marker in user_messages[-1]


def _hold_prompt(
    tui: PackagedTui,
    server: MockResponsesServer,
    thread_id: str,
    marker: str,
    held_gates: list[threading.Event],
) -> tuple[threading.Event, CapturedResponsesRequest, str]:
    gate = threading.Event()
    held_gates.append(gate)
    response_id = f"response-{marker.lower()}"
    answer = f"{marker}_ANSWER"
    route = server.enqueue_sse_for_request(
        lambda request: _matches_prompt(request, thread_id, marker),
        sse([
            ev_response_created(response_id),
            ev_assistant_message(f"message-{marker.lower()}", answer),
            ev_completed(response_id),
        ]),
        gate=gate,
    )
    tui.send(f"Please reply exactly with {marker}_ANSWER.")
    # Keep text and Enter as separate writes: the packaged TUI classifies paste bursts.
    tui.send("\r")
    route.wait_until_selected()
    request = server.wait_for_request(
        lambda captured: _matches_prompt(captured, thread_id, marker)
    )
    actual_thread_id, turn_id = _turn_metadata(request)
    assert actual_thread_id == thread_id
    return gate, request, turn_id


def _assert_persisted_turn(
    package: SmokePackage,
    thread_id: str,
    request: CapturedResponsesRequest,
    turn_id: str,
    marker: str,
    status: TurnStatus,
) -> None:
    request_thread_id, request_turn_id = _turn_metadata(request)
    assert request_thread_id == thread_id
    assert request_turn_id == turn_id
    with _sdk(package) as client:
        thread = client.thread_resume(thread_id)
        history = thread.read(include_turns=True)
    matching = [turn for turn in history.thread.turns if turn.id == turn_id]
    assert len(matching) == 1, [turn.id for turn in history.thread.turns]
    turn = matching[0]
    assert marker in json.dumps(turn.model_dump(mode="json")), turn.model_dump(mode="json")
    assert turn.status == status, (turn.id, turn.status, status)


def _open_agents(tui: PackagedTui) -> str:
    # Use the existing packaged selector and accept the real popup, not a source-only menu.
    tui.until("Ask Codex to do anything")
    tui.send("/agents")
    popup = tui.until("open the agent command center")
    assert "/agents" in popup, popup
    tui.send("\r")
    return tui.until_screen("Agent command center", required_markers=("Group:",))


def _finish_positive(
    tui: PackagedTui,
    package: SmokePackage,
    thread_id: str,
    gate: threading.Event,
    request: CapturedResponsesRequest,
    turn_id: str,
    marker: str,
    status: TurnStatus = TurnStatus.completed,
) -> None:
    gate.set()
    answer = f"{marker}_ANSWER"
    tui.until_screen(answer)
    _assert_persisted_turn(package, thread_id, request, turn_id, marker, status)


def test_packaged_double_escape_interrupt_persists_exact_turn_and_resumes(
    package: SmokePackage,
) -> None:
    isolated, home = _isolated(package)
    held_gates: list[threading.Event] = []
    try:
        with MockResponsesServer() as server, _release_gates_on_exit(held_gates):
            assert urlsplit(server.url).hostname in {"127.0.0.1", "localhost", "::1"}
            _mock_config(home, server)
            with _sdk(isolated) as client:
                thread = client.thread_start(
                    ephemeral=False,
                    approval_mode=ApprovalMode.deny_all,
                    sandbox=Sandbox.workspace_write,
                )
                thread_id = thread.id

            # E2 source tests own typed modifier/repeat/release semantics. This packaged
            # fixture uses raw bytes only and never claims a physical Alt or repeat event.
            with PackagedTui(isolated, "resume", thread_id) as tui:
                # Case 01: one Esc positively arms; releasing the exact held request completes.
                gate, request, turn_id = _hold_prompt(
                    tui, server, thread_id, "FIRST_ESC_POSITIVE_01", held_gates
                )
                tui.send("\x1b")
                tui.until_screen(ARMED_HINT)
                _finish_positive(
                    tui, isolated, thread_id, gate, request, turn_id,
                    "FIRST_ESC_POSITIVE_01",
                )

                # Case 07: a new active turn needs its own fresh arm after completion.
                gate, request, turn_id = _hold_prompt(
                    tui, server, thread_id, "FRESH_EPOCH_POSITIVE_07", held_gates
                )
                tui.send("\x1b")
                tui.until_screen(ARMED_HINT)
                _finish_positive(
                    tui, isolated, thread_id, gate, request, turn_id,
                    "FRESH_EPOCH_POSITIVE_07",
                )

                # Case 02: ESC+x is only a positive byte-stream completion control.
                gate, request, turn_id = _hold_prompt(
                    tui, server, thread_id, "ALT_PREFIX_POSITIVE_02", held_gates
                )
                tui.send("\x1bx")
                _finish_positive(
                    tui, isolated, thread_id, gate, request, turn_id,
                    "ALT_PREFIX_POSITIVE_02",
                )

                # Case 03: a separate second raw Esc interrupts; screen and persisted
                # thread/turn identity are both required before any follow-up.
                marker = "DOUBLE_ESCAPE_CANCEL_03"
                gate, request, turn_id = _hold_prompt(
                    tui, server, thread_id, marker, held_gates
                )
                first_esc_at = time.monotonic()
                tui.send("\x1b")
                tui.until_screen(ARMED_HINT)
                second_esc_at = time.monotonic()
                assert 0 <= second_esc_at - first_esc_at <= 1.0
                tui.send("\x1b")
                tui.until_screen(INTERRUPTED_NOTICE)
                _assert_persisted_turn(
                    isolated, thread_id, request, turn_id, marker, TurnStatus.interrupted
                )
                gate.set()

                # A post-interrupt same-thread follow-up proves the session remains usable.
                followup = "AFTER_INTERRUPT_FOLLOWUP_03"
                gate, request, turn_id = _hold_prompt(
                    tui, server, thread_id, followup, held_gates
                )
                _finish_positive(
                    tui, isolated, thread_id, gate, request, turn_id, followup
                )

                # Case 04: close the actual /agents popup while another exact request is
                # held, then release it and require its positive completion.
                menu_marker = "MENU_ESCAPE_ACTIVE_04"
                gate, request, turn_id = _hold_prompt(
                    tui, server, thread_id, menu_marker, held_gates
                )
                _open_agents(tui)
                tui.send("\x1b")
                tui.until_screen("Ask Codex to do anything")
                _finish_positive(
                    tui, isolated, thread_id, gate, request, turn_id, menu_marker
                )

                # Case 05: Ctrl+R then Esc restores this exact unsent draft.
                draft = "DRAFT_RESTORE_COMPOSER_05"
                tui.send(draft)
                tui.send("\x12")
                tui.send("\x1b")
                tui.until_screen(draft)
                tui.send("\x7f" * len(draft))
                empty_composer = tui.until_screen("Ask Codex to do anything")
                assert draft not in empty_composer

                # Case 06: idle Esc leaves the composer usable; completion is the positive
                # usability witness rather than unchanged-screen or absence-only evidence.
                idle_marker = "IDLE_ESCAPE_USABLE_06"
                tui.send("\x1b")
                gate, request, turn_id = _hold_prompt(
                    tui, server, thread_id, idle_marker, held_gates
                )
                _finish_positive(
                    tui, isolated, thread_id, gate, request, turn_id, idle_marker
                )

                # Case 10: bound both key-write spacing and the fresh rendered frame so
                # natural expiry or delayed key handling cannot make stale E1 look reset.
                marker = "CTRL_L_RESET_POSITIVE_10"
                gate, request, turn_id = _hold_prompt(
                    tui, server, thread_id, marker, held_gates
                )
                t0 = time.monotonic()
                tui.send("\x1b")
                tui.until_screen(ARMED_HINT)
                tui.send("\x0c")
                t1 = time.monotonic()
                tui.send("\x1b")
                fresh_hint = tui.until_screen(ARMED_HINT)
                t2 = time.monotonic()
                assert 0 <= t1 - t0 <= 1.0, (t0, t1, fresh_hint)
                assert 0 <= t2 - t0 <= 1.0, (t0, t2, fresh_hint)
                _finish_positive(
                    tui, isolated, thread_id, gate, request, turn_id, marker
                )

            # Case 08: resume the exact thread; its persisted interrupted history remains
            # readable, and a new turn's first Esc must produce its own current hint.
            _assert_persisted_turn(
                isolated,
                thread_id,
                request,
                turn_id,
                "CTRL_L_RESET_POSITIVE_10",
                TurnStatus.completed,
            )
            with PackagedTui(isolated, "resume", thread_id) as resumed:
                resumed_frame = resumed.until_screen("CTRL_L_RESET_POSITIVE_10_ANSWER")
                assert ARMED_HINT not in resumed_frame
                with _sdk(isolated) as client:
                    resumed_thread = client.thread_resume(thread_id)
                    history = resumed_thread.read(include_turns=True)
                interrupted = [
                    turn for turn in history.thread.turns
                    if turn.status == TurnStatus.interrupted
                ]
                assert len(interrupted) == 1
                assert "DOUBLE_ESCAPE_CANCEL_03" in json.dumps(
                    interrupted[0].model_dump(mode="json")
                )
                next_marker = "RESUME_NEW_EPOCH_08"
                gate, request, turn_id = _hold_prompt(
                    resumed, server, thread_id, next_marker, held_gates
                )
                resumed.send("\x1b")
                resumed.until_screen(ARMED_HINT)
                _finish_positive(
                    resumed, isolated, thread_id, gate, request, turn_id, next_marker
                )

            # Every declared held route is reconciled; no secret-bearing config is printed.
            assert not server.routing_errors(), server.routing_errors()
    finally:
        for gate in held_gates:
            gate.set()
        if home.exists():
            shutil.rmtree(home)
