CREATE TABLE goal_owner_runtime_owners (
    owner_key INTEGER PRIMARY KEY CHECK (owner_key = 1),
    owner_id TEXT NOT NULL,
    acquired_at_ms INTEGER NOT NULL
);
