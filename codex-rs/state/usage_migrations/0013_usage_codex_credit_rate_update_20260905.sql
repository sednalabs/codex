-- Operator-authoritative STANDARD/default Codex credit rates observed on
-- 2026-09-05. The effective boundary is the migration observation timestamp;
-- calls before it retain their prior effective-dated rows.
--
-- Keep this migration append-only for historical pricing. Only the stale open
-- Sol interval is closed; all other existing rows remain intact.
UPDATE usage_codex_credit_rates
SET effective_to = '2026-09-05T05:26:07.605187Z'
WHERE rate_id = 'openai-gpt-5.6-sol-standard-20260402'
  AND effective_to IS NULL;

UPDATE usage_codex_credit_rates
SET effective_to = '2026-09-05T05:26:07.605187Z'
WHERE rate_id = 'openai-gpt-5.6-sol-fast-20260727'
  AND effective_to IS NULL;

INSERT INTO usage_codex_credit_rates (
    rate_id, provider, model, service_tier, speed_mode, rate_card_kind,
    credits_per_1m_uncached_input, credits_per_1m_cached_input,
    credits_per_1m_output, effective_from, effective_to, source_url,
    source_observed_at
) VALUES
    ('openai-gpt-6-astra-standard-20260905', 'openai', 'gpt-6-astra',
     'default', 'standard', 'codex_token_based', 250.0, 25.0, 1250.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-5.6-sol-standard-20260905', 'openai', 'gpt-5.6-sol',
     'default', 'standard', 'codex_token_based', 100.0, 10.0, 500.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-5.6-sol-fast-20260905', 'openai', 'gpt-5.6-sol',
     'priority', 'fast', 'codex_token_based', 250.0, 25.0, 1250.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://learn.chatgpt.com/docs/agent-configuration/speed',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-5.6-alias-standard-20260905', 'openai', 'gpt-5.6',
     'default', 'standard', 'codex_token_based', 100.0, 10.0, 500.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://developers.openai.com/api/docs/models',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-5.6-cyber-standard-20260905', 'openai', 'gpt-5.6-cyber',
     'default', 'standard', 'codex_token_based', 312.5, 31.25, 1875.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-daybreak-blue-standard-20260905', 'openai',
     'gpt-daybreak-blue', 'default', 'standard', 'codex_token_based',
     100.0, 10.0, 500.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://help.openai.com/en/articles/20001259-trusted-access-for-cyber-common-issues-and-troubleshooting',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-daybreak-blue-latest-standard-20260905', 'openai',
     'gpt-daybreak-blue-latest', 'default', 'standard', 'codex_token_based',
     100.0, 10.0, 500.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://help.openai.com/en/articles/20001259-trusted-access-for-cyber-common-issues-and-troubleshooting',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-daybreak-red-standard-20260905', 'openai',
     'gpt-daybreak-red', 'default', 'standard', 'codex_token_based',
     312.5, 31.25, 1875.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://help.openai.com/en/articles/20001259-trusted-access-for-cyber-common-issues-and-troubleshooting',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-daybreak-red-latest-standard-20260905', 'openai',
     'gpt-daybreak-red-latest', 'default', 'standard', 'codex_token_based',
     312.5, 31.25, 1875.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://help.openai.com/en/articles/20001259-trusted-access-for-cyber-common-issues-and-troubleshooting',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-5.4-mini-standard-20260905', 'openai', 'gpt-5.4-mini',
     'default', 'standard', 'codex_token_based', 18.75, 1.875, 113.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-5.3-codex-standard-20260905', 'openai',
     'gpt-5.3-codex', 'default', 'standard', 'codex_token_based',
     43.75, 4.375, 350.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-5.2-standard-20260905', 'openai', 'gpt-5.2',
     'default', 'standard', 'codex_token_based', 43.75, 4.375, 350.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://help.openai.com/en/articles/20001106-codex-rate-card',
     '2026-09-05T05:26:07.605187Z'),
    -- GPT-Image-2 has one provider model ID but two modality rate cards.
    -- Keep both cards visible for future modality-aware matching, but do not
    -- attach them to the token policy until usage calls carry that evidence.
    ('openai-gpt-image-2-image-standard-20260905', 'openai', 'gpt-image-2',
     'default', 'standard', 'codex_token_based_image', 200.0, 50.0, 750.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://developers.openai.com/api/docs/models/gpt-image-2',
     '2026-09-05T05:26:07.605187Z'),
    ('openai-gpt-image-2-text-standard-20260905', 'openai', 'gpt-image-2',
     'default', 'standard', 'codex_token_based_text', 125.0, 31.25, 250.0,
     '2026-09-05T05:26:07.605187Z', NULL,
     'https://developers.openai.com/api/docs/models/gpt-image-2',
     '2026-09-05T05:26:07.605187Z');
