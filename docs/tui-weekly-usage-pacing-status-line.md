# Weekly Usage + Pacing Status-Line Copy Decision (Work Item 1135)

This note records the copy/layout decision for adding a weekly pacing signal to the TUI status line.
It is the UX decision artifact that unblocks implementation work in `1134`.

## Scope

- In scope: status-line copy/layout for weekly usage + pacing.
- Out of scope: telemetry source changes, protocol changes, or final rendering implementation.

## Current Baseline

- Current weekly limit item renders as `weekly {remaining}%`.
- It does not show pace versus time remaining in the weekly window.
- Footer-level truncation already exists and uses ellipsis when space is constrained.

## Candidate Variants

| Variant | Type | Example | Pros | Cons |
| --- | --- | --- | --- | --- |
| A | Compact | `weekly 44% (over 6%)` | Short, low truncation risk, no sign parsing needed | Slightly less explicit than saying "pace" |
| B | Compact | `weekly 44% · pace -6%` | Very compact, mathematically direct | Sign semantics are easy to misread (`-` means over pace) |
| C | Expressive | `weekly 44% remaining · over pace by 6%` | Maximum clarity in plain language | Long; high truncation pressure in multi-item status lines |
| D | Expressive | `weekly remaining 44% · over by 6% vs time` | Explicit comparison target | Verbose and noisy in narrow widths |

## Decision

Choose **Variant A** as the canonical status-line format because it best balances clarity and width.

### Canonical Copy Rules

- On pace: `weekly {remaining:.0}% (on pace)`
- Over pace: `weekly {remaining:.0}% (over {abs_delta:.0}%)`
- Under pace: `weekly {remaining:.0}% (under {abs_delta:.0}%)`

## Pacing Semantics and Thresholds

Use this exact sign convention in `1134`:

- `usage_remaining_pct = clamp(100 - used_percent, 0..100)`
- `time_remaining_pct = clamp((resets_at - captured_at) / (window_minutes * 60) * 100, 0..100)`
- `pace_delta = usage_remaining_pct - time_remaining_pct`

Classification:

- `on pace` when `abs(pace_delta) <= 3`
- `over` when `pace_delta < -3`
- `under` when `pace_delta > 3`

Display rules:

- Display `abs_delta = abs(pace_delta).round()` as the percentage magnitude.
- Do not display signed percentages in user-facing copy.

## Truncation / Narrow-Width Behavior

- Do not add item-local truncation heuristics in `1134`.
- Rely on the existing footer-level truncation path, which preserves the mode indicator and applies an ellipsis to the left status-line content.
- The selected copy is intentionally compact to reduce truncation frequency in common layouts.

## Implementation Handoff to 1134

`1134` should:

- implement the above copy/state rules in the weekly status-line rendering path,
- preserve existing weekly remaining percentage,
- keep missing-data behavior explicit (render current weekly value without pacing suffix).
