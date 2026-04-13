# Codex CLI — Prompt Stack & Sub-Agent Executive Reference

This is the short front-door reference for how a Codex CLI turn is assembled in
the current implementation. For the implementation-faithful walkthrough and
source-level caveats, see [Prompt Stack Deep Dive](prompt-stack-deep-dive.md).
For the ownership view of which layer should hold which kind of instruction,
see [Prompt Stack Ownership Matrix](prompt-stack-ownership-matrix.md).

Related pages:

- [Prompt Stack Deep Dive](prompt-stack-deep-dive.md)
- [Prompt Stack Ownership Matrix](prompt-stack-ownership-matrix.md)

## At a glance

- There is no single giant static prompt. Runtime behavior is assembled from a
  resolved base instruction layer, a per-turn developer bundle, a contextual
  user/environment bundle, and the active user or tool task payload.
- `AGENTS.md` is not the whole contextual-user layer. Project docs are loaded
  into `user_instructions`, which may also include config-level user
  instructions, feature-gated JavaScript REPL guidance, and the hierarchical
  AGENTS message before environment context is appended.
- Review mode is not a small appendix on top of the normal base prompt. It
  explicitly replaces `base_instructions` with `REVIEW_PROMPT` from
  `codex-rs/core/review_prompt.md` and applies review-specific restrictions.
- Spawned children do not start blank. They inherit the parent's effective
  config and live runtime-owned state before role layering and task-specific
  overrides are applied.
- The built-in role labels in the current implementation are lighter than they
  may first appear. `default` and `worker` have no embedded role config,
  `explorer` has an effectively empty built-in config file, and the wait lanes
  inherit the parent stack before their built-in role config is layered on.
- Commit attribution and realtime-mode guidance are developer-layer
  instructions. They shape model behavior, but they are not hard
  protocol-enforced guarantees.

## Visual maps

### Normal session message stack

```text
NORMAL TURN

resolved base instructions
    ├─ config.base_instructions
    ├─ session/history base instructions
    └─ model built-ins
         └─ default path: codex-rs/core/prompt.md
                ↓
per-turn developer bundle
    ├─ model-switch instructions
    ├─ sandbox / approval / policy instructions
    ├─ turn_context.developer_instructions
    ├─ memory instructions
    ├─ collaboration-mode instructions
    ├─ realtime start/end instructions
    ├─ personality text
    ├─ apps / connectors
    ├─ skills
    ├─ plugins
    └─ commit attribution instruction
                ↓
contextual user/environment bundle
    ├─ user_instructions
    │    ├─ config.user_instructions
    │    ├─ project-doc blend (AGENTS.md / AGENTS.override.md)
    │    ├─ feature-gated JavaScript REPL instructions
    │    └─ hierarchical AGENTS message
    └─ EnvironmentContext
         ├─ cwd
         ├─ date / timezone
         ├─ network state
         └─ known sub-agents
                ↓
current user input or task payload
```

### Review-mode flow

```text
PARENT SESSION
      ↓ clone config
apply review-time restrictions
    ├─ web_search_mode := Disabled
    ├─ Feature::SpawnCsv := disabled
    ├─ Feature::Collab := disabled
    ├─ approval := AskForApproval::Never
    ├─ base_instructions := REVIEW_PROMPT
    └─ optional model := config.review_model
      ↓
REVIEW CHILD TURN
    = review_prompt.md base layer
    + normal per-turn developer/contextual layers
    + review task payload
```

### Spawned child inheritance path

```text
PARENT TURN
    ├─ effective config
    ├─ resolved base instructions
    └─ live runtime-owned state
         ├─ model / provider
         ├─ reasoning effort / summary
         ├─ developer instructions
         ├─ compact prompt
         ├─ approval policy
         ├─ shell environment policy
         ├─ cwd
         └─ sandbox policy
                ↓
CHILD CONFIG CLONE
    ├─ inherit parent effective config
    ├─ copy live runtime-owned state
    ├─ inherit resolved base instructions from parent session
    ├─ apply role layer
    ├─ re-apply preserved model-selection carry if needed
    ├─ apply explicit spawn overrides
    │    ├─ model
    │    └─ reasoning_effort
    ├─ validate against selected model
    ├─ re-apply runtime overrides and depth-related overrides
    └─ attach child task payload
                ↓
SPAWNED CHILD
```

