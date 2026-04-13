# Prompt Stack Deep Dive

This page is the implementation-faithful companion to
[Codex CLI — Prompt Stack & Sub-Agent Executive Reference](prompt-stack.md). It
describes the current implementation and should be refreshed when prompt
assembly, project-doc composition, review flow, or built-in role
registration changes.

For the concise ownership breakdown of which layer should own which kind of
instruction, see
[Prompt Stack Ownership Matrix](prompt-stack-ownership-matrix.md).

Related pages:

- [Codex CLI — Prompt Stack & Sub-Agent Executive Reference](prompt-stack.md)
- [Prompt Stack Ownership Matrix](prompt-stack-ownership-matrix.md)

## 1. Normal session prompt waterfall

The normal session prompt stack is built in layers, not loaded as one monolithic
string.

### 1.1 Base instructions

Base instructions are resolved in `codex-rs/core/src/codex.rs` in this order:

1. `config.base_instructions`
2. `conversation_history.get_base_instructions()` from resumed/forked session
   metadata
3. the selected model's built-in instructions

That priority order is documented directly in `codex.rs`.

Absent an override, the durable default base prompt for a normal session comes
from `codex-rs/core/prompt.md`, referenced via `BASE_INSTRUCTIONS` in
`codex-rs/core/src/models_manager/model_info.rs`.

Some models can wrap those base instructions with a
`model_messages.instructions_template` when `get_model_instructions(...)` is
called. In those cases the effective base layer is still the resolved base
instructions, but it may be wrapped before being sent.

### 1.2 What becomes `turn_context.user_instructions`

The contextual `user_instructions` value is itself assembled in
`codex-rs/core/src/project_doc.rs`.

`get_user_instructions(config)` composes:

1. `config.user_instructions`, if present
2. the discovered project-doc blend
3. feature-gated JavaScript REPL instructions, when `Feature::JsRepl` is enabled
4. the hierarchical AGENTS message, when `Feature::ChildAgentsMd` is enabled

The project-doc blend is the concatenation of project-level instruction files
found from project root down to the current working directory. Discovery uses
`AGENTS.md` by default and prefers `AGENTS.override.md` over `AGENTS.md` at a
given directory level.

This is the key reason a live prompt can contain instruction text "above" a root
`AGENTS.md` even when that text is not present in the repo's own project docs:
`user_instructions` may include config text, feature-gated JS REPL guidance, and
the hierarchical AGENTS message in addition to the project-doc blend.

### 1.3 Per-turn developer bundle

After the base instructions are resolved, the code assembles a per-turn
developer bundle in `codex-rs/core/src/codex.rs`.

In the current implementation, the developer sections are built in this general
order:

- model-switch instructions when the active model changed
- sandbox / approval / policy guidance
- `turn_context.developer_instructions`
- memory-tool instructions, when enabled
- collaboration-mode developer instructions
- realtime start/end developer instructions
- personality text when it is not already baked into the model instructions
- apps / connectors section
- skills section
- plugin capability section
- commit attribution instruction

These fragments are aggregated into one developer message via
`build_developer_update_item(...)`.

### 1.4 Contextual user / environment bundle

Later in the same path, Codex assembles a contextual user message. In the
current implementation this includes:

- serialized `UserInstructions` text built from `turn_context.user_instructions`
- serialized `EnvironmentContext`
- the formatted known-subagents inventory injected into that environment context

This is emitted as a user-role message via `build_contextual_user_message(...)`.

### 1.5 Clean mental model

```text
resolved base instructions
+ developer bundle
+ contextual user/environment bundle
+ current user input or task payload
```

So the normal session prompt is assembled, not monolithic.

## 2. What counts as the system prompt here

The most accurate practical answer is:

The normal-session "system prompt equivalent" is the resolved base instruction
layer.

That usually means one of:

- `codex-rs/core/prompt.md`, when nothing overrides it
- `config.base_instructions`, when config overrides it
- session-meta / rollout-provided base instructions, when the thread resumes
  with stored base instructions
