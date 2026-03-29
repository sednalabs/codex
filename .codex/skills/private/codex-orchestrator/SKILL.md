---
name: codex-orchestrator
description: Coordinate codex repository work with explicit lane selection, specialist-skill routing, Build Helper validation, PR babysitter dispatch, bounded subagent introspection, and seam-specific prompt contracts. Use when the main thread should stay on orchestration and integration while delegated lanes own scouting, review, bounded implementation, PR monitoring, or Ops bookkeeping. Choose role/model/reasoning in lane selection; pass only the task-specific contract, required skills, exact resources, and real overrides in the prompt.
---

# Codex Orchestrator

Use this skill when the thread should act as the codex repo orchestrator rather than as the primary worker.

## Core rules

- Default to **no time pressure**. This is a hard-coded orchestration default: do not rush, and assume wall-clock time is acceptable unless the operator explicitly says otherwise.
- Treat waiting as valid work.
- Keep the main thread on orchestration, integration, acceptance, and stop decisions.
- Delegate scout, review, bounded implementation, long waits, and exploratory Ops bookkeeping when ownership is clean.
- Give one owner to each seam at a time.
- Do not interrupt a running lane for opacity alone.
- When the open question is whether a running child is still making progress, prefer bounded introspection over a poke: use `$subagent-session-tail` and, when useful, `$codex-usage-ledger-query` before considering `interrupt=true` or a status-only follow-up.
- When the seam is PR monitoring, CI/review watch duty, or mergeability babysitting, route it through `$babysit-pr` instead of inventing raw `gh` watch loops or detached watcher scripts.
- Keep one-off child-status decisions in the parent thread or a bounded `orchestrator`; use `terminal-babysitter` only when repeated monitoring or blocking wait supervision is the actual seam.
- The parent orchestrator may read directly known Ops items and perform simple direct-target creates itself; reserve `ops-clerk` for search, trawl, paging, and cross-item exploration.
- If the task is read-only and the target is already known, prefer MCP resources or resource templates over tool calls when that surface is available.
- When handing a known Ops work item to a delegated lane that does not need to mutate it, pass the relevant Ops resource URI derived from the resource template, usually `ops://work_item/{work_item_ref}`, and add tree/comments/events URIs only if that lane truly needs them.
- Do not inline full Ops or Build Helper workflows here; route to the existing specialist skills.

Open [references/lane-policy.md](references/lane-policy.md) for role-selection rules and model-lane defaults.
Open [references/prompt-patterns.md](references/prompt-patterns.md) for ready-to-use prompt shapes.
Open [references/codex-workflows.md](references/codex-workflows.md) for codex-specific execution patterns.

## Lane selector

Pick the lightest role that can own the seam cleanly:

- `super-orchestrator`: repo-level sequencing, acceptance, final integration, and stop decisions
- `orchestrator`: one bounded subproblem that may need its own nested lanes
- `scout`: read-only code or context discovery
- `reviewer`: bounded review and risk finding
- `worker`: normal bounded implementation
- `worker-tight`: tiny explicit fix
- `worker-spark-tight`: extremely targeted, file-pointed fix with no broad repo exploration
- `terminal-babysitter`: waiting, polling, or Build Helper/task monitoring
- `ops-clerk`: exploratory Ops MCP search, trawl, paging, and bookkeeping

If the seam is not cleanly ownable, keep it in the parent thread until it is.
Sub-orchestrators are allowed when they can take one bounded subproblem to terminal more cheaply than relaying every step through the parent thread.

## Prompt Contract

Treat lane selection and prompt payload as two different surfaces.

Lane selection or spawn config already supplies:

- the lane role (`super-orchestrator`, `orchestrator`, `scout`, `reviewer`, `worker`, and so on)
- the default model and reasoning profile for that role
- the generic role behavior that comes with that lane choice
- the hard-coded codex-orchestrator tempo default that waiting is valid work and there is no time pressure unless explicitly changed

The prompt must carry the seam-specific contract:

- the exact objective
- exact worktree, branch, files, tool surface, or Ops resources
- task-specific ownership boundaries and non-goals
- validation limits for this seam
- waiting policy for this seam
- closure policy for this seam
- the exact specialist skills, ids, refs, resources, task ids, PR URLs, or evidence the lane needs
- any true overrides to the lane defaults

Do not restate by default:

- the selected role label or "act as X" wording when the lane role was already chosen
- the default model or reasoning names already attached to that role
- generic role behavior already encoded by the selected lane
- broad parent orchestration history that does not materially change the owned seam

Restate role/profile facts only when:

- you are intentionally overriding a lane default
- you need to narrow a role more than usual for this seam
- the child needs one short reminder to avoid a known misread of scope

## Spawn contract

Every delegated prompt must state:

1. objective
2. ownership boundaries
3. waiting policy
4. closure policy

Every delegated prompt should add only the task-specific extras the role/profile does not already supply:

- validation limits when they matter for this seam
- an explicit urgency override only when this seam genuinely differs from the hard-coded no-time-pressure / do-not-rush default
- the exact specialist skill injections needed for this seam
- the exact work item refs, Ops resources, task ids, PR URLs, or evidence the lane needs
- any explicit narrowing of the selected role's normal behavior

