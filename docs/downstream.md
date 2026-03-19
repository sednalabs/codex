# Downstream / Fork Notes

This fork tracks upstream `main` on the local `main` branch and carries additional patches on `carry/main`.
GitHub default branch is `carry/main` so downstream behavior is the repository landing view, while `main` stays pristine.

## Branch Policy

- `main`: fast-forward mirror of upstream `main` (no local commits)
- `carry/main`: upstream + downstream patches (merge-based carry-forward branch)
- do not push feature commits to `origin/main`
- use `git sync-main` to update `main` as an upstream mirror
- use `git sync-carry` to merge `upstream/main` into `carry/main` and push `origin/carry/main`
- avoid force-push on `carry/main` during normal sync; reserve `--force-with-lease` for exceptional repair only
- new feature branches: create from `carry/main` by default
- upstream-only compatibility/test probes: create from `main`, then cherry-pick to `carry/main` if retained downstream

## Divergence Summary

This section tracks intentional downstream behavior differences from `upstream/main`.
Last reviewed: 2026-03-15.

Current state at review time:
- `carry/main` is `135` commits ahead and `1` behind `upstream/main`
- `main` is currently 1 commit behind `upstream/main` and should be fast-forwarded via `git sync-main`

### Core + protocol: blocking wait for unified exec, stable wait output, and compaction turn-count metadata

Why:
- Support "wait until terminal" semantics directly on `exec_command` and `write_stdin` for long-running exact/tool-driven command flows.
- Avoid model-layer short-poll loops that waste turns, duplicate context, and make orchestration look busy without changing state.
- Let downstream operator workflows treat long-running shell work as an actual blocking join instead of repeated "check again" tool chatter.
- Keep wait responses aligned with the current unified-exec output shape after upstream refactors.
- Expose compaction count on turn completion so clients can distinguish "normal turn complete" from "turn completed after one or more compactions".

User-visible behavior:
- `exec_command` and `write_stdin` support blocking wait parameters (`wait_until_terminal`, `max_wait_ms`, `heartbeat_interval_ms`).
- `write_stdin` still requires `chars` to be empty when `wait_until_terminal=true`.
- Wait-timeout notes are appended to emitted `raw_output`, and token accounting is derived from the final response text.
- `TurnCompleteEvent` includes `compaction_events_in_turn`.
- In downstream operator environments, this pairs cleanly with other blocking coordination primitives such as `wait_agent` and build-helper `*_and_wait` flows, so agents can wait on real state transitions instead of spinning on repeated status polls.
- This downstream blocking MCP tool pattern predates fully operational task support and exists specifically so the tool layer, not the transcript, absorbs the wait.

### Usage ledger: shared ledger owned by `agent-usage-ledger`

Why:
- Downstream still participates in the shared usage ledger, but the schema, ingest, and reporting implementation now live in the dedicated `agent-usage-ledger` repo instead of this fork.
- Billing turns still need stable canonical identities and historical AUD cost reporting that upstream does not provide.

User-visible behavior:
- Shared usage-ledger scripts and docs live in [`agent-usage-ledger`](/home/grant/mmm/agent-usage-ledger).
- [usage-ledger.md](/home/grant/mmm/agent-usage-ledger/docs/usage-ledger.md) documents the ledger workflow.
- Billing turns are canonicalized before ingest, and historical AUD cost views remain available downstream through that shared repo.
- Patched Codex clients now emit authoritative local usage facts into `usage.sqlite`; rollout JSONL remains a compatibility fallback for historical or unpatched installs.

### MCP tool orchestration: blocking waits before task support matured

Why:
- Shared-host validation and release builds are more reliable when they run through build-helper MCP instead of ad hoc shell commands.
- The same downstream execution model should apply to build/test orchestration: prefer a blocking wait on a real task over repeated status polling from the model layer.
- Downstream operator workflows benefit when long-running MCP tool calls can block on a real state transition instead of relying on repeated model-driven status polling.
- This fork implemented blocking wait semantics before task support was fully operational, so agents could coordinate against terminal states without transcript churn.

User-visible behavior:
- `.build-helper/presets.json` defines fork-local Codex presets for formatting, core tests, and release build/install flows.
- Downstream instructions can reference those presets directly for reproducible validation and release steps.
- `codex.core-test` now maps to the progressive default path (`just core-test-progressive`), which runs compile, carry-divergence, and usage-ledger smoke gates before the larger codex-core suite.
- [`downstream-regression-matrix.md`](/home/grant/mmm/codex/docs/downstream-regression-matrix.md) maps each intentional divergence to a concrete smoke/progressive lane.
- For routine build-helper runs, downstream local guidance prefers `wait_until_terminal=true` so the tool layer, not the model transcript, absorbs the wait.
- Downstream docs and operator guidance prefer MCP tool surfaces that can block in-tool until useful state changes occur.
- The intended execution model is: start work, block on the tool contract, resume on a terminal or timeout condition, rather than simulate a scheduler in the chat transcript.

### Sub-agent model override precedence

