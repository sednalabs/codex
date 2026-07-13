# Downstream Divergence Tracking Design

This note records the next-step maintenance model for downstream divergence
tracking.

Phase 1 is now implemented as the CI-backed `scripts/downstream-divergence-audit.py`
runner plus the checked-in `docs/divergences/index.yaml` registry. The
`codex.downstream-docs-check` validation lane runs PR-local docs and registry
sanity, while the explicit `codex.downstream-divergence-audit` lane and
`sedna-sync-upstream` run the full audit after mirror refreshes or deliberate
baseline checkpoints. The later generation phases below remain the forward path
for ledger and regression projection.

## Why This Exists

The current downstream docs are doing three jobs at once:

- stable fork policy and workflow guidance
- live divergence inventory
- historical upstream-equivalent carry history

That works when the downstream carry branch is small. It will age badly as the
maintained downstream branch (historically `carry/main`, now `main`) keeps
moving further ahead of `upstream/main`.

## Recommended Split

Keep the existing docs, but narrow their responsibilities:

- `docs/downstream.md`
  - stable manual policy and high-signal narrative rationale
- `docs/downstream-tool-surface-matrix.md`
  - exact carry vs upstream tool-surface comparison for native coordination
    tools
- `docs/carry-divergence-ledger.md`
  - generated audit view of current live divergences plus upstream-equivalent
    history
- `docs/downstream-regression-matrix.md`
  - generated guardrail view of divergence-to-test-lane mapping

Use the canonical registry:

- `docs/divergences/index.yaml`

## What Should Be Generated

- `main` vs `upstream/main` counts and current SHAs
- current live divergence list
- changed-file inventory per divergence
- upstream-equivalent carry matches
- regression-lane and test mapping for each divergence
- stale-entry warnings when a registry item no longer matches the live tree

## What Should Stay Manual

- branch policy
- workflow guidance
- narrative rationale for why a divergence exists
- taxonomy choices
- the decision that a change is a real divergence rather than derivative churn
- lane ownership when the choice is subjective
- The enforced audit command is `--code-only`, so docs-registry coverage is advisory unless a non-code audit path is run.

## Minimal Registry Shape

One checked-in registry entry per divergence is enough.

```yaml
id: exec-blocking-wait
title: Blocking unified-exec waits
status: live
category: core-protocol
surface:
  - exec_command
  - write_stdin
behavior: wait_until_terminal blocks until exit or timeout
upstream_equivalent: false
introduced_in:
  carry_commit: <sha>
  upstream_commit: null
files:
  - codex-rs/core/src/tools/spec_plan.rs
  - codex-rs/core/src/tools/handlers/unified_exec.rs
guardrail_lane: core-carry-core-smoke
tests:
  - exec_command_reports_chunk_and_exit_metadata
  - write_stdin_returns_exit_metadata_and_clears_session
  - completion_rule_distinguishes_any_from_all
  - command_execution_notifications_include_process_id
  - shell_command_approval_emits_task_complete_before_tool_response
owner: downstream
notes: |
  Tool-layer wait semantics and blocking completion ordering, not transcript polling.
```

Keep the schema small:

- `id`
- `title`
- `status`
- `category`
- `behavior`
- `upstreamability_tier`
- `boundary_type`
- `hotspot_files`
- `extraction_target`
- `surface`
- `surface_type`
- `files`
- `required_markers`
- `introduced_in`
- `upstream_equivalent`
- `guardrail_lane`
- `tests`
- `owner`
- `notes`

Paths can point at directories (terminate with `/` to capture every child) or use glob-friendly tokens (`*`, `?`, `[]`). The audit matches these specs against the live diff so you can cover a directory such as `.github/workflows/` without listing each workflow individually.

The optional `required_markers` object maps exact repo-relative POSIX paths to
non-empty text markers. In strict mode, the audit reads those paths from the
resolved downstream commit and fails if a file or marker is missing. Use this
for high-value behavior and regression seams that could otherwise disappear
while a broad `files` match keeps the carry entry looking live.

The optional `surface_type` string (for example `agent-facing`, `operator-facing`, or `both`) signals how a divergence presents itself. The downstream audit renders that value in the registry reconciliation table and the code-path surface column to show whether a change touches agent-facing or operator-facing surfaces.

