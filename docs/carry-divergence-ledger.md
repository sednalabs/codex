# Carry Divergence Ledger

This ledger records current source contracts for downstream carries. It is
source documentation, not provider activation or delivery acceptance.

## Carry and convergence principles

Prefer equivalent upstream behavior and existing configuration, hooks, MCP,
or documented app-server interfaces. Keep service-specific workflows, policy,
and state in their owning systems, connected through small adapters suited to
the actual capability. Do not add a generic plugin framework for a small carry
or rebuild a full harness elsewhere merely to reduce the fork diff.

The behavior is the contract; its current files and implementation are not.
Keep necessary runtime integration local when it is the simplest reliable
design, and state its concise rationale and removal condition with the carry.
Compare observable end-to-end semantics, including authorization,
permissions, cancellation, persistence and recovery, typed images, and the
difference between actionable attention and routine progress. MCP availability
alone does not establish wake, cancellation, or takeover capability. Retire
behavior only when an equivalent replacement is demonstrated or its loss is
explicitly accepted. Tests should protect these consumer-visible contracts and
meaningful security boundaries, not incidental file, module, or handler layout.

## Namespaced native computer-use provider adapters

- Codex owns per-session `DynamicTool` advertisement, exact namespace/tool
  routing, request correlation, truthful failure projection, and forwarding
  native `InputImage` content into the active model turn.
- The existing Browser adapter remains a separate `codex_browser` provider
  seam. The Android adapter is MCP-backed and preserves the historical
  `android_observe`, `android_step`, and `android_install_build_from_run`
  behavior. The opt-in Desktop adapter uses only the configured command
  provider and exposes `desktop_observe` and `desktop_step` in `codex_desktop`.
- Namespace collisions, duplicate call IDs, and calls from abandoned side
  threads fail closed. Desktop request/stdout/stderr/deadline limits are
  explicit, uncertain actions are not replayed, and visual success requires a
  native `InputImage`. No general registry, shared protocol, live service,
  device, or session-lifecycle capability is added; real provider activation
  remains outside this source assignment.
- Primary source and regression files:
  - `codex-rs/Cargo.toml`
  - `codex-rs/android-computer-use/BUILD.bazel`
  - `codex-rs/android-computer-use/Cargo.toml`
  - `codex-rs/android-computer-use/src/lib.rs`
  - `codex-rs/tui/Cargo.toml`
  - `codex-rs/tui/src/android_computer_use_provider.rs`
  - `codex-rs/tui/src/desktop_computer_use_provider.rs`
  - `codex-rs/tui/src/dynamic_tools_mcp.rs`
  - `codex-rs/tui/src/app/app_server_events.rs`
  - `codex-rs/tui/src/app/tests/session_lifecycle_requests.rs`
  - `docs/native-computer-use.md`
  - `docs/downstream-tool-surface-matrix.md`
  - `docs/downstream-regression-matrix.md`
  - `docs/divergences/index.yaml`

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

### External-Agent Session Import Compatibility

- Upstream consolidated the former downstream session-import crate into
  `codex-external-agent-migration::sessions`. Preserve the imported-session
  ledger, source-content SHA-256 identity, and single-pass record parsing while
  using the upstream module and package layout.
- The workspace uses `sha2` 0.11, so content hashes retain the downstream
  explicit hexadecimal encoder rather than relying on the older digest
  formatting implementation. Callers inside the consolidated module must
  resolve that helper through the `sessions` module, not the crate root.
- `codex.external-agent-session-migration-targeted` is the focused hosted
  guardrail for future upstream moves. The divergence registry deliberately
  lists only the five session files that remain different from upstream so
  adjacent upstream-owned migration code is not hidden from the audit.
- Primary files:
  - `codex-rs/external-agent-migration/src/sessions/export.rs`
  - `codex-rs/external-agent-migration/src/sessions/ledger.rs`
  - `codex-rs/external-agent-migration/src/sessions/ledger_tests.rs`
  - `codex-rs/external-agent-migration/src/sessions/mod.rs`
  - `codex-rs/external-agent-migration/src/sessions/records.rs`
  - `.github/validation-lanes.json`
  - `justfile`

