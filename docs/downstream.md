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
- completed feature, bugfix, docs, or cleanup branch work must be committed, pushed, and opened as a PR targeting `origin/main` before handoff; do not leave finished work local-only
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
- when validating exact local state that is not yet on a clean remote branch, prefer the disposable snapshot-ref dispatch path (`validation/snapshot-*` + `validation-lab` input `ref`) documented in `docs/github-ci-offload.md`
- branch artifacts are disposable and retain for 3 days
- official releases are published only from the protected Sedna release workflow
- the authoritative divergence audit lives in `scripts/downstream-divergence-audit.py` and writes artifacts under `target/downstream-divergence-audit/`
- the intended-divergence registry lives at `docs/divergences/index.yaml`
- PRs that touch downstream divergence docs or the divergence registry run
  `codex.downstream-docs-check`, a PR-local docs and registry sanity lane.
- The full registry/code divergence audit is `codex.downstream-divergence-audit`.
  It is an explicit baseline-maintenance lane because it validates the entire
  downstream fork against the current upstream mirror, not only the files in a
  single pull request.

## Divergence Summary

This section tracks intentional downstream behavior differences from
`upstream/main`.
References to `carry/main` elsewhere in the repo are historical pre-cutover
baselines and should be read as prior names for the maintained downstream
branch.

Current downstream audit baseline (validated on `2026-07-11`):

- downstream branch `main` code tree:
  `9af58e26bcbed15d55713008959d755baf918eb2`
- comparison basis: `mirror`
- mirror branch `upstream-main` (`origin/upstream-main`):
  `dffe1f02a3c4849478c4f412a69d25af2e6b9359`
- `upstream/main`:
  `dffe1f02a3c4849478c4f412a69d25af2e6b9359`
- downstream divergence counts (`upstream/main...main`):
  `0` upstream ahead, `1610` downstream ahead
- mirror health (`upstream/main...origin/upstream-main`): `0` ahead / `0`
  behind (`exact`)

These counts intentionally anchor to the audited code tree before the
docs-only refresh commit that records this snapshot.

Supporting docs:

- [`downstream-tool-surface-matrix.md`](downstream-tool-surface-matrix.md) captures the exact native tool-surface deltas that remain live on the downstream branch.
- [`downstream-divergence-tracking.md`](downstream-divergence-tracking.md) sketches the next-step registry and generation model for keeping these notes current as the fork grows.
- [`native-computer-use.md`](native-computer-use.md) documents the first-party computer-use adapter contract, including Android, browser, app-server, TUI, rollout, and validation boundaries.

### Core + protocol: blocking wait for unified exec, stable wait output, and compaction turn-count metadata

Why:

- Support "wait until terminal" semantics directly on `exec_command` and `write_stdin` for long-running exact/tool-driven command flows.
- Avoid model-layer short-poll loops that waste turns, duplicate context, and make orchestration look busy without changing state.
- Let downstream interactive automation treat long-running shell work as an actual blocking join instead of repeated "check again" tool chatter.
- Keep wait responses aligned with the current unified-exec output shape after upstream refactors.
- Expose compaction count on turn completion so clients can distinguish "normal turn complete" from "turn completed after one or more compactions".

User-visible behavior:

- `exec_command` and `write_stdin` support blocking wait parameters (`wait_until_terminal`, `max_wait_ms`, `heartbeat_interval_ms`).
- Live command lifecycle events preserve the selected terminal-wait primitive
  through the canonical command item, its legacy begin/end projections, and
  app-server v2 command items; reconstructed history leaves it absent when no
  persisted value is available.
- `wait_until_terminal` gates provider resume until the process reaches a terminal state or the wait budget expires. The default and maximum wait budget is two hours.
- `write_stdin` still requires `chars` to be empty when `wait_until_terminal=true`.
- Wait-timeout notes are appended to emitted `raw_output`, and token accounting is derived from the final response text.
- Tool-spec guardrails cover the full blocking-wait contract, including the
  surfaced wait fields, invalid-type rejection, and the empty-`chars`
  requirement for `write_stdin(wait_until_terminal=true)`.
- Code-mode nested `exec_command` results use the same bounded unified-exec
  output summaries, including truncation warning headers, before any later
  code-mode or history output budget applies.
- Code mode keeps the read-only `get_context_remaining` budget helper
  available while direct-model-only tools that require interactive user input
  stay hidden from nested execution.
- `TurnCompleteEvent` preserves upstream's optional structured terminal `error`
  payload while adding downstream `compaction_events_in_turn`, `final_model`,
  and `model_snapshot` metadata.
