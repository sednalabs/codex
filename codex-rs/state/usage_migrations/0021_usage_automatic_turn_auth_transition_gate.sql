ALTER TABLE usage_automatic_turn_auth_epochs
    ADD COLUMN transition_pending INTEGER NOT NULL DEFAULT 0
    CHECK (transition_pending IN (0, 1));
