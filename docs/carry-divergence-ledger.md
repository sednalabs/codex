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

The independently maintained companion source branch is
`companion/runtime-proof`. Its initial source is commit
`8eac9e3526b0e41b6b8108719a44855bfc150a31`, also retained by the historical tag
`companion-runtime-proof-8eac9e35`; the full commit, not an alias, is the source
authority. Keep historical tags fixed. Companion updates are reviewed,
fast-forward source changes that preserve this consumer; do not mirror native
`main` deletions into it. Compatible upstream changes can be adopted selectively.

The default branch retains `sedna-branch-build.yml` as a manual-dispatch
entrypoint, not a native companion builder. Select `companion/runtime-proof`
as the workflow ref to run its existing producer with `platform=linux-x86_64`,
`host_only=false`, and `ref` set to the full reviewed source commit. Its
resolved source manifest must match that commit. Pair the
CLI, code-mode host, proxy and runtime-proof launcher from the same source
build; a native package or a host-only artifact is not a compatible substitute.
The manifest's launcher hash identifies the launcher member, not the full
archive. Retain the producer run and artifact digest separately, and qualify
each updated companion source before consumer adoption. Building or retaining
this source does not authorize bearer provisioning, credential installation or
live use.

## OAuth login refresh capability

Ordinary OAuth login preserves legacy issuer-less access-token-only grants, but
rejects a newly returned refresh token without an authorization-server issuer
before reporting completion or saving credentials. Refresh already requires
that original binding; current discovery cannot reconstruct it later. The
failure is actionable and leaves existing stored credentials unchanged rather
than silently persisting refresh capability that cannot be used securely.
Remove this producer check when upstream provides an equivalent login contract.

## Historical usage-ledger checksums

Usage databases may retain exact historical checksums for migrations 1, 5,
and 9. The usage opener selects those expected checksums in memory
only when that recorded identity matches, then runs the existing migrations.
It does not rewrite migration history or accept unknown checksum mismatches.
The original migration 9 is accepted only with its two ordinary nullable text
principal columns under their original names, without the newer names. Native
provider-call and thread usage APIs do not consume those dormant columns, so
the adapter preserves their schema and data rather than inventing a rename.
Focused tests execute the original shipped SQL through migration 19, including
the retained migration 6, then exercise native runtime usage writes, queries,
and reopen. They preserve original ledger tuples while applying normal pending
migrations, retain canonical behavior, and reject unknown checksums or affected
schema before applying pending migrations.
Remove this adapter when an equivalent native upgrade path is demonstrated, or
when support for these historical databases is explicitly retired.

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
