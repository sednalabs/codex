# Prompt Patterns

Use these as starting shapes. Keep prompts short and explicit.

These patterns assume the lane role was already selected in `spawn_agent` or an equivalent lane-choice step.
The prompt should carry the seam-specific contract, required skill injections, exact resources, and true overrides.
It should not carry redundant role, model, or reasoning boilerplate that the selected lane already provides.

## Prompt payload rule

Always pass:

- the exact objective
- exact ownership boundaries
- waiting policy for this seam
- closure policy for this seam

Pass when needed:

- validation limits
- an explicit urgency override when the task genuinely differs from the hard-coded no-time-pressure / do-not-rush default
- explicit specialist-skill injections
- exact work item refs, Ops resources, task ids, PR URLs, or evidence
- any narrowing or override of the selected role's normal behavior

Do not repeat by default:

- the selected role label or "act as X" wording
- model names or reasoning-effort names already set by the lane
- generic role behavior already encoded by the lane
- broad parent history that does not materially change the owned seam
- the hard-coded no-time-pressure / do-not-rush default unless the task truly overrides it

## Shared contract

Every delegated prompt should contain the seam contract, not redundant role/profile boilerplate:

```text
Objective:
- [exact outcome]

Ownership:
- exact worktree: [absolute path]
- exact branch: [branch name]
- exact files / repo area / tool surface: [bounded scope]
- mode: [read-only / write-allowed]
- stay inside the named worktree and branch unless the prompt explicitly expands scope
- keep unchanged: [explicit non-goals]

Validation limits:
- keep validation narrow and lane-local
- prefer GitHub or other remote branch validation for heavy builds/tests when local heavy validation would be wasteful
- do not run heavy local cargo/just/bazel paths unless the prompt explicitly allows them
- if validation is needed, run only the smallest allowed command and stop when the limit is reached

Urgency override:
- omit by default
- include only when the task truly has time pressure or otherwise overrides the hard-coded no-time-pressure / do-not-rush default

Waiting policy:
- wait until terminal on long-running waits unless a concrete blocker appears
- for perpetual watch or stream commands, only use wait-until-terminal when the command itself exits on actionable or terminal state; otherwise prefer a bounded wrapper/mode that returns on meaningful change
- for PR monitoring or watch seams, inject $babysit-pr and prefer its actionable wait mode over raw gh or script watch loops
- do not interrupt subordinate lanes for opacity alone

Closure policy:
- [may commit / may comment / may close item / hand back only]
- stop condition: [exact condition that ends this lane]

Public artifact hygiene:
- do not put personal identifiers or machine-specific labels into branch names, commit subjects, or PR text
- normalize public-facing names to neutral descriptions
```

For read-only direct lookups where the target is already known:

```text
Lookup rule:
- prefer MCP resources or resource templates over tool calls when no mutation, search, or paging is needed
```

If the lane is being assigned one or more `work_item_ref` values, also do this:

```text
Work item rule:
- if this lane is read-only or does not need to mutate the work item, pass the relevant Ops resource URI derived from the template, usually ops://work_item/{work_item_ref}
- add ops://work_item/{work_item_ref}/comments, /events, or /tree only when this lane truly needs them
- if this lane will mutate or own the work item operationally, also use $ops-work-items
- if this lane will mutate or own the work item operationally, first action: claim the assigned work item directly
- if this lane will mutate or own the work item operationally, use the hydrated claim result as working context
- comment on or close the item only if the closure policy allows it
```

Context rule:

```text
- do not pass full thread history unless this lane truly needs the exact prior conversation state
- otherwise give a compact brief plus the minimum files, refs, work item ids, task ids, or evidence needed
```

## Super-orchestrator handoff

Use when the selected lane role is `super-orchestrator` and the main thread should stay on coordination:

```text
Use $codex-orchestrator for this seam.

[Shared contract]

Keep the main thread on orchestration, acceptance, and stop decisions.
Delegate scout, review, implementation, waiting, or Ops bookkeeping to the lightest suitable role.
Do direct-ID Ops reads or simple direct-target creates yourself when that is cheaper than spawning an ops clerk.
Use an ops clerk only for exploratory Ops search, trawling, paging, or cross-item bookkeeping.
Use existing specialist skills when needed instead of recreating their workflows.
```

