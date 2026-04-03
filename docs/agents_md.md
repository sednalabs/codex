# AGENTS.md

For information about AGENTS.md, see [this documentation](https://developers.openai.com/codex/guides/agents-md).

## Hierarchical agents message

When the `child_agents_md` feature flag is enabled (via `[features]` in `config.toml`), Codex appends additional guidance about AGENTS.md scope and precedence to the user instructions message and emits that message even when no AGENTS.md is present.

## Local override precedence

If an `AGENTS.override.md` file exists, it is loaded instead of `AGENTS.md` at that path level. The search order is:

1. `AGENTS.override.md`
2. `AGENTS.md`
3. Continue walking upward until the project root marker.

The override only affects project-doc instructions. It does not control realtime startup context, which is configured separately (see `experimental_realtime_ws_startup_context` in `docs/config.md`).
