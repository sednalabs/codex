#!/usr/bin/env python3
"""Synthetic-only v2.7.2 sidecar roster/rate adapter; no live home or ledger access."""
import collections
import datetime as dt
import decimal
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time
import urllib.request

CANDIDATE_SHA = "8b63bb1a6b6313da7a647981c4c9a549e376caf3"
PORT = 43189
FIELDS = ("input", "cached_input", "cache_write_input", "output", "reasoning_output", "total")
ZERO = dict.fromkeys(FIELDS, 0)
ROSTER = {
    "pilot-root": {"parent_id": None, "configured_model": "configured-root"},
    "pilot-child": {"parent_id": "pilot-root", "configured_model": "configured-child"},
    "pilot-grandchild": {"parent_id": "pilot-child", "configured_model": "configured-grandchild"},
    "pilot-idle-child": {"parent_id": "pilot-root", "configured_model": "configured-idle"},
}
FIXTURE_CREDITS_PER_1M_TOKENS = {
    "synthetic-boundary-model": {
        "standard": [
            {"effective": "2026-10-01T00:00:00Z", "credits_per_1M_tokens": {
                "uncached_input": "10", "cached_input": "2",
                "cache_write_input": "12", "output": "40"}},
            {"effective": "2026-10-05T00:00:00Z", "credits_per_1M_tokens": {
                "uncached_input": "20", "cached_input": "4",
                "cache_write_input": "24", "output": "80"}},
        ],
        "fast": [
            {"effective": "2026-10-01T00:00:00Z", "credits_per_1M_tokens": {
                "uncached_input": "30", "cached_input": "6",
                "cache_write_input": "36", "output": "120"}},
            {"effective": "2026-10-05T00:00:00Z", "credits_per_1M_tokens": {
                "uncached_input": "40", "cached_input": "8",
                "cache_write_input": "48", "output": "160"}},
        ],
    },
    "synthetic-second-model": {
        "standard": [
            {"effective": "2026-10-01T00:00:00Z", "credits_per_1M_tokens": {
                "uncached_input": "5", "cached_input": "0.5",
                "cache_write_input": "7.5", "output": "30"}},
        ],
    },
}
D = decimal.Decimal


def usage(input_tokens=100, cached=20, write=10, output=20, reasoning=2):
    return {"input_tokens": input_tokens, "cached_input_tokens": cached,
            "cache_write_input_tokens": write, "output_tokens": output,
            "reasoning_output_tokens": reasoning, "total_tokens": input_tokens + output}


def write_session(home, session_id, parent_id, events):
    folder = home / "sessions" / "2026" / "10" / "09"
    folder.mkdir(parents=True, exist_ok=True)
    source = {}
    if parent_id:
        source = {"subagent": {"thread_spawn": {"parent_thread_id": parent_id}}}
    lines = [{"timestamp": "2026-10-09T00:00:00Z", "type": "session_meta",
              "payload": {"id": session_id, "cwd": "/synthetic/" + session_id,
                          "originator": "codex_cli_rs", "source": source}}]
    for index, event in enumerate(events):
        turn = "pilot-turn-" + str(index)
        lines.append({"timestamp": event["timestamp"], "type": "turn_context",
                      "payload": {"turn_id": turn, "cwd": "/synthetic/" + session_id,
                                  "model": event["model"], "service_tier": event["tier"]}})
        vector = usage()
        lines.append({"timestamp": event["timestamp"], "type": "token_usage_record",
                      "payload": {"thread_id": session_id, "turn_id": turn,
                                  "response_id": "pilot-response-" + session_id + "-" + str(index),
                                  "usage": vector, "turn_token_usage": vector}})
    path = folder / (session_id + ".jsonl")
    path.write_text("".join(json.dumps(row, separators=(",", ":")) + "\n" for row in lines),
                    encoding="utf-8")


def get_json(url):
    with urllib.request.urlopen(url, timeout=3) as response:
        return json.load(response)


def credits_for_event(event):
    model = event.get("model", "")
    by_mode = FIXTURE_CREDITS_PER_1M_TOKENS.get(model)
    if by_mode is None:
        return None, "unknown_model"
    mode = event.get("service_mode")
    rates_by_date = by_mode.get(mode)
    if rates_by_date is None:
        return None, "unknown_mode"
    timestamp = dt.datetime.fromisoformat(event["timestamp"].replace("Z", "+00:00"))
    applicable = [entry for entry in rates_by_date
                  if dt.datetime.fromisoformat(entry["effective"].replace("Z", "+00:00")) <= timestamp]
    if not applicable:
        return None, "no_effective_rate"
    rates = applicable[-1]["credits_per_1M_tokens"]
    counts = event["usage"]
    # v2.7.2 validates cached_input <= input and cache_write_input <=
    # input - cached_input, then applies the same disjoint category split.
    uncached_input = counts["input"] - counts["cached_input"] - counts["cache_write_input"]
    credits = (D(uncached_input) * D(rates["uncached_input"]) +
               D(counts["cached_input"]) * D(rates["cached_input"]) +
               D(counts["cache_write_input"]) * D(rates["cache_write_input"]) +
               D(counts["output"]) * D(rates["output"])) / D(1_000_000)
    return credits, None