- a model-specific wrapper around the same base instructions, for models that
  use instruction templates

### What is not the base instruction layer

These are important, but they are not the base layer itself:

- project-doc / `AGENTS.md` content
- feature-gated JavaScript REPL guidance
- hierarchical AGENTS guidance
- sandbox / approval guidance
- collaboration-mode guidance
- realtime mode transitions
- apps / connectors / skills / plugins inventory
- commit attribution guidance
- environment context and known sub-agents
- the explicit user message or task payload

Many of these still sit above the active user task in the final prompt history,
but they are layered separately from the base instruction layer.

## 3. Where `AGENTS.md` sits in the stack

If you are reasoning about "system prompts above root `AGENTS.md`", the key
clarification is:

`AGENTS.md` is not the base prompt.

At the code path level:

- project docs are discovered by `read_project_docs(...)`
- they are merged into `get_user_instructions(...)`
- that string becomes `turn_context.user_instructions`
- later, Codex serializes it into the contextual user bundle

So a root `AGENTS.md` is one input into the contextual user layer, not the base
instruction layer and not the whole prompt stack.

## 4. Review mode prompt override

Review mode is implemented in `codex-rs/core/src/tasks/review.rs`.

When a review turn starts, the code clones the current config into a sub-agent
config and then applies explicit review-time overrides.

The important review-time changes are:

- `web_search_mode` is forced to `Disabled`
- `Feature::SpawnCsv` is disabled
- `Feature::Collab` is disabled
- approval policy is forced to `AskForApproval::Never`
- `base_instructions` is explicitly set to `crate::REVIEW_PROMPT.to_string()`
- the model is set to `config.review_model` when configured, otherwise it reuses
  the current model

The review rubric itself is defined in `codex-rs/core/src/client_common.rs` as:

```rust
pub const REVIEW_PROMPT: &str = include_str!("../review_prompt.md");
```

So the effective review-child base prompt is `codex-rs/core/review_prompt.md`.

### Clean mental model for review mode

```text
normal parent config clone
+ review restrictions
+ base_instructions := review_prompt.md
+ approval policy := never
+ optional review-model override
```

Then the normal per-turn message assembly continues on top of that.

## 5. Spawned sub-agent prompt and config waterfall

Spawned children are built through
`codex-rs/core/src/tools/handlers/multi_agents_common.rs` and
`codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`.

The critical point is that a spawned child starts from the parent's effective
config and live turn state, not from a tiny standalone role prompt.

### 5.1 What is inherited from the parent session

`build_agent_spawn_config(...)` starts from the parent's effective config and
sets:

- `config.base_instructions = Some(base_instructions.text.clone())`

where `base_instructions` comes from `session.get_base_instructions().await`.

That means the child inherits the parent's currently resolved base instructions,
not just the static default file.

### 5.2 What is copied from the live parent turn

`build_agent_shared_config(...)` and
`apply_spawn_agent_runtime_overrides(...)` copy live turn state such as:

- model slug
- model provider
- model reasoning effort
- model reasoning summary
- developer instructions
- compact prompt
- approval policy
- shell environment policy
- cwd
- sandbox policy
- file-system sandbox policy
- network sandbox policy

This matters because those values are runtime-owned and may not match stale
on-disk config.

### 5.3 What can be explicitly overridden at spawn time

The spawn path may also apply explicit `spawn_agent(...)` overrides for:

- `model`
- `reasoning_effort`

If a requested model is supplied, the code resolves it against the available
model list, updates the child config, and validates the requested reasoning
effort against the model's supported reasoning levels.

### 5.4 Role layering

In the current implementation, spawn-time role layering happens before the
requested model/reasoning overrides are applied.

The current order is:

1. build the parent-derived spawn config
2. apply the role layer with `apply_role_to_spawn_config(...)`
3. re-apply any preserved model-selection carry
4. apply requested `model` / `reasoning_effort` overrides
5. validate or reset reasoning settings against the final selected model
6. re-apply runtime overrides
7. apply depth-related overrides
8. spawn the child