### External-Agent Migration Path Containment

- Repository-scoped external-agent detection and import canonicalize the
  project selected by the trusted local app-server client before deriving
  migration paths. Static source and destination components under that root
  must not be symlinks, including settings leaves, MCP configuration leaves,
  hook scripts, and dangling target leaves.
- This fail-closed check applies to configuration, MCP settings, subagents,
  hooks, commands, skills, and instruction-file imports. Home-scoped imports
  retain their separate source-root behavior, while shared target-leaf checks
  avoid overwriting symlink entries in either scope. It does not claim
  handle-relative protection against concurrent replacement of an already
  checked repository path.
- Upstream MEMORY migration remains home-scoped, but its client-selected project
  keys must be exactly one normal path component. Before memory detection or
  import mutates the workspace, the memory root, extension/resource ancestors,
  project root, `scope.json`, Markdown resource leaves, and `instructions.md`
  are checked beneath `CODEX_HOME` and rejected when any existing component is
  a symlink. This preserves stale-project removal without allowing absolute or
  parent-relative selections to escape the extension workspace.
- A resource directory is managed only when it contains a regular, non-symlink
  `scope.json` marker. Ordinary files and unmarked metadata directories are
  ignored, but every directory entry and candidate marker is symlink-preflighted
  before its type is used, so stale project-root and marker symlinks fail closed.
- The marker rule affects future detection. A one-time backfill for unchanged
  imports that predate the marker is an explicit follow-up, not an automatic
  mutation performed by this sync.
- The focused lane covers both the extracted migration-crate denial tests and
  the app-server request boundary.
  `projects_needing_import_rejects_symlinked_stale_memory_project` proves stale
  owned projects are preflighted before detection offers them. In particular,
  `projects_needing_import_rejects_symlinked_stale_project_scope` proves a real
  project directory cannot redirect its ownership marker, while
  `external_agent_memory_import_rejects_stale_symlink_before_workspace_mutation`
  proves a rejected stale-project selection cannot initialize or rewrite the
  memory workspace before the containment error is reported.
- Recursive copy helpers skip symlink entries and refuse symlinked target
  directories. Empty-text target checks use `symlink_metadata`, so an empty or
  dangling symlink is protected rather than treated as an overwritable file.
- Upstream's closed draft containment change covered an empty leaf target but
  did not cover exact repository settings/MCP leaves, source roots, or
  destination-ancestor symlinks. Keep this carry until upstream has equivalent
  detection-and-import containment.
- Primary files:
  - `.github/scripts/test_ci_planners.py`
  - `.github/validation-lanes.json`
  - `.github/workflows/sedna-heavy-tests.yml`
  - `codex-rs/app-server/tests/suite/v2/external_agent_config.rs`
  - `codex-rs/external-agent-migration/Cargo.toml`
  - `codex-rs/external-agent-migration/src/detect/mod.rs`
  - `codex-rs/external-agent-migration/src/hooks_common.rs`
  - `codex-rs/external-agent-migration/src/lib_tests.rs`
  - `codex-rs/external-agent-migration/src/memory_import.rs`
  - `codex-rs/external-agent-migration/src/memory_import_tests.rs`
  - `codex-rs/external-agent-migration/src/migration_source.rs`
  - `codex-rs/external-agent-migration/src/scope.rs`
  - `codex-rs/external-agent-migration/src/scope_tests.rs`
  - `codex-rs/external-agent-migration/src/service.rs`
  - `codex-rs/external-agent-migration/src/service_tests.rs`
  - `codex-rs/external-agent-migration/src/service_tests/containment.rs`
  - `codex-rs/external-agent-migration/src/service_tests/general/config_import.rs`
  - `codex-rs/external-agent-migration/src/service_tests/general/detection.rs`
  - `codex-rs/external-agent-migration/src/service_tests/general/repo_import.rs`
  - `codex-rs/external-agent-migration/src/service_tests/plugins/basics.rs`
  - `codex-rs/external-agent-migration/src/service_tests/plugins/marketplaces.rs`
  - `codex-rs/external-agent-migration/src/source/cla.rs`
  - `codex-rs/external-agent-migration/src/source/cur.rs`
  - `codex-rs/external-agent-migration/src/subagents.rs`
  - `codex-rs/external-agent-migration/src/utils.rs`
  - `justfile`
