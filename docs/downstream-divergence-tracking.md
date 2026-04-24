# Downstream Divergence Tracking Design

This note records the next-step maintenance model for downstream divergence
tracking.

Phase 1 is now implemented as the CI-backed `scripts/downstream-divergence-audit.py`
runner plus the checked-in `docs/divergences/index.yaml` registry. The
`codex.downstream-docs-check` validation lane runs that audit against PR heads,
while `sedna-sync-upstream` runs it after mirror refreshes on `main`. The later
generation phases below remain the forward path for ledger and regression
projection.

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
  - codex-rs/core/src/tools/spec.rs
  - codex-rs/core/src/tools/handlers/unified_exec.rs
guardrail_lane: core-carry-core-smoke
tests:
  - exec_command_reports_chunk_and_exit_metadata
  - write_stdin_returns_exit_metadata_and_clears_session
  - multi_agent_v2_wait_agent_honors_return_when_all
  - command_execution_completion_precedes_turn_completion_and_preserves_process_id
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
- `surface`
- `surface_type`
- `files`
- `introduced_in`
- `upstream_equivalent`
- `guardrail_lane`
- `tests`
- `owner`
- `notes`

Paths can point at directories (terminate with `/` to capture every child) or use glob-friendly tokens (`*`, `?`, `[]`). The audit matches these specs against the live diff so you can cover a directory such as `.github/workflows/` without listing each workflow individually.

The optional `surface_type` string (for example `agent-facing`, `operator-facing`, or `both`) signals how a divergence presents itself. The downstream audit renders that value in the registry reconciliation table and the code-path surface column to show whether a change touches agent-facing or operator-facing surfaces.

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

## Workflow write permission secret

The `sedna-sync-upstream` job fast-forwards `origin/upstream-main`, which contains workflow definitions and scripts. GitHub's `GITHUB_TOKEN` lacks the `workflow: write` scope needed to modify workflow files, so the job depends on the `SEDNA_SYNC_UPSTREAM_PUSH_TOKEN` secret. This should hold a PAT or machine-account token with `repo` write access plus `workflow: write`, stored only in this repository's secrets and rotated per policy. The secret is only used by the sync job when pushing the mirrored ref.

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
