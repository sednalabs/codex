# App-server schema, helper, and documentation divergence inventory

This artifact documents the current downstream divergence in the bundled snapshot and is intentionally corrected to match the code that is actually present here. The earlier handoff narrative was stale in two important ways:

- downstream already ships `codex-rs/app-server/src/transport/remote_control/*`; this is **not** a “missing transport to restore” in this bundle
- downstream code already supports `--listen off`, so docs that describe only `stdio://` and `ws://IP:PORT` are behind the implementation

The remaining app-server carry in this snapshot is therefore centered on **protocol/schema/runtime semantics and helper/client compatibility**, not on remote-control transport absence.

## 1. Transport and docs reality in this snapshot

| Surface | Current downstream state | Doc correction |
| --- | --- | --- |
| `codex-rs/app-server/src/transport/remote_control/*` | Present downstream and present upstream in this bundle. The subtree is no longer the blocker story for this handoff. | Remove any wording that says downstream omits remote-control or still needs a transport restore before alignment can start. |
| `codex-rs/app-server/src/transport/mod.rs` / `lib.rs` | Downstream parses and wires `AppServerTransport::Off`. | Document `--listen off` alongside `stdio://` and `ws://IP:PORT` in the README/protocol docs. |

### What changed in the handoff story

The correct high-level story for this bundle is:

- **remote-control exists downstream already**
- **the real remaining divergence is protocol/schema/helper behavior**
- **the transport docs were partially stale**

## 2. Remaining protocol/schema differences that still matter

These are the highest-value divergences still visible in the bundled snapshot.

| Surface | Downstream state | Upstream state | Why it matters |
| --- | --- | --- | --- |
| `schema/json/v2/McpResourceReadParams.json` and `McpResourceReadResponse.json` plus TS analogues | Missing downstream | Present upstream | `mcpServer/resource/read` is not part of the downstream generated surface, so schema-driven clients cannot assume the upstream resource-read contract exists here. |
| `schema/json/v2/ListMcpServerStatusParams.json` and TS analogue | Downstream only exposes `cursor` + `limit` | Upstream also exposes `detail` (`McpServerStatusDetail`) | Downstream narrows `mcpServerStatus/list`; clients cannot rely on the upstream detail selector being accepted. |
| `schema/typescript/v2/Thread.ts` | No `forkedFromId` | Upstream includes `forkedFromId` | Thread metadata is slimmer downstream, so clients reading fork lineage from thread payloads will diverge. |
| `schema/typescript/v2/Turn.ts` and corresponding JSON bundle types | No `startedAt`, `completedAt`, `durationMs` in the turn type | Upstream includes all three | Downstream turn timing metadata is slimmer, which affects timeline/status UIs and any telemetry or replay tooling built from the generated types. |
| `schema/typescript/v2/ThreadStartParams.ts`, `ThreadResumeParams.ts`, `ThreadForkParams.ts` | `baseInstructions` / `developerInstructions` are typed as `string | null` | Upstream types preserve the explicit-null override contract as `string | null | undefined` | The generated downstream types collapse the explicit-null semantics that upstream uses to distinguish “field omitted” from “explicitly clear / use built-in behavior”. |
| `app-server-protocol/src/protocol/item_builders.rs` | Removed downstream | Present upstream | Downstream routes this behavior through other files, so replay/guardian/item construction behavior must be compared at runtime rather than assumed from upstream structure. |
| `app-server-protocol/src/protocol/v2.rs`, `thread_history.rs` | Rewritten downstream | Different upstream implementations | These files remain central carry points for thread history, item shaping, and replay/runtime semantics. |
| `codex-rs/app-server-client/src/lib.rs` and `src/remote.rs` | Downstream helper/client layer follows the downstream schema/runtime | Upstream helper/client layer follows the upstream schema/runtime | Any realignment needs client/schema/runtime changes treated as one unit rather than as independent cleanup. |

## 3. What the downstream docs should say now

Use the following framing going forward:

- The downstream app-server suite still has meaningful carry, but **it is no longer accurate to describe that carry as “remote-control transport removed downstream.”**
- The transport docs should reflect all supported runtime modes in the bundled snapshot, including `--listen off`.
- The remaining alignment work is mainly about **schema parity, runtime semantics, and helper/client compatibility**.

## 4. Extension map for cleanup

| Divergence ID | Surface type | Tangible differences | Action to take during cleanup |
| --- | --- | --- | --- |
| `codex-app-server-suite` | Agent-facing | Protocol/schema/runtime/helper carry; no longer a “remote-control missing” story in this bundle | Realign remaining schema/runtime/helper behavior, or keep it documented as intentional carry. |
| `codex-tui-session-surfaces` | Agent-facing | TUI session/usage surfaces diverge from upstream widgets; we maintain divergent snapshots and multi-agent behavior | Evaluate which UI diff can be upstreamed or documented before removing the divergence entry. |
| `shell-tool-mcp-project` | Operator-facing | Separate npm project with shell helpers | Decide whether to upstream the tooling or retire the entry. |
| `ci-workflow-automation` | Operator-facing | New workflows, scripts, validation lanes | Determine whether upstream can adopt these lanes or keep them documented in the ledger. |

## 5. Corrected cleanup checklist

1. **Stop treating remote-control as the primary missing seam in this bundle.** The transport subtree is already present downstream; verify behavior only where runtime differences still exist.  
2. **Re-synch schema artifacts deliberately** — decide whether to restore upstream surfaces such as `mcpServer/resource/read`, the `detail` selector on `mcpServerStatus/list`, richer thread/turn metadata, and explicit-null instruction semantics.  
3. **Align helper/client code with whichever schema you keep** — `app-server-client` and any downstream helpers must match the runtime/schema contract as a single unit.  
4. **Keep the docs honest** — README/protocol docs should enumerate `--listen off`, and divergence docs should describe the remaining carry as schema/runtime/helper divergence rather than transport absence.

Use this artifact as the corrected reference point for the app-server alignment handoff.
