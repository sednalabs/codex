ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN event_occurrence_id TEXT;
ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN connection_principal TEXT;

ALTER TABLE usage_automatic_turns
    ADD COLUMN event_occurrence_id TEXT NOT NULL DEFAULT '';
ALTER TABLE usage_automatic_turns
    ADD COLUMN connection_principal TEXT;

-- The original trigger key treated a repeated turn id as one occurrence. Rebuild the table so
-- duplicate suppression is scoped to the immutable server event occurrence as well as the
-- generation and attempt. Existing rows use their trigger identity as the best available
-- occurrence identity from the pre-hardening schema.
ALTER TABLE usage_automatic_turn_triggers RENAME TO usage_automatic_turn_triggers_legacy;

CREATE TABLE usage_automatic_turn_triggers (
    thread_id TEXT NOT NULL,
    trigger_turn_id TEXT NOT NULL,
    event_occurrence_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    outcome TEXT NOT NULL DEFAULT 'pending',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (thread_id, trigger_turn_id, event_occurrence_id, generation, attempt),
    CHECK (generation >= 0),
    CHECK (attempt >= 1)
);

INSERT INTO usage_automatic_turn_triggers (
    thread_id, trigger_turn_id, event_occurrence_id, generation, attempt, outcome, created_at
)
SELECT thread_id, trigger_turn_id, trigger_turn_id, generation, attempt, outcome, created_at
FROM usage_automatic_turn_triggers_legacy;

DROP TABLE usage_automatic_turn_triggers_legacy;

UPDATE usage_automatic_turn_eligibility
SET event_occurrence_id = trigger_turn_id
WHERE event_occurrence_id IS NULL;

UPDATE usage_automatic_turns
SET event_occurrence_id = turn_id
WHERE event_occurrence_id = '';

CREATE INDEX IF NOT EXISTS usage_automatic_turn_triggers_occurrence_idx
    ON usage_automatic_turn_triggers(thread_id, generation, attempt, event_occurrence_id);

-- Preserve the legacy lookup path after rebuilding the table above.
CREATE INDEX IF NOT EXISTS usage_automatic_turn_triggers_thread_idx
    ON usage_automatic_turn_triggers(thread_id, generation, attempt, created_at);
