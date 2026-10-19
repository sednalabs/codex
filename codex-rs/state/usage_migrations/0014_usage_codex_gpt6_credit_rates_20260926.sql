-- Official ChatGPT Work/Codex credit rates observed on 2026-09-26.
--
-- This migration is additive and effective-dated.  Existing rows remain the
-- source of truth for historical calls; provider-reported credits continue to
-- take precedence over estimates.  The rate card does not establish
-- entitlement for a plan not represented in the ledger policy tables.
--
-- Standard/default rates are 50/5/250 for GPT-6 Sol and 2.5/0.25/12.5 for
-- GPT-6 Luna (uncached input/cached input/output credits per 1M tokens).
-- Fast/priority is the documented 2.5x tier.  GPT-6 Astra already has a
-- standard row in migration 0013, so its non-overlapping Fast row is included
-- here as well.

INSERT INTO usage_codex_credit_rates (
    rate_id, provider, model, service_tier, speed_mode, rate_card_kind,
    credits_per_1m_uncached_input, credits_per_1m_cached_input,
    credits_per_1m_output, effective_from, effective_to, source_url,
    source_observed_at
) VALUES
    ('openai-gpt-6-sol-standard-20260926', 'openai', 'gpt-6-sol',
     'default', 'standard', 'codex_token_based', 50.0, 5.0, 250.0,
     '2026-09-26T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-26T00:00:00Z'),
    ('openai-gpt-6-luna-standard-20260926', 'openai', 'gpt-6-luna',
     'default', 'standard', 'codex_token_based', 2.5, 0.25, 12.5,
     '2026-09-26T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-26T00:00:00Z'),
    ('openai-gpt-6-sol-fast-20260926', 'openai', 'gpt-6-sol',
     'priority', 'fast', 'codex_token_based', 125.0, 12.5, 625.0,
     '2026-09-26T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-26T00:00:00Z'),
    ('openai-gpt-6-luna-fast-20260926', 'openai', 'gpt-6-luna',
     'priority', 'fast', 'codex_token_based', 6.25, 0.625, 31.25,
     '2026-09-26T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-26T00:00:00Z'),
    ('openai-gpt-6-astra-fast-20260926', 'openai', 'gpt-6-astra',
     'priority', 'fast', 'codex_token_based', 625.0, 62.5, 3125.0,
     '2026-09-26T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-26T00:00:00Z');
