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

run_lib codex-api 'sse::responses::tests::parses_items_and_completed'
run_lib codex-core 'session::tests::turn_inference_identity_captures_final_request_service_tier'
run_lib codex-core 'session::tests::real_turn_construction_publishes_configured_and_request_identity'
run_lib codex-core 'session::tests::configured_inference_identity_preserves_original_model_alias_and_fallback'
run_lib codex-core 'client_tests::pending_setup_delivers_started_then_cancelled_while_sink_is_blocked'
run_lib codex-core 'client_tests::websocket_fallback_records_distinct_attempts_and_http_completion_evidence'
run_lib codex-core 'client_tests::websocket_completion_records_observed_identity_and_exact_usage'
run_lib codex-core 'client_tests::dropping_pending_websocket_setup_records_cancellation'
run_lib codex-core 'client_tests::websocket_trace_uses_concrete_request_except_after_untraced_warmup'
