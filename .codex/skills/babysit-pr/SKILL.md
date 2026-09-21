---
name: babysit-pr
description: "Watch a GitHub PR through CI, review, and mergeability; fix branch-caused issues and bounded flakes until ready, merged, or blocked. Always use the bundled blocking watcher."
---

# PR Babysitter

## Objective
Babysit a PR persistently until one of these terminal outcomes occurs:

- The PR is merged or closed.
- CI is successful, there are no unaddressed review comments surfaced by the watcher, required review approval is not blocking merge, there are no potential merge conflicts (PR is mergeable / not reporting conflict risk), and the PR is not actively waiting in GitHub's merge queue.
- A situation requires user help (for example CI infrastructure issues, repeated flaky failures after retry budget is exhausted, permission problems, or ambiguity that cannot be resolved safely).

Do not stop merely because a single snapshot returns `idle` while checks are still pending.

A readiness-only leaf returns its proof to the named queue-admission owner.
Queue entry is not merged delivery: an already-active queue watch remains
owned until merge, closure, or an actionable failure. Any separately authorized
watcher handoff needs an accepting successor and explicit process/state custody;
do not invent a helper terminal receipt or abandon a running watcher.

## Operating Model

- Use `--watch-until-terminal` for delegated wait seams when current-head checks must finish before handoff. Use `--watch-until-action` for a repair owner that should wake on review feedback or a failure that can be acted on immediately.
- Use `--watch` only when the lane is actively consuming the live JSONL stream in the foreground.
- Use `--once` for one-shot diagnosis or local debugging, not for a full babysitting handoff.
- Blocking waits emit one final receipt and no per-poll progress by default. Use `--progress` only for deliberate interactive debugging; never enable it for a delegated wait whose output will be sent back to a model.
- Blocking waits return a compact action-complete receipt by default. Use `--verbose-details` only for debugging when the compact receipt and saved state file are insufficient.
- Use the bundled watcher helper script, `scripts/gh_pr_watch.py`, as the monitoring surface; do not replace it with ad hoc `gh`/API polling loops.
- Bind every explicit target to its repository in the command itself. Delegated
  prose does not scope `gh`: pass the full PR URL, or pass a bare number together
  with `--repo OWNER/REPO`. The helper rejects unqualified bare numbers.
- Treat this as the PR-local shepherd surface. If the seam stops being PR-local, or if an exact failing workflow run needs deeper or terminal-complete evidence, hand the exact run to `$babysit-gh-workflow-run` rather than improvising raw workflow polling.
- Treat an observed active `mergeQueueEntry` (including `AWAITING_CHECKS`) as a nonterminal queue wait even if the ordinary mergeability fields look clean. Its queue state and queue head SHA are merge evidence; a failed, removed, or unreadable queue outcome is an actionable stop, not `stop_ready_to_merge`.
- While that merge-queue entry remains active, keep ordinary PR-check failures visible in `check_details` and `merge_blockers` but do not classify an old or non-required PR-check failure as queue failure. The queue is the authoritative terminal-state machine at that point; wait for merge, queue failure/removal, or independently actionable review feedback.
- For a source, workflow, or head failure, read the exact causal run/job evidence before classifying it. Pending checks are `WAIT`, not `BLOCK`; a known branch-caused repair is actionable and must not wait for unrelated trailing jobs. Keep terminal-complete evidence collection available when a named consumer or governing contract requires it, but do not continue collecting unrelated terminal checks after an actionable candidate failure when repair is next.
- After any fix commit or flaky rerun, restart the same monitoring mode immediately and keep exactly one watcher session active for the PR.
- Read `merge_blockers` in each snapshot; it now summarizes blocker kinds explicitly (review threads, review-gate status, merge-conflict/dirty state, pending checks, failing checks).
- An unexplained `merge_policy_blocked` result is an action-required state even
  when all checks are green. The watcher emits `action_required_merge_policy_blocked`
  with the exact PR head and compact check, review, and blocker counts; ignored
  review threads remain policy evidence and never become approval. Persisted
  watcher state records the last exact-head decision and scheduled wake so a
  successor can distinguish an actionable blocker from a legitimate queue wait.
