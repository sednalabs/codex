# Workflow Strategy

The workflows in this directory are split so that pull requests get fast,
review-friendly signal while heavyweight Cargo-native coverage stays on
deliberate schedule/manual checkpoints instead of re-running after every merge.

## Pull Requests

- `bazel.yml` is the main pre-merge verification path for Rust code.
  It runs Bazel `test` and Bazel `clippy` on the supported Bazel targets.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes

The PR workflow still keeps the Linux lint lane on the default-targets-only invocation for now, but the released linter runs on Linux, macOS, and Windows before merge.

## `main` And Checkpoints

- `bazel.yml` also runs on pushes to `main`.
  This re-verifies the merged Bazel path and helps keep the BuildBuddy caches warm.
- `rust-ci-full.yml` is the full Cargo-native verification workflow.
  It keeps the heavier checks off the PR path and runs only on deliberate
  checkpoints:
  - scheduled hygiene sweeps
  - manual dispatch when a broad Cargo-native proof is actually needed
  - not every ordinary push to `main`
  It still covers:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`
  - Linux remote-env tests

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Reserve `rust-ci-full.yml` for heavyweight Cargo-native coverage that Bazel
  does not replace yet, not for routine post-merge reruns.
