ALTER TABLE usage_provider_calls
ADD COLUMN requested_service_tier TEXT;

ALTER TABLE usage_provider_calls
ADD COLUMN actual_service_tier TEXT;

ALTER TABLE usage_provider_calls
ADD COLUMN actual_service_tier_source TEXT;

ALTER TABLE usage_provider_calls
ADD COLUMN fast_mode_requested INTEGER
CHECK (fast_mode_requested IS NULL OR fast_mode_requested IN (0, 1));

ALTER TABLE usage_provider_calls
ADD COLUMN fast_mode_used INTEGER
CHECK (fast_mode_used IS NULL OR fast_mode_used IN (0, 1));

ALTER TABLE usage_provider_calls
ADD COLUMN billing_surface TEXT;

ALTER TABLE usage_provider_calls
ADD COLUMN account_plan TEXT;

ALTER TABLE usage_provider_calls
ADD COLUMN provider_reported_credits REAL
CHECK (provider_reported_credits IS NULL OR provider_reported_credits >= 0);

CREATE INDEX usage_provider_calls_service_tier_idx
ON usage_provider_calls(actual_service_tier);

CREATE TABLE usage_codex_credit_rates (
    rate_id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    service_tier TEXT NOT NULL,
    speed_mode TEXT NOT NULL,
    rate_card_kind TEXT NOT NULL,
    credits_per_1m_uncached_input REAL NOT NULL CHECK (credits_per_1m_uncached_input >= 0),
    credits_per_1m_cached_input REAL NOT NULL CHECK (credits_per_1m_cached_input >= 0),
    credits_per_1m_output REAL NOT NULL CHECK (credits_per_1m_output >= 0),
    effective_from TEXT NOT NULL,
    effective_to TEXT,
    source_url TEXT NOT NULL,
    source_observed_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    CHECK (effective_to IS NULL OR effective_to > effective_from)
);

CREATE INDEX usage_codex_credit_rates_lookup_idx
ON usage_codex_credit_rates(
    provider, model, service_tier, speed_mode, rate_card_kind, effective_from
);

CREATE TRIGGER usage_codex_credit_rates_no_overlap_insert
BEFORE INSERT ON usage_codex_credit_rates
BEGIN
    SELECT RAISE(ABORT, 'ambiguous Codex credit rate interval')
    WHERE EXISTS (
        SELECT 1
        FROM usage_codex_credit_rates AS existing
        WHERE existing.provider = NEW.provider
          AND existing.model = NEW.model
          AND existing.service_tier = NEW.service_tier
          AND existing.speed_mode = NEW.speed_mode
          AND existing.rate_card_kind = NEW.rate_card_kind
          AND existing.effective_from < COALESCE(NEW.effective_to, '9999-12-31T23:59:59.999Z')
          AND COALESCE(existing.effective_to, '9999-12-31T23:59:59.999Z') > NEW.effective_from
    );
END;

CREATE TRIGGER usage_codex_credit_rates_no_overlap_update
BEFORE UPDATE OF provider, model, service_tier, speed_mode, rate_card_kind,
    effective_from, effective_to
ON usage_codex_credit_rates
BEGIN
    SELECT RAISE(ABORT, 'ambiguous Codex credit rate interval')
    WHERE EXISTS (
        SELECT 1
        FROM usage_codex_credit_rates AS existing
        WHERE existing.rate_id <> NEW.rate_id
          AND existing.provider = NEW.provider
          AND existing.model = NEW.model
          AND existing.service_tier = NEW.service_tier
          AND existing.speed_mode = NEW.speed_mode
          AND existing.rate_card_kind = NEW.rate_card_kind
          AND existing.effective_from < COALESCE(NEW.effective_to, '9999-12-31T23:59:59.999Z')
          AND COALESCE(existing.effective_to, '9999-12-31T23:59:59.999Z') > NEW.effective_from
    );
END;

