# Native Computer Use

Codex exposes configured computer-use providers as namespaced `DynamicTool`
functions. The provider owns environment-specific interaction; Codex owns
session registration, request correlation, failure projection and forwarding
native `InputImage` content to the active model turn. Browser remains the
separate `codex_browser` adapter; the Android carry restores the isolated
`codex_android` namespace and Desktop uses its own opt-in `codex_desktop`
namespace. Namespace collisions fail closed; persisted MCP entries for a
native namespace are rejected before thread startup when that provider is
configured. There is no generic provider registry or fallback.

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

The advertised schemas document optional observation serial and stability
controls; `android_step` accepts one action or an ordered `actions` array with
the supported action discriminator and its selector, coordinate, text, key,
wait, or pointer fields; and `android_install_build_from_run` requires a
`workflow_run_id`, with optional repository, artifact, and serial
selectors. A configured device serial is used when no per-call serial is given.

## Desktop

The optional `codex_desktop` namespace exposes `desktop_observe` and
`desktop_step` only when a command provider is configured. Configuration is
read from `desktop-computer-use.json` or the compatibility filename
`desktop-dynamic-tools.json` in the selected Codex home. The command may be a
string parsed into arguments or an explicit argument array; Codex starts that
program directly, without an implicit shell, default executable, or provider
fallback. `CODEX_DESKTOP_COMPUTER_USE_PROVIDER`,
`CODEX_DESKTOP_COMPUTER_USE_COMMAND`, and
`CODEX_DESKTOP_COMPUTER_USE_TIMEOUT_SECS` are the only environment overrides.
An absent or unsupported provider remains unavailable.

Requests are capped at 65,536 serialized JSON bytes before spawn. The default
deadline is 120 seconds; configured deadlines above 300 seconds fail without
clamping. Stdout is capped at 50,331,648 bytes and overflow is failure with an
unknown action result, never truncated success. Stderr capture is capped at
16,384 bytes while excess output is drained and reported as truncated. Timeout,
cancellation, partial input, and uncertain command results are not replayed
automatically; use `desktop_observe` to recover state. Successful visual
responses must include native `InputImage` content rather than only text or an
artifact path. These interfaces do not imply permission to contact a real
desktop or execute a configured real command.

## Shared boundary

Each provider is advertised only from its per-session configuration. App-server
events route exact `(namespace, tool)` pairs, reject duplicate request IDs, and
reject calls from abandoned side threads before starting a provider. The
completion preserves the original request ID and typed image content so the
existing model consumer can include it in the same active turn. Browser's
separate `codex_browser` adapter, `codex_tui`, and generic dynamic-tool routes
remain unchanged. Provider artifacts and paths are diagnostic only, never
substitutes for model-visible pixels.