- Read `ci_startup_blockers` before diagnosing a failed check. A failed job with
  no runner and no steps is startup infrastructure evidence; the helper reads
  its annotations, identifies GitHub billing/spending-limit refusals when
  present, stops with `stop_ci_startup_blocked`, and never recommends a blind
  rerun for that state.

## Inputs
Accept any of the following:

- No PR argument: infer the PR from the current branch (`--pr auto`)
- PR number together with `--repo OWNER/REPO`
- PR URL

## Core Workflow

1. When the user asks to "monitor"/"watch"/"babysit" a PR, invoke one blocking watcher. Prefer `--watch-until-terminal` for a delegated check wait and `--watch-until-action` for a repair owner that should wake on actionable review or CI state. Use `--watch` only for deliberate foreground debugging.
2. Run the watcher script to snapshot PR/CI/review state (or consume each streamed snapshot from `--watch` / the final result from `--watch-until-action`).
3. Inspect the `actions` list in the JSON response.
4. If `diagnose_ci_failure` is present, inspect failed run logs and classify the failure.
   - If `checks_source` is `stale_fallback`, treat old-HEAD failures as context only and continue waiting for the current-head diagnostics to stabilize before rerun/branch-fix decisions.
   - If `ci_startup_blockers` is nonempty, use its no-runner/no-steps
     annotation evidence. Do not attribute the failure to the candidate or
     rerun it; stop for the external infrastructure or billing intervention.
5. If the failure is likely caused by the current branch, patch code locally, commit, and push.
6. If `process_review_comment` is present, inspect surfaced review items and decide whether to address them.
7. If a review item is actionable and correct, patch code locally, commit, and push.
8. If the failure is likely flaky/unrelated and `retry_failed_checks` is present, rerun failed jobs with `--retry-failed-now`.
9. If both actionable review feedback and `retry_failed_checks` are present, prioritize review feedback first; a new commit will retrigger CI, so avoid rerunning flaky checks on the old SHA unless you intentionally defer the review change.
10. On every loop, verify mergeability / merge-conflict status (for example via `gh pr view`) in addition to CI and review state.
11. After any push or rerun action, immediately return to step 1 and continue the watcher loop on the updated SHA/state.
12. If you had been using a watcher mode before pausing to patch/commit/push, relaunch the same monitoring mode yourself in the same turn immediately after the push (do not wait for the user to re-invoke the skill).
13. Repeat the watcher loop until the PR is green + review-clean + mergeable and not in an active queue, `stop_pr_closed` appears, or a user-help-required blocker is reached.
14. Maintain terminal/session ownership: while babysitting is active, keep consuming watcher output in the same turn; do not leave a detached watcher process running and then end the turn as if monitoring were complete. When the lane is using a blocking terminal wait, prefer `--watch-until-action` so the process exits on actionable or terminal state instead of streaming forever.

## Branch-Advancing-Safe Validation

Use this ritual whenever a PR branch, Dependabot branch, or base branch may advance while checks are running. This is the default for security, dependency, release, and public-repo stewardship work.

1. Capture the intended PR head before relying on any check result:
   `gh pr view <pr-url> --json headRefOid,baseRefOid,headRefName,baseRefName,mergeStateStatus,statusCheckRollup`.
2. Treat `headRefOid` as the validation identity. A workflow run, check rollup, or review result proves only the head SHA it reports, not the branch name by itself.
3. When watching workflow/ref runs outside the PR watcher, pass the exact head SHA whenever it is known:
   `--target "workflow=<name>,ref=<branch>,head-sha=<headRefOid>,min-run-id=<optional-lower-bound>"`.
4. If the watcher reports stale current-head context, `failed_runs_stale`, `checks_source=stale_fallback`, `followed_newer_run`, a cancelled superseded run, or any mismatch between the watched run head and the latest `headRefOid`, discard that run as merge evidence. Keep it only as diagnostic context, re-read the PR, and restart validation on the new head.
5. If the base branch advanced while the PR remained open, re-read mergeability and branch currency. Use the repository's normal update-branch/rebase path when required, then treat the resulting `headRefOid` as a new validation identity.
6. Immediately before merging, re-read the PR and require all of these to describe the same latest head SHA:
   current `headRefOid`, green required checks, review-clean state, mergeable/non-conflict state, and any workflow-run evidence cited in Ops.
