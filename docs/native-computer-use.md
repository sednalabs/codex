# Native Computer-Use and Android Tooling

This document describes the first-party computer-use surface in the Sedna
Codex fork. It is intentionally scoped to Codex-owned protocol, transcript,
tool-registry, runtime-adapter, app-server, TUI, rollout, and validation
behavior.

The Android runtime itself is supplied by an external Android harness or
provider. Codex owns the native contract that lets a capable client expose that
runtime to the model, plus the first-party runtime adapter that normalizes
provider responses into Codex computer-use responses.

## Ownership Boundaries

- Codex owns native computer-use transcript semantics: request and response
  protocol events, model-visible function tool definitions, thread-history
  projection, app-server request routing, the first-party runtime adapter, TUI
  rendering, rollout persistence, and rollout-trace runtime boundaries.
- An Android harness or provider owns device/session lifecycle, screenshots,
  UI digest generation, input execution, app launch behavior, and any emulator
  or physical-device setup.
- Solar Gravity Lab can consume and prove the Android flow, but it is not the
  generic owner of Codex computer-use tooling.
- Android MCP or app-specific repositories may provide runtime capability, but
  they should not redefine Codex's transcript or app-server protocol shape.

## Model-Facing Tools

Codex recognizes three bare Android dynamic tool names as native computer-use
handlers:

- `android_observe`: captures the current Android screen as model-visible
  image output, optionally paired with a compact UI digest.
- `android_step`: performs one or more bounded Android actions, then returns a
  fresh post-action observation.
- `android_install_build_from_run`: installs a GitHub Actions Android build
  artifact into the active Android session, optionally launches it, then
  returns a fresh post-install observation when available.

The North Star is that screenshots are delivered to the model as native
`inputImage` content items in the computer-use response. Provider artifact paths
may exist for audit, replay, and diagnostics, but they are not the primary
model-facing visual channel and should not be exposed as instructions for the
model to fetch local files. If screenshot inlining fails, the response may
include a concise diagnostic that names the provider artifact involved; that is
an error breadcrumb, not the normal contract.

These tools are installed from dynamic thread tools supplied through app-server
thread start, resume, or fork requests. Fresh interactive Codex threads can also
reacquire the same native surface from local Android runtime configuration.
Core session acquisition and the first-party Android runtime adapter share the
same `codex-tools` runtime-config loader, so `$CODEX_HOME` resolution and
fallback ordering cannot drift between the model-visible tool list and the
provider that handles the call.

The shared loader accepts `CODEX_ANDROID_MCP_URL` or `SOLARLAB_ANDROID_MCP_URL`
first. It can also derive `https://<hostname>/mcp` from
`CODEX_ANDROID_MCP_HOSTNAME` or `SOLARLAB_ANDROID_MCP_HOSTNAME`, and then falls
back to `$CODEX_HOME/android-computer-use.json`,
`$CODEX_HOME/android-dynamic-tools.json`, and
`$CODEX_HOME/solarlab-android-dynamic-tools.json` when one of those files
contains a non-empty `mcp_url`. Optional Cloudflare Access credentials are read
from the matching `*_CF_ACCESS_CLIENT_ID` and `*_CF_ACCESS_CLIENT_SECRET`
environment variables by the same loader for the runtime adapter.

When the tool has no namespace and the name is `android_observe`,
`android_step`, or `android_install_build_from_run`, Codex replaces the
provider's ad hoc schema with its canonical first-party function schema and
registers the handler as `ComputerUse`.

Namespaced tools are not promoted. For example, `codex_app.android_observe`
remains a normal dynamic tool. This preserves room for app-specific dynamic
tools while keeping the bare Android names as the stable native contract.

`android_observe` is treated as non-mutating. `android_step` and
`android_install_build_from_run` are treated as mutating. Install receives a
longer response timeout than ordinary observe/step calls because it may need to
download an artifact, install an APK, launch it, and verify foreground state.

## Provider Capability and Manifest Integration

Runtime providers advertise Android support by adding dynamic tools to the
thread. `DynamicToolSpec` carries optional capability metadata:

- `family`, such as `android`
- `capabilityScope`, such as `environment`
- `mutationClass`, such as `mutating`
- `leaseMode`, such as `exclusive_write`

