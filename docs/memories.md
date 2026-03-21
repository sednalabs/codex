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
