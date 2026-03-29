# Codex Workflows

Use this file for codex-specific orchestration patterns.

## Carry/main implementation work

- Keep one owner per seam.
- Use a sub-orchestrator only when one bounded subproblem truly needs its own internal coordination loop.
- Use scouts for code reading and file discovery.
- Use workers for bounded edits.
- Use `worker-spark-tight` for extremely targeted, file-pointed fixes where `gpt-5.3-codex-spark` can stay inside a tiny context window.
- Use reviewers for non-trivial patches.
- Keep final integration and acceptance in the parent thread unless the seam was explicitly delegated end-to-end.
- Do not fork broad parent orchestration history into narrow workers or explorers; hand them only the seam-specific context they need.

## Waiting and child status

- The default operating stance is hard-coded: no time pressure, do not rush, and treat waiting as valid work unless the operator explicitly changes that.
- Prefer long waits when there is no decision to make.
- When the parent does need a mid-flight status, use bounded introspection before poking a child lane.
- Use `$subagent-session-tail` for the latest meaningful child-session events.
- Add `$codex-usage-ledger-query` when turn, provider-call, or token movement matters to the decision.
- Treat the session-tail result as a decision surface: keep waiting, inspect more deeply, or send a bounded follow-up.
- Treat active tool work, fresh child commentary, or advancing ledger totals as evidence to keep waiting.
- Treat a flat tail plus flat ledger as the threshold for a gentle follow-up or deeper inspection.
- Do not spend parent-thread tokens on repeated blind polling if a small introspection read would answer the question.

## Build Helper and validation

- Prefer Build Helper over ad-hoc terminal test/build commands when presets exist.
- Use atomic industrial slices during active iteration.
- Use a `terminal-babysitter` when the main value is waiting on Build Helper or another long-running status surface.
- Do not let the parent thread burn tokens watching a long-running validation loop when a babysitter can own it.

## PR babysitting and monitoring

- Use `$babysit-pr` for PR monitoring, CI/review/mergeability watch loops, and branch-local babysitter fixes.
- When delegating a PR watcher, pass the exact PR URL plus the exact worktree or cwd, not just a bare PR number from some other checkout.
- For blocking waits, use the babysitter skill's actionable wait path so the watcher exits on meaningful or terminal state instead of streaming forever behind a terminal wait.
- If the lane pauses to patch or push, it should restart the same monitoring mode itself instead of handing control back after the commit.

## Ops bookkeeping

- The parent orchestrator may read directly known work items or frictions itself and do simple direct-target creates when no exploratory Ops work is needed.
- Prefer MCP resources or resource templates for direct read-only Ops lookups when mutation, search, and paging are unnecessary.
- When handing a known work item to a read-only delegated lane, pass the relevant Ops resource URI, usually `ops://work_item/{work_item_ref}`, and only add `/comments`, `/events`, or `/tree` URIs if that lane truly needs them.
- Use `ops-clerk` for exploratory Ops search, trawling, paging, cross-item bookkeeping, or when the parent would otherwise spend too much context gathering IDs.
- Reuse `$ops-work-items` and `$ops-friction-reports` instead of duplicating their workflows in the orchestrator skill.
- When a delegated lane is assigned a `work_item_ref`, pass `$ops-work-items` and tell it to claim the item immediately so it can work from the hydrated context.
- Keep the parent thread out of broad, exploratory Ops work unless delegation is unavailable and the operator explicitly wants an exception.

## Usage-ledger questions

- Use `$codex-usage-ledger-query` when the question is about thread-tree usage, token accounting, provider/tool-call counts, or model economics.
- Prefer a scout or clerk-style delegated lane when the question is read-only and bounded.
- Keep synthesis in the parent thread only when the answer materially affects orchestration strategy.

## Review/fix/review loops

- Start with the smallest lane that can answer the open question.
- If review finds bounded issues, route them back to the smallest suitable worker.
- Re-run the narrowest meaningful validation path after each fix bundle.
- Do not escalate to a larger-model review pass until the smaller review lanes are genuinely exhausted.
