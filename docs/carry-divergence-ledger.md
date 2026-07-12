# Carry Divergence Ledger

This document records the current live divergences of the downstream branch
(historically `carry/main`, now `main`) from `upstream/main`.

It is an audit ledger, not a changelog. Ahead-count alone is not evidence of a
live divergence.

The snapshot below intentionally anchors to the audited code tree before the
docs-only refresh commit that records this snapshot.

## Audit Baseline

- Audited on: `2026-07-12`
- downstream branch `main` code tree: `772c74cacf80f52417a92ba099dd781c3e01e9b4`
- comparison basis: `mirror`
- mirror branch `upstream-main` (`origin/upstream-main`): `9e552e9d15ba52bed7077d5357f3e18e330f8f38`
- `upstream/main`: `9e552e9d15ba52bed7077d5357f3e18e330f8f38`
- downstream branch vs `upstream/main`: `1655` downstream ahead, `0` upstream ahead
- Mirror vs `upstream/main`: `0` ahead, `0` behind (`exact`)
- Downstream-only commits at audit time: `1451` unique, `0` patch-equivalent

## Audit Rules

- Count a live divergence only when the maintained downstream branch tree
  (historically `carry/main`, now `main`) still differs from `upstream/main`.
- Count generated schemas, snapshots, and inline test-module moves as
  derivative churn, not as standalone divergence items.
- Keep canonical config schema generation in `codex-config`; `codex-core`
  delegates to that implementation so Cargo and Bazel cannot normalize the
  same fixture through different code paths.
- Track exact-subject upstream matches separately as historical carry history.
- Treat the exact-subject upstream match list as a lower bound for "already
  upstreamed" history, not a complete semantic-duplicate detector.

## Current Live Divergences

### Fork Workflow And Validation Policy

- `main` is now the default PR and integration branch, while `upstream-main`
  is the exact upstream mirror.
- Downstream sync policy is merge-based, not rebase-based.
- Upstream now groups PR-blocking checks through reusable leaf workflows called
  by `blocking-ci.yml`. Downstream preserves that upstream topology and carries
  only the wrapper entrypoint expansion for `merge_group` and `upstream-main`
  pushes, instead of reintroducing direct triggers on every child workflow.
- Hosted Rust archive builders reclaim common Linux runner disk headroom before
  `cargo nextest archive`, build the `ci-test` archive payload with debug info
  disabled and symbols stripped, stay archive-only, and leave test execution
  to the archive-consuming `tests` and `remote_tests` jobs. Those replay jobs also
  install `bubblewrap` and reclaim hosted disk before archive extraction so
  sandbox and remote replay failures are not artifacts of runner packaging or
  disk pressure. The `remote_tests` replay job builds its Docker remote-env
  Codex binary in an isolated temporary Cargo target directory and removes
  those host-side build artifacts before replaying the shared nextest archive,
  so remote-env setup does not consume the extraction headroom needed by the
  227-binary archive. The archive uses default Cargo features, matching upstream
  and the non-sandbox V8 release artifact; explicit sandbox coverage remains in
  `v8-canary`. The `remote_tests` replay job keeps a 45-minute hosted budget so
  long archive download and remote-environment setup time does not masquerade
  as a product failure. Remote replay skips host-only compact/resume and hook
  fixtures, while Guardian's local proxy fixtures use a host-native cwd. The
  large-output summary remains host-only until exec-server replay preserves
  bounded head, tail, and omission metadata before core subscribes. The
  full-suite skill-loader fixture explicitly suppresses parent project layers
  so ambient hosted-runner repository markers cannot alter its non-Git case. The
  rust-ci-full summary parser records final nextest
  retry statuses so `TRY 1 FAIL` followed by `TRY 2 PASS` does not block, while
  persistent `TRY 2 FAIL` / `TRY 2 TIMEOUT` lines still appear in structured
  harvest artifacts. Validation-lab Rust batches reclaim target artifacts before
  the first lane and between later lanes when hosted disk falls below the safety
  floor, the link-heavy native computer-use tool-registry lane is weighted as a
  singleton batch, archive jobs skip sccache, and validation-lab Rust batches
  retry once on narrow Cargo registry transport failures such as crates.io
  HTTP/2 or EOF download flakes, and argument-comment lint retries once on the
  same narrow Cargo metadata/fetch failure class before reporting a lint blocker.
  Large validation-lab plans keep their resolved metadata in a runner-temp
  JSON file for per-field parsing and fingerprinting instead of exporting the
  complete plan through the process environment, so `full`, `broad`, and
  Frontier Max dispatches do not exceed the host exec environment limit.
  Runtime
  permission policy keeps the configured `codex_linux_sandbox_exe` readable
  under restricted filesystem profiles so GitHub-hosted archived nextest runs
  can re-enter the sandbox helper from extracted test binaries; the Linux bwrap
  launch path also adds the helper directory and `:minimal` system runtime roots
  to the outer bootstrap filesystem view before re-entering the inner seccomp
  stage. The
  workspace JWT dependency uses `jsonwebtoken` with the
  `aws_lc_rs` provider so hosted Cargo/Bazel `--locked` runs avoid pulling the
  RustCrypto RSA graph. Downstream dependency-policy validation preserves
  upstream's current `quick-xml` advisory shape: the direct workspace
  dependency is on the fixed line, and the remaining trusted transitive
  `plist`/`syntect` and `wayland-scanner`/`arboard` paths keep synchronized
  RustSec exceptions in `deny.toml` and `.cargo/audit.toml` until those
  upstream crates can use `quick-xml >=0.41.0`. The reqwest ownership ratchet
  names `codex-android-computer-use` as temporary downstream migration debt
  until its MCP transport moves behind `codex-http-client`; other new direct
  reqwest owners remain denied. Hosted macOS V8 staging, Bazel
  clippy, and Bazel
  release-build verification keep fanout below runner process/thread ceilings.
  Python SDK runtime-package staging rejects archive traversal, links, and
  special entries before writing ordinary package files beneath the staging
  root, without an unfiltered compatibility fallback on older Python runtimes.
  Hosted frontier argument-comment lint uses the prebuilt linter package so
  cold validation-lab runs do not spend the lane compiling V8/ICU before
  linting ordinary Rust call sites; V8 proof-of-concept buildability remains
  covered by build/test workflows. Hosted `rust-ci` callers pass `GH_TOKEN`
  through to the composite action so DotSlash can use authenticated
  `gh release download` fallback on Windows. Direct-runtime permission profiles
  stay on the bubblewrap/seccomp
  enforcement path when legacy Landlock is configured so sandbox validation
  fails safely instead of weakening policy. TUI carry smoke uses the same
  hosted test stack floor as core carry smoke so frontier/checkpoint validation
  can stay on GitHub hosted compute instead of falling back to local compute.
  Remote executor sweeps skip host-local managed-network approval and denial
  fixtures until the remote harness provides a proxy endpoint reachable from the
  target process; environment-specific approval scoping remains covered by unit
  tests and host-local integration. Compact/resume rollback fixtures keep their
  event wait above nextest's 30-second slow threshold so hosted remote replay
  load does not masquerade as a product hang.
