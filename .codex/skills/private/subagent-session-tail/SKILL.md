---
name: subagent-session-tail
description: Inspect one running or recently completed subagent cheaply by exact `child-thread-id` or by `parent-thread-id` plus `agent_path`, then decide whether to keep waiting, inspect more deeply, or send a bounded follow-up. Use for asks like "is this child still running?", "did that babysitter finish?", or "should I poke this lane instead of polling it?"
---

# Subagent Session Tail

Use this skill to answer one narrow orchestration question without broad log trawling: is the child still active, already done, or quiet enough that the parent should inspect deeper or send one bounded poke?

## Operating Model

- Prefer exact `child-thread-id` from `list_agents` when available.
- Read the helper's top-level status fields first: `session_state`, `terminal_event`, `terminal_reason`, `current_turn_id`, and `last_event_at`.
- Use the tail for recent context, not as the only truth source.
- Use the usage ledger as a heartbeat only when you need to know whether turns, provider calls, or token totals are still moving.
- If the lookup is empty or ambiguous, do not guess from filenames alone. Report that and recommend deeper inspection.
- End with one explicit recommendation: keep waiting, inspect more deeply, or send a bounded follow-up.

## Default Path

1. Prefer the child thread id from `list_agents` when available.
2. If the child thread id is not convenient, use the parent thread id plus the child `agent_path` to find the right session file.
3. Run `scripts/inspect_subagent_tail.py` and keep the output cap small.
4. If the helper reports no matching session file, treat that as a lookup failure. Recommend deeper inspection after checking the exact `child-thread-id` or the `parent-thread-id` plus `agent_path` pair.
5. Read `session_state` first:
   - `active`: the child still appears to be in-flight
   - `completed`: the child reached `task_complete`
   - `interrupted`: the child reached `turn_aborted`
   - `unknown`: no clear terminal marker was found
6. If present, read `terminal_event`, `terminal_reason`, `current_turn_id`, `last_event_at`, and `matched_session_files` before interpreting the tail.
7. Use the session tail for the latest meaningful events and the usage ledger as a secondary heartbeat.

## Decision Map

- `keep waiting`: `session_state` is `active` and the tail or usage heartbeat is still advancing, or the child is clearly inside a long tool call or build.
- `inspect more deeply`: the helper finds no session, `matched_session_files` is greater than `1`, `session_state` is `unknown`, or the terminal marker and recent events do not line up cleanly.
- `send a bounded follow-up`: `session_state` is still `active` or `unknown`, the tail is quiet or stale, usage is flat, and there is no obvious long-running tool call explaining the silence.
- `completed`: the lane reached `task_complete`; say so explicitly before summarizing the tail.
- `interrupted`: the lane reached `turn_aborted`; say so explicitly before summarizing the tail.
- `blocked`: the lane is still `active` or `unknown`, but the latest meaningful event shows an actionable failure, repeated tool error, or explicit action-needed state.
- `idle`: the lane is still `active` or `unknown`, the tail is quiet, and usage is flat without evidence of ongoing tool work.

## Commands

Use the helper script directly:

```bash
scripts/inspect_subagent_tail.py --child-thread-id <thread-id>
scripts/inspect_subagent_tail.py --parent-thread-id <parent-thread-id> --agent-path <agent-path>
scripts/inspect_subagent_tail.py --child-thread-id <thread-id> --tail 12 --days 2
scripts/inspect_subagent_tail.py --child-thread-id <thread-id> --tail 8 --no-usage
```

## Working Rules

- Keep the check cheap: one child, one session file, one small tail.
- Do not grep the entire `~/.codex/sessions` tree unless the narrow lookup failed.
- Do not dump full JSONL lines into the transcript. Summarize only the helper's status fields and recent meaningful records.
- Treat `session_state` and any terminal marker as the primary classification signal.
- If the helper finds no session, say that explicitly and prefer deeper inspection over guessing that the lane is done.
- If parent-plus-agent lookup reports `matched_session_files > 1`, mention that in the handoff so the parent knows the helper selected among multiple candidates.
- Treat the usage ledger as a complementary heartbeat, not the only truth source.
- If `session_state` is `active` and the tail shows active tool work or advancing token counts, prefer waiting.
- If `session_state` is `completed` or `interrupted`, say so explicitly before summarizing the tail.
- If you call the lane `blocked` or `idle`, tie that label to the helper output and recent events instead of guessing from silence alone.
- If the tail is quiet, `session_state` is still `active`, and the ledger is flat, that is the moment to consider a gentle poke.
- If the child is still actively compiling or waiting on a long tool call, do not interrupt just to ask for status.

## Finish Line

Report only:
- which child you inspected
- the helper's top-level status (`session_state` and terminal marker when present)
- the latest meaningful events
- whether the lane looks active, completed, interrupted, blocked, or idle
- whether the usage ledger is still moving
- the next action: keep waiting, inspect more deeply, or send a bounded follow-up