7. Merge with a head guard whenever using `gh`:
   `gh pr merge <pr> --squash --delete-branch --match-head-commit <headRefOid>` (adjust merge method only to match repo policy).
8. If GitHub rejects the guarded merge because the head changed, do not retry the same command with the new SHA blindly. Re-run the validation loop for the new `headRefOid`.
9. Record evidence with the trusted PR number, exact `headRefOid`, base SHA if relevant, workflow run ids or check names, and whether any stale runs were discarded.

The short version: branch names are pointers; `headRefOid` is the proof target.

## Commands

### GitHub authentication and rate-limit recovery

For workstation runs, a future local integration may set the provisional
`CODEX_GITHUB_APP_TOKEN_COMMAND` to an executable
command that prints one short-lived GitHub App installation token on stdout.
The watcher invokes it without a shell, passes `CODEX_GITHUB_REPOSITORY` when
known, never includes its stdout/stderr in diagnostics, and caches the token for
the process. Existing `GH_TOKEN`, `GITHUB_TOKEN`, and interactive `gh` auth
remain the fallback only when the helper hook is absent. A configured helper
that fails is fatal. On an authentication rejection, the helper is refreshed
once;
if GitHub reports a rate limit, the watcher reads the authoritative
`rate_limit` reset where possible and performs one bounded sleep before retrying
the same exact-head operation. It never retries recursively or changes the
watched PR/run identity.

This hook is not an active broker integration: the accepted broker work exists
on repository `main`, while this installed skill currently lacks the broker
module and direct local compatibility/expiry continuity remain open. Do not
invent credentials or claim App-token use until that integration is installed
and read back.

### One-shot snapshot

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr auto --once
```

### Actionable wait for Codex babysitter lanes

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr auto --watch-until-action
```

### Terminal current-head check wait

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr auto --watch-until-terminal
```

### Ignore a known review thread while watching

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr <pr-url> --watch-until-action --ignore-review-thread <thread-url-or-id>
```

### Continuous watch (foreground JSONL)

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr auto --watch
```

### Trigger flaky retry cycle (only when watcher indicates)

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py \
  --pr <pr-url> --retry-failed-now --expected-head-sha <headRefOid> \
  --run-id <failed-run-id>
```

Retry mode requires the exact caller-observed PR head SHA. Repeat `--run-id` to
select a bounded set of failed workflow runs; when omitted, the current
snapshot's failed run IDs are bound into the receipt. Immediately before any
rerun, the helper re-reads the PR and every selected run and fails closed on a
head, lifecycle, run identity, terminal-state, or rerunnable-conclusion
mismatch. Each admitted run is mutated at most once. A successful rerun is
followed by an authoritative run readback that reports `run_attempt` and the
derived attempt identity when GitHub exposes it; the receipt is inconclusive
unless the attempt is strictly newer than the pre-mutation attempt. The run's
authoritative `pull_requests` association must include the selected PR. An
ambiguous provider command failure stops without a second mutation. One retry
cycle is durably reserved immediately after preflight and before the first
provider mutation, so a partial batch cannot silently reset its retry budget.

### Explicit PR target

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr <pr-url> --once

# A bare number is accepted only with an exact repository binding:
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr <number> --repo <owner/repo> --once
```

### GitHub App installation observer

When the separately governed broker has supplied a short-lived GitHub App
installation token, opt into the read-only observer path explicitly:

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py \
  --pr <number-or-url> --repo <owner/repo> --installation-observer --once
```

This mode skips `gh api user`, which installation tokens do not support, and
keeps review filtering conservative. It cannot be combined with
`--retry-failed-now`; the broker and watcher must not print or persist the
token. Long waits still require the separate token-expiry continuity contract.