- The Bazel crate macro accepts and forwards optional unit-test arguments so
  upstream's serialized exec-server unit-test declaration remains analyzable
  until equivalent macro support lands upstream.
- Windows hosted setup prefers a real Dev Drive but falls back to an existing
  secondary or system volume when the runner image lacks Dev Drive formatting,
  so validation does not fail before the requested command starts.
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
  - `.github/scripts/run_validation_lane_batch.py`
  - `.github/scripts/rusty_v8_bazel.py`
  - `.github/scripts/test_ci_planners.py`
  - `.github/workflows/blocking-ci.yml`
  - `.github/workflows/bazel.yml`
  - `.github/workflows/rust-ci-full.yml`
  - `.github/workflows/v8-canary.yml`
  - `defs.bzl`
  - `codex-rs/core/src/config/mod.rs`
  - `codex-rs/core/src/config/permissions.rs`
  - `codex-rs/core/src/config/permissions_tests.rs`
  - `codex-rs/linux-sandbox/src/linux_run_main.rs`
  - `codex-rs/Cargo.toml`
  - `codex-rs/Cargo.lock`
  - `docs/contributing.md`
  - `docs/downstream.md`

### Python Code Quality Corrections

- Downstream carries three upstreamable Python maintenance corrections so
  GitHub Code Quality can verify the corresponding findings are closed on
  `main`.
- The Windows timeout security smoke exceeds the harness deadline, records its
  expected `subprocess.TimeoutExpired`, and allows unexpected failures to
  surface before checking that outside writes remain denied.
- The Unix `just` shell launcher declares its `os.execvp` path as non-returning,
  and the issue-digest collector no longer retains an unused derived local.
- The same maintenance change removes an unused list from the downstream
  divergence audit; that script remains covered by the existing branch-policy
  carry rather than this upstream-fix record.
- Preserve these corrections during upstream syncs until equivalent control
  flow lands upstream. Once it does, drop the redundant patch and this carry
  record.
- Primary files:
  - `codex-rs/windows-sandbox-rs/sandbox_smoketests.py`
  - `scripts/just-shell.py`
  - `.codex/skills/codex-issue-digest/scripts/collect_issue_digest.py`

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
  - `codex-rs/core/src/session/session.rs`
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
- `usage_provider_calls` also stores provider-confirmed `final_model` and
  `model_snapshot` values when turn completion reports them, preserving the
  downstream distinction between requested/configured model, historical
  `actual_model_used`, and final provider identity. Core turn completion
  captures terminal `ResponseEvent::ServerModelIdentity` values so app-server,
  TUI, and usage-ledger consumers receive provider-confirmed identity instead
  of falling back to `None`.
- Completed thread/list/read and TUI status surfaces prefer thread-local
  provider identity evidence from turn completion or the usage ledger before
  falling back to configured session metadata; active/running threads keep the
  live effective model first so sub-agent status does not regress to the
  parent/session model.
