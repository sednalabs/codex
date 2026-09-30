"""First-binary Responses completions joined to durable usage estimates."""

from __future__ import annotations

import sqlite3
from pathlib import Path
from typing import Any

from app_server_harness import (
    MockResponsesServer,
    ev_assistant_message,
    ev_response_created,
    sse,
)
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage
from package_acceptance import _mock_config


def _completed_event(
    response_id: str,
    *,
    model: str | None,
    usage: dict[str, int] | None,
) -> dict[str, Any]:
    response: dict[str, Any] = {"id": response_id}
    if model is not None:
        response["headers"] = {"openai-model": model}
    if usage is not None:
        response["usage"] = {
            "input_tokens": usage["input"],
            "input_tokens_details": {
                "cached_tokens": usage["cached"],
                "cache_write_tokens": usage["cache_write"],
            },
            "output_tokens": usage["output"],
            "output_tokens_details": {"reasoning_tokens": usage["reasoning"]},
            "total_tokens": usage["total"],
        }
    return {"type": "response.completed", "response": response}


def _usage_tool_response(
    response_id: str,
    call_id: str,
    *,
    model: str,
    usage: dict[str, int],
) -> str:
    return sse(
        [
            ev_response_created(response_id),
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "custom_tool_call",
                    "call_id": call_id,
                    "name": "exec",
                    "input": "text('usage ledger fixture continuation')",
                },
            },
            _completed_event(response_id, model=model, usage=usage),
        ]
    )


def _assistant_response(
    response_id: str,
    text: str,
    *,
    model: str | None,
    usage: dict[str, int] | None,
) -> str:
    return sse(
        [
            ev_response_created(response_id),
            ev_assistant_message(f"message-{response_id}", text),
            _completed_event(response_id, model=model, usage=usage),
        ]
    )


