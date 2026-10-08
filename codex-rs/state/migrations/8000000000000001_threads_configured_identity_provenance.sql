-- Keep configured-identity authority private from generic thread metadata.
-- Existing rows and older binaries that omit the column remain unknown.
ALTER TABLE threads ADD COLUMN configured_identity_provenance INTEGER NOT NULL DEFAULT 0
    CHECK (configured_identity_provenance IN (0, 1, 2));