## Direct answers

### What is the normal prompt stack?

```text
resolved base instructions
+ per-turn developer bundle
+ contextual user/environment bundle
+ current user input or task payload
```

### What is the review child stack?

```text
review_prompt.md as base instructions
+ review capability restrictions
+ normal per-turn developer/contextual bundles
+ review task payload
```

### What does a spawned child get?

```text
parent effective config
+ parent's resolved base instructions
+ copied live runtime state from the parent turn
+ optional explicit spawn overrides
+ optional role layer
+ per-turn developer/contextual bundles
+ child task payload
```

### Where does `AGENTS.md` land?

```text
AGENTS.md / AGENTS.override.md
    ↓
project-doc blend
    ↓
user_instructions
    ↓
contextual user/environment bundle
```

In other words, project docs are part of the contextual user layer, not the
base instruction layer.

## One-screen role reality

```text
ROLE                 BUILT IN   ACTIVE   BUILTIN FILE                  PRACTICAL EFFECT
default              yes        yes      none                          inheritance only
explorer             yes        yes      explorer.toml (effectively    metadata + inheritance
                                          empty)
worker               yes        yes      none                          metadata + inheritance + task
awaiter              yes        yes      awaiter.toml                  passive wait config + inheritance
terminal-babysitter  yes        yes      terminal-babysitter.toml      monitored wait config + inheritance
scout                no         n/a      n/a                           not built in
spark                no         n/a      n/a                           not built in; use worker + overrides
```

## What that means in practice

- `default` is the fallback role and does not add its own built-in role config
  file in the current implementation.
- `explorer` matters at the orchestration and metadata level here, but its
  built-in config file is effectively empty.
- `worker` matters semantically, but in the current implementation it is not
  backed by a large dedicated built-in role prompt/config file.
- `awaiter` and `terminal-babysitter` are first-class built-in roles in the
  current implementation and do add embedded wait-lane config, but they still
  inherit the parent session state before that role config is layered on.
- `scout` and `spark` are not first-class built-in roles in the current
  implementation.
- The explicit child task payload usually does more practical steering than the
  built-in role file unless the role is one of the wait lanes.

## Source map

The main files to re-check when this behavior changes are:

- `codex-rs/core/src/codex.rs`
- `codex-rs/core/src/project_doc.rs`
- `codex-rs/core/hierarchical_agents_message.md`
- `codex-rs/core/src/tasks/review.rs`
- `codex-rs/core/src/tools/handlers/multi_agents_common.rs`
- `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`
- `codex-rs/core/src/agent/role.rs`
- `codex-rs/core/templates/collaboration_mode/default.md`
- `codex-rs/core/templates/collaboration_mode/plan.md`
- `codex-rs/core/src/commit_attribution.rs`
- `codex-rs/core/src/context_manager/updates.rs`
- `codex-rs/core/prompt.md`
- `codex-rs/core/review_prompt.md`
- `codex-rs/protocol/src/prompts/realtime/realtime_start.md`
- `codex-rs/protocol/src/prompts/realtime/realtime_end.md`

## Caveats

This page reflects the current implementation. Refresh it whenever prompt
assembly, project-doc composition, role registration, review flow, or realtime
prompt plumbing changes.

## Bottom line

The prompt stack is assembled, not static. Review mode replaces the normal base
prompt with a dedicated rubric. Spawned children inherit the parent's resolved
prompt/config/runtime state before role layering. And in the current
implementation, project docs such as `AGENTS.md` live inside the contextual user
bundle rather than standing in for the base instruction layer.