- Primary files:
  - `codex-rs/core/src/session/turn.rs`
  - `codex-rs/core/src/session/turn_context.rs`
  - `codex-rs/core/src/tasks/mod.rs`
  - `codex-rs/app-server/src/request_processors/thread_processor.rs`
  - `codex-rs/core/src/state/service.rs`
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/state/src/lib.rs`
  - `codex-rs/state/src/migrations.rs`
  - `codex-rs/state/src/runtime.rs`
  - `codex-rs/state/src/runtime/usage.rs`
  - `codex-rs/state/usage_migrations/0001_usage_tables.sql`
  - `codex-rs/state/usage_migrations/0003_usage_provider_call_model_identity.sql`
  - `codex-rs/tui/src/chatwidget/status_surfaces.rs`
  - `codex-rs/tui/src/session_resume.rs`
  - `codex-rs/state/Cargo.toml`

### Side Chat Persistence And Usage Ledger Tracking

- `/side` conversations are persisted as side-tagged fork threads instead of
  pathless ephemeral forks, so they keep rollout transcripts, remain resumable,
  and can be forked by thread id or rollout path.
- Default history/list/search surfaces hide side chats; explicit
  `threadSources: ["side"]` requests expose the side-chat history class.
- Usage-ledger lineage records side forks in `usage_threads` and
  `usage_fork_snapshots`, marks `usage_threads.thread_source = "side"`, and
  writes normal provider-call rows for side turns.
- Forks created from an existing side conversation inherit the side
  `thread_source` unless the caller explicitly supplies a different source,
  keeping nested side-chat forks hidden from default history surfaces and
  marked in usage-ledger lineage.
- `scripts/codex-resume-recent.sh` skips side chats by default, with
  `--include-side` available when an operator deliberately wants side-chat
  resume candidates.
- Primary files:
  - `codex-rs/tui/src/app/side.rs`
  - `codex-rs/app-server/src/request_processors/thread_processor.rs`
  - `codex-rs/app-server/src/filters.rs`
  - `codex-rs/state/src/runtime/usage.rs`
  - `codex-rs/state/usage_migrations/0002_usage_thread_source.sql`
  - `scripts/codex-resume-recent.sh`

### App-server Thread Source And History Mode Compatibility

- Preserve downstream `thread_source` provenance alongside upstream
  `history_mode` metadata in thread listing, summary, resume, persisted
  metadata, and generated protocol schemas.
- These fields are independent dimensions: history storage mode must not erase
  whether a thread came from a side conversation, sub-agent, or another
  attributed source.
- Primary files:
  - `codex-rs/protocol/`
  - `codex-rs/rollout/`
  - `codex-rs/state/`
  - `codex-rs/thread-store/`
  - `codex-rs/app-server-protocol/`
  - `codex-rs/app-server/tests/suite/conversation_summary.rs`

### Phase-2 Memory Attestation And Prepared-Input Fingerprinting

- Downstream phase-2 memory consolidation remains fail-closed once attestation
  support has been initialized for a memory root.
- Consolidated memory artifacts are fingerprinted against the prepared input
  tree, the effective consolidator contract, and the output tree, then recorded
  in runtime state so unchanged workspaces can safely reuse existing outputs
  while drifted or tampered artifacts are rejected after bootstrap.
- This is an intentional downstream carry, not derivative test churn: losing
  the attestation runtime while keeping the attestation tests is a regression.
- Because these downstream state migrations occupy slots that upstream did not
  have at the time they were introduced, later upstream migrations may need to
  be replayed into the next free downstream migration version while preserving
  their SQL content. Current examples include upstream's device-key binding
  table (`0028_device_key_bindings.sql` upstream, `0031_device_key_bindings.sql`
  downstream) and upstream's thread-goals table (`0029_thread_goals.sql`
  upstream, `0032_thread_goals.sql` downstream), avoiding collisions with the
  already-shipped downstream `0028` through `0031` migration versions. When an
  upstream sync collides with already-shipped downstream state migration
  versions, keep upstream migration numbers when possible and move downstream
  additive carry to the next free version with checksum-gated runtime repair
  for databases that already recorded the old version. The current example is
  preserving upstream `0040_threads_history_mode.sql` while moving downstream
  visible-thread sort indexes to `0044_threads_visible_sort_indexes.sql`.
- Primary files:
  - `codex-rs/memories/write/src/phase2.rs`
  - `codex-rs/memories/write/src/phase2_attestation.rs`
  - `codex-rs/memories/write/src/phase2_attestation_tests.rs`
  - `codex-rs/memories/write/src/startup_tests.rs`
  - `codex-rs/state/src/migrations.rs`
  - `codex-rs/state/src/runtime/migration_repair.rs`
  - `codex-rs/state/src/runtime/phase2_attestation.rs`
  - `codex-rs/state/migrations/0024_phase2_attestation_roots.sql`
  - `codex-rs/state/migrations/0038_phase2_attested_baselines.sql`
  - `codex-rs/state/migrations/0031_device_key_bindings.sql`
  - `codex-rs/state/migrations/0032_thread_goals.sql`
  - `codex-rs/state/migrations/0044_threads_visible_sort_indexes.sql`
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
- Spawn-agent tool guidance should follow upstream's authorization wording that
  a user request or applicable `AGENTS.md`/skill instruction can authorize
  delegation, and should keep upstream's warning that `model` overrides are
  exceptional. Downstream additionally keeps the guardrail that requests for
  depth, thoroughness, research, investigation, or detailed codebase analysis
  do not by themselves authorize spawning.
- Keep downstream itineraries that explicitly call `spawn_agent(model=..., reasoning_effort=...)` aligned with the requested model/economy, even when a role is applied.
- Roles still control locked models when they explicitly set `model`, `model_provider`, `model_reasoning_effort`, or `model_verbosity`, so downstream policy remains defendable.
- Carry also preserves the requested `model_reasoning_summary`, so the summary the child asked for survives role reload unless a role or active profile explicitly locks it, and active-profile overrides that set these fields retain precedence across the split role/spawn path.
- `core/src/agent/role.rs` is now back on the upstream-native layered reload shape with resolved active-profile materialization; the remaining downstream delta is the deliberate sticky spawn-time override policy for model, reasoning effort, reasoning summary, and verbosity when the role does not own those fields.
- The live tool-contract schema in
  `codex-rs/core/src/tools/handlers/multi_agents_spec.rs` and
  `codex-rs/core/src/tools/spec_plan.rs`, plus the regression suite in
  `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`, are already back
  on upstream-native shape; the remaining carry is
  concentrated in role application, descendant inventory, spawn result
  metadata, wait summaries, and `agent/control.rs`.
- The historical `spawn_approval` argument was unused by both spawn handlers;
  the upstream removal is retained rather than carried as a phantom contract.
- The v1 spawn result retains upstream `agent_id`/`nickname`. The v2 result exposes canonical `task_name`, conditionally visible `agent_id`/`nickname`, and the requested/effective model and reasoning fields after role application. Role, status, identity source, provider ID, and reasoning summary remain inventory or internal metadata rather than spawn-result fields.
- V2 requires `task_name`; when no effective reasoning effort is known it
  serializes `null` rather than manufacturing a `medium` value. Wait completion
  derives pending target ids from refreshed status snapshots, not the original
  requested-id list.
- `list_agents` is a first-class inventory tool on `carry/main`: the live handler is already on the upstream `multi_agents_v2` path, and the stale downstream `multi_agents/list_agents.rs` copy was dead carry rather than active behavior.
- The remaining inventory divergence is therefore not a separate handler path; it is the extra descendant and persisted edge-status plumbing available from `agent/control.rs`, which still needs to be re-homed onto the upstream-native v2 inventory shape rather than dropped.
- Downstream policy is to preserve the intent of the live carry while keeping the tree as close to upstream as possible; we explicitly carry the always-on, cheap live `list_agents` surface (including `has_active_subagents`/`active_subagent_count` and nested visibility/status metadata) to keep nested-agent live visibility intact, pair it with a richer, potentially stale `inspect_agent_tree` surface for deeper inventory sweeps, and welcome upstream-native reimplementation whenever it preserves these behaviors with less divergence.
- `inspect_agent_tree` now surfaces the richer tree inspection contract: it can toggle `live` vs `stale` descendant visibility, focus on selected `agent_roots`, and returns compact depth/row-limited tree rows so downstream observability stays explicit without replaying bulky historical snapshots.
- `wait_agent` adds `return_when=any|all` plus `requested_ids`, `pending_ids`,
  `completion_reason`, and `timed_out` so downstream joins happen on explicit
  tool contracts rather than transcript polling. These completion fields are
  public tool-output-only; canonical transcript items retain target identities
  and agent-state snapshots without duplicating timeout, mailbox, or pending
  outcome state. The v2 schema also permits omitting `targets` when the caller
  intentionally wants a current-turn input-activity wait, including mailbox
  delivery or user steering, or timeout.
- The built-in downstream awaiter profile also raises its default background timeout and prefers longer blocking waits plus `list_agents` snapshots over repeated short polling from the model layer.
- Primary files:
  - `codex-rs/core/src/agent/builtins/awaiter.toml`
  - `codex-rs/core/src/agent/role.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_v2/list_agents.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents/wait.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_spec.rs`
  - `codex-rs/core/src/tools/spec_plan.rs`
  - `codex-rs/core/src/tools/tool_runtime_capabilities.rs`
  - `docs/config.md`
  - `docs/downstream-tool-surface-matrix.md`

### Dead-Cwd Absolute Path Handling

- `AbsolutePathBuf::from_absolute_path()` avoids consulting process cwd for
  already-absolute inputs.
- This preserves path resolution after cwd disappears.
- Primary files:
  - `codex-rs/utils/absolute-path/src/lib.rs`
  - `codex-rs/utils/absolute-path/tests/dead_cwd.rs`

### Session Environment And Thread-Tail State

- Session environment updates validate duplicate and unknown environment ids
  before mutating stored session state.
- When a session cwd/environment update changes the legacy fallback cwd,
  sticky environment selections retarget to that cwd instead of retaining stale
  path selections.
- Default turns refresh runtime `ThreadEnvironments` from stored selections so
  explicit empty or non-fallback stored environments are honored.
- Mailbox deferral must not overtake explicit steered user input, while
  response-only queued items may still defer after an answer boundary.
- Legacy active turns that only contain `UserMessageEvent` tails are still
  treated as mid-turn so replay/fork state does not discard the active start.
- Primary files:
  - `codex-rs/core/src/session/input_queue.rs`
  - `codex-rs/core/src/session/mod.rs`
  - `codex-rs/core/src/session/session.rs`
  - `codex-rs/core/src/session/turn_context.rs`
  - `codex-rs/core/src/thread_manager.rs`

### Blocking Unified-Exec Waits And Compaction-Aware Turn Completion

- `exec_command` and `write_stdin` support blocking wait semantics via
  `wait_until_terminal`, `max_wait_ms`, and `heartbeat_interval_ms`.
- `write_stdin` still requires an empty `chars` payload when
  `wait_until_terminal=true`.
- Spec and regression guardrails also cover the surfaced wait fields, reject
  invalid `wait_until_terminal` types, and enforce the empty-`chars`
  requirement for blocking `write_stdin`.
- Timeout notes are appended to returned `raw_output`.
- Canonical command-execution items retain optional live `terminal_wait`
  metadata so upstream item-lifecycle adapters reproduce it on legacy begin
  and end events and app-server v2 projects it into command items. History
  reconstruction uses `None` when older persisted items do not contain that
  live-only detail.
- The downstream intent is to absorb long-running shell waits in the tool layer
  instead of spending model turns on repeated short-poll status checks.
- Code-mode nested `exec_command` output follows the same model-policy bounded
  unified-exec summary shape before JavaScript observes `result.output`; do not
  restore raw large-output preservation expectations in code-mode tests when
  the tool response already carries truncation warning headers.
- Remote unified-exec command resolution must keep the session/user shell for
  commands that omit `shell`, while matching explicit aliases such as
  `powershell` reuse the selected environment shell instead of resolving that
  alias on the host running Codex.
- Code mode may expose the read-only `get_context_remaining` helper so scripts
  can inspect remaining budget, but interactive direct-model-only tools such as
  `request_user_input` remain hidden from nested execution.
- In local downstream workflows, this composes with existing blocking
  coordination primitives such as `wait_agent` and helper-backed `*_and_wait`
  calls so joins happen on state transitions rather than transcript churn.
- This blocking MCP tool pattern was carried downstream before task support was
  fully operational.
- `TurnCompleteEvent` retains upstream's optional structured terminal `error`
  payload alongside downstream `compaction_events_in_turn`, `final_model`, and
  `model_snapshot` metadata. Future conflict resolution must keep this additive
  union rather than choosing either field set.
- Token-count events also carry provider and model context in downstream flow.
- Sub-agent delegate forwarding should continue to surface `TokenCount` events
  back to the parent session; preserve this behavior even when re-homing the
  delegate code onto newer upstream structure.
- Provider `ServerOverloaded` responses use cancellable capacity-retry backoff
  for sampling, inline compaction, and remote compaction instead of terminating
  the turn immediately. Preserve the `capacity_retry` loops in
  `session/turn.rs`, `compact.rs`, and `compact_remote.rs` during upstream
  syncs.
- Primary files:
  - `codex-rs/core/src/capacity_retry.rs`
  - `codex-rs/core/src/session/turn.rs`
  - `codex-rs/core/src/compact.rs`
  - `codex-rs/core/src/compact_remote.rs`
  - `codex-rs/core/src/tools/spec_plan.rs`
  - `codex-rs/core/src/tools/handlers/unified_exec.rs`
  - `codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs`
  - `codex-rs/core/src/tools/events.rs`
  - `codex-rs/app-server/src/bespoke_event_handling.rs`
  - `codex-rs/app-server-protocol/src/protocol/v2/item.rs`
  - `codex-rs/core/tests/suite/code_mode.rs`
  - `codex-rs/core/tests/suite/remote_env.rs`
  - `codex-rs/core/tests/suite/unified_exec.rs`
  - `codex-rs/protocol/src/items.rs`
  - `codex-rs/protocol/src/legacy_events.rs`
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/core/src/session/mod.rs`
  - `docs/downstream.md`
  - `docs/downstream-regression-matrix.md`

