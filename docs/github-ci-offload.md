# GitHub CI Offload

This repository treats GitHub Actions as the default factory for expensive validation and preview
artifacts.

## Lanes

- `sedna-branch-build`
  - trigger: same-repo branch pushes and manual dispatch
  - purpose: disposable preview binaries for pushed work
  - retention: 3 days
  - release visibility: never published as a GitHub Release
- `sedna-heavy-tests`
  - trigger: manual dispatch, `ci:heavy` PR label, nightly schedule, and selected `main` pushes
  - purpose: expensive Linux-heavy Rust validation without using the shared local machine as the
    build factory
  - scopes: `protocol`, `tui_app_server`, `cli`, `core`, `workspace`
- `sedna-release`
  - trigger: Sedna release tags or manual dispatch
  - purpose: official public release artifacts
  - release visibility: the only lane that may publish a GitHub Release
- `sedna-sync-upstream`
  - trigger: manual dispatch and scheduled sync
  - purpose: fast-forward `upstream-main` from `upstream/main`

## Operating model

1. Edit locally.
2. Run the smallest relevant local Build Helper smoke check.
3. Commit and push.
4. Use `sedna-branch-build` when you need a preview binary.
5. Use `sedna-heavy-tests` when the change needs broad or expensive validation.
6. Use `sedna-release` only for official releases.

## Workflow replacement matrix

| Workflow | Status | Sedna role |
| --- | --- | --- |
| `rust-ci.yml` | rewrite in place | Stable required Rust CI for PRs and mainline |
| `ci.yml` | rewrite in place | JS/docs/root checks on the Sedna branch model |
| `cargo-deny.yml` | keep with new branch topology | Dependency policy on `main` and `upstream-main` |
| `codespell.yml` | keep with new branch topology | Fast text hygiene on `main` and `upstream-main` |
| `bazel.yml` | keep with new branch topology | Experimental Bazel validation |
| `sdk.yml` | rewrite in place | SDK checks on GitHub-hosted Linux for the Sedna branch model |
| `v8-canary.yml` | rewrite in place | V8 canary validation on `main` and `upstream-main` |
| `sedna-branch-build.yml` | new | Preview binary offload lane |
| `sedna-heavy-tests.yml` | new | Expensive Linux validation lane |
| `sedna-sync-upstream.yml` | new | Mirror maintenance lane |
| `sedna-release.yml` | keep and harden | Official Sedna release publisher |
| `rust-release.yml` | superseded | Upstream release contract, no longer the Sedna publisher |

## Retention and cleanup

- branch artifacts retain for 3 days
- release workflow artifacts retain for 3 days in Actions storage
- official GitHub Releases remain until manually removed
- branch artifacts are disposable; delete or ignore them if they are no longer useful
