# GitHub CI Offload

This repository treats GitHub Actions as the default factory for expensive validation and preview
artifacts.

## Lanes

- `validation-lab`
  - trigger: manual dispatch only
  - purpose: remote-first validation for scratch refs, integration refs, orphan-branch experiments,
    and broad targeted sweeps that should not pollute normal PR status surfaces
  - frontier model: `targeted` keeps the full named seam, while `frontier` uses curated lane
    metadata and split setup-class fanout instead of reusing the same lane selection with
    `fail-fast=false`
  - operator signal: metadata now records `profile_intent`, `profile_notes`, and a compact
    `lane_summary` so watchers can understand the selection without reopening the planner
  - retention: summary plus any requested preview artifacts
- `docs-sanity`
  - trigger: pushes and PRs that touch `README.md`, `docs/**`, or its own checker wiring
  - purpose: cheap markdown-link proof whenever documentation moves, without widening into validation-lab
  - retention: ordinary workflow logs only
- `sedna-branch-build`
  - trigger: manual dispatch only
  - purpose: disposable preview binaries when buildability is the actual question
  - retention: 3 days
  - release visibility: never published as a GitHub Release
- `rust-ci-full`
  - trigger: scheduled hygiene sweeps and manual dispatch
  - purpose: heavyweight Cargo-native checkpoint coverage when broad proof is
    actually needed
  - retention: ordinary workflow logs only
- `sedna-heavy-tests`
  - trigger: manual dispatch, `ci:heavy` PR label, and merge-group checkpoints
  - purpose: expensive Linux-heavy Rust validation without using the shared local machine as the
    build factory
  - fanout: smoke and selected lanes now split by `setup_class` so light workflow/docs shards do
    not queue behind heavier Rust runners
  - scopes: `protocol`, `tui`, `cli`, `core`, `workspace`
- `sedna-release`
  - trigger: Sedna release tags or manual dispatch
  - purpose: official public release artifacts
  - release visibility: the only lane that may publish a GitHub Release
- `sedna-sync-upstream`
  - trigger: manual dispatch and scheduled sync
  - purpose: fast-forward `upstream-main` from `upstream/main` and run the authoritative downstream divergence audit from the exact synced SHA

## Operating model

1. Edit locally.
2. Run the smallest relevant local Build Helper smoke check.
3. Commit and push.
4. Use `validation-lab` for ordinary remote-first validation on `validation/*`, `integration/*`,
   or other non-PR refs.
5. Let `docs-sanity` answer documentation-only changes first instead of manually dispatching
   `validation-lab`.
6. Let `rust-ci` handle routine PR gating; tiny initial PRs and already-green
   PR follow-up pushes may route to incremental targeted validation
   automatically when the relevant diff is small and maps cleanly to one
   guarded seam.
   - Workflow planning and route-map edits also run cheap planner fixtures so
     the exact-route path stays trustworthy.
7. Use `validation-lab` `profile=targeted` with `lane_set=release` when the question is Linux
   release-build dependency or lockfile readiness under `--locked`.
8. Use `sedna-heavy-tests` only when the change needs labeled PR heavy validation, merge-group
   heavy validation, or a named heavy lane.
9. Use `rust-ci-full` only for scheduled/manual broad Cargo-native checkpoints,
   not as a routine post-merge rerun.
10. Use `sedna-branch-build` only when you intentionally want a preview binary.
11. Use `sedna-release` only for official releases.

## Validation ladder

1. Tiny local checks only.
   - `git diff --check`, workflow syntax validation, and the smallest relevant Build Helper smoke
     lane.
   - Reason: cheapest signal, zero extra GitHub runner pressure.
