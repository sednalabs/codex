# Carry divergence ledger

This ledger records current source contracts for downstream carries. It is
source documentation, not provider activation or delivery acceptance.

## Carry and convergence principles

Prefer equivalent upstream behavior and existing configuration, hooks, MCP,
or documented app-server interfaces. Keep service-specific workflows, policy,
and state in their owning systems, connected through small adapters suited to
the actual capability. Do not add a generic plugin framework for a small carry
or rebuild a full harness elsewhere merely to reduce the fork diff.

The behavior is the contract; its current files and implementation are not.
Keep necessary runtime integration local when it is the simplest reliable
design, and state its concise rationale and removal condition with the carry.
Compare observable end-to-end semantics, including authorization,
permissions, cancellation, persistence and recovery, typed images, and the
difference between actionable attention and routine progress. MCP availability
alone does not establish wake, cancellation, or takeover capability. Retire
behavior only when an equivalent replacement is demonstrated or its loss is
explicitly accepted. Tests should protect these consumer-visible contracts and
meaningful security boundaries, not incidental file, module, or handler layout.

## Dynamic tool image replay

The TUI replay cache projects dynamic tool images to the renderer's existing
`<image content>` placeholder before retaining a copy. Original live typed-image
notifications and canonical thread history remain unchanged, as do text/audio,
item metadata and pending interactive requests. This avoids duplicating image
bytes that the current renderer does not display; it is not a process memory
limit or a change to child-session residency. Focused `thread_event_buffer`
tests compare replay insertion and rendered presentations with the originals.
Remove this carry when upstream provides equivalent replay retention, or revisit
it before introducing an image-aware replay renderer.

## Per-server MCP protocol preference

The `mcp-per-server-protocol-selection` carry exposes `protocol_mode = "legacy"`
or `protocol_mode = "2026-07-28"` for a Streamable HTTP server. The explicit
preference follows the winning server configuration; when absent, native
registration, host-owned Apps, and global defaults retain their precedence.
The field is rejected on stdio. Modern mode requests discovery and can fall
back to legacy; configuration alone does not prove the negotiated wire mode.
Connection reuse remains mode-aware, and uncertain `tools/call` requests are
not replayed. Config parsing, canonical TOML edits, precedence and reuse tests
protect this adapter alongside native transport tests. Installed first use is
separate acceptance. Remove the config adapter when upstream provides an
equivalent per-server preference without losing these boundaries.

## Callback-input authentication boundaries

Native MCP callback input retains the configured server's issuer, resource,
scope and credential store. Fixed-policy service bearer provisioning is a
separate companion consumer, not an interchangeable MCP login path. Its
callback validation, privileged credential envelope and current artifact
qualification remain independent of a native source-ancestry merge. The
native package does not acquire that provisioner by importing its history.

## Upgrading and retiring carries

Pin the upstream revision being considered, then compare it against current
downstream `main` and its accepted capabilities. First look for equivalent
upstream behavior or a documented native interface; assess each remaining
behavioral difference as a candidate residual carry. Preserve current-main
ancestry and accepted capabilities when composing the upgrade.

Use existing hosted lockfile, schema, and focused-consumer validation routes
for the exact candidate. Validate the exact package artifact with its consumer
in the same run. Then satisfy required pull-request checks, merge-queue checks,
and confirm the actual merge before calling the upgrade landed.

For state or profile cutovers, verify compatibility and define a specific
rollback for the affected state as well as the executable; restoring an old
executable alone does not reverse a migration. Record each justified residual
carry's rationale and removal condition. If an upgrade would materially lose
behavior, seek acceptance of that loss before retiring the carry.
