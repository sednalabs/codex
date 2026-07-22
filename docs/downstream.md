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

## External-agent migration containment

Repository-scoped external-agent detection and import canonicalize the project
selected by the trusted local app-server client. Before file-backed migration
work begins, static source and destination paths under that project are checked
without following symlinks; settings leaves, MCP configuration leaves, hook
scripts, source roots, destination roots, and dangling target leaves fail
closed. Home-scoped migration remains separate. These checks prevent migration
through repository symlinks; they are not a handle-relative authorization
boundary against a checkout being replaced concurrently. Shared target-leaf
checks also avoid overwriting symlink entries during home-scoped migration,
without applying the repository-root preflight to home sources.
Home-scoped imported-memory ownership is marker-based: only directories with a
regular, non-symlink `scope.json` are managed. Ordinary metadata is ignored,
while project-root and marker symlinks fail closed before detection. Existing
unchanged imports are not silently backfilled by this sync.

## Upstream hook context limits

Upstream commit `e4836f998d` owns per-hook `additionalContextLimit` behavior.
An unset value keeps the approximate 2,500-token spill default, while `0`
disables spilling for that hook. The setting is retained through hook
discovery and hashing, app-server config and hook-list responses, generated
schemas, and the TUI hook browser. This is upstream behavior, not a downstream
divergence; schema refreshes must preserve it alongside the fork's dynamic-tool
and native computer-use protocol additions.

The hook runner additionally preserves stdout and the real exit status when a
successful hook exits without consuming stdin and the parent observes
`BrokenPipe`. Other stdin write errors remain fatal. This is temporary direct
upstream-fix carry with a deterministic cross-platform regression and must be
dropped when upstream adopts equivalent behavior.

## Upstream compact-hook continuation ordering

Upstream commit `8c41ed33ce` owns the post-compaction ordering contract. After
mid-turn auto-compaction, pending `SessionStart` hooks run before sampling
continues. A stop request ends the turn; otherwise hook-provided context reaches
the immediately following sample, including when a turn compacts repeatedly.
Future syncs must preserve this ordering alongside downstream realtime and
permission-context reconstruction.

## Upstream approval rejection reasons

Upstream commit `e52c35b000` owns structured `ReviewDecision::Denied {
rejection }` values and their `denied.rejection` wire shape. Rejection reasons
must remain intact through command, patch, network, MCP, delegated, automatic
review, and shell-escalation paths, while model-visible text stays bounded.
Generated schemas must retain this upstream object beside downstream dynamic
tool, timeout, and native computer-use additions. The fork-specific
`GuardianUserAuthorization` module path remains an independent compatibility
detail, not a reason to restore the removed guardian rejection side map.

## Upstream history and hook test convergence

Upstream commit `ec3140db12` now owns the `ContextManager::raw_items()` audio
history assertion and the Windows hook fixture's
`additional_context_limit` initializer. This semantically absorbs the earlier
downstream-only test repair in `796d4248c5`; neither assertion represents live
runtime carry.

## Upstream paginated rollout lineages

Upstream commit `b7e39aa316` owns bounded local rollout-lineage resolution for
paginated threads. It follows ordered `history_base` segments, including
archived ancestors and explicit history positions, while rejecting cycles,
missing or mismatched sources, non-paginated sources, and invalid cutoffs.
Downstream history, resume, and usage overlays must consume that canonical
lineage rather than reconstructing a competing chain.

## Upstream threadless MCP connections

Upstream commit `19940967bd` lets callers create MCP connections without a
session event channel. Threadless resource reads, status snapshots, and
connector discovery skip startup notifications, decline interactive
elicitations, and continue the underlying non-interactive operation. Preserve
that behavior beside downstream MCP pagination, OAuth, blocking waits, and
runtime-snapshot safety controls.

## Upstream Linux preflight isolation

Upstream commit `44481a1c45` runs the bubblewrap `/proc` probe with a temporary
minimal read-only filesystem view instead of the requested command filesystem
and working directory. The probe still preserves the requested network
namespace mode. Downstream additionally retains the constrained-host fallback
that retries the `/proc` mount preflight without network isolation when the
network namespace itself is unavailable; the fallback must not broaden the
probe's filesystem view.

