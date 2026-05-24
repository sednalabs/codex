# Native Computer-Use Adapter Tooling

This document describes the first-party computer-use surface in the Sedna
Codex fork. It is intentionally scoped to Codex-owned protocol, transcript,
tool-registry, app-server, TUI, rollout, and validation behavior.

Native runtime backends are supplied by external providers. Android is the
first implemented provider. Browser is now a registered adapter with a TUI
provider bridge that can either invoke an operator-configured command or use
the built-in Playwright backend for `backend=auto`.

## Ownership Boundaries

- Codex owns native computer-use transcript semantics: request and response
  protocol events, model-visible function tool definitions, thread-history
  projection, app-server request routing, TUI rendering, rollout persistence,
  and rollout-trace runtime boundaries.
- Runtime providers own backend/session lifecycle, screenshots or viewport
  capture, UI digest generation, input execution, launch/navigation behavior,
  and any emulator, physical-device, browser, or desktop setup.
- Solar Gravity Lab can consume and prove the Android flow, but it is not the
  generic owner of Codex computer-use tooling.
- Android MCP or app-specific repositories may provide runtime capability, but
  they should not redefine Codex's transcript or app-server protocol shape.

## Model-Facing Tools

Codex promotes a small set of bare dynamic tool names to native computer-use
handlers. Namespaced tools remain ordinary dynamic tools.

### Android

- `android_observe`: captures the current Android screen as model-visible
  image output, optionally paired with a compact UI digest.
- `android_step`: performs one or more bounded Android actions, then returns a
  fresh post-action observation.
- `android_install_build_from_run`: installs a GitHub Actions Android build
  artifact into the active Android session, optionally launches it, then
  returns a fresh post-install observation when available.

### Browser

- `browser_observe`: captures the current browser viewport as model-visible
  image output, optionally paired with compact page metadata.
- `browser_step`: performs one or more bounded browser actions, then returns a
  fresh post-action viewport observation.

Browser tools include a `backend` hint with `auto`, `iab`, and `chrome`
values. `auto` lets the provider choose the best available browser backend.
`iab` is intended for the Codex app in-app browser. `chrome` is intended for
signed-in Chrome-extension-backed browser state. The hint is part of the
provider contract. The current TUI bridge handles `auto` through Playwright or
forwards any backend to an operator-configured provider command. `iab` and
`chrome` require such a provider command until a dedicated in-app-browser or
Chrome-extension bridge is connected.

The TUI bridge is intentionally pluggable:

- `CODEX_BROWSER_COMPUTER_USE_PROVIDER=playwright` enables the embedded
  Playwright bridge for `backend=auto`. The bridge launches a persistent
  Chromium profile, executes bounded browser actions, and returns the viewport
  screenshot as native `inputImage` content. This backend requires `node` and a
  Playwright package that Node can resolve in the runtime environment.
- `CODEX_BROWSER_COMPUTER_USE_COMMAND` points to an external provider command.
  Codex sends `ComputerUseCallParams` JSON on stdin and expects a
  `ComputerUseCallResponse` JSON object on stdout. This is the extension point
  for in-app-browser, signed-in Chrome, remote, or hosted browser providers.
- `~/.codex/browser-computer-use.json` may provide the same configuration with
  `provider`, `command`, `node`, `timeout_secs`, `state_dir`, and `headless`
  fields.

Example command-provider configuration:

```json
{
  "command": ["node", "/path/to/browser-provider.mjs"],
  "timeout_secs": 120
}
```

Example built-in Playwright configuration:

```json
{
  "provider": "playwright",
  "state_dir": "/path/to/browser-state",
  "headless": true,
  "timeout_secs": 120
}
```

An external command provider should read one `ComputerUseCallParams` JSON object
from stdin and write one `ComputerUseCallResponse` JSON object to stdout. For
successful visual responses, that object must include a native `inputImage`
content item. The TUI bridge fails loudly when a successful browser provider
response contains only text, metadata, or artifact paths.

The North Star is that screenshots are delivered to the model as native
`inputImage` content items in the computer-use response. Provider artifact paths
may exist for audit, replay, and diagnostics, but they are not the primary
model-facing visual channel and should not be exposed as instructions for the
model to fetch local files. If screenshot inlining fails, the response may
include a concise diagnostic that names the provider artifact involved; that is
an error breadcrumb, not the normal contract.

These tools are installed from dynamic thread tools supplied through app-server
thread start, resume, or fork requests. When the tool has no namespace and the
name matches one of the native tool names above, Codex replaces the provider's
ad hoc schema with its canonical first-party function schema and registers the
handler as `ComputerUse`.

Namespaced tools are not promoted. For example, `codex_app.android_observe`
and `codex_app.browser_step` remain normal dynamic tools. This preserves room
for app-specific dynamic tools while keeping the bare names as the stable
native contract.

Observe tools are treated as non-mutating. Step and install tools are treated
as mutating. Android install receives a longer response timeout than ordinary
observe/step calls because it may need to download an artifact, install an APK,
launch it, and verify foreground state.

## Provider Capability and Manifest Integration

