-- Rebuild the credit-estimate views after upgrading a database whose applied
-- 0005 migration predates the P2C success-gating repair. This changes only
-- read-time classification; usage rows and their provider provenance remain
-- untouched. The provider-reported value remains the first pricing source.

DROP VIEW IF EXISTS usage_thread_credit_summary;
DROP VIEW IF EXISTS usage_turn_credit_summary;
DROP VIEW IF EXISTS usage_provider_call_credit_estimates;

CREATE VIEW usage_provider_call_credit_estimates AS
WITH calls AS (
    SELECT
        p.*,
        COALESCE(NULLIF(lower(p.final_model), ''), NULLIF(lower(p.actual_model_used), ''))
            AS pricing_model,
        CASE p.fast_mode_used
            WHEN 1 THEN 'fast'
            WHEN 0 THEN 'standard'
        END AS speed_mode
    FROM usage_provider_calls AS p
),
policy_matches AS (
    SELECT
        p.provider_call_id,
        COUNT(c.policy_id) AS matching_policy_count,
        MIN(c.rate_card_kind) AS rate_card_kind
    FROM calls AS p
    LEFT JOIN usage_codex_credit_policies AS c
      ON c.provider = lower(p.provider)
     AND c.billing_surface = lower(p.billing_surface)
     AND c.account_plan = lower(p.account_plan)
     AND p.started_at >= c.effective_from
     AND (c.effective_to IS NULL OR p.started_at < c.effective_to)
    GROUP BY p.provider_call_id
),
rate_matches AS (
    SELECT
        p.provider_call_id,
        COUNT(
            CASE WHEN p.actual_service_tier_source IS NOT NULL
                       AND r.service_tier = lower(p.actual_service_tier)
                       AND r.speed_mode = p.speed_mode
                 THEN r.rate_id
            END
        ) AS matching_rate_count,
        MIN(
            CASE WHEN p.actual_service_tier_source IS NOT NULL
                       AND r.service_tier = lower(p.actual_service_tier)
                       AND r.speed_mode = p.speed_mode
                 THEN r.rate_id
            END
        ) AS rate_id,
        COUNT(r.rate_id) AS matching_model_rate_count
    FROM calls AS p
    JOIN policy_matches AS c ON c.provider_call_id = p.provider_call_id
    LEFT JOIN usage_codex_credit_rates AS r
      ON c.matching_policy_count = 1
     AND r.provider = lower(p.provider)
     AND r.rate_card_kind = c.rate_card_kind
     AND r.model = p.pricing_model
     AND p.started_at >= r.effective_from
     AND (r.effective_to IS NULL OR p.started_at < r.effective_to)
    GROUP BY p.provider_call_id
),
priced AS (
    SELECT
        p.*,
        c.matching_policy_count,
        c.rate_card_kind AS selected_rate_card_kind,
        m.matching_rate_count,
        m.matching_model_rate_count,
        r.rate_id,
        r.rate_card_kind,
        r.effective_from AS rate_effective_from,
        r.effective_to AS rate_effective_to,
        r.source_url AS rate_source_url,
        r.source_observed_at AS rate_source_observed_at,
        CASE WHEN p.status = 'ok'
                  AND c.matching_policy_count = 1
                  AND m.matching_rate_count = 1
                  AND p.input_tokens_uncached IS NOT NULL
                  AND p.input_tokens_cached IS NOT NULL
                  AND p.output_tokens IS NOT NULL
                  AND COALESCE(p.input_tokens_cache_write, 0) = 0
             THEN p.input_tokens_uncached * r.credits_per_1m_uncached_input / 1000000.0
        END AS uncached_input_credits,
        CASE WHEN p.status = 'ok'
                  AND c.matching_policy_count = 1
                  AND m.matching_rate_count = 1
                  AND p.input_tokens_uncached IS NOT NULL
                  AND p.input_tokens_cached IS NOT NULL
                  AND p.output_tokens IS NOT NULL
                  AND COALESCE(p.input_tokens_cache_write, 0) = 0
             THEN p.input_tokens_cached * r.credits_per_1m_cached_input / 1000000.0
        END AS cached_input_credits,
        CASE WHEN p.status = 'ok'
                  AND c.matching_policy_count = 1
                  AND m.matching_rate_count = 1
                  AND p.input_tokens_uncached IS NOT NULL
                  AND p.input_tokens_cached IS NOT NULL
                  AND p.output_tokens IS NOT NULL
                  AND COALESCE(p.input_tokens_cache_write, 0) = 0
             THEN p.output_tokens * r.credits_per_1m_output / 1000000.0
        END AS output_credits
    FROM calls AS p
    JOIN policy_matches AS c ON c.provider_call_id = p.provider_call_id
    JOIN rate_matches AS m ON m.provider_call_id = p.provider_call_id
    LEFT JOIN usage_codex_credit_rates AS r
      ON r.rate_id = m.rate_id AND m.matching_rate_count = 1
)
SELECT
    provider_call_id,
    thread_id,
    turn_id,
    spawn_request_id,
    started_at,
    completed_at,
    provider,
    billing_surface,
    account_plan,
    requested_model,
    actual_model_used,
    final_model,
    pricing_model,
    requested_service_tier,
    actual_service_tier,
    actual_service_tier_source,
    fast_mode_requested,
    fast_mode_used,
    input_tokens_uncached,
    input_tokens_cached,
    input_tokens_cache_write,
    output_tokens,
    total_tokens,
    provider_reported_credits,
    uncached_input_credits,
    cached_input_credits,
    output_credits,
    uncached_input_credits + cached_input_credits + output_credits
        AS rate_card_estimated_total_credits,
    COALESCE(
        provider_reported_credits,
        uncached_input_credits + cached_input_credits + output_credits
    ) AS estimated_total_credits,
    rate_id,
    rate_card_kind,
    rate_effective_from,
    rate_effective_to,
    rate_source_url,
    rate_source_observed_at,
    selected_rate_card_kind,
    CASE
        WHEN provider_reported_credits IS NOT NULL THEN 'provider_reported'
        WHEN status IS NULL
          OR status <> 'ok'
          OR total_tokens IS NULL
          OR input_tokens_uncached IS NULL
          OR input_tokens_cached IS NULL
          OR output_tokens IS NULL THEN 'provider_usage_missing'
        WHEN pricing_model IS NULL THEN 'actual_model_missing'
        WHEN actual_service_tier IS NULL OR actual_service_tier_source IS NULL
            THEN 'actual_tier_missing'
        WHEN fast_mode_used IS NULL THEN 'fast_rate_unknown'
        WHEN COALESCE(input_tokens_cache_write, 0) > 0 THEN 'token_breakdown_incomplete'
        WHEN matching_policy_count > 1 OR matching_rate_count > 1 THEN 'ambiguous_rate'
        WHEN selected_rate_card_kind LIKE 'legacy%' THEN 'legacy_rate_card'
        WHEN matching_policy_count = 0 THEN 'rate_card_unknown'
        WHEN matching_rate_count = 0 AND fast_mode_used = 1 THEN 'fast_rate_unknown'
        WHEN matching_model_rate_count = 0 THEN 'model_rate_missing'
        WHEN matching_rate_count = 0 THEN 'tier_rate_missing'
        ELSE 'priced_estimate'
    END AS pricing_status,
    CASE
        WHEN provider_reported_credits IS NOT NULL
            THEN 'provider credits take precedence; any rate-card estimate remains separately visible'
        WHEN COALESCE(input_tokens_cache_write, 0) > 0
            THEN 'cache-write tokens retained diagnostically; overlap with uncached input is not assumed'
        WHEN matching_policy_count = 0
            THEN 'no unambiguous credit policy matched provider, billing surface, plan, and timestamp'
        WHEN matching_rate_count > 1
            THEN 'multiple effective rate rows matched'
        WHEN matching_rate_count = 0
            THEN 'no rate matched actual model, actual tier, speed mode, and timestamp'
    END AS pricing_notes,
    CASE
        WHEN provider_reported_credits IS NOT NULL THEN 'provider_reported'
        WHEN status = 'ok'
         AND matching_policy_count = 1
         AND matching_rate_count = 1
         AND input_tokens_uncached IS NOT NULL
         AND input_tokens_cached IS NOT NULL
         AND output_tokens IS NOT NULL
         AND COALESCE(input_tokens_cache_write, 0) = 0 THEN 'rate_card_estimate'
    END AS credit_source
