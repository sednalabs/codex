# GitHub CI Offload

This repository treats GitHub Actions as the default factory for expensive validation and preview
artifacts.

## Lanes

- `validation-lab`
  - trigger: manual dispatch only
  - purpose: remote-first validation for scratch refs, integration refs, orphan-branch experiments,
    and broad targeted sweeps that should not pollute normal PR status surfaces
  - retention: summary plus any requested preview artifacts
- `sedna-branch-build`
  - trigger: manual dispatch only
  - purpose: disposable preview binaries when buildability is the actual question
  - retention: 3 days
  - release visibility: never published as a GitHub Release
- `sedna-heavy-tests`
  - trigger: manual dispatch, `ci:heavy` PR label, nightly schedule, and selected `main` pushes
  - purpose: expensive Linux-heavy Rust validation without using the shared local machine as the
    build factory
  - scopes: `protocol`, `tui`, `cli`, `core`, `workspace`
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
4. Use `validation-lab` for ordinary remote-first validation on `validation/*`, `integration/*`,
   or other non-PR refs.
5. Use `sedna-heavy-tests` when the change needs PR/main heavy validation or a named heavy lane.
6. Use `sedna-branch-build` only when you intentionally want a preview binary.
7. Use `sedna-release` only for official releases.

## Validation ladder

1. Tiny local checks only.
   - `git diff --check`, workflow syntax validation, and the smallest relevant Build Helper smoke
     lane.
   - Reason: cheapest signal, zero extra GitHub runner pressure.
2. `validation-lab` for normal iterative remote validation.
   - Default to `profile=smoke` or `profile=targeted`.
   - Reason: best signal per runner-minute without polluting PR surfaces.
3. `validation-lab` broad/full only when the question is broader.
   - Use `profile=broad` or `profile=full` only when multiple seams are moving or you need a
     deliberate soak.
   - Reason: these runs are expensive and should answer a specific question.
4. PR checks once the branch is promotion-ready.
   - `rust-ci` and path-aware heavy lanes are the formal promotion gate, not the exploratory lab.
5. Preview/buildability validation only at deliberate checkpoints.
   - Use `sedna-branch-build`, `validation-lab` artifact mode, merge-group, or `main` promotion
     when the question is shipping/buildability rather than seam correctness.

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
| `validation-lab.yml` | new | Dispatch-only remote validation lab for scratch/integration/orphan refs |
| `sedna-branch-build.yml` | new | Preview binary offload lane |
| `sedna-heavy-tests.yml` | new | Expensive Linux validation lane |
| `sedna-sync-upstream.yml` | new | Mirror maintenance lane |
| `sedna-release.yml` | keep and harden | Official Sedna release publisher |
| `rust-release.yml` | superseded | Upstream release contract, no longer the Sedna publisher |

## Retention and cleanup

- validation-lab summaries persist with the workflow run; requested preview artifacts retain for
  3 days
- branch artifacts retain for 3 days
- release workflow artifacts retain for 3 days in Actions storage
- official GitHub Releases remain until manually removed
- branch and lab artifacts are disposable; delete or ignore them if they are no longer useful

## Bootstrap limitation

- GitHub's `gh workflow run` path can only dispatch workflows that already exist on the default
  branch.
- That means a brand-new dispatch-only workflow such as `validation-lab.yml` cannot bootstrap its
  own first remote run from a scratch or integration branch before the workflow is merged to
  `main`.
- During rollout, use an existing manual-dispatch workflow such as `sedna-heavy-tests.yml` as the
  bootstrap validator for the branch that introduces the new workflow.
