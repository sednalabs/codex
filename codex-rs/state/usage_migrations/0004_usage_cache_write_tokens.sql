ALTER TABLE usage_provider_calls
ADD COLUMN input_tokens_cache_write INTEGER DEFAULT 0;

ALTER TABLE usage_fork_snapshots
ADD COLUMN parent_cumulative_cache_write_tokens INTEGER;
