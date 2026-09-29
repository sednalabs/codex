# Workflow Strategy

The workflows in this directory are split so that pull requests get fast, review-friendly signal while `main` still gets the full cross-platform verification pass.

## Pull Requests

- Required checks run against GitHub's synthetic merge commit, not the pull
  request head alone. This includes changes already on `main` and catches
  conflicts before they reach the branch.
- `bazel.yml` is the main pre-merge verification path for Rust code.
  It runs Bazel `test` and Bazel `clippy` on the supported Bazel targets,
  including the generated Rust test binaries needed to lint inline `#[cfg(test)]`
  code.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes

## Post-Merge On `main`

- `bazel.yml` also runs on pushes to `main`.
  This re-verifies the merged Bazel path and helps keep the BuildBuddy caches warm.
- `rust-ci-full.yml` is the full Cargo-native verification workflow.
  It keeps the heavier checks off the PR path while still validating them after merge:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix via per-platform archive-backed shards
  - Windows ARM64 nextest archives cross-compiled on Windows x64, then replayed on native Windows ARM64 shards
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`
  - Linux remote-env tests
## Security Scanning

- `codeql.yml` is the maintained advanced CodeQL setup for this repository.
  Keep the checked-in workflow authoritative so language coverage, query
  selection, permissions, and scheduling remain reviewable with the rest of the
  workflow catalog.
- Protected branch pushes, pull requests, scheduled runs, and manual dispatch
  analyze Actions, C/C++, JavaScript/TypeScript, Python, and Rust with
  `build-mode: none`. This keeps coverage over the vendored C sandbox code and
  Rust sources without relying on CodeQL autobuild, which has no useful build
  system to discover in this repository.
- Pull requests and merge-queue runs use `classify_ci_paths.py` to select the
  affected CodeQL languages from changed paths. If base checkout or path
  classification fails, the planner falls back to the full language matrix.
  Protected branch pushes, schedules, and manual dispatches retain full
  repository coverage, so path-scoped PR analysis does not replace the
  authoritative branch scans.
- The workflow uses `.github/codeql/codeql-config.yml` for shared CodeQL
  settings, `.github/codeql/codeql-actions.yml` for Actions-only
  query additions, and `.github/codeql/codeql-rust.yml` for Rust-specific
  contract checks. The
  Actions lane prepares a runtime config so same-repository pull requests can
  validate checked-out query-pack changes, while fork pull requests use the
  trusted-base copy of `.github/codeql/actions-workflow-security` when it is
  available. Rust lanes add `.github/codeql/rust-computer-use-contract`
  to catch native computer-use image-content regressions, including missing
  native-image guards, advisory text-vs-image match handling smells, and
  contradictory success-with-error response construction. The
  `codeql-query-tests.yml` workflow compiles that Rust contract pack and runs
  its fixtures when the pack changes; code-scanning still provides the
  repository-wide analysis surface. Add Actions workflow policy queries to the
  Actions pack, Rust semantic contract queries to the Rust pack, and
  language-neutral CodeQL settings to the shared config.
- The CodeQL config deliberately uses the broad `security-and-quality` suite
  and the local threat model. This is noisier than the default or
  `security-extended` suite, but it is the maintained built-in shape that gives
  this project the widest CodeQL signal, including local files, command-line
  arguments, environment variables, and standard input as taint sources where
  CodeQL supports them.
- Rust CodeQL currently uses no-build analysis through `rust-analyzer`. The
  workflow prepares that lane by installing the checked-in Rust toolchain
  channels with only `rust-src`, restoring Cargo registry/git caches, and
  prefetching the Rust workspaces before CodeQL initializes. CodeQL's native
  dependency cache runs in restore-only mode on PRs and restore/store mode on
  protected branch or scheduled runs. Do not cache Rust toolchain executables or
  pass normal Cargo `target/`, test binaries, or nextest archives into CodeQL;
  they are compiled outputs, not the source extraction data CodeQL needs.
- CodeQL findings are evaluated by GitHub code scanning itself. A successful
  repository-owned analysis summary is not a dismissal or proof that the
  separate code-scanning result is clean; the applicable ruleset must require
  and report that result independently before protected landing.
- When a pull request closes, `cancel-pr-runs.yml` cancels active PR-scoped
  workflow runs for that PR. Merged PRs still get the authoritative post-merge
  CodeQL scan from the `main` push; the canceller deliberately leaves protected
  branch push runs alone so the branch-tip result is not hidden by stale PR
  evidence.
- `codeql.yml` intentionally avoids workflow-level concurrency. Rapid PR
  updates and protected-branch pushes can start their own CodeQL runs instead of
  waiting behind an older same-ref run. Closed PR cleanup remains owned by
  `cancel-pr-runs.yml`.
- If GitHub creates a generated CodeQL/default setup workflow, disable that
  duplicate after this advanced workflow is green. Running both creates
  confusing check surfaces and can hide which CodeQL configuration is actually
  producing alerts.
- GitHub Code Quality is deliberately disabled for this repository. The
  checked-in coverage-test workflow generates coverage reports as a test
  guardrail, but it does not grant `code-quality: write` or upload those
  reports to the product. Keep that boundary separate from the maintained
  CodeQL security workflow and its required gate.

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Reserve `rust-ci-full.yml` for heavyweight Cargo-native coverage that Bazel does not replace yet.