def test_real_response_usage_joins_standard_rate_scenario_after_resume(
    package: SmokePackage,
    responses_server: MockResponsesServer,
    tmp_path: Path,
) -> None:
    """Packaged sessions persist response-local rows and saved fork lineage."""
    home = package.directory / f"usage-ledger-{tmp_path.name}"
    _mock_config(home, responses_server)
    environment = {**package.environment, "CODEX_HOME": str(home)}
    config = CodexConfig(
        codex_bin=str(package.cli),
        cwd=str(package.directory),
        env=environment,
    )

    known_usage = {
        "input": 1_000,
        "cached": 200,
        "cache_write": 0,
        "output": 25,
        "reasoning": 5,
        "total": 1_025,
    }
    changed_replay_usage = {**known_usage, "input": 1_001, "total": 1_026}
    unknown_usage = {
        "input": 400,
        "cached": 40,
        "cache_write": 0,
        "output": 12,
        "reasoning": 4,
        "total": 412,
    }

    # One code-mode turn continues through distinct response IDs, then replays
    # one ID identically and once with a changed payload.
    for response_id, call_id, model, usage in [
        ("response-root-known", "call-known", "gpt-6.1-sol", known_usage),
        (
            "response-root-unknown-model",
            "call-unknown-model",
            "gpt-ledger-fixture-unpriced",
            unknown_usage,
        ),
        ("response-root-known", "call-known-replay", "gpt-6.1-sol", known_usage),
        (
            "response-root-known",
            "call-known-conflict",
            "gpt-6.1-sol",
            changed_replay_usage,
        ),
    ]:
        responses_server.enqueue_sse(
            _usage_tool_response(
                response_id,
                call_id,
                model=model,
                usage=usage,
            )
        )
    responses_server.enqueue_sse(
        _assistant_response(
            "response-root-finish",
            "root complete",
            model=None,
            usage=None,
        )
    )

    with Codex(config=config) as client:
        root = client.thread_start(
            ephemeral=False,
            approval_mode=ApprovalMode.deny_all,
            sandbox=Sandbox.workspace_write,
        )
        root_turn = root.run("exercise response-local usage persistence")
        assert root_turn.final_response == "root complete", root_turn

        # This is a saved thread fork, not a model-facing spawned subagent.
        child = client.thread_fork(root.id, ephemeral=False, include_turns=True)
        responses_server.enqueue_sse(
            _assistant_response(
                "response-child-missing-usage",
                "child complete without usage metadata",
                model=None,
                usage=None,
            )
        )
        child_turn = child.run("record a response without usage metadata")
        assert (
            child_turn.final_response == "child complete without usage metadata"
        ), child_turn

    usage_db = home / "usage_1.sqlite"
    assert usage_db.is_file()
    with sqlite3.connect(f"file:{usage_db.as_posix()}?mode=ro", uri=True) as db:
        root_rows = db.execute(
            """
            SELECT request_id, turn_id, provider, requested_model, actual_model_used,
                   input_tokens_uncached, input_tokens_cached,
                   input_tokens_cache_write, output_tokens, total_tokens, status
            FROM usage_provider_calls
            WHERE thread_id = ?
            ORDER BY request_id
            """,
            (root.id,),
        ).fetchall()
        assert len(root_rows) == 3, root_rows
        root_by_response = {row[0]: row for row in root_rows}
        assert set(root_by_response) == {
            "response-root-known",
            "response-root-unknown-model",
            "response-root-finish",
        }
        assert len({row[1] for row in root_rows}) == 1, root_rows

        known = root_by_response["response-root-known"]
        assert known[2], known
        assert known[3] == "package-smoke", known
        assert known[4] == "gpt-6.1-sol", known
        assert known[5:10] == (800, 200, 0, 25, 1_025), known
        assert known[10] == "ok", known

        idempotency_count = db.execute(
            """
            SELECT COUNT(*) FROM usage_response_idempotency
            WHERE provider = ? AND account_scope = ? AND thread_id = ?
              AND response_id = ?
            """,
            (known[2], "", root.id, "response-root-known"),
        ).fetchone()[0]
        assert idempotency_count == 1

        scenario = db.execute(
            """
            SELECT scenario_status, estimated_total_credits,
                   estimate_scenario, assumption_source,
                   assumed_rate_provider, assumed_rate_card_kind,
                   assumed_service_tier, assumed_speed_mode, estimate_unit,
                   rate_effective_from, rate_source_observed_at,
                   observed_provider, observed_model, model_evidence,
                   requested_model
            FROM usage_provider_call_standard_rate_estimates
            WHERE provider_call_id = (
                SELECT provider_call_id FROM usage_provider_calls
                WHERE thread_id = ? AND request_id = ?
            )
            """,
            (root.id, "response-root-known"),
        ).fetchone()
        assert scenario is not None
        assert scenario[0] == "priced_scenario_estimate", scenario
        assert abs(scenario[1] - 0.04675) < 1e-12, scenario
        assert scenario[2:9] == (
            "operator_supplied_standard_rate_card_scenario",
            "operator_supplied_credit_rate_guide",
            "openai",
            "codex_token_based",
            "default",
            "standard",
            "credits",
        ), scenario
        assert scenario[9:14] == (
            "2026-09-29T23:52:00Z",
            "2026-09-29T23:52:00Z",
            known[2],
            "gpt-6.1-sol",
            "actual_model_used",
        ), scenario
        assert scenario[14] == "package-smoke", scenario

        strict = db.execute(
            """
            SELECT pricing_status, estimated_total_credits
            FROM usage_provider_call_credit_estimates
            WHERE provider_call_id = (
                SELECT provider_call_id FROM usage_provider_calls
                WHERE thread_id = ? AND request_id = ?
            )
            """,
            (root.id, "response-root-known"),
        ).fetchone()
        assert strict == ("actual_tier_missing", None), strict

        unknown = db.execute(
            """
            SELECT scenario_status, estimated_total_credits, observed_model,
                   model_evidence
            FROM usage_provider_call_standard_rate_estimates
            WHERE provider_call_id = (
                SELECT provider_call_id FROM usage_provider_calls
                WHERE thread_id = ? AND request_id = ?
            )
            """,
            (root.id, "response-root-unknown-model"),
        ).fetchone()
        assert unknown == (
            "model_rate_missing",
            None,
            "gpt-ledger-fixture-unpriced",
            "actual_model_used",
        ), unknown

        rows = db.execute(
            """
            SELECT thread_id, parent_thread_id, root_thread_id,
                   fork_parent_thread_id
            FROM usage_threads
            WHERE thread_id IN (?, ?)
            ORDER BY thread_id
            """,
            (root.id, child.id),
        ).fetchall()
        lineage = {row[0]: row[1:] for row in rows}
        assert lineage[root.id] == (None, root.id, None), lineage
        assert lineage[child.id] == (root.id, root.id, root.id), lineage

        missing = db.execute(
            """
            SELECT p.actual_model_used, p.input_tokens_uncached,
                   p.input_tokens_cached, p.input_tokens_cache_write,
                   p.output_tokens, p.total_tokens,
                   e.scenario_status, e.estimated_total_credits
            FROM usage_provider_calls AS p
            JOIN usage_provider_call_standard_rate_estimates AS e
              USING (provider_call_id)
            WHERE p.thread_id = ? AND p.request_id = ?
            """,
            (child.id, "response-child-missing-usage"),
        ).fetchone()
        assert missing == (
            None,
            None,
            None,
            None,
            None,
            None,
            "provider_usage_missing",
            None,
        )

    # A new packaged process resumes the saved fork and appends another row.
    responses_server.enqueue_sse(
        _assistant_response(
            "response-child-after-resume",
            "child resumed",
            model="gpt-6.1-sol",
            usage=known_usage,
        )
    )
    with Codex(config=config) as resumed:
        assert resumed.thread_resume(root.id).id == root.id
        resumed_child = resumed.thread_resume(child.id)
        assert resumed_child.id == child.id
        resumed_turn = resumed_child.run("append one response after resume")
        assert resumed_turn.final_response == "child resumed", resumed_turn

    with sqlite3.connect(f"file:{usage_db.as_posix()}?mode=ro", uri=True) as db:
        child_rows = db.execute(
            """
            SELECT request_id, actual_model_used, input_tokens_uncached,
                   input_tokens_cached, input_tokens_cache_write, output_tokens,
                   total_tokens, status
            FROM usage_provider_calls
            WHERE thread_id = ?
            ORDER BY request_id
            """,
            (child.id,),
        ).fetchall()
        assert len(child_rows) == 2, child_rows
        assert child_rows[0] == (
            "response-child-after-resume",
            "gpt-6.1-sol",
            800,
            200,
            0,
            25,
            1_025,
            "ok",
        )
        assert child_rows[1] == (
            "response-child-missing-usage",
            None,
            None,
            None,
            None,
            None,
            None,
            "provider_usage_missing",
        )
        resumed_scenario = db.execute(
            """
            SELECT scenario_status, estimated_total_credits
            FROM usage_provider_call_standard_rate_estimates
            WHERE provider_call_id = (
                SELECT provider_call_id FROM usage_provider_calls
                WHERE thread_id = ? AND request_id = ?
            )
            """,
            (child.id, "response-child-after-resume"),
        ).fetchone()
        assert resumed_scenario is not None
        assert resumed_scenario[0] == "priced_scenario_estimate", resumed_scenario
        assert abs(resumed_scenario[1] - 0.04675) < 1e-12, resumed_scenario