2. `validation-lab` for normal iterative remote validation.
   - Default to `profile=smoke` or `profile=targeted`.
   - `profile=smoke` fans out the smoke bundle as parallel shards instead of
     running one serial smoke recipe on a single runner.
   - The workflow summary now records the profile intent, profile notes, and a
     compact lane-selection summary for operator handoff.
   - Reason: best signal per runner-minute without polluting PR surfaces.
   - `profile=frontier` now derives a curated blocker-harvest bundle from lane
     metadata and runs it by setup class (`light`, `rust`, `heavy`) so cheap
     workflow/docs seams can fan out harder without letting heavier Rust lanes
     monopolize the same runner budget.
3. `validation-lab` broad/full only when the question is broader.
   - Use `profile=broad` or `profile=full` only when multiple seams are moving or you need a
     deliberate soak.
   - Reason: these runs are expensive and should answer a specific question.
4. PR checks once the branch is promotion-ready.
   - `rust-ci` and path-aware heavy lanes are the formal promotion gate, not the exploratory lab.
5. Docs-only proof should stay cheap.
   - Use `docs-sanity` for relative markdown-link proof on documentation-only changes instead of
     widening into lab or PR-heavy validation.
6. Preview/buildability validation only at deliberate checkpoints.
   - Use `sedna-branch-build`, `validation-lab` artifact mode, merge-group, or `main` promotion
     when the question is shipping/buildability rather than seam correctness.
   - Use `validation-lab` `lane_set=release` when the question is specifically the Linux
     `cargo build --locked --release` path without needing packaging or publishing.

## Workflow replacement matrix

| Workflow | Status | Sedna role |
| --- | --- | --- |
| `rust-ci.yml` | rewrite in place | Stable required Rust CI for PRs with guarded incremental follow-ups |
| `rust-ci-full.yml` | keep but narrow | Scheduled/manual Cargo-native checkpoint workflow |
| `ci.yml` | rewrite in place | JS/docs/root checks on the Sedna branch model |
| `cargo-deny.yml` | keep with new branch topology | Dependency policy on `main` and `upstream-main` |
| `codespell.yml` | keep with new branch topology | Fast text hygiene on `main` and `upstream-main` |
| `docs-sanity.yml` | new | Cheap docs-only markdown link proof |
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

## Frontier metadata

`validation-lab` frontier planning now relies on per-lane metadata in
`.github/validation-lanes.json`.

The important fields are:

- `frontier_default`: whether the lane belongs in the default `lane_set=all`
  frontier harvest
- `frontier_lane_sets`: named frontier families for non-`all` frontier runs
- `setup_class`: runner-cost bucket used for split fanout and per-class
  parallelism
- `frontier_role`: whether the lane is a family sentinel or a deeper companion
- `summary_family`: the family key used to collapse raw lane failures into one
  primary blocker per family
- `cost_class`: a lightweight signal for relative runner cost

When the requested validation target predates these fields, the host workflow
derives them deterministically so dispatching `validation-lab` from downstream
`main` can still validate older refs truthfully.

## Summary artifact

The top-level `validation-summary` artifact is now family-aware.

It records:

- setup-class job results and started-lane counts
- `primary_blockers`: one strongest active blocker per family, plus setup-class
  startup failures when no lanes in that class ever started
- `secondary_findings`: the remaining cancelled or missing depth lanes
- `candidate_next_slices`: the watcher-facing next queue derived from those
  blockers instead of a flat raw failed-lane list

## Bootstrap limitation

- GitHub's `gh workflow run` path can only dispatch workflows that already exist on the default
  branch.
- That means a brand-new dispatch-only workflow such as `validation-lab.yml` cannot bootstrap its
  own first remote run from a scratch or integration branch before the workflow is merged to
  `main`.
- During rollout, use an existing manual-dispatch workflow such as `sedna-heavy-tests.yml` as the
  bootstrap validator for the branch that introduces the new workflow.
- Be aware that `sedna-heavy-tests.yml` still uses a coarse concurrency group keyed only by
  workflow plus ref, so same-ref manual lanes serialize or cancel rather than running truly in
  parallel.
- The finer-grained `validation-lab` concurrency key (`ref + profile + lane set + explicit lanes`)
  is what unlocks parallel scratch/integration validation once that workflow exists on `main`.