### App-Server Remote Control Account Wake

- Remote-control enrollment waits must wake when cached ChatGPT auth changes
  the account id, not only when refresh-token material changes.
- `AuthManager::auth_change_receiver()` is therefore account-scoped for
  request recovery, while `auth_changed_for_refresh` remains the narrower
  token-refresh decision.
- This prevents remote control from sleeping until the retry interval after an
  account-id-only reload, and keeps `UnauthorizedRecovery` aligned with the
  fresh auth state before reconnect/enroll attempts.
- Primary files:
  - `codex-rs/app-server-transport/src/transport/remote_control/websocket.rs`
  - `codex-rs/login/src/auth/manager.rs`

### Native Computer-Use Adapter Bridge

- Downstream promotes bare `android_observe`, `android_step`,
  `android_install_build_from_run`, `browser_observe`, `browser_step`,
  `desktop_observe`, and `desktop_step` dynamic tools into first-party native
  computer-use function tools with Codex-owned schemas.
- Namespaced Android-like, browser-like, or desktop-like tools remain ordinary dynamic tools
  so app-specific providers can keep their own tool surfaces without taking
  over the native Codex contract.
- App-server `dynamicTools` accepts a deferred bare native tool so it can be
  discovered through `tool_search`; the same bare deferred shape remains
  invalid for ordinary dynamic tools. A capability-bearing native tool forces
  a loaded-thread resume reload because its provider contract may have changed.
