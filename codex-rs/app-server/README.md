# MCP App UI

`mcpToolCall.mcpAppUi` records the invoked descriptor's `resourceUri`
and `preferredModelDisplayMode` (`inline` or `fullscreen`). Descriptors with a widget
URI default to `inline` when the preference is missing or unsupported. The
UI information is preserved in tool-call events and saved history so clients can
render without waiting for the full MCP catalog.

The field is null for older history and tools that declare widgets only in
result metadata; clients retain catalog discovery for those calls. Existing
resource URI fields remain available for older clients.

# Initial Daybreak choice (experimental)

Persistent threads accept `daybreakEnabled` on `thread/start` with the
`experimentalApi` opt-in. The response and `thread/started` notification both
include the initial choice in `thread.daybreakEnabled`. The choice is staged
with the thread's other initial metadata and saved when the thread is persisted.
An unused thread is not guaranteed to survive restart. Omitted or null leaves
the choice unset. Ephemeral threads cannot save it.
Use `thread/metadata/update` for later changes. This preference does not select
`turn/start.cyberAccessProgram` or grant access to an access program.

# User verification cancellation (experimental)

Local UI clients can cancel a native user-verification RPC by sending
`userVerification/cancel` with `{requestId}` and the `experimentalApi` opt-in.
The result is an empty acknowledgment (`{}`). This API does not enable desktop
verification capability advertisement.

`requestId` is the original status, enroll, delete, or verify RPC's string or
integer ID on the same connection, not the server elicitation ID. Use fresh IDs
for each operation and a distinct ID for the cancel RPC. Unknown, finished,
unrelated, and other-connection requests are no-ops.

The acknowledgment confirms the cancellation signal without waiting for the OS
prompt to close. The original RPC completes independently, with
`cancelled/interrupted` when cancellation prevents completion. Cancellation
cannot roll back completed effects. It remains effective while a proof waits for
outbound queue capacity, but cannot retract a response already enqueued.

Canceling or resolving an elicitation does not itself stop a separate
`userVerification/verify` RPC. Clients must cancel that RPC separately and discard
late proofs after the approval is canceled or resolved. Only one native worker
runs per app-server; if an OS call remains active after cancellation or timeout,
subsequent local operations return `failed/providerError` until that worker exits.

# Hosted Codex Apps MCP protocol

The host-owned HTTP `codex_apps` server uses Legacy by default in app-server and
standalone Codex. To discover the 2026-07-28 protocol, set
`codex_apps_mcp_2026_07_28 = true` under `[features]`, or send a true runtime
override via `experimentalFeature/enablement/set`. Discovery falls back to Legacy
when the server does not support it. Explicit config takes precedence.
The dedicated setting does not apply to third-party HTTP or local `codex_app`
stdio servers. The existing `mcp_2026_07_28` flag still governs eligible other
servers, regardless of whether their names or URLs resemble hosted Apps.
App-server does not persist this selection.

# Thread removal

`thread/archive` and `thread/delete` reject attempts to remove a live internal
worker with JSON-RPC error `-32600`. The worker's owner controls its shutdown.
For example, a Guardian reviewer remains available to its parent conversation
after a client tries to archive or delete it.

After the owner releases the worker, its saved conversation can be archived or
deleted normally. Ordinary client-controlled threads keep their existing behavior.

## User verification (experimental)

Codex app-server advertises `openai/elicitation.userVerification` to the
host-owned plugin service for bundled, in-process TUI sessions (`codex-tui`) and
local stdio desktop sessions (`Codex Desktop`) on devices with supported biometric
hardware and the `experimentalApi` opt-in. This is an app-server decision,
independent of whether a key exists; TUI/Desktop/mobile do not advertise this MCP
capability. Mobile integration requires a separate rollout. Other clients and
network connections do not receive this mode, even with a recognized client name.
Before sending verification requests to desktop sessions, deploy a GUI that
handles the typed verification request, cancellation, and late proofs. The general
`experimentalApi` opt-in does not identify a compatible GUI version.

Local UI clients use five methods. They require the existing
`experimentalApi` opt-in. The local provider reports
`unavailable/providerUnavailable` on unsupported platforms or without the required
ChatGPT account identity.

| Method | Params | Result |
| --- | --- | --- |
| `userVerification/status` | `{}` | `{credentialId, unavailableReason, unavailableMessage}` |
| `userVerification/enroll` | `{}` | `{credentialId, algorithm?, publicKey?}` |
| `userVerification/delete` | `{}` | `{}` |
| `userVerification/verify` | `{challenge, title, description}` | `{proof: {credentialId, signature}}` |
| `userVerification/cancel` | `{requestId}` | `{}` |

Status reads local readiness without prompting or contacting a backend. A null
`unavailableReason` means local checks passed, not that registration is valid.
Unsupported platforms and missing account identity are reported in the status
response's `unavailableReason` field.
Enrollment creates or reuses the local key and returns its public metadata. The
`publicKey` is unpadded base64url SPKI-DER; `algorithm` is `ecdsaP256Sha256X962`.
During the experimental rollout, `algorithm` and `publicKey` are optional for
compatibility with older app-servers. Current servers populate both fields;
callers must check that both are present and non-null before backend registration.
The trusted UI host owns backend registration: obtain an enrollment challenge,
sign it with `userVerification/verify`, check that the proof's `credentialId`
matches this response, and submit the public metadata and proof to the backend.
Local success is not server enrollment. The caller must preserve the authenticated
account across this flow and reconcile uncertain registration before retrying.
Deletion removes the local key; the caller owns backend revocation.
Enrollment and deletion coordinate credential lifecycle; callers do not issue
separate generate or rotate commands. Identity comes from the authenticated
account; this API exposes no caller-selected scope.

