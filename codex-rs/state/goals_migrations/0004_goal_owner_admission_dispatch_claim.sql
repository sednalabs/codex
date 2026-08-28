ALTER TABLE goal_owner_admissions ADD COLUMN dispatch_claim_id TEXT;
ALTER TABLE goal_owner_admissions ADD COLUMN dispatch_claimed_at_ms INTEGER;

CREATE INDEX goal_owner_admissions_dispatch_claim
ON goal_owner_admissions(dispatch_claim_id)
WHERE dispatch_claim_id IS NOT NULL;