- This sync intentionally retains the flat `DynamicToolSpec` compatibility
  shape (`namespace` plus function metadata) because app-server requests,
  persisted SQLite rows, provider registries, and resume filtering still share
  that representation. Upstream's tagged function/namespace model should land
  only through the tracked Dynamic Tools alignment work with lossless legacy
  ingestion and state migration, not as an incidental conflict resolution.
- `codex-core` owns `ComputerUseCallRequest` and
  `ComputerUseCallResponse` events, pending response registration, timeout
  cleanup, success/error projection, adapter selection, mutating
  classification, install-specific timeout selection, and hook payload
  formatting.
- App-server API v2 owns `item/computerUse/call`, response forwarding, and
  `ThreadItem::ComputerUseCall` start/completion projection.
- The active TUI session renders native computer-use items from live protocol
  events. Computer-use events are transient, so thread history and snapshots do
  not replay them after resume. The TUI provider registry handles Android and
  routes browser calls to either a configured provider command or the built-in
  Playwright provider for `backend=auto`; when that browser provider is
  configured, CLI/TUI thread start, resume, and fork requests advertise
  `browser_observe` and `browser_step` automatically. The Playwright bridge
  supports accessibility-oriented selectors plus human-like mouse and keyboard
  primitives, defaults to per-thread browser profile isolation for concurrent
  sidecars, can return visible-control metadata and selector candidates for UX
  loops, can save redacted audit artifacts, can use locally configured
  service-account navigation headers for allowed hosts, and can still be
  configured for shared, environment-scoped, or per-call profiles when that
  lifecycle is intentional. Thread-spawned agents
  inherit the parent thread's native dynamic tools, so browser-capable sidecars
  receive the native browser surface rather than silently dropping to a
  compatibility adapter.
- The Android adapter is retained as the reference MCP-backed runtime provider:
  reuse `android-emulator-mcp` or a successor when it exposes the current
  Android MCP contract, and adapt harness-specific behavior provider-side
  rather than in hot Codex core paths.
- The desktop adapter is the cleanroom provider seam for macOS Screen
  Recording/Accessibility-style runtimes and future native desktop providers.
  TUI dispatch stays behind an operator-configured command provider.
- `codex doctor` includes read-only native provider diagnostics for browser
  provider configuration, headed display/Chrome fields, and Android provider
  endpoint/credential shape without launching browsers, connecting to profiles,
  or starting emulator sessions.
