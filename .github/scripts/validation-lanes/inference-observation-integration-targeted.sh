#!/usr/bin/env bash
set -euo pipefail

cd codex-rs

run_test() {
  local package="$1"
  local test_target="$2"
  local selector="$3"

  RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}" cargo nextest run --locked \
    -p "${package}" \
    --test "${test_target}" \
    --no-tests=fail \
    -E "test(=${selector})"
}

run_test codex-core all 'suite::inference_observations::http_retry_emits_distinct_failed_and_completed_attempts'
run_test codex-core all 'suite::inference_observations::websocket_retry_emits_distinct_failed_and_completed_attempts'
run_test codex-core all 'suite::inference_observations::interrupting_pending_http_response_emits_cancelled_without_completion_evidence'
run_test codex-core all 'suite::inference_auth_recovery::http_401_auth_recovery_records_distinct_attempts'
run_test codex-core all 'suite::inference_auth_recovery::websocket_401_auth_recovery_records_distinct_attempts'
run_test codex-core all 'suite::inference_observation_persistence::detached_delivery_persists_whole_event_pairs_in_both_history_modes'
run_test codex-core all 'suite::inference_observation_persistence::cancelling_real_http_setup_persists_started_then_cancelled'