- Hosted guardrails:
  - `codex.external-agent-migration-containment-targeted`
  - `rust-ci-full`
  - `CodeQL Advanced`

## Not Counted As Standalone Live Divergences

- Merge and sync history:
  - carry-only merge commits are sync history, not independent downstream
    behaviors; do not treat their count as a behavior inventory.
- Merge-repair and promotion-fix history:
  - examples include `Fix main core regressions after upstream sync`,
    `Fix main promotion follow-ups`, and
    `Fix hybrid merge API drift in core/tui tests`
  - hosted strict-lint compatibility repairs that preserve an upstream API's
    behavior, such as using `SseStream::from_bytes_stream` after the dependency
    deprecated its typo-named predecessor; retain only until upstream adopts
    the same API rename and do not classify the rename as MCP policy carry
- Generated and derivative churn:
  - schema outputs under `codex-rs/app-server-protocol/schema/`
  - generated SDK outputs under `sdk/python/`
  - TUI snapshot updates under `codex-rs/tui/src/**/snapshots/`
- Structural test-only churn in large modules:
  - `codex-rs/core/src/plugins/manager.rs`
  - startup plugin sync bounded wait and completion re-arm
  - `codex-rs/core/src/config/edit.rs`
  - `codex-rs/core/src/tools/spec_plan.rs`
  - `codex-rs/tui/src/test_support.rs` centralizes the existing 8 MiB test
    stack budget for stack-heavy app and app-server-session regressions; keep
    their behavioral assertions intact when importing upstream test changes
  - `codex-rs/app-server-daemon/src/backend/pid_tests.rs` removes the PID file
    while its reservation lock remains held, preventing test cleanup races;
    drop this carry when upstream adopts equivalent ordering
  - `codex-rs/tui/src/diff_render.rs` retains the downstream ratatui `0.30.1`
    `Widget::render` test adapter and tab-free gallery fixtures; production
    rendering follows upstream's borrowed-file-change implementation
  - `codex-rs/tui/src/history_cell/plans_tests.rs` asserts that transcript
    measurement primes the plan cache without pinning upstream's
    ratatui-version-specific row count; the sentinel equality still proves
    reuse, and this adaptation can drop when ratatui versions converge or
    upstream adopts an equivalent semantic assertion
  - `codex-rs/core/tests/suite/pending_input.rs` uses the interrupted sleep
    completion as the sole consequence barrier so an unrelated event wait
    cannot consume the completion before its assertion
  - `codex-rs/core/tests/suite/otel.rs` waits for orderly session shutdown
    before reading the trace buffer so retained response spans are closed;
    both metadata assertions remain unchanged
- Schema-generation adapters that preserve legacy wire deserialization while
  keeping generated app-server schemas on the current public shape, such as
  `#[schemars(!from)]` around `MultiAgentMode` wire aliases, belong with
  app-server/protocol maintenance rather than as standalone behavior.
- The removed config template-interpolation module is deliberately not carry.
  Effective config now follows upstream's authoritative layer-stack
  materialization; old interpolation helpers and their tests must not be
  resurrected during later syncs.

## Historical Carry Commits Now Upstream-Equivalent

The following carry commits have exact-subject matches on `upstream/main`. They
should not be treated as current fork-only behavior by title alone.

Two additional validation-only repairs have behavior-equivalent upstream
coverage despite different subjects: `ec042322e2` and `ddb5443d28` are both
absorbed by upstream `f21f98936c` and must not be counted as live carry.

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

## GitHub App installation observer carry

