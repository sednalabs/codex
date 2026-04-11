---
name: sidecar-review-loop
description: Run a high-rigor sidecar review, fix, verify, and escalation loop on a repo or worktree. Use for requests like "audit this branch before merge", "keep reviewing and fixing until clean", or "run sidecar review passes with verification after each round", especially when the main thread should stay on orchestration while sidecars own scoped review and repair work.
---

# Sidecar Review Loop

Use this skill when the thread should coordinate a serious multi-pass review/fix workflow instead of doing the bulk of the review and patching locally.

## Operating Model

- Keep the main thread on orchestration, validation choices, acceptance, and stop decisions.
- Reuse specialist skills instead of rebuilding their workflows:
  - `$build-helper-general` for preset discovery, execution, status, and failure summaries
  - `$subagent-session-tail` for cheap quiet-lane inspection
  - `$codex-usage-ledger-query` only when turn or token movement still matters after session-tail
- Start with a real baseline: exact repo/worktree state plus the smallest credible verification path.
- Use read-only sidecar reviewers for findings and bounded sidecar workers for fixes. Keep their scopes disjoint.
- Treat a green verification result as the baseline for the next review round, not proof the work is done.
- Default to no time pressure. Waiting is valid work.
- Stop only when the latest mini sweep is clean and the latest frontier escalation pass is clean.

## Default Loop

1. Confirm the target.
- Inspect the repo state, diff shape, and intended verification path before spawning review lanes.
- If the worktree is expected to be buildable, establish the smallest meaningful baseline first.

2. Establish the baseline.
- Prefer `$build-helper-general` when repo presets exist.
- Validate that the chosen preset actually targets the current checkout or repo path before relying on it.
- Keep the exact preset ids and task ids so later summaries are precise.
- If no suitable preset exists, or the discovered preset clearly targets some other checkout, fall back to the smallest relevant repo-native build/test command and say so explicitly.
- If the baseline is already red, treat that failure as the first fix bundle before broad review.

3. Split review ownership.
- Start with `reviewer` lanes using `gpt-5.4-mini` at `xhigh`.
- Use disjoint scopes so findings are attributable and fixes can be assigned cleanly.
- Adapt the split to the actual diff. Use fewer reviewers for narrow seams; do not force a four-way split onto a tiny change.
- A good default split for a broad change is:
  - metadata/catalog reload semantics
  - runtime/settings/connection-pool/secret resolution
  - query validation/security/docs alignment
  - docs/build-helper alignment when docs changed substantially

Each reviewer should:
- inspect only its owned files or diff slice
- report only material findings
- order findings by severity
- say explicitly when no material findings remain in scope

4. Wait correctly.
- Use `wait_agent` with long timeouts.
- Do not interrupt a running sidecar just because the shared worktree changed or because the task is taking a while.
- Assume a running sidecar may be waiting on Build Helper, nested sidecars, validation, or a still-correct MCP call.
- If a sidecar goes quiet and you need evidence before deciding what to do, inspect it first:
  - `$subagent-session-tail` first
  - `$codex-usage-ledger-query` only if session-tail is still insufficient to tell whether the lane is moving
- Only interrupt for one of these reasons:
  - the user explicitly asks for interruption
  - there is a real host-pressure emergency
  - two workers have a proven conflicting write scope and one must stop immediately

5. Turn findings into fix bundles.
- Group findings into narrow bundles with one defect family, one disjoint write set, and one clear verification expectation.
- For normal bundles, spawn one `worker` using `gpt-5.4-mini` at `xhigh`.
- For tiny, explicit, file-pointed, near-mechanical follow-ups, prefer `worker-tight` or `worker-spark-tight` instead of a broader worker lane.
- Give each worker exact file ownership and tell it that it is not alone in the codebase and must not revert others' edits.
- Keep the main thread out of local patching unless there is a truly trivial unblock that is smaller than the cost of another sidecar.

6. Verify after every fix round.
- Close completed sidecars to free thread budget once their outputs are integrated.
- Re-run the exact verification path from the parent thread.
- Prefer a build preset plus a smoke or targeted test preset when Build Helper is available.
- Do not silently swap verification paths mid-loop. If you need a different preset or command, say why.
- If verification fails, fix the build/test break first before launching the next review wave.

7. Escalate only after the mini loop is clean.
- Repeat mini review/fix/verify cycles until the latest `gpt-5.4-mini` `xhigh` sweep reports no material findings.
- Then run one `gpt-5.4` `xhigh` full review over the whole worktree.
- If it finds material issues, fix them with narrow workers, re-run verification, then run a quick mini confirmation sweep.
- Re-run the `gpt-5.4` `xhigh` escalation pass only if the prior escalation round found material issues that changed the tree.

## Guardrails

- Prefer sidecar reviewers and workers over doing the bulk of the fixes locally.
- Keep reviewer and worker scopes disjoint.
- Use `$build-helper-general` when suitable Build Helper presets exist.
- Distinguish coverage gaps from code defects.
- Do not silently rerun a failed preset without explaining why.
- Do not let stale completed agents accumulate if thread budget is limited.
- Do not escalate to the frontier review pass while the current mini sweep is still red.
- Do not treat a quiet sidecar as stalled until session-tail and, if needed, usage-ledger evidence say so.

## Finish Line

End in one explicit state:

- `Completed`: the latest mini sweep is clean, the latest frontier escalation pass is clean, and the verification state is clearly reported
- `Blocked`: a concrete blocker prevented a safe next review or fix round
- `Partial`: useful review/fix progress landed, but one explicit remaining seam or decision is still open

Close out with:

- the final commit id if a commit was created
- confirmation that the worktree is clean or a statement that it is not
- the exact Build Helper presets and task ids used, or the exact fallback commands if Build Helper was not used
- whether the latest mini sweep was clean
- whether the latest `gpt-5.4` `xhigh` pass was clean
- any residual risks that are coverage gaps rather than known local defects
