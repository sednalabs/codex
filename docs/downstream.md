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

### TUI: Weekly usage pacing signal + stale handling

Why:
- Show a compact weekly pacing indicator without displaying misleading percentages when snapshot data is stale.

User-visible behavior:
- Weekly status line shows `weekly {remaining:.0}%` as the base value.
- Fresh snapshot adds one compact suffix: `(on pace)`, `(over {n}%)`, or `(under {n}%)`.
- Stale snapshot shows `weekly {remaining:.0}% (stale)` and hides pace percentage.
- `/status` and footer use the same stale predicate helper to keep stale behavior consistent.

### TUI: Interrupted-turn queue handling and queued model ordering

Why:
- Keep `Esc` interrupts from auto-submitting queued turns while still applying queued model switches promptly.
- Avoid stale model/effort on the next queued command when interrupt cleanup overlaps with MCP startup running-state.

User-visible behavior:
- On interrupt, queued user drafts are restored to the composer; non-model queued slash commands remain queued.
- Queued model selections are applied immediately during interrupt cleanup.
- Queued `/clear` remains queued while a task is running and is not executed during interrupt cleanup.
- Inline queued slash command arguments preserve expanded pending-paste payload content.

### Core: MCP forced approvals still participate in session remember keys

Why:
- Preserve Auto-mode approval-key caching even when a call is force-prompted.

User-visible behavior:
- Auto approval mode continues to use per-session remembered approvals for matching MCP tool calls, including force-prompted calls.
- Repeated calls can still be approved from the current session memory instead of always re-prompting.

### Core tests: unified_exec race-tolerant completed-process polling (test-only)

Why:
- Post-`exit` polling can race between final terminal response and process-store removal in test runs.

User-visible behavior:
- No product behavior change; this divergence only makes downstream core tests more tolerant of completion/polling races.
