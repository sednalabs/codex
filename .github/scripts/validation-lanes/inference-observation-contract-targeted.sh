#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

run_lib() {
  local package="$1"
  local selector="$2"

  cargo nextest run --locked \
    -p "${package}" \
    --lib \
    --no-tests=fail \
    -E "test(=${selector})"
}

run_lib codex-protocol 'protocol::tests::inference_call_event_round_trips_legacy_wire_shape_and_typescript_contract'
run_lib codex-protocol 'protocol::tests::inference_call_event_bounds_strings_and_noncompleted_evidence'
run_lib codex-rollout-trace 'inference::tests::enabled_attempt_adds_inference_request_header'
run_lib codex-rollout-trace 'inference::tests::observations_use_configured_provider_id_in_both_trace_modes'
run_lib codex-rollout-trace 'inference::tests::enabled_context_records_replayable_inference_attempt'
run_lib codex-rollout-trace 'inference::tests::raw_trace_toggle_preserves_configured_and_requested_identity'
run_lib codex-rollout-trace 'inference::tests::observations_keep_exact_usage_and_distinct_retry_boundaries'
run_lib codex-rollout-trace 'inference_tests::duplicate_terminal_records_return_exactly_one_observation'
run_lib codex-rollout-trace 'inference_tests::concurrent_terminal_race_returns_exactly_one_observation'
run_lib codex-rollout 'policy::tests::inference_call_events_persist_in_legacy_and_paginated_rollouts'
