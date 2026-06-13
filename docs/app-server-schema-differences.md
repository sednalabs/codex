# App-server parity and intentional downstream carry

This document is the human-readable companion to `docs/divergences/index.yaml` for the app-server stack.

The old version of this file was stale: it treated remote-control and several protocol/runtime surfaces as still missing downstream. The current downstream realignment path restores those upstream surfaces first and then keeps only the fork-specific carries that are still intentional.

## Upstream parity restored by the current bundle sequence

These are no longer open divergence buckets once Bundles A + B land:

- `mcpServer/resource/read` is restored end-to-end.
- `mcpServerStatus/list` regains the upstream `detail` selector.
- `Thread.forkedFromId` is restored on forked thread objects.
- `Turn.startedAt`, `completedAt`, and `durationMs` flow through replay and live runtime again.
- explicit-null `baseInstructions` / `developerInstructions` semantics match upstream again.
- guardian review action/source typing and structured replay item builders are restored.

## Intentional downstream carry that remains after parity

These are the app-server differences that are still meant to survive downstream:

| Area | Paths | Why it stays downstream |
| --- | --- | --- |
| Raw-byte websocket secrets | `codex-rs/app-server/src/transport/auth.rs` | Accept binary secret material for websocket auth instead of forcing UTF-8 text decoding. |
| Non-blocking streamed output deltas | `codex-rs/app-server/src/command_exec.rs`, `codex-rs/app-server/src/outgoing_message.rs`, `codex-rs/app-server/src/extensions.rs` | Keep command output streaming responsive by enqueueing delta notifications without waiting for transport write completion. |
| Rich fs/watch behavior | `codex-rs/app-server/src/fs_watch.rs`, `codex-rs/app-server/src/extensions.rs` | Preserve watch-before-create parent registration, parent-event remapping, recursive directory watching, and changed-path dedupe. |
| Startup/plugin-sync follow-up | `codex-rs/app-server/src/codex_message_processor.rs`, `codex-rs/app-server/src/extensions.rs` | Keep cache-clear plus plugin-startup-task follow-up after config mutations, alongside the separate bounded startup sync carry in `codex-core`. |
| `collabToolCall.timedOut` | protocol + server runtime | Preserve downstream timeout visibility on collab wait calls. |
| Downstream MCP safety controls | config/protocol surfaces | Keep downstream MCP safety fields while remaining compatible with upstream OAuth resource support. |
| Runtime-context overlay | replay/read paths + extension seam | Preserve downstream preference for fresher live runtime/accounting state over stale replay-only summaries when a thread is still active. |

## Internal extension seam

Bundle B.2 introduces a small internal extension seam in `codex-rs/app-server/src/extensions.rs`. This is not a general plugin system. It exists so downstream policy/lifecycle carries can be kept behind narrow hooks while the restored upstream protocol/runtime structure stays readable.

Current hook areas:

- notification dispatch policy
- fs/watch policy
- config-mutation follow-up
- startup lifecycle follow-up
- history/runtime overlay hook points (currently no-op)

## Test and docs expectations

Bundle C is where the preserved carry is locked in with tests and docs:

- restore upstream app-server tests for `mcpServer/resource/read` and `mcpServerStatus/list detail`
- refresh downstream tests for restored `forkedFromId` and explicit-null instruction semantics
- keep downstream tests that assert review token summaries and collab timeout behavior
- keep auth / command-exec / fs-watch unit tests proving the preserved downstream semantics

If a future cleanup cannot explain a difference as either “restored upstream parity” or “intentional downstream carry”, it probably does not belong in the app-server divergence bucket anymore.
