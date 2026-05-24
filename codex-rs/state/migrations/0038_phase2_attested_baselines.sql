CREATE TABLE IF NOT EXISTS phase2_attested_baselines (
    memory_root_key TEXT NOT NULL,
    output_tree_sha256 TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    selection_sha256 TEXT NOT NULL,
    prepared_inputs_sha256 TEXT NOT NULL,
    consolidator_sha256 TEXT NOT NULL,
    completion_watermark INTEGER NOT NULL,
    selected_count INTEGER NOT NULL,
    attested_at INTEGER NOT NULL,
    PRIMARY KEY(memory_root_key, output_tree_sha256)
);

CREATE INDEX IF NOT EXISTS idx_phase2_attested_baselines_root_attested_at
    ON phase2_attested_baselines(memory_root_key, attested_at);
