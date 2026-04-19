---
name: babysit-pr
description: Watch an open GitHub pull request from the current branch, a PR number, or a PR URL; monitor CI, reviewer feedback, and mergeability; fix valid branch-caused issues, rerun likely flaky failures up to 3 times, and keep monitoring until the PR is merged, ready to merge, or blocked on user help. For blocking babysitter waits, use `--watch-until-action`; use `--watch` only for active foreground JSONL consumption.
---

# PR Babysitter

## Objective
Babysit a PR persistently until one of these terminal outcomes occurs:

- The PR is merged or closed.
- CI is successful, there are no unaddressed review comments surfaced by the watcher, required review approval is not blocking merge, and there are no potential merge conflicts (PR is mergeable / not reporting conflict risk).
- A situation requires user help (for example CI infrastructure issues, repeated flaky failures after retry budget is exhausted, permission problems, or ambiguity that cannot be resolved safely).

Do not stop merely because a single snapshot returns `idle` while checks are still pending.

## Operating Model

- Use `--watch-until-action` when the lane will block on a terminal wait and should only return on actionable or terminal PR state.
- Use `--watch` only when the lane is actively consuming the live JSONL stream in the foreground.
- Use `--once` for one-shot diagnosis or local debugging, not for a full babysitting handoff.
- After any fix commit or flaky rerun, restart the same monitoring mode immediately and keep exactly one watcher session active for the PR.

## Inputs
Accept any of the following:

- No PR argument: infer the PR from the current branch (`--pr auto`)
- PR number
- PR URL

## Core Workflow

1. When the user asks to "monitor"/"watch"/"babysit" a PR, prefer the bounded actionable wait mode (`--watch-until-action`) when the agent will block on a terminal wait or should only hand back on meaningful PR state changes. Use the continuous stream (`--watch`) only when you are actively consuming the live JSONL output in the foreground.
2. Run the watcher script to snapshot PR/CI/review state (or consume each streamed snapshot from `--watch` / the final result from `--watch-until-action`).
3. Inspect the `actions` list in the JSON response.
4. If `diagnose_ci_failure` is present, inspect failed run logs and classify the failure.
5. If the failure is likely caused by the current branch, patch code locally, commit, and push.
6. If `process_review_comment` is present, inspect surfaced review items and decide whether to address them.
7. If a review item is actionable and correct, patch code locally, commit, push, and then mark the associated review thread/comment as resolved once the fix is on GitHub.
8. If a review item from another author is non-actionable, already addressed, or not valid, post one reply on the comment/thread explaining that decision (for example answering the question or explaining why no change is needed). Prefix the GitHub reply body with `[codex]` so it is clear the response is automated. If the watcher later surfaces your own reply, treat that self-authored item as already handled and do not reply again.
9. If the failure is likely flaky/unrelated and `retry_failed_checks` is present, rerun failed jobs with `--retry-failed-now`.
10. If both actionable review feedback and `retry_failed_checks` are present, prioritize review feedback first; a new commit will retrigger CI, so avoid rerunning flaky checks on the old SHA unless you intentionally defer the review change.
11. On every loop, look for newly surfaced review feedback before acting on CI failures or mergeability state, then verify mergeability / merge-conflict status (for example via `gh pr view`) alongside CI.
12. After any push or rerun action, immediately return to step 1 and continue polling on the updated SHA/state.
13. If you had been using `--watch` or `--watch-until-action` before pausing to patch/commit/push, relaunch that same monitoring mode yourself in the same turn immediately after the push (do not wait for the user to re-invoke the skill).
14. Repeat polling until the watcher reaches a terminal stop condition such as `stop_ready_to_merge`, `stop_pr_closed`, or a user-help-required blocker. A green + review-clean + mergeable PR is only a stopping point when the chosen watcher mode treats it as actionable.
15. Maintain terminal/session ownership: while babysitting is active, keep consuming watcher output in the same turn; do not leave a detached watcher process running and then end the turn as if monitoring were complete. When the lane is using a blocking terminal wait, prefer `--watch-until-action` so the process exits on actionable or terminal state instead of streaming forever.

## Commands