That ordering matters. If a doc or audit claims that requested model overrides
happen before role application, it is describing an older implementation shape
rather than the current one.

### 5.5 Final spawn order

```text
clone parent effective config
+ copy live turn-owned runtime state
+ inherit resolved base instructions from parent session
+ apply role layer
+ re-apply preserved model-selection carry where appropriate
+ apply explicit spawn_agent model / reasoning overrides
+ validate reasoning settings against the selected model
+ re-apply runtime overrides and depth-related overrides
+ spawn child
```

The practical result is that a spawned child behaves much more like "the current
session, plus role/task overlays" than like "a blank worker prompt".

## 6. Built-in role reality in the current implementation

The built-in role registry for the current implementation lives in
`codex-rs/core/src/agent/role.rs`.

The relevant built-ins here are:

- `default`
- `explorer`
- `worker`
- `awaiter`
- `terminal-babysitter`

### 6.1 What is light vs substantial

- `default` is built in but has no dedicated config file.
- `worker` is built in but has no dedicated config file.
- `explorer` uses `explorer.toml`, which is effectively just a comment and does
  not materially own model selection.
- `awaiter` uses `awaiter.toml`, which contains real wait-lane instructions.
- `terminal-babysitter` uses `terminal-babysitter.toml`, which contains real
  monitored-wait instructions.

### 6.2 What that means in practice

- `default` child = parent stack + child task payload
- `explorer` child = parent stack + lightweight role metadata + child task
  payload
- `worker` child = parent stack + ownership-oriented role metadata + child task
  payload
- `awaiter` child = parent stack + embedded passive-wait config + child task
  payload
- `terminal-babysitter` child = parent stack + embedded monitored-wait config +
  child task payload

`scout` and `spark` are still not first-class built-in roles in the current
implementation. Spark-like behavior remains an orchestration convention built
from a `worker` plus explicit spawn overrides.

## 7. Commit attribution instruction

Commit attribution is implemented in
`codex-rs/core/src/commit_attribution.rs` and injected from
`codex-rs/core/src/codex.rs`.

### 7.1 When it is injected

The instruction is considered when `Feature::CodexGitCommit` is enabled.

If that feature is enabled, `commit_message_trailer_instruction(...)` is called
with `turn_context.config.commit_attribution.as_deref()`.

### 7.2 Exact behavior

The helper builds a trailer of the form:

```text
Co-authored-by: <name and email>
```

If config provides no explicit attribution value, it defaults to:

```text
Codex <noreply@openai.com>
```

If config provides a blank string, the helper treats that as disabled and
returns no instruction.

The injected instruction tells the model:

- when it writes or edits a git commit message, the message must end with the
  required trailer exactly once
- keep existing trailers
- append the trailer if missing
- do not duplicate it
- keep one blank line between the commit body and the trailer block

### 7.3 What this means architecturally

This is behavioral guidance, not hard enforcement. It shapes commit-message
behavior, but the model could still fail to comply.

## 8. Realtime transition instructions

Realtime mode transition instructions are built in
`codex-rs/core/src/context_manager/updates.rs`.

### 8.1 When they appear

The builder compares previous and next realtime state.

The important cases are:

- inactive to active: inject a realtime-start developer instruction
- active to inactive: inject a realtime-end developer instruction
- previous context missing but previous turn settings say realtime was active
  and the next turn is inactive: still inject the realtime-end instruction

### 8.2 Default message source

For realtime start, the instruction source is:

- `turn_context.config.experimental_realtime_start_instructions`, if configured
- otherwise the default prompt at
  `codex-rs/protocol/src/prompts/realtime/realtime_start.md`

For realtime end, the default text comes from:

- `codex-rs/protocol/src/prompts/realtime/realtime_end.md`

### 8.3 What the default realtime instruction says

The default realtime-start instruction says, in substance:

- realtime conversation has started
- the model is acting as a backend executor behind an intermediary
- the user is not speaking directly to this model instance
- transcript-style text may be unpunctuated or contain recognition errors
- responses should stay concise and action-oriented to avoid adding latency