Codex preserves that metadata for dynamic tool discovery and state persistence,
but native Android promotion is intentionally based on the bare tool names
above. The provider capability describes the available runtime; Codex still
owns the canonical model-facing schema and computer-use handler once the bare
Android names are selected.

When capability metadata is present, app-server validates and forwards it as
part of the dynamic tool contract. That metadata describes runtime capability;
it does not replace the Codex-owned native schema or transcript behavior for
bare `android_observe`, `android_step`, and
`android_install_build_from_run`.

Acquired Android tools are marked `family=android`,
`capabilityScope=environment`, and `persistOnResume=false`. This prevents
stale device/session handles from being blindly restored; resumed and forked
threads must validate or reacquire the current environment. Legacy persisted
bare Android tools with no capability metadata are also denied on resume, so an
older thread cannot silently resurrect `android_observe`, `android_step`, or
`android_install_build_from_run` without the current runtime being configured
or explicitly supplied again. Namespaced provider tools are not denied by this
bare-name guard.

Deferred tool search also treats bare Android dynamic tools as native
computer-use candidates, so deferred discovery loads the canonical Codex tool
definition rather than the provider's raw dynamic schema.

## Runtime Flow

1. A thread is started, resumed, or forked with `dynamicTools` containing bare
   `android_observe`, `android_step`, or `android_install_build_from_run`, or
   a fresh interactive thread reacquires those tools from local Android runtime
   configuration.
2. The tool registry promotes those names to canonical Codex function tools and
   registers `ToolHandlerKind::ComputerUse`.
3. When the model calls one of those tools, `codex-core` emits a
   `ComputerUseCallRequest` event with `callId`, `turnId`, optional
   `environmentId`, `adapter`, `tool`, and JSON arguments.
4. App-server API v2 projects the event to a `computerUseCall` thread item and
   sends `item/computerUse/call` to the connected client.
5. The capable client executes the Android operation and returns
   `ComputerUseCallResponse` with text plus native image content items,
   `success`, and optional `error`. For `android_observe`, post-action
   `android_step`, and post-install `android_install_build_from_run`
   observations, screenshots should be returned as `inputImage` data URLs or
   another Codex-supported image reference, not as model-facing local artifact
   paths.
   When the Android provider is reached through an MCP-style `tools/call`
   bridge, Codex treats `structuredContent` and `content[]` as complementary:
   `structuredContent` supplies state, artifacts, and UI digests, while
   `content[]` image entries supply the model-visible pixels. A provider must
   not rely on `structuredContent` alone for visual computer-use output.
6. Codex submits the response back into the active turn, emits
   `ComputerUseCallResponse`, and passes the resulting content to the model as
   function-call output.

If no selected environment exists, Codex returns a failed native response
without sending an external client request. If the client does not answer before
the computer-use timeout, Codex unregisters the pending response and returns a
failed timeout response.

## Runtime Adapter, App-Server, and TUI Projection

Native computer-use requires app-server API v2. Older API versions receive a
failed response explaining that v2 is required.

`codex-computer-use-runtime` owns the Codex-side Android runtime adapter. It
connects to the configured provider, initializes the provider session, lists
available tools, calls provider operations, parses JSON-RPC/event-stream
responses, preserves provider `structuredContent` alongside `content[]` image
entries, and returns native `ComputerUseCallResponse` values. The TUI
app-server adapter only forwards app-server computer-use requests into this
crate; it does not own Android provider protocol details.

The v2 app-server protocol includes:

- `item/computerUse/call` server requests
- `ComputerUseCallParams`
- `ComputerUseCallResponse`
- `ComputerUseCallOutputContentItem`
- `ComputerUseCallStatus`
- `ThreadItem::ComputerUseCall`

Thread history reconstructs in-progress and completed computer-use items from
protocol events, and app-server turn snapshots replay the same
`ThreadItem::ComputerUseCall` shape on resume or thread reads. The TUI renders
live and replayed computer-use cells, including fallback messaging when the TUI
session has no native computer-use provider for the request.

Transcript visibility depends on the native computer-use event path. Android
operations are expected to enter Codex as `ComputerUseCallRequest` and
`ComputerUseCallResponse` events after bare `android_observe`, `android_step`,
or `android_install_build_from_run` tool names are promoted to
`ToolHandlerKind::ComputerUse`. Calls injected by an outer host environment or
compatibility bridge are useful runtime probes, but they do not prove TUI or
`Ctrl+T` transcript visibility unless they are bridged back into those native
Codex events.

