-- GPT-5.6 Luna and Terra credit rates changed on 2026-07-30.
-- Preserve the prior half-open intervals and only close the expected open rows.
UPDATE usage_codex_credit_rates
SET effective_to = '2026-07-30T00:00:00Z'
WHERE rate_id IN (
    'openai-gpt-5.6-luna-standard-20260402',
    'openai-gpt-5.6-terra-standard-20260402',
    'openai-gpt-5.6-luna-fast-20260727',
    'openai-gpt-5.6-terra-fast-20260727'
)
  AND effective_to IS NULL;

INSERT INTO usage_codex_credit_rates (
    rate_id, provider, model, service_tier, speed_mode, rate_card_kind,
    credits_per_1m_uncached_input, credits_per_1m_cached_input,
    credits_per_1m_output, effective_from, effective_to, source_url,
    source_observed_at
) VALUES
    ('openai-gpt-5.6-luna-standard-20260730', 'openai', 'gpt-5.6-luna',
     'default', 'standard', 'codex_token_based', 5.0, 0.5, 30.0,
     '2026-07-30T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-07-30T00:00:00Z'),
    ('openai-gpt-5.6-terra-standard-20260730', 'openai', 'gpt-5.6-terra',
     'default', 'standard', 'codex_token_based', 50.0, 5.0, 300.0,
     '2026-07-30T00:00:00Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-07-30T00:00:00Z'),
    ('openai-gpt-5.6-luna-fast-20260730', 'openai', 'gpt-5.6-luna',
     'priority', 'fast', 'codex_token_based', 12.5, 1.25, 75.0,
     '2026-07-30T00:00:00Z', NULL,
     'https://learn.chatgpt.com/docs/agent-configuration/speed',
     '2026-07-30T00:00:00Z'),
    ('openai-gpt-5.6-terra-fast-20260730', 'openai', 'gpt-5.6-terra',
     'priority', 'fast', 'codex_token_based', 125.0, 12.5, 750.0,
     '2026-07-30T00:00:00Z', NULL,
     'https://learn.chatgpt.com/docs/agent-configuration/speed',
     '2026-07-30T00:00:00Z');
