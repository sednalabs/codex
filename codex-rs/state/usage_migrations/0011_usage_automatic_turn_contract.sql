ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN allowed_operation_kind TEXT;
ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN allowed_expected_turn_id TEXT;
ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN trigger_context_fingerprint TEXT;
