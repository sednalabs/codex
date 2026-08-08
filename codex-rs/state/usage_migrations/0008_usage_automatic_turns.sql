CREATE TABLE IF NOT EXISTS usage_automatic_turns (
    thread_id TEXT NOT NULL,
    client_user_message_id TEXT NOT NULL,
    trigger_turn_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    origin TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    max_attempts INTEGER NOT NULL,
    provenance_source TEXT NOT NULL,
    outcome TEXT NOT NULL DEFAULT 'started',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    completed_at TEXT,
    PRIMARY KEY (thread_id, client_user_message_id),
    UNIQUE (turn_id),
    CHECK (attempt >= 1),
    CHECK (max_attempts >= 1),
    CHECK (attempt <= max_attempts)
);

CREATE INDEX IF NOT EXISTS usage_automatic_turns_thread_idx
    ON usage_automatic_turns(thread_id);
CREATE INDEX IF NOT EXISTS usage_automatic_turns_trigger_idx
    ON usage_automatic_turns(thread_id, trigger_turn_id);
CREATE INDEX IF NOT EXISTS usage_automatic_turns_origin_idx
    ON usage_automatic_turns(origin);
