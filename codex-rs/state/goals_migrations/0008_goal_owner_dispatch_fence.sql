-- Bind each durable dispatch claim to the exact in-memory continuation fence
-- that minted its successor token. Legacy claims remain non-authoritative
-- until a new claim records this identity.
ALTER TABLE goal_owner_admissions ADD COLUMN dispatch_fence_id TEXT;

-- A pre-v2 dispatch claim has no fence identity and therefore cannot be
-- replayed safely. It was only a publication reservation (not a provider
-- lease), so return it to pending and let the current scheduler claim it.
UPDATE goal_owner_admissions
SET dispatch_claim_id = NULL,
    dispatch_claimed_at_ms = NULL
WHERE dispatch_claim_id IS NOT NULL;
