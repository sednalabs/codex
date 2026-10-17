CREATE TABLE IF NOT EXISTS phase2_attestation_roots (
    memory_root_key TEXT PRIMARY KEY,
    required_since INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
