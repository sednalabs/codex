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

# Verify the exact-head and hosted-runner validation contract.
[no-cd]
check-validation-lanes:
    {{ python }} -m unittest {{ justfile_directory() }}/test_ci_planners.py

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
    bazel mod deps --lockfile_mode=error; if ($LASTEXITCODE -ne 0) { Write-Error "MODULE.bazel.lock is out of date. Run 'just bazel-lock-update' and commit the updated lockfile."; exit 1 }

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

# Validation-only recipes imported from the current hosted harness. This
# wrapper commit is not part of the product candidate.
tui-agent-picker-targeted:
    cargo test -p codex-tui app::tests::open_agent_picker_marks_loaded_threads_open --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::inactive_thread_started_notification_initializes_replay_session --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::session_lifecycle_requests::session_lifecycle_avoids_redundant_subagent_metadata_reads --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::selected_and_resumed_threads_use_server_capability_for_v1_and_v2_children --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::thread_events::tests::thread_event_store_skips_large_replay_irrelevant_notifications --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::thread_events::tests::thread_event_store_tracks_active_turn_lifecycle --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::thread_events::tests::thread_event_store_rebase_preserves_mcp_startup_notifications --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::enqueue_thread_event_does_not_block_when_channel_full --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::side_parent_status_tracks_parent_turn_lifecycle --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::side_parent_status_prioritizes_input_over_approval --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::handle_start_side_seeds_navigation_before_thread_started --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::side_fork_config_is_persistent_and_appends_developer_guardrails --lib -- --exact --test-threads=1
    cargo test -p codex-tui app_server_session::tests::side_fork_skips_parent_title_lookup_but_normal_ephemeral_fork_keeps_it --lib -- --exact --test-threads=1
    cargo test -p codex-tui app_server_session::tests::side_fork_excludes_turns_without_clearing_regular_ephemeral_fork --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_falls_back_to_thread_id_without_usage --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_compact_token_usage_when_present --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_remaining_context_when_known --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_compact_age_when_known --lib -- --exact --test-threads=1
    cargo test -p codex-tui multi_agents::tests::picker_description_includes_model_effort_and_task_when_available --lib -- --exact --test-threads=1

spawn-agent-tool-model-surface-targeted:
    cargo test -p codex-tools spawn_agent_tool_v2_requires_task_name_and_lists_visible_models --lib -- --exact --test-threads=1
    cargo test -p codex-tools spawn_agent_tool_v2_lists_upgradeable_legacy_models --lib -- --exact --test-threads=1

spawn-agent-description-model-surface-targeted:
    CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo test -p codex-core --test all suite::spawn_agent_description::spawn_agent_description_lists_visible_models_and_reasoning_efforts -- --exact --test-threads=1

tui-agent-picker-tree-targeted:
    cargo test -p codex-tui app::tests::open_agent_picker_marks_loaded_threads_open --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::tests::inactive_thread_started_notification_initializes_replay_session --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::agent_navigation::tests::upsert_preserves_running_state_until_closed --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::agent_navigation::tests::picker_tree_prefixes_reflect_nested_agent_paths --lib -- --exact --test-threads=1
    cargo test -p codex-tui app::loaded_threads::tests::finds_loaded_subagent_tree_for_primary_thread --lib -- --exact --test-threads=1

core-subagent-model-pinning-targeted:
    RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --lib -- agent::role::tests::apply_role_preserves_unspecified_keys agent::role::tests::spawn_tool_spec_marks_terminal_babysitter_locked_model_and_reasoning_effort tools::handlers::multi_agents_spec::tests::spawn_agent_tool_v2_requires_task_name_and_lists_visible_models tools::handlers::multi_agents::tests::spawn_agent_reasoning_effort_accepts_empty_support_metadata tools::handlers::multi_agents::tests::spawn_agent_uses_explorer_role_and_preserves_approval_policy tools::handlers::multi_agents::tests::multi_agent_v2_spawn_accepts_child_model_without_backend_assignment tools::handlers::multi_agents::tests::multi_agent_v2_spawn_defaults_builtin_explorer_to_luna tools::handlers::multi_agents::tests::multi_agent_v2_spawn_rejects_child_model_from_different_backend tools::handlers::multi_agents::tests::multi_agent_v2_spawn_fork_turns_all_rejects_agent_type_override tools::handlers::multi_agents::tests::multi_agent_v2_spawn_partial_fork_turns_allows_agent_type_override tools::handlers::multi_agents::tests::multi_agent_v2_spawn_terminal_babysitter_uses_role_locked_model tools::handlers::multi_agents::tests::failed_spawn_keeps_requested_identity_separate_from_terminal_effective_identity tools::handlers::multi_agents::tests::multi_agent_v2_spawn_without_snapshot_omits_unobserved_effective_identity --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --test all -- suite::subagent_notifications::spawn_agent_uses_configured_subagent_defaults suite::subagent_notifications::spawn_agent_preserves_configured_defaults_through_unrelated_role suite::subagent_notifications::spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role suite::subagent_notifications::spawn_agent_role_overrides_requested_model_and_reasoning_settings suite::subagent_notifications::spawn_agent_rejects_reasoning_effort_unsupported_by_role_model --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --test all -- suite::subagent_notifications::spawned_full_history_v2_child_uses_model_precedence_without_dropping_context
    RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}" cargo nextest run -p codex-state -p codex-thread-store --no-fail-fast --no-tests=fail --lib -- extract::tests::turn_context_sets_model_and_reasoning_effort extract::tests::thread_settings_applied_updates_resume_metadata local::read_thread::tests::read_thread_keeps_complete_indexed_identity_during_rollout_overlay thread_metadata_sync::tests::thread_settings_applied_updates_live_metadata types::tests::thread_metadata_patch_round_trips_optional_clears --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --lib -- agent::control::tests::spawn_agent_can_fork_parent_thread_history_with_sanitized_items agent::control::tests::paginated_subagent_fork_cold_resume_preserves_child_settings agent::control::tests::ensure_v2_agent_loaded_reloads_registered_unloaded_agent --exact
    RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}" CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --no-tests=fail --test all -- suite::multi_agent_resume::cold_root_resume_restores_agent_identity_and_reloads_target_on_followup suite::multi_agent_resume::cold_root_resume_restores_agent_identity_and_role_on_followup --exact

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
