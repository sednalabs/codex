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

# Validation-only P3 terminal-consumer probe. This recipe belongs only to the
# hosted-proof overlay and is not product source.
p3-terminal-consumer-probe:
    RUST_MIN_STACK={{ rust_min_stack }} CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --lib -- config::schema::tests::config_schema_matches_fixture agent::control_tests::spawn_agent_creates_thread_and_sends_prompt agent::control_tests::send_inter_agent_communication_without_turn_queues_message_without_triggering_turn tools::handlers::multi_agents_tests::multi_agent_v2_spawn_returns_path_and_send_message_accepts_relative_path tools::handlers::multi_agents_tests::multi_agent_v2_wait_agent_returns_summary_for_mailbox_activity tools::handlers::multi_agents_tests::multi_agent_v2_interrupt_agent_accepts_task_name_target tools::handlers::multi_agents_tests::multi_agent_v2_followup_task_completion_notifies_parent_on_every_turn --exact
    RUST_MIN_STACK={{ rust_min_stack }} CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}" cargo nextest run -p codex-core --no-fail-fast --test all -- suite::pending_input::steer_interrupts_wait_agent_and_is_sent_in_follow_up_request suite::subagent_notifications::multi_agent_v2_spawn_sends_agent_message_to_child suite::subagent_notifications::plaintext_multi_agent_v2_completion_sends_agent_message --exact

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
