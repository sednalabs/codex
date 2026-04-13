# Prompt Stack Ownership Matrix

This page explains who owns each major prompt layer in the current
implementation and what kinds of content belong in each layer.

Use it when the practical question is not just "what is in the prompt stack?"
but "which layer should own this instruction?"

For the front-door overview, see
[Codex CLI — Prompt Stack & Sub-Agent Executive Reference](prompt-stack.md). For
the implementation-faithful walkthrough, see
[Prompt Stack Deep Dive](prompt-stack-deep-dive.md).

## Layer matrix

| Layer | Typical contents | Source of truth | Typical owner | What this layer should own |
| --- | --- | --- | --- | --- |
| Resolved base instructions | `config.base_instructions`, resumed session base instructions, model built-ins such as `codex-rs/core/prompt.md`, review override via `REVIEW_PROMPT` | runtime config, session metadata, built-in prompt assets | runtime/harness owners, model/runtime code owners, user config when explicitly overridden | Core durable behavior baseline for the active session |
| Per-turn developer bundle | sandbox and approval guidance, collaboration-mode instructions, memory instructions, realtime start/end hints, personality text, apps/connectors, skills, plugins, commit attribution | per-turn assembly in `codex-rs/core/src/codex.rs` plus runtime feature/config plumbing | runtime/harness owners, plugin and skill authors, user config for feature toggles | Mode, runtime wiring, tool and connector availability, short-lived execution scaffolding |
| Contextual user/environment bundle | `config.user_instructions`, project-doc blend from `AGENTS.md` / `AGENTS.override.md`, feature-gated JS REPL guidance, hierarchical AGENTS message, serialized environment context | `codex-rs/core/src/project_doc.rs` plus runtime environment assembly | user config, repo instruction authors, runtime feature owners | Durable local/repo behavior defaults plus environment state |
| Current user or task payload | user turn text, review task payload, spawned child task prompt, tool-generated task payloads | the active turn or the orchestrator that created the child task | the current user, the active parent agent, task-specific workflow code | Immediate task-specific intent |

## Cleanup contract

- The resolved base instruction layer should own hard platform-level or
  model-level behavior defaults, not repo-local operating rules.
- The per-turn developer bundle should own mode, runtime wiring, transient
  execution hints, and tool or connector scaffolding.
- The contextual user/environment bundle should own durable machine-wide and
  repo-local behavioral defaults plus serialized environment state.
- Long-form philosophy, manuals, and implementation-specific architecture notes
  should not live in always-injected hot-cache layers unless they are truly
  required by the runtime.
- Task payloads should stay task-specific. They should not become catch-all
  homes for general policy that belongs in a higher durable layer.

## What this means for `AGENTS.md`

`AGENTS.md` is one contributor to the contextual user/environment bundle. It is
not the whole contextual layer, and it is not the resolved base instruction
layer.

That means instruction text appearing "above" a root `AGENTS.md` can come from
any of these places:

- resolved base instructions
- per-turn developer injections
- `config.user_instructions`
- feature-gated contextual additions such as JS REPL guidance or the
  hierarchical AGENTS message

So the right cleanup move is usually to narrow each layer to one job rather
than trying to force everything into `AGENTS.md`.

## Practical rule of thumb

- Put cross-repo behavioral defaults in `AGENTS.md`.
- Put long-form philosophy and manuals in policy/reference docs.
- Put transient runtime or tool scaffolding in developer/session layers.
- Put task-specific instructions in the current user or child-task payload.