## CI Failure Classification
Use `gh` commands to inspect failed runs before deciding to rerun.

- `gh run view <run-id> --json jobs,name,workflowName,conclusion,status,url,headSha`
- `gh run view <run-id> --log-failed`

Prefer treating failures as branch-related when logs point to changed code (compile/test/lint/typecheck/snapshots/static analysis in touched areas).

Prefer treating failures as flaky/unrelated when logs show transient infra/external issues (timeouts, runner provisioning failures, registry/network outages, GitHub Actions infra errors).

If classification is ambiguous, perform one manual diagnosis attempt before choosing rerun.
If the exact failed run needs deeper or terminal-complete workflow evidence, hand it to `$babysit-gh-workflow-run` rather than staying in ad hoc `gh` inspection.

Read `.codex/skills/babysit-pr/references/heuristics.md` for a concise checklist.

## Review Comment Handling
The watcher surfaces review items from:

- PR issue comments
- Inline review comments
- Review submissions (COMMENT / APPROVED / CHANGES_REQUESTED)

It intentionally surfaces actionable review bot feedback (for example comments/reviews from `chatgpt-codex-connector[bot]` and `gemini-code-assist[bot]`) in addition to human reviewer feedback. Operational bot issue comments such as quota/usage notices are intentionally ignored and should not trip `process_review_comment`.
For safety, the watcher only auto-surfaces trusted human review authors (for example repo OWNER/MEMBER/COLLABORATOR, plus the authenticated operator) and approved review bots when the artifact is an actual review/review-comment signal rather than generic PR issue chatter.
On a fresh watcher state file, existing pending review feedback may be surfaced immediately (not only comments that arrive after monitoring starts). This is intentional so already-open review comments are not missed.

For Gemini Code Assist feedback, route the specific thread through `$gemini-code-review-feedback` before replying or resolving it. That workflow records a precise accepted, partially accepted, already-addressed, or not-applicable disposition that can become useful persistent review memory after merge. Do not use a reaction, a generic acknowledgement, or silent thread resolution as the feedback signal.
An informational bot review that explicitly reports no review comments and no
feedback is not actionable and must not interrupt a pending-CI wait. Concrete
top-level feedback, unresolved inline threads, and change requests remain
actionable.

A bot top-level review submission tied to an older PR head is historical and
must not interrupt a newer exact-head watch. Current-head bot feedback,
unresolved inline threads, and trusted human review submissions remain
actionable.

When you agree with a comment and it is actionable:

1. Patch code locally.
2. Commit with `codex: address PR review feedback (#<n>)`.
3. Push to the PR head branch.
4. Resume watching on the new SHA immediately (do not stop after reporting the push).
5. If monitoring was running in `--watch` or `--watch-until-action` mode, restart that watcher mode immediately after the push in the same turn; do not wait for the user to ask again.

If you disagree or the comment is non-actionable/already addressed, record it as handled by continuing the watcher loop (the script de-duplicates surfaced items via state after surfacing them).
If a code review comment/thread is already marked as resolved in GitHub, treat it as non-actionable and safely ignore it unless new unresolved follow-up feedback appears.
`--watch-until-action` should verify the live unresolved-thread state before stopping on review feedback: open unresolved review threads remain actionable, but stale already-resolved review history should not trigger `action_required`.
If the operator knows a particular unresolved thread should be ignored for this babysitting run, pass `--ignore-review-thread <thread-url-or-id>` and keep that ignore list stable across restarts of the same watcher lane.
CI diagnostics are now current-head-first: older failed workflow runs for superseded heads are tracked as `failed_runs_stale` and surfaced in `ci_head_context` as background context while `checks_source` remains authoritative for actioning.
Current-head `failed_runs` now also carry exact failed-job references when GitHub exposes them, so handing off to the workflow-run watcher can use the concrete run/job pair instead of a heuristic run-level guess.

The watcher now surfaces richer review summaries: `review_state` includes recent top-level review submissions (`top_level_review_submissions`) and merge-blocking review submission signals, while `merge_blockers` explicitly reports why merge is blocked (including review threads, review-gate state, merge-conflict/dirty conditions, and CI check health).

