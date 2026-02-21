# Weekly Usage + Pacing Status-Line Copy Decision (Work Item 1135)

This note records the copy/layout decision for adding a weekly pacing signal to the TUI status line.
It is the UX decision artifact that unblocks implementation work in `1134`.

## Scope

- In scope: status-line copy/layout for weekly usage + pacing.
- Out of scope: telemetry source changes, protocol changes, or final rendering implementation.

## Current Behavior

- Weekly limit item always starts from `weekly {remaining:.0}%`.
- When pacing inputs are available and the snapshot is fresh, the footer appends a compact suffix:
  - `(on pace)`, `(over {n}%)`, or `(under {n}%)`.
- When the snapshot is stale, footer copy shows `(stale)` and intentionally hides pace percentage.
- Footer-level truncation still applies globally and uses ellipsis when space is constrained.

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

- Base weekly value: `weekly {remaining:.0}%`
- Stale snapshot: `weekly {remaining:.0}% (stale)` (takes precedence over pace detail)
- Fresh + on pace: `weekly {remaining:.0}% (on pace)`
- Fresh + over pace: `weekly {remaining:.0}% (over {abs_delta:.0}%)`
- Fresh + under pace: `weekly {remaining:.0}% (under {abs_delta:.0}%)`
- Missing/invalid pacing data: render base weekly value with no pacing suffix.
- Weekly stale state is derived from the codex weekly snapshot itself (not from unrelated limit buckets).

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

- Display `abs_delta` using upward rounding (`ceil`) for non-on-pace states so labels do not understate out-of-band deltas.
- Do not display signed percentages in user-facing copy.
- If `resets_at - captured_at` cannot be represented safely, treat pacing as unavailable (base weekly value only).

## Stale Snapshot Semantics

- Staleness uses shared helper `is_snapshot_stale(captured_at, now)` from `status/rate_limits.rs`.
- A snapshot is stale only when age is strictly greater than the threshold (`> 15 minutes`).
- Future capture timestamps are tolerated only within a small skew window (`<= 60s`); beyond that they are treated as stale/skewed.
- Exact boundary behavior:
  - exactly `15m` old is **not** stale,
  - `15m + 1s` old **is** stale.
- Future-skew boundary behavior:
  - exactly `+60s` ahead is **not** stale,
  - `+61s` ahead **is** stale.
- The same stale predicate is reused in `/status` and footer weekly signal logic to avoid drift.

## Truncation / Narrow-Width Behavior

- Do not add item-local truncation heuristics in `1134`.
- Rely on the existing footer-level truncation path, which preserves the mode indicator and applies an ellipsis to the left status-line content.
- The selected copy is intentionally compact to reduce truncation frequency in common layouts.

## Maintenance Notes

- Weekly signal internals now use a typed `WeeklyPacingSignal` representation in `chatwidget`, with suffix rendering centralized in one method.
- Non-codex limit snapshots are pruned after a retention window so stale orphan buckets do not linger indefinitely.
- Tests cover:
  - epsilon boundaries for on-pace classification,
  - stale threshold exact-edge behavior,
  - stale precedence over missing pacing inputs,
  - overflow-safe fallback when `seconds_remaining` cannot be computed.
