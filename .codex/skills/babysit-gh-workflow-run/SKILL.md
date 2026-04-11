---
name: babysit-gh-workflow-run
description: Watch GitHub Actions workflow runs such as `validation-lab`, `sedna-heavy-tests`, or `sedna-branch-build` by run id or by workflow/ref; monitor status, summarize failures, and keep waiting until the run succeeds, fails, or needs operator action. Use the bundled watcher helper script instead of ad-hoc `gh` polling loops.
---

# GitHub Workflow Run Babysitter

## Objective

Babysit a GitHub Actions workflow run until one of these terminal outcomes occurs:

- the watched run succeeds
- the watched run fails or is cancelled and needs diagnosis
- the run remains blocked in a way that needs operator help

This skill is for workflow-run monitoring, not PR review/comment shepherding. Use `$babysit-pr` for PR-local CI/review loops. In the orchard split, this helper can be used directly in the parent thread for one blocking wait, inside `awaiter` for pure delegated waits, inside `terminal-babysitter` for monitored waits, or inside a cheap workflow shepherd lane when the seam is likely to need one bounded fix/rerun and resumed watch ownership.

## Operating Model

- Use the bundled launcher, `scripts/gh_workflow_run_watch`, as the monitoring surface. It will locate a Python interpreter even when `python3` is not already on `PATH`.
- The helper is intentionally stdlib-only at runtime: it still needs a Python interpreter, `gh`, and network access to GitHub/Gemini, but it does not require an extra Python package install.
- If the default interpreter search is not the one you want, set `GH_WORKFLOW_RUN_WATCH_PYTHON` to an explicit Python path.
- If Gemini summaries are noisy or not currently useful, set `GH_WORKFLOW_RUN_WATCH_DISABLE_GEMINI=1` or pass `--no-gemini-diagnosis`; the watcher will still return the structured failure bundle without making a Gemini call.
- If there is a single blocking helper-backed wait and no better concurrent parent work, run the helper directly in the parent thread instead of spawning a babysitter lane.
- If the seam is still a pure delegated wait after applying that parent-direct rule, prefer routing it to `awaiter` instead of `terminal-babysitter`.
- If the seam is likely to become “watch -> tiny fix or rerun -> resume,” use this helper inside a cheap workflow shepherd lane rather than pretending the seam is a pure babysitter wait.
- Prefer `--watch-until-action` when a sidecar should block until the run reaches a meaningful terminal or actionable state.
- When you want to stay in a blocking wait until every watched run is terminal (even if a failure shows up while it is still in progress), use `--watch-until-terminal` (alias `--wait-until-terminal`, equivalent to `--watch-until-action --require-terminal-run`).
- `--wait-for all_done` waits until all watched targets are non-idle, but it will keep polling if a surfaced `diagnose_run_failure` action still lacks retrievable logs.
- `--watch-until-action` now includes a default appearance warm-up window for workflow/ref targets, so the helper waits for GitHub dispatch lag before reporting that no matching run appeared.
- The watcher now treats an already-failed job inside an in-progress run as actionable by default; you no longer need to wait for the whole workflow run to turn terminal before handing the failure to a worker.
- If you want to hold until the watched run reports a completed status before surfacing that failure (e.g., when logs/annotations finish later), add `--require-terminal-run` to `--watch-until-action` so the helper stays in-progress until the run is terminal.
- When a surfaced action has been handled, resume the same watcher loop with `--ack-action <fingerprint>` so it suppresses that exact blocker and keeps waiting for the next one.
- Prefer multi-target mode with repeated `--target` for large pending-action batches so one babysitter can cover many runs.
- Use `--watch` only when actively consuming the JSONL stream in the foreground.
- Use `--once` for one-shot diagnosis or local debugging.
- When watching by workflow plus ref rather than exact run id, the helper follows the newest matching run automatically so superseded scratch/integration runs do not require manual handoff. Once that run id is known, the helper keeps following it directly for a few polls before re-entering `gh run list` discovery, which cuts steady-state polling cost without changing the exact-run path.
- For `validation-lab`, treat same-question scratch/integration reruns as `supersession_mode=auto` by default, but use explicit retained intent such as `compare`, `milestone`, or `retain` when a run is evidence you want to preserve rather than auto-supersede.
- `gh_dispatch_and_watch` now retries once without `supersession_*` inputs when a workflow rejects those fields, so plain workflows such as `rust-ci-full` can still use the helper safely.
- When a watched run publishes a `validation-summary` artifact, prefer that structured summary over raw log scraping.
- For `profile=frontier` validation runs, treat the summary artifact's blocker queue as the primary handoff surface before any raw log tail.

## Inputs

Accept any of the following:

- exact run id (legacy single target mode): `--run-id`
- workflow name or workflow file plus a ref
- optional head SHA pin when a workflow/ref target may have several recent runs
- optional host-ref when the run host branch differs from the logical ref (common for `workflow_dispatch` with an input ref)
- optional `--min-run-id` when using a workflow target directly to skip older stale matching runs
- no ref argument: infer the current branch when possible
- when passing a logical workflow input like `ref=target-branch`, `--ref auto` resolves to the
  repository default branch for dispatch while `--head-sha` keeps guarding the logical input ref
