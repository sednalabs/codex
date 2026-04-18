# Carry Divergence Ledger

This document records the current live divergences of the downstream branch
(historically `carry/main`, now `main`) from `upstream/main`.

It is an audit ledger, not a changelog. Ahead-count alone is not evidence of a
live divergence.

## Audit Baseline

- Audited on: `2026-04-17`
- `upstream/main`: `fe7c959e90d46abb8311e4a0b369e6cb32bf337e`
- downstream branch `main` (`origin/main`): `88b12a0e145af4533b58cf1a8b67369795eb7786`
- mirror branch `upstream-main` (`origin/upstream-main`): `fe7c959e90d46abb8311e4a0b369e6cb32bf337e`
- `main` vs `upstream/main`: `624` ahead, `103` behind
- Downstream-only commits at audit time: `554` non-merge, `70` merge
- Patch-equivalent downstream commits in symmetric diff: `0`

## Audit Rules

- Count a live divergence only when the maintained downstream branch tree
  (historically `carry/main`, now `main`) still differs from `upstream/main`.
- Count generated schemas, snapshots, and inline test-module moves as
  derivative churn, not as standalone divergence items.
- Track exact-subject upstream matches separately as historical carry history.
- Treat the exact-subject upstream match list as a lower bound for "already
  upstreamed" history, not a complete semantic-duplicate detector.

## Current Live Divergences

### Fork Workflow And Validation Policy

- `main` is now the default PR and integration branch, while `upstream-main`
  is the exact upstream mirror.
- Downstream sync policy is merge-based, not rebase-based.
- Helper-backed local validation and release flows may be used when configured,
  but those presets are not a tracked repository contract.
- Divergence regression ownership is tracked in
  [`downstream-regression-matrix.md`](downstream-regression-matrix.md).
- Field-level native tool-surface deltas are summarized in
  [`downstream-tool-surface-matrix.md`](downstream-tool-surface-matrix.md).
- Future registry-plus-generation maintenance direction is captured in
  [`downstream-divergence-tracking.md`](downstream-divergence-tracking.md).
- Downstream guidance prefers MCP tool surfaces with blocking wait
  semantics over transcript-driven polling when the tool contract supports it.
- Primary files:
  - `docs/contributing.md`
  - `docs/downstream.md`

### First-Party Usage Ledger Ownership

- Downstream keeps usage-ledger ownership in this repo.
- Billing-turn canonicalization and historical AUD reporting remain downstream
  requirements, and the canonical local ledger implementation lives in
  `usage.sqlite` rather than an external sibling repository.
- `codex-rs/state/src/runtime/usage.rs` and
  `codex-rs/state/usage_migrations/0001_usage_tables.sql` do not currently
  have upstream counterparts, so future sync passes should treat them as
  downstream-owned behavior to preserve rather than as stale carry to delete.
- Usage-ledger ownership stays here: any upstream-native reimplementation must
  reproduce the downstream per-turn ledger, provider/token metadata, and
  billing-turn reporting semantics before the canonical source of truth can
  move out of this repository.
- Primary files:
  - `codex-rs/core/src/codex.rs`
  - `codex-rs/state/src/runtime.rs`
  - `codex-rs/state/src/runtime/usage.rs`
  - `codex-rs/state/usage_migrations/0001_usage_tables.sql`
  - `docs/downstream.md`

### Usage Event Logging And Metadata Capture

- Sessions record a downstream-only usage log database that tracks token,
  provider, tool, and spawn metadata per thread so downstream reporting can
  export a full list of agents, tool calls, weighting/effort metadata, rate-
  limit snapshots, and completion/forging regions for downstream billing and
  audit workflows.
- The new `usage` SQLite DB stores `usage_threads`, `usage_provider_calls`,
  `usage_tool_calls`, `usage_quota_snapshots`, `usage_spawn_requests`, and
  `usage_fork_snapshots`, capturing per-turn requested model/provider hints,
  tool invocation lifecycles, rate-limit snapshots, and parent/child thread
  relationships for spawn requests.
