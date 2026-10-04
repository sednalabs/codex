# Downstream Tool Surface Matrix

This focused matrix records the restored namespaced computer-use surfaces. It
does not claim that provider implementations or their external environments
are available on every installation.

| Adapter | Dynamic-tool namespace | Tools | Registration and result contract |
| --- | --- | --- | --- |
| Browser | `codex_browser` | `browser_observe`, `browser_step` | Existing separate Browser adapter; retains its provider and lifecycle contract. |
| Android | `codex_android` | `android_observe`, `android_step`, `android_install_build_from_run` | Thin adapter to the existing Android MCP service; preserves ordered actions and returns typed `InputImage` content. |
| Desktop | `codex_desktop` | `desktop_observe`, `desktop_step` | Explicit opt-in bounded JSON command provider; returns typed `InputImage` content on successful visual results. |

Codex advertises configured providers as per-session `DynamicTool` namespace
specifications and routes only exact namespace/tool pairs. Namespace collisions
fail closed. Duplicate request IDs are ignored, and abandoned-side-thread
requests are rejected before provider start. The completion event correlates
to the existing active turn; no new protocol, general registry, provider
lifecycle framework, or `ComputerUseCall` event is introduced by these adapters.

Desktop source safety policy: request JSON is limited to 65,536 bytes; stdout
JSON to 50,331,648 bytes; captured stderr to 16,384 bytes (excess is drained
with a truncation marker). Timeout defaults to 120 seconds and configured
values above 300 seconds are rejected before spawn. Timeout, cancellation, or
lost output is an uncertain result and is never replayed automatically.