## Upstream SQLite tightening and CSV-job retirement

Upstream commit `81e89fa5af` makes the test-only SQLite constructor accept an
`AbsolutePathBuf` directly. Downstream-only state fixtures use the same checked
`.abs()` conversion; production database paths and MCP behavior are unchanged.

Upstream commit `687f05cb94` removes the legacy CSV batch-job tools
`spawn_agents_on_csv` and `report_agent_job_result`, their coordinator/runtime
state, and the obsolete job tables. The compatibility keys
`features.enable_fanout` and `agents.job_max_runtime_seconds` still parse as
no-ops. This does not remove ordinary `spawn_agent`, MultiAgentV2,
role-configured skills, child-model selection, reasoning-effort selection,
inventory, wait joins, or native/dynamic tool plumbing.

The fork preserves upstream migration `0042_drop_agent_jobs.sql` exactly and
moves the already-shipped downstream
`0042_external_agent_config_imports.sql` to version `0047`. Startup repairs
the exact legacy `0041` and `0042` checksums to downstream versions `0046` and
`0047` before applying upstream migrations. A migrated database cannot be
reopened by the pre-sync binary because that binary knows the former migration
checksums, so rollback requires a pre-upgrade database copy. The upgrade also
intentionally removes any unfinished CSV-job rows; ordinary thread rows,
spawn edges, and external-import history remain intact.

## Upstream completed-hook warning headers

Upstream commit `cf821e8ec8` moves the first line of a completed hook warning
into its TUI header after `says:`, indents any continuation lines, and avoids
rendering the warning again in the body. Hooks without a warning keep the
existing header. The downstream compact-transcript path uses the same header
shape while continuing to collapse only hook context, so rich, compact, and
raw transcript views do not disagree about warning content.

## Upstream app/read connector metadata

Upstream commit `60272096bc` enriches experimental `app/read` responses with
dark-icon URLs, distribution channel, install URL, and the display names of
plugins that declare each app. It accepts both supported dark-icon spellings
and derives plugin names without starting MCP servers.

Downstream keeps its auth-dependent plugin app routing. `app/read` synchronizes
the plugin manager from the live ChatGPT auth snapshot before loading plugin
declarations, so externally updated auth cannot leave connector metadata and
plugin display names on different auth modes. The focused transition regression
also proves that this projection does not issue an MCP request.

## Upstream exec-server Windows sandbox spawning

Upstream commit `35c2278dd5` adds a shared native process launcher for pipe,
PTY, inherited-descriptor, and Windows sandbox execution, and routes exec-server
Windows sandbox requests through it. The sync drops the fork's superseded
direct launch block while retaining the downstream proxy-aware backend policy,
prepared filesystem overrides, metrics, telemetry, and bounded final-output
behavior around the new upstream seam.

The upstream API migration missed one Wine PTY test call site, and the fork's
argument-comment lint is stricter for one optional exec-server test argument.
The downstream corrections are validation-only and should be dropped
individually as soon as upstream carries equivalent fixes.

## Upstream shared skill models

Upstream commit `56c11cf658` moves shared host and environment skill metadata,
policy, dependency, interface, and configuration-rule types into
`codex-skills`. `codex-core-skills` retains compatibility re-exports, so the
move preserves type identity, implicit-invocation defaults, and product
restriction behavior for existing consumers.

Downstream preferred-user skill-name precedence remains implemented in the
upstream `SkillsService` architecture. Plugin auth routing, dynamic tools, and
app/read's no-MCP-start projection are unchanged. The merge keeps only the
still-used compatibility import in the downstream-expanded plugin tests and
uses upstream's canonical `codex_skills::SkillConfigRules` type.

## Upstream typed app-server test helpers

Upstream commit `10cc57c95c` centralizes typed app-server initialization,
request, response, notification, thread-start, and mock-response helpers.
Downstream-expanded tests use those same helpers rather than maintaining a
parallel JSON-RPC parsing harness. Exact-head hosted run `29942410506` passed
the app-server V2 contract and core-runtime surface lanes after signed commit
`bde9e26567` migrated the remaining retained tests.

