#!/usr/bin/env bash
set -euo pipefail

export RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}"
export CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}"

evidence_dir="$(mktemp -d)"
trap 'rm -rf -- "${evidence_dir}"' EXIT
test_index=0

run_exact() {
  local package="$1"
  local target_kind="$2"
  local target_name="$3"
  local test_name="$4"
  local log_file="${evidence_dir}/test-${test_index}.log"
  local -a target_args
  test_index=$((test_index + 1))

  case "${target_kind}" in
    lib)
      target_args=(--lib)
      ;;
    test)
      target_args=(--test "${target_name}")
      ;;
    *)
      echo "unsupported target kind: ${target_kind}" >&2
      return 64
      ;;
  esac

  echo "::group::${package} ${target_kind}:${target_name} ${test_name}"
  set +e
  cargo test --locked -p "${package}" "${target_args[@]}" "${test_name}" -- \
    --exact --test-threads=1 2>&1 | tee "${log_file}"
  local cargo_status="${PIPESTATUS[0]}"
  set -e
  echo "::endgroup::"

  if [[ "${cargo_status}" -ne 0 ]]; then
    echo "exact test failed: ${package} ${target_kind}:${target_name} ${test_name}" >&2
    return "${cargo_status}"
  fi
  if [[ "$(grep -Ec 'test result: ok\. 1 passed; 0 failed;' "${log_file}")" -ne 1 ]]; then
    echo "exact test did not execute exactly one passing test: ${package} ${target_kind}:${target_name} ${test_name}" >&2
    return 65
  fi
}

core_lib_tests=(
  'session::input_queue::tests::native_activity_snapshot_has_a_stable_generation_boundary'
  'session::input_queue::tests::native_activity_subscription_serializes_snapshot_and_publication'
  'session::input_queue::tests::cancelling_native_snapshot_releases_publication_boundary'
  'session::input_queue::tests::input_queue_notifies_mailbox_subscribers'
  'session::input_queue::tests::input_queue_notifies_steer_subscribers'
  'session::input_queue::tests::input_queue_notifies_terminal_completion_subscribers'
  'session::input_queue::tests::input_queue_deduplicates_terminal_completion_instance_ids'
  'tools::handlers::multi_agents_v2::wait::tests::native_wait_ignores_queue_only_mailbox_progress'
  'tools::handlers::multi_agents_v2::wait::tests::native_wait_accepts_actionable_target_mailbox_progress'
  'tools::handlers::multi_agents_v2::wait::tests::targetless_native_wait_accepts_any_actionable_mailbox_progress'
  'tools::handlers::multi_agents_v2::wait::tests::native_wait_does_not_replay_the_snapshot_boundary'
  'tools::handlers::multi_agents_v2::wait::tests::completion_rule_distinguishes_any_from_all'
  'tools::handlers::multi_agents_v2::wait::tests::native_wait_all_stays_pending_until_every_target_is_terminal'
  'tools::handlers::multi_agents_v2::wait::tests::native_lease_expiry_and_queue_only_mail_stay_inside_wait'
  'tools::handlers::multi_agents::tests::multi_agent_v2_wait_agent_accepts_explicit_timeout_at_configured_min'
  'guardian::tests::guardian_review_request_layout_matches_model_visible_request_snapshot'
  'guardian::tests::guardian_reuses_prompt_cache_key_and_appends_prior_reviews'
)

for test_name in "${core_lib_tests[@]}"; do
  run_exact codex-core lib '' "${test_name}"
done

run_exact codex-core test all 'suite::scenarios::multi_agent_catalog_parameters'
run_exact codex-tui lib '' \
  'app::tests::navigation_reconnect::reconnect_daemon_command_center_after_socket_replacement_without_a_conversation'

echo "Executed ${test_index} exact tests successfully."
