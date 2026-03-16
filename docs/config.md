# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

For custom MCP servers, you can also apply server-local safety controls in each
`[mcp_servers.<name>]` entry:

- `enable_elicitation = true` to allow the server to issue MCP elicitation prompts.
- `read_only = true` to block mutating tools (based on tool `read_only_hint` metadata).
- `strict_tool_classification = true` to fail closed when tool mutability metadata is missing.
- `require_approval_for_mutating = true` to force explicit approval before mutating tool calls.
- `oauth_resource = "..."` to include an OAuth `resource` parameter (RFC 8707) during MCP login.

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

## Notify

Codex can run a notification hook when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

When Codex knows which client started the turn, the legacy notify JSON payload also includes a top-level `client` field. The TUI reports `codex-tui`, and the app server reports the `clientInfo.name` value from `initialize`.

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## SQLite State DB

Codex stores the SQLite-backed state DB under `sqlite_home` (config key) or the
`CODEX_SQLITE_HOME` environment variable. When unset, WorkspaceWrite sandbox
sessions default to a temp directory; other modes default to `CODEX_HOME`.

Codex now keeps three local SQLite stores under that same directory:

- `state.sqlite` for thread metadata and local runtime state
- `logs.sqlite` for rollout/log mirroring
- `usage.sqlite` for authoritative local usage facts

`usage.sqlite` is the forward-looking billing and attribution source for
downstream operator workflows. It stores thread lineage, spawn requests, tool
calls, provider-call token usage, quota snapshots, and fork snapshots without
depending on copied rollout history.

Rollout JSONL files under `~/.codex/sessions` still exist for UX, debugging,
and compatibility, but they should be treated as a fallback compatibility
source rather than the primary accounting record for newly patched clients.

If you need to inspect the local usage store directly, point SQLite tooling at
`$CODEX_SQLITE_HOME/usage.sqlite` or the equivalent file under `sqlite_home`.

## Custom CA Certificates

Codex can trust a custom root CA bundle for outbound HTTPS and secure websocket
connections when enterprise proxies or gateways intercept TLS. This applies to
login flows and to Codex's other external connections, including Codex
components that build reqwest clients or secure websocket clients through the
shared `codex-client` CA-loading path and remote MCP connections that use it.

Set `CODEX_CA_CERTIFICATE` to the path of a PEM file containing one or more
certificate blocks to use a Codex-specific CA bundle. If
`CODEX_CA_CERTIFICATE` is unset, Codex falls back to `SSL_CERT_FILE`. If
neither variable is set, Codex uses the system root certificates.

`CODEX_CA_CERTIFICATE` takes precedence over `SSL_CERT_FILE`. Empty values are
treated as unset.

The PEM file may contain multiple certificates. Codex also tolerates OpenSSL
`TRUSTED CERTIFICATE` labels and ignores well-formed `X509 CRL` sections in the
same bundle. If the file is empty, unreadable, or malformed, the affected Codex
HTTP or secure websocket connection reports a user-facing error that points
back to these environment variables.

## Notices

Codex stores "do not show again" flags for some UI prompts under the `[notice]` table.

## Plan mode defaults

`plan_mode_reasoning_effort` lets you set a Plan-mode-specific default reasoning
effort override. When unset, Plan mode uses the built-in Plan preset default
(currently `medium`). When explicitly set (including `none`), it overrides the
Plan preset. The string value `none` means "no reasoning" (an explicit Plan
override), not "inherit the global default". There is currently no separate
config value for "follow the global default in Plan mode".

## Sub-agent model precedence

For economical orchestration, treat model selection as a three-level precedence stack:

- Parent-session model settings are defaults for children.
- Role TOML files provide role-level defaults and may also lock fields for a role.
- Explicit `spawn_agent(model=..., reasoning_effort=...)` arguments are child-specific overrides.

If a role explicitly sets `model` or `model_reasoning_effort`, that role remains authoritative for those fields. Otherwise, explicit spawn-time overrides should beat inherited parent-profile settings. This keeps role defaults useful without preventing per-task cost and capability control.

## Realtime start instructions

`experimental_realtime_start_instructions` lets you replace the built-in
developer message Codex inserts when realtime becomes active. It only affects
the realtime start message in prompt history and does not change websocket
backend prompt settings or the realtime end/inactive message.

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).

## TUI interrupt defaults

By default, interrupting a running turn in the TUI uses double-`Esc` confirmation
to reduce accidental interrupts on terminals that encode Alt/meta with a leading
`Esc` byte.

- `[tui].double_esc_interrupt = true` (default) requires `Esc Esc`.
- `[tui].double_esc_interrupt = false` restores single-`Esc` interrupt behavior.
- `CODEX_TUI_DOUBLE_ESC_INTERRUPT=0` overrides config and forces single-`Esc`.

## TUI weekly pacing

The weekly usage status-line item can render pacing in two ways when fresh
weekly reset data is available:

- `[tui].weekly_limit_pacing_style = "qualitative"` (default) shows
  `weekly 44% (on pace)`, `weekly 44% (over 6%)`, or `weekly 44% (under 7%)`.
- `[tui].weekly_limit_pacing_style = "ratio"` shows `weekly 44%/50%`, where
  the numerator is weekly usage remaining and the denominator is week time remaining.

Stale snapshots still render as `weekly {remaining}% (stale)` in either mode.
