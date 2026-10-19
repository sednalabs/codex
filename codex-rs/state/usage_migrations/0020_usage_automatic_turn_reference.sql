ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN continuation_ref TEXT NOT NULL DEFAULT '';
ALTER TABLE usage_automatic_turn_eligibility
    ADD COLUMN retry_reason TEXT NOT NULL DEFAULT 'legacy_unredeemable';

-- Re-key existing pending rows to references that were never issued to a client. A capability
-- from an older version must not remain redeemable through the reference-only protocol.
UPDATE usage_automatic_turn_eligibility
SET continuation_ref = lower(hex(randomblob(32))),
    retry_reason = 'legacy_unredeemable';

CREATE UNIQUE INDEX IF NOT EXISTS usage_automatic_turn_eligibility_reference_idx
    ON usage_automatic_turn_eligibility(continuation_ref);
