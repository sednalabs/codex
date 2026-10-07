# Native Computer Use

Codex exposes configured computer-use providers as namespaced `DynamicTool`
functions. The provider owns environment-specific interaction; Codex owns
session registration, request correlation, failure projection and forwarding
native `InputImage` content to the active model turn. Browser remains the
separate `codex_browser` adapter; the Android carry restores the isolated
`codex_android` provider namespace. Namespace collisions fail closed; there is
no generic provider registry or fallback.

## Android

The `codex_android` namespace exposes `android_observe`, `android_step`, and
`android_install_build_from_run` only when the established Android MCP
provider is configured. Configuration is read from
the selected Codex home at `android-computer-use.json`; the legacy
`android-dynamic-tools.json` and `solarlab-android-dynamic-tools.json` names
remain accepted. Existing `CODEX_ANDROID_*` and `SOLARLAB_ANDROID_*` aliases
and config defaults are preserved. Optional Cloudflare Access client ID and
secret inputs are accepted only as a pair; values are never logged or added to
model-visible output.

`android_step` preserves ordered non-empty action batches, stops on the first
failure, reports completed action summaries, and requests a fresh post-action
observation. A failed or uncertain action is never replayed automatically; the
caller should recover with `android_observe`. Screenshots must be returned as
native `InputImage` content, not only text or an artifact path. The install
tool calls the existing MCP method
`interactive_session.install_build_from_run` with its 300-second timeout and
then requests a fresh observation. These interfaces do not imply permission to
contact a real service or device.

## Shared boundary

Each provider is advertised only from its per-session configuration. App-server
events route exact `(namespace, tool)` pairs, reject duplicate request IDs, and
reject calls from abandoned side threads before starting a provider. The
completion preserves the original request ID and typed image content so the
existing model consumer can include it in the same active turn. Browser's
separate `codex_browser` adapter, `codex_tui`, and generic dynamic-tool routes
remain unchanged. Provider artifacts and paths are diagnostic only, never
substitutes for model-visible pixels.
