# Native Computer-Use Provider Contracts

This document defines provider-facing interoperability contracts for native
computer-use runtimes. It covers desktop, browser-shell, Chrome-extension,
and Android MCP providers, including their capabilities, request/response
schemas, permissions, lifecycle transitions, and model-visible image output.

Providers implement these contracts behind the configured command, MCP, or
provider-registry seams while Codex owns the canonical transcript and
model-facing tool contract.

The requirements use public Codex interfaces, documented operating-system and
browser APIs, and ordinary observable behavior of compatible environments.

## Codex-Owned Contract

Codex owns the transcript and model-facing computer-use contract. Providers
own runtime capability.

All providers called through the TUI command-provider seam read a single
`ComputerUseCallParams` JSON object from stdin and write a single
`ComputerUseCallResponse` JSON object to stdout. Successful visual responses
must include at least one native image content item. Text summaries,
accessibility trees, DOM digests, artifact paths, and telemetry may accompany
the image, but they do not replace it.

Providers must be side-effect conscious:

- observe tools are non-mutating
- step tools are mutating
- providers should execute batched `actions[]` in order
- every step should return a fresh post-action observation when possible
- permission and safety failures should return `success=false` plus a concise
  `error`
- provider-specific artifact paths are diagnostics and replay breadcrumbs, not
  instructions for the model to fetch local files

## Desktop Provider Contract

The desktop adapter is the provider target for macOS Screen Recording and
Accessibility implementations. It is also suitable for future native Windows
desktop providers if the OS APIs can satisfy the same contract.

Codex exposes:

- `desktop_observe`: capture the current app/window state as native image
  output, optionally paired with a compact accessibility/UI digest
- `desktop_step`: execute bounded UI actions, then return a fresh native image
  observation

The current TUI runtime seam is an external command provider configured with
`CODEX_DESKTOP_COMPUTER_USE_COMMAND` or `~/.codex/desktop-computer-use.json`.
The file form can hold either one legacy command provider or a `providers[]`
registry plus `routing.fallback_order`, so macOS Screen
Recording/Accessibility, Linux/noVNC, and future Windows desktop providers can
be selected by platform without changing Codex hot paths.

Minimum provider capabilities:

- determine whether screen-capture permission is granted, pending, denied, or
  not applicable
- determine whether accessibility/input-control permission is granted, pending,
  denied, or not applicable
- capture a screenshot or appshot suitable for model input
- produce a compact, stable UI digest from platform accessibility APIs where
  available
- execute click, text entry, key, scroll, drag, set-value, select-text, wait,
  and pointer-move actions
- map provider element identifiers and view coordinates back to the screenshot
  used in the preceding observation
- fail safely when the screen is locked, the target app disappears, or
  permission changes mid-session

Recommended macOS implementation shape:

- use ScreenCaptureKit, CoreGraphics, or another public Apple screen-capture
  API for pixels
- use public Accessibility APIs for UI tree capture, element metadata, and
  element actions
- use public event APIs for bounded input synthesis
- keep permission prompting and repair in the provider, not Codex core
- report pending permissions as retryable failures when the user is actively
  granting them
- avoid embedding product-specific app instructions in the Codex repo; app
  hints belong in provider-private data or user-approved skills

Desktop provider configuration:

```json
{
  "provider": "command",
  "platforms": ["macos"],
  "command": ["/path/to/desktop-provider", "stdio"],
  "timeout_secs": 120
}
```

## Browser Shell Provider Contract

The browser adapter is the provider target for in-app-browser shells,
Windows-hosted browser shells, Chrome extension backends, remote browsers, and
the built-in Playwright provider. The Playwright provider can remain a portable
headless fallback, but visual review should configure it as a headed Google
Chrome provider with an operator-managed display, persistent profile, and
explicit Node module path.

Codex exposes:

- `browser_observe`: capture the current browser viewport as native image
  output, optionally paired with compact page metadata
- `browser_step`: execute bounded browser actions, then return a fresh native
  image observation

Backend hints:

- `auto`: provider chooses the most suitable available browser backend
- `iab`: in-app-browser shell
- `chrome`: signed-in Chrome/profile-backed browser state

The built-in Playwright provider does not implement signed-in Chrome/profile
integration and therefore never claims `backend=chrome`. Route that backend to
an external command provider using the JSON stdin/stdout seam below; the
provider may use a public Chrome extension, Native Messaging, or CDP host and
must return only redacted provider metadata (never profile paths, cookies,
credentials, or history).

Minimum in-app-browser capabilities:

- maintain per-thread or per-conversation browser/session identity
- create, select, name, and finalize tabs
- navigate HTTP(S), localhost, and file-backed preview targets as policy allows
- execute Playwright-like or CDP-backed browser actions
- capture viewport screenshots as native image content
- expose compact tab/page metadata without leaking unnecessary profile data
- hide or show the browser surface according to task intent and user request
- release claimed user tabs or visible shell ownership at the end of a task

Recommended Windows implementation shape:

- host a visible browser shell using public WebView2, Electron, or browser
  automation APIs
- expose the same command-provider stdin/stdout contract used by other browser
  providers
- keep OS-window lifecycle, display visibility, downloads, file upload, and
  shell-specific policies provider-side
- the provider owns any native pipe or other internal transport behind the
  command-provider seam

## Chrome Extension Provider Contract

The Chrome backend needs signed-in browser state and must preserve user
control. An independent Chrome provider should use public Chrome extension,
Native Messaging, and Chrome DevTools Protocol APIs.

Compatibility requirements:

- extension-to-native-host transport is JSON-RPC-like
- the extension attaches to tabs through Chrome debugger/CDP APIs
- the provider owns tab leases, claimed user tabs, created task tabs, tab
  grouping or naming, and finalization
- the provider can list user tabs and, when explicitly permitted, query browser
  history
- the provider sends CDP events and download-state changes back to the browser
  runtime
- the provider should distinguish extension, in-app-browser, and CDP backends
  in diagnostics
- the provider should ask before interacting with new websites unless policy
  already allows that host

Provider integration interfaces:

- the provider owns native-host and extension packaging, installation, and
  repair integration
- Codex core should see only `browser_observe` and `browser_step` native
  computer-use calls plus provider diagnostics
- history access is a separate sensitive capability and should not have an
  unconditional always-allow path

## Provider Integration Interfaces

Browser, Chrome, and computer-use integrations can be represented through these
public Codex seams:

- plugin discovery and skill routing tell the agent which surface to prefer
- browser providers expose browser sessions, tabs, screenshots, and actions
- Chrome providers add signed-in browser state and user-tab claiming
- desktop providers expose app screenshots, accessibility/UI digests, and UI
  input actions
- providers are replaceable behind command, MCP, or future native-host
  transports

The fork must keep provider-specific implementation out of hot core paths.
Core/app-server/tool code should only know adapter names, canonical schemas,
request/response events, timeouts, mutating classification, and image-output
requirements.

## Validation Expectations

Provider-independent Codex changes are covered by:

- canonical tool schema tests for Android, browser, and desktop tools
- TUI provider bridge tests for command-provider JSON round trips and native
  image enforcement
- app-server computer-use round-trip tests
- downstream docs and divergence registry checks

Runtime-provider implementations need their own provider-side tests:

- permission-state transitions
- screenshot capture with native image output
- accessibility/DOM/UI digest generation
- action execution followed by fresh observation
- tab/session finalization for browser providers
- failure behavior when permissions, tabs, windows, or sessions disappear
