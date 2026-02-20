# Downstream / Fork Notes

This fork tracks upstream `main` on the local `main` branch and carries additional patches on `carry/main`.
GitHub default branch is `carry/main` so downstream behavior is the repository landing view, while `main` stays pristine.

## Branch Policy

- `main`: fast-forward mirror of upstream `main` (no local commits)
- `carry/main`: upstream + downstream patches (merge-based carry-forward; no rebases)
- new feature branches: create from `carry/main` by default
- upstream-only compatibility/test probes: create from `main`, then cherry-pick to `carry/main` if retained downstream

## Divergence Summary

### TUI: Queue slash commands and model selection while busy

Why:
- Allow queueing follow-up actions while a task is running (without retyping / losing intent)

User-visible behavior:
- Some slash commands can be queued while a task is in progress and will run after the task completes.
- `/status` remains immediate (not queued).
- `/model` opens the picker immediately (the picker action is not queued).
- Selecting a model while busy queues the model change to apply when the current task completes.

Key files:
- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/tui/src/app_event.rs`
- `codex-rs/tui/src/app.rs`
- `codex-rs/tui/src/bottom_pane/chat_composer.rs`
- `codex-rs/tui/src/chatwidget/tests.rs`
- `docs/tui-chat-composer.md`

### Core tests: unified_exec race-tolerant assertion (test-only)

- Scope: test-only divergence; no product behavior change.
- Reason: post-`exit` polling can race between final terminal response and process-store removal.
- Reference: upstream issue `#12330`.
- File: `codex-rs/core/src/unified_exec/mod.rs`.
