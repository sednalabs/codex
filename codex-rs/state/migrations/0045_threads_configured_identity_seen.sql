-- Record that a settings snapshot established configured-identity provenance.
-- Identity precedence is applied separately by metadata consumers.
ALTER TABLE threads ADD COLUMN configured_identity_seen INTEGER NOT NULL DEFAULT 0;