Every delegated prompt should also make the context policy explicit:
- pass full thread history only when the child truly needs the exact parent conversation state
- otherwise prefer a fresh lane with a short task brief plus the minimum files, refs, work item ids, or evidence it needs
- do not pass broad orchestration history to workers, scouts, or explorers that only own a narrow seam
- do not pass parent-level container/program chatter to a sub-orchestrator whose scope is only one lower-level subproblem

If a delegated lane receives one or more assigned `work_item_ref` values:
- if the lane is read-only or otherwise does not need to mutate the item, pass the relevant Ops resource URI derived from the template instead of routing it through a tool read, and do not make it claim the item unless that later becomes necessary
- if the lane will mutate or own the item operationally, pass `$ops-work-items`
- for a mutating or owning lane, tell it to claim the assigned item immediately before doing other work
- for a mutating or owning lane, tell it to use the hydrated claim result as its working context
- say whether the lane may comment on or close the item when done

Minimum tempo rule:
- The default is hard-coded: **no time pressure**, **do not rush**, and waiting is valid work.
- Do not restate that default in ordinary delegated prompts.
- Mention tempo only when the task genuinely overrides that default.

Minimum waiting rule:
- Waiting policy is seam-specific, so say it explicitly.
- If the lane is expected to wait on Build Helper, `wait_agent`, or long-running tools, tell it to **wait until terminal** unless a concrete blocker appears.
- If the lane is expected to wait on a perpetual watch/stream command, only use **wait until terminal** when the command itself exits on actionable or terminal state; otherwise switch to a bounded wrapper/mode that does.
- If you need an intermediate status before terminal, prefer bounded introspection first: session-tail plus usage-ledger heartbeat before a poke, interrupt, or repeated short wait loop.

Minimum closure rule:
- Say whether the lane may commit, comment on Ops, close a work item, or must hand back for acceptance.

## Existing skill routing

Use existing specialist skills instead of recreating their workflows:

- `$build-helper-general` for Build Helper discovery, execution, and status work
- `$babysit-pr` for PR monitoring, CI/review/mergeability watch loops, and branch-local babysitter fixes
- `$ops-work-items` for work item reads and mutations
- `$ops-friction-reports` for friction reads and mutations
- `$codex-usage-ledger-query` for token, thread-tree, provider-call, or usage-heartbeat questions
- `$subagent-session-tail` for cheap child-session progress checks from rollout JSONL plus usage-ledger heartbeat

Route to these skills from the correct delegated lane:
- `ops-clerk` for exploratory Ops search, trawling, or page-heavy bookkeeping
- `terminal-babysitter` for Build Helper or blocking wait supervision, including repeated bounded child-status checks when that is the main job
- `terminal-babysitter` plus `$babysit-pr` for PR watch seams where the actual job is ongoing monitoring and reporting, not one-shot diagnosis
- `scout` or `super-orchestrator` for usage-ledger questions, depending on scope
- `super-orchestrator` or a bounded `orchestrator` for one-off running-lane introspection when the goal is to decide whether to keep waiting, inspect more deeply, or send a bounded follow-up

The parent `super-orchestrator` or a bounded `orchestrator` may handle direct-ID Ops reads and simple direct-target creates itself when there is no need for exploratory Ops work.

## Running-Lane Introspection

Use this pattern when a child is still running and the parent needs evidence, not a full handoff:

- prefer `$subagent-session-tail` first for latest meaningful child events
- add `$codex-usage-ledger-query` when you need to know whether turns, provider calls, or token totals are still advancing
- treat the session-tail result as a decision surface: keep waiting, inspect more deeply, or send a bounded follow-up
- treat active tool work, fresh child commentary, or advancing token totals as evidence to keep waiting
- treat a flat tail plus flat ledger as the threshold for a gentle follow-up or deeper inspection
- do not interrupt a running child just to ask for status if bounded introspection would answer the question

## Codex-specific operating model

- Respect the codex repo's closest `AGENTS.md`.
- Prefer Build Helper for builds/tests/checks when presets exist.
- Prefer atomic industrial testing over broad suites during active iteration.
- Keep one owner per seam and keep the main thread out of an actively owned seam.
- If an active lane is already answering the exact open question, block on it instead of redoing the same work locally.
- Use an `orchestrator` sidecar only when it can take a bounded subproblem to terminal more cheaply than relaying every step through the main thread.
- Before poking a long-running child lane, prefer the session-tail plus usage-ledger path unless the child already owes an immediate answer or the parent has hit a concrete blocker.

## Anti-patterns

- Treating silence as failure.
- Interrupting a worker just because it is taking a long time.
- Poking a running lane for status when cheap introspection would answer the question.
- Stuffing delegated prompts with role labels, model names, or generic "act as X" boilerplate that lane selection already supplied.
- Launching raw PR watch loops when `$babysit-pr` already owns the monitoring workflow and actionable wait behavior.
- Doing routine scout work on the main thread.
- Spawning an `ops-clerk` just to read a directly known item or do a simple direct-target create.
- Spawning a large-model worker for a tiny bounded change.
- Letting the main thread and a delegated lane both verify or edit the same seam.
- Injecting babysitter behavior into every turn instead of only the lanes that need it.

## Finish line

End in one of these states:

- `Completed`: lane selection was correct, delegated work is integrated, and the terminal outcome is clear
- `Blocked`: a concrete blocker prevented a safe handoff or completion
- `Partial`: useful progress landed, but one explicit remaining seam or decision remains

Final reports should name:
- which lanes were used
- which specialist skills were invoked
- which seam stayed in the main thread
- why any lane was kept local instead of delegated
