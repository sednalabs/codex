-- Runtime Custodian protocol v4. The installed process capability is still
-- recorded separately, while this canonical per-thread row carries the only
-- durable owner generation. It is intentionally independent of the weak live
-- registry, so dropping an in-process handle can never recreate authority.
CREATE TABLE goal_runtime_thread_lifecycles (
    thread_id TEXT PRIMARY KEY,
    installation_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation >= 1),
    phase TEXT NOT NULL CHECK(phase IN ('active', 'draining', 'retired', 'retired_crash')),
    transition_id TEXT,
    updated_at_ms INTEGER NOT NULL,
    CHECK(
        (phase = 'draining' AND transition_id IS NOT NULL)
        OR (phase != 'draining' AND transition_id IS NULL)
    )
);

CREATE INDEX goal_runtime_thread_lifecycles_installation
ON goal_runtime_thread_lifecycles(installation_id, phase);

-- Canonical thread deletion reclaims lifecycle tombstones only after every
-- durable provider boundary is settled. There is no TTL or process-memory
-- based reclamation path.
CREATE TRIGGER goal_runtime_thread_lifecycles_reject_unsettled_thread_delete
BEFORE DELETE ON thread_goals
WHEN EXISTS (
    SELECT 1
    FROM goal_owner_admissions
    WHERE thread_id = OLD.thread_id
      AND (
          phase IN ('acquired', 'in_flight')
          OR (phase = 'terminal' AND terminal_outcome = 'uncertain')
      )
)
BEGIN
    SELECT RAISE(ABORT, 'cannot reclaim a goal runtime lifecycle with unsettled work');
END;

CREATE TRIGGER goal_runtime_thread_lifecycles_delete_on_canonical_thread_delete
AFTER DELETE ON thread_goals
BEGIN
    DELETE FROM goal_runtime_thread_lifecycles WHERE thread_id = OLD.thread_id;
END;

CREATE TABLE goal_owner_runtime_protocol_v4 (
    protocol_key INTEGER PRIMARY KEY CHECK (protocol_key = 1),
    protocol_version INTEGER NOT NULL CHECK (protocol_version = 4)
);

INSERT INTO goal_owner_runtime_protocol_v4 (protocol_key, protocol_version)
VALUES (1, 4);

DROP TABLE goal_owner_runtime_protocol;
ALTER TABLE goal_owner_runtime_protocol_v4 RENAME TO goal_owner_runtime_protocol;
