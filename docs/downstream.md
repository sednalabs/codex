# Downstream / Fork Notes

This fork tracks upstream `main` on the local `main` branch and carries additional patches on `carry/main`.
GitHub default branch is `carry/main` so downstream behavior is the repository landing view, while `main` stays pristine.

## Branch Policy

- `main`: fast-forward mirror of upstream `main` (no local commits)
- `carry/main`: upstream + downstream patches (merge-based carry-forward branch)
- do not push feature commits to `origin/main`
- use `git sync-main` to update `main` as an upstream mirror
- use `git sync-carry` to merge `upstream/main` into `carry/main` and push `origin/carry/main`
- avoid force-push on `carry/main` during normal sync; reserve `--force-with-lease` for exceptional repair only
- new feature branches: create from `carry/main` by default
- upstream-only compatibility/test probes: create from `main`, then cherry-pick to `carry/main` if retained downstream

## Divergence Summary

### TUI: Queue slash metadata preparation and recall

Why:
- Preserve slash-command arguments/metadata and make queued recall/edit paths consistent.

User-visible behavior:
- Queued slash commands and queued message drafts are shown in one queue preview.
- `Alt+Up` recalls queued items in strict reverse-chronological order across both entry types.
- `/status` remains immediate (not queued).
- `/model` still opens the picker immediately; selecting a model while busy queues the model switch.

Key files:
- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/tui/src/app_event.rs`
- `codex-rs/tui/src/app.rs`
- `codex-rs/tui/src/bottom_pane/chat_composer.rs`
- `codex-rs/tui/src/bottom_pane/queued_user_messages.rs`
- `codex-rs/tui/src/chatwidget/tests.rs`
- `docs/tui-chat-composer.md`

### TUI: Weekly usage pacing signal + stale handling

Why:
- Show a compact weekly pacing indicator without displaying misleading percentages when snapshot data is stale.

User-visible behavior:
- Weekly status line shows `weekly {remaining:.0}%` as the base value.
- Fresh snapshot adds one compact suffix: `(on pace)`, `(over {n}%)`, or `(under {n}%)`.
- Stale snapshot shows `weekly {remaining:.0}% (stale)` and hides pace percentage.
- `/status` and footer use the same stale predicate helper to keep stale behavior consistent.

Key files:
- `codex-rs/tui/src/status/rate_limits.rs`
- `codex-rs/tui/src/status/mod.rs`
- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/tui/src/bottom_pane/status_line_setup.rs`
- `codex-rs/tui/src/chatwidget/tests.rs`
- `docs/tui-weekly-usage-pacing-status-line.md`

### Core tests: unified_exec race-tolerant completed-process polling (test-only)

- Scope: test-only divergence; no product behavior change.
- Reason: post-`exit` polling can race between final terminal response and process-store removal.
- Reference: upstream issue `#12330`.
- File: `codex-rs/core/src/unified_exec/mod.rs`.