Every live divergence also declares an upstreamability boundary:

- `upstreamability_tier` must be one of `upstream-pr`, `neutral-seam`, `downstream-adapter`, or `operator-only`.
- `boundary_type` names the narrow architecture boundary that should own the divergence, such as `app-server-contributor`, `tui-contributor-slot`, `tool-runtime-capability`, or `operator-workflow`.
- `hotspot_files` lists high-churn files or directories touched by the divergence. Use an empty list only when the carry does not touch a known hot file or workflow surface.
- `extraction_target` names the seam, adapter, provider registry, workflow layer, or upstream PR target that should reduce future sync pain.

The audit fails strict registry validation when a live divergence omits these fields, uses an unknown upstreamability tier, or touches known hot paths such as core tool handlers, app-server processors, TUI orchestration files, state runtime files, workflow files, or `justfile` without listing `hotspot_files` and a guardrail lane.

## Suggested Taxonomy

Use a small fixed category set:

- `branch-policy`
- `core-protocol`
- `subagents`
- `tui`
- `config`
- `mcp`
- `usage-ledger`
- `build-validation`
- `docs-only`
- `test-only`

If a divergence does not fit one of those, the taxonomy needs tightening.

## Generation Inputs

The registry should be reconciled against live git state:

- `git rev-list --left-right --count upstream/main...main`
- `git diff upstream/main...main --name-only`
- `git log --left-right --cherry-pick --oneline upstream/main...main`

Where useful, generator code can also read local helper preset metadata,
but the tracked docs should not depend on a committed preset file being present
in the repository.

## Expected Workflow

1. Add or update one registry entry whenever a carry patch lands.
2. Regenerate the ledger and regression matrix in the same PR.
3. Fail CI when generated docs drift from the registry plus git state.
4. During sync audits, fail if a live diff exists without a registry entry.
5. Keep historical upstream-equivalent items in the registry with
   `status: upstream-equivalent` instead of deleting them.

## Workflow write credential

The `sedna-sync-upstream` job is the only privileged mirror writer. It
fast-forwards `origin/upstream-main`, which contains workflow definitions and
scripts, and then runs the authoritative divergence audit against the exact
synced SHA.

GitHub's default `GITHUB_TOKEN` is not the right credential for this mirror
write, so the workflow mints a short-lived GitHub App installation token from
`SEDNA_SYNC_UPSTREAM_APP_CLIENT_ID` and
`SEDNA_SYNC_UPSTREAM_APP_PRIVATE_KEY`. The app should be installed only on this
repository and granted the narrow repository permissions needed to update the
mirror ref, including contents write and workflows write. During migration the
workflow may fall back to the legacy `SEDNA_SYNC_UPSTREAM_PUSH_TOKEN` secret,
but that PAT path should be retired after the GitHub App proof run succeeds.

Pull request validation is read-only. The `codex.downstream-docs-check` lane is
PR-local and does not require mirror writes. The explicit
`codex.downstream-divergence-audit` lane fetches live `upstream/main`; when the
public mirror is stale it audits against that exact fetched snapshot and leaves
mirror updates to `sedna-sync-upstream`.
The rendered tree diff still shows both upstream-ahead and downstream-ahead
paths, but registry enforcement is scoped to the downstream carry diff from the
merge base to the audited downstream ref. That keeps upstream-only files visible
without requiring the downstream registry to document upstream work before a
sync lands.

## Phased Adoption

Phase 1 (implemented):

- keep the current manual docs current
- use `docs/downstream-tool-surface-matrix.md` for high-signal field-level
  comparison
- use `scripts/downstream-divergence-audit.py` and `docs/divergences/index.yaml`
  for the authoritative audit path

Phase 2 (implemented):

- `docs/divergences/index.yaml` is the canonical divergence registry

Phase 3 (in progress):

- generate `docs/carry-divergence-ledger.md`
- generate `docs/downstream-regression-matrix.md`
- add CI drift checks

Manual docs remain the narrative layer; the registry plus audit runner are the
authoritative live-state ledger.
