# Memories

Codex Memories is the local startup memory pipeline that turns prior interactive rollouts into bounded, reusable context. It is separate from `ops-knowledge`: this is per-user/local Codex state under `~/.codex/memories`, not a shared knowledge MCP server.

## What it stores

- `raw_memories.md` - merged stage-1 memory output, newest first.
- `rollout_summaries/` - one summary file per retained rollout.
- `MEMORY.md` - navigational memory index built from retained outputs.
- `memory_summary.md` - compact summary injected into memory prompts.
- `skills/` - optional reusable skill packages derived from recurring patterns.

## When it runs

Codex only starts the pipeline for eligible root sessions. In practice that means:

- the session is not ephemeral
- the memory feature is enabled
- the session is not a sub-agent session
- the local state database is available

The pipeline runs in two phases:

1. Phase 1 scans recent eligible rollouts and extracts a structured memory from each one.
2. Phase 2 consolidates the retained memories into the on-disk memory workspace.

Phase 2 also writes attestation sidecars and records a durable per-memory-root
requirement in the local state DB. After a memory root has completed that
bootstrap path once, Codex treats missing attestation as a fail-closed error on
later reuse instead of silently reopening the bootstrap path. That protects the
consolidated workspace from drift when retained artifacts or sidecars disappear.

## Resume and refresh behavior

Resumed sessions are tracked at the thread level, not from a special resume checkpoint.

- If a thread has not changed since its last successful stage-1 extraction, Codex skips it as already up to date.
- If a thread has new activity and later becomes eligible again, stage 1 reprocesses the persisted rollout for that thread and updates the same stored memory record in place.
- This means Codex does not create duplicate stage-1 database rows for the same thread just because it was revisited later.
- The persisted rollout is the source of truth for re-ingest. If earlier history was compacted away or never persisted, the memory pipeline cannot recover it.

This is the main reason long-lived resumed sessions can still incur fresh summarization cost later: the thread can be reconsidered after new activity, and stage 1 reads the persisted rollout again before applying its prompt-size bounds.

## How growth is bounded

Memories do not accumulate forever.

- Startup extraction only considers rollouts inside a recent age window and only after they have been idle long enough.
- Phase 1 is capped per startup so each run only claims a bounded number of rollouts.
- Phase 2 keeps only a bounded retained set for the shared memory artifacts.
- Stale, unused memories fall out of the retained set and can be pruned from the local store.

The built-in defaults are intentionally conservative:

- `max_rollout_age_days = 30`
- `min_rollout_idle_hours = 6`
- `max_rollouts_per_startup = 16`
- `max_raw_memories_for_consolidation = 256`
- `max_unused_days = 30`

Stage 1 input is also bounded when a retained rollout is large. Codex truncates the extracted rollout context before sending it to the memory model, and the shared memory summary injected into prompts is separately capped so the memory layer stays bounded even when individual sessions grow.

## Discard and reset behavior

There are two different ways memory state can disappear:

- Normal retention-driven discard: if phase 2 has no retained memories left, Codex rewrites `raw_memories.md` to an empty stub and removes `MEMORY.md`, `memory_summary.md`, and `skills/`.
- Full reset: maintenance helpers can clear the memory root on disk and clear memory rows and jobs from the local state database.

That distinction matters because an empty retained set is not the same as a full wipe. The normal consolidation path leaves the memory workspace in a valid empty state rather than deleting every file unconditionally.

## Config knobs

The settings live under `[memories]` in `config.toml`.

- `generate_memories` controls whether new threads are stored in memory mode.
- `use_memories` controls whether memory usage instructions are injected into prompts.
- `no_memories_if_mcp_or_web_search` marks threads as polluted when web search or MCP tool use is detected.
- `extract_model` overrides the phase-1 summarization model. When unset, Codex uses `gpt-5.1-codex-mini` with `Low` reasoning effort.
- `consolidation_model` overrides the phase-2 consolidation model. When unset, Codex uses `gpt-5.3-codex` with `Medium` reasoning effort.

The built-in memory pipeline defaults are:

- Phase 1 extraction: `gpt-5.1-codex-mini`
- Phase 2 consolidation: `gpt-5.3-codex`

If you want the system to stay compact, keep the defaults. If you want to tune recall or reduce startup work, adjust the retention caps carefully and re-check the resulting memory workspace.
