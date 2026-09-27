PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS usage_threads (
    thread_id TEXT PRIMARY KEY,
    parent_thread_id TEXT,
    root_thread_id TEXT,
    fork_parent_thread_id TEXT,
    agent_nickname TEXT,
    agent_role TEXT,
    source TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE IF NOT EXISTS usage_spawn_requests (
    spawn_request_id TEXT PRIMARY KEY,
    parent_thread_id TEXT NOT NULL,
    child_thread_id TEXT,
    requested_model TEXT,
    requested_role TEXT,
    requested_reasoning_effort TEXT,
    status TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    completed_at TEXT
);
CREATE INDEX IF NOT EXISTS usage_spawn_requests_parent_idx ON usage_spawn_requests(parent_thread_id);

CREATE TABLE IF NOT EXISTS usage_provider_calls (
    provider_call_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    turn_id TEXT,
    spawn_request_id TEXT,
    tool_call_id TEXT,
    provider TEXT,
    requested_model TEXT,
    actual_model_used TEXT,
    request_id TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    input_tokens_uncached INTEGER DEFAULT 0,
    input_tokens_cached INTEGER DEFAULT 0,
    output_tokens INTEGER DEFAULT 0,
    total_tokens INTEGER DEFAULT 0,
    provider_reported_cost REAL,
    provider_reported_currency TEXT,
    status TEXT
);
CREATE INDEX IF NOT EXISTS usage_provider_calls_thread_idx ON usage_provider_calls(thread_id);
CREATE INDEX IF NOT EXISTS usage_provider_calls_spawn_idx ON usage_provider_calls(spawn_request_id);
CREATE INDEX IF NOT EXISTS usage_provider_calls_tool_idx ON usage_provider_calls(tool_call_id);

CREATE TABLE IF NOT EXISTS usage_tool_calls (
    tool_call_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    turn_id TEXT,
    tool_name TEXT NOT NULL,
    server_name TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    status TEXT,
    duration_ms INTEGER
);
CREATE INDEX IF NOT EXISTS usage_tool_calls_thread_idx ON usage_tool_calls(thread_id);

CREATE TABLE IF NOT EXISTS usage_quota_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    thread_id TEXT,
    turn_id TEXT,
    observed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    quota_source TEXT,
    quota_percent_remaining REAL,
    quota_percent_used REAL,
    plan TEXT,
    notes TEXT
);
CREATE INDEX IF NOT EXISTS usage_quota_snapshots_thread_idx ON usage_quota_snapshots(thread_id);

CREATE TABLE IF NOT EXISTS usage_fork_snapshots (
    child_thread_id TEXT PRIMARY KEY,
    parent_thread_id TEXT NOT NULL,
    forked_at TEXT NOT NULL,
    parent_last_provider_call_id TEXT,
    parent_cumulative_uncached_tokens INTEGER,
    parent_cumulative_cached_tokens INTEGER,
    parent_cumulative_output_tokens INTEGER,
    parent_cumulative_total_tokens INTEGER
);