- Primary files:
  - `codex-rs/core/src/codex.rs`
  - `codex-rs/core/src/state/service.rs`
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/state/src/lib.rs`
  - `codex-rs/state/src/migrations.rs`
  - `codex-rs/state/src/runtime.rs`
  - `codex-rs/state/src/runtime/usage.rs`
  - `codex-rs/state/usage_migrations/0001_usage_tables.sql`
  - `codex-rs/state/Cargo.toml`

### Phase-2 Memory Attestation And Prepared-Input Fingerprinting

- Downstream phase-2 memory consolidation remains fail-closed once attestation
  support has been initialized for a memory root.
- Consolidated memory artifacts are fingerprinted against the prepared immutable
  input tree and the effective consolidator contract, then recorded in
  attestation sidecars plus runtime state so unchanged selections can safely
  reuse existing outputs while drifted or tampered artifacts are rejected.
- This is an intentional downstream carry, not derivative test churn: losing
  the attestation runtime while keeping the attestation tests is a regression.
- Primary files:
  - `codex-rs/core/src/memories/phase2.rs`
  - `codex-rs/core/src/memories/phase2_attestation_tests.rs`
  - `codex-rs/core/src/memories/tests.rs`
  - `codex-rs/state/src/runtime/phase2_attestation.rs`
  - `codex-rs/state/migrations/0024_phase2_attestation_roots.sql`
  - `docs/memories.md`

### Release Metadata And Rebuild Triggers

- Release builds embed canonical release identity plus compact provenance
  metadata.
- Version metadata rebuilds when git state changes, including shared worktree
  git state.
- Primary files:
  - `codex-rs/utils/version/build.rs`
  - `codex-rs/utils/version/src/lib.rs`
  - `codex-rs/cli/src/main.rs`

### Sub-agent orchestration override preservation, inventory metadata, and wait joins

- Upstream already supports explicit `spawn_agent(model=..., reasoning_effort=...)` child overrides; the live carry divergence is preserving those requests across role reload unless the role explicitly locks the fields.
- Keep downstream itineraries that explicitly call `spawn_agent(model=..., reasoning_effort=...)` aligned with the requested model/economy, even when a role is applied.
- Roles still control locked models when they explicitly set `model`, `model_provider`, `model_reasoning_effort`, or `model_verbosity`, so downstream policy remains defendable.
- Carry also preserves the requested `model_reasoning_summary`, so the summary the child asked for survives role reload unless a role or active profile explicitly locks it, and active-profile overrides that set these fields retain precedence across the split role/spawn path.
- `core/src/agent/role.rs` is now back on the upstream-native layered reload shape with resolved active-profile materialization; the remaining downstream delta is the deliberate sticky spawn-time override policy for model, reasoning effort, reasoning summary, and verbosity when the role does not own those fields.
- The live tool-contract schema in `codex-rs/core/src/tools/spec.rs` and the
  regression suite in `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`
  are already back on upstream-native shape; the remaining carry is
  concentrated in role application, descendant inventory, spawn result
  metadata, wait summaries, and `agent/control.rs`.
- Spawn-agent result and direct-child inventory reporting expose `role`, `status`, `identity_source`, `effective_model`, `effective_reasoning_effort`, and `effective_model_provider_id` after role application, so the surviving setting is visible.
- `list_agents` is a first-class inventory tool on `carry/main`: the live handler is already on the upstream `multi_agents_v2` path, and the stale downstream `multi_agents/list_agents.rs` copy was dead carry rather than active behavior.
- The remaining inventory divergence is therefore not a separate handler path; it is the extra descendant and persisted edge-status plumbing available from `agent/control.rs`, which still needs to be re-homed onto the upstream-native v2 inventory shape rather than dropped.
- Downstream policy is to preserve the intent of the live carry while keeping the tree as close to upstream as possible; we explicitly carry the always-on, cheap live `list_agents` surface (including `has_active_subagents`/`active_subagent_count` and nested visibility/status metadata) to keep nested-agent live visibility intact, pair it with a richer, potentially stale `inspect_agent_tree` surface for deeper inventory sweeps, and welcome upstream-native reimplementation whenever it preserves these behaviors with less divergence.
- `inspect_agent_tree` now surfaces the richer tree inspection contract: it can toggle `live` vs `stale` descendant visibility, focus on selected `agent_roots`, and returns compact depth/row-limited tree rows so downstream observability stays explicit without replaying bulky historical snapshots.
- `wait_agent` adds `return_when=any|all` plus `requested_ids`, `pending_ids`, `completion_reason`, and `timed_out` so downstream joins happen on explicit tool contracts rather than transcript polling.
- The built-in downstream awaiter profile also raises its default background timeout and prefers longer blocking waits plus `list_agents` snapshots over repeated short polling from the model layer.
- Primary files:
  - `codex-rs/core/src/agent/builtins/awaiter.toml`
  - `codex-rs/core/src/agent/role.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents/list_agents.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents/wait.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`
  - `codex-rs/core/src/tools/spec.rs`
  - `docs/config.md`
  - `docs/downstream-tool-surface-matrix.md`

### Dead-Cwd Absolute Path Handling

- `AbsolutePathBuf::from_absolute_path()` avoids consulting process cwd for
  already-absolute inputs.
- This preserves path resolution after cwd disappears.
- Primary files:
  - `codex-rs/utils/absolute-path/src/lib.rs`
  - `codex-rs/utils/absolute-path/tests/dead_cwd.rs`

### Blocking Unified-Exec Waits And Compaction-Aware Turn Completion

- `exec_command` and `write_stdin` support blocking wait semantics via
  `wait_until_terminal`, `max_wait_ms`, and `heartbeat_interval_ms`.
- `write_stdin` still requires an empty `chars` payload when
  `wait_until_terminal=true`.
- Timeout notes are appended to returned `raw_output`.
- The downstream intent is to absorb long-running shell waits in the tool layer
  instead of spending model turns on repeated short-poll status checks.
- In local downstream workflows, this composes with existing blocking
  coordination primitives such as `wait_agent` and helper-backed `*_and_wait`
  calls so joins happen on state transitions rather than transcript churn.
- This blocking MCP tool pattern was carried downstream before task support was
  fully operational.
- `TurnCompleteEvent` carries `compaction_events_in_turn`.
- Token-count events also carry provider and model context in downstream flow.
- Sub-agent delegate forwarding should continue to surface `TokenCount` events
  back to the parent session; preserve this behavior even when re-homing the
  delegate code onto newer upstream structure.
- Primary files:
  - `codex-rs/core/src/tools/handlers/unified_exec.rs`
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/core/src/codex.rs`
  - `docs/downstream.md`
  - `docs/downstream-regression-matrix.md`