Why:
- Preserve explicit `spawn_agent(model=..., reasoning_effort=...)` overrides even when launching a role-backed sub-agent, so downstream economical deployments do not drift back to upstream parent defaults.
- Let role defaults remain fixed when they intentionally set a model or reasoning effort, making the policy deterministic.

User-visible behavior:
- The spawn-agent response and inventory snapshot now report the advised `model`/`reasoning_effort` when a role does not lock those fields, so costs and capabilities stay under control.
- Roles that explicitly set `model`, `model_provider`, `model_reasoning_effort`, or `model_verbosity` continue to be authoritative, even when a child requests a different setting.
- Docs and tooling (e.g., `spawn_agent` spec, `docs/config.md`) now document the precedence stack.

Primary files:
- `codex-rs/core/src/agent/role.rs`
- `codex-rs/core/src/config/mod.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`
- `codex-rs/core/src/tools/spec.rs`
- `docs/config.md`

### TUI: safer interrupt handling for Alt/meta terminals (double-`Esc` by default)

Why:
- Some terminals (especially mobile/SSH flows) encode Alt/meta as an `Esc` prefix, which can accidentally interrupt running turns.

User-visible behavior:
- Running-turn interrupt defaults to `Esc Esc` confirmation.
- First `Esc` shows a confirmation hint (`Esc again to interrupt`) instead of interrupting immediately.
- Bare `Esc` release events and `Esc`-prefixed Alt sequences do not trigger unintended interrupts.
- `[tui].double_esc_interrupt` controls this behavior, with `CODEX_TUI_DOUBLE_ESC_INTERRUPT=0` as an override.

### MCP config: retain downstream safety controls while supporting upstream OAuth resource

Why:
- Preserve downstream MCP mutability controls while remaining compatible with upstream OAuth improvements.

User-visible behavior:
- Downstream safety fields remain available per server (`enable_elicitation`, `read_only`, `strict_tool_classification`, `require_approval_for_mutating`).
- Upstream `oauth_resource` is also supported in the same server config entry.

### TUI: Queue slash metadata preparation and recall

Why:
- Preserve slash-command arguments/metadata and make queued recall/edit paths consistent.

User-visible behavior:
- Queued slash commands and queued message drafts are shown in one queue preview.
- `Alt+Up` recalls queued items in strict reverse-chronological order across both entry types.
- `/status` remains immediate (not queued).
- Unavailable slash commands replay after the current task completes instead of being blocked.

### TUI: Weekly usage pacing signal + stale handling

Why:
- Show a compact weekly pacing indicator without displaying misleading percentages when snapshot data is stale.

User-visible behavior:
- Weekly status line shows `weekly {remaining:.0}%` as the base value.
- Fresh snapshot supports two pacing render modes:
  - default `qualitative`: `(on pace)`, `(over {n}%)`, or `(under {n}%)`
  - optional `ratio`: `{usage_remaining}%/{week_remaining}%`
- Stale snapshot shows `weekly {remaining:.0}% (stale)` and hides pace percentage.
- `[tui].weekly_limit_pacing_style` selects the fresh-snapshot render mode.
- `/status` and footer use the same stale predicate helper to keep stale behavior consistent.

### TUI: Interrupted-turn queue handling and queued model ordering

Why:
- Keep `Esc` interrupts from auto-submitting queued turns while still applying queued model switches promptly.
- Avoid stale model/effort on the next queued command when interrupt cleanup overlaps with MCP startup running-state.
- Keep explicit task-control commands immediate only when they should be.

User-visible behavior:
- On interrupt, queued user drafts are restored to the composer; non-model queued slash commands remain queued.
- Queued model selections are applied immediately during interrupt cleanup.
- Queued `/clear` remains queued while a task is running and is not executed during interrupt cleanup.
- `/quit` remains immediate while a task is running instead of being queued behind the active turn.
- Inline queued slash command arguments preserve expanded pending-paste payload content.

### Review + history: downstream accounting and runtime-context alignment

Why:
- Keep review token summaries, app-server history, and review-mode effort selection aligned with the live turn state rather than stale defaults.

User-visible behavior:
- Review token usage is aligned across live flows and app-server/history views.
- Review flows reuse the runtime turn effort and preserve downstream sampling rollout context needed for faithful reconstruction.

### Core: MCP forced approvals still participate in session remember keys

Why:
- Preserve Auto-mode approval-key caching even when a call is force-prompted.

User-visible behavior:
- Auto approval mode continues to use per-session remembered approvals for matching MCP tool calls, including force-prompted calls.
- Repeated calls can still be approved from the current session memory instead of always re-prompting.

### Core tests: unified_exec race-tolerant completed-process polling (test-only)

Why:
- Post-`exit` polling can race between final terminal response and process-store removal in test runs.

User-visible behavior:
- No product behavior change; this divergence only makes downstream core tests more tolerant of completion/polling races.

### TUI: disable `Ctrl+L` clear shortcut on carry branch

Why:
- Avoid accidental terminal/UI clears from `Ctrl+L` in local terminal workflows where the shortcut is easy to trigger unintentionally.

User-visible behavior:
- `Ctrl+L` no longer triggers terminal/UI clear behavior on `carry/main`.
- `/clear` remains available and unchanged for explicit "clear + start fresh chat" behavior.
