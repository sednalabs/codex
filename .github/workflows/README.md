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
  - `argument-comment-lint` on Linux `x86_64`
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes

The downstream PR workflow intentionally stays on Linux `x86_64` only. Historical non-Linux paths
remain in the repository for future re-enablement, but they are not part of the active Sedna CI
contract today.

## `main` And Checkpoints

- `bazel.yml` also runs on pushes to `main`.
  This re-verifies the merged Bazel path and helps keep the BuildBuddy caches warm.
- `rust-ci-full.yml` is the full Cargo-native verification workflow.
  It keeps the heavier checks off the PR path and runs only on deliberate
  checkpoints:
  - chained scheduled hygiene sweeps after the scheduled `rust-ci` gate succeeds
  - manual dispatch when a broad Cargo-native proof is actually needed
  - not every ordinary push to `main`
  It still covers:
  - Linux `x86_64` Cargo `clippy`
  - Linux `x86_64` Cargo `nextest`
  - Linux `x86_64` release-profile Cargo builds
  - Linux `x86_64` `argument-comment-lint`
  - Linux `x86_64` remote-env tests
  The scheduled path deliberately reuses the fast scheduled `rust-ci` run
  instead of racing it on a second cron. For scheduled full runs, duplicate
  fast checks such as format, shear, and argument-comment-lint are skipped
  once `rust-ci` has already passed; manual dispatch still runs the whole
  broad checkpoint directly. The normal and remote-env nextest lanes both
  consume the same uploaded nextest archive so the workflow can compare normal
  and Docker-backed remote behavior without compiling the same test binaries
  twice. The result job
  uploads a compact `rust-ci-full-summary` JSON artifact for failure triage.

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Reserve `rust-ci-full.yml` for heavyweight Cargo-native coverage that Bazel
  does not replace yet, not for routine post-merge reruns.

## Maintenance Workflows

- `sync-models-json.yml` keeps the normal no-op scheduled path read-only. Its
  `check` job fetches and compares upstream metadata with `contents: read`;
  the `create_pr` job receives `contents: write` and `pull-requests: write`
  only when a changed payload needs an automated PR.
- `sedna-sync-upstream.yml` intentionally keeps mirror synchronization and the
  downstream divergence audit in separate jobs. That costs a second checkout,
  but it preserves a smaller credential boundary: the audit job does not
  receive the upstream mirror write token.
- GitHub code scanning currently runs through the repository's active CodeQL
  setup in GitHub. If a checked-in advanced CodeQL workflow is disabled, treat
  it as non-authoritative until it is deliberately re-enabled or removed.