- Android screenshots and browser viewport captures are expected to reach the
  model as native image content items. Provider artifact paths are kept for
  diagnostics, audit, or replay; they are not the normal model-facing visual
  channel.
- Computer-use events remain transient in every history mode; live rollout
  tracing maps them to tool-runtime start/end boundaries without writing them
  into thread snapshots.
- Runtime providers own Android sessions, browser sessions, screenshots,
  viewport capture, UI digests, input execution, and provider-side build
  installation. Solar Gravity Lab is a proving and consumer app, not the
  generic owner of Codex computer-use tooling.
- The built-in Playwright browser provider clears Chromium tab-session restore
  artifacts before opening a persistent browser context, so stale restored tabs
  cannot hit an old localhost target before an explicit requested navigation.
  Provider-managed `state.json`, cookies, local storage, and other profile data
  are preserved.
- Plugin app declarations are validated on the authenticated ChatGPT app
  projection. The unauthenticated plugin projection may intentionally omit apps,
  so future syncs should not treat app absence from unauthenticated manager
  tests as evidence that downstream app/plugin carry is removable.
- Primary files:
  - `codex-rs/protocol/src/computer_use.rs`
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/tools/src/android_tool.rs`
  - `codex-rs/tools/src/browser_tool.rs`
  - `codex-rs/tools/src/computer_use_tool.rs`
  - `codex-rs/tools/src/desktop_tool.rs`
  - `codex-rs/core-plugins/src/lib.rs`
  - `codex-rs/core/src/tools/handlers/computer_use.rs`
  - `codex-rs/tools/src/tool_search.rs`
  - `codex-rs/app-server/src/computer_use.rs`
  - `codex-rs/app-server/src/bespoke_event_handling.rs`
  - `codex-rs/app-server-protocol/src/protocol/common.rs`
  - `codex-rs/app-server-protocol/src/protocol/v2/item.rs`
  - `codex-rs/app-server-protocol/src/protocol/thread_history.rs`
  - `codex-rs/tui/src/android_computer_use_provider.rs`
  - `codex-rs/browser-computer-use/src/lib.rs`
  - `codex-rs/browser-computer-use/src/browser_playwright_provider.mjs`
  - `codex-rs/tui/src/browser_computer_use_provider.rs`
  - `codex-rs/tui/src/computer_use_provider.rs`
  - `codex-rs/tui/src/desktop_computer_use_provider.rs`
  - `codex-rs/exec/src/lib.rs`
  - `codex-rs/tui/src/app/app_server_requests.rs`
  - `codex-rs/tui/src/app/app_server_events.rs`
  - `codex-rs/tui/src/chatwidget.rs`
  - `codex-rs/tui/src/chatwidget/interrupts.rs`
  - `codex-rs/tui/src/history_cell/computer_use.rs`
  - `codex-rs/rollout/src/policy.rs`
  - `codex-rs/rollout-trace/src/protocol_event.rs`
  - `codex-rs/app-server/tests/suite/v2/computer_use.rs`
  - `codex-rs/tools/src/android_tool_tests.rs`
  - `codex-rs/tools/src/browser_tool_tests.rs`
  - `codex-rs/tools/src/computer_use_tool_tests.rs`
  - `docs/native-computer-use.md`
  - `docs/native-computer-use-cleanroom.md`

### Review And History Accounting Alignment

- Review and history token summaries use a stable unavailable fallback string
  instead of ad hoc formatting.
- Review-mode accounting remains aligned with live runtime state rather than
  stale defaults.
- Primary files:
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/core/src/guardian/review_session.rs`
  - `codex-rs/core/src/context_manager/history.rs`
  - `docs/downstream.md`

### Direct And App-Backed MCP Tool Catalog Reconciliation

- When a direct MCP server and an app connector expose the same logical
  callable, Codex compares the complete model-visible contract before reducing
  the catalog to one route.
- Exact matches prefer the direct MCP route and make that routing decision
  visible in the namespace description. Direct-only and app-only environments
  remain unchanged, so the app route is still a working fallback.
- A difference in descriptions, input or output schemas, safety annotations,
  or task execution keeps both routes visible, labels both namespaces, and
  emits a structured warning. Routing names, connector metadata, icons, and
  server scheduling policy are provenance rather than callable-contract fields.
- Primary files:
  - `codex-rs/core/src/mcp_tool_exposure.rs`
  - `codex-rs/core/src/mcp_tool_exposure_test.rs`
  - `.github/scripts/test_ci_planners.py`
  - `.github/validation-lanes.json`
  - `.github/workflows/sedna-heavy-tests.yml`
  - `justfile`
  - `docs/downstream-tool-surface-matrix.md`

### Complete MCP Tool Catalogue Collection And Refresh

- MCP pagination and host-side deferred loading are separate contracts. Codex
  drains every non-null opaque `tools/list` cursor, including the empty string,
  before publishing a catalogue for ordinary exposure or deferred tool search.
- Collection fails closed on cursor cycles, duplicate tool names, more than 64
  pages, or more than 10,000 tools. A partial walk is never published.
- Tool-list change notifications advance a generation. Codex discards and
  retries a walk that crosses generations, atomically swaps only a complete
  replacement, and retains the last complete snapshot when refresh fails.
- The Streamable HTTP regression performs deferred `tool_search` for a tool
  supplied only on page two, invokes that tool, and verifies its output.
- Preserve this carry until upstream issue #26094 is resolved by behavior that
  covers the complete bounded snapshot and refresh contract, not only a basic
  happy-path page walk.
