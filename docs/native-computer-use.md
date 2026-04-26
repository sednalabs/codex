# Native Computer-Use and Android Tooling

This document describes the first-party computer-use surface in the Sedna
Codex fork. It is intentionally scoped to Codex-owned protocol, transcript,
tool-registry, app-server, TUI, rollout, and validation behavior.

The Android runtime itself is supplied by an external Android harness or
provider. Codex owns the native contract that lets a capable client expose that
runtime to the model.

## Ownership Boundaries

- Codex owns native computer-use transcript semantics: request and response
  protocol events, model-visible function tool definitions, thread-history
  projection, app-server request routing, TUI rendering, rollout persistence,
  and rollout-trace runtime boundaries.
- An Android harness or provider owns device/session lifecycle, screenshots,
  UI digest generation, input execution, app launch behavior, and any emulator
  or physical-device setup.
- Solar Gravity Lab can consume and prove the Android flow, but it is not the
  generic owner of Codex computer-use tooling.
- Android MCP or app-specific repositories may provide runtime capability, but
  they should not redefine Codex's transcript or app-server protocol shape.

## Model-Facing Tools

Codex recognizes two bare Android dynamic tool names as native computer-use
handlers:

- `android_observe`: captures the current Android screen as model-visible
  output, optionally paired with a compact UI digest.
- `android_step`: performs one or more bounded Android actions, then returns a
  fresh post-action observation.

These tools are installed from dynamic thread tools supplied through app-server
thread start, resume, or fork requests. When the tool has no namespace and the
name is `android_observe` or `android_step`, Codex replaces the provider's
ad hoc schema with its canonical first-party function schema and registers the
handler as `ComputerUse`.

Namespaced tools are not promoted. For example, `codex_app.android_observe`
remains a normal dynamic tool. This preserves room for app-specific dynamic
tools while keeping the bare Android names as the stable native contract.

`android_observe` is treated as non-mutating. `android_step` is treated as
mutating, including compatibility aliases and batched `actions[]` calls.

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
bare `android_observe` and `android_step`.

Deferred tool search also treats bare Android dynamic tools as native
computer-use candidates, so deferred discovery loads the canonical Codex tool
definition rather than the provider's raw dynamic schema.

## Runtime Flow

1. A thread is started, resumed, or forked with `dynamicTools` containing bare
   `android_observe` or `android_step`.
2. The tool registry promotes those names to canonical Codex function tools and
   registers `ToolHandlerKind::ComputerUse`.
3. When the model calls one of those tools, `codex-core` emits a
   `ComputerUseCallRequest` event with `callId`, `turnId`, optional
   `environmentId`, `adapter`, `tool`, and JSON arguments.
4. App-server API v2 projects the event to a `computerUseCall` thread item and
   sends `item/computerUse/call` to the connected client.
5. The capable client executes the Android operation and returns
   `ComputerUseCallResponse` with text and/or image content items, `success`,
   and optional `error`.
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

Transcript visibility depends on the native computer-use event path. Android
operations are expected to enter Codex as `ComputerUseCallRequest` and
`ComputerUseCallResponse` events after bare `android_observe` or `android_step`
tool names are promoted to `ToolHandlerKind::ComputerUse`. Calls injected by an
outer host environment or compatibility bridge are useful runtime probes, but
they do not prove TUI or `Ctrl+T` transcript visibility unless they are bridged
back into those native Codex events.

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
- `codex-rs/tools/src/android_tool.rs`
- `codex-rs/tools/src/tool_registry_plan.rs`
- `codex-rs/core/src/tools/handlers/computer_use.rs`
- `codex-rs/core/src/tools/tool_search_entry.rs`
- `codex-rs/app-server/src/computer_use.rs`
- `codex-rs/app-server/src/bespoke_event_handling.rs`
- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server-protocol/src/protocol/thread_history.rs`
- `codex-rs/tui/src/app/app_server_adapter.rs`
- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/tui/src/history_cell.rs`
- `codex-rs/rollout/src/policy.rs`
- `codex-rs/rollout-trace/src/protocol_event.rs`
- `codex-rs/app-server/tests/suite/v2/computer_use.rs`
- `.github/validation-lanes.json`
- `justfile`