The PR watcher now has an explicit `--installation-observer` source-mode
adaptation for a caller that already supplies a short-lived GitHub App
installation token. It skips `gh api user`, passes an unbound login to the
existing conservative review filter, and preserves the default login lookup
outside that mode. The path is read-only and does not mint, print, persist, or
broaden credentials. Hosted tests must cover parser selection, observer
no-`/user` behavior, default-mode login behavior, and trusted-association/bot
filtering with a blank identity; no live token or watcher activation belongs to
this carry.

## Exact-head PR watcher policy carry

## Virtual workflow watcher identity carry

The workflow watcher preserves the maintained queue, exact-head, and failure
receipt behavior while adding a strict REST fallback for GitHub virtual or
uncatalogued workflow runs. The fallback validates repository, run, job, and
step identity and consumes every paginated job page according to a stable
`total_count`; malformed, incomplete, duplicate, or wrong-target responses
remain operator errors. The PR watcher also retains its current merge-queue
and broker boundaries while falling back to `statusCheckRollup` when an older
GitHub CLI cannot emit `gh pr checks --json`.

The workflow watcher additionally exposes an opt-in assignment timeout for
queued jobs. It surfaces only provider-confirmed missing runner assignment,
optionally scoped to exact required job names, and binds acknowledgement to
the run attempt, head SHA, and job identity. Missing assignment fields remain
unknown; queue state is not treated as evidence of capacity, labels, or auth
failure, and terminal wait semantics remain unchanged.

Focused regressions cover wrong run/job identity, incomplete pagination, and
the compatibility check-rollup path. The carry must not import ambient Gemini
diagnosis, overloaded target SHA semantics, credentials, or a second watcher
control plane.

The canonical PR watcher keeps its existing read-only observer and review/check
contracts while recording a compact decision receipt and the next scheduled wake
against the exact repository, PR number, and observed head SHA. An unexplained
GitHub `BLOCKED` merge state is surfaced as
`action_required_merge_policy_blocked`; it fails closed for readiness and keeps
the polling interval bounded instead of entering green-state backoff.

Focused source guardrails cover BLOCKED versus CLEAN actions, pending checks,
published review evidence, and a fake-clock exact-head schedule receipt. This
carry also keeps active `QUEUED` and `AWAITING_CHECKS` merge-queue entries on
the configured base cadence, including when the pending queue head is missing
or unreadable, and includes queue-entry identity in change detection so a
replacement or removal cannot preserve stale green backoff. Ordinary green
PRs without an active queue entry retain bounded backoff. Queue failure,
removal, readiness, credential, and mutation behavior remain fail-closed and
separate ownership boundaries.

## RMCP Streamable HTTP cancellation and backlog capacity

The downstream client uses released rmcp 3.2 concurrent HTTP transport and a
scoped request guard for `tools/call`. Dropping a call, including active-time
expiry, sends a cancellation notification. Completed requests retain rmcp's
normal response cleanup. Existing elicitation pauses continue to suspend the
active-time budget, and uncertain tool mutations are not retried.

When Streamable HTTP reports an expired session for `tools/call`, the client
returns an uncertain outcome without replaying that mutation. It retires the
failed generation only if it is still current; the next distinct operation
reinitializes lazily, while a late failure from an older generation cannot
evict its replacement. Keep this recovery tied to the existing initialize
context and recovery semaphore rather than adding a replay path.

The SDK upgrade also adapts its direct consumers to typed request metadata,
flat content and resource models, and non-exhaustive constructors. The client
initialize lifecycle and negotiated protocol version remain unchanged; SDK
support for discovery does not opt this client into that lifecycle. OAuth setup
continues to require published metadata; SDK-synthesized endpoint fallback is
refused for login, registration, and restored-token connections. Token restoration
uses the existing serialized request-only credential adoption path.

The manual stdio MCP server explicitly supports protocol revisions 2024-11-05,
2025-03-26, and 2025-06-18. Other requested revisions receive 2025-06-18 during
initialization, rather than an unsupported version claim. Its tool results use
the SDK legacy projection so initialization and response shape agree.

