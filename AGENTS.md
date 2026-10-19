# Repository guidance

This repository is a downstream fork of [openai/codex](https://github.com/openai/codex). Keep changes focused on user-visible behavior and total maintenance cost.

- Prefer equivalent upstream behavior, existing configuration, hooks, MCP, and documented app-server interfaces before carrying new behavior.
- Keep service-specific workflows, policy, and state in their owning systems; use small adapters suited to the capability rather than a generic plugin framework.
- Preserve required functionality, not a particular implementation. Validate observable end-to-end behavior, including authorization, cancellation, persistence and recovery, typed images, and actionable attention versus routine progress. MCP availability alone does not imply wake, cancellation, or takeover support.
- Keep necessary runtime integration local when that is the simplest reliable design. A small direct patch is legitimate; record its rationale and removal condition in the [carry divergence ledger](docs/carry-divergence-ledger.md). Do not reimplement a full harness elsewhere merely to shrink the fork diff.
- Retire behavior only after demonstrating an equivalent replacement or explicitly accepting its loss. Test meaningful behavior and security boundaries rather than incidental file, module, or handler layout; retain structural checks where they enforce security properties.

For repository-specific carry decisions, see [the carry divergence ledger](docs/carry-divergence-ledger.md).
