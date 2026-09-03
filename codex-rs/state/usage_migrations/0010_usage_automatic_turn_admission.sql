ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN admitted_client_user_message_id TEXT;
ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN admitted_operation_kind TEXT;
ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN admitted_expected_turn_id TEXT;

CREATE INDEX IF NOT EXISTS usage_automatic_turn_eligibility_admission_idx
    ON usage_automatic_turn_eligibility(thread_id, admitted_client_user_message_id);