CREATE TABLE usage_codex_credit_policies (
    policy_id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    billing_surface TEXT NOT NULL,
    account_plan TEXT NOT NULL,
    rate_card_kind TEXT NOT NULL,
    effective_from TEXT NOT NULL,
    effective_to TEXT,
    source_url TEXT NOT NULL,
    source_observed_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    CHECK (effective_to IS NULL OR effective_to > effective_from)
);

CREATE INDEX usage_codex_credit_policies_lookup_idx
ON usage_codex_credit_policies(
    provider, billing_surface, account_plan, effective_from
);

CREATE TRIGGER usage_codex_credit_policies_no_overlap_insert
BEFORE INSERT ON usage_codex_credit_policies
BEGIN
    SELECT RAISE(ABORT, 'ambiguous Codex credit policy interval')
    WHERE EXISTS (
        SELECT 1
        FROM usage_codex_credit_policies AS existing
        WHERE existing.provider = NEW.provider
          AND existing.billing_surface = NEW.billing_surface
          AND existing.account_plan = NEW.account_plan
          AND existing.effective_from < COALESCE(NEW.effective_to, '9999-12-31T23:59:59.999Z')
          AND COALESCE(existing.effective_to, '9999-12-31T23:59:59.999Z') > NEW.effective_from
    );
END;

CREATE TRIGGER usage_codex_credit_policies_no_overlap_update
BEFORE UPDATE OF provider, billing_surface, account_plan, effective_from, effective_to
ON usage_codex_credit_policies
BEGIN
    SELECT RAISE(ABORT, 'ambiguous Codex credit policy interval')
    WHERE EXISTS (
        SELECT 1
        FROM usage_codex_credit_policies AS existing
        WHERE existing.policy_id <> NEW.policy_id
          AND existing.provider = NEW.provider
          AND existing.billing_surface = NEW.billing_surface
          AND existing.account_plan = NEW.account_plan
          AND existing.effective_from < COALESCE(NEW.effective_to, '9999-12-31T23:59:59.999Z')
          AND COALESCE(existing.effective_to, '9999-12-31T23:59:59.999Z') > NEW.effective_from
    );
END;

INSERT INTO usage_codex_credit_policies (
    policy_id, provider, billing_surface, account_plan, rate_card_kind,
    effective_from, effective_to, source_url, source_observed_at
) VALUES
    ('openai-chatgpt-plus-token-20260402', 'openai', 'chatgpt_credits', 'plus',
     'codex_token_based', '2026-04-02T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-07-27T00:00:00Z'),
    ('openai-chatgpt-pro-token-20260402', 'openai', 'chatgpt_credits', 'pro',
     'codex_token_based', '2026-04-02T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-07-27T00:00:00Z'),
    ('openai-chatgpt-business-token-20260402', 'openai', 'chatgpt_credits', 'business',
     'codex_token_based', '2026-04-02T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-07-27T00:00:00Z');

