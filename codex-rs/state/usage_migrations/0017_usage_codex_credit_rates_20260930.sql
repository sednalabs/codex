-- Operator-adopted ChatGPT Work/Codex credit card observed at
-- 2026-09-29T23:52:00Z. This is the adoption boundary, not a claim about the
-- vendor's historical effective start. Preserve prior intervals unchanged.
--
-- Close only the currently open Fast rows that the current card no longer
-- lists as supported, and correct the three GPT-6 Fast rows from 2.5x to 2x.
UPDATE usage_codex_credit_rates
SET effective_to = '2026-09-29T23:52:00Z'
WHERE effective_to IS NULL
  AND rate_id IN (
    'openai-gpt-5.4-fast-20260727',
    'openai-gpt-5.5-fast-20260727',
    'openai-gpt-5.6-luna-fast-20260926',
    'openai-gpt-5.6-sol-fast-20260905',
    'openai-gpt-5.6-terra-fast-20260926',
    'openai-gpt-6-astra-fast-20260926',
    'openai-gpt-6-sol-fast-20260926',
    'openai-gpt-6-luna-fast-20260926'
  );

INSERT INTO usage_codex_credit_rates (
    rate_id, provider, model, service_tier, speed_mode, rate_card_kind,
    credits_per_1m_uncached_input, credits_per_1m_cached_input,
    credits_per_1m_output, effective_from, effective_to, source_url,
    source_observed_at
) VALUES
    ('openai-gpt-6-astra-fast-20260930', 'openai', 'gpt-6-astra',
     'priority', 'fast', 'codex_token_based', 500.0, 50.0, 2500.0,
     '2026-09-29T23:52:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-29T23:52:00Z'),
    ('openai-gpt-6-sol-fast-20260930', 'openai', 'gpt-6-sol',
     'priority', 'fast', 'codex_token_based', 100.0, 10.0, 500.0,
     '2026-09-29T23:52:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-29T23:52:00Z'),
    ('openai-gpt-6-luna-fast-20260930', 'openai', 'gpt-6-luna',
     'priority', 'fast', 'codex_token_based', 5.0, 0.5, 25.0,
     '2026-09-29T23:52:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-29T23:52:00Z'),
    ('openai-gpt-6.1-sol-standard-20260930', 'openai', 'gpt-6.1-sol',
     'default', 'standard', 'codex_token_based', 50.0, 2.5, 250.0,
     '2026-09-29T23:52:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-29T23:52:00Z'),
    ('openai-gpt-6.1-sol-fast-20260930', 'openai', 'gpt-6.1-sol',
     'priority', 'fast', 'codex_token_based', 100.0, 5.0, 500.0,
     '2026-09-29T23:52:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-29T23:52:00Z'),
    ('openai-gpt-rosalind-research-standard-20260930', 'openai',
     'gpt-rosalind-research', 'default', 'standard', 'codex_token_based',
     125.0, 12.5, 625.0,
     '2026-09-29T23:52:00Z', NULL,
     'https://help.openai.com/en/articles/11481834-chatgpt-rate-card-business-enterpriseedu-credit-based-pricing',
     '2026-09-29T23:52:00Z');