The realtime-end instruction says, in substance:

- realtime conversation has ended
- subsequent user input returns to typed text
- the model should stop assuming recognition errors once realtime has ended

### 8.4 What this means architecturally

This is not a permanent mode baked into the base prompt. It is a dynamic
developer-layer mode-transition hint injected when realtime state changes.

## 9. Misconceptions this page is meant to correct

- There is no single giant static prompt.
- `AGENTS.md` is not the base prompt and is not the whole contextual-user layer.
- Project-doc text can appear alongside feature-gated JS REPL guidance and the
  hierarchical AGENTS message inside `user_instructions`.
- Review mode is not just the normal prompt plus a short review appendix; it
  swaps the base prompt layer.
- Spawned children inherit the parent's resolved prompt/config/runtime state
  before role/task overlays.
- `default` and `worker` are not backed by large dedicated built-in config files
  in the current implementation.
- `explorer` is still intentionally light in the current implementation.
- `awaiter` and `terminal-babysitter` are now real built-in wait roles in the
  current implementation.
- `scout` and `spark` are not first-class built-in roles here.
- Commit attribution and realtime-mode behavior are instructions, not
  protocol-enforced guarantees.

## 10. Operational implications

- If you want to understand text that appears "above" a root `AGENTS.md`, check
  `project_doc.rs` before assuming it came from the project docs themselves.
- If you want a Spark-like worker today, the implementation-faithful pattern is:
  spawn a `worker`, then override `model` and/or `reasoning_effort`.
- If you want stronger commit attribution guarantees, move from
  instruction-layer guidance to enforced commit contracts or post-action
  validation.
- If you want stronger realtime guarantees, move from mode-transition
  instructions to structured event or telemetry contracts.
- When auditing future branches, the highest-value files to re-check are
  `codex.rs`, `project_doc.rs`, `review.rs`, `role.rs`, the built-in role files,
  and the realtime prompt/update plumbing.

## 11. Audit scope and source map

This page reflects the current implementation at the time it was written.

That matters because prompt assembly, project-doc composition, review flow,
spawn ordering, and built-in role registration may change over time. In
particular:

- `explorer.toml` could gain real content later
- the wait-role registry could change again
- review restrictions or review-model selection may evolve
- new roles such as `scout` or `spark` could be added later as user-defined or
  built-in roles
- `project_doc.rs` could change what is appended into `user_instructions`

The key files for this behavior in the current implementation are:

- `codex-rs/core/src/codex.rs`
- `codex-rs/core/src/project_doc.rs`
- `codex-rs/core/hierarchical_agents_message.md`
- `codex-rs/core/src/models_manager/model_info.rs`
- `codex-rs/core/src/tasks/review.rs`
- `codex-rs/core/src/client_common.rs`
- `codex-rs/core/review_prompt.md`
- `codex-rs/core/prompt.md`
- `codex-rs/core/templates/collaboration_mode/default.md`
- `codex-rs/core/templates/collaboration_mode/plan.md`
- `codex-rs/core/src/tools/handlers/multi_agents_common.rs`
- `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`
- `codex-rs/core/src/agent/role.rs`
- `codex-rs/core/src/agent/builtins/explorer.toml`
- `codex-rs/core/src/agent/builtins/awaiter.toml`
- `codex-rs/core/src/agent/builtins/terminal-babysitter.toml`
- `codex-rs/core/src/commit_attribution.rs`
- `codex-rs/core/src/context_manager/updates.rs`
- `codex-rs/protocol/src/prompts/realtime/realtime_start.md`
- `codex-rs/protocol/src/prompts/realtime/realtime_end.md`

## 12. Bottom line

The prompt stack is assembled, not static. Review mode replaces the normal base
prompt with a dedicated rubric. Spawned children inherit the parent's resolved
prompt/config/runtime state before role layering. And in the current
implementation, a root `AGENTS.md` lives inside the composed
`user_instructions` bundle, which may also include config-provided instructions
and feature-gated injected guidance before environment context is added.