- Primary files:
  - `codex-rs/rmcp-client/src/rmcp_client.rs`
  - `codex-rs/codex-mcp/src/rmcp_client.rs`
  - `codex-rs/codex-mcp/src/rmcp_client_tests.rs`
  - `codex-rs/core/tests/suite/rmcp_client.rs`

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

### MCP OAuth Fallback Hardening And Keyring Backend Preservation

- Downstream treats the MCP OAuth fallback credential file as best-effort
  recovery state: empty files are absent, corrupt files after keyring failure
  are warnings instead of hard startup failures, and writes use atomic temp-file
  replacement with explicit syncs.
- Cached expired tokens remain visibly expired when reloaded so the OAuth
  manager refreshes them before reconnecting instead of treating stale access
  tokens as usable.
- Proactive refresh failures such as missing refresh tokens are classified as
  authentication-required startup failures, so operators get the reauth path
  instead of a generic MCP startup failure.
- The selected keyring backend is intentional carry now that upstream supports
  encrypted local secrets storage. Syncs must preserve both upstream
  concrete-store pinning and `AuthKeyringBackendKind::Secrets` support, plus the
  downstream resolved-store refresh lock that prevents replaying stale rotating
  refresh tokens from a different backend.
- Normal requests stage access-only credentials in RMCP. Refresh material is
  installed only inside the serialized refresh transaction, and RMCP-derived
  persistence reacquires that same per-server lock, rereads the pinned store,
  and adopts newer durable credentials rather than overwriting or deleting them.
- Request-only staging strips `refresh_token` and derived `expires_in`; durable
  reconciliation restores omitted refresh material, scopes, and expiry fields.
  A matching `expires_at` makes a countdown-only `expires_in` difference
  non-conflicting, and a newly rotated refresh token is retained in memory
  before the durable write is attempted.
- Refresh-only credential staging deliberately omits granted scopes when
  handing credentials to RMCP so refresh requests do not broaden explicit
  persisted scopes with authorization-server-advertised `offline_access`; Codex
  preserves the durable stored scope set when the provider omits scopes.
- During future syncs, do not "simplify" the OAuth helpers by dropping either
  the secrets-backed keyring path or the downstream resolved-store reread/save
  discipline unless an upstream replacement covers those behaviors and the
  no-unrequested-`offline_access` refresh contract.
- Primary files:
  - `codex-rs/rmcp-client/src/oauth.rs`
  - `codex-rs/rmcp-client/src/oauth/resolved_store.rs`
  - `codex-rs/rmcp-client/src/oauth/store_lock.rs`
  - `codex-rs/rmcp-client/tests/streamable_http_oauth_startup.rs`
  - `codex-rs/rmcp-client/src/rmcp_client.rs`
  - `codex-rs/rmcp-client/src/startup_error.rs`
  - `codex-rs/codex-mcp/src/connection_manager.rs`

### MCP OAuth Device Login For Headless Servers

- `codex mcp login --device-auth <server>` lets an operator complete MCP OAuth
  login from SSH-only or browserless hosts through the OAuth Device
  Authorization Grant instead of relying on a local browser callback.
- Streamable HTTP OAuth discovery preserves `token_endpoint`,
  `device_authorization_endpoint`, `registration_endpoint`, and
  `grant_types_supported`, so the CLI can fail loudly when a server does not
  actually advertise device-login support or lacks both configured client id
  and dynamic registration support.
- The device-login flow uses a configured public MCP OAuth `client_id` when one
  is available. Otherwise, it performs standards-based dynamic client
  registration using the device grant shape, a public-client token endpoint
  auth method, optional requested scopes, and refresh-token registration only
  when server grant metadata permits or omits grant support.
- After client-id resolution, the flow uses PKCE, the identity-provider
  verification URL/user code, token-endpoint polling, and the existing MCP
  OAuth token cache.
- This is an intentional downstream carry for headless MCP server login until
  upstream ships an equivalent headless MCP OAuth login contract. During
  upstream syncs, preserve this behavior unless the upstream replacement covers
  the same discovery, grant-validation, dynamic-registration fallback,
  configured-client-id fast path, PKCE, polling, and token-cache path.
- Primary files:
  - `codex-rs/cli/src/mcp_cmd.rs`
  - `codex-rs/codex-mcp/src/mcp/auth.rs`
  - `codex-rs/rmcp-client/src/auth_status.rs`
  - `codex-rs/rmcp-client/src/perform_oauth_device_login.rs`
  - `codex-rs/rmcp-client/src/lib.rs`
  - `.github/scripts/test_ci_planners.py`
  - `.github/validation-lanes.json`
  - `.github/workflows/sedna-heavy-tests.yml`
  - `justfile`
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
- Active-turn runtime choice commands such as `/model`, `/permissions`, `/plan`,
  and model service-tier slash commands remain selectable while a task is
  running so their chosen settings apply to queued follow-up turns.
- Interrupt handling defaults to double-`Esc` confirmation, including status-row
  interrupts while a turn is running, and preserves queued follow-ups and
  queued model changes coherently.
- Active-turn status labels preserve downstream operator cues, including
  showing `Compacting context` while context compaction is running instead of
  falling back to generic `Working`.
- Bottom-pane transient views run their pre-draw tick and completion path so
  request-user-input overlays and other timed active views can redraw,
  auto-resolve, and pop through the same active-view seam instead of stalling
  behind static composer redraws. Stacked selection popups can be replaced or
  dismissed by view id even when another view sits above them, `/resume` stays
  available during MCP startup while active-turn blocking remains in force, and
  `/status` history preserves cached status text while a refresh is pending.