INSERT INTO usage_codex_credit_rates (
    rate_id, provider, model, service_tier, speed_mode, rate_card_kind,
    credits_per_1m_uncached_input, credits_per_1m_cached_input,
    credits_per_1m_output, effective_from, effective_to, source_url,
    source_observed_at
) VALUES
    ('openai-gpt-5.6-luna-standard-20260402', 'openai', 'gpt-5.6-luna',
     'default', 'standard', 'codex_token_based', 25.0, 2.5, 150.0,
     '2026-04-02T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-07-27T00:00:00Z'),
    ('openai-gpt-5.6-terra-standard-20260402', 'openai', 'gpt-5.6-terra',
     'default', 'standard', 'codex_token_based', 62.5, 6.25, 375.0,
     '2026-04-02T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-07-27T00:00:00Z'),
    ('openai-gpt-5.6-sol-standard-20260402', 'openai', 'gpt-5.6-sol',
     'default', 'standard', 'codex_token_based', 125.0, 12.5, 750.0,
     '2026-04-02T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-07-27T00:00:00Z'),
    ('openai-gpt-5.6-luna-fast-20260727', 'openai', 'gpt-5.6-luna',
     'priority', 'fast', 'codex_token_based', 62.5, 6.25, 375.0,
     '2026-07-27T00:00:00Z', NULL,
     'https://learn.chatgpt.com/docs/agent-configuration/speed',
     '2026-07-27T00:00:00Z'),
    ('openai-gpt-5.6-terra-fast-20260727', 'openai', 'gpt-5.6-terra',
     'priority', 'fast', 'codex_token_based', 156.25, 15.625, 937.5,
     '2026-07-27T00:00:00Z', NULL,
     'https://learn.chatgpt.com/docs/agent-configuration/speed',
     '2026-07-27T00:00:00Z'),
    ('openai-gpt-5.6-sol-fast-20260727', 'openai', 'gpt-5.6-sol',
     'priority', 'fast', 'codex_token_based', 312.5, 31.25, 1875.0,
     '2026-07-27T00:00:00Z', NULL,
     'https://learn.chatgpt.com/docs/agent-configuration/speed',
     '2026-07-27T00:00:00Z');

CREATE VIEW usage_provider_call_credit_estimates AS
WITH calls AS (
    SELECT
        p.*,
        COALESCE(NULLIF(lower(p.final_model), ''), NULLIF(lower(p.actual_model_used), ''))
            AS pricing_model,
        CASE WHEN p.fast_mode_used = 1 THEN 'fast' ELSE 'standard' END AS speed_mode
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
        COUNT(r.rate_id) AS matching_rate_count,
        MIN(r.rate_id) AS rate_id,
        COUNT(DISTINCT CASE WHEN r.model = p.pricing_model THEN r.model END)
            AS matching_model_count
    FROM calls AS p
    JOIN policy_matches AS c ON c.provider_call_id = p.provider_call_id
    LEFT JOIN usage_codex_credit_rates AS r
      ON c.matching_policy_count = 1
     AND r.provider = lower(p.provider)
     AND r.rate_card_kind = c.rate_card_kind
     AND r.model = p.pricing_model
     AND p.actual_service_tier_source IS NOT NULL
     AND r.service_tier = lower(p.actual_service_tier)
     AND r.speed_mode = p.speed_mode
     AND p.started_at >= r.effective_from
     AND (r.effective_to IS NULL OR p.started_at < r.effective_to)
    GROUP BY p.provider_call_id
),
model_matches AS (
    SELECT
        p.provider_call_id,
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
        mm.matching_model_rate_count,
        r.rate_id,
        r.rate_card_kind,
        r.effective_from AS rate_effective_from,
        r.effective_to AS rate_effective_to,
        r.source_url AS rate_source_url,
        r.source_observed_at AS rate_source_observed_at,
        CASE WHEN c.matching_policy_count = 1
                  AND m.matching_rate_count = 1
                  AND p.input_tokens_uncached IS NOT NULL
                  AND p.input_tokens_cached IS NOT NULL
                  AND p.output_tokens IS NOT NULL
                  AND COALESCE(p.input_tokens_cache_write, 0) = 0
             THEN p.input_tokens_uncached * r.credits_per_1m_uncached_input / 1000000.0
        END AS uncached_input_credits,
        CASE WHEN c.matching_policy_count = 1
                  AND m.matching_rate_count = 1
                  AND p.input_tokens_uncached IS NOT NULL
                  AND p.input_tokens_cached IS NOT NULL
                  AND p.output_tokens IS NOT NULL
                  AND COALESCE(p.input_tokens_cache_write, 0) = 0
             THEN p.input_tokens_cached * r.credits_per_1m_cached_input / 1000000.0
        END AS cached_input_credits,
        CASE WHEN c.matching_policy_count = 1
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
    JOIN model_matches AS mm ON mm.provider_call_id = p.provider_call_id
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
        WHEN matching_policy_count = 1
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
