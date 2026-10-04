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