- Guardrails for the carry-only turn-complete metadata live in
  `codex.app-server-protocol-test` (`preserves_compaction_only_turn`), the core
  `continue_after_stream_error` regression, and broader `TurnCompleteEvent`
  shape coverage in `codex-core`, `codex-exec`, and `codex-tui` tests.
- Sub-agent delegate forwarding continues to emit `TokenCount` events back to the parent session, ensuring the downstream token accounting and provider/model metadata remain accurate even if upstream-native structures eventually rehost this carry.
- This pairs cleanly with other blocking coordination primitives such as `wait_agent` and helper-backed `*_and_wait` flows, so agents can wait on real state transitions instead of spinning on repeated status polls.
- This downstream blocking MCP tool pattern predates fully operational task support and exists specifically so the tool layer, not the transcript, absorbs the wait.

### Core + app-server: native computer-use adapter bridge

Why:

- Preserve native computer-use as a Codex-owned transcript and tool contract instead of treating Android or browser observe/step tools as ordinary ad hoc dynamic tools.
- Let runtime providers supply Android, browser, or desktop capability while Codex owns the canonical model-facing schema, adapter dispatch, protocol events, app-server requests, live TUI projection, and rollout-trace runtime boundaries. Computer-use events remain transient rather than thread-snapshot state.
- Keep Solar Gravity Lab positioned as a proving and consumer app rather than the generic owner of Codex Android tooling, and keep browser runtime ownership in a provider bridge rather than in hot core code.

User-visible behavior:

- Bare `android_observe`, `android_step`, and `android_install_build_from_run` dynamic tools are promoted to canonical Codex function tools and handled by `ToolHandlerKind::ComputerUse`.
- Bare `browser_observe` and `browser_step` dynamic tools are also promoted to canonical Codex function tools with adapter `browser`; the shared browser provider crate routes them to a configured browser bridge for TUI and exec, and CLI/TUI sessions auto-advertise those browser tools when a local browser provider is configured.
- Bare `desktop_observe` and `desktop_step` dynamic tools are promoted to canonical Codex function tools with adapter `desktop`; the TUI routes them to an operator-configured command provider for cleanroom macOS Screen Recording/Accessibility-style runtimes or future native desktop providers.
- Namespaced Android-like, browser-like, and desktop-like tools remain normal dynamic tools.
- App-server `dynamicTools` accepts deferred bare native tools for
  `tool_search`, while still rejecting deferred bare ordinary dynamic tools.
  A capability-bearing native tool requests a full loaded-thread reload so the
  provider contract cannot silently remain stale.
- The current compatibility contract still serializes dynamic functions in the
  flat `DynamicToolSpec` shape with an optional namespace; omitted
  `deferLoading` defaults to `false` and omitted `persistOnResume` defaults to
  `true`. A future tagged
  function/namespace migration must preserve app-server input, persisted thread
  state, resume filtering, and native promotion before this carry is removed.
- `android_observe` is non-mutating; `android_step` is mutating and supports both compatibility single-action fields and preferred batched `actions[]`; `android_install_build_from_run` is mutating and maps provider-side artifact installation into the same native transcript path.
- `browser_observe` is non-mutating and can return compact visible-control, attention-state, and multi-capture viewport metadata for UX review; `browser_step` is mutating and supports compatibility single-action fields plus preferred batched `actions[]`, with a `backend` hint for `auto`, `browser`, `chrome`, `chromium`, or provider-declared backends such as `iab`, accessibility-oriented selectors, and human-like mouse/keyboard primitives for pages where coordinate-level interaction is the right fallback.
- The browser bridge supports a built-in Playwright backend for `backend=auto/browser/chrome/chromium` plus an operator-configured command provider for in-app-browser, signed-in Chrome, remote, or hosted browser providers. The Playwright backend can run headed Google Chrome against an operator-managed display for realistic remote-editor UX loops, keeps native image output available through screenshot fallbacks when headed Chrome window state is stale, returns a fresh screenshot and selector candidates on action failure when possible, can save redacted audit artifacts, supports locally configured service-account navigation headers, and defaults to per-thread profile isolation so concurrent sidecars do not share a Chrome profile, lock, or restored URL unless an operator explicitly configures shared isolation.
- Thread-spawned sidecars inherit the parent thread's advertised native dynamic tools, so browser-capable agents receive `browser_observe` and `browser_step` through the native computer-use path instead of having to fall back to Playwright MCP or another compatibility adapter.
- The Android adapter remains the MCP-backed reference runtime provider; reuse
  `android-emulator-mcp` or a successor when it exposes the current Android MCP
  contract, and keep harness-specific translation in the provider rather than
  in hot Codex core paths.
