# Industrial Validation Workflow

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
- one recorded conclusion in the team's operational tracker

If a slice cannot be described as "one question, one change, one measurement,"
it is probably too large.

Suggested tracking fields:

- slice id
- question or invariant
- commit SHA
- validation workflow/profile/lane
- run id
- outcome
- key signal
- next action

## Validation ladder

Use the smallest validator that can answer the current question.

1. Tiny local sanity checks
   - examples: `git diff --check`, syntax/schema/config validation
2. `validation-lab` `profile=smoke`
   - use when the branch changed substantially or the validation wiring changed
3. `validation-lab` `profile=targeted`
   - the default inner-loop path for one active seam
4. `validation-lab` `profile=frontier`
   - use only after a recent trusted smoke or targeted baseline
   - harvest a bounded queue of likely next blockers without running a full
     milestone checkpoint
5. `validation-lab` `profile=broad`
   - use only after the active seam is green
   - use it to reveal the next interaction-heavy divergence
6. `validation-lab` `profile=full`
   - use only for explicit broader confidence or pre-promotion soak
7. `profile=artifact` or `artifact_build=true`
   - use only when the question is buildability or preview delivery

## Fan-out and concurrency

Use matrix fan-out when several lanes answer the same seam question.
Use separate runs only when the questions are genuinely independent.

Default validation-lab policy:

- `targeted`: low fan-out
- `frontier`: bounded fan-out with `fail-fast=false`
- `broad`: moderate fan-out
- `full`: conservative fan-out

Do not widen every iteration into a broad or full run.
Get one seam green first, use `frontier` to harvest nearby blockers when the
baseline is trustworthy, and only then widen deliberately.

## Remote measurement and summaries

`validation-lab` is expected to produce a compact workflow-level
`validation-summary` artifact.

That summary should identify:

- the validated ref and head SHA
- the selected profile and lanes
- the first failing lane, if any
- the frontier blocker queue when `profile=frontier`
- the key failure signal, if available
- whether smoke gate, targeted lanes, or artifact build ran

Watchers and follow-up tooling should prefer this structured summary over raw
log scraping when it is available.

## Documentation boundaries

Keep tracked repository docs contributor-safe and generalized.

Tracked docs are the right place for:

- intended carried behavior
- seam-to-guardrail mappings
- generalized workflow rules

Do not put local machine details, operator-specific routing habits, or private
workflow notes into tracked docs.
Those belong in local-only guidance or in the team's operational tracker.

## Promotion and buildability

PR and merge-group workflows are promotion surfaces.
Use them once a branch is ready for promotion semantics.

Preview or prerelease builds should happen when:

- the branch is a promotion candidate
- packaging or toolchain changes need buildability proof
- someone explicitly needs a preview artifact

Do not use preview or prerelease builds as the default validator for ordinary
carry iteration.