## Upstream skill-catalog notices and long-line highlighting

Upstream commit `f343d1237d` keeps omission counts in skill render reports but
suppresses omission notices for core-compatible catalogs. Downstream follows
the renamed policy-aware regression in the existing skill lane. Upstream commit
`5381edb133` falls back to plain text when one syntax-highlighted line exceeds
4 KiB; downstream adds its unit and transcript snapshot tests to the existing
TUI transcript/viewport lane and supplies the downstream-only `terminal_wait`
fixture field without changing upstream runtime behavior.

## Upstream remote compaction optimization

Upstream commit `fd3c1dc13d` avoids repeatedly estimating and cloning large
remote-compaction histories. It caches per-item token estimates, preserves an
unclamped total while rewriting trailing tool outputs, snapshots input history
only when rollout tracing is enabled, and reuses the v2 request input before
removing the compaction trigger for installed history.

The touched compaction runtime was upstream-exact before the merge and required
no downstream resolution. Existing downstream realtime world-state, hook
ordering, capacity retry, compaction metadata, and dynamic-media guardrails
remain independently tracked.

## Upstream catalog approval messages

Upstream commit `2be7d3bcd9` lets model catalogs provide approval instructions
for `never` and `unless_trusted` policies as well as `on_request`. Missing keys
retain the built-in policy text, while an explicitly empty value suppresses
only that approval section. This affects model-visible instructions, not policy
enforcement. Downstream's Schemars 1.2 protocol adapter remains in place
without changing the new field semantics.

## Upstream explicit outbound proxy routes

Upstream commit `c9ef7eff00` resolves system-proxy failures into explicit
environment-proxy or direct routes, carries `NO_PROXY` across HTTP and
WebSocket transports, and uses cached decisions first. It also provides an
async resolver that keeps serialized platform discovery off Tokio workers, but
the current production WebSocket connector still uses the synchronous resolver.
Downstream adopts the route behavior intact and retains only its sha2
0.11-compatible hexadecimal cache-key encoder.

## Upstream managed-profile proxy lookup

Upstream commit `88fac6fe10` includes permission profiles supplied by
`requirements.toml` when resolving the active profile's network proxy settings.
Duplicate profile IDs still fail closed, inheritance is resolved before proxy
construction, and top-level managed network constraints remain authoritative.
Downstream Windows elevated-backend and firewall enforcement continues after
the shared `NetworkProxySpec` is resolved.

## Upstream patch-approval test stabilization

Upstream commit `c0cd337766` increases the patch-approval test helper's Linux
per-event silence timeout from 10 to 15 seconds. The macOS floor remains 30
seconds, the suite remains excluded on Windows, and production approval policy,
protocol, request handling, and deadlines are unchanged.

## Upstream buffered code-mode yields

Upstream commit `99efeef650` adds disabled-by-default
`code_mode_buffered_exec`. When enabled, omitted nested-exec yield times default
to 30 seconds instead of 10 seconds, while explicit values stay authoritative.
The declaration reports the effective default. Downstream retains its usage,
audio, generated-image, native-tool, and `ALL_TOOLS` description additions
around that upstream behavior.

## Upstream route-aware HTTP clients

Upstream commit `9078e32371` exports a bounded route-aware client pool that
resolves the exact request URL, reuses clients per resolved route, and prevents
system-proxy transport redirects from crossing route decisions. The pool has no
production consumer at this boundary; it is upstream-owned infrastructure and
a future transport-migration harvest seam, not live downstream carry.

## Upstream external-session limits and attribution

Upstream commit `3bc49e1721` lets trusted local clients choose optional maximum
session age and count for external-agent detection while preserving the existing
30-day and 50-session defaults when omitted. Upstream commit `a30aee8d90` adds
an independent optional provider ID to import completion and failure analytics.
Neither change weakens downstream repository path containment, imported-session
identity, or migration-version carry. Custom and zero-limit coverage, an
operational maximum, and provider-ID data-hygiene bounds remain suitable
upstream harvests.