- Android provider behavior should absorb emulator-QA discipline without
  pushing shell recipes into Codex core: explicit device serial/readiness,
  UIAutomator-style hierarchy capture, selector-to-bounds targeting, screenshot
  and hierarchy receipts, log/performance artifacts for focused debugging, and
  post-failure observations that help agents recover from partially completed
  mutating actions.
- App-server API v2 sends `item/computerUse/call` requests to capable clients and records `ThreadItem::ComputerUseCall` start/completion items.
- The active TUI session renders live native computer-use items with compact
  adapter-specific labels such as `Used browser`, `Used computer`, and `Used
Android emulator`; these transient app-server projections are not persisted
  into thread snapshots or replayed after resume. Exec JSON/human output
  projects the same calls as compact computer-use events without embedding
  screenshot bytes in transcript text.
- Responses can include `inputText` and `inputImage` content items plus `success` and optional `error`.
- Android screenshots and browser viewport captures are model-facing only when returned as native image content. Provider artifact paths can be used for diagnostics, audit, and replay, but they are not instructions for the model to fetch local files.
- For MCP-backed Android providers, `structuredContent` is parsed for state and UI metadata without dropping `content[]` image entries. The native bridge must preserve both channels so JSON summaries never preempt the screenshot pixels.
- Computer-use events remain transient in every history mode; live rollout tracing maps them to tool-runtime start/end events without persisting them into thread snapshots.
- See [`native-computer-use.md`](native-computer-use.md) for the full contract and validation guidance.
- See [`native-computer-use-cleanroom.md`](native-computer-use-cleanroom.md) for the sanitized desktop, browser-shell, Chrome-extension, and bundled-plugin cleanroom contracts.

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
- Namespaced custom tool calls preserve their namespace through `ToolRouter`, so MCP/app custom tools route by their registry name instead of a flattened plain name.
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
- The v1 `spawn_agent` result stays on the upstream `agent_id`/`nickname` shape. The v2 result returns its canonical `task_name`, includes `agent_id` and `nickname` only when spawn metadata is visible, and reports requested/effective model and reasoning fields so callers can see what actually launched after role/profile resolution. Role, status, identity source, provider ID, and the preserved `model_reasoning_summary` remain inventory or internal metadata rather than raw spawn-result fields.
- Active-profile updates (parent/session config/role) that set `model`, `model_reasoning_summary`, or `model_reasoning_effort` continue to override child requests; the precedence stack is role-defined fields > active profile overrides > child requests, and the split between `core/src/agent/role.rs` and the spawn handlers encodes that boundary explicitly.
- The built-in `explorer` role no longer hard-locks a model or reasoning setting; instead the cheap-first policy lives in availability-aware `spawn_agent` behavior and supporting guidance so codebase-question lanes stay compatible with the caller's loaded model catalog.
- `list_agents` remains the always-on, cheap live inventory view across both collaboration surfaces rather than being hidden behind `MultiAgentV2`; it exposes `has_active_subagents` / `active_subagent_count` plus nested visibility/status metadata so callers retain nested-agent live visibility without dumping full trees.
- `inspect_agent_tree` is the intentionally richer downstream observability surface, separate from `list_agents`: it inspects the current subtree or a target path, can toggle `live` versus `stale` descendant visibility, can filter to selected branches with `agent_roots`, and returns compact tree rows with bounded depth and row limits.
- `wait_agent` supports `return_when=any|all` and returns `requested_ids`, `pending_ids`, `completion_reason`, and `timed_out`. Those completion fields are tool-output-only; canonical transcript items retain identities and status snapshots without duplicating timeout, mailbox, or pending outcome state. In the v2 surface, callers may omit `targets` when they intentionally want to wait only for current-turn input activity, such as mailbox delivery or user steering, or timeout.
- Roles that explicitly set `model`, `model_provider`, `model_reasoning_effort`, or `model_verbosity` continue to be authoritative, even when a child requests a different setting.
- Docs and tooling now spell out the precedence stack and the intended `list_agents` / `inspect_agent_tree` / `wait_agent` workflow: cheap live view first to keep nested-agent visibility, compact nested or stale inspection when deeper context is needed, and blocking wait only when a transition must complete.

Primary files:

- `codex-rs/core/src/agent/role.rs`
- `codex-rs/core/src/agent/control.rs`
- `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_v2/list_agents.rs`
- `codex-rs/core/src/tools/handlers/inspect_agent_tree.rs`
- `codex-rs/core/src/tools/handlers/multi_agents/wait.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_v2/wait.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_spec.rs`
- `codex-rs/core/src/tools/spec_plan.rs`
- `codex-rs/core/src/tools/tool_runtime_capabilities.rs`
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
- Preserve upstream's concrete-store pinning and RMCP persistence while keeping downstream refresh locking tied to the file or direct/encrypted-secrets store that originally supplied the tokens.

