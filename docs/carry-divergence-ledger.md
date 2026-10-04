# Carry Divergence Ledger

This ledger records the current source contract for the computer-use provider
adapter carry. It is source documentation, not provider activation or delivery
acceptance.

## Namespaced native computer-use provider adapters

- Codex owns per-session `DynamicTool` advertisement, exact namespace/tool
  routing, request correlation, truthful failure projection, and forwarding
  native `InputImage` content into the active model turn.
- The existing Browser adapter remains a separate `codex_browser` provider
  seam. The Android adapter is MCP-backed and preserves the historical
  `android_observe`, `android_step`, and `android_install_build_from_run`
  behavior. Desktop is an opt-in `codex_desktop` command provider with
  `desktop_observe` and `desktop_step` only.
- Namespace collisions, duplicate call IDs, and calls from abandoned side
  threads fail closed. No general registry, shared protocol, live service,
  device, or session-lifecycle capability is added; real provider activation
  remains outside this source assignment.
- Desktop safety limits are explicit source policy rather than historical
  guarantees: 65,536-byte request JSON, 50,331,648-byte complete stdout JSON,
  16,384-byte captured stderr with continued draining and a truncation marker,
  120-second default timeout, and rejection of configured timeouts above 300
  seconds. Timeout, cancellation, output overflow, and lost output can leave a
  step effect unknown; never replay it automatically.
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
  - `.github/validation-named-tests.json`
  - `justfile`

## Local control-plane diagnostics source preparation

This entry adds a source-prepared local diagnostic contract. Existing entries
above are retained incoming source documentation, not proof that every carry
has already been composed into or qualified on the same checkout.

- The diagnostics crate owns typed, bounded in-memory recording and the common
  reducer. Exact event identities, conflicting variants, loss and incomplete
  timelines remain explicit; a missing producer is not reconstructed from
  timing. Diagnostic capture is default off.
- The finite `debug control-plane` consumer accepts explicitly selected local
  metadata inputs. Exposed host tool request/return intervals are not actual
  blocked durations; requested agent-path references are not resolved runtime
  thread identities. Raw message bodies and completed-agent text are ignored.
- Wait helper `return_when` and a native primitive's target-any/all setting
  remain separate fields. Repeated identical observed results do not establish
  redundant polling, obsolete dependencies, or logical goal completion.
- Message intent, enqueue, drain, input recording, activity publication,
  scheduling and semantic acknowledgement are separate observations. Missing
  receipts stay unknown. Scheduler joins use exact keys and tolerate
  publication before registration without reordering runtime work.
- The optional usage projection reads only an explicitly selected existing
  frozen, quiescent, no-writer/no-WAL snapshot asserted by its custodian. Those
  claims are preserved separately from the reader's observed immutable,
  read-only transaction operations. There is no state runtime initialization,
  migration, profile discovery, database creation or live-ledger fallback.
- Usage joins conserve exact call identities and same-window/source partitions.
  Requested models never fill in missing actual models. Covered credit estimates,
  unpriced rows, missing evidence and optional rate scenarios remain distinct;
  none is an assertion of billing or wake-caused cost.
- This preparation does not establish owned runtime hook emission, experimental
  app-server query integration, hosted qualification, protected landing,
  installation or live activation. It adds no remote telemetry, persistent
  diagnostic retention, provider/model calls or reader grants, and changes no
  justified short-wait, timeout, quiet-goal or queue scheduling behavior.
