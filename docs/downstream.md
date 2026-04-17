# Downstream / Fork Notes

This fork publishes downstream behavior on `main` and keeps an exact upstream mirror on
`upstream-main`.

## Branch Policy

- `main`: maintained downstream branch and public default branch
- `upstream-main`: fast-forward mirror of `upstream/main` (no local feature commits)
- do not push feature commits to `origin/upstream-main`
- downstream sync is merge-based (`upstream-main` -> `main`), not rebase-based
- `sedna-sync-upstream` fast-forwards the mirror and then runs the downstream divergence audit against the exact synced SHA.
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
git remote set-url origin git@github.com:sednalabs/codex.git
```

## Validation policy

- use tiny local sanity checks first (`git diff --check`, formatting, focused unit tests)
- use remote validation as the default measurement surface for substantive work
- `validation-lab` `profile=smoke`, `targeted`, and `frontier` are the default non-PR remote validation ladder
- PR and merge-group workflows are promotion surfaces rather than the default inner-loop validator
- helper-backed local runs are optional convenience infrastructure when available, not the tracked repository default
- heavy Rust tests, release-mode builds, and preview binaries should be offloaded to GitHub Actions after commit and push
- branch artifacts are disposable and retain for 3 days
- official releases are published only from the protected Sedna release workflow
- the authoritative divergence audit lives in `scripts/downstream-divergence-audit.py` and writes artifacts under `target/downstream-divergence-audit/`
- the intended-divergence registry lives at `docs/divergences/index.yaml`

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
- Let downstream interactive automation treat long-running shell work as an actual blocking join instead of repeated "check again" tool chatter.
- Keep wait responses aligned with the current unified-exec output shape after upstream refactors.
- Expose compaction count on turn completion so clients can distinguish "normal turn complete" from "turn completed after one or more compactions".

User-visible behavior:
- `exec_command` and `write_stdin` support blocking wait parameters (`wait_until_terminal`, `max_wait_ms`, `heartbeat_interval_ms`).
- `write_stdin` still requires `chars` to be empty when `wait_until_terminal=true`.
- Wait-timeout notes are appended to emitted `raw_output`, and token accounting is derived from the final response text.
- `TurnCompleteEvent` includes `compaction_events_in_turn`.
- Guardrails for the carry-only turn-complete compaction count currently live in `codex.app-server-protocol-test` (`preserves_compaction_only_turn`) plus broader `TurnCompleteEvent` shape coverage in `codex-core`, `codex-exec`, and `codex-tui` tests.
- Sub-agent delegate forwarding continues to emit `TokenCount` events back to the parent session, ensuring the downstream token accounting and provider/model metadata remain accurate even if upstream-native structures eventually rehost this carry.
- This pairs cleanly with other blocking coordination primitives such as `wait_agent` and helper-backed `*_and_wait` flows, so agents can wait on real state transitions instead of spinning on repeated status polls.
- This downstream blocking MCP tool pattern predates fully operational task support and exists specifically so the tool layer, not the transcript, absorbs the wait.

### Usage ledger: first-party local `usage.sqlite`

Why:
- Downstream keeps usage-ledger ownership in this repo so the CLI and runtime can emit authoritative local facts without depending on transcript reconstruction or an external sibling repository.
- Usage-ledger ownership stays here: any upstream-native reimplementation must replicate the canonical per-turn ledger, rate/provider metadata, and billing-turn reporting semantics before the ledger can move out of this repo.
- Billing turns still need stable canonical identities and historical AUD cost reporting that upstream does not provide.

User-visible behavior:
- Downstream builds maintain a local `usage.sqlite` alongside `state.sqlite` and `logs.sqlite` under `CODEX_SQLITE_HOME`.
- `usage.sqlite` is the authoritative local store for thread lineage, spawn metadata, tool calls, provider-call usage, quota snapshots, and fork snapshots.
- Billing turns are canonicalized before ingest, and downstream reporting can consume exact local facts directly from `usage.sqlite`.
- Rollout JSONL remains a compatibility fallback for historical or unpatched installs, not the primary ledger source.

### MCP tool orchestration: blocking waits before task support matured

Why:
- Validation and release work are more reliable when they run through a task-oriented tool surface instead of ad hoc shell commands.
- The same downstream execution model should apply to build/test orchestration: prefer a blocking wait on a real task over repeated status polling from the model layer.
- Downstream automation benefits when long-running MCP tool calls can block on a real state transition instead of relying on repeated model-driven status polling.
- This fork implemented blocking wait semantics before task support was fully operational, so agents could coordinate against terminal states without transcript churn.

User-visible behavior:
- Helper presets, when used, are environment-local convenience configuration rather than a tracked repo contract.
- When local presets are present, downstream instructions can reference them for reproducible validation and release steps in that environment.
- The default progressive path remains `just core-test-progressive`, which runs compile, carry-divergence, and usage-ledger smoke gates before the larger codex-core suite.
- [`downstream-regression-matrix.md`](downstream-regression-matrix.md) maps each intentional divergence to a concrete smoke/progressive lane.
- For helper-backed or other long-running tool calls, prefer `wait_until_terminal=true` so the tool layer, not the model transcript, absorbs the wait.
- Downstream docs prefer MCP tool surfaces that can block in-tool until useful state changes occur.
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
- Preserve those explicit child overrides at the spawn boundary, even when launching a role-backed sub-agent whose role file does not lock model/economy fields, so downstream economical deployments do not drift back to inherited parent-profile defaults during role reload.
- Surface the effective resolved child settings directly in the tool layer so callers can see what actually launched.
- Let downstream multi-agent orchestration block on clear tool contracts (`list_agents`, `inspect_agent_tree`, `wait_agent(return_when=...)`) instead of transcript polling.
- Upstream-native reimplementation is welcome when it preserves the live nested-agent visibility, the cheap `list_agents` surface, the richer `inspect_agent_tree` inspection, and the explicit blocking `wait_agent` contract so we can shrink the divergence without losing the downstream visibility model.

User-visible behavior:
- Explicit child `model` and `model_reasoning_effort` requests survive role application unless the selected role explicitly sets those fields or locks the summary, and the `model_reasoning_summary` is preserved internally so downstream metadata can keep the intended reasoning context even though it is not part of the tool response. The role reload itself stays on the upstream-native profile/provider path; the sticky child override carry now lives in the spawn handlers.
- `spawn_agent` returns `role`, `status`, `identity_source`, `effective_model`, `effective_reasoning_effort`, and `effective_model_provider_id`, letting callers see the resolved settings that actually launched after the role/profile overrides. That preserved `model_reasoning_summary` stays available through our internal metadata, not the raw tool response or inventory fields.
- Active-profile updates (parent/session config/role) that set `model`, `model_reasoning_summary`, or `model_reasoning_effort` continue to override child requests; the precedence stack is role-defined fields > active profile overrides > child requests, and the split between `core/src/agent/role.rs` and the spawn handlers encodes that boundary explicitly.
- The built-in `explorer` role no longer hard-locks a model or reasoning setting; instead the cheap-first policy lives in availability-aware `spawn_agent` behavior and supporting guidance so codebase-question lanes stay compatible with the caller's loaded model catalog.
- `list_agents` remains the always-on, cheap live inventory view across both collaboration surfaces rather than being hidden behind `MultiAgentV2`; it exposes `has_active_subagents` / `active_subagent_count` plus nested visibility/status metadata so callers retain nested-agent live visibility without dumping full trees.
- `inspect_agent_tree` is the intentionally richer downstream observability surface, separate from `list_agents`: it inspects the current subtree or a target path, can toggle `live` versus `stale` descendant visibility, can filter to selected branches with `agent_roots`, and returns compact tree rows with bounded depth and row limits.
- `wait_agent` supports `return_when=any|all` and returns `requested_ids`, `pending_ids`, `completion_reason`, and `timed_out`.
- Roles that explicitly set `model`, `model_provider`, `model_reasoning_effort`, or `model_verbosity` continue to be authoritative, even when a child requests a different setting.
- Docs and tooling now spell out the precedence stack and the intended `list_agents` / `inspect_agent_tree` / `wait_agent` workflow: cheap live view first to keep nested-agent visibility, compact nested or stale inspection when deeper context is needed, and blocking wait only when a transition must complete.

Primary files:
- `codex-rs/core/src/agent/role.rs`
- `codex-rs/core/src/agent/control.rs`
- `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_v2/list_agents.rs`
- `codex-rs/core/src/tools/handlers/inspect_agent_tree.rs`
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

### MCP OAuth: best-effort fallback credential recovery and atomic writes

Why:
- Keep MCP OAuth fallback credentials from becoming a brittle single point of failure when the keyring is unavailable or the fallback file is left empty/corrupt.
- Reduce auth churn during login and reconnect flows by treating the fallback file as best-effort recovery state instead of authoritative required state.
- Avoid partially-written replacement files by writing and syncing a temp file before the final rename.

User-visible behavior:
- Empty fallback credential files are treated as absent instead of fatal.
- If keyring loading fails and the fallback credential file is corrupt, downstream logs a warning and proceeds as though no cached OAuth credentials were available.
- Fallback credential writes are atomic temp-file replacements with explicit syncs, which reduces the chance of leaving a half-written file behind after interruption or crash.

### App-server transport: raw-byte websocket auth secrets

Why:
- Preserve support for binary websocket auth secret material instead of forcing UTF-8 text decoding and trimming.
- Keep the signed-bearer shared-secret path compatible with raw-byte secrets generated by external tooling.

User-visible behavior:
- Websocket auth secret files are read as raw bytes and ASCII-trimmed rather than decoded with `read_to_string`.
- Empty/whitespace-only secrets are still rejected.
- Capability-token auth continues to hash the trimmed secret bytes for comparison.

### App-server delivery/runtime: non-blocking output deltas and rich fs/watch policy

Why:
- Keep command streaming responsive by enqueueing output-delta notifications without waiting for transport write completion.
- Preserve watch-before-create registration, parent-event remapping, recursive directory watching, and changed-path dedupe for `fs/watch`.
- Keep these policy choices isolated behind the app-server extension seam rather than scattering the carry through protocol/replay code.

User-visible behavior:
- Streamed `command/exec/outputDelta` and `fs/changed` notifications are enqueue-only rather than transport-blocking.
- `fs/watch` can register a recursive parent watcher for not-yet-created targets, map parent events back onto the requested watch target, and dedupe repeated changed paths before notification delivery.
- The no-op upstream-style behavior still exists conceptually in `codex-rs/app-server/src/extensions.rs`, but downstream opts into the richer delivery/watch policy by default.

### TUI: Queue slash metadata preparation and recall

Why:
- Preserve slash-command arguments/metadata and make queued recall/edit paths consistent.

User-visible behavior:
- Queued slash commands and queued message drafts are shown in one queue preview.
- `Alt+Up` recalls queued items in strict reverse-chronological order across both entry types.
- `/status` remains immediate (not queued).
- Unavailable non-inline slash commands replay after the current task completes instead of being blocked.

### TUI: thread-session continuity and `/agent` / status accounting

Why:
- Preserve per-thread approval/sandbox/reviewer choices while moving between the main thread and subagents.
- Keep config refresh and fresh-session cloning from silently resetting the active thread's mutable session policy.
- Surface enough `/agent` and status-line accounting to explain per-thread versus combined-session usage without requiring a broader context/history pass.

User-visible behavior:
- Per-thread approval/sandbox/reviewer overrides survive thread switches.
- Active-thread session state survives config refresh and fresh-session clones keep policy mutability before new-thread/fork flows.
- `/agent` picker rows show per-thread used-token totals from cached thread usage.
- Combined session token totals remain visible across `/status` and footer/status-line surfaces without overwriting the active thread's own usage totals.

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
