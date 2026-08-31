CREATE TABLE IF NOT EXISTS usage_automatic_turn_chains (
    thread_id TEXT PRIMARY KEY,
    generation INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    CHECK (generation >= 0)
);

-- Every server-observed policy trigger is recorded, including suppressed and exhausted events.
-- The generation/attempt pair permits repeated attempts to share a regular turn id while keeping
-- duplicate and stale policy events from minting a fresh attempt-1 ticket.
CREATE TABLE IF NOT EXISTS usage_automatic_turn_triggers (
    thread_id TEXT NOT NULL,
    trigger_turn_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    outcome TEXT NOT NULL DEFAULT 'pending',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (thread_id, trigger_turn_id, generation, attempt),
    CHECK (generation >= 0),
    CHECK (attempt >= 1)
);

CREATE TABLE IF NOT EXISTS usage_automatic_turn_eligibility (
    thread_id TEXT PRIMARY KEY,
    trigger_turn_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    next_attempt INTEGER NOT NULL,
    max_attempts INTEGER NOT NULL,
    capability TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    CHECK (generation >= 0),
    CHECK (next_attempt >= 1),
    CHECK (max_attempts >= 1),
    CHECK (next_attempt <= max_attempts)
);

CREATE TABLE IF NOT EXISTS usage_automatic_turns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    client_user_message_id TEXT NOT NULL,
    trigger_turn_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    origin TEXT NOT NULL,
    generation INTEGER NOT NULL,
    capability TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    max_attempts INTEGER NOT NULL,
    provenance_source TEXT NOT NULL,
    outcome TEXT NOT NULL DEFAULT 'started',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    completed_at TEXT,
    UNIQUE (thread_id, client_user_message_id),
    CHECK (attempt >= 1),
    CHECK (generation >= 0),
    CHECK (max_attempts >= 1),
    CHECK (attempt <= max_attempts)
);

CREATE INDEX IF NOT EXISTS usage_automatic_turns_thread_turn_idx
    ON usage_automatic_turns(thread_id, turn_id, outcome, id);
CREATE INDEX IF NOT EXISTS usage_automatic_turns_trigger_idx
    ON usage_automatic_turns(thread_id, trigger_turn_id);
CREATE INDEX IF NOT EXISTS usage_automatic_turns_origin_idx
    ON usage_automatic_turns(origin);
CREATE INDEX IF NOT EXISTS usage_automatic_turn_triggers_thread_idx
    ON usage_automatic_turn_triggers(thread_id, generation, attempt, created_at);
