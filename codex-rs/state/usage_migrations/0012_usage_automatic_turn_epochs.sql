ALTER TABLE usage_automatic_turn_chains
    ADD COLUMN settings_generation INTEGER NOT NULL DEFAULT 0;

ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN settings_generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN auth_generation INTEGER NOT NULL DEFAULT 0;

ALTER TABLE usage_automatic_turns
    ADD COLUMN abort_event_occurrence_id TEXT;

-- Rows written before abort occurrences were tracked have no separate abort identity. Preserve
-- their historical event identity so terminal abort replay remains idempotent after upgrade.
UPDATE usage_automatic_turns
SET abort_event_occurrence_id = event_occurrence_id
WHERE outcome = 'aborted' AND abort_event_occurrence_id IS NULL;

CREATE INDEX IF NOT EXISTS usage_automatic_turns_abort_occurrence_idx
    ON usage_automatic_turns(thread_id, abort_event_occurrence_id, generation, attempt);

CREATE TABLE IF NOT EXISTS usage_automatic_turn_auth_epochs (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    generation INTEGER NOT NULL DEFAULT 0,
    CHECK (generation >= 0)
);

INSERT INTO usage_automatic_turn_auth_epochs (id, generation)
VALUES (1, 0)
ON CONFLICT(id) DO NOTHING;