`codex.rmcp-client-transport` exercises observed request/cancellation IDs,
blocked-POST independence, queued cancellation, capacity recovery after abort,
mutation no-replay, expired-session lazy recovery, generation-safe late errors,
and existing elicitation tests. `codex.rmcp-consumers-check`
compiles all direct SDK consumers. Remove this guard when upstream provides
both equivalent caller-drop cancellation and the existing concurrent control
capacity; passing source tests alone does not prove an installed host adopted it.

## MCP per-server protocol selection

`mcp_servers.<name>.protocol_mode` selects `legacy` or prefers MCP 2026-07-28
for that Streamable HTTP server. An explicit extension registration keeps its
existing precedence; otherwise the per-server setting precedes the applicable
host/global preference. The modern choice requests discovery and stateless
requests, with legacy fallback when the peer does not support discovery. The
configured value is a preference, not proof of the negotiated wire lifecycle.
Changing the effective mode uses existing connection reconciliation to replace
the server client; stdio remains on its existing marker-controlled behavior.

This narrow configuration carry lets one HTTP integration move to the current
protocol without changing other providers. Remove it when upstream supplies the
same per-server selection, extension precedence, and connection-replacement
behavior, or when the owning configuration can safely rely on the global mode.
The config parser and connection-selection tests cover the option and fallback;
the RMCP discovery fixture checks the negotiated protocol header and absence of
a legacy session ID on current-protocol requests.

Primary files: `codex-rs/config/src/mcp_types.rs`,
`codex-rs/codex-mcp/src/connection_manager.rs`, and
`codex-rs/rmcp-client/tests/mcp_2026_discovery.rs`.

## Linux x86_64 preview code-mode host artifact

The `sedna-branch-build` workflow has an opt-in `host_only=true` dispatch for
`platform=linux-x86_64`. It builds only `codex-code-mode-host` with Cargo
`--locked`, stages the executable, verifies its Linux x86_64 format, and runs
`--help` before uploading a separate archive. The artifact metadata records the
exact source commit and workflow commit; adjacent SHA-256 files cover both the
binary and archive. The default preview dispatch continues to build the CLI and
responses API proxy. Before Cargo runs, the host-only path uses the target's
existing `setup-rusty-v8` action, which downloads the matching Codex release
archive and binding and verifies them against the target's pinned checksum
manifest. Use the host-only artifact only with companion binaries built from
the same source commit.

## Native per-execution proof for a protected MCP claim

The Linux CLI accepts an optional protected bootstrap through one inherited
anonymous descriptor before dispatching arguments or starting asynchronous
work. The frame contains a bounded issuer certificate and ephemeral signing
seed. Startup checks the inherited process identity and protection state,
marks the process non-dumpable, closes the descriptor, and zeroizes temporary
seed/frame storage. A missing descriptor keeps ordinary CLI use unconfigured;
an invalid protected frame fails startup closed.

Every native session, including a delegated session using the parent's shared
MCP manager, has its own execution nonce. For one pinned claim tool and
recipient, the final rewritten parameters and native execution evidence are
signed into short-lived MCP request metadata. Complete claim preconditions are
required. The proof does not come from model arguments or a bare thread ID,
and the request path does not retry without proof after a protected-scope
failure. Proof material is excluded from the rollout request trace and
redacted from echoed result surfaces.

The hosted external-unload residency case remains the exact core reproducer,
with `session_end_flushes_transcript_and_ignores_control_output` and
`session_end_skips_subagents` as existing shutdown-consumer controls. Earlier
SessionEnd allocation hypotheses were reverted after hosted core validation
still failed; the available traces do not establish a hook-specific cause or
a passing execution. The ordinary Linux qualification restores the repository
standard `RUST_MIN_STACK` value used by its existing Rust test workflows.

The post-sampling token-estimate regression asks the constructed feedback and
state sink stack for direct callsite interest on its excluded TRACE event, and
uses an ordinary TRACE callsite as a positive control. This avoids relying on
the process-wide cached `event_enabled!` result while preserving both sink
filters and their production behavior.

The runtime key is process-scoped and is erased if its protection checks drift.
The issuer certificate binds the fixed execution-lease duration; an ordinary
request or registration replay does not renew that lifetime. This client
carry supplies proof production only. The independent service remains
responsible for authenticating the issuer, validating the request signature,
and enforcing the lease and claim invariants. Preserve this boundary during
upstream sync unless upstream provides an equivalent host-held per-execution
capability and exact-request binding.

