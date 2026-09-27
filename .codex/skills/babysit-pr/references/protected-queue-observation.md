## Protected queue coordinator contract

When this skill is used as the read-only coordinator for a protected queue, one
coordinator owns the queue view and its handoff record. The observer's cutline
is one exact queue-entry snapshot; it does not claim queue throughput, merge
authority, or a ready horizon. Keep independent entries moving when another
entry is `UNMERGEABLE`: isolate that state to the PR owner, record its exact
blocker, and leave the other entries eligible for their own owner-controlled
progress.

Each entry must carry one exact identity record before it is watched:
repository and PR number, PR owner, head SHA, base ref and base SHA, an
explicit queue-entry reference and synthetic candidate SHA/source, static
ancestry evidence proving that the synthetic candidate contains the exact PR
head and current base, the complete workflow run/attempt set, and the active
protection ruleset IDs, conditions, and revision. A queue ID is not a queue
ref; neither is a PR head a synthetic `G`. A display branch, a green check, or
an owner name without these bindings is not an entry identity.

Stage identity is kept separate across the delivery path:

| Stage | Required identity |
| --- | --- |
| `validation-ref` | Exact repository/ref and workflow input, with the full candidate SHA and its run/`G` identity; a ref name alone is only a selector. |
| `pull_request` | Repository/PR, PR owner, full head SHA, base ref/SHA, workflow/run/`G`, and the `pull_request` event. |
| `merge_group` | Queue entry/ref, synthetic candidate SHA, selected workflow/run/`G`, and ancestry proving the exact PR head and current base are included. |
| `post-main` | Resulting merge SHA on `main`, selected post-main workflow/run/`G`, and the `push` event; this is fresh evidence after landing, not a reused PR or queue result. |

Workflow/run aliases are normalized as one identity record: conflicting aliases
or duplicate run IDs invalidate the complete set, and workflow reads must consume
all provider pages before conclusions are drawn. A queue entry reference must be
present as an authoritative field and must be distinct from the provider queue ID.

`ALLGREEN` is a scoped queue-readiness label only. It is emitted only when the
exact synthetic `merge_group` candidate has exactly one terminal-successful
`merge_group` run for each of `CI required` and `CodeQL required`, with matching
full SHA and positive attempt, no unrelated/empty runs, static ancestry proof,
and applicable ruleset identity. It never authorizes a merge or substitutes
for fresh post-main proof when landing applies.

Use the bundled `gh_pr_watch.py` in one helper-owned blocking mode
(`--watch-until-action` for an owner that may need to act, or
`--watch-until-terminal` for a delegated check wait) only for a separately
owned PR-local wait. The queue observer is strictly one-shot and never invokes
that helper: its receipt schema is not compatible with the PR watcher receipt,
and it cannot accept or validate invented provenance fields. The helper covers
PR-local checks/reviews only; it is not a merge-queue, ruleset, or synthetic
candidate event watcher. Do not recreate any cadence with repeated `--once`
calls, raw `gh`/API polling, or a second watcher. After any separately-owned
helper wake, rehydrate the queue, ruleset, head, and run surfaces and bind a
new wait before relying on the result.

Stop the affected wait and rebind before any further conclusion after a new
head SHA, base SHA/ref, validation ref, queue entry/candidate or queue head,
run/`G`, owner, required workflow, or ruleset/protection revision appears. A
changed or removed queue candidate, a stale run, or an `UNMERGEABLE` result
invalidates that entry's prior evidence; it does not invalidate independent
entries. Never borrow an older exact-SHA result for a successor candidate.

The coordinator is an observer and handoff surface, not a mutation authority.
It must not bypass protection, direct merge, rebase/update branches, resolve
review threads, cancel or rerun unrelated work, alter queue/ruleset settings,
or perform raw polling against a provider. Report the exact owner action and
evidence needed instead. Existing PR babysitter repair, review, retry, and landing
semantics below remain in force for an explicitly authorized owner; observing
an entry or emitting `ALLGREEN` grants none of that authority.

### Merge-queue observer boundary

The bundled `gh_merge_queue_shepherd.py` is read-only and one-shot. It does
not enqueue, dequeue, merge, rerun, or alter rulesets. A snapshot with missing
queue ref, synthetic source, ancestry, required workflow evidence, ruleset
conditions/revision, or recognized queue state is deliberately unbound.
Ancestry is accepted only from the allowlisted `hosted-static-ancestry-v1`
schema; a caller-provided or `fabricated` source is never evidence. The
GraphQL adapter exposes only fields returned authoritatively by the provider;
it does not derive a queue ref from an entry ID or a synthetic SHA from a raw
head field. Its supported query currently returns the entry ID, position,
state, base commit, synthetic head commit, and pull-request identity. It does
not return `queueEntryRef`, queue attempt, or `ancestryEvidence`; those fields
remain unbound and are reported as an explicit external hosted prerequisite.
When the provider cannot return the required structural evidence, the runbook
outcome is `identity_mismatch_rebind_required` and a hosted follow-up must
obtain that evidence before claiming `ALLGREEN`.

