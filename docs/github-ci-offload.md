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
- `codeql`
  - trigger: PRs, protected branch pushes, schedule, and manual dispatch
  - purpose: authoritative CodeQL code scanning through the checked-in advanced
    setup
  - PR routing: the language router keeps the workflow-level check alive while
    selecting only the CodeQL languages touched by the diff; docs-only or
    unrelated PRs report success through the required gate without starting
    analysis jobs; PR planning uses the base checkout plus GitHub PR file
    metadata instead of fetching contributor-controlled head repositories
  - full-scan fallback: protected branch pushes, schedules, manual dispatch,
    unavailable PR metadata, and edits to CodeQL workflow/config/router
    fixtures run the full Actions, C/C++, JavaScript/TypeScript, Python, and
    Rust matrix
  - not covered: GitHub Code Quality's public-preview dynamic workflow is a
    separate repository setting and may still consume Actions minutes unless it
    is disabled or narrowed in GitHub's Code quality settings
  - retention: ordinary workflow logs plus code-scanning results
- `sedna-branch-build`
  - trigger: manual dispatch only
  - purpose: disposable preview binaries when buildability is the actual question
  - retention: 3 days
  - release visibility: never published as a GitHub Release
- `rust-ci-full`
  - trigger: successful scheduled `rust-ci` workflow completion and manual dispatch
  - purpose: heavyweight Linux `x86_64` Cargo-native checkpoint coverage when broad proof is
    actually needed
  - scheduled dedupe: before heavy jobs start, the workflow checks for a prior
    successful `rust-ci-full` run on the same branch and commit; if found, the
    summary gate reports success and reuses that result instead of rerunning
    the checkpoint
  - artifact reuse: builds one nextest archive for the Linux `x86_64` dev
    profile, then reuses that archive for both the normal all-features test
    lane and the full Docker-backed remote-env lane
  - result signal: uploads a compact `rust-ci-full-summary` JSON artifact with
    job results plus first clippy/nextest blockers
  - cache policy: keep the `sccache` GitHub backend disabled and use explicit
    `.sccache` restore/save steps instead; fallback archives are restore-only
    by default to avoid run-id-keyed cache churn and implicit token writes
  - sccache versioning: use the current installer-managed `sccache` binary so
    the workflow's GitHub cache-service backend setup and the binary's GHA
    contract stay aligned
  - retention: ordinary workflow logs only
- `rust-ci`
  - trigger: routine PRs, manual dispatch, and schedule
  - purpose: required Rust promotion gate with path-aware routing
  - scheduled dedupe: scheduled runs skip the expensive Rust jobs when a prior
    successful `rust-ci` run already exists on the same branch and commit
  - diff source: PR runs use GitHub PR/compare metadata for changed-file
    routing where safe, then fall back to git diff when metadata is unavailable
    or ambiguous
- `sedna-heavy-tests`
  - trigger: manual dispatch, `ci:heavy` PR label, and merge-group checkpoints
  - purpose: expensive Linux-heavy Rust validation without using the local development machine as the
    build factory
  - fanout: smoke and selected lanes now split by `setup_class` so light workflow/docs shards do
    not queue behind heavier Rust runners
  - cache policy: same restore-only fallback archive policy as `rust-ci-full`
  - scopes: `protocol`, `tui`, `cli`, `core`, `workspace`
- `sedna-release`
  - trigger: Sedna release tags or manual dispatch
  - purpose: official public Linux `x86_64` release artifacts
  - release visibility: the only lane that may publish a GitHub Release
- `sedna-sync-upstream`
  - trigger: manual dispatch and scheduled sync
  - purpose: fast-forward `upstream-main` from `upstream/main` and run the
    authoritative downstream divergence audit from the exact synced SHA
  - credential boundary: keep the divergence audit in its own read-only job
    unless a future change deliberately trades that boundary for lower wall
    clock time
- `sync-models-json`
  - trigger: manual dispatch and scheduled sync
  - purpose: keep the local models catalog aligned with upstream when it
    changes
  - credential boundary: normal scheduled comparisons run with `contents: read`;
    write and pull-request permissions are granted only to the PR-creation job
    after the read-only check detects a change

## Operating model

1. Edit locally.
2. Run the smallest relevant local smoke check.
3. Commit and push.
4. Use `validation-lab` for ordinary remote-first validation on `validation/*`, `integration/*`,
   or other non-PR refs.
5. Let `docs-sanity` answer documentation-only changes first instead of manually dispatching
   `validation-lab`.