### One-shot snapshot

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr auto --once
```

### Actionable wait for Codex babysitter lanes

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr auto --watch-until-action
```

### Ignore a known review thread while watching

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr <number-or-url> --watch-until-action --ignore-review-thread <thread-url-or-id>
```

### Continuous watch (foreground JSONL)

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr auto --watch
```

### Trigger flaky retry cycle (only when watcher indicates)

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr auto --retry-failed-now
```

### Explicit PR target

```bash
python3 .codex/skills/babysit-pr/scripts/gh_pr_watch.py --pr <number-or-url> --once
```

## CI Failure Classification
Use `gh` commands to inspect failed runs before deciding to rerun.

- `gh run view <run-id> --json jobs,name,workflowName,conclusion,status,url,headSha`
- `gh run view <run-id> --log-failed`

Prefer treating failures as branch-related when logs point to changed code (compile/test/lint/typecheck/snapshots/static analysis in touched areas).

Prefer treating failures as flaky/unrelated when logs show transient infra/external issues (timeouts, runner provisioning failures, registry/network outages, GitHub Actions infra errors).

If classification is ambiguous, perform one manual diagnosis attempt before choosing rerun.

Read `.codex/skills/babysit-pr/references/heuristics.md` for a concise checklist.

## Review Comment Handling
The watcher surfaces review items from:

- PR issue comments
- Inline review comments
- Review submissions (COMMENT / APPROVED / CHANGES_REQUESTED)

It intentionally surfaces Codex reviewer bot feedback (for example comments/reviews from `chatgpt-codex-connector[bot]`) in addition to human reviewer feedback. Most unrelated bot noise should still be ignored.
For safety, the watcher only auto-surfaces trusted human review authors (for example repo OWNER/MEMBER/COLLABORATOR, plus the authenticated operator) and approved review bots such as Codex.
On a fresh watcher state file, existing pending review feedback may be surfaced immediately (not only comments that arrive after monitoring starts). This is intentional so already-open review comments are not missed.

When you agree with a comment and it is actionable:

1. Patch code locally.
2. Commit with `codex: address PR review feedback (#<n>)`.
3. Push to the PR head branch.
4. Resume watching on the new SHA immediately (do not stop after reporting the push).
5. If monitoring was running in `--watch` or `--watch-until-action` mode, restart that watcher mode immediately after the push in the same turn; do not wait for the user to ask again.

If you disagree or the comment is non-actionable/already addressed, reply once directly on the GitHub comment/thread so the reviewer gets an explicit answer, then continue the watcher loop. Prefix any GitHub reply to a code review comment/thread with `[codex]` so it is clear the response is automated and not from the human user. If the watcher later surfaces your own reply because the authenticated operator is treated as a trusted review author, treat that self-authored item as already handled and do not reply again.
If a code review comment/thread is already marked as resolved in GitHub, treat it as non-actionable and safely ignore it unless new unresolved follow-up feedback appears.
`--watch-until-action` should verify the live unresolved-thread state before stopping on review feedback: open unresolved review threads remain actionable, but stale already-resolved review history should not trigger `action_required`.
If the operator knows a particular unresolved thread should be ignored for this babysitting run, pass `--ignore-review-thread <thread-url-or-id>` and keep that ignore list stable across restarts of the same watcher lane.

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
Use this loop in a live Codex session:

1. Run `--once`.
2. Read `actions`.
3. First check whether the PR is now merged or otherwise closed; if so, report that terminal state and stop polling immediately.
4. Check CI summary, new review items, and mergeability/conflict status.
5. Diagnose CI failures and classify branch-related vs flaky/unrelated.
6. Process actionable review comments before flaky reruns when both are present; if a review fix requires a commit, push it and skip rerunning failed checks on the old SHA.
7. Retry failed checks only when `retry_failed_checks` is present and you are not about to replace the current SHA with a review/CI fix commit.
8. If you pushed a commit or triggered a rerun, report the action briefly and continue polling (do not stop).
9. After a review-fix push, proactively restart monitoring in the same mode you were using before the fix (`--watch-until-action` for blocking babysitter waits, `--watch` for active foreground streaming) unless a strict stop condition has already been reached.
10. If everything is passing, mergeable, not blocked on required review approval, and there are no unaddressed review items, report success and stop.
11. If blocked on a user-help-required issue (infra outage, exhausted flaky retries, unclear reviewer request, permissions), report the blocker and stop.
12. Otherwise sleep according to the polling cadence below and repeat.

When the user explicitly asks to monitor/watch/babysit a PR, prefer `--watch-until-action` for Codex babysitter lanes that will sleep on a blocking terminal wait and only need to hand back when something meaningful happens. Use `--watch` only when the agent is actively consuming a live JSONL stream in the foreground. Use repeated `--once` snapshots only for debugging, local testing, or when the user explicitly asks for a one-shot check.
Do not stop to ask the user whether to continue polling; continue autonomously until a strict stop condition is met or the user explicitly interrupts.
Do not hand control back to the user after a review-fix push just because a new SHA was created; restarting the watcher and re-entering the poll loop is part of the same babysitting task.
If a foreground `--watch` process is still running and no strict stop condition has been reached, the babysitting task is still in progress; keep streaming/consuming watcher output instead of ending the turn. If the lane is blocked on a terminal wait, use `--watch-until-action` so the watcher exits when there is a strict stop or actionable work to surface.

## Polling Cadence
Use adaptive polling and continue monitoring even after CI turns green:

- While CI is not green (pending/running/queued or failing): poll every 1 minute.
- After CI turns green: start at every 1 minute, then back off exponentially when there is no change. For foreground `--watch`, cap at every 1 hour. For `--watch-until-action`, cap at every 20 minutes so new review/check regressions surface in bounded time without chatty polling.
- Reset the green-state polling interval back to 1 minute whenever anything changes (new commit/SHA, check status changes, new review comments, mergeability changes, review decision changes).
- If CI stops being green again (new commit, rerun, or regression): return to 1-minute polling.
- If any poll shows the PR is merged or otherwise closed: stop polling immediately and report the terminal state.

## Stop Conditions (Strict)
Stop only when one of the following is true:

- PR merged or closed (stop as soon as a poll/snapshot confirms this).
- PR is ready to merge: CI succeeded, no surfaced unaddressed review comments, not blocked on required review approval, and no merge conflict risk.
- User intervention is required and Codex cannot safely proceed alone.

Keep polling when:

- `actions` contains only `idle` but checks are still pending.
- CI is still running/queued.
- Review state is quiet but CI is not terminal.
- CI is green but mergeability is unknown/pending.
- CI is green and mergeable, but the PR is still open and you are waiting for possible new review comments or merge-conflict changes per the green-state cadence.
- The PR is green but blocked on review approval (`REVIEW_REQUIRED` / similar); continue polling on the green-state cadence and surface any new review comments without asking for confirmation to keep watching.

## Output Expectations
Provide concise progress updates while monitoring and a final summary that includes:

- During long unchanged monitoring periods, avoid emitting a full update on every poll; summarize only status changes plus occasional heartbeat updates.
- Treat push confirmations, intermediate CI snapshots, and review-action updates as progress updates only; do not emit the final summary or end the babysitting session unless a strict stop condition is met.
- A user request to "monitor" is not satisfied by a couple of sample polls; remain in the loop until a strict stop condition or an explicit user interruption.
- A review-fix commit + push is not a completion event; immediately resume monitoring in the same turn and continue reporting progress updates.
- When CI first transitions to all green for the current SHA, emit a one-time celebratory progress update (do not repeat it on every green poll). Preferred style: `🚀 CI is all green! 33/33 passed. Still on watch for review approval.`
- Do not send the final summary while a watcher terminal is still running unless the watcher has emitted/confirmed a strict stop condition; otherwise continue with progress updates. For blocking waits, prefer `--watch-until-action` so the process itself returns when a meaningful state change occurs.

- Final PR SHA
- CI status summary
- Mergeability / conflict status
- Fixes pushed
- Flaky retry cycles used
- Remaining unresolved failures or review comments

## References

- Heuristics and decision tree: `.codex/skills/babysit-pr/references/heuristics.md`
- GitHub CLI/API details used by the watcher: `.codex/skills/babysit-pr/references/github-api-notes.md`
