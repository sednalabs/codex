# Terminal Wait Primitive Observability

Codex Sedna deliberately exposes terminal wait primitives in the TUI transcript for unified exec work. This is downstream operator observability, not an accidental debug leak.

## Purpose

Long-running local commands can look identical from the outside: the agent may be actively working, waiting on a process, polling an interactive terminal, or burning turns by repeatedly asking for the same state. Sedna keeps the primitive visible so an operator can distinguish those cases while supervising local agent work.

The visible labels are intentionally literal:

- `exec_command(wait_until_terminal=true)` means the original command invocation requested a blocking terminal wait.
- `write_stdin(wait_until_terminal=true)` means a follow-up stdin call requested a blocking wait and must not include non-empty input.
- `write_stdin(empty stdin poll)` means the agent polled a background terminal by issuing an empty stdin interaction rather than using the blocking wait primitive.

That last case is intentionally visible because it is the one operators most often need to challenge. Polling is sometimes appropriate for genuinely interactive processes, prompts, REPLs, dev servers, or commands where intermediate output changes the next action. For ordinary build, test, lint, release, or validation commands, the preferred downstream behavior is usually a blocking wait until terminal state or timeout.

## Relationship to blocking waits

This observability surface is paired with the downstream `wait_until_terminal` carry. The wait primitive gives agents a way to let the tool/runtime layer absorb long waits instead of simulating a scheduler in the chat transcript. The transcript label then makes the chosen execution strategy auditable.

The intended operator reading is:

- blocking wait primitive: the agent is letting the runtime wait on a real process state transition;
- empty stdin poll: the agent is manually checking a still-running process and may be wasting turns unless intermediate output is needed;
- repeated empty stdin polls: possible polling-loop or token-waste smell, especially for non-interactive compile/test flows.

## Boundary

Primitive labels are not model reasoning and should not be treated as semantic task progress. They describe the terminal-control primitive used by the tool layer so humans and companion clients can reason about orchestration efficiency.

This is a downstream divergence because upstream-style UI polish generally hides these low-level actions behind generic waiting or running indicators. Sedna keeps the raw primitive name visible on purpose for expert/operator workflows.

## Guardrails

The surface is covered by the unified exec blocking-wait and TUI history-cell lanes. Relevant checks include:

- `exec_command_wait_until_terminal_defers_provider_resume_until_exit`
- `write_stdin_wait_until_terminal_blocks_until_exit`
- `unified_exec_tools_include_wait_until_terminal_contract_fields`
- `unified_exec_wait_fields_are_capability_gated`
- `exec_command_tool_matches_expected_spec`
- `write_stdin_tool_matches_expected_spec`
- TUI history-cell rendering checks for `WaitPrimitiveCell`

When changing this area, preserve the distinction between blocking waits and empty-stdin polling. Do not collapse the labels into a generic “waiting” message unless a replacement observability surface exposes the same distinction.