## Rollout and Trace Semantics

Computer-use request and response events are persisted in extended rollout
mode. Rollout-trace maps them to tool-runtime start and end boundaries:

- `ComputerUseCallRequest` starts the runtime span.
- `ComputerUseCallResponse` ends the runtime span.
- Successful responses map to completed execution status.
- Failed responses map to failed execution status.

This keeps native Android calls visible in the same trace vocabulary as exec,
patch, MCP, and collaboration tool runtimes without adding separate core hooks.

## Validation

For documentation-only changes in this downstream docs set, use the lightweight
repository checks before broader hosted validation:

```bash
python3 .github/scripts/check_markdown_links.py
just downstream-docs-check
git diff --check
```

For changes that touch the divergence registry or current divergence baseline,
run the PR-local downstream docs sanity lane:

```bash
bash .github/scripts/validation-lanes/downstream-docs-check.sh
```

That lane checks formatting, registry JSON syntax, and relative Markdown links
without requiring the whole downstream fork to have a complete current
divergence registry.

When the goal is to refresh or prove the full downstream divergence baseline,
run the explicit full-history audit instead:

```bash
bash .github/scripts/validation-lanes/downstream-divergence-audit.sh
```

The full audit compares the checked-out downstream head with the current
upstream mirror and enforces registry coverage for all live downstream code
differences, so it belongs on explicit baseline-maintenance or checkpoint
validation rather than ordinary docs-only PR validation.

For implementation changes, prefer hosted validation through `validation-lab`.
The focused lanes are:

- `codex.app-server-computer-use-targeted`: app-server v2 routing, client
  response handling, and thread start/resume/fork injection.
- `codex.tui-native-computer-use-targeted`: native request/response events
  render as transcript-visible computer-use cells and can be inserted into the
  live `Ctrl+T` transcript overlay.
- `codex.native-computer-use-tool-registry-targeted`: canonical Android schema
  conversion, duplicate handling, deferred tool search, and core timeout
  cleanup.
- `codex.app-server-protocol-test`: protocol schema and thread-history
  projection coverage.
- Hosted CodeQL Rust contract checks cover native-image guard dominance,
  success-with-error contradictions, advisory text/image match drops, Android
  MCP tool-result parsing that would directly return `structuredContent` while
  dropping sibling `content[]` images, missing session startup acquisition,
  duplicated Android runtime-config source names outside the shared loader, and
  Android tool-promotion paths that reference canonical Android tools without a
  `ComputerUse` handler. The advisory text/image match query also covers
  app-server protocol conversion surfaces so native image preservation stays
  guarded across the thread-history boundary.

The local just recipes behind those lanes are:

```bash
just app-server-computer-use-targeted
just tui-native-computer-use-targeted
just native-computer-use-tool-registry-targeted
```

Do not use local Android builds or app-specific validation as the default proof
for Codex protocol/tool semantics. Use Android harness or Solar Gravity Lab
validation only when the question is the runtime provider or a consumer app,
not the generic Codex computer-use contract.

## Primary Files

- `codex-rs/protocol/src/computer_use.rs`
- `codex-rs/protocol/src/protocol.rs`
- `codex-rs/protocol/src/dynamic_tools.rs`
- `codex-rs/computer-use-runtime/src/lib.rs`
- `codex-rs/tools/src/android_runtime_config.rs`
- `codex-rs/tools/src/android_tool.rs`
- `codex-rs/tools/src/tool_registry_plan.rs`
- `codex-rs/core/src/native_android_computer_use.rs`
- `codex-rs/core/src/tools/handlers/computer_use.rs`
- `codex-rs/core/src/tools/tool_search_entry.rs`
- `codex-rs/app-server/src/computer_use.rs`
- `codex-rs/app-server/src/bespoke_event_handling.rs`
- `codex-rs/app-server-protocol/src/protocol/common.rs`
- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server-protocol/src/protocol/thread_history.rs`
- `codex-rs/tui/src/app/app_server_adapter.rs`
- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/tui/src/chatwidget/interrupts.rs`
- `codex-rs/tui/src/history_cell.rs`
- `codex-rs/rollout/src/policy.rs`
- `codex-rs/rollout-trace/src/protocol_event.rs`
- `codex-rs/app-server/tests/suite/v2/computer_use.rs`
- `.github/validation-lanes.json`
- `justfile`
