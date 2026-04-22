# Validation Workflow for Multi-Step Changes

This document describes the contributor-safe validation ladder for hard,
multi-step work in this repository. It keeps the tracked guidance focused on
how to choose the right remote validation depth without encoding local-only
orchestration tactics.

## Core principles

- Work in micro-slices.
- Keep one owner per seam.
- Use remote validation as the default measurement surface.
- Treat preview and release builds as buildability checkpoints, not routine
  inner-loop validation.

## Micro-slice contract

Every slice should answer one explicit question.

A slice is expected to produce:

- one narrow diff
- one atomic commit
- one exact validation run
- one recorded conclusion in the issue, PR, or work item record chosen for the
  change

If a slice cannot be described as "one question, one change, one measurement,"
it is probably too large.

Suggested record fields:

- slice id
- question or invariant
- commit SHA
- validation workflow/profile/lane
- run id
- outcome
- key signal
- next action

Tracked docs should not prescribe a particular operational tracker, role
layout, or orchestration style for that record.

## Validation ladder

Use the smallest validator that can answer the current question.

1. Tiny local sanity checks
   - examples: `git diff --check`, syntax/schema/config validation
   - for documentation-only changes, prefer `docs-sanity` as the first remote proof surface
2. `validation-lab` `profile=smoke`
   - use when the branch changed substantially or the validation wiring changed
3. `validation-lab` `profile=targeted`
   - the default inner-loop path for one active seam
4. `validation-lab` `profile=frontier`
   - use only after a recent trusted smoke or targeted baseline
   - harvest a bounded queue of likely next blockers without running a full
     milestone checkpoint
   - the default `lane_set=all` frontier sweep should come from curated lane
     metadata, not by reusing the full targeted lane set with `fail-fast=false`
5. `validation-lab` `profile=broad`
   - use only after the active seam is green
   - use it to reveal the next interaction-heavy divergence
6. `validation-lab` `profile=full`
   - use only for explicit broader confidence or pre-promotion soak
7. `profile=artifact` or `artifact_build=true`
   - use only when the question is buildability or preview delivery

For Linux release readiness, prefer `validation-lab` `profile=targeted` with
`lane_set=release` when the question is narrow `--locked` release-build drift.
That lane currently resolves to `sedna.release-linux-smoke`, which is a
preflight check only; it does not publish a GitHub Release. Use artifact mode
only when you also need a disposable preview package.

## Snapshot refs for exact-tree remote proof

When the tree you need to validate is not yet available as a clean remote ref
(for example, dirty worktree state, detached HEAD, or scratch history), use a
disposable snapshot ref and validate that exact snapshot remotely.

Preferred pattern:

1. Create/push a disposable `validation/snapshot-*` ref from the exact local
   tree.
2. Dispatch `validation-lab.yml` from downstream `main`.
3. Pass the snapshot branch name via workflow input `ref`.

For the concrete command helper and dispatch examples, use
[`github-ci-offload.md`](github-ci-offload.md) (`validation-lab` dispatch rule
and snapshot helper section).

This remote-proof path is intentionally current-contract only. The validated
ref must carry the current `.github/validation-lanes.json` schema plus the
lane helper scripts the workflows call. Historical refs that predate the
explicit lane contract are no longer supported by the lab planner.

## Fan-out and concurrency

Use matrix fan-out when several lanes answer the same seam question.
Use separate runs only when the questions are genuinely independent.

Default validation-lab policy:

- `targeted`: low fan-out
- `frontier`: bounded fan-out with `fail-fast=false` and split setup-class
  matrices so `workflow`, `node`, `rust_minimal`, `rust_integration`, and
  `release` lanes can scale independently
- `broad`: moderate fan-out
- `full`: conservative fan-out

Do not widen every iteration into a broad or full run.
Get one seam green first, use `frontier` to harvest nearby blockers when the
baseline is trustworthy, and only then widen deliberately.

## Lane catalog contract

The validation planners now consume an explicit lane catalog rather than
deriving execution behavior from an inline command string.

Every lane row in `.github/validation-lanes.json` is expected to define:

- `setup_class`
- `working_directory`
- `script_path`
- `script_args`
- `needs_just`
- `needs_node`
- `needs_nextest`
- `needs_linux_build_deps`
- `needs_dotslash`
- `needs_sccache`

Execution is script-backed:

- reusable workflows fan out by `setup_class`
- each lane runs the checked-in script referenced by `script_path`
- the root `justfile` is a convenience layer, not the workflow source of truth
- lanes that still rely on `just` now declare that explicitly via `needs_just`
  and can call a small wrapper script instead of embedding `run_command` in the
  catalog

This keeps setup cost visible and makes it much harder for small lanes to
silently inherit heavyweight Node, `apt`, DotSlash, `nextest`, or `sccache`
preparation they do not actually need.

## Remote measurement and summaries

`validation-lab` is expected to produce a compact workflow-level
`validation-summary` artifact.

That summary should identify:

- the host workflow ref when the run is dispatched from a branch that carries
  newer workflow behavior than the validated target
- the validated ref and head SHA
- the selected profile and lanes
- setup-class job results and started-lane counts
- setup-versus-command timing totals so slow setup paths are visible separately
  from the actual lane command time
- the first strong blocker, if any
- the frontier blocker queue when `profile=frontier`
- one primary blocker per exercised summary family, rather than a raw duplicate
  list of every failing sentinel and depth lane
- secondary findings for remaining cancelled or missing depth lanes
- the key failure signal, if available
- whether smoke gate, targeted lanes, or artifact build ran
- enough structured failure context to route debugging without embedding raw
  commands, planner payload fragments, or bulky log excerpts in the public
  summary surface
- avoid restating exact refs, commit SHAs, or workflow URLs in the public
  summary payload when GitHub already provides that run context separately

Watchers and follow-up tooling should prefer this structured summary over raw
log scraping when it is available.

The summary should also state:

- the profile intent (`smoke`, `targeted`, `frontier`, `checkpoint`, or
  `buildability`)
- short profile notes explaining when that mode is appropriate
- a compact lane-selection summary so operators can see the active shape at a
  glance

For workflow/planner-only changes, prefer an exact light-weight route instead
of broad product lanes. In this fork that means remote proof should stay on the
workflow sanity and downstream docs lanes unless the diff also touches real
runtime/product seams.

## Documentation boundaries

Keep tracked repository docs contributor-safe and generalized.

Tracked docs are the right place for:

- intended carried behavior
- seam-to-guardrail mappings
- generalized workflow rules

Do not put local machine details, operator-specific routing habits, local cost
or quota policy, or private workflow notes into tracked docs.
Those belong in local-only guidance or in the change record chosen by the team.

## Promotion and buildability

PR and merge-group workflows are promotion surfaces.
Use them once a branch is ready for promotion semantics.

Preview or prerelease builds should happen when:

- the branch is a promotion candidate
- packaging or toolchain changes need buildability proof
- someone explicitly needs a preview artifact

Do not use preview or prerelease builds as the default validator for ordinary
carry iteration.
