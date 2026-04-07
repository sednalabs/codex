# Lane Policy

Use this file when the lane choice or model choice is not obvious.

## Tempo defaults

- Default assumption: **no time pressure**
- Do not rush unless the operator explicitly says urgency matters
- Waiting is valid work
- Long wall-clock time is acceptable when it saves main-thread tokens or preserves rigor
- Treat this as a hard-coded orchestration default, not something each delegated prompt needs to restate

## Validation defaults

- Prefer GitHub-first or other remote heavy validation when local heavy validation would be slow, expensive, or unnecessary.
- Avoid heavy local `cargo`, `just`, or `bazel` loops by default.
- When local validation is still needed, keep it to the smallest lane-local command that can answer the open question.

## Role defaults

- `super-orchestrator`
  - use for repo-level sequencing, acceptance, and stop decisions
  - should not be the default scout or clerk
  - may read directly known Ops items and perform simple direct-target creates itself when no exploratory Ops work is needed

- `orchestrator`
  - use for one bounded subproblem that can stay under a manager lane to terminal
  - do not use for repo-wide steering
  - may read directly known Ops items and perform simple direct-target creates itself when no exploratory Ops work is needed
  - should prefer MCP resources or resource templates for direct read-only lookups when mutation, search, and paging are unnecessary
  - may spawn a sub-orchestrator when one bounded subproblem needs its own internal coordination loop

- `scout`
  - use for read-only seam finding, file discovery, test discovery, or code reading
  - when the lane is tied to a known Ops work item and does not need to mutate it, pass the relevant Ops resource URI instead of making it reread through tools

- `reviewer`
  - use for bounded review only
  - findings, risks, and missing tests
  - when the lane is tied to a known Ops work item and does not need to mutate it, pass the relevant Ops resource URI instead of making it reread through tools

- `worker`
  - use for normal bounded implementation

- `worker-tight`
  - use only for very small, explicitly bounded fixes

- `worker-spark-tight`
  - use for extremely targeted, file-pointed fixes where the seam is already narrowed
  - do not use when the lane needs broad repo exploration or long context gathering
  - best for read-heavy, output-light work and narrow first-pass edits
  - stop using it once the seam becomes iterative, reviewer-driven, explanation-heavy, or compaction-heavy

- `terminal-babysitter`
  - use when the primary work is waiting on `wait_agent`, Build Helper, or another long-running status surface
  - should not own product reasoning or repo edits
  - may use `$subagent-session-tail` for repeated bounded child-status checks when that is the explicit seam
  - for PR monitoring seams, pair it with `$babysit-pr` and prefer actionable wait modes that return on meaningful change instead of inventing raw watch loops

- `ops-clerk`
  - use for exploratory work item, friction, link, or comment search, trawling, paging, or cross-item bookkeeping
  - should call the existing Ops skills instead of reimplementing their workflows

## Model lanes

- `super-orchestrator`: `gpt-5.4`
- `orchestrator`: `gpt-5.4`
- `scout`: `gpt-5.4-mini`
- `reviewer`: `gpt-5.4-mini`
- `worker`: `gpt-5.3-codex`
- `worker-tight`: `gpt-5.1-codex-mini`
- `worker-spark-tight`: `gpt-5.3-codex-spark`
- `terminal-babysitter`: `gpt-5.1-codex-mini`
- `ops-clerk`: `gpt-5.4-mini`

Step away from the default only when the seam clearly justifies it.

For very small code changes:
- use `worker-tight` when the fix is tiny and cheap enough for `gpt-5.1-codex-mini`
- use `worker-spark-tight` instead of `worker` when the fix is extremely targeted, the relevant file or tiny file set is already known, and `gpt-5.3-codex-spark` does not need to roam around the repo or enter an iterative fix/review loop

## Role-selected context versus prompt payload

Choose these in lane selection or spawn config:

- role
- model
- reasoning effort
- any explicit override to the lane defaults

Pass these in the prompt:

- exact objective
- exact ownership boundaries
- waiting policy
- closure policy
- validation limits when they matter for this seam
- exact ids, refs, resources, PR URLs, task ids, or evidence
- specialist-skill injections needed for this seam
- any narrowing of the selected lane's normal behavior