User-visible behavior:

- Empty fallback credential files are treated as absent instead of fatal.
- If keyring loading fails and the fallback credential file is corrupt, downstream logs a warning and proceeds as though no cached OAuth credentials were available.
- Fallback credential writes are atomic temp-file replacements with explicit syncs, which reduces the chance of leaving a half-written file behind after interruption or crash.
- MCP OAuth token load, refresh, save, and delete paths honor `AuthKeyringBackendKind::Direct` versus `AuthKeyringBackendKind::Secrets`; retries and session recovery stay pinned to the resolved concrete store instead of silently switching authority.
- RMCP receives request-only credentials during normal operation. Refresh material is exposed only inside the serialized refresh transaction, and every RMCP-derived durable save or delete reacquires the per-server refresh lock and reconciles the current pinned store before mutating it.
- Request-only staging strips `refresh_token` and derived `expires_in`; durable
  reconciliation restores omitted durable refresh material, scopes, and expiry
  fields. A matching `expires_at` makes countdown-only `expires_in` drift
  non-conflicting, and a rotated refresh token remains in memory before an
  attempted durable write.

### MCP OAuth: device-code login for headless servers

Why:

- Let operators authenticate MCP servers from SSH-only or browserless hosts without installing temporary login helpers or copying fallback credential files by hand.
- Preserve OAuth discovery metadata needed for standards-based Device Authorization Grant flows instead of flattening the authorization-server response down to browser-only fields.
- Keep headless MCP server login on a normal `codex mcp login --device-auth <server>` contract, with either an explicitly configured public client id or standards-based dynamic client registration when the authorization server advertises a registration endpoint.
- Preserve grant-aware registration shape so device-login DCR asks for the Device Authorization Grant, keeps `token_endpoint_auth_method=none`, and only requests `refresh_token` when server metadata does not rule it out.

User-visible behavior:

- `codex mcp login --device-auth <server>` uses the discovered `device_authorization_endpoint` and requires `grant_types_supported` to include `urn:ietf:params:oauth:grant-type:device_code`.
- The command uses the configured public MCP OAuth `client_id` when one is present; otherwise it performs dynamic client registration from OAuth discovery before requesting the device code.
- Dynamic registration keeps the request public-client shaped (`token_endpoint_auth_method=none`), uses `grant_types=["urn:ietf:params:oauth:grant-type:device_code"]`, adds `refresh_token` only when server metadata permits or omits grant support, and forwards the configured scope string when scopes are requested.
- The device-code step uses PKCE, prints the verification URL and user code, polls the token endpoint, and stores the resulting OAuth tokens through the existing MCP credential cache.
- Discovery keeps `token_endpoint`, `device_authorization_endpoint`, `registration_endpoint`, and `grant_types_supported` available to the login flow for Streamable HTTP MCP servers.
- If no configured client id exists and the authorization server does not advertise dynamic registration, the CLI fails with an explicit public-client-id-required error instead of reporting a misleading generic registration failure.
- This is an intentional downstream carry until upstream has an equivalent headless MCP OAuth login path. If upstream lands native device-login support, compare behavior and drop or re-home this carry rather than keeping both paths.

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
- `Alt+Up` dequeues the newest queued item back into the composer in strict reverse-chronological order across both entry types.
- Recalled items disappear from the queued preview until they are re-queued or re-submitted.
- `Ctrl+Shift+Q` remains the explicit "run next" path for inserting a fresh draft at the front of the queue.
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

### TUI: Side conversation local exit

Why:

- Keep `/side` conversations scoped to their parent session so closing a side
  question does not end the whole TUI session.
- Preserve the existing main-thread `/quit` and `/exit` behavior.

User-visible behavior:

- `/quit` and `/exit` in an active side conversation close that side conversation
  and return to the parent thread.
- `/quit` and `/exit` in the main conversation remain application exits.

### Review + history: downstream accounting and runtime-context alignment

Why:

- Keep review token summaries, app-server history, and review-mode effort selection aligned with the live turn state rather than stale defaults.

User-visible behavior:

- Review token usage is aligned across live flows and app-server/history views.
- Review flows reuse the runtime turn effort and preserve downstream sampling rollout context needed for faithful reconstruction.
- Thread records preserve downstream `thread_source` provenance alongside
  upstream `history_mode`; list, read, resume, and stored-session paths carry
  both fields independently.

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
