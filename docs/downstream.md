# Downstream / Fork Notes

This fork publishes downstream behavior on `main` and keeps an exact upstream mirror on
`upstream-main`.

## Branch Policy

- `main`: maintained downstream branch and public default branch
- `upstream-main`: fast-forward mirror of `upstream/main` (no local feature commits)
- do not push feature commits to `origin/upstream-main`
- downstream sync is merge-based (`upstream-main` -> `main`), not rebase-based
- avoid force-push on `main` during normal sync; reserve `--force-with-lease` for exceptional repair only
- new feature branches: create from `main` by default
- upstream-only compatibility/test probes: create from `upstream-main`, then cherry-pick to `main` if retained downstream

## Local clone migration

If your clone still tracks the old carry-branch model, repoint it like this after the cutover:

```bash
git fetch origin --prune
git branch -m main upstream-main 2>/dev/null || true
git branch -m carry/main main 2>/dev/null || true
git branch -u origin/main main
git branch -u origin/upstream-main upstream-main
git switch main
```

If your `origin` remote still points at the personal namespace, update it:

```bash
git remote set-url origin git@github.com:SednaLabs/codex.git
```

## Validation policy

- local Build Helper runs are the default narrow lane for formatting, smoke tests, and targeted checks
- heavy Rust tests, release-mode builds, and preview binaries should be offloaded to GitHub Actions after commit and push
- branch artifacts are disposable and retain for 3 days
- official releases are published only from the protected Sedna release workflow

## Divergence Summary

This section tracks intentional downstream behavior differences from `upstream-main`.
References to `carry/main` elsewhere in the repo are historical pre-cutover baselines and should be
read as prior names for the maintained downstream branch.

Current state at the last validated pre-cutover review baseline (`5d474e652d91c7f371a28ad2069cc51a1c5b9ee8`):
- downstream branch (then `carry/main`, now `main`) was `175` commits ahead and `0` behind `upstream/main`
- mirror branch (then `main`, now `upstream-main`) matched `upstream/main`

Supporting docs:
- [`downstream-tool-surface-matrix.md`](downstream-tool-surface-matrix.md) captures the exact native tool-surface deltas that remain live on the downstream branch.
- [`downstream-divergence-tracking.md`](downstream-divergence-tracking.md) sketches the next-step registry and generation model for keeping these notes current as the fork grows.

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
- Guardrails for the carry-only turn-complete compaction count currently live in `codex.app-server-protocol-test` (`preserves_compaction_only_turn`) plus broader `TurnCompleteEvent` shape coverage in `codex-core`, `codex-exec`, and `codex-tui` tests.
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

### Code mode: imported tool declarations instead of inline `tools` const examples

Why:
- Keep downstream code-mode declarations aligned with the imported namespace pattern used by the current carry branch tool metadata exporter.
- Preserve the downstream formatting that pairs builtin and namespaced MCP tool metadata with a shared imported `tools` namespace instead of an inline `declare const tools` example.

User-visible behavior:
- Code-mode declarations use the imported form `import { tools } from "..."; declare function ...`.
- Builtin tool metadata and namespaced MCP tool metadata are documented and tested against the same imported namespace shape.
- Downstream code-mode examples therefore differ slightly from upstream examples that still inline `declare const tools: { ... }`.

### Sub-agent orchestration: override preservation, richer inventory, and blocking joins

Why:
- Upstream already supports explicit `spawn_agent(model=..., reasoning_effort=...)` child overrides, so the live downstream divergence is narrower than the historical carry title suggests.
- Preserve those explicit child overrides even when launching a role-backed sub-agent whose role file does not lock model/economy fields, so downstream economical deployments do not drift back to inherited parent-profile defaults during role reload.
- Surface the effective resolved child settings directly in the tool layer so operators can see what actually launched.
- Let downstream multi-agent orchestration block on clear tool contracts (`list_agents`, `wait_agent(return_when=...)`) instead of transcript polling.

User-visible behavior:
- Explicit child `model` and `model_reasoning_effort` requests survive role application unless the selected role explicitly sets those fields or locks the summary, and the `model_reasoning_summary` is preserved internally so downstream metadata can keep the intended reasoning context even though it is not part of the tool response.
- `spawn_agent` returns `role`, `status`, `identity_source`, `effective_model`, `effective_reasoning_effort`, and `effective_model_provider_id`, letting operators see the resolved settings that actually launched after the role/profile overrides. That preserved `model_reasoning_summary` stays available through our internal metadata, not the raw tool response or inventory fields.
- Active-profile updates (parent/session config/role) that set `model`, `model_reasoning_summary`, or `model_reasoning_effort` continue to override child requests; the precedence stack is role-defined fields > active profile overrides > child requests, and `core/src/agent/role.rs` contains the precise logic we rely on.
- `list_agents` is available to inspect direct-child inventory with the same provenance and effective-setting metadata, including `identity_source`; with `include_descendants=true`, it also surfaces persisted subtree rows and their `spawn_edge_status` (`open` or `closed`) even when those descendants are not currently live.
- `wait_agent` supports `return_when=any|all` and returns `requested_ids`, `pending_ids`, `completion_reason`, and `timed_out`.
- Roles that explicitly set `model`, `model_provider`, `model_reasoning_effort`, or `model_verbosity` continue to be authoritative, even when a child requests a different setting.
- Docs and tooling now spell out the precedence stack and the intended `list_agents`-before-`wait_agent` orchestration pattern.

Primary files:
- `codex-rs/core/src/agent/role.rs`
- `codex-rs/core/src/tools/handlers/multi_agents/list_agents.rs`
- `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`
- `codex-rs/core/src/tools/handlers/multi_agents/wait.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`
- `codex-rs/core/src/tools/spec.rs`
- `docs/config.md`
- `docs/downstream-tool-surface-matrix.md`

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

### Core: startup plugin sync uses a bounded race window and a curated-repo completion signal

Why:
- Upstream startup sync can miss curated marketplace reconciliation when the local curated repo finishes after process startup.
- Downstream keeps the initial startup wait bounded to the curated-repo sync window, then parks the single-flight worker until curated-repo completion explicitly re-arms it.

User-visible behavior:
- Startup remote plugin sync waits up to 30 seconds for curated marketplace prerequisites during the startup race window.
- If curated-repo sync finishes after that window, the existing worker is re-armed by a completion signal and resumes using the latest stored config/auth snapshot.
- That completion signal now fires on both curated-repo success and failure paths.
- Repeated startup/config-triggered sync attempts still collapse into a single in-process reconciliation per `codex_home`, so the remote sync does not run concurrently in duplicate.
- If curated-repo sync has not completed yet, the worker stays parked waiting for completion instead of dropping the attempt and missing the eventual reconciliation.

### Core tests: unified_exec race-tolerant completed-process polling (test-only)

Why:
- Post-`exit` polling can race between final terminal response and process-store removal in test runs.

User-visible behavior:
- No product behavior change; this divergence only makes downstream core tests more tolerant of completion/polling races.