The observer does not implement a delegated wait. PR-local helper output is
kept on its own owner-controlled surface and is never treated as a queue
identity receipt. This prevents the bundled `gh_pr_watch.py` receipt (which
lacks queue-observer helper/version/mode, run-set, required-conclusion,
thread-state, and fingerprint fields) from being accepted as if it supplied
queue evidence.

- Use `--watch-until-terminal` for delegated wait seams when current-head checks must finish before handoff. Use `--watch-until-action` for a repair owner that should wake on review feedback or a failure that can be acted on immediately.
- Use `--watch` only when the lane is actively consuming the live JSONL stream in the foreground.
- Use `--once` for one-shot diagnosis or local debugging, not for a full babysitting handoff.
- Blocking waits emit one final receipt and no per-poll progress by default. Use `--progress` only for deliberate interactive debugging; never enable it for a delegated wait whose output will be sent back to a model.
- Blocking waits return a compact action-complete receipt by default. Use `--verbose-details` only for debugging when the compact receipt and saved state file are insufficient.
- After any fix commit or flaky rerun, restart the same monitoring mode immediately and keep exactly one watcher session active for the PR.

The source watcher records a compact `watch_decision` and a `watch_schedule`
receipt keyed by the exact repository, PR number, and observed head SHA. An
unexplained `mergeStateStatus=BLOCKED` is reported as the actionable
`action_required_merge_policy_blocked` outcome. Readiness remains fail-closed,
and this blocker keeps the next wake bounded at the configured poll interval;
it must not enter green-state backoff. These receipts are observer state only:
they do not authorize merge, rerun, review, credential, or other provider
mutation.

An active merge-queue entry in `QUEUED` or `AWAITING_CHECKS` likewise keeps the
watcher on the configured base cadence, including when checks are green. A
missing or unreadable pending queue head is not readiness evidence and remains
on that base cadence. Queue-entry identity changes are part of snapshot change
detection, so re-enqueue, replacement, or removal cannot silently retain a
green-state backoff. Ordinary green PRs with no active queue entry retain the
existing bounded backoff.

## Inputs
Accept any of the following:

- No PR argument: infer the PR from the current branch (`--pr auto`)
- PR number together with explicit `--repo OWNER/REPO`
- PR URL

Bare PR numbers without `--repo` are rejected because repository-context inference can silently select an unrelated PR with the same number.

## Core Workflow

1. When the user asks to "monitor"/"watch"/"babysit" a PR, invoke one blocking watcher. Prefer `--watch-until-terminal` for a delegated check wait and `--watch-until-action` for a repair owner that should wake on actionable review or CI state. Use the continuous stream (`--watch`) only for deliberate foreground debugging.
2. Run the watcher script to snapshot PR/CI/review state (or consume each streamed snapshot from `--watch` / the final result from `--watch-until-action`).
3. Inspect the `actions` list in the JSON response.
4. If `diagnose_ci_failure` is present, inspect failed run logs and classify the failure.
5. If the failure is likely caused by the current branch, patch code locally, commit, and push. Do not patch random flaky tests, CI infrastructure, dependency outages, runner issues, or other failures that are unrelated to the branch.
6. If `process_review_comment` is present, inspect surfaced published review items and decide whether to address them.
7. If a review item is actionable and correct, patch code locally, commit, push, and then resolve the associated review thread only when allowed by the GitHub state mutation policy below.
8. Every Gemini Code Assist thread on a PR touched by the lane must receive a substantive, evidence-backed threaded reply and be resolved before the lane can be called clean, ready, or complete. This includes outdated threads. If a thread is genuinely blocked, leave it open with an explicit blocker and do not report terminal readiness. Re-read the live thread ledger at the final exact head.
9. Do not post replies to human-authored review comments/threads unless the user explicitly confirms the exact response. If a human review item is non-actionable, already addressed, or not valid, surface the item and recommended response to the user instead of replying on GitHub.
10. If the failure is likely flaky/unrelated and `retry_failed_checks` is present, rerun failed jobs with `--retry-failed-now`.
11. If both actionable review feedback and `retry_failed_checks` are present, prioritize review feedback first; a new commit will retrigger CI, so avoid rerunning flaky checks on the old SHA unless you intentionally defer the review change.
12. On every loop, look for newly surfaced review feedback before acting on CI failures or mergeability state, then verify mergeability / merge-conflict status (for example via `gh pr view`) alongside CI.
13. After any push or rerun action, immediately return to step 1 and continue polling on the updated SHA/state.
14. If you had been using `--watch` or `--watch-until-action` before pausing to patch/commit/push, relaunch that same monitoring mode yourself in the same turn immediately after the push (do not wait for the user to re-invoke the skill).
14. Repeat polling until the watcher reaches a terminal stop condition such as `stop_ready_to_merge`, `stop_pr_closed`, or a user-help-required blocker. A green + review-clean + mergeable PR is only a stopping point when the chosen watcher mode treats it as actionable.
15. Maintain terminal/session ownership: while babysitting is active, keep consuming watcher output in the same turn; do not leave a detached watcher process running and then end the turn as if monitoring were complete. When the lane is using a blocking terminal wait, prefer `--watch-until-action` so the process exits on actionable or terminal state instead of streaming forever.

## Commands
