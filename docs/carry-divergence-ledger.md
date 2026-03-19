# Carry Divergence Ledger

This document records the current live divergences of `carry/main` from
`upstream/main`.

It is an audit ledger, not a changelog. Ahead-count alone is not evidence of a
live divergence.

## Audit Baseline

- Audited on: `2026-03-15`
- `upstream/main`: `4b31848f5b3adb7f237dd5109f83428fbd2cf343`
- `carry/main`: `e9b67a9ff9b6fc6b95a0b5294ff8adb02e64213c`
- `main`: `70eddad6b075f26f0f93c66f7ec9a4e49cdadc93`
- `carry/main` vs `upstream/main`: `135` ahead, `1` behind
- Carry-only commits at audit time: `103` non-merge, `32` merge
- Exact-subject upstream matches found during audit: `40`

## Audit Rules

- Count a live divergence only when the current `carry/main` tree still differs
  from `upstream/main`.
- Count generated schemas, snapshots, and inline test-module moves as
  derivative churn, not as standalone divergence items.
- Track exact-subject upstream matches separately as historical carry history.
- Treat the exact-subject upstream match list as a lower bound for "already
  upstreamed" history, not a complete semantic-duplicate detector.

## Current Live Divergences

### Fork Workflow And Operator Policy

- `carry/main` remains the default PR and integration branch, while `main`
  remains an upstream mirror.
- Downstream sync policy is merge-based, not rebase-based.
- Shared-host validation and release flows are standardized through build-helper
  presets.
- Divergence regression ownership is tracked in
  [`downstream-regression-matrix.md`](/home/grant/mmm/codex/docs/downstream-regression-matrix.md).
- Primary files:
  - `.build-helper/presets.json`
  - `docs/contributing.md`
  - `docs/downstream.md`

### External Usage Ledger Ownership

- Downstream usage-ledger workflow is intentionally externalized to
  `agent-usage-ledger`.
- Billing-turn canonicalization and historical AUD reporting remain downstream
  requirements, but the shared ledger implementation no longer lives in this
  repo.
- Primary files:
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

### CLI Git Metadata And Rebuild Triggers

### Sub-agent model override precedence

- Keep downstream itineraries that explicitly call `spawn_agent(model=..., reasoning_effort=...)` aligned with the requested model/economy, even when a role is applied.
- Roles still control locked models when they explicitly set `model`, `model_provider`, `model_reasoning_effort`, or `model_verbosity`, so downstream policy remains defendable.
- The documentation and tooling now spell out the precedence stack for spawn-time overrides.

Primary files:
- `codex-rs/core/src/agent/role.rs`
- `codex-rs/core/src/config/mod.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`
- `codex-rs/core/src/tools/spec.rs`
- `docs/config.md`

- CLI builds embed `git describe` metadata.
- CLI builds rerun when git state changes, including shared worktree git state.
- Primary files:
  - `codex-rs/cli/build.rs`
  - `codex-rs/cli/src/main.rs`

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
- In local downstream operator workflows, this composes with existing blocking
  coordination primitives such as `wait_agent` and build-helper `*_and_wait`
  calls so joins happen on state transitions rather than transcript churn.