Runtime providers advertise support by adding dynamic tools to the thread.
`DynamicToolSpec` carries optional capability metadata:

- `family`, such as `android`
- `capabilityScope`, such as `environment`
- `mutationClass`, such as `mutating`
- `leaseMode`, such as `exclusive_write`

Codex preserves that metadata for dynamic tool discovery and state persistence,
but native promotion is intentionally based on the bare tool names above. The
provider capability describes the available runtime; Codex still owns the
canonical model-facing schema and computer-use handler once the bare native
names are selected.

When capability metadata is present, app-server validates and forwards it as
part of the dynamic tool contract. That metadata describes runtime capability;
it does not replace the Codex-owned native schema or transcript behavior for
bare `android_observe`, `android_step`, `android_install_build_from_run`,
`browser_observe`, or `browser_step`.

Deferred tool search also treats bare native dynamic tools as computer-use
candidates, so deferred discovery loads the canonical Codex tool definition
rather than the provider's raw dynamic schema.

## Runtime Flow

1. A thread is started, resumed, or forked with `dynamicTools` containing bare
   native computer-use tools such as `android_observe`, `android_step`,
   `android_install_build_from_run`, `browser_observe`, or `browser_step`.
2. The tool registry promotes those names to canonical Codex function tools and
   registers `ToolHandlerKind::ComputerUse`.
3. When the model calls one of those tools, `codex-core` emits a
   `ComputerUseCallRequest` event with `callId`, `turnId`, optional
   `environmentId`, `adapter`, `tool`, and JSON arguments.
4. App-server API v2 projects the event to a `computerUseCall` thread item and
   sends `item/computerUse/call` to the connected client.
5. The capable client executes the provider operation and returns
   `ComputerUseCallResponse` with text plus native image content items,
   `success`, and optional `error`. For observe, step, and install
   observations, screenshots or browser viewports should be returned as
   `inputImage` data URLs or another Codex-supported image reference, not as
   model-facing local artifact paths.
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

## App-Server and TUI Projection

Native computer-use requires app-server API v2. Older API versions receive a
failed response explaining that v2 is required.

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

Transcript visibility depends on the native computer-use event path. Provider
operations are expected to enter Codex as `ComputerUseCallRequest` and
`ComputerUseCallResponse` events after bare native tool names are promoted to
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
- `codex.native-computer-use-tool-registry-targeted`: canonical Android and
  browser schema conversion, adapter classification, duplicate handling,
  deferred tool search, and core timeout cleanup.
- `codex.app-server-protocol-test`: protocol schema and thread-history
  projection coverage.
- Hosted CodeQL Rust contract checks cover native-image guard dominance,
  success-with-error contradictions, advisory text/image match drops, and a
  regression sentinel for Android MCP tool-result parsing that would directly
  return `structuredContent` while dropping sibling `content[]` images. The
  advisory text/image match query also covers app-server protocol conversion
  surfaces so native image preservation stays guarded across the thread-history
  boundary.

The local just recipes behind those lanes are:

```bash
just app-server-computer-use-targeted
just tui-native-computer-use-targeted
just native-computer-use-tool-registry-targeted
```

Do not use local Android builds, browser sessions, or app-specific validation
as the default proof for Codex protocol/tool semantics. Use Android harness,
browser-provider, or Solar Gravity Lab validation only when the question is the
runtime provider or a consumer app, not the generic Codex computer-use
contract.

## Primary Files

- `codex-rs/protocol/src/computer_use.rs`
- `codex-rs/protocol/src/protocol.rs`
- `codex-rs/protocol/src/dynamic_tools.rs`
- `codex-rs/tools/src/android_tool.rs`
- `codex-rs/tools/src/browser_tool.rs`
- `codex-rs/tools/src/computer_use_tool.rs`
- `codex-rs/core/src/tools/handlers/computer_use.rs`
- `codex-rs/core/src/tools/tool_search_entry.rs`
- `codex-rs/core-plugins/src/lib.rs`
- `codex-rs/app-server/src/computer_use.rs`
- `codex-rs/app-server/src/bespoke_event_handling.rs`
- `codex-rs/app-server-protocol/src/protocol/common.rs`
- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server-protocol/src/protocol/thread_history.rs`
- `codex-rs/tui/src/android_computer_use_provider.rs`
- `codex-rs/tui/src/browser_computer_use_provider.rs`
- `codex-rs/tui/src/browser_playwright_provider.mjs`
- `codex-rs/tui/src/computer_use_provider.rs`
- `codex-rs/tui/src/app/app_server_adapter.rs`
- `codex-rs/tui/src/app/app_server_events.rs`
- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/tui/src/chatwidget/interrupts.rs`
- `codex-rs/tui/src/history_cell.rs`
- `codex-rs/rollout/src/policy.rs`
- `codex-rs/rollout-trace/src/protocol_event.rs`
- `codex-rs/app-server/tests/suite/v2/computer_use.rs`
- `codex-rs/tools/src/android_tool_tests.rs`
- `codex-rs/tools/src/browser_tool_tests.rs`
- `codex-rs/tools/src/computer_use_tool_tests.rs`
- `.github/validation-lanes.json`
- `justfile`