FROM priced;

CREATE VIEW usage_thread_credit_summary AS
WITH grouped AS (
    SELECT
        c.thread_id,
        COUNT(*) AS provider_call_count,
        SUM(c.pricing_status IN ('priced_estimate', 'provider_reported')) AS priced_call_count,
        SUM(c.pricing_status NOT IN ('priced_estimate', 'provider_reported')) AS unpriced_call_count,
        MIN(c.started_at) AS first_call_at,
        MAX(c.started_at) AS last_call_at,
        SUM(c.input_tokens_uncached) AS uncached_input_tokens,
        SUM(c.input_tokens_cached) AS cached_input_tokens,
        SUM(c.input_tokens_cache_write) AS cache_write_input_tokens,
        SUM(c.output_tokens) AS output_tokens,
        SUM(c.total_tokens) AS total_tokens,
        SUM(c.uncached_input_credits) AS uncached_input_credits,
        SUM(c.cached_input_credits) AS cached_input_credits,
        SUM(c.output_credits) AS output_credits,
        SUM(c.estimated_total_credits) AS priced_credits_total,
        group_concat(DISTINCT c.pricing_model) AS models_used,
        group_concat(DISTINCT c.actual_service_tier) AS service_tiers_used,
        group_concat(DISTINCT c.rate_card_kind) AS rate_card_kinds
    FROM usage_provider_call_credit_estimates AS c
    GROUP BY c.thread_id
)
SELECT
    g.thread_id,
    t.parent_thread_id,
    t.root_thread_id,
    t.agent_role,
    t.thread_source,
    g.provider_call_count,
    g.priced_call_count,
    g.unpriced_call_count,
    g.unpriced_call_count > 0 AS partial,
    g.first_call_at,
    g.last_call_at,
    g.uncached_input_tokens,
    g.cached_input_tokens,
    g.cache_write_input_tokens,
    g.output_tokens,
    g.total_tokens,
    CASE WHEN g.unpriced_call_count = 0 THEN g.uncached_input_credits END
        AS uncached_input_credits,
    CASE WHEN g.unpriced_call_count = 0 THEN g.cached_input_credits END
        AS cached_input_credits,
    CASE WHEN g.unpriced_call_count = 0 THEN g.output_credits END
        AS output_credits,
    CASE WHEN g.unpriced_call_count = 0 THEN g.priced_credits_total END
        AS estimated_total_credits,
    g.priced_credits_total,
    g.models_used,
    g.service_tiers_used,
    g.rate_card_kinds