## Upstream alpha hotfix release versions

Upstream commit `9970cd706f` adds one shared release-version conversion path and
maps Python alpha hotfix versions such as `0.116.0a1.post2` to Codex tags such
as `rust-v0.116.0-alpha.1.2`. The sync adopts the upstream workflow, Python
runtime, SDK, installer-test, and conversion changes. The shell and PowerShell
installer conflicts retain downstream's broader SemVer validator because it
subsumes the new upstream tag shape while preserving Sedna prerelease suffixes
and optional build metadata.

## Upstream release distribution and plugin proxy routing

Upstream commits `cc875d61ce` and `a148e0b50a` publish verified Rust release
artifacts and channel metadata to the upstream distribution service. Commits
`94bb6a09a6` and `d937bfac84` make plugin startup and remote-plugin transport
honor system proxy settings. The accompanying generated lock repair is carried
as one exact `http 1.4.0` to `1.4.2` key correction; these changes do not add a
native browser, Android, desktop, or computer-use provider.

## Upstream optional installer source

Upstream commit `765675a122` adds opt-in `releases.openai.com` metadata and
asset downloads, GitHub fallback, installed-version verification, and legacy
package fallback to both installers. Downstream composes that feature with its
release-origin adapter: the upstream source is available for the default
`openai/codex` plus `rust-v` origin, while any configured repository or tag
prefix remains on its own GitHub metadata and asset URLs. Commit `7982aa27ff`
is codespell configuration only, and `b9800de486` supplies the explicit empty
inherited-descriptor argument in the Wine PTY test.

## Upstream MCP connection-manager structure

Upstream commit `2d85e6d3a6` splits required-server validation and tool-catalog
operations into focused connection-manager modules without changing the public
API. Downstream adopts the new structure and keeps its existing
generation-aware async catalogue snapshot and publication adapter only in the
new `connection_manager/tool_catalog.rs` module. The refactor is a cleaner seam
for separately tracked stale-server lifecycle work, but does not itself unload
servers or add a native browser, Android, desktop, or computer-use provider.

## Upstream step-scoped extension data

Upstream commit `c44c4de7b4` adds one `ExtensionData` store to each sampling
step and passes it to context, world-state, turn-input, and tool contributors.
Compaction and initial-context reconstruction retain that captured store.
Downstream adopts it as the canonical step-local extension seam: dynamic tools,
image generation, memories, skills, goals, web search, realtime world state,
and native computer-use adapters should compose through it rather than adding
parallel state in hot session code. This is extension infrastructure, not a new
native browser, Android, desktop, or computer-use provider.

## Upstream compacted rollout item construction

Upstream commit `f69f88f811` centralizes persisted `CompactedItem`
construction in `Session::replace_compacted_history`, after assigning missing
response-item IDs. Compaction callers now pass only message and window
metadata, ensuring the live and persisted replacement histories use the same
items. Downstream adopts this boundary without a carry-specific patch;
capacity retry, realtime world state, hooks, dynamic media, compaction
metadata, and step-scoped extension data remain composed around it.

## Validation policy

- use tiny local static sanity checks first (`git diff --check`, schema parsing, and conflict-marker scans)
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

Current downstream audit baseline (validated on `2026-07-23`):

- downstream integration code tree:
  `c132d50f17ef07018817dfdaca569a170208af88`
- comparison basis: `upstream/main`
- mirror branch `upstream-main` (`origin/upstream-main`):
  `f343d1237d8d360e8224997a846acde0b04a17cd`
- `upstream/main`:
  `f343d1237d8d360e8224997a846acde0b04a17cd`
- downstream divergence counts (`upstream/main...main`):
  `0` upstream ahead, `2019` downstream ahead
- downstream-only non-merge commits: `1717` unique, `0` patch-equivalent
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

### Core: source-owned unified-exec final transcript

Why:

- Keep final command output and omission accounting independent of the
  best-effort live delta channel, which may lag under sustained output.

User-visible behavior:

- `ExecCommandEnd.aggregated_output` comes from a bounded, non-draining process
  transcript recorded before the response buffer and output-delta broadcast.
- Normal completion waits up to the established exec I/O drain bound for
  stdout/stderr or exec-server source closure, then orders already-published
  deltas before the final command event. Chunks recorded before that bound are
  retained; output produced only after the deadline is outside the completed
  transcript.
- An exec-server `Exited` event starts the same bound immediately, so a later
  `Closed` event delayed by inherited descriptors cannot suppress completion.
- Large output still intentionally retains only its head and tail, with an
  explicit marker for omitted middle bytes.

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
- Ordinary dynamic-tool and code-mode audio follows upstream preparation,
  duration-aware accounting, truncation, compaction, and history replay while
  preserving downstream optional image detail. The upstream audio fixtures are
  adapted to the flat compatibility record rather than reviving the tagged
  namespace representation. Cargo and Bazel locks must be regenerated from the
  merged graph so Symphonia resolves to the dependency version actually
  selected downstream; parent lock entries are not safe to union mechanically.
- `android_observe` is non-mutating; `android_step` is mutating and supports both compatibility single-action fields and preferred batched `actions[]`, including atomic two-to-five-pointer `multi_touch` input that never degrades to sequential single-touch calls; `android_install_build_from_run` is mutating and maps provider-side artifact installation into the same native transcript path.
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
- The stable protocol-v1 host payload remains unchanged; after a host round trip, code mode reconstructs MCP short-name/module catalog metadata from the canonical `mcp__server__tool` name.
- Namespaced custom tool calls preserve their namespace through `ToolRouter`, so MCP/app custom tools route by their registry name instead of a flattened plain name.
- Downstream code-mode examples therefore differ slightly from upstream examples that still inline `declare const tools: { ... }`.

### Sub-agent orchestration: selection compatibility, richer inventory, and blocking joins

Why:

- Upstream now owns configured default and explicit
  `spawn_agent(model=..., reasoning_effort=...)` child selection, applies role
  settings after selection, and validates the final effective pair.
- Upstream now unifies multi-agent settings and role declarations under
  `[agents]`, normalizes legacy `max_threads` to
  `max_concurrent_threads_per_session`, and exposes `agent_type` only when roles
  are configured. Default sub-agent model and reasoning settings are active
  upstream behavior rather than downstream carry.
- Preserve flattened `[agents.<role>]` declarations in the generated schema
  despite the downstream Schemars 1.2 generator: named roles must remain
  role-valued additional properties rather than being rejected as unknown.
- Preserve downstream compatibility at the edges of that upstream pipeline:
  incomplete reasoning-support metadata must not reject a valid selection, and
  role reload must retain the resolved runtime provider object, reasoning
  summary, and verbosity when the role does not replace them.
- Keep models with unspecified MultiAgentV2 backend metadata selectable while
  rejecting models known to belong to a different backend. Luna is the narrow
  downstream exception: it remains selectable for V2 child work while its
  upstream catalog entry still defaults top-level Luna sessions to V1. Remove
  the rule when upstream marks Luna V2-compatible.
- Surface the effective resolved child settings directly in the tool layer so callers can see what actually launched.
- Let downstream multi-agent orchestration block on clear tool contracts (`list_agents`, `inspect_agent_tree`, `wait_agent(return_when=...)`) instead of transcript polling.
- Upstream-native reimplementation is welcome when it preserves the live nested-agent visibility, the cheap `list_agents` surface, the richer `inspect_agent_tree` inspection, and the explicit blocking `wait_agent` contract so we can shrink the divergence without losing the downstream visibility model.

User-visible behavior:

- Configured defaults and explicit child `model` and
  `model_reasoning_effort` requests follow upstream selection and role
  precedence. Downstream does not duplicate that pipeline; it preserves the
  runtime provider object, `model_reasoning_summary`, and verbosity across an
  unrelated role reload.