def main(binary):
    with tempfile.TemporaryDirectory(prefix="w15087-synthetic-") as temp:
        root = pathlib.Path(temp)
        fake_home, state = root / "codex-home", root / "usage-state"
        fake_home.mkdir()
        state.mkdir()
        os.environ["CODEX_HOME"] = str(fake_home)
        os.environ["CODEX_USAGE_HOME"] = str(state)
        (state / "config.json").write_text(json.dumps({"listen_address": "127.0.0.1", "port": PORT, "scan_interval_seconds": 600}),
                                            encoding="utf-8")
        # Prevent the candidate server updater from making background GitHub calls.
        (state / ".codex-usage-updates.json").write_text(json.dumps({"auto_check": False}),
                                                        encoding="utf-8")

        write_session(fake_home, "pilot-root", None, [
            {"timestamp": "2026-10-04T00:00:01Z", "model": "synthetic-boundary-model", "tier": "default"},
        ])
        write_session(fake_home, "pilot-child", "pilot-root", [
            {"timestamp": "2026-10-04T00:01:01Z", "model": "synthetic-boundary-model", "tier": "default"},
            {"timestamp": "2026-10-06T00:01:01Z", "model": "synthetic-boundary-model", "tier": "default"},
            {"timestamp": "2026-10-06T00:01:30Z", "model": "synthetic-boundary-model", "tier": "priority"},
            {"timestamp": "2026-10-06T00:02:01Z", "model": "synthetic-second-model", "tier": "default"},
        ])
        write_session(fake_home, "pilot-grandchild", "pilot-child", [
            {"timestamp": "2026-09-30T23:59:00Z", "model": "synthetic-second-model", "tier": "default"},
            {"timestamp": "2026-10-06T00:03:01Z", "model": "synthetic-boundary-model", "tier": "mystery"},
            {"timestamp": "2026-10-06T00:03:30Z", "model": "model-without-rate", "tier": "default"},
        ])
        write_session(fake_home, "pilot-unrostered-session", None, [
            {"timestamp": "2026-10-06T00:04:01Z", "model": "synthetic-boundary-model", "tier": "default"},
        ])

        subprocess.run([binary, "scan", "--json"], check=True, capture_output=True, text=True)
        server = subprocess.Popen([binary, "serve"], stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL)
        try:
            base = "http://127.0.0.1:" + str(PORT)
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if server.poll() is not None:
                    raise RuntimeError("candidate serve exited before readiness")
                try:
                    get_json(base + "/healthz")
                    break
                except Exception:
                    time.sleep(0.1)
            else:
                raise RuntimeError("candidate health endpoint did not become ready")

            tree = get_json(base + "/api/v1/session-tree?since=all")
            exported = get_json(base + "/api/v1/export?since=all&format=json")
            assert len(exported) < 100, "synthetic corpus exceeds event bound"
            nodes = {row["session_id"]: row for row in tree["items"]}
            own = collections.defaultdict(lambda: dict.fromkeys(FIELDS, 0))
            per_model = collections.defaultdict(set)
            per_model_own = collections.defaultdict(lambda: dict.fromkeys(FIELDS, 0))
            per_model_modes = collections.defaultdict(set)
            per_mode = collections.defaultdict(set)
            per_tier = collections.defaultdict(set)
            own_credits = collections.defaultdict(lambda: D(0))
            per_model_credits = collections.defaultdict(lambda: D(0))
            unpriced = collections.defaultdict(collections.Counter)
            per_model_unpriced = collections.defaultdict(collections.Counter)
            unknown_modes = set()
            for event in exported:
                session_id = event["session_id"]
                vector = event["usage"]
                model = event.get("model") or "unknown"
                model_key = (session_id, model)
                for field in FIELDS:
                    amount = int(vector.get(field, 0) or 0)
                    own[session_id][field] += amount
                    per_model_own[model_key][field] += amount
                per_model[session_id].add(model)
                per_model_modes[model_key].add(event.get("service_mode") or "unknown")
                per_mode[session_id].add(event.get("service_mode") or "unknown")
                per_tier[session_id].add(event.get("service_tier") or "unknown")
                mode = event.get("service_mode")
                if mode == "unknown":
                    unknown_modes.add(session_id)
                credits, unpriced_reason = credits_for_event(event)
                if unpriced_reason:
                    unpriced[session_id][unpriced_reason] += 1
                    per_model_unpriced[model_key][unpriced_reason] += 1
                else:
                    own_credits[session_id] += credits
                    per_model_credits[model_key] += credits

            def recorded_model_rows(session_id):
                rows = []
                for model in sorted(per_model[session_id]):
                    model_key = (session_id, model)
                    statuses = per_model_unpriced[model_key]
                    model_credits = (None if statuses else
                                     str(per_model_credits[model_key].quantize(D("0.00000001"))))
                    rows.append({
                        "recorded_model": model,
                        "recorded_modes": sorted(per_model_modes[model_key]),
                        "own_usage": dict(per_model_own[model_key]),
                        "estimated_credits": model_credits,
                        "unpriced_event_statuses": dict(statuses),
                    })
                return rows

            def aggregate_model_credits(session_id, model_rows):
                if not model_rows or any(row["estimated_credits"] is None for row in model_rows):
                    return None
                raw_total = sum(
                    (per_model_credits[(session_id, row["recorded_model"])] for row in model_rows),
                    D(0),
                )
                return str(raw_total.quantize(D("0.00000001")))

            known_ids = set(ROSTER)
            assert {"pilot-root", "pilot-child", "pilot-grandchild"} <= set(nodes)
            assert nodes["pilot-child"]["parent_id"] == "pilot-root"
            assert nodes["pilot-grandchild"]["parent_id"] == "pilot-child"
            assert "pilot-idle-child" not in own and "pilot-idle-child" not in nodes
            assert "pilot-unrostered-session" in own and "pilot-unrostered-session" not in known_ids
            grandchild_events = [event for event in exported if event["session_id"] == "pilot-grandchild"]
            unknown_tier_events = [event for event in grandchild_events
                                   if event.get("service_tier") == "mystery"]
            assert len(unknown_tier_events) == 1
            assert all(event.get("service_mode") == "unknown" and
                       event.get("mode_source") == "jsonl_turn_context"
                       for event in unknown_tier_events)
            assert "pilot-grandchild" in unknown_modes
            assert unpriced["pilot-grandchild"]["unknown_mode"] == 1
            assert unpriced["pilot-grandchild"]["no_effective_rate"] == 1
            assert unpriced["pilot-grandchild"]["unknown_model"] == 1
            assert {"synthetic-boundary-model", "synthetic-second-model"} <= per_model["pilot-child"]
            child_model_rows = {row["recorded_model"]: row
                                for row in recorded_model_rows("pilot-child")}
            assert set(child_model_rows) == {"synthetic-boundary-model", "synthetic-second-model"}
            assert child_model_rows["synthetic-boundary-model"]["own_usage"] == {
                "input": 300, "cached_input": 60, "cache_write_input": 30,
                "output": 60, "reasoning_output": 6, "total": 360,
            }
            assert child_model_rows["synthetic-boundary-model"]["estimated_credits"] == "0.00721000"
            assert child_model_rows["synthetic-second-model"]["own_usage"] == {
                "input": 100, "cached_input": 20, "cache_write_input": 10,
                "output": 20, "reasoning_output": 2, "total": 120,
            }
            assert child_model_rows["synthetic-second-model"]["estimated_credits"] == "0.00103500"
            assert not child_model_rows["synthetic-boundary-model"]["unpriced_event_statuses"]
            assert not child_model_rows["synthetic-second-model"]["unpriced_event_statuses"]
            grandchild_model_rows = {row["recorded_model"]: row
                                     for row in recorded_model_rows("pilot-grandchild")}
            assert grandchild_model_rows["synthetic-boundary-model"]["unpriced_event_statuses"] == {
                "unknown_mode": 1,
            }
            assert grandchild_model_rows["synthetic-second-model"]["unpriced_event_statuses"] == {
                "no_effective_rate": 1,
            }
            assert grandchild_model_rows["model-without-rate"]["unpriced_event_statuses"] == {
                "unknown_model": 1,
            }
            assert all(grandchild_model_rows[model]["estimated_credits"] is None for model in (
                "synthetic-boundary-model", "synthetic-second-model", "model-without-rate",
            ))

            # Boundary proof: same model is valued under both fixture rate versions.
            standard_boundary_events = [event for event in exported
                                        if event["session_id"] == "pilot-child"
                                        and event.get("model") == "synthetic-boundary-model"
                                        and event.get("service_mode") == "standard"]
            boundary_values = {event["timestamp"][:10]: credits_for_event(event)[0]
                               for event in standard_boundary_events}
            assert set(boundary_values) == {"2026-10-04", "2026-10-06"}
            assert boundary_values["2026-10-04"] != boundary_values["2026-10-06"]
            standard_after = boundary_values["2026-10-06"]
            fast_event = next(event for event in exported
                              if event["session_id"] == "pilot-child"
                              and event.get("model") == "synthetic-boundary-model"
                              and event.get("service_tier") == "priority")
            assert fast_event.get("service_mode") == "fast"
            fast_after, fast_status = credits_for_event(fast_event)
            assert fast_status is None and standard_after != fast_after

            report = []
            for session_id, entry in ROSTER.items():
                node = nodes.get(session_id, {})
                model_rows = recorded_model_rows(session_id)
                own_tokens = (dict.fromkeys(FIELDS, 0) if model_rows else None)
                if model_rows:
                    for model_row in model_rows:
                        for field in FIELDS:
                            own_tokens[field] += model_row["own_usage"][field]
                    assert own_tokens == own.get(session_id, dict.fromkeys(FIELDS, 0))
                else:
                    assert session_id not in own
                report.append({
                    "id": session_id, "parent_id": entry["parent_id"],
                    "configured_model": entry["configured_model"],
                    "recorded_models": sorted(per_model[session_id]),
                    "recorded_modes": sorted(per_mode[session_id]),
                    "recorded_tiers": sorted(per_tier[session_id]),
                    "provider_effective_model": "unavailable",
                    "usage_status": ("recorded" if model_rows else "no_recorded_usage"),
                    "usage_recorded": session_id in own,
                    "own_usage_by_recorded_model": model_rows,
                    "own_usage": own_tokens,
                    "tree_usage": node.get("subtree_usage") if node else own_tokens,
                    "estimated_credits": aggregate_model_credits(session_id, model_rows),
                    "unpriced_event_statuses": dict(unpriced[session_id]),
                })
            roster_by_id = {row["id"]: row for row in report}
            idle_child = roster_by_id["pilot-idle-child"]
            assert idle_child["usage_status"] == "no_recorded_usage"
            assert idle_child["usage_recorded"] is False
            assert idle_child["estimated_credits"] is None
            assert idle_child["own_usage"] is None
            assert idle_child["tree_usage"] is None
            unknown_id_model_rows = recorded_model_rows("pilot-unrostered-session")
            unknown_id_usage_status = (
                "recorded" if any(row["own_usage"]["total"] > 0 for row in unknown_id_model_rows)
                else "no_recorded_usage"
            )
            assert unknown_id_usage_status == "recorded"
            assert idle_child["usage_status"] != unknown_id_usage_status

            root_tree = nodes["pilot-root"]["subtree_usage"]
            expected_tree = dict.fromkeys(FIELDS, 0)
            for session_id in ("pilot-root", "pilot-child", "pilot-grandchild"):
                for field in FIELDS:
                    expected_tree[field] += own[session_id][field]
            assert all(int(root_tree.get(field, 0)) == expected_tree[field] for field in FIELDS)
            assert all(int(nodes["pilot-child"]["subtree_usage"].get(field, 0)) ==
                       own["pilot-child"][field] + own["pilot-grandchild"][field] for field in FIELDS)
            for session_id in ("pilot-root", "pilot-child", "pilot-grandchild"):
                assert all(int(nodes[session_id]["usage"].get(field, 0)) == own[session_id][field]
                           for field in FIELDS)
            assert own["pilot-root"]["total"] < int(root_tree["total"])

            print(json.dumps({
                "candidate_sha": CANDIDATE_SHA, "candidate_version": "2.7.2",
                "rate_basis": "fixture-only synthetic credits_per_1M_tokens selected by recorded model, mode, and UTC effective date; no USD conversion",
                "accounting_label": "fixture-only estimated credits; not a price, billed amount, debit, or provider attestation",
                "roster": report,
                "unknown_ids": [{"id": "pilot-unrostered-session",
                                 "usage_status": unknown_id_usage_status,
                                 "own_usage_by_recorded_model": unknown_id_model_rows,
                                 "own_usage": own["pilot-unrostered-session"],
                                 "recorded_models": sorted(per_model["pilot-unrostered-session"]),
                                 "recorded_modes": sorted(per_mode["pilot-unrostered-session"]),
                                 "recorded_tiers": sorted(per_tier["pilot-unrostered-session"]),
                                 "estimated_credits": str(own_credits["pilot-unrostered-session"].quantize(D("0.00000001")))}],
                "unknown_mode_ids": sorted(unknown_modes),
                "root_tree_usage": root_tree,
            }, sort_keys=True))
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: run.py PATH_TO_VERIFIED_CANDIDATE")
    main(sys.argv[1])
