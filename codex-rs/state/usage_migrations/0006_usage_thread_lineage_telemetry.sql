-- Persist the unambiguous relationship kind and spawn request on each thread.
-- Both fields are nullable so older ledgers remain readable and can be upgraded
-- without reconstructing historical provenance.
ALTER TABLE usage_threads ADD COLUMN lineage_edge_kind TEXT;
ALTER TABLE usage_threads ADD COLUMN spawn_request_id TEXT;

CREATE INDEX IF NOT EXISTS usage_threads_root_thread_idx
ON usage_threads(root_thread_id);

CREATE INDEX IF NOT EXISTS usage_threads_spawn_request_idx
ON usage_threads(spawn_request_id);
