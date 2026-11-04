set working-directory := "codex-rs"
set positional-arguments
export CODEX_REPO_ROOT := justfile_directory()
export JUST_SHELL := justfile_directory() / "scripts/just-shell.py"
set shell := ["python3", "-c", 'import os, runpy; runpy.run_path(os.environ["JUST_SHELL"], run_name="__main__")']
set windows-shell := ["python", "-c", 'import os, runpy; runpy.run_path(os.environ["JUST_SHELL"], run_name="__main__")']

rust_min_stack := "8388608" # 8 MiB
python := if os_family() == "windows" { "python" } else { "python3" }

# Display help
help:
    just -l

# `codex`
alias c := codex
codex *args:
    cargo run --bin codex -- {args}

# `codex exec`
exec *args:
    cargo run --bin codex -- exec {args}

# Start `codex exec-server` and run codex-tui.
[no-cd]
[positional-arguments]
[unix]
tui-with-exec-server *args:
    {{ justfile_directory() }}/scripts/run_tui_with_exec_server.sh "$@"

# Run the CLI version of the file-search crate.
file-search *args:
    cargo run --bin codex-file-search -- {args}

# Run the standalone code-mode host from source.
code-mode-host *args:
    cargo run --bin codex-code-mode-host -- {args}

# Assemble a local Codex package.
[no-cd]
assemble-codex-package *args:
    {{ python }} {{ justfile_directory() }}/scripts/build_codex_package.py {args}

# Build the CLI and run the app-server test client
app-server-test-client *args:
    cargo build -p codex-cli
    cargo run -p codex-app-server-test-client -- --codex-bin ./target/debug/codex {args}

# Format the justfile, Rust, Bazel/Starlark, Python SDK code, and Python scripts.
fmt:
    @{{ python }} ../scripts/format.py

# Check formatting without modifying files.
fmt-check:
    @{{ python }} ../scripts/format.py --check

fix *args:
    cargo clippy --fix --tests --allow-dirty {args}

clippy *args:
    cargo clippy --tests {args}

[unix]
install:
    rustup show active-toolchain
    cargo fetch

