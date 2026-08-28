-- Protocol v3 is incompatible with runtimes that only understand the
-- previous owner/recovery contract. Rebuild the marker because v2's CHECK
-- constraint intentionally admits no downgrade update.
CREATE TABLE goal_owner_runtime_protocol_v3 (
    protocol_key INTEGER PRIMARY KEY CHECK (protocol_key = 1),
    protocol_version INTEGER NOT NULL CHECK (protocol_version = 3)
);

INSERT INTO goal_owner_runtime_protocol_v3 (protocol_key, protocol_version)
VALUES (1, 3);

DROP TABLE goal_owner_runtime_protocol;
ALTER TABLE goal_owner_runtime_protocol_v3 RENAME TO goal_owner_runtime_protocol;
