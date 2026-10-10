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