The `certificate_sha256` wire field is the bare lowercase hexadecimal SHA-256
of the exact compact certificate bytes, matching the Ops verifier. The
`operation_sha256` field retains its separate `sha256:`-prefixed encoding; the
two fields must not share an encoding change. The
`certificate_digest_matches_ops_protocol_vector` regression pins this producer
contract to the Ops protocol's expected digest.

Protected startup also consumes a separate finite authentication frame from
the root launcher. It imports ChatGPT credentials through the existing
ephemeral auth path and keeps the selected MCP bearer in private runtime state.
The endpoint check uses the effective built-in Responses provider URL, and
every resolved model provider is checked against the canonical built-in
configuration before auth resolution and again before HTTP or websocket
transport construction. This rejects role-level provider or auth overrides,
including transport overrides, before protected credentials can leave the
process. The
synthetic Linux fixture uses a fixed public fake JWT and a loopback URL through
the same importer and transport path; invalid frame, recipient, or token
variants must fail before provider credential egress.
Protected MCP startup accepts only the reserved local environment and always
uses the process-local HTTP client, even when the environment manager has
registered its local environment; a remote descriptor is rejected even if a
caller pairs it with the local environment ID.

The existing cloud-config service fetches managed requirements for
business-like, enterprise and education plans through a credential-bearing
backend route that is outside the selected protected provider recipient. Until
that consumer is admitted and guarded, protected startup refuses eligible or
unclassified account plans before importing provider credentials or starting
the loader. Ordinary cloud-config loading and managed policy handling remain
unchanged. This protected eligibility limit is a source safety boundary, not
live acceptance of a different cohort.

Protected runtime failure is terminal within the process, including retained
MCP credential caches. Every protected HTTP send and recovery rechecks the
active recipient and prevents redirects before network access. Per-invocation
redaction covers successful and failed results and JSON keys before output
sinks; protected server notifications and elicitation payloads are suppressed.
Formatter and OpenTelemetry layers exclude raw SDK transport targets even
when TRACE is enabled. Linux custody is gated so ordinary Windows startup
remains supported; protected non-Linux startup fails closed.

The delayed MCP response fixture covers result redaction after the runtime
credentials expire; it does not establish a cached-send rejection. A separate
ignored Linux root fixture imports the real synthetic protected frame and
uses the production protected HTTP client twice: the initial selected request
reaches a recording delegate, while the retained client rejects a second send
after expiry before that delegate is called. Both tests are source fixtures
until explicitly run on a disposable hosted Linux runner.

## Current MCP device authorization and protected launcher joins

Device authorization retains its trusted metadata and routed HTTP client. Resolved
scopes are sent unchanged; scope rejection never triggers an empty-scope login.
The original device deadline bounds token requests, including response-body
collection, and cancellation does not publish credentials.

The Linux package also carries a fixed-policy one-shot launcher and its
systemd unit template. The launcher has no caller-selected arguments; its
root-owned policy selects one Work task, principal, project, MCP recipient,
lease, and task instruction. It starts the measured `codex-dev` ELF through an
already hashed file descriptor, drops to the configured runtime UID/GID, and
checks the actual executable and process identity before reading any of the
fixed `LoadCredential` inputs. It then sends the existing root-peer bootstrap
and authentication frames, waits for the startup ACK, supplies the fixed task
prompt through a deadline-bounded pipe, and enforces the certificate deadline
while supervising the child. Issuer, provider-auth and bearer buffers are
zeroized after their corresponding signed frame is prepared or sent. Runtime
directories are populated while root still has owner write access, then child
ownership and final immutable modes are applied without requiring
`CAP_DAC_OVERRIDE`; installation state is chowned only after its final mode is
applied, avoiding an unnecessary `CAP_FOWNER` requirement.
The unit uses `KillMode=control-group` and a finite stop timeout to clean up
descendants, and masks `/home`, `/root` and `/run/user` rather than exposing
host user auth stores to the root service or its child. The normal `codex-dev`
entrypoint remains user-owned and unchanged.

