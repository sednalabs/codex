-- This is an operator-supplied standard-rate scenario, not provider billing.
-- It deliberately does not use requested model, actual service tier, speed,
-- billing surface, account plan, or provider-reported credits. The OpenAI
-- standard/default card is an explicit assumption applied only to the model
-- observed on the completed response and its response-local token dimensions.
CREATE VIEW usage_provider_call_standard_rate_estimates AS
WITH calls AS (
    SELECT
        p.*,
        lower(NULLIF(trim(p.actual_model_used), '')) AS observed_model
    FROM usage_provider_calls AS p
), model_rate_matches AS (
    SELECT
        p.provider_call_id,
        COUNT(r.rate_id) AS model_rate_count
    FROM calls AS p
    LEFT JOIN usage_codex_credit_rates AS r
      ON lower(r.provider) = 'openai'
     AND lower(r.model) = p.observed_model
     AND lower(r.service_tier) = 'default'
     AND lower(r.speed_mode) = 'standard'
     AND lower(r.rate_card_kind) = 'codex_token_based'
    GROUP BY p.provider_call_id
), effective_rate_matches AS (
    SELECT
        p.provider_call_id,
        COUNT(r.rate_id) AS matching_rate_count,
        MIN(r.rate_id) AS rate_id
    FROM calls AS p
    LEFT JOIN usage_codex_credit_rates AS r
      ON lower(r.provider) = 'openai'
     AND lower(r.model) = p.observed_model
     AND lower(r.service_tier) = 'default'
     AND lower(r.speed_mode) = 'standard'
     AND lower(r.rate_card_kind) = 'codex_token_based'
     AND p.started_at >= r.effective_from
     AND (r.effective_to IS NULL OR p.started_at < r.effective_to)
    GROUP BY p.provider_call_id
), estimates AS (
    SELECT
        p.*,
        mm.model_rate_count,
        em.matching_rate_count,
        r.rate_id,
        r.model AS rate_model,
        r.credits_per_1m_uncached_input,
        r.credits_per_1m_cached_input,
        r.credits_per_1m_output,
        r.effective_from AS rate_effective_from,
        r.effective_to AS rate_effective_to,
        r.source_url AS rate_source_url,
        r.source_observed_at AS rate_source_observed_at
    FROM calls AS p
    JOIN model_rate_matches AS mm USING (provider_call_id)
    JOIN effective_rate_matches AS em USING (provider_call_id)
    LEFT JOIN usage_codex_credit_rates AS r
      ON r.rate_id = em.rate_id
     AND em.matching_rate_count = 1
)
SELECT
    provider_call_id,
    thread_id,
    turn_id,
    request_id AS response_id,
    provider AS observed_provider,
    requested_model,
    actual_model_used AS observed_model,
    CASE WHEN observed_model IS NOT NULL THEN 'actual_model_used' END
        AS model_evidence,
    started_at,
    completed_at,
    input_tokens_uncached,
    input_tokens_cached,
    input_tokens_cache_write,
    output_tokens,
    total_tokens,
    'operator_supplied_standard_rate_card_scenario'
        AS estimate_scenario,
    'operator_supplied_credit_rate_guide' AS assumption_source,
    'openai' AS assumed_rate_provider,
    'codex_token_based' AS assumed_rate_card_kind,
    'default' AS assumed_service_tier,
    'standard' AS assumed_speed_mode,
    'credits' AS estimate_unit,
    'OpenAI standard/default Codex rate card is an operator assumption; this is not provider-reported or invoiced billing.'
        AS assumption_notes,
    rate_id,
    rate_model,
    rate_effective_from,
    rate_effective_to,
    rate_source_observed_at,
    rate_source_url,
    model_rate_count,
    matching_rate_count,
    CASE
        WHEN status IS NULL OR status <> 'ok' THEN 'provider_usage_missing'
        WHEN observed_model IS NULL THEN 'actual_model_missing'
        WHEN input_tokens_uncached IS NULL
          OR input_tokens_cached IS NULL
          OR input_tokens_cache_write IS NULL
          OR output_tokens IS NULL
          OR total_tokens IS NULL THEN 'token_breakdown_incomplete'
        WHEN input_tokens_cache_write <> 0 THEN 'cache_write_unsupported'
        WHEN matching_rate_count > 1 THEN 'ambiguous_rate'
        WHEN matching_rate_count = 0 AND model_rate_count > 0
            THEN 'rate_not_effective'
        WHEN matching_rate_count = 0 THEN 'model_rate_missing'
        ELSE 'priced_scenario_estimate'
    END AS scenario_status,
    CASE
        WHEN status IS NULL OR status <> 'ok'
            THEN 'response-local provider usage is missing or incomplete'
        WHEN observed_model IS NULL
            THEN 'same-response observed model is missing; requested model is not substituted'
        WHEN input_tokens_uncached IS NULL
          OR input_tokens_cached IS NULL
          OR input_tokens_cache_write IS NULL
          OR output_tokens IS NULL
          OR total_tokens IS NULL
            THEN 'one or more response-local token dimensions are missing'
        WHEN input_tokens_cache_write <> 0
            THEN 'nonzero cache-write tokens are unsupported by this standard-card estimate'
        WHEN matching_rate_count > 1
            THEN 'multiple supplied standard rates match the observed model and response time'
        WHEN matching_rate_count = 0 AND model_rate_count > 0
            THEN 'the supplied standard rate is not effective at the response start time'
        WHEN matching_rate_count = 0
            THEN 'no supplied standard rate matches the observed model'
        ELSE 'estimate applies the operator-supplied OpenAI standard/default card only'
    END AS scenario_reason,
    CASE WHEN status = 'ok'
               AND observed_model IS NOT NULL
               AND input_tokens_uncached IS NOT NULL
               AND input_tokens_cached IS NOT NULL
               AND input_tokens_cache_write = 0
               AND output_tokens IS NOT NULL
               AND total_tokens IS NOT NULL
               AND matching_rate_count = 1
         THEN input_tokens_uncached * credits_per_1m_uncached_input / 1000000.0
    END AS uncached_input_estimate_credits,
    CASE WHEN status = 'ok'
               AND observed_model IS NOT NULL
               AND input_tokens_uncached IS NOT NULL
               AND input_tokens_cached IS NOT NULL
               AND input_tokens_cache_write = 0
               AND output_tokens IS NOT NULL
               AND total_tokens IS NOT NULL
               AND matching_rate_count = 1
         THEN input_tokens_cached * credits_per_1m_cached_input / 1000000.0
    END AS cached_input_estimate_credits,
    CASE WHEN status = 'ok'
               AND observed_model IS NOT NULL
               AND input_tokens_uncached IS NOT NULL
               AND input_tokens_cached IS NOT NULL
               AND input_tokens_cache_write = 0
               AND output_tokens IS NOT NULL
               AND total_tokens IS NOT NULL
               AND matching_rate_count = 1
         THEN output_tokens * credits_per_1m_output / 1000000.0
    END AS output_estimate_credits,
    CASE WHEN status = 'ok'
               AND observed_model IS NOT NULL
               AND input_tokens_uncached IS NOT NULL
               AND input_tokens_cached IS NOT NULL
               AND input_tokens_cache_write = 0
               AND output_tokens IS NOT NULL
               AND total_tokens IS NOT NULL
               AND matching_rate_count = 1
         THEN input_tokens_uncached * credits_per_1m_uncached_input / 1000000.0
            + input_tokens_cached * credits_per_1m_cached_input / 1000000.0
            + output_tokens * credits_per_1m_output / 1000000.0
    END AS estimated_total_credits
FROM estimates;
