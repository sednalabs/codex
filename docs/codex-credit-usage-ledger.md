# Codex credit usage ledger

The usage database preserves raw provider-call token counts and exposes
rate-card estimates through these views:

- usage_provider_call_credit_estimates
- usage_thread_credit_summary
- usage_turn_credit_summary

The call view distinguishes routing intent from execution evidence:

- requested_model and requested_service_tier are request metadata;
- actual_model_used and actual_service_tier are runtime execution fields;
- fast_mode_requested and fast_mode_used remain separate from the
  provider-facing priority service tier.

Credit estimates use the actual model, actual tier, started_at, and the
versioned rows in usage_codex_credit_rates. The rate table stores the source
URL and observation timestamp for every rate regime. Effective intervals use
the half-open rule effective_from <= started_at < effective_to.

The estimate is calculated from uncached input, cached input, and output
components. total_tokens is never used as the pricing basis. Cache-write
tokens remain visible as diagnostic evidence; when they are non-zero, pricing
is marked token_breakdown_incomplete rather than guessing whether they are
already included in the uncached-input component.

Missing or ambiguous rate coverage produces NULL credit estimates and an
explicit pricing_status. A thread summary sets partial = true whenever it
contains any unpriced call; unpriced calls are not treated as zero-cost calls.

The initial seeded token-based rates are sourced from the [Codex rate
card](https://help.openai.com/en/articles/20001106-codex-rate-card), with
Fast/API-priority regimes sourced from the [Codex speed
documentation](https://learn.chatgpt.com/docs/agent-configuration/speed).
Provider-reported credits, if later populated, remain separate from local
rate-card estimates.
