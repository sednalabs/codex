# Native Computer Use

Codex exposes configured computer-use providers as namespaced `DynamicTool`
functions. The provider owns environment-specific interaction; Codex owns
session registration, request correlation, failure projection and forwarding
native `InputImage` content to the active model turn. Browser remains the
separate `codex_browser` adapter; the Android and Desktop carries restore the
isolated `codex_android` and `codex_desktop` provider namespaces. Namespace
collisions fail closed; there is no generic provider registry or fallback.

## Android

The `codex_android` namespace exposes `android_observe`, `android_step`, and
`android_install_build_from_run` only when the established Android MCP
provider is configured. Configuration is read from
`~/.codex/android-computer-use.json`; the legacy
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

## Desktop

The opt-in `codex_desktop` namespace exposes `desktop_observe` and
`desktop_step` only for one configured command provider. Configuration may be
provided by `CODEX_DESKTOP_COMPUTER_USE_COMMAND` and the existing
`CODEX_DESKTOP_COMPUTER_USE_PROVIDER` / `CODEX_DESKTOP_COMPUTER_USE_TIMEOUT_SECS`
controls, or by `~/.codex/desktop-computer-use.json` and the legacy
`desktop-dynamic-tools.json`. The configured command is parsed to argv and
executed directly; no shell, default executable, multi-provider list, or
fallback is implied. Explicit `none` and unsupported provider values remain
unavailable.

The source policy rejects serialized requests above 65,536 bytes before spawn,
accepts at most 50,331,648 stdout bytes as complete response JSON, and captures
at most 16,384 stderr bytes while continuing to drain excess output. Excess
stderr is marked as truncated; verbosity alone is not failure. The default
timeout is 120 seconds; configured values above 300 seconds are rejected, not
clamped. Timeout, cancellation, stdout overflow, or lost output can leave a
step's effect uncertain. Codex kills its owned child on timeout/cancellation,
never parses truncated JSON as success, and never replays a step automatically;
recover with `desktop_observe`. Successful visual results require native
`InputImage` content. Very large or multiple-image responses may exceed the
stdout ceiling, and commands configured for more than five minutes are
intentionally rejected.

## Shared boundary

Each provider is advertised only from its per-session configuration. App-server
events route exact `(namespace, tool)` pairs, reject duplicate request IDs, and
reject calls from abandoned side threads before starting a provider. The
completion preserves the original request ID and typed image content so the
existing model consumer can include it in the same active turn. Browser's
separate `codex_browser` adapter, `codex_tui`, and generic dynamic-tool routes
remain unchanged. Provider artifacts and paths are diagnostic only, never
substitutes for model-visible pixels.