- `TurnCompleteEvent` carries `compaction_events_in_turn`.
- Token-count events also carry provider and model context in downstream flow.
- Primary files:
  - `codex-rs/core/src/tools/handlers/unified_exec.rs`
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/core/src/codex.rs`
  - `docs/downstream.md`

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

### TUI Queue, Interrupt, And Weekly-Pacing Behavior

- Unavailable slash commands queue and replay after the current task instead of
  being rejected immediately.
- Interrupt handling defaults to double-`Esc` confirmation and preserves queued
  follow-ups and queued model changes coherently.
- Weekly status-line pacing keeps downstream stale handling and selectable
  render styles.
- Primary files:
  - `codex-rs/tui/src/app.rs`
  - `codex-rs/tui/src/bottom_pane/chat_composer.rs`
  - `codex-rs/tui/src/status/rate_limits.rs`
  - `docs/config.md`
  - `docs/tui-weekly-usage-pacing-status-line.md`
  - `docs/downstream.md`

### Code-Mode Declaration Formatting

- `carry/main` still emits imported tool declarations of the form:
  `import { tools } from "..."; declare function ...`
- `upstream/main` still emits the older inline
  `declare const tools: { ... }` example.
- This is a live carry-only divergence.
- This divergence is not currently summarized in `docs/downstream.md`.
- Primary files:
  - `codex-rs/core/src/tools/code_mode_description.rs`

## Not Counted As Standalone Live Divergences

- Merge and sync history:
  - `32` carry-only merge commits are sync history, not independent downstream
    behaviors.
- Merge-repair and promotion-fix history:
  - examples include `Fix carry/main core regressions after upstream sync`,
    `Fix carry/main promotion follow-ups`, and
    `Fix hybrid merge API drift in core/tui tests`
- Generated and derivative churn:
  - schema outputs under `codex-rs/app-server-protocol/schema/`
  - generated SDK outputs under `sdk/python/`
  - TUI snapshot updates under `codex-rs/tui/src/**/snapshots/`
- Structural test-only churn in large modules:
  - `codex-rs/core/src/plugins/manager.rs`
  - `codex-rs/core/src/config/edit.rs`
  - `codex-rs/core/src/tools/spec.rs`

## Known Stale Or Over-Broad Downstream Notes

- `docs/downstream.md` still lists `Ctrl+L` disablement as a carry divergence.
- That is not counted as a current live divergence in this ledger.
- At audit time, both `upstream/main` and `carry/main` implement the same
  task-running gate in `codex-rs/tui/src/chatwidget.rs`.

## Historical Carry Commits Now Upstream-Equivalent

The following carry commits have exact-subject matches on `upstream/main`. They
should not be treated as current fork-only behavior by title alone.

```text
8af97ce4b -> 7f2232938 | Revert "Pass more params to compaction" (#14298)
2cfa10609 -> fd4a67352 | Responses: set x-client-request-id as convesration_id when talking to responses (#14312)
78280f872 -> f385199cc | fix(arc_monitor): api path (#14290)
052ec629b -> 180a5820f | Add keyboard based fast switching between agents in TUI (#13923)
816e447ea -> 12ee9eb6e | Add snippets annotated with types to tools when code mode enabled (#14284)
cc417c39a -> a4d884c76 | Split spawn_csv from multi_agent (#14282)
5b10b93ba -> 39c1bc1c6 | Add realtime start instructions config override (#14270)
566897d42 -> 31bf1dbe6 | Make unified exec session_id numeric (#14279)
24b8d443b -> 01792a4c6 | Prefix code mode output with success or failure message and include error stack (#14272)
cec211cab -> da74da668 | render local file links from target paths (#13857)
3f7cb0304 -> c8446d7cf | Stabilize websocket response.failed error delivery (#14017)
567ad7faf -> 285b3a514 | Show spawned agent model and effort in TUI (#14273)
37f51382f -> 8a099b3df | Rename code mode tool to exec (#14254)
16daab66d -> e77b2fd92 | prompt changes to guardian (#14263)
f6e966e64 -> 9b5078d3e | Stabilize pipe process stdin round-trip test (#14013)
295b56bec -> c1a424691 | chore: add a separate reject-policy flag for skill approvals (#14271)
18199d4e0 -> 83b22bb61 | Add store/load support for code mode (#14259)
f8ef154a6 -> 2621ba17e | Pass more params to compaction (#14247)
de2a73cd9 -> 889b4796f | feat: Add additional macOS Sandbox Permissions for Launch Services, Contacts, Reminders (#14155)
e4bc35278 -> 8ac27b2a1 | Add ephemeral flag support to thread fork (#14248)
8b3348530 -> 07c22d20f | Add code_mode output helpers for text and images (#14244)
bf936fa0c -> ce1d9abf1 | Clarify close_agent tool description (#14269)
44bfd2f12 -> b1dddcb76 | Increase sdk workflow timeout to 15 minutes (#14252)
b73228722 -> a67660da2 | Load agent metadata from role files (#14177)
e79155902 -> 3d41ff0b7 | Add model-controlled truncation for code mode results (#14258)
c7e28cffa -> ee8f84153 | Add output schema to MCP tools and expose MCP tool results in code mode (#14236)
15163050d -> d5694529c | app-server: propagate nested experimental gating for AskForApproval::Reject (#14191)
28934762d -> 722e8f08e | unifying all image saves to /tmp to bug-proof (#14149)
2895d3571 -> 91ca20c7c | Add spawn_agent model overrides (#14160)
22d0aea5b -> 3d4628c9c | Add granular metrics for cloud requirements load (#14108)
2544bd02a -> d751e68f4 | feat: Allow sync with remote plugin status. (#14176)
bda9e55c7 -> f2d66fadd | add(core): arc_monitor (#13936)
01e2c3b8d -> b7f8e9195 | Add OpenAI Docs skill (#13596)
b33edebd6 -> 4ac604285 | Mark incomplete resumed turns interrupted when idle (#14125)
46e6661d4 -> c4d35084f | Reuse McpToolOutput in McpHandler (#14229)
77a02909a -> 52a7f4b68 | Stabilize split PTY output on Windows (#14003)
e52afd28b -> 00ea8aa7e | Expose strongly-typed result for exec_command (#14183)
e4edafe1a -> f9cba5cb1 | Log ChatGPT user ID for feedback tags (#13901)
9a501ddb0 -> 026cfde02 | Fix Linux tmux segfault in user shell lookup (#13900)
b90921eba -> 7144f84c6 | Fix release-mode integration test compiler failure (#13603)
```