## Bounded orchestrator handoff

Use when the selected lane role is `orchestrator` and a manager-sidecar can own one subproblem to terminal:

```text
Use $codex-orchestrator for this subproblem only.

[Shared contract]

You may use nested lanes only when the ownership split is clean.
Do direct-ID Ops reads or simple direct-target creates yourself when that is cheaper than spawning an ops clerk.
Prefer MCP resources or resource templates for direct read-only lookups when that surface is available.
Spawn a sub-orchestrator only when this bounded subproblem genuinely needs its own internal coordination loop.
Return one integrated outcome to the parent thread.
```

## Scout handoff

```text
Use $codex-orchestrator for this seam.

[Shared contract]

Inspect the closest AGENTS.md and the relevant code before concluding.
If the lane is anchored to a known work item and does not need mutation, work from the passed Ops resource URI rather than re-reading it through tools.
Return evidence, candidate seams, and one best next action.
```

## Reviewer handoff

```text
Use $codex-orchestrator for this seam.

[Shared contract]

Review only the requested files or diff.
If the lane is anchored to a known work item and does not need mutation, work from the passed Ops resource URI rather than re-reading it through tools.
Return material findings ordered by severity, or say explicitly that no material findings remain in scope.
```

## Worker handoff

```text
Use $codex-orchestrator for this seam.

[Shared contract]

Implement only the bounded change.
Run only the smallest allowed validation path and report exact commands and results.
```

## Tight worker handoff

```text
Use $codex-orchestrator for this seam.

[Shared contract]

Accept only a very small, explicitly bounded fix.
If the seam is broader than that, stop and say so.
```

## Spark-tight worker handoff

```text
Use $codex-orchestrator for this seam.

[Shared contract]

Accept only a read-heavy, output-light, extremely targeted, file-pointed first-pass fix.
Do not roam around the repository or spend time on broad discovery.
If the seam needs wider context gathering, iterative fix/review passes, or long explanations, stop and hand it back.
```

## Terminal babysitter handoff

```text
Use $codex-orchestrator for this seam.
Also use $build-helper-general if Build Helper is involved.
Also use $babysit-pr if the seam is PR monitoring.

Objective:
- wait on the specified task or agent and report only meaningful status changes and the terminal result

Ownership:
- no repo edits
- no Ops mutations unless explicitly requested

Waiting policy:
- wait until terminal by default
- poll only when the runtime or tool surface requires it
- for perpetual watch or stream commands, prefer a mode that exits on actionable or terminal state instead of waiting forever on an endless stream
- for PR monitoring seams, prefer the babysitter skill's actionable wait mode and pass the exact PR URL plus exact worktree or cwd
- do not interrupt the active worker for opacity alone

Closure policy:
- report the terminal outcome and hand back
```

## PR babysitter handoff

```text
Use $codex-orchestrator for one PR watch seam.
Also use $babysit-pr.

Objective:
- monitor the exact PR until actionable or terminal state and report only meaningful changes

Ownership:
- exact worktree: [absolute path]
- exact branch: [branch name if relevant]
- exact PR target: [full PR URL]
- mode: [read-only / write-allowed on the PR head branch]
- do not invent raw gh watch loops or detached watcher processes

Validation limits:
- keep validation narrow and branch-local
- prefer remote CI and GitHub status surfaces over heavy local test loops unless the prompt explicitly allows a small local fix validation

Waiting policy:
- if blocking on the watcher, use the babysitter skill's actionable wait mode so the command exits on meaningful or terminal state
- if you pause to patch, push, or rerun flaky checks, restart the same monitoring mode yourself in the same lane
- do not hide an endless stream behind wait-until-terminal

Closure policy:
- return only on actionable state, strict terminal state, or an explicit blocker that needs the parent
```

## Ops clerk handoff

```text
Use $codex-orchestrator for this seam.
Also use $ops-work-items or $ops-friction-reports as appropriate.

Objective:
- perform the exact exploratory Ops search, trawl, paging, or bookkeeping requested

Ownership:
- Ops only
- no repo edits

Waiting policy:
- use the lightest correct Ops path

Closure policy:
- return the smallest useful set of exact IDs or exact mutations, plus resulting status, so the parent can read directly known items itself
```