[windows]
install:
    #!powershell.exe -File
    $pwsh = Get-Command pwsh.exe -ErrorAction SilentlyContinue
    if (-not $pwsh) {
        winget install --exact --id Microsoft.PowerShell --source winget --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    rustup show active-toolchain
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo fetch
    exit $LASTEXITCODE

# Run nextest with --no-fail-fast so all tests are run.
#
# Run `cargo install --locked cargo-nextest` if you don't have it installed.
# Prefer this for routine local runs. Workspace crate features are banned, so
# there should be no need to add `--all-features`.
[unix]
test *args:
    RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=local cargo nextest run --no-fail-fast "$@"

[windows]
test *args:
    $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; cargo nextest run --no-fail-fast @($args | Select-Object -Skip 1)

# Run from the repository root so scripts that resolve paths from `cwd` see
# the same layout they use in GitHub Actions.
[no-cd]
test-github-scripts:
    {{ python }} -m unittest discover -s {{ justfile_directory() }}/.github/scripts -p 'test_*.py'

# Run explicit workspace benchmark targets.
bench *args:
    cargo bench --workspace --bench '*' {args}

# Run benchmark targets once to ensure they start successfully.
bench-smoke:
    just bench -- --test

# Run Bazel-backed end-to-end macrobenchmarks with optimized binaries.
bench-e2e:
    # Keep measured binaries comparable to production-style optimized builds.
    bazel test --compilation_mode=opt --cache_test_results=no --test_output=streamed //codex-rs:e2e-benchmarks

# Run Bazel-backed end-to-end macrobenchmarks once per case with release-like
# Rust cfg paths but fastbuild codegen.
bench-e2e-smoke:
    # Avoid optimizer cost because smoke runs only check that benchmarks work.
    # Compile target Rust code through the same release-only cfg paths as opt.
    # Compile exec-platform Rust tools through those release-only cfg paths too.
    bazel test --compilation_mode=fastbuild --@rules_rust//rust/settings:extra_rustc_flag=-Cdebug-assertions=no --@rules_rust//rust/settings:extra_exec_rustc_flag=-Cdebug-assertions=no --cache_test_results=no --test_output=streamed --test_arg=--test //codex-rs:e2e-benchmarks

# Build and run Codex from source using Bazel.
# On Unix, use `[no-cd]` and `--run_under="cd $PWD &&"` to ensure Bazel runs
# the command in the current working directory.
[no-cd]
[unix]
bazel-codex *args:
    bazel run //codex-rs/cli:codex --run_under="cd $PWD &&" -- "$@"

[windows]
bazel-codex *args:
    bazel run //codex-rs/cli:codex --run_under='cd /d "{{ invocation_directory_native() }}" &&' -- @($args | Select-Object -Skip 1)

# Build and run the standalone code-mode host from source using Bazel.
[no-cd]
[unix]
bazel-code-mode-host *args:
    bazel run //codex-rs/code-mode-host:codex-code-mode-host --run_under="cd $PWD &&" -- "$@"

[windows]
bazel-code-mode-host *args:
    bazel run //codex-rs/code-mode-host:codex-code-mode-host --run_under='cd /d "{{ invocation_directory_native() }}" &&' -- @($args | Select-Object -Skip 1)

[no-cd]
bazel-lock-update:
    bazel mod deps --lockfile_mode=update

[no-cd]
[unix]
bazel-lock-check:
    {{ justfile_directory() }}/scripts/check-module-bazel-lock.sh

[windows]
bazel-lock-check:
    bazel mod deps --lockfile_mode=error; if ($LASTEXITCODE -ne 0) { Write-Error "Unable to verify MODULE.bazel.lock; see the Bazel error above. If Bazel reports an out-of-date lockfile, run 'just bazel-lock-update' and commit the updated lockfile."; exit 1 }

bazel-test:
    bazel test --test_tag_filters=-argument-comment-lint //... --keep_going

[no-cd]
[unix]
bazel-clippy:
    bazel_targets="$({{ justfile_directory() }}/scripts/list-bazel-clippy-targets.sh)" && bazel build --config=clippy -- ${bazel_targets}

[no-cd]
[unix]
bazel-argument-comment-lint:
    bazel build --config=argument-comment-lint -- $({{ justfile_directory() }}/tools/argument-comment-lint/list-bazel-targets.sh)

build-for-release:
    bazel build //codex-rs/cli:release_binaries

# Regenerate the json schema for config.toml from the current config types.
write-config-schema:
    cargo run -p codex-config-schema --bin codex-write-config-schema

# Regenerate app-server protocol schemas and the Python SDK derived from them.
write-app-server-schema *args:
    {{ python }} app-server-protocol/scripts/write_schema_fixtures.py {args}

[no-cd]
write-hooks-schema:
    cargo run --manifest-path {{ justfile_directory() }}/codex-rs/Cargo.toml -p codex-hooks --bin write_hooks_schema_fixtures

# Run the argument-comment Dylint checks across codex-rs.
[no-cd]
[unix]
argument-comment-lint *args:
    if [ "$#" -eq 0 ]; then \
      bazel build --config=argument-comment-lint -- $({{ justfile_directory() }}/tools/argument-comment-lint/list-bazel-targets.sh); \
    else \
      {{ justfile_directory() }}/tools/argument-comment-lint/run-prebuilt-linter.py "$@"; \
    fi

[no-cd]
argument-comment-lint-from-source *args:
    {{ python }} {{ justfile_directory() }}/tools/argument-comment-lint/run.py {args}

# Tail logs from the state SQLite database
[unix]
log *args:
    if [ "${1:-}" = "--" ]; then shift; fi; cargo run -p codex-cli --bin logs_client -- "$@"

[windows]
log *args:
    $forwarded_args = @($args | Select-Object -Skip 1); if ($forwarded_args.Count -gt 0 -and $forwarded_args[0] -eq "--") { $forwarded_args = @($forwarded_args | Select-Object -Skip 1) }; cargo run -p codex-cli --bin logs_client -- @forwarded_args

# Focused current-main recipes retained for the validation-lane catalog.
thread-manager-sample-targeted:
    cargo fmt -p codex-thread-manager-sample -- --check
    cargo check --locked -p codex-thread-manager-sample
    TARGETED_CLIPPY_PACKAGES='["codex-thread-manager-sample"]' .github/scripts/run_targeted_clippy.sh

core-config-consumers-targeted:
    cargo fmt -p codex-core-api -p codex-thread-manager-sample -- --check
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" cargo check --locked -p codex-core-api -p codex-thread-manager-sample
    TARGETED_CLIPPY_PACKAGES='["codex-core-api","codex-thread-manager-sample"]' .github/scripts/run_targeted_clippy.sh

external-agent-migration-containment-targeted:
    cargo nextest run --locked -p codex-external-agent-migration --no-fail-fast --no-tests=fail --lib
    cargo nextest run --locked -p codex-app-server --no-fail-fast --no-tests=fail --test all -- suite::v2::external_agent_config::external_agent_memory_import_rejects_stale_symlink_before_workspace_mutation --exact

tui-collab-spawn-identity-targeted:
    cargo test -p codex-tui chatwidget::tests::history_replay::replayed_collab_spawn_terminal_uses_only_explicit_effective_identity --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::history_replay::replayed_historic_terminal_collab_spawn_renders_legacy_identity_as_effective --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::history_replay::replayed_failed_collab_spawn_without_receiver_keeps_requested_identity --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::app_server::live_app_server_collab_spawn_completed_renders_requested_model_and_effort --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::app_server::live_app_server_spawn_completion_does_not_fill_missing_effective_identity_from_metadata --lib -- --exact --test-threads=1

tui-weekly-pacing-status-line-targeted:
    cargo test -p codex-tui chatwidget::tests::status_and_layout::status_line_weekly_limit_renders_pacing_suffixes_from_live_status_line --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::status_and_layout::status_line_weekly_limit_renders_stale_suffix_over_pace_details --lib -- --exact --test-threads=1
    cargo test -p codex-tui chatwidget::tests::status_and_layout::status_line_weekly_limit_omits_pacing_when_inputs_are_missing --lib -- --exact --test-threads=1

core-multi-agent-orchestration-targeted:
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --lib -- tools::handlers::multi_agents::tests::multi_agent_v2_list_agents_returns_completed_status tools::handlers::multi_agents_v2::wait::tests::completion_rule_distinguishes_any_from_all session::input_queue::tests::input_queue_wait_input_allows_terminal_completion_after_mailbox_delivery_deferral agent::control::residency::tests::residency_slot_reservation_unloads_oldest_idle_v2_agent agent::control::residency::tests::interrupted_v2_agent_remains_known_and_reloads_after_residency_eviction agent::control::residency::tests::ephemeral_v2_agent_is_not_evicted_without_reloadable_history agent::control::residency::tests::spawned_v2_terminal_child_unloads_at_terminal_idle_deadline agent::registry::tests::cold_status_text_stays_compact_when_json_escaped agent::registry::tests::spawn_delivery_ownership_prevents_cancellation_from_claiming_abort agent::control::tests::delivery_owned_initial_input_failure_reconciles_and_preserves_the_delivery_error agent::control::tests::ensure_v2_agent_loaded_reloads_registered_unloaded_agent tools::parallel::tests::configured_v2_spawn_cancellation_after_new_thread_reconciles_before_runtime_abort tools::parallel::tests::v1_spawn_cancellation_after_initial_delivery_returns_published_result tools::parallel::tests::configured_v2_spawn_cancellation_after_initial_delivery_returns_published_result context::world_state::multi_agent_mode::tests::custom_mode_removal_replaces_retained_instructions context::world_state::multi_agent_mode::tests::snapshots --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --test all -- suite::spawn_agent_description::spawn_agent_description_lists_visible_models_and_reasoning_efforts suite::agent_execution::v2_evicted_completed_agent_keeps_final_status suite::agent_execution::v2_cold_mailbox_allows_eviction_and_replays_on_followup suite::multi_agent_mode::changing_configured_mode_hint_to_empty_appends_explicit_reset suite::pending_input::queue_only_agent_mail_wakes_sleeping_root_and_persists_message --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test --locked -p codex-exec --test all suite::completion_backfill::ignores_unrelated_turn_completion_before_backfilling_primary_turn -- --exact --test-threads=1
    cargo nextest run -p codex-protocol --lib --no-tests=fail -- protocol::tests::turn_complete_without_provider_usage_remains_compatible --exact
    RUST_MIN_STACK=8388608 CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --test all --no-tests=fail -- suite::safety_check_downgrade::openai_model_header_mismatch_only_emits_one_warning_per_turn suite::safety_check_downgrade::nonterminal_response_identity_is_not_reported_when_follow_up_fails suite::compact_remote::remote_compact_replaces_history_for_followups suite::compact_remote::remote_compact_v2_reuses_compaction_trigger_for_followups suite::pending_input::steered_user_input_follows_compact_when_only_the_steer_needs_follow_up --exact

core-subagent-model-pinning-targeted:
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --lib -- agent::role::tests::apply_role_preserves_unspecified_keys agent::role::tests::spawn_tool_spec_marks_terminal_babysitter_locked_model_and_reasoning_effort tools::handlers::multi_agents_spec::tests::spawn_agent_tool_v2_requires_task_name_and_lists_visible_models tools::handlers::multi_agents::tests::spawn_agent_reasoning_effort_accepts_empty_support_metadata tools::handlers::multi_agents::tests::spawn_agent_uses_explorer_role_and_preserves_approval_policy tools::handlers::multi_agents::tests::multi_agent_v2_spawn_accepts_child_model_without_backend_assignment tools::handlers::multi_agents::tests::multi_agent_v2_spawn_defaults_builtin_explorer_to_luna tools::handlers::multi_agents::tests::multi_agent_v2_spawn_rejects_child_model_from_different_backend tools::handlers::multi_agents::tests::multi_agent_v2_spawn_fork_turns_all_rejects_agent_type_override tools::handlers::multi_agents::tests::multi_agent_v2_spawn_partial_fork_turns_allows_agent_type_override tools::handlers::multi_agents::tests::multi_agent_v2_spawn_terminal_babysitter_uses_role_locked_model tools::handlers::multi_agents::tests::failed_spawn_keeps_requested_identity_separate_from_terminal_effective_identity tools::handlers::multi_agents::tests::multi_agent_v2_spawn_without_snapshot_omits_unobserved_effective_identity --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --test all -- suite::subagent_notifications::spawn_agent_uses_configured_subagent_defaults suite::subagent_notifications::spawn_agent_preserves_configured_defaults_through_unrelated_role suite::subagent_notifications::spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role suite::subagent_notifications::spawn_agent_role_overrides_requested_model_and_reasoning_settings suite::subagent_notifications::spawn_agent_rejects_reasoning_effort_unsupported_by_role_model --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --test all -- suite::subagent_notifications::spawned_full_history_v2_child_uses_model_precedence_without_dropping_context
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" cargo nextest run -p codex-state -p codex-thread-store --no-fail-fast --no-tests=fail --lib -- extract::tests::turn_context_sets_model_and_reasoning_effort extract::tests::thread_settings_applied_updates_resume_metadata local::read_thread::tests::read_thread_keeps_complete_indexed_identity_during_rollout_overlay thread_metadata_sync::tests::thread_settings_applied_updates_live_metadata types::tests::thread_metadata_patch_round_trips_optional_clears --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --lib -- agent::control::tests::spawn_agent_can_fork_parent_thread_history_with_sanitized_items agent::control::tests::paginated_subagent_fork_cold_resume_preserves_child_settings agent::control::tests::ensure_v2_agent_loaded_reloads_registered_unloaded_agent --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --test all -- suite::multi_agent_resume::cold_root_resume_restores_agent_identity_and_reloads_target_on_followup suite::multi_agent_resume::cold_root_resume_restores_agent_identity_and_role_on_followup --exact

app-server-collab-spawn-identity-targeted:
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" cargo test --locked -p codex-app-server --test all suite::v2::turn_start::turn_start_emits_multi_agent_v1_spawn_requested_and_effective_identity_v2 -- --exact --test-threads=1
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" cargo test --locked -p codex-app-server --test all suite::v2::turn_start::turn_start_emits_multi_agent_v1_role_spawn_requested_and_effective_identity_v2 -- --exact --test-threads=1

skill-loader-fixture-hermeticity-targeted:
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" cargo nextest run -p codex-core-skills --lib --no-tests=fail -- loader::tests::non_git_repo_skills_search_does_not_walk_parents loader::tests::skill_roots_include_admin_with_lowest_priority --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" cargo nextest run -p codex-skills-extension --lib --no-tests=fail -- provider::host::tests::host_catalog_entries_carry_their_prompt_scope render::tests::ordering_follows_render_policy render::tests::description_selection_follows_render_policy render::tests::omission_notice_follows_render_policy_and_is_charged_to_catalog_budget render::tests::character_fallback_counts_multibyte_metadata_by_characters render::tests::catalog_report_counts_partial_description_truncation render::tests::catalog_emits_omission_marker_when_every_minimum_skill_line_exceeds_budget render::tests::catalog_preserves_report_when_no_fragment_fits_budget --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-{{ rust_min_stack }}}" cargo nextest run -p codex-skills-extension --test skills_extension --no-tests=fail -- moderate_budget_pressure_keeps_every_catalog_entry extreme_budget_pressure_removes_descriptions_before_omitting_entries --exact