- The v1 `spawn_agent` result stays on the upstream `agent_id`/`nickname` shape. The v2 result returns its canonical `task_name`, includes `agent_id` and `nickname` only when spawn metadata is visible, and reports requested/effective model and reasoning fields so callers can see what actually launched after role/profile resolution. Role, status, identity source, provider ID, and the preserved `model_reasoning_summary` remain inventory or internal metadata rather than raw spawn-result fields.
- V2 description tests resolve the configured tool namespace rather than
  assuming an upstream or downstream literal, and a delegate cancelled before
  launch returns `TurnAborted` without spawning a child session.
- Role-defined fields remain authoritative over configured defaults and
  explicit child requests, with final compatibility validation performed after
  role application.
- The built-in `explorer` role no longer hard-locks a model or reasoning setting; instead the cheap-first policy lives in availability-aware `spawn_agent` behavior and supporting guidance so codebase-question lanes stay compatible with the caller's loaded model catalog.
- The built-in `terminal-babysitter` role intentionally locks
  `gpt-5.4-mini` with low reasoning for bounded monitored waits.
- `list_agents` remains the always-on, cheap live inventory view across both collaboration surfaces rather than being hidden behind `MultiAgentV2`; it exposes `has_active_subagents` / `active_subagent_count` plus nested visibility/status metadata so callers retain nested-agent live visibility without dumping full trees.
- `inspect_agent_tree` is the intentionally richer downstream observability surface, separate from `list_agents`: it inspects the current subtree or a target path, can toggle `live` versus `stale` descendant visibility, can filter to selected branches with `agent_roots`, and returns compact tree rows with bounded depth and row limits.
- `wait_agent` supports `return_when=any|all` and returns `requested_ids`, `pending_ids`, `completion_reason`, and `timed_out`. Those completion fields are tool-output-only; canonical transcript items retain identities and status snapshots without duplicating timeout, mailbox, or pending outcome state. In the v2 surface, callers may omit `targets` when they intentionally want to wait only for current-turn input activity, such as mailbox delivery or user steering, or timeout.
- When V2 reaches its resident-thread capacity, it may evict the least-recently
  used quiescent child after a clean shutdown. Completed and errored identities
  retain final outcomes, while an interrupted identity retains its resumable
  state; all three use a compact cold status for `list_agents` and direct agent
  tools, remain reloadable from persisted history, and cannot be deleted by a
  stale eviction racing with a replacement thread instance.
- One generation-scoped lifecycle authority serializes V2 unload, reload,
  message delivery, and explicit close. Queue-only `send_message` mail can move
  into a registry-owned FIFO without waking a cold runtime; `followup_task`
  reloads once on an eviction-independent gate and transfers that FIFO ahead of
  its triggering message. Failed transitions keep the FIFO recoverable, and
  explicit close discards it.
  Existing subscriptions spanning a cold reload remain separate follow-up work.
- Ephemeral V2 children have no reloadable persisted history, so they are not
  eligible for cold eviction; capacity pressure fails closed and leaves the
  existing runtime resident.
- Roles that explicitly set `model`, `model_provider`, `model_reasoning_effort`, or `model_verbosity` continue to be authoritative, even when a child requests a different setting.
- Full-history forks keep their conversation and agent identity while accepting
  configured or explicit child model/reasoning selection; they still reject an
  explicit `agent_type` because that would change the preserved identity.
- Paginated cold V2 reload restores the child's newest persisted
  `approvals_reviewer` alongside its indexed model, provider, reasoning, role,
  and agent path, rather than inheriting the reload caller's ambient reviewer.
- Docs and tooling now spell out the precedence stack and the intended `list_agents` / `inspect_agent_tree` / `wait_agent` workflow: cheap live view first to keep nested-agent visibility, compact nested or stale inspection when deeper context is needed, and blocking wait only when a transition must complete.

Primary files:

- `codex-rs/core/src/agent/role.rs`
- `codex-rs/core/src/agent/role_tests.rs`
- `codex-rs/core/src/agent/builtins/terminal-babysitter.toml`
- `codex-rs/core/src/agent/control.rs`
- `codex-rs/core/src/agent/control/lifecycle.rs`
- `codex-rs/core/src/agent/control/residency.rs`
- `codex-rs/core/src/agent/registry.rs`
- `codex-rs/core/src/thread_manager.rs`
- `codex-rs/config/src/config_toml.rs`
- `codex-rs/core/config.schema.json`
- `codex-rs/core/src/codex_delegate.rs`
- `codex-rs/core/src/config/schema_tests.rs`
- `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_v2/list_agents.rs`
- `codex-rs/core/src/tools/handlers/inspect_agent_tree.rs`
- `codex-rs/core/src/tools/handlers/multi_agents/wait.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_v2/wait.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_spec.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_spec_tests.rs`
- `codex-rs/core/src/tools/spec_plan.rs`
- `codex-rs/core/src/tools/tool_runtime_capabilities.rs`
- `codex-rs/core/tests/suite/spawn_agent_description.rs`
- `codex-rs/core/tests/suite/subagent_notifications.rs`
- `.github/scripts/test_ci_planners.py`
- `justfile`
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

### TUI: retained realtime voice transport

Why:

- Upstream removed its realtime WebRTC crate and TUI voice surface, while the
  downstream non-Linux voice path still depends on that transport.
- Keep the transport isolated in `codex-realtime-webrtc` rather than spreading
  platform-specific WebRTC code through the TUI.

User-visible behavior:

- macOS retains the realtime offer/answer flow, microphone audio track, and
  local audio-level events consumed by the TUI voice session.
- Unsupported targets return an explicit unavailable error; Linux retains the
  corresponding TUI stubs rather than silently exposing an unusable session.
- The carry includes `codex-rs/realtime-webrtc/{Cargo.toml,BUILD.bazel}` and
  `src/{lib.rs,native.rs}`, plus the target-specific TUI dependency and lock
  graph. Hosted Bazel release and clippy jobs on macOS are the buildability
  proof for this platform boundary.

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
- Goal-first and forked threads cold-resume their newest persisted approval
  policy and permissions together with model, provider, reasoning effort,
  approvals reviewer, cwd, and active named permission profile. Explicit resume
  overrides still win, and a later `TurnContext` supersedes an older settings
  snapshot.
- Thread-store writes serialize canonical rollout append order with derived
  SQLite metadata observation and persistence barriers. Concurrent handles to
  one live thread therefore cannot leave indexed model, provider, reasoning,
  or cwd metadata ordered differently from the JSONL settings history. The
  holistic ordering assertion canonicalizes both cwd values so Windows verbatim
  prefixes do not masquerade as metadata drift.

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

### Windows sandbox: proxy-aware launch and command-cwd policy

Why:

- Managed proxy enforcement requires the elevated Windows backend to apply its
  firewall identity consistently across direct and unified exec.
- App-server `command/exec` permission profiles are project-scoped and must not
  lose the trusted command project's Windows sandbox mode.

User-visible behavior:

- The `/tmp` special permission root resolves only on Unix; Windows policy
  construction does not reinterpret it as a drive-root path.
- The compatible restricted token excludes Everyone from its restricting SID
  set while retaining Everyone on the default DACL needed for child-process
  pipes and IPC.
- Workspace writes fail closed if the matching root-capability ACE cannot be
  installed; the restricted child is never launched after a silently failed
  grant. A direct local-filesystem helper regression asserts the promised write
  on release-shaped targets and reports the exact Bazel gnullvm loader
  incompatibility instead of treating it as an ACL denial. Hosted Windows MSVC
  proof remains required before promotion.
- Write-root ACL refresh uses effective rights for required access but only
  explicit allow ACEs when checking stale `FILE_DELETE_CHILD`. An inherited
  grant does not trigger a repair that `SET_ACCESS` cannot make converge.
- Proxy-enforced Windows commands use one effective elevated-backend decision
  for filesystem overrides, PowerShell startup, process spawning, and telemetry.
- Unified exec cannot bypass the managed proxy with an unrelated direct
  loopback connection.
- App-server commands using `permissionProfile` resolve the Windows sandbox
  mode from the command `cwd` alongside the rest of that project's policy.
