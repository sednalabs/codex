set working-directory := "codex-rs"
set positional-arguments

# Display help
help:
    just -l

# `codex`
alias c := codex
codex *args:
    cargo run --bin codex -- "$@"

# `codex exec`
exec *args:
    cargo run --bin codex -- exec "$@"

# Run the CLI version of the file-search crate.
file-search *args:
    cargo run --bin codex-file-search -- "$@"

# Build the CLI and run the app-server test client
app-server-test-client *args:
    cargo build -p codex-cli
    cargo run -p codex-app-server-test-client -- --codex-bin ./target/debug/codex "$@"

# format code
fmt:
    cargo fmt -- --config imports_granularity=Item 2>/dev/null

fix *args:
    cargo clippy --fix --tests --allow-dirty "$@"

clippy:
    cargo clippy --tests "$@"

install:
    rustup show active-toolchain
    cargo fetch

# Run `cargo nextest` since it's faster than `cargo test`, though including
# --no-fail-fast is important to ensure all tests are run.
#
# Run `cargo install cargo-nextest` if you don't have it installed.
# Prefer this for routine local runs; use explicit `cargo test --all-features`
# only when you specifically need full feature coverage.
test:
    cargo nextest run --no-fail-fast

# Fast smoke checks for fragile codex-core integration buckets.
core-test-smoke:
    set -euo pipefail
    export CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}"
    cargo test -p codex-core --test all suite::rmcp_client::stdio_server_round_trip -- --exact --test-threads=1
    cargo test -p codex-core --test all suite::code_mode::code_mode_exports_all_tools_metadata_for_namespaced_mcp_tools -- --exact --test-threads=1
    cargo test -p codex-core --test all suite::plugins::plugin_mcp_tools_are_listed -- --exact --test-threads=1
    cargo test -p codex-core --test all suite::truncation::mcp_tool_call_output_exceeds_limit_truncated_for_model -- --exact --test-threads=1
    cargo test -p codex-core --test all suite::client::usage_limit_error_emits_rate_limit_event -- --exact --test-threads=1
    cargo test -p codex-core --test all suite::client_websockets::responses_websocket_usage_limit_error_emits_rate_limit_event -- --exact --test-threads=1

# Progressive codex-core ladder:
# 1) smoke gate, 2) high-churn buckets, 3) full suite.
core-test-progressive:
    set -euo pipefail
    export CODEX_JS_REPL_NODE_PATH="${CODEX_JS_REPL_NODE_PATH:-/tmp/codex-node22/bin/node}"
    just core-test-smoke
    cargo test -p codex-core --test all suite::rmcp_client:: -- --test-threads=1
    cargo test -p codex-core --test all suite::code_mode:: -- --test-threads=1
    cargo test -p codex-core --test all suite::truncation:: -- --test-threads=1
    cargo test -p codex-core --test all suite::plugins:: -- --test-threads=1
    CARGO_BUILD_JOBS=1 cargo test -p codex-core -j1 -- --test-threads=1

# Build and run Codex from source using Bazel.
# Note we have to use the combination of `[no-cd]` and `--run_under="cd $PWD &&"`
# to ensure that Bazel runs the command in the current working directory.
[no-cd]
bazel-codex *args:
    bazel run //codex-rs/cli:codex --run_under="cd $PWD &&" -- "$@"

[no-cd]
bazel-lock-update:
    bazel mod deps --lockfile_mode=update

[no-cd]
bazel-lock-check:
    ./scripts/check-module-bazel-lock.sh

bazel-test:
    bazel test //... --keep_going

bazel-remote-test:
    bazel test //... --config=remote --platforms=//:rbe --keep_going

build-for-release:
    bazel build //codex-rs/cli:release_binaries --config=remote

# Run the MCP server
mcp-server-run *args:
    cargo run -p codex-mcp-server -- "$@"

# Regenerate the json schema for config.toml from the current config types.
write-config-schema:
    cargo run -p codex-core --bin codex-write-config-schema

# Regenerate vendored app-server protocol schema artifacts.
write-app-server-schema *args:
    cargo run -p codex-app-server-protocol --bin write_schema_fixtures -- "$@"

[no-cd]
write-hooks-schema:
    cargo run --manifest-path ./codex-rs/Cargo.toml -p codex-hooks --bin write_hooks_schema_fixtures

# Run the argument-comment Dylint checks across codex-rs.
[no-cd]
argument-comment-lint *args:
    ./tools/argument-comment-lint/run.sh "$@"

# Tail logs from the state SQLite database
log *args:
    if [ "${1:-}" = "--" ]; then shift; fi; cargo run -p codex-state --bin logs_client -- "$@"