Do not repeat in the prompt by default:

- the selected role label or "act as X" wording
- model names or reasoning-effort labels already chosen for the lane
- generic role behavior already encoded by the lane
- the hard-coded no-time-pressure / do-not-rush default unless the task truly overrides it

If the task needs a tighter boundary than the role normally implies, say that narrowing explicitly in the prompt.
Examples:

- a `worker` that must stay inside one file
- a `terminal-babysitter` that must avoid repo edits
- a read-only lane that should only use passed Ops resources instead of broader tool reads

## Skill injection rules

Pass explicit specialist skills when the lane depends on them:

- Build Helper work: `$build-helper-general`
- PR monitoring or GitHub watch work: `$babysit-pr`
- Ops work items: `$ops-work-items`
- Ops friction: `$ops-friction-reports`
- usage.sqlite or shared usage-ledger questions: `$codex-usage-ledger-query`
- bounded child-session status checks: `$subagent-session-tail`

If a delegated lane is assigned a `work_item_ref` it owns:
- if the lane is read-only or otherwise does not need to mutate the item, pass the relevant Ops resource URI derived from the template, usually `ops://work_item/{work_item_ref}`, and add comments/tree/events URIs only when needed; do not make it claim unless mutation later becomes necessary
- if the lane will mutate or own the item operationally, pass `$ops-work-items`
- for a mutating or owning lane, instruct it to claim the item immediately as its first action
- for a mutating or owning lane, let it use the claim hydration instead of asking the parent to reread the item
- state whether the lane may comment on or close the item when done

Do not blindly inject every specialist skill into every prompt.

## Polling versus introspection

- Default to long blocking waits when there is no new decision to make.
- If you need mid-flight evidence, prefer bounded introspection over a poke: `$subagent-session-tail` first, then `$codex-usage-ledger-query` when token or turn movement matters.
- Do not interrupt a running child just to ask whether it is alive if the tail or ledger can answer that cheaply.
- Escalate to a gentle follow-up only after the introspection signals are flat, contradictory, or insufficient for the decision.
- Use a `terminal-babysitter` only when repeated bounded monitoring is the actual seam, not as a reflex for every running child.
- Keep one-off child-progress decisions in the parent thread or a bounded `orchestrator`; reserve `terminal-babysitter` for genuine monitoring seams such as Build Helper waits or PR babysitting lanes that already use `$babysit-pr`.

## Reuse versus fresh spawn

- Reuse an existing lane when it already owns the seam and only a delta follow-up is needed.
- Keep a warmed specialist lane alive when a clear short-horizon reuse path exists, especially for clerk-style or tool-heavy roles.
- Spawn fresh when the seam, ownership, or required role materially changes.
- Do not respawn a new lane just because the current one is quiet.
- Close warmed lanes once that near-term reuse window passes; do not keep an idle forest open just in case.

## Lane boundary rules

- Name the exact worktree and branch in every delegated prompt that owns repo work.
- Treat worktree and branch boundaries as hard ownership lines.
- Do not let one lane read, edit, validate, or commit across a sibling lane unless the parent explicitly reassigns scope.

## Context handoff rules

- Do not pass full thread history by default.
- Pass full thread history only when the child truly needs the exact prior conversation state to reason correctly.
- Prefer a fresh lane with a compact task brief plus the exact refs, files, work item ids, task ids, or evidence it needs.
- Workers, scouts, reviewers, and explorers should usually get narrowed context, not the whole parent transcript.
- A sub-orchestrator may need more context than a worker, but still should not inherit unrelated parent-level program or container history above its scope.
- If the child only needs one seam, one file set, or one work item, summarize and target that seam instead of forking the whole thread.

## Escalation back to the main thread

Pull work back only when:

- the seam was not actually bounded
- two lanes have a real ownership collision
- sandbox or permissions block the delegated path
- the delegated result is contradictory or unsafe to accept without direct judgment

## Public artifact hygiene

- Use neutral public names from the start.
- Do not put personal identifiers, hostnames, or machine-specific labels into branch names, commit subjects, or PR text.
