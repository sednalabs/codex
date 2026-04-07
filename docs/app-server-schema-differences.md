# App-server schema & transport divergence inventory

This artifact catalogs the downstream-only divergences that cause `codex-app-server-suite` to show up in the ledger. It documents exactly what we no longer export upstream, what helpers we ship instead, and how the JSON/TypeScript schema shapes differ so cleanup work can proceed from a concrete checklist.

## 1. Remote-control transport (removed downstream)

| Path | Notes |
| --- | --- |
| `codex-rs/app-server/src/transport/remote_control/client_tracker.rs` | Upstream still compiles and runs this file; downstream never merged it. It tracks remote-control clients. |
| `codex-rs/app-server/src/transport/remote_control/enroll.rs` | Remote-control enrollment handler that upstream continues to ship and downstream omits. |
| `codex-rs/app-server/src/transport/remote_control/protocol.rs` | Defines the enrollment/websocket protocol. Missing from downstream, so clients cannot speak remote-control anymore. |
| `codex-rs/app-server/src/transport/remote_control/websocket.rs` | Upstream websocket dispatcher for remote-control; no counterpart downstream. |
| `codex-rs/app-server/src/transport/remote_control/tests.rs` | Test suite asserting the remote-control flow; downstream deletes it. |

**What to do:** decide whether to reintroduce the remote-control transport (merge upstream files or port the feature), or keep it removed and document that the divergence is intentional. Track the decision in OPS work item `w3564`. The ledger entry `codex-app-server-suite` already references this surface and will keep the audit flag alive until this gap is resolved.

## 2. Protocol schema & helper differences

The downstream fork rewrites or removes most of the schema-generated artifacts that upstream still publishes. The table below lists upstream files that no longer exist downstream (the audit lists them as missing because `codex-app-server-suite` covers the gap).

| Upstream schema file | Downstream status | Why it matters |
| --- | --- | --- |
| `schema/json/ClientRequest.json` | Removed | Defines the base RPC request shape; downstream now ships a different request contract. |
| `schema/json/ServerNotification.json` | Removed | Downstream uses its own notification payload shapes, so agents receive different metadata. |
| `schema/json/codex_app_server_protocol.schemas.json` | Removed | The canonical schema bundle is downstream-specific, so schema-driven clients must be updated. |
| `schema/json/codex_app_server_protocol.v2.schemas.json` | Removed | The v2 schema registry is different downstream; thread/turn metadata no longer matches upstream. |
| `schema/json/v2/*` (ConfigRequirementsReadResponse, GuardianApprovalReview*, ListMcpServerStatusParams, McpResourceRead{Params,Response}, ReviewStartResponse, Thread*Response, Turn*Notification) | Removed/rewritten | Each of these describes specific endpoint request/responses; downstream either omits the fields or rewrites them with fork-specific metadata. |
| `schema/typescript/*` | Removed/rewritten analogues (see upstream file names above) | TypeScript bindings diverged as well, so downstream clients are incompatible with upstream-generated types. |
| `app-server-protocol/src/protocol/item_builders.rs` | Removed | Downstream reimplemented item builders inside `protocol/v2.rs` and `thread_history.rs`, so the metadata pipeline behaves differently. |
| `app-server-protocol/src/protocol/v2.rs`, `thread_history.rs` | Rewritten | These files produce different thread state/turn metadata than upstream’s versions. |

### Downstream-specific helper clients

| Path | Purpose |
| --- | --- |
| `codex-rs/app-server-client/src/lib.rs` and supporting modules | Clients that understand the downstream schema and orchestrate how agents/ops talk to our forked protocol. They must be aligned with the schema above before any cleanup completes. |

## 3. Extension map for cleanup

Use the table below as your “human-readable ledger” when auditing divergences:

| Divergence ID | Surface type | Tangible differences | Action to take during cleanup |
| --- | --- | --- | --- |
| `codex-app-server-suite` | Agent-facing | Downstream-only transports (remote control removal) and schema rework plus helper clients | Either merge upstream files back in or keep the divergence entry and document the intentional differences. |
| `codex-tui-session-surfaces` | Agent-facing | TUI session/usage surfaces diverge from upstream widgets; we maintain divergent snapshots and multi-agent behavior | Evaluate which UI diff can be upstreamed or documented before removing the divergence entry. |
| `shell-tool-mcp-project` | Operator-facing | Separate npm project with shell helpers | Decide whether to upstream the tooling or retire the entry. |
| `ci-workflow-automation` | Operator-facing | New workflows, scripts, validation lanes | Determine whether upstream can adopt these lanes or keep them documented in the ledger. |

## 4. Cleanup checklist

1. **Remote-control reintroduction** – copy or merge `transport/remote_control/*` sequence files from upstream; update `guardrail_lane` `codex-app-server-suite` once the transport exists.  
2. **Re-synch schema artifacts** – reconcile each missing JSON/TS file with upstream: either reintroduce them or maintain documentation describing why downstream shapes differ.  
3. **Helper alignment** – ensure `app-server-client` matches whichever schema you keep; if you remove the divergence by matching upstream, delete the downstream-only helpers or port them upstream.  
4. **Document decisions** – add notes to `docs/divergences/index.yaml` and Ops work items (`w2915`, `w3564`) describing whether these differences are intentional, so the audit becomes an accurate ledger.

Use this artifact as your canonical reference while cleaning up the app-server divergence. If you need exports for other divergence IDs (TUI, shell tool, CI), I can create companion documents detailing those surfaces as well.
