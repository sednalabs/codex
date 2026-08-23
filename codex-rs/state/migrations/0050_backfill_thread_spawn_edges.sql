WITH source_candidates AS (
    SELECT
        child.id AS child_thread_id,
        CASE
            WHEN json_valid(child.source)
            THEN json_extract(
                child.source,
                '$.subagent.thread_spawn.parent_thread_id'
            )
        END AS parent_thread_id
    FROM threads AS child
    JOIN backfill_state AS backfill
        ON backfill.id = 1 AND backfill.status = 'complete'
)
INSERT OR IGNORE INTO thread_spawn_edges (
    parent_thread_id,
    child_thread_id,
    status
)
SELECT
    source_candidates.parent_thread_id,
    source_candidates.child_thread_id,
    'open'
FROM source_candidates
JOIN threads AS parent
    ON parent.id = source_candidates.parent_thread_id
WHERE source_candidates.parent_thread_id IS NOT NULL
  AND source_candidates.parent_thread_id != source_candidates.child_thread_id;