Verify signs 1–4096 decoded challenge bytes using P-256 ECDSA with SHA-256. The
challenge and DER signature use unpadded base64url. Title is 1–256 UTF-8 bytes;
description is at most 4096 bytes. The UI obtains approval for that display
context before calling. Verify does not require a pending elicitation; a UI with
its own authenticator can return proof directly in elicitation response content.
The calling flow owns pending-request checks and discards late proofs.
Native enroll, delete, and verify accept local stdio and in-process connections.
WebSocket and remote-control peers must use their own device authenticator;
status remains available for local readiness. Dropping an embedded RPC, disconnecting,
or changing authentication cancels its native operation. Responses recheck the
captured identity after waiting for outbound queue capacity.
Canceling or resolving an elicitation does not itself stop a separate
`userVerification/verify` RPC. The GUI must use `userVerification/cancel` to
cancel that RPC and discard late proofs when an approval is canceled or resolved.
See [User verification cancellation](#user-verification-cancellation-experimental)
for request ID and acknowledgment semantics.
Only one native worker runs per app-server. If an OS call remains active after
cancellation or timeout, subsequent local operations return `failed/providerError`
until that worker exits.

Failures use the normal JSON-RPC error envelope with closed `{type, reason}` data:
`invalidRequest`, `unavailable`, `cancelled`, or `failed`. UI clients branch on
these values rather than message text. Native diagnostic payloads stay private.

## Managed model provider requirements

Existing threads retain their provider configuration. Input RPCs reject requests when managed
`model_provider` or `model_providers` requirements no longer match that configuration, or cannot
be loaded. This covers turn start/steer, review, compaction, manual queue start, and active goal
updates. Realtime connections use separate routing configuration and are not checked here.
Interrupt, realtime stop, and goal pause/clear remain available. User and project
configuration changes alone do not invalidate existing threads.

# Amazon Bedrock authentication

If `model_providers.amazon-bedrock.aws.credential_export` is configured, Bedrock setup and
Bedrock login return an error without changing configuration or saved credentials. Remove the
exporter configuration before selecting another credential source. `aws.credential_export` and
`aws.profile` cannot be configured together.

## Stored thread attachments

- `thread/attachment/add` — add a durable resource reference to a stored thread without loading it. Repeated writes with the same attachment type and identity key return the existing attachment.
- `thread/attachment/list` — list attachments for one stored thread in a cursor-paginated request, including a thread that is not loaded.
- `thread/attachment/remove` — remove an attachment by its thread, attachment type, and identity key; returns `{}`.
- `thread/attachment/updated` — notification broadcast after an attachment is created or removed; contains the thread, attachment identity, attachment id, and operation.
### Example: Manage stored thread attachments

Attachments record the resources currently associated with a thread, independently of conversation history. Clients can add, remove, and list attachments for one stored thread at a time without resuming those threads. Adding or removing an attachment does not create or delete the underlying resource or rewrite history. An attachment is idempotently identified by its thread, `attachmentType`, and `identityKey`. For pull requests, clients should reuse the canonical application identity `JSON.stringify([canonicalHostname, lowercaseOwner, lowercaseRepository, pullRequestNumber])` so addition and removal agree across surfaces.

```json
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
{
  "method": "initialize",
  "id": 0,
  "params": {
    "clientInfo": {
      "name": "codex_vscode",
      "title": "Codex VS Code Extension",
      "version": "0.1.0"
    }
  }
}
```

Example with notification opt-out:

```json
{
  "method": "initialize",
  "id": 1,
  "params": {
    "clientInfo": {
      "name": "my_client",
      "title": "My Client",
      "version": "0.1.0"
    },
    "capabilities": {
      "experimentalApi": true,
      "optOutNotificationMethods": ["thread/started", "item/agentMessage/delta"]
    }
  }
}
```

## API Overview

- `thread/start` — create a new thread; emits `thread/started` (including the current `thread.status`) and auto-subscribes you to turn/item events for that thread. Experimental `historyMode: "paginated"` selects projection-backed durable history. When the request includes a `cwd` and the resolved sandbox is `workspace-write` or full access, app-server also marks that project as trusted in the user `config.toml`. Pass `sessionStartSource: "clear"` when starting a replacement thread after clearing the current session so `SessionStart` hooks receive `source: "clear"` instead of the default `"startup"`. Experimental `allowProviderModelFallback` lets providers backed by an authoritative static model catalog replace an unavailable requested `model` with the catalog default; dynamic or cached catalogs preserve the requested model. Experimental `runtimeWorkspaceRoots` supplies the runtime workspace roots used when app-server creates default environment selections; paths must be absolute. For permissions, prefer experimental `permissions` profile selection by id; the legacy `sandbox` shorthand is still accepted but cannot be combined with `permissions`. Deprecated experimental `multiAgentMode` is ignored; use Ultra reasoning effort for proactive multi-agent behavior. Experimental `environments` selects the sticky execution environments for turns on the thread; omit it to use the server default, pass `[]` to disable environments, or pass explicit environment ids with per-environment `cwd` and optional environment-native `runtimeWorkspaceRoots`. Explicit environments ignore the top-level roots; omitted per-environment roots default to that environment's `cwd`, while an empty list explicitly selects no roots. Experimental `selectedCapabilityRoots` selects environment-owned plugin or standalone-skill roots using environment-native absolute paths. Skills found below those roots are listed and read through the owning environment. Stdio MCP servers declared by selected plugins are started in that environment, and HTTP MCP connections use that environment's HTTP client.
- `thread/resume` — reopen an existing thread by id so subsequent `turn/start` calls append to it. Accepts the same permission override rules as `thread/start`.
- `thread/fork` — fork an existing thread into a new thread id by copying the stored history; pass an optional `lastTurnId` to copy history only through that turn, inclusive, and drop later turns from the fork. An in-progress `lastTurnId` boundary is rejected. Experimental `beforeTurnId` instead copies history strictly before the referenced turn, including when that turn is in progress, and cannot be combined with `lastTurnId`. If both boundaries are null while the source thread is mid-turn, the fork records the same interruption marker as `turn/interrupt` instead of inheriting an unmarked partial turn suffix. The returned `thread.forkedFromId` points at the source thread when known. Accepts `ephemeral: true` for an in-memory temporary fork, emits `thread/started` (including the current `thread.status`), and auto-subscribes you to turn/item events for the new thread. Experimental clients can pass `excludeTurns: true` when they plan to page fork history via `thread/turns/list` instead of receiving the full turn array immediately, or `deferGoalContinuation: true` to carry the source thread's current goal into the fork and run an explicit turn before automatic continuation resumes. Deferred goal continuation is persisted until that turn starts and cannot be combined with `ephemeral: true`. Accepts the same permission override rules as `thread/start`.
- `thread/start`, `thread/resume`, and `thread/fork` responses include the legacy `sandbox` compatibility projection. `instructionSources` lists loaded instruction files using each source environment's native absolute path syntax, including files loaded from remote environments. Experimental clients can read `runtimeWorkspaceRoots` for the thread-scoped runtime roots and `activePermissionProfile` for the named or implicit built-in profile identity/provenance when known. Their deprecated experimental `multiAgentMode` field, and the corresponding thread setting, always report `explicitRequestOnly`; Ultra reasoning effort is the source of proactive multi-agent behavior.
- `thread/list` — page through stored threads; supports cursor-based pagination and optional `modelProviders`, `sourceKinds`, `archived`, `isPinned`, `cwd`, and `searchTerm` filters. Experimental clients can use `parentThreadId` for direct spawned children or `ancestorThreadId` for spawned descendants at any depth; the two filters are mutually exclusive. Relationship-filtered responses may include `relationLimitReached` when the bounded persisted descendant subset was reached; in that case, `nextCursor` only exhausts the retained subset and is not proof of exhaustive traversal. Review and Guardian threads are not included because they do not participate in that spawn-edge lifecycle. Each returned `thread` includes `status` (`ThreadStatus`), defaulting to `notLoaded` when the thread is not currently loaded. Subagent threads also include `parentThreadId` when the immediate parent is known.
- `thread/loaded/list` — list the thread ids currently loaded in memory.
- `thread/read` — read a stored thread by id without resuming it; optionally include turns via `includeTurns`. The returned `thread` includes `status` (`ThreadStatus`), defaulting to `notLoaded` when the thread is not currently loaded. For loaded threads, experimental clients can use `canAcceptDirectInput` to determine whether `turn/start` and `turn/steer` are accepted; unloaded stored threads report `null` when that capability is unavailable.
- `thread/turns/list` — experimental; page through a stored thread’s turn history without resuming it; supports cursor-based pagination with `sortDirection`, `itemsView`, `nextCursor`, and `backwardsCursor`.
- `thread/items/list` — experimental; page through persisted thread items without resuming the thread. Pass `turnId` to restrict results to one turn, or omit it to page items across the thread. The active thread store must support item pagination.
- `thread/searchOccurrences` — experimental; find literal, case-insensitive matches in visible user messages and summary-selected final assistant messages within one paginated thread.
- `thread/metadata/update` — patch stored thread metadata in sqlite; supports updating persisted `gitInfo` fields and `isPinned`, and returns the refreshed `thread`.
- `thread/settings/update` — experimental; queue a partial update to a loaded thread’s next-turn settings without starting a turn or adding transcript items. Omitted fields leave settings unchanged; `serviceTier: null` clears the tier; deprecated `multiAgentMode` is ignored, while Ultra reasoning effort enables proactive multi-agent behavior; `sandboxPolicy` and `permissions` cannot be combined. Returns `{}` when the update is accepted and emits `thread/settings/updated` with the full effective settings only if they actually change. `turn/start` settings overrides emit the same notification when they change the stored settings.
- `thread/memoryMode/set` — experimental; set a thread’s persisted memory eligibility to `"enabled"` or `"disabled"` for either a loaded thread or a stored rollout; returns `{}` on success.
- `memory/reset` — experimental; clear the current `CODEX_HOME/memories` directory and reset persisted memory stage data in sqlite while preserving existing thread memory modes; returns `{}` on success.
- `thread/goal/set` — create or update the single persisted goal for a materialized thread; returns the current goal and emits `thread/goal/updated`.
- `thread/goal/get` — fetch the current persisted goal for a materialized thread; returns `goal: null` when no goal exists.
- `thread/goal/clear` — clear the current persisted goal for a materialized thread; returns whether a goal was removed and emits `thread/goal/cleared` when state changes.
- `thread/goal/updated` — notification emitted whenever a thread goal changes; includes the full current goal.
- `thread/goal/cleared` — notification emitted whenever a thread goal is removed.
- `thread/settings/updated` — experimental notification emitted to subscribed clients when a loaded thread’s effective next-turn settings change; includes `threadId` and the full `threadSettings`.
- `thread/status/changed` — notification emitted when a loaded thread’s status changes (`threadId` + new `status`).
- `thread/archive` — move a thread’s rollout file into the archived directory and attempt to move any spawned descendant thread rollout files; returns `{}` on success and emits `thread/archived` for each archived thread.
- `thread/delete` — hard-delete an active or archived thread and any spawned descendant threads; returns `{}` on success and emits `thread/deleted` for each deleted thread.
- `thread/unsubscribe` — unsubscribe this connection from thread turn/item events. If this was the last subscriber, the server keeps the thread loaded and unloads it only after it has had no subscribers and no thread activity for 30 minutes, runs `SessionEnd` hooks, then emits `thread/closed`.
- `thread/name/set` — set or update a thread’s user-facing name for either a loaded thread or a persisted rollout; returns `{}` on success and emits `thread/name/updated` to initialized, opted-in clients. Thread names are not required to be unique; name lookups resolve to the most recently updated thread.
- `thread/unarchive` — move an archived rollout file back into the sessions directory; returns the restored `thread` on success and emits `thread/unarchived`.
- `thread/compact/start` — trigger conversation history compaction for a thread; returns `{}` immediately while progress streams through standard turn/item notifications.
- `thread/shellCommand` — run a user-initiated `!` shell command against a thread; this runs unsandboxed with full access rather than inheriting the thread sandbox policy. Returns `{}` immediately while progress streams through standard turn/item notifications and any active turn receives the formatted output in its message stream.
- `thread/backgroundTerminals/clean` — terminate all running background terminals for a thread (experimental; requires `capabilities.experimentalApi`); returns `{}` when the cleanup request is accepted.
- `thread/backgroundTerminals/list` — list running background terminals for a loaded thread (experimental; requires `capabilities.experimentalApi`); returns `data` with the running terminal ids.
- `thread/backgroundTerminals/terminate` — terminate one running background terminal by app-server `processId` (experimental; requires `capabilities.experimentalApi`); returns whether a process was terminated.
- `thread/rollback` — deprecated and will be removed soon. Drop the last N turns from the agent’s in-memory context and persist a rollback marker in the rollout so future resumes see the pruned history; returns the updated `thread` (with `turns` populated) on success. Paginated threads do not support rollback.
- `turn/start` — add user input to a thread and begin Codex generation; responds with the initial `turn` object and streams `turn/started`, `item/*`, and `turn/completed` notifications. `clientUserMessageId` is optional; when supplied, the corresponding `userMessage` item echoes it as `clientId`. Experimental `runtimeWorkspaceRoots` supplies the default roots for newly resolved environment selections. Explicit `environments[].runtimeWorkspaceRoots` override that fallback with environment-native absolute paths. Prefer experimental `permissions` profile selection by id for permission overrides; the legacy `sandboxPolicy` field is still accepted but cannot be combined with `permissions`. For `collaborationMode`, `settings.developer_instructions: null` means "use built-in instructions for the selected mode". Deprecated experimental `multiAgentMode` is ignored; Ultra reasoning effort selects proactive behavior.
- `thread/inject_items` — append raw Responses API items to a loaded thread’s model-visible history without starting a user turn; returns `{}` on success.
- `turn/steer` — add user input to an already in-flight regular turn without starting a new turn; returns the active `turnId` that accepted the input. `clientUserMessageId` is optional; when supplied, the corresponding `userMessage` item echoes it as `clientId`. Review and manual compaction turns reject `turn/steer`.
- `turn/interrupt` — request cancellation of an in-flight turn by `(thread_id, turn_id)`; success is an empty `{}` response and the turn finishes with `status: "interrupted"`.
- `thread/realtime/start` — start a thread-scoped realtime session (experimental); pass `outputModality: "text"` or `outputModality: "audio"` to choose model output, optionally pass `model` and `version` to override configured realtime selection for this session only, pass `includeStartupContext: false` to omit Codex's generated startup context, and optionally pass `initialItems` to seed V3 with complete role-bearing text messages at session creation. Version `"v1"` uses legacy Bidi `conversation.handoff.*`, `"v2"` uses the Realtime Voice API, and `"v3"` preserves V1 Codex Voice behavior while using Frameless Bidi `delegation.*`. For V3 automatic Codex text, `codexResponseHandoffMode` accepts `"thinking"` (the default; all output uses channel-less thinking appends), `"commentary"` (all output uses the commentary channel), or `"bemTags"` (the raw BEM envelope selects the API channel: BEM `analysis` and `commentary` use `commentary`, while BEM `final` and unparsable output use `speakable`). The BEM envelope remains in the appended text for the frontend model to interpret. V1 and V2 ignore this setting. V3 handoffs do not prepend the legacy `"Agent Final Message"` label. Pass `clientManagedHandoffs: true` to disable automatic Codex response delivery so only the client's explicit append calls produce handoffs. Pass `codexResponsesAsItems: true` to send automatic Codex responses as realtime conversation items instead, and optionally pass `codexResponseItemPrefix` to prepend experiment instructions to those items. Returns `{}` and streams `thread/realtime/*` notifications. Omit `transport` for the websocket transport, or pass `{ "type": "webrtc", "sdp": "..." }` to create a Bidi WebRTC session from a browser-generated SDP offer; the remote answer SDP is emitted as `thread/realtime/sdp`. Conversation `version: "v2"` requests remain unsupported for WebRTC.
- `thread/realtime/appendAudio` — append an input audio chunk to the active realtime session (experimental); returns `{}`.
- `thread/realtime/appendText` — append text input to the active realtime session with a required `role` of `user`, `developer`, or `assistant` (experimental); returns `{}`. Older clients that omit `role` default to `user`.
- `thread/realtime/appendSpeech` — append text that the realtime model should speak to the user (experimental); returns `{}`.
- `thread/realtime/stop` — stop the active realtime session for the thread (experimental); returns `{}`.
- `review/start` — kick off Codex’s automated reviewer for a thread; responds like `turn/start`. Inline reviews emit `item/started`/`item/completed` notifications with `enteredReviewMode` and `exitedReviewMode` items, plus a final assistant `agentMessage` containing the review. Detached reviews stream ordinary turn items on the new review thread.
- `command/exec` — run a single command under the server sandbox without starting a thread/turn (handy for utilities and validation).
- `command/exec/write` — write base64-decoded stdin bytes to a running `command/exec` session or close stdin; returns `{}`.
- `command/exec/resize` — resize a running PTY-backed `command/exec` session by `processId`; returns `{}`.
- `command/exec/terminate` — terminate a running `command/exec` session by `processId`; returns `{}`.
- `command/exec/outputDelta` — notification emitted for base64-encoded stdout/stderr chunks from a streaming `command/exec` session.
- `process/spawn` — experimental; spawn a standalone process without the Codex sandbox on the host where the app server is running; returns after the process starts and emits `process/outputDelta` and `process/exited` notifications.
- `process/writeStdin` — experimental; write base64-decoded stdin bytes to a running `process/spawn` session or close stdin; returns `{}`.
- `process/resizePty` — experimental; resize a running PTY-backed `process/spawn` session by `processHandle`; returns `{}`.
- `process/kill` — experimental; terminate a running `process/spawn` session by `processHandle`; returns `{}`.
- `process/outputDelta` — experimental; notification emitted for base64-encoded stdout/stderr chunks from a streaming `process/spawn` session.
- `process/exited` — experimental; notification emitted when a `process/spawn` session exits.
- `fs/readFile` — read an absolute file path and return `{ dataBase64 }`.
- `fs/writeFile` — write an absolute file path from base64-encoded `{ dataBase64 }`; returns `{}`.
- `fs/createDirectory` — create an absolute directory path; `recursive` defaults to `true`.
- `fs/getMetadata` — return metadata for an absolute path: `isDirectory`, `isFile`, `isSymlink`, `createdAtMs`, and `modifiedAtMs`.
- `fs/readDirectory` — list direct child entries for an absolute directory path; each entry contains `fileName`, `isDirectory`, and `isFile`, and `fileName` is just the child name, not a path.
- `fs/remove` — remove an absolute file or directory tree; `recursive` and `force` default to `true`.
- `fs/copy` — copy between absolute paths; directory copies require `recursive: true`.
- `fs/watch` — subscribe this connection to filesystem change notifications for an absolute file or directory path and caller-provided `watchId`; returns the canonicalized `path`.
- `fs/unwatch` — stop sending notifications for a prior `fs/watch`; returns `{}`.
- `fs/changed` — notification emitted when watched paths change, including the `watchId` and `changedPaths`.
- `model/list` — list available models (set `includeHidden: true` to include entries with `hidden: true`), with model-advertised string reasoning effort options in the catalog's intended progression order, `additionalSpeedTiers`, `serviceTiers`, optional `defaultServiceTier`, optional legacy `upgrade` model ids, optional `upgradeInfo` metadata (`model`, `upgradeCopy`, `modelLink`, `migrationMarkdown`), and optional `availabilityNux` metadata. Clients should preserve the `supportedReasoningEfforts` array order rather than deriving order from the effort names.
- `modelProvider/capabilities/read` — read provider-level capabilities for the currently configured model provider.
- `experimentalFeature/list` — list feature flags with stage metadata (`beta`, `underDevelopment`, `stable`, etc.), enabled/default-enabled state, and cursor pagination. Pass `threadId` when showing feature state for an existing loaded thread so `enabled` is computed from that thread's refreshed config, including project-local config for the thread's cwd; if omitted, the server uses its default config resolution context. For non-beta flags, `displayName`/`description`/`announcement` are `null`.
- `permissionProfile/list` — beta; list available permission profile ids with optional display `description` text and an `allowed` flag reflecting effective requirements, using cursor pagination. Pass `cwd` when the caller needs project-local `[permissions.<id>]` entries to be included in the current catalog view.
- `experimentalFeature/enablement/set` — patch the in-memory process-wide runtime feature enablement for currently supported feature keys. For each feature, precedence is: cloud requirements > --enable <feature_name> > config.toml > experimentalFeature/enablement/set (new) > code default. Invalid keys will be ignored.
- `environment/add` — experimental; add or replace a named remote environment by `environmentId` and `execServerUrl` for later selection by `thread/start` or `turn/start`; optional `connectTimeoutMs` overrides the WebSocket connection timeout; returns `{}` and does not change the default environment.
- `environment/info` — experimental; connect to a configured environment by `environmentId` and return its detected `shell` plus its default `cwd` as a canonical environment-native `file:` URI. Connection failures are returned as request errors.
- `environment/status` — experimental; read the current status for one configured `environmentId`. Ready remote environments are probed over their existing exec-server connection without starting or reconnecting environments; the response reports `ready`, `pending`, `disconnected`, or `unknown`.
- `thread/environment/connected` and `thread/environment/disconnected` — experimental; report exec-server connection transitions observed after thread startup for selected environments. Current connection state is not replayed.
- `collaborationMode/list` — list available collaboration mode presets (experimental, no pagination). Built-in presets do not select a model; the Plan preset selects medium reasoning effort. This response omits built-in developer instructions; clients should either pass `settings.developer_instructions: null` when setting a mode to use Codex's built-in instructions, or provide their own instructions explicitly.
- `skills/list` — list skills for one or more `cwd` values (optional `forceReload`).
- `skills/extraRoots/set` — replace the app-server process runtime extra standalone skill roots. The roots are not persisted; missing directories are accepted and simply load no skills.
- `hooks/list` — list discovered hooks for one or more `cwd` values.
- `marketplace/add` — add a remote plugin marketplace from an HTTP(S) Git URL, SSH Git URL, or GitHub `owner/repo` shorthand, then persist it into the user marketplace config. Returns the installed root path plus whether the marketplace was already present.
- `marketplace/remove` — remove a configured marketplace by name from the user marketplace config, and delete its installed marketplace root when one exists.
- `marketplace/upgrade` — upgrade all configured Git plugin marketplaces, or one named marketplace when `marketplaceName` is provided. Returns selected marketplace names, upgraded roots, and per-marketplace errors.
- `plugin/list` — list discovered plugin marketplaces and plugin state, including effective marketplace install/auth policy metadata, nullable remote install-policy provenance in `installPolicySource` (`WORKSPACE_SETTING` or `IMPLICIT_CANONICAL_APP`), the remote marketplace `version` and locally materialized `localVersion` when available, plugin `availability` (`AVAILABLE` by default or `DISABLED_BY_ADMIN` for remote plugins blocked upstream), fail-open `marketplaceLoadErrors` entries for marketplace files that could not be parsed or loaded, and best-effort `featuredPluginIds` for the official curated marketplace. Every `PluginSummary` returned by plugin list, installed, read, and share-list methods includes `mustShowInstallationInterstitial`: remote service values preserve `true` or `false`, while local plugins and remote responses that omit the policy return `null`. Clients should fail closed when the value is `null`. Clients can explicitly request the remote `workspace-directory`, `shared-with-me`, or `created-by-me-remote` marketplace kinds. Set `forceRefetch: true` to bypass TTL-backed remote catalog caches for the requested marketplaces and wait for fresh data; cache entries are replaced only after a successful fetch. When local marketplaces are included, the request also waits for configured plugin caches to reconcile before marketplace summaries are returned. At app-server startup, existing cached catalogs remain available to `plugin/list` while they refresh in the background. `interface.category` uses the marketplace category when present; otherwise it falls back to the plugin manifest category (**under development; do not call from production clients yet**).
- `plugin/installed` — list installed plugin rows plus any explicitly requested local install-suggestion plugin names, without fetching the broader remote catalog. Remote rows include nullable `installPolicySource`; local rows return `null`. Mention surfaces can use this narrower view when they need plugin mention payloads rather than plugin-page discovery data (**under development; do not call from production clients yet**).
- `plugin/read` — read one plugin by `marketplacePath` plus `pluginName`, returning marketplace info, a list-style `summary`, manifest descriptions/interface metadata, and bundled skills/hooks/apps/MCP server names. Remote plugin details can include scheduled task summaries from the catalog; `scheduledTasks: null` means the metadata is unavailable, while an empty array means the catalog found no scheduled tasks. Remote plugin details expose the canonical `shareUrl` supplied by the remote catalog when available; it is `null` for local plugins or when the catalog omits it. This field is separate from `summary.shareContext`, which continues to describe user and workspace sharing state. For owned workspace plugins, `summary.shareContext.canPublishToWorkspace` reports whether the current user may add the plugin to the workspace directory; `plugin/share/save` returns the same capability after creating or updating a share, and clients should fail closed when either value is `null`. Remote skill interfaces expose `iconSmallUrl` and `iconLargeUrl` when the catalog supplies icon URLs. Returned plugin skills include their current `enabled` state after local config filtering; bundled hooks are returned as lightweight declaration summaries keyed for correlation with `hooks/list`. Use `plugin/install`'s `appsNeedingAuth` to drive post-install authentication and `app/list`'s `isAccessible` to determine current connector accessibility (**under development; do not call from production clients yet**).
- `plugin/skill/read` — read remote plugin skill markdown on demand by `remoteMarketplaceName`, `remotePluginId`, and `skillName`. This lets clients preview uninstalled remote plugin skills without downloading the plugin bundle.
- `skills/changed` — notification emitted when watched local skill files change.
- `app/installed` — read installed connector runtime state from the last committed snapshot, optionally refreshing it first.
- `app/list` — list available apps.
- `remoteControl/enable` — experimental; enable remote control for the current app-server process and return the current remote-control status snapshot. By default, any missing enrollment is completed before the response and the preference is persisted for the current app-server client scope. Pass `ephemeral: true` to enable remote control only for the current process without changing the persisted preference.
- `remoteControl/disable` — experimental; disable remote control for the current app-server process and return the current remote-control status snapshot. By default, the disabled preference is persisted for the current app-server client scope. Pass `ephemeral: true` to disable only for the current process without changing the persisted preference. This does not revoke already enrolled controller devices.
- `remoteControl/status/read` — experimental; read the current remote-control status snapshot. `status` is one of `disabled`, `connecting`, `connected`, or `errored`; `serverName` is the local machine name used by this app-server process; `environmentId` is a string when the app-server has a current enrollment and `null` when that enrollment is cleared, invalidated, or remote control is disabled.
- `remoteControl/pairing/start` — experimental; start a short-lived remote-control pairing artifact for the current app-server process. Pass `manualCode: true` to also request a manual pairing code. Returns `pairingCode`, `manualPairingCode`, `environmentId`, and Unix-seconds `expiresAt`; app-server intentionally does not expose the backend `serverId`.
- `remoteControl/pairing/status` — experimental; poll whether a remote-control `pairingCode` or `manualPairingCode` has been claimed. Pass exactly one of the two fields. Returns `claimed`.
- `remoteControl/client/list` — experimental; list controller devices granted access to an environment. Pass `environmentId` and optional `cursor`, `limit`, and `order`; returns picker-oriented client metadata plus `nextCursor`. This signed-in account-management operation works while the local relay is disabled or unenrolled.
- `remoteControl/client/revoke` — experimental; revoke one controller device's grant for an environment. Pass `environmentId` and `clientId`; returns an empty object. This signed-in account-management operation works while the local relay is disabled or unenrolled.
- `remoteControl/status/changed` — notification emitted when the remote-control status or client-visible environment id changes. `status` is one of `disabled`, `connecting`, `connected`, or `errored`; `serverName` is the local machine name used by this app-server process; `environmentId` is a string when the app-server has a current enrollment and `null` when that enrollment is cleared, invalidated, or remote control is disabled. Newly initialized app-server clients always receive the current status snapshot.
- `skills/config/write` — write user-level skill config by name or absolute path.
- `plugin/install` — install a plugin from a discovered marketplace entry, rejecting marketplace entries marked unavailable for install, install MCPs if any, and return the effective plugin auth policy plus any apps that still need auth (**under development; do not call from production clients yet**).
- `plugin/uninstall` — uninstall a local plugin by `pluginId` in `<plugin>@<marketplace>` form by removing its cached files and clearing its user-level config entry, or uninstall a remote ChatGPT plugin by backend `pluginId` by forwarding the uninstall to the ChatGPT plugin backend and removing any downloaded remote-plugin cache (**under development; do not call from production clients yet**).
- `mcpServer/oauth/login` — start an OAuth login for a configured MCP server; pass `threadId` to resolve servers from that thread's selected plugins and executor, and receive an `authorization_url` followed by `mcpServer/oauthLogin/completed` once the browser flow finishes.
- `tool/requestUserInput` — prompt the user with 1–3 short questions for a tool call and return their answers (experimental).
- `config/mcpServer/reload` — reload MCP server config from disk and queue a refresh for loaded threads (applied on each thread's next active turn); returns `{}`. Use this after editing `config.toml` without restarting the server.
- `mcpServerStatus/list` — enumerate configured MCP servers with their tools, auth status, server info, plus resources/resource templates for `full` detail; supports optional `threadId` and cursor+limit pagination. If `threadId` is omitted, the server reads from the latest global config directly. If `detail` is omitted, the server defaults to `full`.
- `mcpServer/resource/read` — read a resource from a configured MCP server by optional `threadId`, `server`, and `uri`, returning text/blob resource `contents`. If `threadId` is omitted, the server reads from the latest MCP config directly.
- `mcpServer/tool/call` — call a tool on a thread's configured MCP server by `threadId`, `server`, `tool`, optional `arguments`, and optional `_meta`, returning the MCP tool result.
- `windowsSandbox/setupStart` — start Windows sandbox setup for the selected mode (`elevated` or `unelevated`); accepts an optional absolute `cwd` to target setup for a specific workspace, returns `{ started: true }` immediately, and later emits `windowsSandbox/setupCompleted`.
- `feedback/upload` — submit a feedback report (classification + optional reason/logs, conversation_id, and optional `extraLogFiles` attachments array); returns the tracking thread id.
- `config/read` — fetch the runtime-effective config after resolving config layering and managed requirements, including opaque `desktop` values stored in `config.toml`.
- `externalAgentConfig/detect` — detect migratable external-agent artifacts with `includeHome`, optional `cwds`, and an optional `migrationSource` selector. Omitted, `null`, or unrecognized migration-source values retain the default behavior. The deprecated optional `source` field remains accepted for compatibility but does not select the migration source. Each detected item includes `cwd` (`null` for home), and multi-item migrations may additionally include structured `details` with plugin ids, skill names, memory, session metadata, or other artifact names.
- `externalAgentConfig/import` — apply selected external-agent migration items by passing explicit `migrationItems` with `cwd` (`null` for home) and any `details` returned by detect. Pass the same optional `migrationSource` used for detection so the server reads from the matching source; omitted, `null`, or unrecognized values retain the default behavior. The optional `source` identifies the product that initiated the import, while the optional opaque `providerId` attributes analytics to the provider selected by that product without affecting migration-source selection. The response acknowledges the synchronous import phase with an `importId`. Expected migration failures are reported as per-item failures rather than JSON-RPC errors, so the server still returns that `importId` and emits `externalAgentConfig/import/completed` with the same ID once all synchronous and background work finishes. The completion notification contains type-level `itemTypeResults` with successes and failures, including raw failure messages for the client to report separately.
- `externalAgentConfig/import/readHistories` — read completed import histories and connector candidates detected from successfully imported session histories. Connector candidates include a normalized display `name`, the number of imported sessions that used the connector, and the source metadata field used for detection.
- `config/value/write` — write a single config key/value to the user's config.toml on disk; dotted paths such as `desktop.someKey` use the same generic write surface. Writes that overlap a managed requirement are rejected with `configRequirementReadonly`.
- `config/batchWrite` — apply multiple config edits atomically to the user's config.toml on disk, with optional `reloadUserConfig: true` to hot-reload loaded threads, including multiple `desktop.*` edits. Session-static model, reasoning-effort, Plan-mode reasoning-effort, service-tier, and personality defaults do not reload existing threads.
- `configRequirements/read` — fetch loaded requirements constraints from `requirements.toml` and/or MDM (or `null` if none are configured), including exact managed values (`sqliteHome`, `logDir`, `modelCatalogJson`, `checkForUpdateOnStartup`, `allowLoginShell`, `feedback.enabled`, and `windowsSandboxPrivateDesktop`), allow-lists (`allowedApprovalPolicies`, `allowedSandboxModes`, `allowedWebSearchModes`), the layered permission-profile allow map (`allowedPermissionProfiles`), the managed permission-profile default (`defaultPermissions`), lifecycle hook lockdown (`allowManagedHooksOnly`), remote-control policy (`allowRemoteControl`; `false` force-disables remote control while `true` or `null` preserves existing behavior), computer use policy (`computerUse`), Browser Use policy (`browserUse.disableAutoReview`), pinned feature values (`featureRequirements`), managed lifecycle hooks (`hooks`, including each command handler's optional `additionalContextLimit`), `enforceResidency`, managed new-thread defaults (`models.newThread.model`, `models.newThread.modelReasoningEffort`, and `models.newThread.serviceTier`), and `network` constraints such as canonical domain/socket permissions plus `managedAllowedDomainsOnly` and `dangerFullAccessDenylistOnly`.

### Example: Start or resume a thread

Start a fresh thread when you need a new Codex conversation.

```json
{ "method": "thread/start", "id": 10, "params": {
    // Optionally set config settings. If not specified, will use the user's
    // current config settings.
    "model": "gpt-5.1-codex",
    "cwd": "/Users/me/project",
    "approvalPolicy": "never",
    "sandbox": "workspaceWrite",
    // Prefer experimental profile selection:
    // "permissions": ":workspace"
    // Experimental runtime roots for :workspace_roots materialization:
    // "runtimeWorkspaceRoots": ["/Users/me/project", "/Users/me/openai"],
    // Experimental capability roots selected by the hosting platform:
    "selectedCapabilityRoots": [
        {
            "id": "github@openai",
            "location": {
                "type": "environment",
                "environmentId": "workspace",
                "path": "/opt/cca/plugins/github"
            }
        }
    ],
    // Do not send both "sandbox" and "permissions".
    "personality": "friendly",
    "serviceName": "my_app_server_client", // optional metrics tag (`service_name`)
    "sessionStartSource": "startup", // optional: "startup" (default) or "clear"
    // Experimental: requires opt-in
    "dynamicTools": [
        {
            "namespace": "tickets",
            "name": "lookup_ticket",
            "description": "Fetch a ticket by id",
            "deferLoading": true,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" }
                },
                "required": ["id"]
            }
        }
    ],
} }
{ "id": 10, "result": {
    "thread": {
        "id": "thr_123",
        "preview": "",
        "modelProvider": "openai",
        "createdAt": 1730910000
    }
} }
{ "method": "thread/started", "params": { "thread": { … } } }
```

Valid `personality` values are `"friendly"`, `"pragmatic"`, and `"none"`. When `"none"` is selected, the personality placeholder is replaced with an empty string.

To continue a stored session, call `thread/resume` with the `thread.id` you previously recorded. The response shape matches `thread/start`. When the stored session includes persisted token usage, the server emits `thread/tokenUsage/updated` immediately after the response so clients can render restored usage before the next turn starts. You can also pass the same configuration overrides supported by `thread/start`, including `approvalsReviewer`.

By default, `thread/resume` includes the reconstructed turn history in `thread.turns`. Experimental clients can pass `excludeTurns: true` to return only thread metadata and live resume state, then call `thread/turns/list` separately if they want to page the turn history over the network. In that mode the server also skips replaying restored `thread/tokenUsage/updated`, which avoids rebuilding turns just to attribute historical usage.

Paginated threads keep the same resume contract as legacy threads. A default resume materializes the full projected history into `thread.turns`; `excludeTurns: true` keeps that array empty and includes `turnsBackwardsCursor` and `itemsBackwardsCursor` for the durable history visible at the resume boundary. Pass each cursor directly to its matching list API with `sortDirection: "desc"`; the first page includes the cursor's head row, while newer records arrive through live notifications. Either cursor is `null` when there is no durable row yet.

Only one app-server process can hold a paginated thread open for writing at a time. If another process already owns the thread, `thread/resume`, `thread/archive`, and `thread/delete` fail with JSON-RPC error `-32600`. Archive and deletion also fail if another process owns any spawned descendant. Read-only requests remain available without resuming the thread.

Experimental clients that want the live resume subscription plus a turns page in one round trip can pass `initialTurnsPage`. It accepts the same `limit`, `sortDirection`, and `itemsView` controls as `thread/turns/list`; omitted controls use its defaults. The response includes `initialTurnsPage` with `nextCursor` and `backwardsCursor` for follow-up pagination.

By default, resume uses the latest persisted `model` and `reasoningEffort` values associated with the thread. Supplying any of `model`, `modelProvider`, `config.model`, or `config.model_reasoning_effort` disables that persisted fallback and uses the explicit overrides plus normal config resolution instead.

Example:

```json
{ "method": "thread/resume", "id": 11, "params": {
=======
{ "method": "thread/attachment/add", "id": 20, "params": {
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    "threadId": "thr_123",
    "attachmentType": "pull_request",
    "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
    "payload": { "url": "https://github.com/openai/codex/pull/123" }
} }
{ "id": 20, "result": {
    "outcome": "created",
    "attachment": {
        "id": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
        "attachmentType": "pull_request",
        "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
        "payload": { "url": "https://github.com/openai/codex/pull/123" },
        "createdAt": 1750000000
    }
} }

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
When `nextCursor` is `null`, you’ve reached the final page.

### Example: List descendant threads

Enable `capabilities.experimentalApi` during initialization, then use `thread/list` with `ancestorThreadId` to page through the bounded persisted spawn-edge descendant subset of a thread. The ancestor itself is excluded, and each result's `parentThreadId` remains its immediate parent. Use `parentThreadId` instead when only direct children are wanted; sending both filters is invalid. Review and Guardian threads are not included because they do not participate in the spawn-edge lifecycle. When `modelProviders` or `sourceKinds` is omitted, relationship-filtered requests include every provider or source kind, respectively. Explicit filters retain the ordinary `thread/list` behavior, including the interactive-only default for an empty `sourceKinds` list.

Relationship-filtered traversal is safety-bounded. When the response contains `relationLimitReached: true`, the server reached its bounded raw descendant subset before applying outer filters; the returned pages and `nextCursor` cover only the retained subset, not a promise of exhaustive graph traversal. Clients must surface or otherwise preserve that signal rather than treating `nextCursor: null` as proof that no additional persisted descendants exist. Exhaustive continuation beyond the safety bound is a separate future capability and is not provided by this API.

```json
{ "method": "thread/list", "id": 21, "params": {
    "ancestorThreadId": "00000000-0000-0000-0000-000000000100",
    "limit": 25
=======
{ "method": "thread/attachment/list", "id": 21, "params": {
    "threadId": "thr_123",
    "limit": 100
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
} }
{ "id": 21, "result": {
    "data": [{
        "id": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
        "attachmentType": "pull_request",
        "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
        "payload": { "url": "https://github.com/openai/codex/pull/123" },
        "createdAt": 1750000000
    }],
    "nextCursor": null
} }

{ "method": "thread/attachment/remove", "id": 22, "params": {
    "threadId": "thr_123",
    "attachmentType": "pull_request",
    "identityKey": "[\"github.com\",\"openai\",\"codex\",123]"
} }
{ "id": 22, "result": {} }

{ "method": "thread/attachment/updated", "params": {
    "threadId": "thr_123",
    "attachmentType": "pull_request",
    "identityKey": "[\"github.com\",\"openai\",\"codex\",123]",
    "attachmentId": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
    "operation": "deleted"
} }
```

`thread/attachment/list` accepts one `threadId` and returns at most 100 attachments per page, ordered by creation time and attachment id. Continue with `nextCursor` and the same `threadId` until the cursor is `null`. Each thread can retain up to 100 attachments. Removing an attachment frees a slot for a new attachment.

A non-ephemeral fork copies the source thread's current attachments, even when forking at an earlier turn. The copies have new attachment IDs and creation timestamps, but retain the same resource identities and payloads. Clients use `forkedFromId` on `thread/started` to detect forks and call `thread/attachment/list` with the new thread ID to load their attachments. Fork copying does not emit per-attachment updates; explicit add/remove operations still do. Copying is awaited before publishing the fork, but is best effort: a copy failure is logged and the conversation fork succeeds without attachments. Membership can then change independently on either thread; the referenced resources themselves are not copied. Resuming a fork does not repeat the copy.

Attachment creation and deletion requests using the same thread ID are serialized across connections. The requesting client receives its response before the compact update is broadcast, and duplicate creates or absent deletes do not emit updates. Deleting the owning thread removes its attachments under the same lifecycle exclusion; queued attachment mutations then report that the thread was not found.

# Thread plugin settings

`thread/settings/update` and `turn/start` accept `disabledPluginIds`, a list of
`PluginSummary.id` values from `plugin/list`, in the
`<plugin-name>@<marketplace-name>` format. A supplied list replaces the selection;
omission or `null` preserves it, and `[]` clears it. Saving this selection does
not yet filter plugin capabilities.

Read the selection from `threadSettings.disabledPluginIds` in
`thread/settings/updated` notifications, or from `disabledPluginIds` in
`thread/start`, `thread/resume`, and `thread/fork` responses. Selections persist
across resume. Forks restore the selection from the history retained at the
requested fork boundary.

# MCP server capabilities

`mcpServerStatus/list` returns `serverCapabilities` for each initialized MCP server
in both `full` and `toolsAndAuthOnly` detail modes, including thread-scoped reads.
This is the server's advertised MCP capabilities object, including its `extensions`
map. It is null when the connection has not initialized successfully; capabilities
are never inferred from tools or copied from a shared catalog cache.

# Thread rollback

`thread/rollback` has been removed from the API, including its request and response
types. Requests use the generic unknown-method rejection path. Use `thread/revert`
for paginated threads instead.

Existing rollouts may contain historical `ThreadRolledBack` events. Their replay
and migration remain supported so resuming, reading, and forking those threads
preserves the surviving history. This disk compatibility does not require restoring
support for new `thread/rollback` requests.

# Selected workspace routing

The experimental `account/read.workspaceRouting` response field returns the selected ChatGPT workspace's `chatgptAccountId`, resolved HTTPS `backendOrigin`, and backend-provided `accountRoutingOverride`. The routing value is `us`, `us_cr`, or the explicit `NO_CONSTRAINT` value. API-only and signed-out accounts return `null` and do not need `accounts/check`.

App-server discovers routing for saved ChatGPT logins at startup and for new logins or workspace switches. After requirements and routing are ready, it sends the existing `account/updated` notification. Newly initialized connections also receive this notification once saved-workspace routing is ready, including when discovery finished before the connection initialized. Clients then reread `configRequirements/read` and `account/read`. Saved ChatGPT credentials without a selected workspace ID retain their account information and return `workspaceRouting: null`; app-server does not guess a workspace from the backend's default account. Discovery failures for a selected workspace, including missing or null fields from older backends, return an `account/read` error. They never produce a successful unrestricted result. A later read retries failed discovery. Logout clears the cached routing, and results from earlier authentication owners are discarded. Token refreshes for the same known user and workspace invalidate cached routing without cancelling discovery or failing sign-in. Configuration is reloaded after discovery; a changed backend, model provider, or required backend rejects the result so the next read discovers against current configuration. Account notifications recheck the auth owner generation after waiting for outbound queue capacity. Superseded sign-in attempts emit a failed `account/login/completed` event instead of silently dropping completion. Notifications remain snapshots: clients reread current account and requirements state rather than treating a queued notification as authorization.

The origin of a required `chatgpt_base_url` must match the discovered origin by scheme, host, and effective port. The base URL's API path is not part of this comparison. Either origin alone is sufficient. If requirements specify no base URL and discovery explicitly returns `NO_CONSTRAINT`, the effective `chatgpt_base_url` supplies the origin, including its existing default. `backendOrigin` is always a resolved origin; `accountRoutingOverride` preserves `NO_CONSTRAINT` when the backend explicitly returns it. Discovering an origin does not change API paths or apply routing headers to requests.

## Windows sandbox implementation selection

`windowsSandbox/setupStart` and `windowsSandbox/readiness` apply only to the
legacy `elevated` and `unelevated` backends. Clients resolve the desired sandbox
implementation from configuration. When it is `mxc`, they skip both methods;
`allowedWindowsSandboxImplementations` can allow `mxc` independently of the
legacy setup modes. Non-Windows hosts report `notConfigured` for the legacy
readiness API.

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
Use `thread/delete` to hard-delete a thread and its spawned descendant threads. Existing rollout files and associated metadata must be removed before the request succeeds; missing rollout files are treated as already deleted.

```json
{ "method": "thread/delete", "id": 23, "params": { "threadId": "thr_b" } }
{ "id": 23, "result": {} }
{ "method": "thread/deleted", "params": { "threadId": "thr_b" } }
```

### Example: Unarchive a thread

Use `thread/unarchive` to move an archived rollout back into the sessions directory.

```json
{ "method": "thread/unarchive", "id": 24, "params": { "threadId": "thr_b" } }
{ "id": 24, "result": { "thread": { "id": "thr_b" } } }
{ "method": "thread/unarchived", "params": { "threadId": "thr_b" } }
```

### Example: Trigger thread compaction

Use `thread/compact/start` to trigger manual history compaction for a thread. The request returns immediately with `{}`.

Progress is emitted as standard `turn/*` and `item/*` notifications on the same `threadId`. Clients should expect a single compaction item:

- `item/started` with `item: { "type": "contextCompaction", ... }`
- `item/completed` with the same `contextCompaction` item id

While compaction is running, the thread is effectively in a turn so clients should surface progress UI based on the notifications.

```json
{ "method": "thread/compact/start", "id": 25, "params": { "threadId": "thr_b" } }
{ "id": 25, "result": {} }
```

### Example: Run a thread shell command

Use `thread/shellCommand` for the TUI `!` workflow. The request returns immediately with `{}`.
This API runs unsandboxed with full access; it does not inherit the thread
sandbox policy.

If the thread already has an active turn, the command runs as an auxiliary action on that turn. In that case, progress is emitted as standard `item/*` notifications on the existing turn and the formatted output is injected into the turn’s message stream:

- `item/started` with `item: { "type": "commandExecution", "source": "userShell", ... }`
- zero or more `item/commandExecution/outputDelta`
- `item/completed` with the same `commandExecution` item id

If the thread does not already have an active turn, the server starts a standalone turn for the shell command. In that case clients should expect:

- `turn/started`
- `item/started` with `item: { "type": "commandExecution", "source": "userShell", ... }`
- zero or more `item/commandExecution/outputDelta`
- `item/completed` with the same `commandExecution` item id
- `turn/completed`

```json
{ "method": "thread/shellCommand", "id": 26, "params": { "threadId": "thr_b", "command": "git status --short" } }
{ "id": 26, "result": {} }
```

### Example: Start a turn (send user input)

Turns attach user input (text, images, or audio) to a thread and trigger Codex generation. The `input` field is a list of discriminated unions:

- `{"type":"text","text":"Explain this diff"}`
- `{"type":"image","url":"data:image/png;base64,…"}`
- `{"type":"localImage","path":"/tmp/screenshot.png"}`
- `{"type":"audio","url":"data:audio/wav;base64,…"}`
- `{"type":"localAudio","path":"/tmp/recording.mp3"}`

The `image` variant accepts inline data URLs. Remote HTTP(S) image URLs are rejected; use a data URL or `localImage` instead.
The `audio` variant accepts data URLs. Other URL schemes are rejected. `localAudio` reads local wav, mp3, m4a, webm, and ogg files and converts them to data URLs before the Responses API request.

You can optionally specify config overrides on the new turn. If specified, these settings become the default for subsequent turns on the same thread. `outputSchema` applies only to the current turn. Experimental `environments` is turn-scoped: omit it to inherit the thread's sticky environments, pass `[]` to run the turn with no environments, or pass explicit environment ids to override the sticky selection for this turn only.

`approvalsReviewer` accepts:

- `"user"` — default. Review approval requests directly in the client.
- `"auto_review"` — route approval requests to a carefully prompted subagent, which gathers relevant context and applies a risk-based decision framework before approving or denying the request. The legacy value `"guardian_subagent"` is still accepted for compatibility.

```json
{ "method": "turn/start", "id": 30, "params": {
    "threadId": "thr_123",
    "clientUserMessageId": "client_msg_123",
    "input": [ { "type": "text", "text": "Run tests" } ],
    // Below are optional config overrides
    "cwd": "/Users/me/project",
    // Experimental: turn-scoped environment selection.
    "environments": [
        { "environmentId": "local", "cwd": "/Users/me/project" }
    ],
    "approvalPolicy": "unlessTrusted",
    "sandboxPolicy": {
        "type": "workspaceWrite",
        "writableRoots": ["/Users/me/project"],
        "networkAccess": true
    },
    // Prefer experimental profile selection:
    // "permissions": ":workspace"
    // Experimental runtime roots for :workspace_roots materialization:
    // "runtimeWorkspaceRoots": ["/Users/me/project", "/Users/me/openai"],
    // Do not send both "sandboxPolicy" and "permissions".
    "model": "gpt-5.1-codex",
    "effort": "medium",
    "summary": "concise",
    "personality": "friendly",
    // Optional JSON Schema to constrain the final assistant message for this turn.
    "outputSchema": {
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"],
        "additionalProperties": false
    }
} }
{ "id": 30, "result": { "turn": {
    "id": "turn_456",
    "status": "inProgress",
    "items": [],
    "error": null
} } }
```

### Example: Start a turn (invoke a skill)

Invoke a skill explicitly by including `$<skill-name>` in the text input and adding a `skill` input item alongside it.

```json
{ "method": "turn/start", "id": 33, "params": {
    "threadId": "thr_123",
    "input": [
        { "type": "text", "text": "$skill-creator Add a new skill for triaging flaky CI and include step-by-step usage." },
        { "type": "skill", "name": "skill-creator", "path": "/Users/me/.codex/skills/skill-creator/SKILL.md" }
    ]
} }
{ "id": 33, "result": { "turn": {
    "id": "turn_457",
    "status": "inProgress",
    "items": [],
    "error": null
} } }
```

### Example: Start a turn (invoke an app)

Invoke an app by including `$<app-slug>` in the text input and adding a `mention` input item with the app id in `app://<connector-id>` form.

```json
{ "method": "turn/start", "id": 34, "params": {
    "threadId": "thr_123",
    "input": [
        { "type": "text", "text": "$demo-app Summarize the latest updates." },
        { "type": "mention", "name": "Demo App", "path": "app://demo-app" }
    ]
} }
{ "id": 34, "result": { "turn": {
    "id": "turn_458",
    "status": "inProgress",
    "items": [],
    "error": null
} } }
```

### Example: Start a turn (invoke a plugin)

Invoke a plugin by including a UI mention token such as `@sample` in the text input and adding a `mention` input item with the exact `plugin://<plugin-name>@<marketplace-name>` path returned by `plugin/installed` or `plugin/list`.

```json
{ "method": "turn/start", "id": 35, "params": {
    "threadId": "thr_123",
    "input": [
        { "type": "text", "text": "@sample Summarize the latest updates." },
        { "type": "mention", "name": "Sample Plugin", "path": "plugin://sample@test" }
    ]
} }
{ "id": 35, "result": { "turn": {
    "id": "turn_459",
    "status": "inProgress",
    "items": [],
    "error": null
} } }
```

### Example: Inject raw history items

Use `thread/inject_items` to append prebuilt Responses API items to a loaded thread’s prompt history without starting a user turn. These items are persisted to the rollout and included in subsequent model requests. Any `input_image` items must use inline data URLs; remote HTTP(S) image URLs are rejected.

```json
{ "method": "thread/inject_items", "id": 36, "params": {
    "threadId": "thr_123",
    "items": [
        {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "Previously computed context." }]
        }
    ]
} }
{ "id": 36, "result": {} }
```

### Example: Start realtime with WebRTC

Use `thread/realtime/start` with `transport.type: "webrtc"` when a browser or webview owns the `RTCPeerConnection` and app-server should create the server-side realtime session. The transport `sdp` must be the offer SDP produced by `RTCPeerConnection.createOffer()`, not a hand-written or minimal SDP string.

The offer should include the media sections the client wants to negotiate. For the standard realtime UI flow, create the audio track/transceiver and the `oai-events` data channel before calling `createOffer()`:

```javascript
const pc = new RTCPeerConnection();

audioElement.autoplay = true;
pc.ontrack = (event) => {
  audioElement.srcObject = event.streams[0];
};

const mediaStream = await navigator.mediaDevices.getUserMedia({ audio: true });
pc.addTrack(mediaStream.getAudioTracks()[0], mediaStream);
pc.createDataChannel("oai-events");

const offer = await pc.createOffer();
await pc.setLocalDescription(offer);
```

Then send `offer.sdp` to app-server. Core uses `experimental_realtime_ws_backend_prompt` for the backend instructions and the thread conversation id as the default Realtime API session identifier. This `realtimeSessionId` value refers to the upstream Realtime API session, not a Codex session/thread-group id. The start response is `{}`; the remote answer SDP arrives later as `thread/realtime/sdp` and should be passed to `setRemoteDescription()`:

```json
{ "method": "thread/realtime/start", "id": 40, "params": {
    "threadId": "thr_123",
    "outputModality": "audio",
    "prompt": "You are on a call.",
    "realtimeSessionId": null,
    "transport": { "type": "webrtc", "sdp": "v=0\r\no=..." }
} }
{ "id": 40, "result": {} }
{ "method": "thread/realtime/sdp", "params": {
    "threadId": "thr_123",
    "sdp": "v=0\r\no=..."
} }
```

Omit `prompt` to use Codex's default realtime backend prompt. Send `prompt: null` or
`prompt: ""` when the session should start without that default backend prompt.
Clients may also pass `model` on `thread/realtime/start` to select a
different realtime session configuration without changing thread or user config.
Clients may pass `version` to select the realtime protocol for this session
only. WebRTC uses AVAS and supports legacy Bidi `"v1"` or Frameless Bidi
`"v3"`; Realtime Voice `"v2"` is rejected for WebRTC.
Pass `includeStartupContext: false` to skip Codex's startup context for this
session while still using the selected backend prompt.
For V3, clients may pass `initialItems` to seed the session with complete text
messages before live input begins:

```json
{
  "initialItems": [
    {
      "role": "developer",
      "text": "Relevant user memory: prefers concise technical answers."
    },
    {
      "role": "user",
      "text": "Continue from the prior discussion."
    }
  ]
}
```

Each item requires a `role` of `"user"`, `"developer"`, or `"assistant"` and a
`text` string. Core serializes these as Frameless Bidi `session.initial_items`
during the initial session bootstrap (including WebRTC call creation).
Requests are limited to 128 items, 8,192 estimated text tokens per item, and
8,192 estimated text tokens across all items.
Omitting `initialItems`, or passing an empty list, preserves the previous
session payload and startup behavior. V1 and V2 reject non-empty
`initialItems`.
Pass `clientManagedHandoffs: true` to suppress automatic Codex response handoffs
and items. The client can then choose which updates to deliver with
`thread/realtime/appendText` or `thread/realtime/appendSpeech`.
Pass `codexResponsesAsItems: true` to inject automatic Codex responses with
`conversation.item.create` instead of the protocol's default speakable output
path. When using that mode, `codexResponseItemPrefix` can prepend short
experiment instructions to each automatic Codex response item. Omit
`codexResponsesAsItems`, or pass `false`, to preserve the default speakable
behavior. In V3, automatic handoffs default to
`codexResponseHandoffMode: "thinking"`, which omits the context append `channel`
for every automatic response. Pass `"commentary"` to route every response to
commentary, or `"bemTags"` to route BEM commentary tags to `commentary`, final
tags to `speakable`, and analysis tags to `commentary`. Unparsable BEM output
falls back to `speakable`. BEM routing reads the raw envelope and preserves it
in the appended text for the frontend model. With `"bemTags"`, clients may pass
`codexResponseHandoffChannelPrefixes` to override the accepted prefixes for
individual channels, for example
`{"analysis":["[THINKING]"],"commentary":["[PROGRESS]","[UPDATE]"],"final":["[DONE]"]}`.
Omitted channels keep the hard-coded `[ANALYSIS]`, `[COMMENTARY]`, and `[FINAL]`
defaults. This
setting has no effect on V1 or V2. V3 handoffs never prepend the legacy `"Agent Final Message"` label. Older
clients may continue to send the removed `codexResponseHandoffPrefix` field; the
server ignores unknown request fields.
Call
`thread/realtime/appendText` to append app-provided realtime text items, or
`thread/realtime/appendSpeech` when the app decides a realtime update should be
spoken.

```javascript
await pc.setRemoteDescription({
  type: "answer",
  sdp: notification.params.sdp,
});
```

### Example: Interrupt an active turn

You can cancel a running Turn with `turn/interrupt`.

```json
{ "method": "turn/interrupt", "id": 31, "params": {
    "threadId": "thr_123",
    "turnId": "turn_456"
} }
{ "id": 31, "result": {} }
```

The server requests cancellation of the active turn, then emits a `turn/completed` event with `status: "interrupted"`. This does not terminate background terminals; use `thread/backgroundTerminals/clean` when you explicitly want to stop those shells. Rely on the `turn/completed` event to know when turn interruption has finished.

### Example: Clean background terminals

Use `thread/backgroundTerminals/clean` to terminate all running background terminals associated with a thread. This method is experimental and requires `capabilities.experimentalApi = true`.

```json
{ "method": "thread/backgroundTerminals/clean", "id": 35, "params": {
    "threadId": "thr_123"
} }
{ "id": 35, "result": {} }
```

### Example: List and terminate background terminals

Use `thread/backgroundTerminals/list` to inspect running background terminals associated with a loaded thread. The `backgroundTerminals` segment intentionally follows the existing `thread/backgroundTerminals/clean` method. The returned `processId` is the app-server process id; host OS metadata is nullable. The request accepts the standard `cursor` and `limit` pagination fields. When `nextCursor` is non-null, pass it as `cursor` to fetch the next page.

```json
{ "method": "thread/backgroundTerminals/list", "id": 36, "params": { "threadId": "thr_123" } }
{ "id": 36, "result": { "data": [
    {
        "itemId": "item_456",
        "processId": "42",
        "command": "python3 -m http.server",
        "cwd": "/workspace",
        "osPid": null,
        "cpuPercent": null,
        "rssKb": null
    }
], "nextCursor": null } }
```

Use `thread/backgroundTerminals/terminate` to terminate one running background terminal by that `processId`.

```json
{ "method": "thread/backgroundTerminals/terminate", "id": 37, "params": { "threadId": "thr_123", "processId": "42" } }
{ "id": 37, "result": { "terminated": true } }
```

### Example: Steer an active turn

Use `turn/steer` to append additional user input to the currently active regular turn. This does
not emit `turn/started` and does not accept thread settings overrides.

```json
{ "method": "turn/steer", "id": 32, "params": {
    "threadId": "thr_123",
    "clientUserMessageId": "client_msg_124",
    "input": [ { "type": "text", "text": "Actually focus on failing tests first." } ],
    "expectedTurnId": "turn_456"
} }
{ "id": 32, "result": { "turnId": "turn_456" } }
```

`expectedTurnId` is required. If there is no active turn, `expectedTurnId` does not match the
active turn, or the active turn kind does not accept same-turn steering (for example review or
manual compaction), the request fails with an `invalid request` error.

### Example: Request a code review

Use `review/start` to run Codex’s reviewer on the currently checked-out project. The request takes the thread id plus a `target` describing what should be reviewed:

- `{"type":"uncommittedChanges"}` — staged, unstaged, and untracked files.
- `{"type":"baseBranch","branch":"main"}` — diff against the provided branch’s upstream (see prompt for the exact `git merge-base`/`git diff` instructions Codex will run).
- `{"type":"commit","sha":"abc1234","title":"Optional subject"}` — review a specific commit.
- `{"type":"custom","instructions":"Free-form reviewer instructions"}` — fallback prompt equivalent to the legacy manual review request.
- `delivery` (`"inline"` or `"detached"`, default `"inline"`) — where the review runs:
  - `"inline"`: run the review as a new turn on the existing thread. The response’s `reviewThreadId` equals the original `threadId`, and no new `thread/started` notification is emitted.
  - `"detached"`: fork a new review thread from the parent conversation and run the review there. The response’s `reviewThreadId` is the id of this new review thread, and the server emits a `thread/started` notification for it before streaming review items.

Example request/response:

```json
{ "method": "review/start", "id": 40, "params": {
    "threadId": "thr_123",
    "delivery": "inline",
    "target": { "type": "commit", "sha": "1234567deadbeef", "title": "Polish tui colors" }
} }
{ "id": 40, "result": {
    "turn": {
        "id": "turn_900",
        "status": "inProgress",
        "items": [
            { "type": "userMessage", "id": "turn_900", "content": [ { "type": "text", "text": "Review commit 1234567: Polish tui colors" } ] }
        ],
        "error": null
    },
    "reviewThreadId": "thr_123"
} }
```

For a detached review, use `"delivery": "detached"`. The response is the same shape, but `reviewThreadId` will be the id of the new review thread (different from the original `threadId`). The server also emits a `thread/started` notification for that new thread before streaming the review turn. Internally, this is a normal forked thread and turn whose prompt mentions the bundled `$review-agent` skill, so normal turn steering, tool, permission, and item-stream behavior applies.

Detached review is unsupported when the parent thread is paginated.

For an inline review, Codex streams the usual `turn/started` notification followed by an `item/started`
with an `enteredReviewMode` item so clients can show progress:

```json
{
  "method": "item/started",
  "params": {
    "item": {
      "type": "enteredReviewMode",
      "id": "turn_900",
      "review": "current changes"
    }
  }
}
```

When the reviewer finishes, the server emits `item/started` and `item/completed`
containing an `exitedReviewMode` item with the final review text:

```json
{
  "method": "item/completed",
  "params": {
    "item": {
      "type": "exitedReviewMode",
      "id": "turn_900",
      "review": "Looks solid overall...\n\n- Prefer Stylize helpers — app.rs:10-20\n  ..."
    }
  }
}
```

The `review` string is plain text that already bundles the overall explanation plus a bullet list for each structured finding (matching `ThreadItem::ExitedReviewMode` in the generated schema). Use this notification to render the reviewer output in your client.

### Example: One-off command execution

Run a standalone command (argv vector) in the server’s sandbox without creating a thread or turn:

```json
{ "method": "command/exec", "id": 32, "params": {
    "command": ["ls", "-la"],
    "processId": "ls-1",                           // optional string; required for streaming and ability to terminate the process
    "cwd": "/Users/me/project",                    // optional; defaults to server cwd
    "env": { "FOO": "override" },                  // optional; merges into the server env and overrides matching names
    "size": { "rows": 40, "cols": 120 },           // optional; PTY size in character cells, only valid with tty=true
    "permissionProfile": ":workspace",             // optional profile id; defaults to user config
    "outputBytesCap": 1048576,                     // optional; per-stream capture cap
    "disableOutputCap": false,                     // optional; cannot be combined with outputBytesCap
    "timeoutMs": 10000,                            // optional; ms timeout; defaults to server timeout
    "disableTimeout": false                        // optional; cannot be combined with timeoutMs
} }
{ "id": 32, "result": {
    "exitCode": 0,
    "stdout": "...",
    "stderr": ""
} }
```

- Prefer using `process/spawn` when you want an explicitly unsandboxed process execution API with immediate spawn acknowledgement, handle-based control, output notifications, and an exit notification.
- For clients that are already sandboxed externally, set the legacy `sandboxPolicy` to `{"type":"externalSandbox","networkAccess":"enabled"}` (or omit `networkAccess` to keep it restricted). Codex will not enforce its own sandbox in this mode; it tells the model it has full file-system access and passes the `networkAccess` state through `environment_context`.

Notes:

- Empty `command` arrays are rejected.
- Prefer `permissionProfile` for command permission overrides. It selects an active profile by id (for example `:read-only`, `:workspace`, or a user-defined `[permissions.<id>]` profile) rather than accepting low-level filesystem/network permissions. The legacy `sandboxPolicy` field accepts the same shape used by `turn/start` (e.g., `dangerFullAccess`, `readOnly`, `workspaceWrite` with flags, `externalSandbox` with `networkAccess` `restricted|enabled`), but cannot be combined with `permissionProfile`.
- When `permissionProfile` is supplied, the command `cwd` resolves trusted project-local permission settings, workspace roots, network policy, and Windows sandbox mode.
- `env` merges into the environment produced by the server's shell environment policy. Matching names are overridden; unspecified variables are left intact.
- When omitted, `timeoutMs` falls back to the server default.
- When omitted, `outputBytesCap` falls back to the server default of 1 MiB per stream.
- `disableOutputCap: true` disables stdout/stderr capture truncation for that `command/exec` request. It cannot be combined with `outputBytesCap`.
- `disableTimeout: true` disables the timeout entirely for that `command/exec` request. It cannot be combined with `timeoutMs`.
- `processId` is optional for buffered execution. When omitted, Codex generates an internal id for lifecycle tracking, but `tty`, `streamStdin`, and `streamStdoutStderr` must stay disabled and follow-up `command/exec/write` / `command/exec/terminate` calls are not available for that command.
- `size` is only valid when `tty: true`. It sets the initial PTY size in character cells.
- Buffered Windows sandbox execution accepts `processId` for correlation, but `command/exec/write` and `command/exec/terminate` are still unsupported for those requests.
- Buffered Windows sandbox execution also requires the default output cap; custom `outputBytesCap` and `disableOutputCap` are unsupported there.
- `tty`, `streamStdin`, and `streamStdoutStderr` are optional booleans. Legacy requests that omit them continue to use buffered execution.
- `tty: true` implies PTY mode plus `streamStdin: true` and `streamStdoutStderr: true`.
- `tty` and `streamStdin` do not disable the timeout on their own; omit `timeoutMs` to use the server default timeout, or set `disableTimeout: true` to keep the process alive until exit or explicit termination.
- `outputBytesCap` applies independently to `stdout` and `stderr`, and streamed bytes are not duplicated into the final response.
- The `command/exec` response is deferred until the process exits and is sent only after all `command/exec/outputDelta` notifications for that connection have been emitted.
- `command/exec/outputDelta` notifications are connection-scoped. If the originating connection closes, the server terminates the process.

Streaming stdin/stdout uses base64 so PTY sessions can carry arbitrary bytes:

```json
{ "method": "command/exec", "id": 33, "params": {
    "command": ["bash", "-i"],
    "processId": "bash-1",
    "tty": true,
    "outputBytesCap": 32768
} }
{ "method": "command/exec/outputDelta", "params": {
    "processId": "bash-1",
    "stream": "stdout",
    "deltaBase64": "YmFzaC00LjQkIA==",
    "capReached": false
} }
{ "method": "command/exec/write", "id": 34, "params": {
    "processId": "bash-1",
    "deltaBase64": "cHdkCg=="
} }
{ "id": 34, "result": {} }
{ "method": "command/exec/write", "id": 35, "params": {
    "processId": "bash-1",
    "closeStdin": true
} }
{ "id": 35, "result": {} }
{ "method": "command/exec/resize", "id": 36, "params": {
    "processId": "bash-1",
    "size": { "rows": 48, "cols": 160 }
} }
{ "id": 36, "result": {} }
{ "method": "command/exec/terminate", "id": 37, "params": {
    "processId": "bash-1"
} }
{ "id": 37, "result": {} }
{ "id": 33, "result": {
    "exitCode": 137,
    "stdout": "",
    "stderr": ""
} }
```

- `command/exec/write` accepts either `deltaBase64`, `closeStdin`, or both.
- Clients may supply a connection-scoped string `processId` in `command/exec`; `command/exec/write`, `command/exec/resize`, and `command/exec/terminate` only accept those client-supplied string ids.
- `command/exec/outputDelta.processId` is always the client-supplied string id from the original `command/exec` request.
- `command/exec/outputDelta.stream` is `stdout` or `stderr`. PTY mode multiplexes terminal output through `stdout`.
- `command/exec/outputDelta.capReached` is `true` on the final streamed chunk for a stream when `outputBytesCap` truncates that stream; later output on that stream is dropped.
- `command/exec.params.env` overrides the server-computed environment per key; set a key to `null` to unset an inherited variable.
- `command/exec/resize` is only supported for PTY-backed `command/exec` sessions.

### Example: Process lifecycle execution

Use `process/spawn` to start a standalone argv-based process without the Codex sandbox on the host where the app server is running. The `process/*` API is experimental and requires `initialize.params.capabilities.experimentalApi: true`. The spawn response means the process has started and the `processHandle` is registered; completion is reported later through `process/exited`.

```json
{ "method": "process/spawn", "id": 40, "params": {
    "command": ["cargo", "check"],
    "processHandle": "cargo-check-1",
    "cwd": "/Users/me/project",                    // required absolute path
    "env": { "RUST_LOG": null },                    // optional; override or unset app-server env vars
    "outputBytesCap": 1048576,                     // optional; omit for default, null disables
    "timeoutMs": 10000                             // optional; omit for default, null disables
} }
{ "id": 40, "result": {} }
{ "method": "process/exited", "params": {
    "processHandle": "cargo-check-1",
    "exitCode": 0,
    "stdout": "...",
    "stdoutCapReached": false,
    "stderr": "",
    "stderrCapReached": false
} }
```

For interactive or streaming processes, set `tty: true` or `streamStdoutStderr: true` and route output notifications by `processHandle`:

```json
{ "method": "process/spawn", "id": 41, "params": {
    "command": ["bash", "-i"],
    "processHandle": "bash-1",
    "cwd": "/Users/me/project",
    "tty": true,
    "size": { "rows": 40, "cols": 120 },
    "outputBytesCap": null,
    "timeoutMs": null
} }
{ "id": 41, "result": {} }
{ "method": "process/outputDelta", "params": {
    "processHandle": "bash-1",
    "stream": "stdout",
    "deltaBase64": "YmFzaC00LjQkIA==",
    "capReached": false
} }
{ "method": "process/writeStdin", "id": 42, "params": {
    "processHandle": "bash-1",
    "deltaBase64": "cHdkCg=="
} }
{ "id": 42, "result": {} }
{ "method": "process/resizePty", "id": 43, "params": {
    "processHandle": "bash-1",
    "size": { "rows": 48, "cols": 160 }
} }
{ "id": 43, "result": {} }
{ "method": "process/kill", "id": 44, "params": {
    "processHandle": "bash-1"
} }
{ "id": 44, "result": {} }
{ "method": "process/exited", "params": {
    "processHandle": "bash-1",
    "exitCode": 137,
    "stdout": "",
    "stdoutCapReached": false,
    "stderr": "",
    "stderrCapReached": false
} }
```

- Empty `command` arrays and empty `processHandle` strings are rejected.
- `cwd` is required and must be absolute.
- `process/spawn` is intentionally unsandboxed and does not define sandbox-selection fields such as `sandboxPolicy` or `permissionProfile`.
- Duplicate active `processHandle` values are rejected on the same connection; the same handle can be reused after the prior process exits.
- `tty: true` implies PTY mode plus `streamStdin: true` and `streamStdoutStderr: true`.
- `process/writeStdin` accepts either `deltaBase64`, `closeStdin`, or both.
- When omitted, `timeoutMs` and `outputBytesCap` fall back to server defaults. Set either field to `null` to disable that limit for terminal-style sessions.
- `outputBytesCap` applies independently to `stdout` and `stderr`; `process/exited.stdoutCapReached` and `stderrCapReached` report whether each stream reached the cap. Streamed bytes are not duplicated into `process/exited`.
- `process/outputDelta` and `process/exited` notifications are connection-scoped. If the originating connection closes, the server terminates the process.

### Example: Filesystem utilities

These methods operate on absolute paths on the host filesystem and cover reading, writing, directory traversal, copying, removal, and change notifications.

All filesystem paths in this section must be absolute.

```json
{ "method": "fs/createDirectory", "id": 40, "params": {
    "path": "/tmp/example/nested",
    "recursive": true
} }
{ "id": 40, "result": {} }
{ "method": "fs/writeFile", "id": 41, "params": {
    "path": "/tmp/example/nested/note.txt",
    "dataBase64": "aGVsbG8="
} }
{ "id": 41, "result": {} }
{ "method": "fs/getMetadata", "id": 42, "params": {
    "path": "/tmp/example/nested/note.txt"
} }
{ "id": 42, "result": {
    "isDirectory": false,
    "isFile": true,
    "isSymlink": false,
    "createdAtMs": 1730910000000,
    "modifiedAtMs": 1730910000000
} }
{ "method": "fs/readFile", "id": 43, "params": {
    "path": "/tmp/example/nested/note.txt"
} }
{ "id": 43, "result": {
    "dataBase64": "aGVsbG8="
} }
```

- `fs/getMetadata` returns whether the path resolves to a directory or regular file, whether the path itself is a symlink, plus `createdAtMs` and `modifiedAtMs` in Unix milliseconds. If a timestamp is unavailable on the current platform, that field is `0`.
- `fs/createDirectory` defaults `recursive` to `true` when omitted.
- `fs/remove` defaults both `recursive` and `force` to `true` when omitted.
- `fs/readFile` always returns base64 bytes via `dataBase64`, and `fs/writeFile` always expects base64 bytes in `dataBase64`.
- `fs/copy` handles both file copies and directory-tree copies; it requires `recursive: true` when `sourcePath` is a directory. Recursive copies traverse regular files, directories, and symlinks; other entry types are skipped.

### Example: Filesystem watch

`fs/watch` accepts absolute file or directory paths. Watching a file emits `fs/changed` for that file path, including updates delivered via replace or rename operations.

```json
{ "method": "fs/watch", "id": 44, "params": {
    "watchId": "0195ec6b-1d6f-7c2e-8c7a-56f2c4a8b9d1",
    "path": "/Users/me/project/.git/HEAD"
} }
{ "id": 44, "result": {
    "path": "/Users/me/project/.git/HEAD"
} }
{ "method": "fs/changed", "params": {
    "watchId": "0195ec6b-1d6f-7c2e-8c7a-56f2c4a8b9d1",
    "changedPaths": ["/Users/me/project/.git/HEAD"]
} }
{ "method": "fs/unwatch", "id": 45, "params": {
    "watchId": "0195ec6b-1d6f-7c2e-8c7a-56f2c4a8b9d1"
} }
{ "id": 45, "result": {} }
```

## Events

Event notifications are the server-initiated event stream for thread lifecycles, turn lifecycles, and the items within them. After you start or resume a thread, keep reading stdout for `thread/started`, `thread/archived`, `thread/unarchived`, `thread/closed`, `turn/*`, and `item/*` notifications.

Thread realtime uses a separate thread-scoped notification surface. `thread/realtime/*` notifications are ephemeral transport events, not `ThreadItem`s, and are not returned by `thread/read`, `thread/resume`, or `thread/fork`.

Recoverable configuration and initialization warnings use the existing `configWarning` notification: `{ summary, details?, path?, range? }`. App-server may emit it during initialization for config parsing and related setup diagnostics, or to the requesting connection during `thread/start` when that thread's exec-policy rules fail to parse.

Generic runtime warnings use the `warning` notification: `{ threadId?, message }`. App-server emits this for non-fatal warnings from the core event stream, including cases where not all enabled skills are included in the model-visible skills list for a session.

### Notification opt-out

Clients can suppress specific notifications per connection by sending exact method names in `initialize.params.capabilities.optOutNotificationMethods`.

- Exact-match only: `item/agentMessage/delta` suppresses only that method.
- Unknown method names are ignored.
- Applies to app-server typed notifications such as `thread/*`, `turn/*`, `item/*`, and `rawResponseItem/*`.
- Does not apply to requests/responses/errors.

Examples:

- Opt out of thread lifecycle notifications: `thread/started`
- Opt out of streamed agent text deltas: `item/agentMessage/delta`

### Fuzzy file search events (experimental)

The fuzzy file search session API emits per-query notifications:

- `fuzzyFileSearch/sessionUpdated` — `{ sessionId, query, files }` with the current matching files for the active query.
- `fuzzyFileSearch/sessionCompleted` — `{ sessionId, query }` once indexing/matching for that query has completed.

### Thread realtime events (experimental)

The thread realtime API emits thread-scoped notifications for session lifecycle and streaming media:

- `thread/realtime/started` — `{ threadId, realtimeSessionId }` once realtime starts for the thread (experimental). `realtimeSessionId` is the upstream Realtime API session identifier, not a Codex session/thread-group id.
- `thread/realtime/itemAdded` — `{ threadId, item }` for raw non-audio realtime items that do not have a dedicated typed app-server notification, including `handoff_request` (experimental). `item` is forwarded as raw JSON while the upstream websocket item schema remains unstable.
- `thread/realtime/transcript/delta` — `{ threadId, role, delta }` for live realtime transcript deltas (experimental).
- `thread/realtime/transcript/done` — `{ threadId, role, text }` when realtime emits the final full text for a transcript part (experimental).
- `thread/realtime/outputAudio/delta` — `{ threadId, audio }` for streamed output audio chunks (experimental). `audio` uses camelCase fields (`data`, `sampleRate`, `numChannels`, `samplesPerChannel`).
- `thread/realtime/error` — `{ threadId, message }` when realtime encounters a transport or backend error (experimental).
- `thread/realtime/closed` — `{ threadId, reason }` when the realtime transport closes (experimental).

Because audio is intentionally separate from `ThreadItem`, clients can opt out of `thread/realtime/outputAudio/delta` independently with `optOutNotificationMethods`.

### Windows sandbox setup events

- `windowsSandbox/setupCompleted` — `{ mode, success, error }` after a `windowsSandbox/setupStart` request finishes.

### MCP server startup events

- `mcpServer/startupStatus/updated` — `{ threadId, name, status, error, failureReason }` when app-server observes an MCP server startup transition. `threadId` identifies the owning thread when startup is thread-scoped and is `null` when startup is app-scoped. `status` is one of `starting`, `ready`, `failed`, or `cancelled`. `error` and `failureReason` are `null` except for `failed`; `failureReason` is `reauthenticationRequired` when stored OAuth credentials have expired and cannot be refreshed, so clients can prompt the user to reconnect the named server.

### Turn events

The app-server streams JSON-RPC notifications while a turn is running. Each turn emits `turn/started` when it begins running and ends with `turn/completed` (final `turn` status). Token usage events stream separately via `thread/tokenUsage/updated`. Clients subscribe to the events they care about, rendering each item incrementally as updates arrive. The per-item lifecycle is always: `item/started` → zero or more item-specific deltas → `item/completed`.

- `turn/started` — `{ turn }` with the turn id, empty `items`, and `status: "inProgress"`.
- `turn/completed` — `{ turn }` where `turn.status` is `completed`, `interrupted`, or `failed`; successful turns include their final agent message when available, and failures carry `{ error: { message, codexErrorInfo?, additionalDetails? } }`.
- `turn/diff/updated` — `{ threadId, turnId, diff }` represents the up-to-date snapshot of the turn-level unified diff, emitted after every FileChange item. `diff` is the latest aggregated unified diff across every file change in the turn. UIs can render this to show the full "what changed" view without stitching individual `fileChange` items.
- `turn/plan/updated` — `{ turnId, explanation?, plan }` whenever the agent shares or changes its plan; each `plan` entry is `{ step, status }` with `status` in `pending`, `inProgress`, or `completed`.
- `rawResponse/completed` — internal-only; when `thread/start.experimentalRawEvents` is enabled, emits `{ threadId, turnId, responseId, usage }` once for each upstream Responses API completion. `usage` is the exact upstream usage payload mapped to the app-server token breakdown shape and is `null` when the upstream completion omitted usage. Unlike `thread/tokenUsage/updated`, this notification is not accumulated, estimated, persisted, or replayed.
- `model/safetyBuffering/updated` — `{ threadId, turnId, model, useCases, reasons, showBufferingUi, fasterModel }` when a response enters safety buffering. `fasterModel` is nullable. This notification is transient and is not persisted in rollout history.
- `model/rerouted` — `{ threadId, turnId, fromModel, toModel, reason }` when the backend reroutes a request to a different model (for example, due to high-risk cyber safety checks).
- `model/verification` — `{ threadId, turnId, verifications }` when the backend flags additional account verification, such as `trustedAccessForCyber`.
- `turn/moderationMetadata` — experimental; `{ threadId, turnId, metadata }` when a first-party backend supplies turn-scoped moderation metadata for client-side presentation.

`turn/started` carries no items. `turn/completed` carries only the final agent message as a summary fallback; continue consuming `item/*` notifications for the full canonical item list.

#### Items

`ThreadItem` is the tagged union carried in turn responses and `item/*` notifications. Currently we support events for the following items:

- `userMessage` — `{id, clientId, content}` where `clientId` is the optional `clientUserMessageId` supplied to `turn/start` or `turn/steer`, and `content` is a list of user inputs (`text`, `image`, `localImage`, `audio`, or `localAudio`).
- `agentMessage` — `{id, text}` containing the accumulated agent reply.
- `plan` — `{id, text}` emitted for plan-mode turns; plan text can stream via `item/plan/delta` (experimental).
- `reasoning` — `{id, summary, content}` where `summary` holds streamed reasoning summaries (applicable for most OpenAI models) and `content` holds raw reasoning blocks (applicable for e.g. open source models).
- `commandExecution` — `{id, pluginId?, scriptPath?, command, cwd, status, commandActions, aggregatedOutput?, exitCode?, durationMs?}` for sandboxed commands; `pluginId` is present only for commands attributed to a trusted first-party plugin, newly attributed items also include `scriptPath` as a safe `/`-separated path relative to the trusted plugin root, older history may omit `scriptPath`, and `status` is `inProgress`, `completed`, `failed`, or `declined`.
- `fileChange` — `{id, changes, status}` describing proposed edits; `changes` list `{path, kind, diff}` and `status` is `inProgress`, `completed`, `failed`, or `declined`.
- `mcpToolCall` — `{id, server, tool, status, arguments, appContext, mcpAppResourceUri?, pluginId, result?, error?}` describing MCP calls; `appContext` is `{connectorId, linkId, resourceUri, appName, actionName}` for calls through a trusted MCP app, where `connectorId` identifies the connector that owns the tool, `linkId` identifies the app link, `resourceUri` points to the widget template, `appName` is the connector's display name, and `actionName` is the stable connector `Action.name`. `appName` and `actionName` may be null for older rollout entries. The top-level `mcpAppResourceUri` is deprecated and temporarily duplicated for client migration. `tool` identifies the raw MCP tool. `status` is `inProgress`, `completed`, or `failed`.
- `collabAgentToolCall` — `{id, tool, status, senderThreadId, receiverThreadIds, prompt, model, reasoningEffort, requestedModel, requestedReasoningEffort, effectiveModel, effectiveReasoningEffort, agentsStates}` describes collaboration tool calls. `tool` is `spawnAgent`, `sendInput`, `resumeAgent`, `wait`, or `closeAgent`; `status` is `inProgress`, `completed`, or `failed`; `receiverThreadIds` lists targets and `agentsStates` maps target ids to `{status, message}`. Agent-state `status` is `pendingInit`, `running`, `interrupted`, `completed`, `errored`, `shutdown`, or `notFound`; `message` is nullable. For a spawn item, the established `model` and `reasoningEffort` aliases mean the request at start and the observed effective identity at terminal completion. `requestedModel`, `requestedReasoningEffort`, `effectiveModel`, and `effectiveReasoningEffort` are additive required nullable fields: request provenance stays in `requested*`, and `effective*` repeats only a terminal observation. Unknown values are serialized as `null`; a terminal item's unknown effective identity must not be inferred from its request or thread metadata. Historic persisted terminal items may lack all four additive fields; their legacy aliases are observed effective identity and are not request provenance.
- `webSearch` — `{id, query, action?, results?}` for a web search request issued by the agent; `action` mirrors the Responses API web_search action payload (`search`, `open_page`, `find_in_page`) and may be omitted until completion. For standalone web search, `results` contains the out-of-band structured result DTOs returned by `/v1/alpha/search`; clients should ignore result types and fields they do not understand.
- `imageView` — `{id, path}` emitted when the agent invokes the image viewer tool.
- `sleep` — `{id, durationMs}` emitted while the agent waits for a duration or new input.
- `enteredReviewMode` — `{id, review}` sent when the reviewer starts; `review` is a short user-facing label such as `"current changes"` or the requested target description.
- `exitedReviewMode` — `{id, review}` emitted when the reviewer finishes; `review` is the full plain-text review (usually, overall notes plus bullet point findings).
- `contextCompaction` — `{id}` emitted when codex compacts the conversation history. This can happen automatically.
- `compacted` - `{threadId, turnId}` when codex compacts the conversation history. This can happen automatically. **Deprecated:** Use `contextCompaction` instead.

All items emit shared lifecycle events:

- `item/started` — emits the full `item` when a new unit of work begins so the UI can render it immediately; the `item.id` in this payload matches the `itemId` used by deltas.
- `item/completed` — sends the final `item` once that work itself finishes (for example, after a tool call or message completes); treat this as the authoritative execution/result state.
- `item/autoApprovalReview/started` — [UNSTABLE] temporary auto-review notification carrying `{threadId, turnId, targetItemId, review, action}` when approval auto-review begins. This shape is expected to change soon.
- `item/autoApprovalReview/completed` — [UNSTABLE] temporary auto-review notification carrying `{threadId, turnId, targetItemId, review, action}` when approval auto-review resolves. This shape is expected to change soon.

`review` is [UNSTABLE] and currently has `{status, riskLevel?, userAuthorization?, rationale?}`, where `status` is one of `inProgress`, `approved`, `denied`, or `aborted`. `riskLevel` is one of `"low"`, `"medium"`, `"high"`, or `"critical"` when present. `userAuthorization` is one of `"unknown"`, `"low"`, `"medium"`, or `"high"` when present. `action` is a tagged union with `type: "command" | "execve" | "applyPatch" | "networkAccess" | "mcpToolCall"`. Command-like actions include a `source` discriminator (`"shell"` or `"unifiedExec"`). These notifications are separate from the target item's own `item/completed` lifecycle and are intentionally temporary while the auto-review app protocol is still being designed.

There are additional item-specific events:

#### agentMessage

- `item/agentMessage/delta` — appends streamed text for the agent message; concatenate `delta` values for the same `itemId` in order to reconstruct the full reply.

#### plan

- `item/plan/delta` — streams proposed plan content for plan items (experimental); concatenate `delta` values for the same plan `itemId`. These deltas correspond to the `<proposed_plan>` block.

#### reasoning

- `item/reasoning/summaryTextDelta` — streams readable reasoning summaries; `summaryIndex` increments when a new summary section opens.
- `item/reasoning/summaryPartAdded` — marks the boundary between reasoning summary sections for an `itemId`; subsequent `summaryTextDelta` entries share the same `summaryIndex`.
- `item/reasoning/textDelta` — streams raw reasoning text (only applicable for e.g. open source models); use `contentIndex` to group deltas that belong together before showing them in the UI.

#### commandExecution

- `item/commandExecution/outputDelta` — streams stdout/stderr for the command; append deltas in order to render live output alongside `aggregatedOutput` in the final item.
  Final `commandExecution` items include parsed `commandActions`, `status`, `exitCode`, and `durationMs` so the UI can summarize what ran and whether it succeeded.

#### fileChange

- `item/fileChange/patchUpdated` - when `features.apply_patch_streaming_events` is enabled, streams structured file-change snapshots parsed from the model-generated patch before it is executed.
- `item/fileChange/outputDelta` - deprecated legacy protocol entry for `apply_patch` text output; retained for compatibility but no longer emitted by the server.

### Errors

`error` event is emitted whenever the server hits an error mid-turn (for example, upstream model errors or quota limits). Carries the same `{ error: { message, codexErrorInfo?, additionalDetails? } }` payload as `turn.status: "failed"` and may precede that terminal notification.

`codexErrorInfo` maps to the `CodexErrorInfo` enum. Common values:

- `ContextWindowExceeded`
- `SessionBudgetExceeded`
- `UsageLimitExceeded`
- `HttpConnectionFailed { httpStatusCode? }`: upstream HTTP failures including 4xx/5xx
- `ResponseStreamConnectionFailed { httpStatusCode? }`: failure to connect to the response SSE stream
- `ResponseStreamDisconnected { httpStatusCode? }`: disconnect of the response SSE stream in the middle of a turn before completion
- `ResponseTooManyFailedAttempts { httpStatusCode? }`
- `ActiveTurnNotSteerable { turnKind }`: `turn/start` or `turn/steer` was submitted while the
  current active turn was not steerable, for example `/review` or manual `/compact`
- `BadRequest`
- `Unauthorized`
- `SandboxError`
- `InternalServerError`
- `Other`: all unclassified errors

When an upstream HTTP status is available (for example, from the Responses API or a provider), it is forwarded in `httpStatusCode` on the relevant `codexErrorInfo` variant.

## Approvals

Certain actions (shell commands or modifying files) may require explicit user approval depending on the user's config. When `turn/start` is used, the app-server drives an approval flow by sending a server-initiated JSON-RPC request to the client. The client must respond to tell Codex whether to proceed. UIs should present these requests inline with the active turn so users can review the proposed command or diff before choosing.

- Requests include `threadId` and `turnId`—use them to scope UI state to the active conversation.
- Respond with a single `{ "decision": ... }` payload. Command approvals support `accept`, `acceptForSession`, `acceptWithExecpolicyAmendment`, `applyNetworkPolicyAmendment`, `decline`, or `cancel`. The server resumes or declines the work and ends the item with `item/completed`.

### Command execution approvals

Order of messages:

1. `item/started` — shows the pending `commandExecution` item with `command`, `cwd`, and other fields so you can render the proposed action.
2. `item/commandExecution/requestApproval` (request) — carries the same `itemId`, `threadId`, `turnId`, the nullable `environmentId` where the command will run, optionally `approvalId` (for subcommand callbacks), and `reason`. New shell and unified-exec approvals set `environmentId`; older events that do not provide one are exposed as `null`. For normal command approvals, the request also includes `command`, `cwd`, and `commandActions` for friendly display. When `initialize.params.capabilities.experimentalApi = true`, it may also include experimental `additionalPermissions` describing requested per-command sandbox access; any filesystem paths in that payload are absolute on the wire, and network access is represented as `additionalPermissions.network.enabled`. For network-only approvals, those command fields may be omitted and `networkApprovalContext` is provided instead. Optional persistence hints may also be included via `proposedExecpolicyAmendment` and `proposedNetworkPolicyAmendments`. Clients can prefer `availableDecisions` when present to render the exact set of choices the server wants to expose, while still falling back to the older heuristics if it is omitted.
3. Client response — for example `{ "decision": "accept" }`, `{ "decision": "acceptForSession" }`, `{ "decision": { "acceptWithExecpolicyAmendment": { "execpolicy_amendment": [...] } } }`, `{ "decision": { "applyNetworkPolicyAmendment": { "network_policy_amendment": { "host": "example.com", "action": "allow" } } } }`, `{ "decision": "decline" }`, or `{ "decision": "cancel" }`.
4. `serverRequest/resolved` — `{ threadId, requestId }` confirms the pending request has been resolved or cleared, including lifecycle cleanup on turn start/complete/interrupt.
5. `item/completed` — final `commandExecution` item with `status: "completed" | "failed" | "declined"` and execution output. Render this as the authoritative result.

### File change approvals

Order of messages:

1. `item/started` — emits a `fileChange` item with `changes` (diff chunk summaries) and `status: "inProgress"`. Show the proposed edits and paths to the user.
2. `item/fileChange/requestApproval` (request) — includes `itemId`, `threadId`, `turnId`, an optional `reason`, and may include unstable `grantRoot` when the agent is asking for session-scoped write access under a specific root.
3. Client response — `{ "decision": "accept" }`, `{ "decision": "acceptForSession" }`, `{ "decision": "decline" }`, or `{ "decision": "cancel" }`.
4. `serverRequest/resolved` — `{ threadId, requestId }` confirms the pending request has been resolved or cleared, including lifecycle cleanup on turn start/complete/interrupt.
5. `item/completed` — returns the same `fileChange` item with `status` updated to `completed`, `failed`, or `declined` after the patch attempt. Rely on this to show success/failure and finalize the diff state in your UI.

UI guidance for IDEs: surface an approval dialog as soon as the request arrives. The turn will proceed after the server receives a response to the approval request. The terminal `item/completed` notification will be sent with the appropriate status.

### request_user_input

`item/tool/requestUserInput` includes required `isBlocking`, which indicates whether the client should wait indefinitely for explicit user input. The older `autoResolutionMs` field is deprecated and retained only for compatibility.

When the client responds to `item/tool/requestUserInput`, the server emits `serverRequest/resolved` with `{ threadId, requestId }`. If the pending request is cleared by turn start, turn completion, or turn interruption before the client answers, the server emits the same notification for that cleanup.

### Attestation generation

Desktop hosts that provide upstream attestation should set `capabilities.requestAttestation` during `initialize` and handle the server-initiated `attestation/generate` request. App-server issues it just in time before ChatGPT Codex requests that forward `x-oai-attestation`; the client responds with `{ "token": "v1.<opaque>" }`, where `token` is an opaque client-owned value. When app-server receives a client response, it forwards a consistent outer envelope such as `{ "v": 1, "s": 0, "t": "v1.<opaque>" }`, where `t` contains the client token unchanged. If app-server attempts attestation but fails within its own boundary, it sends the same envelope shape with an app-server status code and without `t` (`1 = timeout`, `2 = request failed`, `3 = request canceled`, `4 = malformed response`). If no initialized client opted into attestation, app-server omits `x-oai-attestation` for that upstream request.

### Current time

When `[features.current_time_reminder]` is enabled with `clock_source = "external"`, app-server sends the client subscribed to the thread an experimental `currentTime/read` request with `{ "threadId": "thr_123" }` when a time reminder is due. The client responds with `{ "currentTimeAt": 1781717655 }`, where `currentTimeAt` is an integer Unix timestamp in seconds. A failed, canceled, timed-out, or malformed response stops the turn before the model request is sent.

### MCP server elicitations

MCP servers can interrupt a turn and ask the client for structured input via `mcpServer/elicitation/request`.

Order of messages:

1. `mcpServer/elicitation/request` (request) — includes `threadId`, nullable `turnId`, `serverName`, and either:
   - a form request: `{ "mode": "form", "message": "...", "requestedSchema": { ... } }`
   - an OpenAI extended form request: `{ "mode": "openai/form", "message": "...", "requestedSchema": { ... } }`
   - a URL request: `{ "mode": "url", "message": "...", "url": "...", "elicitationId": "..." }`
2. Client response — `{ "action": "accept", "content": ... }`, `{ "action": "decline", "content": null }`, or `{ "action": "cancel", "content": null }`.
3. `serverRequest/resolved` — `{ threadId, requestId }` confirms the pending request has been resolved or cleared, including lifecycle cleanup on turn start/complete/interrupt.

`turnId` is best-effort. When the elicitation is correlated with an active turn, the request includes that turn id; otherwise it is `null`.

For `openai/form`, app-server forwards `requestedSchema` as opaque JSON. The
client owns validation and rendering of supported field types and must return a
valid `decline` or `cancel` response when it cannot render a form.

For MCP tool approval elicitations, form request `meta` includes
`codex_approval_kind: "mcp_tool_call"` and may include `persist: "session"`,
`persist: "always"`, or `persist: ["session", "always"]` to advertise whether
the client can offer session-scoped and/or persistent approval choices.

### Permission requests

The built-in `request_permissions` tool sends an `item/permissions/requestApproval` JSON-RPC request to the client with the requested permission profile. This v2 payload mirrors the command-execution `additionalPermissions` shape: it can request network access and additional filesystem access. The `environmentId` and `cwd` fields identify the environment and directory used to resolve project-root permissions and relative deny globs.

```json
{
  "method": "item/permissions/requestApproval",
  "id": 61,
  "params": {
    "threadId": "thr_123",
    "turnId": "turn_123",
    "itemId": "call_123",
    "environmentId": "local",
    "cwd": "/Users/me/project",
    "reason": "Select a workspace root",
    "permissions": {
      "fileSystem": {
        "write": ["/Users/me/project", "/Users/me/shared"]
      }
    }
  }
}
```

The client responds with `result.permissions`, which should be the granted subset of the requested permission profile. It may also set `result.scope` to `"session"` to make the grant persist for later turns in the same session; omitted or `"turn"` keeps the existing turn-scoped behavior:

```json
{
  "id": 61,
  "result": {
    "scope": "session",
    "permissions": {
      "fileSystem": {
        "write": ["/Users/me/project"]
      }
    }
  }
}
```

Only the granted subset matters on the wire. Any permissions omitted from `result.permissions` are treated as denied. Any permissions not present in the original request are ignored by the server.

Within the same turn, granted permissions are sticky: later shell-like tool calls can automatically reuse the granted subset without reissuing a separate permission request.

If the session approval policy uses `Granular` with `request_permissions: false`, standalone `request_permissions` tool calls are auto-denied and no `item/permissions/requestApproval` prompt is sent. Inline `with_additional_permissions` command requests remain controlled by `sandbox_approval`, and any previously granted permissions remain sticky for later shell-like calls in the same turn.

### Dynamic tool calls (experimental)

`dynamicTools` on `thread/start` and the corresponding `item/tool/call` request/response flow are experimental APIs. To enable them, set `initialize.params.capabilities.experimentalApi = true`.

Each `dynamicTools` entry is a flat function object. Set its optional `namespace`
to group related functions in model-facing tool projection; that grouping does not
change the input representation. Tagged function/namespace input is deferred
until app-server input, persisted state, resume filtering, and provider registries
can migrate without losing legacy entries. Dynamic tool identifiers follow the
same constraints as Responses tools:

- `name` must match `^[a-zA-Z0-9_-]+$` and be between 1 and 128 characters.
- Namespace names must match `^[a-zA-Z0-9_-]+$` and be between 1 and 64 characters.
- Namespace descriptions must be at most 1,024 characters.
- Namespace names must not collide with reserved Responses runtime namespaces such as `functions`, `multi_tool_use`, `file_search`, `web`, `browser`, `image_gen`, `computer`, `container`, `terminal`, `python`, `python_user_visible`, `api_tool`, `tool_search`, `submodel_delegator`, `collaboration`, or `agents`.

Each function may set `deferLoading`. When omitted, it defaults to `false`.
Ordinary deferred functions must belong to a namespace. Bare native computer-use
functions are the deliberate exception: they can defer-load so `tool_search` can
expose their canonical Codex-owned schema. Set it to `true` to keep a function
registered and callable by runtime features such as `code_mode`, while excluding
it from the model-facing tool list sent on ordinary turns. When `tool_search` is
available, deferred dynamic tools are searchable and can be exposed by a matching
search result.

When a dynamic tool is invoked during a turn, the server sends an `item/tool/call` JSON-RPC request to the client:

```json
{
  "method": "item/tool/call",
  "id": 60,
  "params": {
    "threadId": "thr_123",
    "turnId": "turn_123",
    "callId": "call_123",
    "namespace": "tickets",
    "tool": "lookup_ticket",
    "arguments": { "id": "ABC-123" }
  }
}
```

The server also emits item lifecycle notifications around the request:

1. `item/started` with `item.type = "dynamicToolCall"`, `status = "inProgress"`, plus `tool` and `arguments`.
2. `item/tool/call` request.
3. Client response.
4. `item/completed` with `item.type = "dynamicToolCall"`, final `status`, and the returned `contentItems`/`success`.

The client must respond with content items. Use `inputText` for text, `inputImage` for inline image data URLs, and `inputAudio` for inline audio data URLs. Audio data URLs accept wav, mp3, m4a, webm, and ogg media types. Remote HTTP(S) image URLs and non-data audio URLs make the dynamic tool response invalid.

```json
{
  "id": 60,
  "result": {
    "contentItems": [
      { "type": "inputText", "text": "Ticket ABC-123 is open." },
      { "type": "inputImage", "imageUrl": "data:image/png;base64,AAA" },
      { "type": "inputAudio", "audioUrl": "data:audio/wav;base64,AAA" }
    ],
    "success": true
  }
}
```

## Skills

Invoke a skill by including `$<skill-name>` in the text input. Add a `skill` input item (recommended) so the backend injects full skill instructions instead of relying on the model to resolve the name.

```json
{
  "method": "turn/start",
  "id": 101,
  "params": {
    "threadId": "thread-1",
    "input": [
      {
        "type": "text",
        "text": "$skill-creator Add a new skill for triaging flaky CI."
      },
      {
        "type": "skill",
        "name": "skill-creator",
        "path": "/Users/me/.codex/skills/skill-creator/SKILL.md"
      }
    ]
  }
}
```

If you omit the `skill` item, the model will still parse the `$<skill-name>` marker and try to locate the skill, which can add latency.

Example:

```
$skill-creator Add a new skill for triaging flaky CI and include step-by-step usage.
```

Use `skills/list` to fetch the available skills (optionally scoped by `cwds`, with `forceReload`).
`skills/list` might reuse a cached skills result per `cwd`; setting `forceReload` to `true` refreshes the result from disk.
The server also emits `skills/changed` notifications when watched local skill files change. Treat this as an invalidation signal and re-run `skills/list` with your current params when needed.
Use `skills/extraRoots/set` to replace additional standalone skill roots for the current app-server process. These roots use the same layout as other standalone skill roots: each root contains skill directories, and each skill directory contains `SKILL.md`. Missing roots are accepted and load no skills until they exist. This setting is lost when app-server exits.

```json
{ "method": "skills/list", "id": 25, "params": {
    "cwds": ["/Users/me/project", "/Users/me/other-project"],
    "forceReload": true
} }
{ "id": 25, "result": {
    "data": [{
        "cwd": "/Users/me/project",
        "skills": [
            {
              "name": "skill-creator",
              "description": "Create or update a Codex skill",
              "enabled": true,
              "interface": {
                "displayName": "Skill Creator",
                "shortDescription": "Create or update a Codex skill",
                "iconSmall": "icon.svg",
                "iconLarge": "icon-large.svg",
                "brandColor": "#111111",
                "defaultPrompt": "Add a new skill for triaging flaky CI."
              }
            }
        ],
        "errors": []
    }]
} }
```

```json
{
  "method": "skills/changed",
  "params": {}
}
```

```json
{
  "method": "skills/extraRoots/set",
  "id": 26,
  "params": {
    "extraRoots": ["/Users/me/generated-skills"]
  }
}
{ "id": 26, "result": {} }
```

To enable or disable a skill by absolute path:

```json
{
  "method": "skills/config/write",
  "id": 27,
  "params": {
    "path": "/Users/alice/.codex/skills/skill-creator/SKILL.md",
    "name": null,
    "enabled": false
  }
}
```

To enable or disable a skill by name:

```json
{
  "method": "skills/config/write",
  "id": 28,
  "params": {
    "path": null,
    "name": "github:yeet",
    "enabled": false
  }
}
```

Use `hooks/list` to fetch discovered hooks for one or more `cwds`. Each result is evaluated with that `cwd`'s effective config, so feature gates and discovered config layers can differ within a single response.

For linked Git worktrees, project hook declarations come from the matching `.codex/` folders in the root checkout rather than from divergent hook declarations stored only in the linked worktree. This keeps each repo on one authoritative project-hook definition and one trust state.

Hooks are returned even when disabled so clients can render and re-enable them. User-controlled state lives under `hooks.state`. Managed hooks are non-configurable, and user entries for managed hook keys are ignored during loading.

For unmanaged hooks, `currentHash` and `trustStatus` describe whether the current definition is first-seen, approved, or changed since approval. Only trusted unmanaged hooks become runnable. Hook keys combine the source identity with a trailing event/group/handler selector that is currently positional.

```json
{
  "method": "hooks/list",
  "id": 28,
  "params": {
    "cwds": ["/Users/me/project"]
  }
}
```

```json
{
  "id": 28,
  "result": {
    "data": [{
      "cwd": "/Users/me/project",
      "hooks": [{
        "key": "/Users/me/.codex/config.toml:pre_tool_use:0:0",
        "eventName": "pre_tool_use",
        "handlerType": "command",
        "isManaged": false,
        "matcher": "Bash",
        "command": "python3 /Users/me/hook.py",
        "timeoutSec": 5,
        "statusMessage": "running hook",
        "additionalContextLimit": null,
        "sourcePath": "/Users/me/.codex/config.toml",
        "source": "user",
        "pluginId": null,
        "displayOrder": 0,
        "enabled": true,
        "currentHash": "sha256:...",
        "trustStatus": "untrusted"
      }],
      "warnings": [],
      "errors": []
    }]
  }
}
```

To disable a non-managed hook, upsert a state entry at `hooks.state` with `config/batchWrite`:

```json
{
  "method": "config/batchWrite",
  "id": 29,
  "params": {
    "edits": [{
      "keyPath": "hooks.state",
      "value": {
        "/Users/me/.codex/config.toml:pre_tool_use:0:0": {
          "enabled": false
        }
      },
      "mergeStrategy": "upsert"
    }],
    "reloadUserConfig": true
  }
}
```

To re-enable it, upsert the same hook key with `"enabled": true`.
## Apps

Use `app/installed` to read installed apps and whether each app is currently enabled and callable.

```json
{ "method": "app/installed", "id": 49, "params": {
    "threadId": "thr_123",
    "forceRefresh": false
} }
{ "id": 49, "result": {
    "apps": [
        {
            "id": "demo-app",
            "runtimeName": "Demo App",
            "enabled": true,
            "callable": true
        }
    ]
} }
```

`id` is the app's connector ID, and `runtimeName` is the nullable name reported by the runtime. `enabled` reflects effective app configuration and workspace policy. `callable` is true when the app is enabled and has at least one model-visible tool allowed by app and tool policy.

When `threadId` is provided, the response uses that thread's effective configuration; otherwise it uses the current global configuration. `forceRefresh` defaults to `false`. Set it to `true` to refresh the hosted connector runtime tool snapshot before reading the response. When Apps are disabled by global or workspace policy, previously observed apps may still be returned with `enabled` and `callable` set to `false`.

Use `app/list` to fetch available apps (connectors). Each entry includes metadata like the app `id`, display `name`, `installUrl`, legacy logo URLs, structured light and dark icon assets, `branding`, `appMetadata`, `labels`, whether it is currently accessible, and whether it is enabled in config.

```json
{ "method": "app/list", "id": 50, "params": {
    "cursor": null,
    "limit": 50,
    "threadId": "thr_123",
    "forceRefetch": false
} }
{ "id": 50, "result": {
    "data": [
        {
            "id": "demo-app",
            "name": "Demo App",
            "description": "Example connector for documentation.",
            "logoUrl": "https://example.com/demo-app.png",
            "logoUrlDark": null,
            "iconAssets": {
                "256_square": "https://example.com/demo-app-square.png"
            },
            "iconDarkAssets": null,
            "distributionChannel": null,
            "branding": null,
            "appMetadata": null,
            "labels": null,
            "installUrl": "https://chatgpt.com/apps/demo-app/demo-app",
            "isAccessible": true,
            "isEnabled": true
        }
    ],
    "nextCursor": null
} }
```

When `threadId` is provided, app feature gating (`Feature::Apps`) is evaluated using that thread's config snapshot. When omitted, the latest global config is used.

`app/list` returns after both accessible apps and directory apps are loaded. Set `forceRefetch: true` to bypass app caches and fetch fresh data from sources. Cache entries are only replaced when those refetches succeed.

The server also emits `app/list/updated` notifications when newly loaded accessible or directory apps change the merged app list. Each notification includes the latest merged app list. An initial cached `app/list` still emits one final notification so other initialized clients can refresh their app list, while reading an unchanged cached continuation page does not emit a duplicate notification; `forceRefetch: true` preserves the existing progressive notifications while fresh data loads.

```json
{
  "method": "app/list/updated",
  "params": {
    "data": [
      {
        "id": "demo-app",
        "name": "Demo App",
        "description": "Example connector for documentation.",
        "logoUrl": "https://example.com/demo-app.png",
        "logoUrlDark": null,
        "iconAssets": {
          "256_square": "https://example.com/demo-app-square.png"
        },
        "iconDarkAssets": null,
        "distributionChannel": null,
        "branding": null,
        "appMetadata": null,
        "labels": null,
        "installUrl": "https://chatgpt.com/apps/demo-app/demo-app",
        "isAccessible": true,
        "isEnabled": true
      }
    ]
  }
}
```

Use `app/read` when a client already has app ids and only needs metadata. The request accepts at
most 100 `appIds`; repeated ids are deduplicated while preserving first-request order. Both `apps`
and `missingAppIds` follow that order. Unknown or unauthorized ids are returned as partial misses
instead of failing the whole request.

```json
{ "method": "app/read", "id": 51, "params": {
    "appIds": ["demo-app", "missing-app"],
    "includeTools": true
} }
{ "id": 51, "result": {
    "apps": [
        {
            "id": "demo-app",
            "name": "Demo App",
            "description": "Example app for documentation.",
            "iconUrl": "https://files.openai.com/content?id=demo-app",
            "toolSummaries": [
                {
                    "name": "search",
                    "title": "Search",
                    "description": "Search the app.",
                    "isEnabled": true,
                    "disabledReason": null,
                    "isReadOnly": true
                }
            ]
        }
    ],
    "missingAppIds": ["missing-app"]
} }
```

`app/read` reads fresh metadata records from a cache partitioned by backend URL and ChatGPT
account/workspace identity, then makes at most one `POST /ps/apps/batch` for missing or
expired ids. `includeTools` defaults to false and is forwarded as `include_tools`; a fresh
metadata-only cache entry is refetched when tool summaries are requested. Backend or transport
failures return an RPC error without replacing existing cache records. Its metadata shape can
include display-only public tool summaries with enabled/read-only state and intentionally excludes
runtime state, MCP tool state, full actions, and model descriptions.

Connected apps may override the thread's approval reviewer in `config.toml`.
Use `apps._default.approvals_reviewer` to set the reviewer for all apps, and a
per-app value to override that default. When both are omitted, the app inherits
the top-level `approvals_reviewer` value:

```toml
approvals_reviewer = "auto_review"

[apps._default]
approvals_reviewer = "user"
default_tools_approval_mode = "prompt"

[apps.demo-app]
approvals_reviewer = "auto_review"
default_tools_approval_mode = "approve"
```

Setting the app value to `"user"` routes its approval prompts to the user
instead of Guardian; setting it to `"auto_review"` opts that app into Guardian
review when allowed by configuration requirements.

Use `apps._default.default_tools_approval_mode` to set the approval mode for
tools without a per-app or per-tool override. Supported values are `"auto"`,
`"prompt"`, `"writes"`, and `"approve"`. The `"writes"` mode prompts for tools
that do not advertise `readOnlyHint = true` and skips declared read-only tools.
Tool-level `approval_mode` takes precedence over
the per-app `default_tools_approval_mode`, which takes precedence over the
`apps._default` value. Managed tool requirements take precedence over all of
these settings. When none are configured, the mode defaults to `"auto"`.

Invoke an app by inserting `$<app-slug>` in the text input. The slug is derived from the app name and lowercased with non-alphanumeric characters replaced by `-` (for example, "Demo App" becomes `$demo-app`). Add a `mention` input item (recommended) so the server uses the exact `app://<connector-id>` path rather than guessing by name. Plugins use the same `mention` item shape, but with `plugin://<plugin-name>@<marketplace-name>` paths from `plugin/installed` or `plugin/list`.

Example:

```
$demo-app Pull the latest updates from the team.
```

```json
{
  "method": "turn/start",
  "id": 51,
  "params": {
    "threadId": "thread-1",
    "input": [
      {
        "type": "text",
        "text": "$demo-app Pull the latest updates from the team."
      },
      { "type": "mention", "name": "Demo App", "path": "app://demo-app" }
    ]
  }
}
```

## Auth endpoints

The JSON-RPC auth/account surface exposes request/response methods plus server-initiated notifications (no `id`). Use these to determine auth state, start or cancel logins, logout, and inspect ChatGPT rate limits.

### Authentication modes

Codex supports these authentication modes. The current mode is surfaced in `account/updated` (`authMode`), which also includes the current ChatGPT `planType` when available, and can be inferred from `account/read`.

- **API key (`apiKey`)**: Caller supplies an OpenAI API key via `account/login/start` with `type: "apiKey"`. The API key is saved and used for API requests.
- **ChatGPT managed (`chatgpt`)** (recommended): Codex owns the ChatGPT OAuth flow and refresh tokens. Start via `account/login/start` with `type: "chatgpt"` for the browser flow or `type: "chatgptDeviceCode"` for device code; Codex persists tokens to disk and refreshes them automatically.
- **Codex managed Amazon Bedrock auth (`amazonBedrock`, experimental)**: Caller supplies an Amazon Bedrock API key and region via `account/login/start` with `type: "amazonBedrock"`. The client must enable the `experimentalApi` initialization capability for Codex-managed Amazon Bedrock login. Codex replaces the current primary auth with the Bedrock credential and writes `model_provider = "amazon-bedrock"` to the user config.
- **Personal access token (`personalAccessToken`)**: Codex uses a ChatGPT-backed personal access token loaded outside the app-server login RPCs, such as with `codex login --with-access-token` or `CODEX_ACCESS_TOKEN`.

### API Overview

- `account/read` — fetch current account info; optionally refresh tokens.
- `account/login/start` — begin login (`apiKey`, `chatgpt`, `chatgptDeviceCode`, `amazonBedrock`).
- `account/login/completed` (notify) — emitted when a login attempt finishes (success or error).
- `account/login/cancel` — cancel a pending managed ChatGPT login by `loginId`.
- `account/logout` — sign out; triggers `account/updated` on success.
- `account/updated` (notify) — emitted whenever auth mode changes (`authMode`: `apikey`, `bedrockApiKey`, `chatgpt`, `personalAccessToken`, or `null`) and includes the current ChatGPT `planType` when available.
- `account/rateLimits/read` — fetch ChatGPT rate limits, an optional effective monthly credit limit, whether spend control has been reached, and the earned rate-limit resets currently available, including expiry details when provided by the backend. Rate-limit updates arrive via `account/rateLimits/updated` (notify); reset-credit data is snapshot-only.
- `account/rateLimitResetCredit/consume` — consume one earned reset using a caller-provided idempotency key, optionally selecting a reset-credit ID returned by `account/rateLimits/read`.
- `account/usage/read` — fetch ChatGPT account token-activity summary and daily buckets.
- `account/workspaceMessages/read` — fetch active workspace messages, including workspace notification headlines when available.
- `account/rateLimits/updated` (notify) — emitted whenever a user's ChatGPT rate limits change. This is a sparse rolling update; merge available values into the most recent `account/rateLimits/read` response or refetch that snapshot.
  `spendControlReached` is `true` or `false` when the backend reports spend-control state; `null` means unavailable and must not clear a previously observed value in a sparse update.
- `account/sendAddCreditsNudgeEmail` — ask ChatGPT to email the workspace owner about depleted credits or a reached usage limit.
- `mcpServer/oauthLogin/completed` (notify) — emitted after a `mcpServer/oauth/login` flow finishes for a server; payload includes `{ name, threadId, success, error? }`.
- `mcpServer/startupStatus/updated` (notify) — emitted when a configured MCP server's startup status changes; payload includes `{ threadId, name, status, error, failureReason }`, where `threadId` is the owning thread when startup is thread-scoped and `null` when it is app-scoped, and `status` is `starting`, `ready`, `failed`, or `cancelled`. `failureReason` is `reauthenticationRequired` when stored OAuth credentials have expired and cannot be refreshed, so clients can prompt the user to reconnect the named server.

### 1) Check auth state

Request:

```json
{ "method": "account/read", "id": 1, "params": { "refreshToken": false } }
```

Response examples:

```json
{ "id": 1, "result": { "account": { "type": "chatgpt", "email": "user@example.com", "planType": "pro" }, "requiresOpenaiAuth": true } }
{ "id": 1, "result": { "account": { "type": "amazonBedrock", "usesCodexManagedCredentials": false }, "requiresOpenaiAuth": false } }
```

Field notes:

- `refreshToken` (bool): set `true` to force a token refresh.
- `email` is `null` when the ChatGPT account does not have an email address.
- `requiresOpenaiAuth` reflects the active provider; when `false`, Codex can run without OpenAI credentials.
- Amazon Bedrock reports `usesCodexManagedCredentials: true` when it uses a Bedrock API key managed by Codex. It reports `false` for external credential paths, including the AWS credential chain and configured command auth. This identifies whether Codex-managed credentials are selected; it does not validate that the credential source can resolve credentials.

### 2) Log in with an API key

1. Send:
   ```json
   {
     "method": "account/login/start",
     "id": 2,
     "params": { "type": "apiKey", "apiKey": "sk-…" }
   }
   ```
2. Expect:
   ```json
   { "id": 2, "result": { "type": "apiKey" } }
   ```
3. Notifications:
   ```json
   { "method": "account/login/completed", "params": { "loginId": null, "success": true, "error": null } }
   { "method": "account/updated", "params": { "authMode": "apikey", "planType": null } }
   ```

### 3) Log in with ChatGPT (browser flow)

1. Start:
   ```json
   { "method": "account/login/start", "id": 3, "params": { "type": "chatgpt" } }
   { "id": 3, "result": { "type": "chatgpt", "loginId": "<uuid>", "authUrl": "https://chatgpt.com/…&redirect_uri=http%3A%2F%2Flocalhost%3A<port>%2Fauth%2Fcallback" } }
   ```
2. Open `authUrl` in a browser; the app-server hosts the local callback.
   By default, a successful callback redirects to the local success page. Clients may set
   `useHostedLoginSuccessPage: true` to redirect successful callbacks that do not require
   organization setup to the hosted Codex success page instead. When hosted login success is
   enabled, clients may set `appBrand` to `"codex"` or `"chatgpt"` to select the matching hosted
   page artwork; omitted or `null` values default to `"codex"`.
3. Wait for notifications:
   ```json
   { "method": "account/login/completed", "params": { "loginId": "<uuid>", "success": true, "error": null } }
   { "method": "account/updated", "params": { "authMode": "chatgpt", "planType": "plus" } }
   ```

### 3) Log in with an Amazon Bedrock API key

This experimental flow requires the client to initialize with `experimentalApi: true`.

1. Send:
   ```json
   {
     "method": "account/login/start",
     "id": 3,
     "params": { "type": "amazonBedrock", "apiKey": "…", "region": "us-west-2" }
   }
   ```
2. Expect:
   ```json
   { "id": 3, "result": { "type": "amazonBedrock" } }
   ```
3. Notifications:
   ```json
   { "method": "account/login/completed", "params": { "loginId": null, "success": true, "error": null } }
   { "method": "account/updated", "params": { "authMode": "bedrockApiKey", "planType": null } }
   ```

Codex stores the key and region as the primary Codex auth, replacing any previously stored login, and writes `model_provider = "amazon-bedrock"` to the active user config. Existing loaded sessions keep their current provider selection, so clients should restart the app-server before sending more model requests. This limitation will be addressed in a follow-up.

### 4) Log in with ChatGPT (device code flow)

1. Start:
   ```json
   { "method": "account/login/start", "id": 4, "params": { "type": "chatgptDeviceCode" } }
   { "id": 4, "result": { "type": "chatgptDeviceCode", "loginId": "<uuid>", "verificationUrl": "https://auth.openai.com/codex/device", "userCode": "ABCD-1234" } }
   ```
2. Show `verificationUrl` and `userCode` to the user; the frontend owns the UX.
3. Wait for notifications:
   ```json
   { "method": "account/login/completed", "params": { "loginId": "<uuid>", "success": true, "error": null } }
   { "method": "account/updated", "params": { "authMode": "chatgpt", "planType": "plus" } }
   ```

### 5) Cancel a ChatGPT login

```json
{ "method": "account/login/cancel", "id": 5, "params": { "loginId": "<uuid>" } }
{ "method": "account/login/completed", "params": { "loginId": "<uuid>", "success": false, "error": "…" } }
```

### 6) Logout

```json
{ "method": "account/logout", "id": 6 }
{ "id": 6, "result": {} }
{ "method": "account/updated", "params": { "authMode": null, "planType": null } }
```

When using a Codex-managed Bedrock key, logout removes the key and clears `model_provider` if it is still set to `"amazon-bedrock"`. When using AWS-managed credentials, manage them through AWS or switch providers before logging out.

### 7) Rate limits (ChatGPT)

```json
{ "method": "account/rateLimits/read", "id": 7 }
{
  "id": 7,
  "result": {
    "rateLimits": {
      "primary": { "usedPercent": 25, "windowDurationMins": 15, "resetsAt": 1730947200 },
      "secondary": null,
      "rateLimitReachedType": null
    },
    "rateLimitResetCredits": {
      "availableCount": 2,
      "credits": [
        {
          "id": "RateLimitResetCredit_1",
          "resetType": "codexRateLimits",
          "status": "available",
          "grantedAt": 1781654400,
          "expiresAt": 1784246400,
          "title": "Full reset (Weekly + 5 hr)",
          "description": "Ready to redeem"
        }
      ]
    }
  }
}
{ "method": "account/rateLimits/updated", "params": { "rateLimits": { … } } }
```

Field notes:

- `usedPercent` is current usage within the OpenAI quota window.
- `windowDurationMins` is the quota window length.
- `resetsAt` is a Unix timestamp (seconds) for the next reset.
- `rateLimitReachedType` identifies the backend-classified limit state when one has been reached.
- `individualLimit` describes the effective monthly credit limit when available. In an `account/rateLimits/read` response, `null` means no monthly limit is available. In a sparse `account/rateLimits/updated` notification, nullable account metadata may be unavailable and does not clear a previously observed value.
- `rateLimitResetCredits` contains the available earned-reset count when the backend provides it; otherwise it is `null`.
- `rateLimitResetCredits.credits` is `null` when only the count is available. An empty array means details were fetched and no available credits were returned.
- The backend may cap `rateLimitResetCredits.credits`, so `availableCount` is the authoritative total and can be greater than the number of detail rows.
- Refetch `account/rateLimits/read` after consuming a reset.

### 8) Earned rate-limit resets (ChatGPT)

```json
{ "method": "account/rateLimitResetCredit/consume", "id": 8, "params": { "idempotencyKey": "8ae96ff3-3425-4f4c-8772-b6fd61502868", "creditId": "RateLimitResetCredit_1" } }
{ "id": 8, "result": { "outcome": "reset" } }
```

Field notes:

- `idempotencyKey` must be non-empty. A UUID is recommended for each logical redemption attempt; reuse the same value when retrying that attempt.
- `creditId` is optional. When provided, it must be a non-empty opaque ID returned by `account/rateLimits/read`; when omitted, the backend selects the next available credit.
- `reset` means a credit was consumed.
- `alreadyRedeemed` means the same redemption completed previously. Treat it as an idempotent success and refresh account limits.
- `nothingToReset` means there is no eligible rate-limit window to reset.
- `noCredit` means the account has no earned reset credits available.
- Refetch `account/rateLimits/read` after consuming a reset instead of inferring updated state from this response.

### 9) Workspace messages (ChatGPT)

```json
{ "method": "account/workspaceMessages/read", "id": 9 }
{ "id": 9, "result": { "featureEnabled": true, "messages": [
    { "messageId": "msg_123", "messageType": "headline", "messageBody": "Workspace maintenance starts at 5pm.", "createdAt": 1781395200, "archivedAt": null }
] } }
```

When the upstream workspace-message feature is disabled, `featureEnabled` is `false` and `messages` is empty.

### 10) Notify a workspace owner about a limit

```json
{ "method": "account/sendAddCreditsNudgeEmail", "id": 9, "params": { "creditType": "credits" } }
{ "id": 9, "result": { "status": "sent" } }
```

Use `creditType: "credits"` when workspace credits are depleted, or `creditType: "usage_limit"` when the workspace usage limit has been reached. If the owner was already notified recently, the response status is `cooldown_active`.

## Experimental API Opt-in

Some app-server methods and fields are intentionally gated behind an experimental capability with no backwards-compatible guarantees. This lets clients choose between:

- Stable surface only (default): no opt-in, no experimental methods/fields exposed.
- Experimental surface: opt in during `initialize`.

### Generating stable vs experimental client schemas

`codex app-server` schema generation defaults to the stable API surface (experimental fields and methods filtered out). Pass `--experimental` to include experimental methods/fields in generated TypeScript or JSON schema:

```bash
# Stable-only output (default)
codex app-server generate-ts --out DIR
codex app-server generate-json-schema --out DIR

# Include experimental API surface
codex app-server generate-ts --out DIR --experimental
codex app-server generate-json-schema --out DIR --experimental
```

### How clients opt in at runtime

Set `capabilities.experimentalApi` to `true` in your single `initialize` request:

```json
{
  "method": "initialize",
  "id": 1,
  "params": {
    "clientInfo": {
      "name": "my_client",
      "title": "My Client",
      "version": "0.1.0"
    },
    "capabilities": {
      "experimentalApi": true
    }
  }
}
```

Then send the standard `initialized` notification and proceed normally.

Notes:

- If `capabilities` is omitted, `experimentalApi` is treated as `false`.
- This setting is negotiated once at initialization time for the process lifetime (re-initializing is rejected with `"Already initialized"`).

### What happens without opt-in

If a request uses an experimental method or sets an experimental field without opting in, app-server rejects it with a JSON-RPC error. The message is:

`<descriptor> requires experimentalApi capability`

Examples of descriptor strings:

- `mock/experimentalMethod` (method-level gate)
- `thread/start.mockExperimentalField` (field-level gate)
- `askForApproval.granular` (enum-variant gate, for `approvalPolicy: { "granular": ... }`)

### For maintainers: Adding experimental fields and methods

Use this checklist when introducing a field/method that should only be available when the client opts into experimental APIs.

At runtime, clients must send `initialize` with `capabilities.experimentalApi = true` to use experimental methods or fields.

1. Annotate the field in the protocol type (usually `app-server-protocol/src/protocol/v2.rs`) with:
   ```rust
   #[experimental("thread/start.myField")]
   pub my_field: Option<String>,
   ```
2. Ensure the params type derives `ExperimentalApi` so field-level gating can be detected at runtime.

3. In `app-server-protocol/src/protocol/common.rs`, keep the method stable and use `inspect_params: true` when only some fields are experimental (like `thread/start`). If the entire method is experimental, annotate the method variant with `#[experimental("method/name")]`.

Enum variants can be gated too:

```rust
#[derive(ExperimentalApi)]
enum AskForApproval {
    #[experimental("askForApproval.granular")]
    Granular { /* ... */ },
}
```

If a stable field contains a nested type that may itself be experimental, mark
the field with `#[experimental(nested)]` so `ExperimentalApi` bubbles the nested
reason up through the containing type:

```rust
#[derive(ExperimentalApi)]
struct Config {
    #[experimental(nested)]
    approval_policy: Option<AskForApproval>,
}
```

For server-initiated request payloads, annotate the field the same way so schema generation treats it as experimental, and make sure app-server omits that field when the client did not opt into `experimentalApi`.

4. Regenerate protocol fixtures:

   ```bash
   just write-app-server-schema
   # Include experimental API fields/methods in fixtures.
   just write-app-server-schema --experimental
   ```

5. Verify the protocol crate:

   ```bash
   just test -p codex-app-server-protocol
   ```
=======
MXC uses the standard `command/exec` streaming and process-control path, including
ConPTY when `tty` is enabled. The buffered legacy Windows sandbox restrictions on
process control and custom output caps do not apply to MXC.
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
