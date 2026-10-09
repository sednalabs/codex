"""Request-matched Responses fixtures stay deterministic under concurrency."""

from __future__ import annotations

import json
import threading
import urllib.error
import urllib.request

import pytest
from app_server_harness import MockResponsesServer


def _post(server: MockResponsesServer, marker: str) -> bytes:
    request = urllib.request.Request(
        f"{server.url}/v1/responses",
        data=json.dumps({"marker": marker}).encode("utf-8"),
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=5) as response:
        return response.read()


def test_fifo_response_selection_remains_the_default() -> None:
    with MockResponsesServer() as server:
        body = "event: response.completed\ndata: {\"response\":{\"id\":\"fifo\"}}\n\n"
        server.enqueue_sse(body)
        assert _post(server, "fifo") == body.encode("utf-8")


def test_request_routes_match_exact_requests_and_wait_outside_selector_lock() -> None:
    gate = threading.Event()
    done_a = threading.Event()
    done_b = threading.Event()
    errors: list[BaseException] = []
    responses: dict[str, bytes] = {}

    def send(server: MockResponsesServer, marker: str, done: threading.Event) -> None:
        try:
            responses[marker] = _post(server, marker)
        except BaseException as error:
            errors.append(error)
        finally:
            done.set()

    with MockResponsesServer() as server:
        response_a = 'event: response.completed\ndata: {"response":{"id":"a"}}\n\n'
        response_b = 'event: response.completed\ndata: {"response":{"id":"b"}}\n\n'
        route_a = server.enqueue_sse_for_request(
            lambda request: request.body_json().get("marker") == "a",
            response_a,
            gate=gate,
        )
        server.enqueue_sse_for_request(
            lambda request: request.body_json().get("marker") == "b",
            response_b,
        )

        thread_a = threading.Thread(target=send, args=(server, "a", done_a))
        thread_a.start()
        captured_a = server.wait_for_request(
            lambda request: request.body_json().get("marker") == "a"
        )
        assert captured_a.body_json()["marker"] == "a"
        route_a.wait_until_selected()

        thread_b = threading.Thread(target=send, args=(server, "b", done_b))
        thread_b.start()
        assert done_b.wait(5), "sibling request could not select while route A was gated"
        assert responses.get("b") == response_b.encode("utf-8")
        assert not done_a.is_set(), "route A escaped its closed gate"

        gate.set()
        assert done_a.wait(5), "route A did not complete after release"
        assert responses.get("a") == response_a.encode("utf-8")
        thread_a.join(timeout=1)
        thread_b.join(timeout=1)
        assert not errors, errors
        assert {request.body_json()["marker"] for request in server.requests()} == {"a", "b"}


@pytest.mark.parametrize(
    ("routes", "marker", "diagnostic"),
    [
        (1, "unmatched", "unmatched Responses request"),
        (2, "ambiguous", "ambiguous Responses request"),
    ],
)
def test_bad_request_route_sets_fail_and_are_reported_on_teardown(
    routes: int,
    marker: str,
    diagnostic: str,
) -> None:
    with pytest.raises(AssertionError, match=diagnostic):
        with MockResponsesServer() as server:
            for _ in range(routes):
                expected = "different" if marker == "unmatched" else marker
                server.enqueue_sse_for_request(
                    lambda request, expected=expected: (
                        request.body_json().get("marker") == expected
                    ),
                    "event: response.completed\ndata: {}\n\n",
                )
            with pytest.raises(urllib.error.HTTPError, match="500"):
                _post(server, marker)
            assert diagnostic in " ".join(server.routing_errors())


def test_unused_one_shot_routes_fail_at_teardown() -> None:
    with pytest.raises(AssertionError, match="unused_routes=1"):
        with MockResponsesServer() as server:
            server.enqueue_sse_for_request(
                lambda _request: True,
                "event: response.completed\ndata: {}\n\n",
            )


def test_one_shot_route_cannot_be_reused() -> None:
    with pytest.raises(AssertionError, match="one-shot Responses route was matched more than once"):
        with MockResponsesServer() as server:
            server.enqueue_sse_for_request(
                lambda request: request.body_json().get("marker") == "repeat",
                "event: response.completed\ndata: {}\n\n",
            )
            assert _post(server, "repeat").startswith(b"event: response.completed")
            with pytest.raises(urllib.error.HTTPError, match="500"):
                _post(server, "repeat")
            assert "one-shot Responses route was matched more than once" in " ".join(
                server.routing_errors()
            )


def test_fifo_and_request_matched_modes_cannot_be_mixed() -> None:
    with MockResponsesServer() as server:
        server.enqueue_sse("event: response.completed\ndata: {}\n\n")
        with pytest.raises(RuntimeError, match="cannot be mixed"):
            server.enqueue_sse_for_request(
                lambda _request: True,
                "event: response.completed\ndata: {}\n\n",
            )
        assert _post(server, "fifo") == b"event: response.completed\ndata: {}\n\n"


def test_server_teardown_releases_a_gated_request() -> None:
    gate = threading.Event()
    done = threading.Event()
    errors: list[BaseException] = []

    def send(server: MockResponsesServer) -> None:
        try:
            _post(server, "teardown")
        except BaseException as error:
            errors.append(error)
        finally:
            done.set()

    with MockResponsesServer() as server:
        route = server.enqueue_sse_for_request(
            lambda request: request.body_json().get("marker") == "teardown",
            "event: response.completed\ndata: {}\n\n",
            gate=gate,
        )
        thread = threading.Thread(target=send, args=(server,))
        thread.start()
        route.wait_until_selected()
        server.close()
        assert done.wait(1), "server teardown did not release the gated HTTP handler"
        thread.join(timeout=1)
        assert not errors, errors