## Git Safety Rules

- Work only on the PR head branch.
- Avoid destructive git commands.
- Do not switch branches unless necessary to recover context.
- Before editing, check for unrelated uncommitted changes. If present, stop and ask the user.
- After each successful fix, commit and `git push`, then re-run the watcher.
- If you interrupted a live watcher session to make the fix, restart the same watcher mode immediately after the push in the same turn.
- Do not run multiple concurrent `--watch` processes for the same PR/state file; keep one watcher session active and reuse it until it stops or you intentionally restart it.
- A push is not a terminal outcome; continue the monitoring loop unless a strict stop condition is met.

Commit message defaults:

- `codex: fix CI failure on PR #<n>`
- `codex: address PR review feedback (#<n>)`

## Monitoring Loop Pattern

The helper owns polling. Invoke it once in a blocking mode and wait for its single final receipt. Do not build a model-driven loop from repeated `--once` calls.

1. Run `--watch-until-terminal` for a delegated check wait, or `--watch-until-action` for a repair owner.
2. Read the single returned receipt and its `actions`.
3. Process review feedback before retrying an old-head flaky failure when both are present.
4. After a fix push or authorized rerun, invoke the same blocking mode once for the new exact head.
5. Hand an exact workflow run to `$babysit-gh-workflow-run` when deeper workflow evidence is required.

When the user explicitly asks to monitor/watch/babysit a PR, select one blocking helper mode and let it own its internal GitHub cadence. Repeated `--once` snapshots are only for debugging, local testing, or an explicitly requested one-shot check.
Do not stop to ask the user whether to continue waiting; continue autonomously until a strict stop condition is met or the user explicitly interrupts.
Do not hand control back after a review-fix push merely because a new SHA was created; starting one new blocking watcher invocation for that new exact head is part of the same task.
If a deliberate foreground `--watch` process is running, keep consuming it until a strict stop. Delegated lanes use the bounded blocking modes instead.

## Internal GitHub Cadence

The helper adapts its internal GitHub cadence and stops immediately when the PR is merged or closed. This is not authority to reproduce the cadence through model turns or repeated shell invocations.

## Stop Conditions (Strict)
Stop only when one of the following is true:

- PR merged or closed (stop as soon as a poll/snapshot confirms this).
- PR is ready to merge: CI succeeded, no surfaced unaddressed review comments, not blocked on required review approval, no merge conflict risk, and no active queue.
- User intervention is required and Codex cannot safely proceed alone.

Keep polling when:

- `actions` contains only `idle` but checks are still pending.
- CI is still running/queued.
- Review state is quiet but CI is not terminal.
- CI is green but mergeability is unknown/pending.
- CI is green and mergeable, but the PR is still open and the agreed monitor handoff requires watching the protected queue or review gate.
- The PR is green but blocked on review approval (`REVIEW_REQUIRED` / similar); use the single blocking watcher and surface new review comments, but do not invent a periodic model-driven cadence.

## Output Expectations
Return one concise final receipt from a blocking wait:

- Blocking waits produce no periodic model-visible heartbeat. The helper returns one final receipt; `--progress` is an explicit debugging-only exception.
- Treat push confirmations and review-fix actions as nonterminal; start one new blocking wait for the successor head.
- A user request to "monitor" is satisfied by the agreed blocking watcher outcome and its actual receipt, not by sample polls or queue entry alone. A readiness-only leaf must not remain open solely for hypothetical future comments once its strict ready condition is met.
- A review-fix commit + push is not a completion event; immediately resume the appropriate blocking mode in the same turn.
- Do not send the final summary while a watcher process is still running unless it has emitted a strict stop condition.

- Final PR SHA
- CI status summary
- Mergeability / conflict status
- Fixes pushed
- Flaky retry cycles used
- Remaining unresolved failures or review comments

## References

- Heuristics and decision tree: `.codex/skills/babysit-pr/references/heuristics.md`
- GitHub CLI/API details used by the watcher: `.codex/skills/babysit-pr/references/github-api-notes.md`