FROM grouped AS g
LEFT JOIN usage_threads AS t ON t.thread_id = g.thread_id;

CREATE VIEW usage_turn_credit_summary AS
WITH grouped AS (
    SELECT
        thread_id,
        turn_id,
        spawn_request_id,
        COUNT(*) AS provider_call_count,
        SUM(pricing_status IN ('priced_estimate', 'provider_reported')) AS priced_call_count,
        SUM(pricing_status NOT IN ('priced_estimate', 'provider_reported')) AS unpriced_call_count,
        SUM(input_tokens_uncached) AS uncached_input_tokens,
        SUM(input_tokens_cached) AS cached_input_tokens,
        SUM(input_tokens_cache_write) AS cache_write_input_tokens,
        SUM(output_tokens) AS output_tokens,
        SUM(total_tokens) AS total_tokens,
        SUM(uncached_input_credits) AS uncached_input_credits,
        SUM(cached_input_credits) AS cached_input_credits,
        SUM(output_credits) AS output_credits,
        SUM(estimated_total_credits) AS priced_credits_total
    FROM usage_provider_call_credit_estimates
    GROUP BY thread_id, turn_id, spawn_request_id
)
SELECT
    *,
    unpriced_call_count > 0 AS partial,
    CASE WHEN unpriced_call_count = 0 THEN priced_credits_total END
        AS estimated_total_credits
FROM grouped;