The same launcher also has a closed HTTP target for the maintained App claim
route. It measures its own running ELF before reading credentials, signs the
exact final `POST /v0/work-items/claim` body and request ID, and sends one
bounded direct request using the API bearer from the fixed
`ops-api-bearer` system credential. The API certificate binds the measured
HTTP-sending launcher, `agent-ops-api-claim-client`, the canonical API
recipient and the fixed body/precondition; MCP-only server, tool and thread
fields are omitted. The API server remains responsible for authenticating the
bearer and verifying both signatures. The local JWT field comparison is only
a fail-early consistency check, never an authentication decision. An absent
bearer or mismatch fails closed, and an uncertain POST is returned without
retry. The MCP policy and existing MCP child path remain separate. Source
support does not appoint or install the API bearer, authorize a claim policy,
or establish HTTP first use.
The unit template and binary are packaged for Linux x86_64 and Arm64; packaging
does not create the host policy, credentials, service account, issuer
appointment, service installation, or live execution authority. The ordinary
branch installer continues to install only the CLI and its two existing
companion executables.

## Editable client instruction overlays

The client instruction-overlay carry is documented in
[`docs/config.md`](config.md#client-instruction-overlays). It defines nine
closed client-owned text targets and uses the existing enabled configuration
and trusted-project boundary, including role configuration. Ordered literal
operations are bounded and atomic on eligible outgoing request copies; strict
`unique` selection remains the default, while explicit `all_classified`
selection applies counts independently to each known instance. It does not
infer a latest/current instance, change runtime controls or permissions, or
rewrite stored history. Internal, Guardian, compact, memory-consolidation,
`SessionSource::Custom`, unknown, and other non-eligible sources are unchanged;
review requests may select only the review baseline. This source gate is
distinct from operator-defined agent roles: those roles remain included on
otherwise eligible root and ThreadSpawn requests. Managed, administrative,
security, provider-enforced, and unclassified content is excluded.

Eligibility is attached at known client-value producers using the existing
winning configuration-layer origin; ambiguous generic/legacy tags do not
establish ownership. Sampling and eligible WebSocket prewarm account for the
transformed request view through the existing context-window policy, using
coarse content estimates without double-adding measured server usage. Overlay
append/prepend require an existing target; `developer_instructions` remains the
supported way to create ordinary client guidance before applying edits.

Rollback is configuration reconstruction: restore or remove the setting at
its existing layer and start a new process/session. This source carry and its
synthetic tests do not claim installed or live acceptance. The instruction
inspection command reports that it does not evaluate these overlays and is not
proof of a transformed provider request.

## V2 root-directed agent updates

The V2 `send_message` surface preserves quiet FIFO delivery for ordinary
messages and explicit `progress`. Every explicit intent, including `progress`,
requires an `idempotency_key` and keeps the original communication and scope in
durable admission metadata. Only `action_required` and `result_ready` are
actionable root-directed intents; they permit superseding same-scope progress
updates only. Identical retries return the original receipt; a changed request
cannot reuse its key.

Only a matching explicit action/result can wake an active V2 wait, and only
when that wait names the sender. The body-free activity carries the update and
sender/recipient identity plus pending generation; the receiver subscribes
before checking already-pending mail. This does not make plain messages or
progress wakeable, start a new turn, or change follow-up/interrupt authority.
Goal-aware terminal handback remains a separate producer/consumer contract.

Primary sources and guardrails: `codex-rs/core/src/tools/handlers/multi_agents_spec.rs`,
`codex-rs/core/src/agent/control/delivery.rs`,
`codex-rs/history/src/lib.rs`,
`codex-rs/core/src/session/input_queue.rs`,
`codex-rs/core/src/tools/handlers/multi_agents_v2/wait.rs`, and
`codex-rs/core/src/tools/handlers/multi_agents_tests.rs`. See the
[focused regression contract](downstream-regression-matrix.md#core-default-path)
for the behavior selectors.
