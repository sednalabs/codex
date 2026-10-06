CREATE INDEX usage_provider_calls_started_at_idx
ON usage_provider_calls(started_at);

CREATE INDEX usage_provider_calls_thread_started_at_idx
ON usage_provider_calls(thread_id, started_at);

DROP INDEX IF EXISTS usage_provider_calls_thread_idx;

CREATE INDEX usage_threads_reporting_parent_idx
ON usage_threads(
    COALESCE(NULLIF(parent_thread_id,''), NULLIF(fork_parent_thread_id,''))
);