- optional Gemini model override with `--gemini-model` when a failure should be summarized
- optional `--no-gemini-diagnosis` to skip Gemini while still returning the structured diagnostic bundle
- optional `--gemini-diagnosis` to override `GH_WORKFLOW_RUN_WATCH_DISABLE_GEMINI=1` for one run

Multi-target mode:

- `--target "run-id=<id>"`
- `--target "workflow=<name>,ref=<ref>"`
- `--target "workflow=<name>,ref=<ref>,host-ref=<branch>"`
- `--target "workflow=<name>,ref=<ref>,head-sha=<sha>"`
- `--target "workflow=<name>,ref=<ref>,host-ref=<branch>,head-sha=<sha>,min-run-id=<run-id>"`
- repeat `--target` to watch multiple runs in one invocation

Optional:

- repo override
- explicit poll interval
- appearance timeout override with `--appearance-timeout-seconds`
- completion behavior with `--wait-for` when used with `--watch-until-action`
- hold until a failure target's run reaches status completed by adding `--require-terminal-run` with `--watch-until-action`
- terminal-wait semantics in one flag: `--watch-until-terminal` (alias `--wait-until-terminal`) implies both `--watch-until-action` and `--require-terminal-run`
- repeated dispatch input passthrough with `--input key=value` when using `gh_dispatch_and_watch`
- bounded stale-head redispatch with `--stale-head-retries` when branch propagation races produce runs on an older head SHA

## Core Workflow

1. Resolve targets:
   - exact `run-id` targets if present
   - newest matching workflow run for each workflow/ref target
   - optional `min-run-id` filters older matching runs with the same commit when present
2. Emit one normalized aggregate snapshot or enter the watch loop.
3. Inspect the top-level `targets` array and aggregate `actions` list.
4. If one or more targets are still queued or in progress, keep waiting unless wait policy is already satisfied.
5. If the watched target(s) succeed, report terminal success and stop per policy.
6. If any target fails, times out, or is cancelled, report the failure summary and stop per policy.
7. If a requested workflow/ref has no matching run yet, continue waiting unless the caller explicitly asked for a one-shot snapshot.
8. In `--watch-until-action` mode, if a workflow/ref target still has no matching run after the appearance timeout window, return a timeout action so the parent can decide whether to retry dispatch, inspect GitHub, or keep doing other work.

## Commands

### One-shot snapshot for the current branch

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --workflow validation-lab --ref auto --once
```

### One-shot multi-target snapshot

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --target "workflow=validation-lab,ref=auto" --target "workflow=sedna-heavy-tests,ref=auto" --once
```

### Actionable wait on the latest matching run

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --workflow validation-lab --ref integration/upstream-main-sync-20260330-000843 --watch-until-action
```

### Actionable wait with a custom appearance timeout

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --workflow validation-lab --ref integration/upstream-main-sync-20260330-000843 --watch-until-action --appearance-timeout-seconds 180
```

### Actionable wait pinned to one exact branch head

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --workflow validation-lab --ref integration/upstream-main-sync-20260330-000843 --head-sha c7f3212c1c --watch-until-action
```

### Terminal wait for watched runs to finish

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --workflow validation-lab --ref integration/upstream-main-sync-20260330-000843 --watch-until-terminal
```

### Resume after handling one actionable blocker

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --run-id 123456789 --watch-until-action --ack-action diagnose_run_failure:run:123456789:job:987654321:phase:in_progress_failed_job
```

### Actionable wait across many targets (any first, then all policy)

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --target "workflow=validation-lab,ref=auto" --target "run-id=123456789" --target "workflow=sedna-heavy-tests,ref=integration/upstream-main-sync-20260330-000843" --watch-until-action --wait-for first_action

~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --target "workflow=validation-lab,ref=auto" --target "workflow=sedna-heavy-tests,ref=integration/upstream-main-sync-20260330-000843" --watch-until-action --wait-for all_done
```

### Exact run id

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --run-id 123456789 --watch-until-action
```

### Continuous foreground watch

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch --workflow sedna-heavy-tests --ref auto --watch
```

### Deterministic dispatch + run selection

```bash
~/.codex/skills/babysit-gh-workflow-run/scripts/gh_dispatch_and_watch \
  --workflow validation-lab \
  --ref integration/upstream-main-sync-20260330-000843 \
  --head-sha c7f3212c1c \
  --input profile=frontier \
  --input lane_set=subagents \
  --max-wait-seconds 900 \
  --poll-seconds 10 \
  --wait-for first_action

~/.codex/skills/babysit-gh-workflow-run/scripts/gh_dispatch_and_watch \
  --workflow validation-lab \
  --ref integration/upstream-main-sync-20260330-000843 \
  --head-sha c7f3212c1c \
  --supersession-mode milestone \
  --supersession-key pre-merge-checkpoint \
  --wait-for all_done
Use `--min-run-id <id>` with `gh_dispatch_and_watch` when the target workflow/ref should ignore any matching run whose `databaseId` is below the given bound; the watcher helper already uses the same flag for workflow/ref targets.
```

