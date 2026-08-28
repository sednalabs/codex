-- Goal-owner recovery protocol v2. New runtimes must not infer recovery
-- authority from a process-local flag or from a legacy database boundary.
CREATE TABLE goal_owner_runtime_protocol (
    protocol_key INTEGER PRIMARY KEY CHECK (protocol_key = 1),
    protocol_version INTEGER NOT NULL CHECK (protocol_version = 2)
);

INSERT INTO goal_owner_runtime_protocol (protocol_key, protocol_version)
VALUES (1, 2);
