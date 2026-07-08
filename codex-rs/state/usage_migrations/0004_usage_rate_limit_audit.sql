CREATE TABLE IF NOT EXISTS usage_rate_limit_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    thread_id TEXT,
    turn_id TEXT,
    observed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    observed_from TEXT NOT NULL,
    auth_mode TEXT,
    account_id_hash TEXT,
    chatgpt_user_id_hash TEXT,
    account_plan_type TEXT,
    codex_home_hash TEXT,
    sqlite_home_hash TEXT,
    limit_id TEXT,
    limit_name TEXT,
    primary_used_percent REAL,
    primary_window_minutes INTEGER,
    primary_resets_at INTEGER,
    secondary_used_percent REAL,
    secondary_window_minutes INTEGER,
    secondary_resets_at INTEGER,
    credits_has_credits INTEGER,
    credits_unlimited INTEGER,
    credits_balance TEXT,
    individual_limit_limit TEXT,
    individual_limit_used TEXT,
    individual_limit_remaining_percent INTEGER,
    individual_limit_resets_at INTEGER,
    plan TEXT,
    rate_limit_reached_type TEXT,
    reset_credits_available_count INTEGER,
    reset_credits_json TEXT,
    snapshot_json TEXT
);

CREATE INDEX IF NOT EXISTS usage_rate_limit_snapshots_thread_idx
    ON usage_rate_limit_snapshots(thread_id);
CREATE INDEX IF NOT EXISTS usage_rate_limit_snapshots_observed_at_idx
    ON usage_rate_limit_snapshots(observed_at);
CREATE INDEX IF NOT EXISTS usage_rate_limit_snapshots_account_idx
    ON usage_rate_limit_snapshots(account_id_hash, chatgpt_user_id_hash, observed_at);
CREATE INDEX IF NOT EXISTS usage_rate_limit_snapshots_reset_credits_idx
    ON usage_rate_limit_snapshots(reset_credits_available_count, observed_at);

CREATE TABLE IF NOT EXISTS usage_rate_limit_reset_credit_events (
    event_id TEXT PRIMARY KEY,
    observed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    event_type TEXT NOT NULL,
    auth_mode TEXT,
    account_id_hash TEXT,
    chatgpt_user_id_hash TEXT,
    account_plan_type TEXT,
    codex_home_hash TEXT,
    sqlite_home_hash TEXT,
    idempotency_key TEXT NOT NULL,
    credit_id TEXT,
    outcome TEXT,
    status TEXT NOT NULL,
    error TEXT,
    metadata_json TEXT
);

CREATE INDEX IF NOT EXISTS usage_rate_limit_reset_credit_events_observed_at_idx
    ON usage_rate_limit_reset_credit_events(observed_at);
CREATE INDEX IF NOT EXISTS usage_rate_limit_reset_credit_events_account_idx
    ON usage_rate_limit_reset_credit_events(account_id_hash, chatgpt_user_id_hash, observed_at);
CREATE INDEX IF NOT EXISTS usage_rate_limit_reset_credit_events_idempotency_idx
    ON usage_rate_limit_reset_credit_events(idempotency_key);
CREATE INDEX IF NOT EXISTS usage_rate_limit_reset_credit_events_credit_idx
    ON usage_rate_limit_reset_credit_events(credit_id, observed_at);