6. Let `rust-ci` handle routine PR gating; tiny initial PRs and already-green
   PR follow-up pushes may route to incremental targeted validation
   automatically when the relevant diff is small and maps cleanly to one
   guarded seam (a pre-mapped, narrow change boundary the planner can verify
   safely in isolation, such as docs-only, workflow/planner-only, or one
   component seam).
   - PR changed-file routing uses GitHub's PR metadata as a fast path so the
     always-on detector does not need a full repository checkout just to learn
     the diff. Unsafe or incomplete metadata falls back to the git-diff path;
     this is a runtime optimization only, not a coverage reduction.
   - Workflow planning and route-map edits also run cheap planner fixtures so
     the exact-route path stays trustworthy.
   - That light workflow-only route includes the reusable validation-lane
     workflow files plus `.github/validation-lanes.json`, so small CI-only
     follow-ups can stay on planner/workflow proof instead of broad Rust PR
     gates.
7. Use `validation-lab` `profile=targeted` with `lane_set=release` when the question is Linux
   release-build dependency or lockfile readiness under `--locked`.
   - `sedna.release-linux-smoke` is a plain locked release build preflight: it keeps
     Linux build deps and `sccache`, but not DotSlash or release-publish steps.
8. Use `sedna-heavy-tests` only when the change needs labeled PR heavy validation, merge-group
   heavy validation, or a named heavy lane.
9. Use `rust-ci-full` only for scheduled/manual broad Cargo-native checkpoints,
   not as a routine post-merge rerun. Its scheduled path follows the scheduled
   `rust-ci` run and starts only if that upstream gate passed on `main`. Both
   scheduled Rust workflows skip when an equivalent same-branch, same-commit
   success already exists, so idle branches do not spend runner time proving
   the same SHA again.
10. Use `sedna-branch-build` only when you intentionally want a preview binary.
11. Use `sedna-release` only for official releases.

## Current downstream platform policy

- Supported downstream platform: Linux `x86_64`
- Parked but unsupported for now: macOS, Windows, Linux arm64, and other historical upstream targets
- Scheduled and routine heavyweight CI should stay Linux `x86_64` only until Sedna deliberately
  re-enables another platform with matching docs, workflow, and release-policy updates

## Validation ladder

1. Tiny local checks only.
   - `git diff --check`, workflow syntax validation, and the smallest relevant helper-backed smoke
     lane when one is available.
   - Reason: cheapest signal, zero extra GitHub runner pressure.
2. `validation-lab` for normal iterative remote validation.
   - Default to `profile=smoke` or `profile=targeted`.
   - `profile=smoke` fans out the smoke bundle as parallel shards instead of
     running one serial smoke recipe on a single runner.
   - Once a broader `lane_set` run identifies the failing seam, prefer the
     narrowest follow-up rerun that can answer the next question:
     use explicit `lanes=` or the smallest named `lane_set` instead of
     repeating the whole family.
   - The workflow summary now records the profile intent, profile notes, and a
     compact lane-selection summary for operator handoff.
   - Explicit lint lane: `codex.argument-comment-lint` runs the Bazel-backed
     argument-comment check (it verifies required explanatory comments for
     command arguments) as a selectable hosted lane, so comment-lint failures
     can be proven without broad local Rust validation.
   - Reason: best signal per runner-minute without polluting PR surfaces, and
     lower unnecessary compute, carbon, and wait time once the blocker is
     already known.
   - `profile=frontier` now derives a curated blocker-harvest bundle from lane
     metadata and runs it by setup class (`workflow`, `node`, `rust_minimal`,
     `rust_integration`, `release`) so cheap workflow/docs seams can fan out
     harder without letting heavier Rust lanes monopolize the same runner
     budget.
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

### `validation-lab` dispatch rule

`validation-lab` is an intentional downstream operator workflow. Dispatch it from downstream `main`,
then pass the branch, tag, or commit you want to validate via the workflow input `ref`.

Use:

```bash
gh workflow run validation-lab.yml \
  --repo sednalabs/codex \
  --ref main \
  -f ref=<branch-under-test> \
  -f profile=targeted \
  -f lane_set=ui-protocol
```

When the first targeted run has already told you which exact seams are red,
prefer rerunning only those seams:

```bash
gh workflow run validation-lab.yml \
  --repo sednalabs/codex \
  --ref main \
  -f ref=<branch-under-test> \
  -f profile=targeted \
  -f lanes=codex.blocking-waits-targeted,codex.app-server-protocol-test
```

That narrow rerun is the recommended blocker-fix loop. The point of
`validation-lab` is not just remote proof; it is also to let us answer the
next question with the smallest credible hosted slice instead of burning
runner minutes and human wait time on already-known green lanes.

Do not assume `gh workflow run validation-lab --ref <feature-branch> ...` will work. Some downstream
branches intentionally do not carry the latest workflow file, so GitHub may resolve the workflow on
that branch first and return a misleading missing-`workflow_dispatch` error.

### Dirty or orphan local state

If the worktree is dirty, detached, or on an orphan scratch branch, GitHub still
cannot validate it until that exact tree exists on a fetchable remote ref.

Use the snapshot helper:

```bash
scripts/dispatch-validation-lab-snapshot.sh \
  --profile targeted \
  --lanes codex.app-server-protocol-test,codex.app-server-thread-cwd-targeted
```

What it does:

1. Builds a disposable commit from the current local worktree state without
   rewriting the current branch.
2. Pushes that commit to a disposable `validation/snapshot-*` ref on `origin`.
3. Dispatches `validation-lab.yml` from downstream `main` against that pushed
   snapshot ref.

This is the preferred low-friction path when the real question is "prove the
exact local tree remotely" and the branch is not yet in public-PR shape.
Pair it with explicit `--lanes` whenever the blocker is already known so the
snapshot rerun stays as small and cheap as possible.

The target ref still needs to carry the current explicit lane schema and the
lane helper scripts referenced by it. The lab planner no longer backfills the
old implicit `run_command` contract for historical refs.

In earlier revisions, the planner could infer a default command when lane
metadata was missing; that compatibility path has been removed. If you're
replaying an older ref, migrate it to the explicit lane schema used on `main`
(including the referenced lane helper scripts) before dispatching.

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

- `setup_class`: explicit execution bucket: `workflow`, `node`,
  `rust_minimal`, `rust_integration`, or `release`
- `working_directory`: repo-relative cwd for the lane script
- `script_path`: repo-relative executable path used as the workflow truth
- `script_args`: argv list for that script
- `needs_just`, `needs_node`, `needs_nextest`, `needs_linux_build_deps`,
  `needs_dotslash`, `needs_sccache`: explicit setup capabilities
- reusable validation-lane workflows default repository checkout to
  `fetch-depth: 1`; callers only widen that through the explicit
  `checkout_fetch_depth` input when a lane genuinely needs deeper history
- validation-lab also keeps the target checkout shallow for ordinary
  smoke/targeted/frontier runs; it only fetches full target history when
  artifact mode needs merged Sedna tags for preview-version derivation
- reusable validation-lane workflows also resolve shared helper scripts from a
  separate `.workflow-src` checkout at the workflow ref, so older PR heads can
  keep running under newer lane-helper contracts without carrying helper copies
  in the tested checkout
- `needs_sccache` now prefers the GitHub-hosted `sccache` backend when the
  runner exposes the current cache-service environment, and only falls back to
  a local `.sccache` archive when that backend is unavailable
- `rust_minimal` now supports the same `sccache` contract as the heavier Rust
  lane classes, but only the compile-heavy targeted lanes opt into it by
  catalog metadata
- `cache_policy`: Rust-oriented reusable workflows default fallback archives to
  `restore-only`; validation-lab only opts into `write-fallback` for retained
  non-`auto` supersession modes where preserving comparison evidence is
  deliberate
- `frontier_default`: whether the lane belongs in the default `lane_set=all`
  frontier harvest
- `frontier_lane_sets`: named frontier families for non-`all` frontier runs
- `frontier_role`: whether the lane is a family sentinel or a deeper companion
- `summary_family`: the family key used to collapse raw lane failures into one
  primary blocker per family
- `cost_class`: a lightweight signal for relative runner cost

`validation-lab` and `sedna-heavy-tests` both consume this explicit contract.
The checked-in lane scripts are now the workflow source of truth; `just`
remains a convenience layer that some scripts may call, not the planner's
execution primitive.

## Secret Boundary

Reusable validation-lane workflows deliberately separate trusted workflow
helpers from the target checkout being validated. Shared helper scripts come
from the workflow ref through the `.workflow-src` checkout, while lane scripts
come from the target ref selected by the PR, dispatch, or lab input.

Because lane scripts are target-controlled, generic validation lanes must not
receive repository or organization secrets. Keep the `Run requested lane
script` environment limited to routing and execution inputs such as
`WORKING_DIRECTORY`, `SCRIPT_PATH`, and `SCRIPT_ARGS_JSON`. Do not add
`secrets.*`, secret-shaped environment variables, or `secrets: inherit` to the
generic workflow-lane path.

Credentialed integrations belong in one of these narrower places instead:

- a trusted workflow-ref step that does not execute target-checkout scripts
- a protected-branch, scheduled, or post-merge workflow
- a purpose-built reusable workflow with explicit, reviewed secret inputs and a
  trusted execution boundary

For PR and validation-lab runs, prefer unauthenticated cache or service access
over passing credentials into target scripts. Slower hosted validation is a
better tradeoff than making scratch or PR code secret-bearing.

The upstream mirror sync is the exception that proves the rule: only
`sedna-sync-upstream` should receive the upstream mirror write credential,
as defined in `.github/workflows/sedna-sync-upstream.yml`; validation
lanes should audit against read-only refs or read-only fallback state.

## Summary artifact

The top-level `validation-summary` artifact is now family-aware.

It records:

- setup-class job results and started-lane counts
- setup-versus-command timing totals so slow setup paths are visible at the
  workflow summary layer
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