- TUI realtime voice remains a downstream carry on non-Linux targets even
  though upstream removed that surface; Linux keeps explicit unavailable stubs,
  so syncs should preserve the platform split instead of deleting
  `audio_device.rs` or the Linux `voice` stub as stale code. The non-Linux
  split also depends on the target-scoped `cpal` entry in
  `codex-rs/tui/Cargo.toml` and its `codex-rs/Cargo.lock` graph.
- Weekly status-line pacing keeps downstream stale handling and selectable
  render styles.
- Upgradeable legacy models stay visible in the model picker even when ordinary
  hidden presets are excluded.
- `/quit` and `/exit` inside an active `/side` conversation close only that side
  conversation and return to the parent session; the same commands in the main
  conversation remain application exits.
- Primary files:
  - `codex-rs/tui/src/app.rs`
  - `codex-rs/tui/src/app/side.rs`
  - `codex-rs/tui/src/app/event_dispatch.rs`
  - `codex-rs/tui/src/app_event.rs`
  - `codex-rs/tui/Cargo.toml`
  - `codex-rs/Cargo.lock`
  - `codex-rs/tui/src/audio_device.rs`
  - `codex-rs/tui/src/bottom_pane/mod.rs`
  - `codex-rs/tui/src/bottom_pane/textarea.rs`
  - `codex-rs/tui/src/chatwidget/slash_dispatch.rs`
  - `codex-rs/tui/src/multi_agents.rs`
  - `codex-rs/tui/src/voice.rs`
  - `codex-rs/tui/src/slash_command.rs`
  - `codex-rs/tui/src/bottom_pane/chat_composer.rs`
  - `codex-rs/tui/src/bottom_pane/slash_commands.rs`
  - `codex-rs/tui/src/bottom_pane/status_line_setup.rs`
  - `codex-rs/tui/src/chatwidget.rs`
  - `codex-rs/tui/src/chatwidget/slash_dispatch.rs`
  - `codex-rs/tui/src/chatwidget/protocol.rs`
  - `codex-rs/tui/src/chatwidget/tool_lifecycle.rs`
  - `codex-rs/tui/src/chatwidget/status_surfaces.rs`
  - `codex-rs/tui/src/status/card.rs`
  - `codex-rs/tui/src/status/rate_limits.rs`
  - `docs/config.md`
  - `docs/tui-weekly-usage-pacing-status-line.md`
  - `docs/downstream.md`
  - `docs/downstream-regression-matrix.md`

### TUI Transcript Compact Detail Mode

- The `Ctrl+T` transcript overlay intentionally carries two detail modes:
  verbose mode for the complete transcript and compact mode for prompt/agent
  review without terminal/tool noise.
- Verbose mode remains the default audit surface. Compact mode keeps user
  prompts, assistant-facing responses, selected reasoning summaries, and
  important warning/error/stop hook output visible while collapsing injected
  session context, context-only hook entries, and other detail that belongs in
  verbose transcript inspection.
- The overlay footer exposes the active detail mode and the toggle key, and the
  overlay state preserves scroll, selected prompt, raw/rich render mode, and
  verbose/compact detail mode across close/reopen.
- The transcript header now names the active mode (`Transcript: verbose` or
  `Transcript: compact`), and the footer spells out the switch keys as
  `raw render` / `rich render` and `compact view` / `verbose view` when there
  is enough terminal width.
- `[tui].transcript_default_detail_mode = "verbose" | "compact"` chooses the
  default detail mode at startup and after active config refreshes; verbose
  remains the default so the complete audit transcript is preserved unless the
  user opts into quiet review. An already-open transcript keeps its current
  mode until it is closed.
- This is an intentional downstream TUI ergonomics carry. Preserve it during
  upstream syncs unless upstream lands equivalent transcript detail-mode
  behavior that keeps the same verbose audit fallback and compact prompt/agent
  review path.
- Primary files:
  - `codex-rs/config/src/profile_toml.rs`
  - `codex-rs/config/src/types.rs`
  - `codex-rs/core/src/config/mod.rs`
  - `codex-rs/core/config.schema.json`
  - `codex-rs/tui/src/pager_overlay.rs`
  - `codex-rs/tui/src/history_cell/mod.rs`
  - `codex-rs/tui/src/history_cell/hook_cell.rs`
  - `codex-rs/tui/src/history_cell/messages.rs`
  - `codex-rs/tui/src/chatwidget.rs`
  - `codex-rs/tui/src/chatwidget/transcript.rs`
  - `codex-rs/tui/src/app.rs`
  - `codex-rs/tui/src/app/input.rs`
  - `codex-rs/tui/src/app_backtrack.rs`
  - `codex-rs/tui/src/footer_hints.rs`
  - `codex-rs/tui/src/keymap.rs`
  - `codex-rs/tui/src/keymap_setup.rs`
  - `codex-rs/tui/src/resume_picker.rs`
  - `codex-rs/config/src/tui_keymap.rs`
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
- `ToolRouter` preserves `ResponseItem::CustomToolCall.namespace` when
  constructing registry tool names, so namespaced MCP/app custom tools do not
  flatten into plain names before routing.
- Primary files:
  - `codex-rs/core/src/tools/code_mode_description.rs`
  - `codex-rs/core/src/tools/router.rs`

## Not Counted As Standalone Live Divergences

- Merge and sync history:
  - carry-only merge commits are sync history, not independent downstream
    behaviors; do not treat their count as a behavior inventory.
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
  - startup plugin sync bounded wait and completion re-arm
  - `codex-rs/core/src/config/edit.rs`
  - `codex-rs/core/src/tools/spec_plan.rs`
- Schema-generation adapters that preserve legacy wire deserialization while
  keeping generated app-server schemas on the current public shape, such as
  `#[schemars(!from)]` around `MultiAgentMode` wire aliases, belong with
  app-server/protocol maintenance rather than as standalone behavior.

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