## Output Expectations

Return concise structured state that includes:

- aggregate `repo`, `wait_for`, and `targets`
- per-target resolved repo/workflow/ref/run identifiers
- per-target run status and conclusion
- per-target `validation_summary` when the run uploaded a structured summary artifact
- per-target frontier blocker queue when the summary artifact exposes one
- per-target appearance-wait state when no run has appeared yet
- per-target failed-job summary when terminal and non-green
- per-target `gemini_diagnosis` when Gemini was able to summarize the failure
- per-target `diagnostic_evidence` even when Gemini diagnosis is disabled, so the parent still gets the compact failure bundle
- per-target `gemini_error` and `diagnostic_evidence` when the Gemini pass was attempted but could not complete cleanly
- per-target `gemini_telemetry` with Gemini latency and token usage when a diagnosis call completes
- per-target `validation_context` with mode-aware watcher guidance such as `profile`, `failure_structure`, `first_blocker`, and `recommended_follow_up`
- per-target `diagnosis_status` so the parent can distinguish `available`, `unavailable`, `disabled`, `skipped`, and `not_needed`
- per-target `action_triggers` / `action_fingerprints` with stable exact blocker identifiers such as run id, job id, failure phase, and resumable ack fingerprint
- per-target `alerts` when Gemini failed so the parent can notice the missing summary immediately
- whether each target followed a newer matching run
- top-level `actions` list that tells the parent whether to keep waiting or act
- for deterministic dispatch races, `stop_stale_head_dispatch_detected` with `stale_head_dispatch` details when newly created runs keep landing on the wrong head SHA
- for dispatch visibility failures, `stop_dispatch_run_not_visible` with `dispatch_visibility` details when no new run appears within the configured appearance timeout window
- for dispatch host/ref mismatches, `stop_dispatch_host_branch_mismatch` with `appearance_wait.dispatch_host_mismatch` details and a suggested `--target ... host-ref=...` form
- top-level `summary` counts across all targets
- `ts` timestamp

Gemini failure summaries are collected with the direct Gemini REST API using `gemini-3.1-flash-lite-preview` by default. The helper redacts obvious secrets, ranks failed jobs by likely causality, prefers focused excerpts from the primary failing job plus any useful meta-summary job, and tries to add local code snippets for likely file/line hits before it asks Gemini for a JSON diagnosis. When a `validation-summary` artifact exists, the watcher now derives a compact mode-aware summary from it instead of dumping the whole artifact into the prompt, so `targeted`, `frontier`, and checkpoint-like runs can steer the parent with less token spend. When the call succeeds, the watcher also returns a per-target `gemini_telemetry` block with request latency and the API's usage metadata so the parent can see token usage without scraping logs.

## Guardrails

- Do not replace the helper script with ad hoc `gh` polling loops.
- Do not use this skill for PR review/comment monitoring.
- Do not spawn another babysitter for the same workflow-run seam.
- Do not route a pure delegated workflow wait to `terminal-babysitter` when `awaiter` would be sufficient.
- Do not use a pure babysitter lane when the real seam is workflow watch plus bounded fix/rerun ownership.
- Prefer exact run ids when the parent already knows them.
- For a fresh dispatch, use `gh_dispatch_and_watch` to wait for remote branch tip sync, dispatch only when SHA matches, and watch only a newer matching run.
- For downstream-style dispatches with a logical `ref=` input, keep `--ref auto` so the workflow dispatches from the repo default branch; the helper validates the logical input ref head before dispatch and then watches the newly created host-branch run.
- When the parent only knows workflow plus ref, let the helper follow the newest matching run so cancelled superseded runs do not create noise.
- When the parent knows the exact branch head it just dispatched, pass `--head-sha` so the watcher cannot latch onto an older completed run on the same ref.
- If a `workflow_dispatch` run is hosted on a branch different from the logical ref (for example hosted on `main` while testing `validation/...` input), pass `--host-ref` (or `host-ref=` in `--target`) so the watcher can select it deterministically.
- Host-branch mismatch probing is throttled after the first no-match check, so repeated empty polls do not keep re-running the fallback discovery path on every cycle.
- When dispatching `validation-lab`, leave `--supersession-mode auto` unless the run is an intentional comparison or checkpoint. Retained evidence runs must opt out explicitly with `compare`, `milestone`, or `retain`.
- For `--watch-until-action`, the default appearance timeout is intentionally non-zero so GitHub dispatch lag does not cause an immediate false blocker; override it only when the seam really needs a shorter or longer grace window.

## Stop Conditions

Stop only when one of these is true:

- the watched run completed successfully
- the watched run completed unsuccessfully and the parent now needs the failure summary
- the run is blocked on a situation the parent must handle

Keep waiting when:

- no matching run exists yet for a workflow/ref target
- a workflow/ref target is still inside its appearance warm-up window
- one or more targets are queued or still in progress
- any target is still pending and `--wait-for all_done` is active
- a newer matching run appears and the helper switches to it
