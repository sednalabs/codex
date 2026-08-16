## Installing & building

### System requirements

| Requirement                 | Details                                                                         |
| --------------------------- | ------------------------------------------------------------------------------- |
| Operating systems           | Linux `x86_64` or Arm64 GNU (Ubuntu 20.04+/Debian 10+ recommended); Intel macOS |
| Git (optional, recommended) | 2.23+ for built-in PR helpers                                                   |
| RAM                         | 4-GB minimum (8-GB recommended)                                                 |

Hardened Linux asset verification uses Cosign for keyless binary signatures and GitHub CLI for
build attestations. The public post-release verifier provisions Cosign and passes
`--verify-signatures --verify-attestation`; external deployment automation should do the same
after provisioning those tools. The default x86 installer path retains compatibility with
historical release assets that predate the SBOM and attestation contract.

The supported downstream Linux install and release targets are `x86_64` and Arm64 GNU. Intel macOS `x86_64` is also supported by the release contract. Other upstream platform paths remain in the repository for future re-enablement, but Sedna does not currently publish or validate them as supported targets.

### DotSlash

The GitHub Release also contains a [DotSlash](https://dotslash-cli.com/) file for the Codex CLI named `codex`. Using a DotSlash file makes it possible to make a lightweight commit to source control to ensure all contributors use the same version of an executable, regardless of what platform they use for development.

### Build from source

```bash
# Clone the repository and navigate to the root of the Cargo workspace.
git clone https://github.com/sednalabs/codex.git
cd codex/codex-rs

# Install the Rust toolchain, if necessary.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup component add rustfmt
rustup component add clippy
# Install helper tools used by the workspace justfile:
cargo install --locked just
# DotSlash fetches pinned development tools such as buildifier on first use.
cargo install --locked dotslash
# Install nextest for the `just test` helper.
cargo install --locked cargo-nextest

# Build Codex.
cargo build

# Launch the TUI with a sample prompt.
cargo run --bin codex -- "explain this codebase to me"

# After making changes, use the root justfile helpers (they default to codex-rs):
just fmt
just fix -p <crate-you-touched>

# Run the relevant tests (project-specific is fastest), for example:
just test -p codex-tui
# `just test` runs the test suite via nextest:
just test
# Avoid `--all-features` for routine local runs because it increases build
# time and `target/` disk usage by compiling additional feature combinations.
```

## Local usage database

Recent downstream builds also create a dedicated usage database under
`CODEX_SQLITE_HOME` alongside the existing state and logs databases:

- `state.sqlite`
- `logs.sqlite`
- `usage.sqlite`

`usage.sqlite` is the authoritative local store for thread lineage, spawn
metadata, tool calls, provider-call usage, quota snapshots, and fork snapshots.
It exists so downstream accounting can consume exact local facts instead of
reconstructing billing from copied rollout transcript history.

If `CODEX_SQLITE_HOME` is unset, Codex uses the same default SQLite home rules
described in [`docs/config.md`](./config.md#sqlite-state-db). A quick manual
inspection looks like:

```bash
sqlite3 "${CODEX_SQLITE_HOME:-$HOME/.codex}/usage.sqlite" '.tables'
```

## Tracing / verbose logging

Codex is written in Rust, so it honors the `RUST_LOG` environment variable to configure its logging behavior.

The TUI records diagnostics in bounded local stores by default. Set `log_dir` explicitly to enable a plaintext TUI log for a run:

```bash
codex -c log_dir=./.codex-log
tail -F ./.codex-log/codex-tui.log
```

The non-interactive mode (`codex exec`) defaults to `RUST_LOG=error`, but messages are printed inline, so there is no need to monitor a separate file.

See the Rust documentation on [`RUST_LOG`](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for more information on the configuration options.
