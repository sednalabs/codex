# AGENTS.md

For information about AGENTS.md, see the [AGENTS.md guide](https://developers.openai.com/codex/guides/agents-md).

## Hierarchical agents message

When the `child_agents_md` feature flag is enabled (via `[features]` in `config.toml`), Codex appends additional guidance about AGENTS.md scope and precedence to the user instructions message and emits that message even when no AGENTS.md is present.

## Local override precedence

Codex discovers project docs from the project root down to the current working
directory and concatenates one file per directory in that order.

At each directory level, Codex prefers:

1. `AGENTS.override.md`
2. `AGENTS.md`

If `AGENTS.override.md` exists, it replaces `AGENTS.md` for that directory
level only. Codex then continues the hierarchical search and concatenation for
parent/child directories until it reaches the project root marker. If no project
root marker is found, only the current working directory is considered.

This override only affects project-doc instructions. It does not control
realtime startup context, which is configured separately in
[`experimental_realtime_ws_startup_context`](config.md#realtime-startup-context).

## Position in the prompt stack

Project-doc instructions discovered from `AGENTS.md` / `AGENTS.override.md` are
merged into `user_instructions`; they do not replace the base instruction layer.

For the broader assembled prompt stack, review-mode override path, and sub-agent
inheritance model, see
[Codex CLI — Prompt Stack & Sub-Agent Executive Reference](prompt-stack.md).
For the ownership view of which layer should hold which kind of instruction,
see [Prompt Stack Ownership Matrix](prompt-stack-ownership-matrix.md).
