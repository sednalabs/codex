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
  Its scheduled run first checks for an already-successful `rust-ci` run on
  the same branch and commit. When one exists, the required summary job exits
  green and the expensive Rust jobs stay skipped.

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
  The scheduled path deliberately follows the fast scheduled `rust-ci` run
  instead of racing it on a second cron. It also checks for an already-green
  `rust-ci-full` run on the same branch and commit before starting heavy Cargo
  work. For scheduled full runs that do proceed, duplicate fast checks such as
  format, shear, and argument-comment-lint are skipped once `rust-ci` has
  already passed; manual dispatch still runs the whole broad checkpoint
  directly. The normal and remote-env nextest lanes both consume the same
  uploaded nextest archive so the workflow can compare normal and
  Docker-backed remote behavior without compiling the same test binaries
  twice. The result job
  uploads a compact `rust-ci-full-summary` JSON artifact for failure triage.

## Security Scanning

- `codeql.yml` is the maintained advanced CodeQL setup for this repository.
  Keep the checked-in workflow authoritative so language coverage, query
  selection, permissions, and scheduling remain reviewable with the rest of the
  workflow catalog.
- Protected branch pushes, scheduled runs, and manual dispatch analyze Actions,
  C/C++, JavaScript/TypeScript, Python, and Rust with `build-mode: none`. This
  keeps coverage over the vendored C sandbox code and Rust sources without
  relying on CodeQL autobuild, which has no useful build system to discover in
  this repository.
- Pull requests route the language matrix through
  `.github/scripts/resolve_codeql_plan.py`. The planner checkout uses the base
  commit and the PR diff comes from GitHub's pull-request files API, so the
  privileged planning job does not fetch a contributor-controlled head
  repository. The workflow still starts and ends in a required gate, but
  docs-only or unrelated changes can skip analysis, and language-local changes
  analyze only the relevant CodeQL languages. Unavailable PR metadata and edits
  to the CodeQL workflow, config, router, or planner fixtures fall back to the
  full matrix. If the trusted base checkout does not yet contain the planner,
  the workflow bootstraps with a full matrix instead of reading planner code
  from the PR head.
- The workflow uses `.github/codeql/codeql-config.yml` for shared CodeQL
  settings, `.github/codeql/codeql-actions.yml` for Actions-only query
  additions, and `.github/codeql/codeql-rust.yml` /
  `.github/codeql/codeql-rust-pr.yml` for Rust-specific contract checks. The
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
- GitHub Code Quality is a separate public-preview product that may appear as
  a dynamic `CodeQL` / `Code Quality` workflow at
  `dynamic/github-code-scanning/codeql`. That workflow is controlled from the
  repository's Code quality settings, not by this checked-in workflow. Disable
  or narrow it there if it duplicates the maintained advanced CodeQL setup or
  spends runner minutes unexpectedly.

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
- `cancel-pr-runs.yml` is the closed-PR cleanup hook. It cancels active
  `pull_request` runs associated with the closed PR, plus same-repository
  push runs for the PR head branch when that branch is not a protected branch.
  It does not cancel post-merge `main` or `upstream-main` push runs.
- CodeQL code scanning should run through the checked-in advanced workflow.
  Treat generated/default CodeQL workflows as duplicates once `codeql.yml` is
  enabled and green.