### Review And History Accounting Alignment

- Review and history token summaries use a stable unavailable fallback string
  instead of ad hoc formatting.
- Review-mode accounting remains aligned with live runtime state rather than
  stale defaults.
- Primary files:
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/core/src/codex.rs`
  - `docs/downstream.md`

### MCP Server Safety Policy Extensions

- Downstream retains per-server safety controls:
  - `enable_elicitation`
  - `read_only`
  - `strict_tool_classification`
  - `require_approval_for_mutating`
- These coexist with upstream `oauth_resource` support.
- Primary files:
  - `codex-rs/core/src/config/types.rs`
  - `codex-rs/core/src/config/edit.rs`
  - `docs/config.md`
  - `docs/downstream.md`

### Startup Plugin Sync Bounded Wait And Curated-Repo Completion Re-Arm

- Startup remote plugin sync keeps the initial curated-marketplace prerequisite
  wait bounded to the startup race window, then parks the worker instead of
  dropping the attempt when curated-repo sync is still in flight.
- Curated-repo completion emits a signal that re-arms the parked worker so the
  remote reconciliation resumes without a second concurrent sync; that wake now
  happens on both success and failure paths.
- While the worker is parked, repeated startup/config-triggered attempts still
  collapse into a single in-process waiter, and they refresh the stored
  config/auth snapshot that the eventual reconciliation will use.
- The resumed reconciliation re-reads the latest snapshot before syncing, so
  late config/auth updates made during the wait are still applied.
- Primary files:
  - `codex-rs/core/src/plugins/startup_sync.rs`
  - `codex-rs/core/src/plugins/startup_sync_tests.rs`
  - `docs/downstream.md`
  - `docs/downstream-regression-matrix.md`

### TUI Session-State, Queue, Interrupt, And Usage Surfaces

- Per-thread approval/sandbox/reviewer overrides survive thread switches.
- Active-thread session state survives config refresh and fresh-session clones
  keep policy mutability before new-thread and fork flows.
- `/agent` picker rows expose per-thread used-token totals from cached thread
  usage without requiring a broader context-window plumbing pass.
- Combined session token totals remain visible across `/status` and
  footer/status-line surfaces without overwriting the active thread's own usage
  totals.
- Unavailable slash commands queue and replay after the current task instead of
  being rejected immediately.
- Interrupt handling defaults to double-`Esc` confirmation and preserves queued
  follow-ups and queued model changes coherently.
- Weekly status-line pacing keeps downstream stale handling and selectable
  render styles.
- Primary files:
  - `codex-rs/tui/src/app.rs`
  - `codex-rs/tui/src/multi_agents.rs`
  - `codex-rs/tui/src/bottom_pane/chat_composer.rs`
  - `codex-rs/tui/src/bottom_pane/status_line_setup.rs`
  - `codex-rs/tui/src/chatwidget.rs`
  - `codex-rs/tui/src/chatwidget/status_surfaces.rs`
  - `codex-rs/tui/src/status/card.rs`
  - `codex-rs/tui/src/status/rate_limits.rs`
  - `docs/config.md`
  - `docs/tui-weekly-usage-pacing-status-line.md`
  - `docs/downstream.md`
  - `docs/downstream-regression-matrix.md`

### Custom Prompt Discovery And Review Prompt Flow

- Downstream restores a file-backed custom prompt catalogue under
  `$CODEX_HOME/prompts`, including optional frontmatter metadata for prompt
  descriptions and argument hints.
- Downstream also preserves the live ad hoc custom-review prompt entry point in
  the TUI review flow, so users can still open the dedicated custom prompt
  view from the review popup and submit review text without losing the standard
  review interaction.
- Today the runtime wiring is clearest for the ad hoc `ReviewTarget::Custom`
  review path. The separate file-backed prompt catalogue remains a carried
  downstream surface, but it should not be described as fully reconnected to a
  user-facing picker until that runtime wiring is verified or intentionally
  restored.
- Primary files:
  - `codex-rs/core/src/custom_prompts.rs`
  - `codex-rs/core/src/custom_prompts_tests.rs`
  - `codex-rs/protocol/src/custom_prompts.rs`
  - `codex-rs/core/src/lib.rs`
  - `codex-rs/protocol/src/lib.rs`
  - `codex-rs/tui/src/app.rs`
  - `codex-rs/tui/src/app_event.rs`
  - `codex-rs/tui/src/bottom_pane/custom_prompt_view.rs`
  - `codex-rs/tui/src/chatwidget.rs`

### Code-Mode Declaration Formatting

- `main` still emits imported tool declarations of the form:
  `import { tools } from "..."; declare function ...`
- `upstream/main` still emits the older inline
  `declare const tools: { ... }` example.
- This is a live carry-only divergence.
- Primary files:
  - `codex-rs/core/src/tools/code_mode_description.rs`

## Not Counted As Standalone Live Divergences

- Merge and sync history:
  - `39` carry-only merge commits are sync history, not independent downstream
    behaviors.
- Merge-repair and promotion-fix history:
  - examples include `Fix main core regressions after upstream sync`,
    `Fix main promotion follow-ups`, and
    `Fix hybrid merge API drift in core/tui tests`
- Generated and derivative churn:
  - schema outputs under `codex-rs/app-server-protocol/schema/`
  - generated SDK outputs under `sdk/python/`
  - TUI snapshot updates under `codex-rs/tui/src/**/snapshots/`
- Structural test-only churn in large modules:
  - `codex-rs/core/src/plugins/manager.rs`
  - `codex-rs/core/src/config/edit.rs`
  - `codex-rs/core/src/tools/spec.rs`

## Historical Carry Commits Now Upstream-Equivalent

The following carry commits have exact-subject matches on `upstream/main`. They
should not be treated as current fork-only behavior by title alone.

```text
027afb885 -> 3b1c78a5c | [skill-creator] Add forward-testing instructions (#13600)
8b3348530 -> 07c22d20f | Add code_mode output helpers for text and images (#14244)
e4bc35278 -> 8ac27b2a1 | Add ephemeral flag support to thread fork (#14248)
22d0aea5b -> 3d4628c9c | Add granular metrics for cloud requirements load (#14108)
052ec629b -> 180a5820f | Add keyboard based fast switching between agents in TUI (#13923)
e79155902 -> 3d41ff0b7 | Add model-controlled truncation for code mode results (#14258)
01e2c3b8d -> b7f8e9195 | Add OpenAI Docs skill (#13596)
c7e28cffa -> ee8f84153 | Add output schema to MCP tools and expose MCP tool results in code mode (#14236)
5b10b93ba -> 39c1bc1c6 | Add realtime start instructions config override (#14270)
816e447ea -> 12ee9eb6e | Add snippets annotated with types to tools when code mode enabled (#14284)
2895d3571 -> 91ca20c7c | Add spawn_agent model overrides (#14160)
18199d4e0 -> 83b22bb61 | Add store/load support for code mode (#14259)
bda9e55c7 -> f2d66fadd | add(core): arc_monitor (#13936)
15163050d -> d5694529c | app-server: propagate nested experimental gating for AskForApproval::Reject (#14191)
295b56bec -> c1a424691 | chore: add a separate reject-policy flag for skill approvals (#14271)
bf936fa0c -> ce1d9abf1 | Clarify close_agent tool description (#14269)
e52afd28b -> 00ea8aa7e | Expose strongly-typed result for exec_command (#14183)
de2a73cd9 -> 889b4796f | feat: Add additional macOS Sandbox Permissions for Launch Services, Contacts, Reminders (#14155)
2544bd02a -> d751e68f4 | feat: Allow sync with remote plugin status. (#14176)
9a501ddb0 -> 026cfde02 | Fix Linux tmux segfault in user shell lookup (#13900)
b90921eba -> 7144f84c6 | Fix release-mode integration test compiler failure (#13603)
78280f872 -> f385199cc | fix(arc_monitor): api path (#14290)
44bfd2f12 -> b1dddcb76 | Increase sdk workflow timeout to 15 minutes (#14252)
b73228722 -> a67660da2 | Load agent metadata from role files (#14177)
e4edafe1a -> f9cba5cb1 | Log ChatGPT user ID for feedback tags (#13901)
566897d42 -> 31bf1dbe6 | Make unified exec session_id numeric (#14279)
b33edebd6 -> 4ac604285 | Mark incomplete resumed turns interrupted when idle (#14125)
f8ef154a6 -> 2621ba17e | Pass more params to compaction (#14247)
24b8d443b -> 01792a4c6 | Prefix code mode output with success or failure message and include error stack (#14272)
16daab66d -> e77b2fd92 | prompt changes to guardian (#14263)
37f51382f -> 8a099b3df | Rename code mode tool to exec (#14254)
cec211cab -> da74da668 | render local file links from target paths (#13857)
2cfa10609 -> fd4a67352 | Responses: set x-client-request-id as convesration_id when talking to responses (#14312)
46e6661d4 -> c4d35084f | Reuse McpToolOutput in McpHandler (#14229)
8af97ce4b -> 7f2232938 | Revert "Pass more params to compaction" (#14298)
567ad7faf -> 285b3a514 | Show spawned agent model and effort in TUI (#14273)
cc417c39a -> a4d884c76 | Split spawn_csv from multi_agent (#14282)
f6e966e64 -> 9b5078d3e | Stabilize pipe process stdin round-trip test (#14013)
77a02909a -> 52a7f4b68 | Stabilize split PTY output on Windows (#14003)
3f7cb0304 -> c8446d7cf | Stabilize websocket response.failed error delivery (#14017)
28934762d -> 722e8f08e | unifying all image saves to /tmp to bug-proof (#14149)
```
