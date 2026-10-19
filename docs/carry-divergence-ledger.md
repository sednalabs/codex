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
