-- `in_flight` is the durable request-open fence, not proof that a transport
-- received the request. Permit the exact fence winner to record a proved
-- pre-transport cancellation without pretending that an operator must take a
-- later user-turn action. Existing cancellation rows retain their original
-- `await_user_turn` disposition.
DROP INDEX goal_owner_admissions_one_active_generation;
DROP INDEX goal_owner_admissions_dispatch_claim;
DROP TRIGGER goal_owner_admissions_delete_origins;
DROP TRIGGER goal_owner_admissions_delete_last_history;

CREATE TABLE goal_owner_admissions_rebuilt (
    thread_id TEXT NOT NULL,
    goal_id TEXT NOT NULL CHECK(length(goal_id) BETWEEN 1 AND 512),
    generation INTEGER NOT NULL CHECK(generation >= 1),
    origin_turn_id TEXT NOT NULL CHECK(length(origin_turn_id) BETWEEN 1 AND 512),
    origin_request_id TEXT NOT NULL CHECK(length(origin_request_id) BETWEEN 1 AND 512),
    denial_class TEXT NOT NULL CHECK(denial_class IN (
        'capacity',
        'rate_limited',
        'provider_unavailable',
        'policy_denied',
        'authentication_denied'
    )),
    configured_provider_key TEXT CHECK(configured_provider_key IS NULL OR length(configured_provider_key) BETWEEN 1 AND 512),
    requested_model TEXT CHECK(requested_model IS NULL OR length(requested_model) BETWEEN 1 AND 512),
    effective_provider_id TEXT CHECK(effective_provider_id IS NULL OR length(effective_provider_id) BETWEEN 1 AND 512),
    effective_model TEXT CHECK(effective_model IS NULL OR length(effective_model) BETWEEN 1 AND 512),
    intended_request_kind TEXT NOT NULL CHECK(length(intended_request_kind) BETWEEN 1 AND 512),
    successor_turn_id TEXT NOT NULL CHECK(length(successor_turn_id) BETWEEN 1 AND 512),
    logical_successor_request_id TEXT NOT NULL CHECK(length(logical_successor_request_id) BETWEEN 1 AND 512),
    decision_id TEXT NOT NULL CHECK(length(decision_id) BETWEEN 1 AND 512),
    account_context_fingerprint TEXT CHECK(
        account_context_fingerprint IS NULL OR (
            length(account_context_fingerprint) = 64
            AND account_context_fingerprint NOT GLOB '*[^0-9a-f]*'
        )
    ),
    deadline_at_ms INTEGER NOT NULL,
    attempts_started INTEGER NOT NULL DEFAULT 0 CHECK(attempts_started >= 0),
    max_attempts INTEGER NOT NULL CHECK(max_attempts >= 1),
    cancellation_epoch INTEGER NOT NULL DEFAULT 0 CHECK(cancellation_epoch >= 0),
    requested_phase TEXT NOT NULL CHECK(requested_phase IN ('dormant', 'pending')),
    phase TEXT NOT NULL CHECK(phase IN ('dormant', 'pending', 'acquired', 'in_flight', 'terminal')),
    terminal_outcome TEXT NOT NULL CHECK(terminal_outcome IN (
        'none',
        'succeeded',
        'rejected',
        'exhausted',
        'cancelled',
        'uncertain'
    )),
    lease_id TEXT,
    lease_acquired_at_ms INTEGER,
    lease_cancellation_epoch INTEGER,
    deferred_terminal_disposition TEXT NOT NULL CHECK(deferred_terminal_disposition IN (
        'none',
        'await_user_turn',
        'manual_review'
    )),
    retired_at_ms INTEGER,
    retirement_reason TEXT CHECK(retirement_reason IN ('superseded', 'user_recovery')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    dispatch_claim_id TEXT,
    dispatch_claimed_at_ms INTEGER,
    uncertainty_resolution_evidence TEXT,
    uncertainty_resolved_at_ms INTEGER,
    dispatch_fence_id TEXT,
    PRIMARY KEY(thread_id, generation),
    FOREIGN KEY(thread_id, goal_id) REFERENCES goal_owner_admission_goal_chains(thread_id, goal_id),
    FOREIGN KEY(thread_id, generation, goal_id, origin_request_id)
        REFERENCES goal_owner_admission_origins(thread_id, generation, goal_id, origin_request_id),
    CHECK(attempts_started <= max_attempts),
    CHECK((lease_id IS NULL) = (lease_acquired_at_ms IS NULL)),
    CHECK((lease_id IS NULL) = (lease_cancellation_epoch IS NULL)),
    CHECK((retired_at_ms IS NULL) = (retirement_reason IS NULL)),
    CHECK(
        retired_at_ms IS NULL
        OR (phase NOT IN ('acquired', 'in_flight') AND terminal_outcome != 'uncertain')
    ),
    CHECK(
        (phase IN ('dormant', 'pending')
            AND terminal_outcome = 'none'
            AND lease_id IS NULL
            AND deferred_terminal_disposition = 'none')
        OR (phase IN ('acquired', 'in_flight')
            AND terminal_outcome = 'none'
            AND lease_id IS NOT NULL
            AND attempts_started > 0
            AND deferred_terminal_disposition = 'none')
        OR (phase = 'terminal' AND (
            (terminal_outcome IN ('succeeded', 'rejected')
                AND lease_id IS NOT NULL
                AND attempts_started > 0
                AND deferred_terminal_disposition = 'none')
            OR (terminal_outcome = 'uncertain'
                AND lease_id IS NOT NULL
                AND attempts_started > 0
                AND deferred_terminal_disposition = 'manual_review')
            OR (terminal_outcome = 'exhausted'
                AND lease_id IS NULL
                AND deferred_terminal_disposition = 'await_user_turn')
            OR (terminal_outcome = 'cancelled'
                AND lease_id IS NULL
                AND deferred_terminal_disposition IN ('none', 'await_user_turn'))
        ))
    )
);

INSERT INTO goal_owner_admissions_rebuilt (
    thread_id, goal_id, generation, origin_turn_id, origin_request_id, denial_class,
    configured_provider_key, requested_model, effective_provider_id, effective_model,
    intended_request_kind, successor_turn_id, logical_successor_request_id, decision_id,
    account_context_fingerprint, deadline_at_ms, attempts_started, max_attempts,
    cancellation_epoch, requested_phase, phase, terminal_outcome, lease_id,
    lease_acquired_at_ms, lease_cancellation_epoch, deferred_terminal_disposition,
    retired_at_ms, retirement_reason, created_at_ms, updated_at_ms, dispatch_claim_id,
    dispatch_claimed_at_ms, uncertainty_resolution_evidence, uncertainty_resolved_at_ms,
    dispatch_fence_id
)
SELECT
    thread_id, goal_id, generation, origin_turn_id, origin_request_id, denial_class,
    configured_provider_key, requested_model, effective_provider_id, effective_model,
    intended_request_kind, successor_turn_id, logical_successor_request_id, decision_id,
    account_context_fingerprint, deadline_at_ms, attempts_started, max_attempts,
    cancellation_epoch, requested_phase, phase, terminal_outcome, lease_id,
    lease_acquired_at_ms, lease_cancellation_epoch, deferred_terminal_disposition,
    retired_at_ms, retirement_reason, created_at_ms, updated_at_ms, dispatch_claim_id,
    dispatch_claimed_at_ms, uncertainty_resolution_evidence, uncertainty_resolved_at_ms,
    dispatch_fence_id
FROM goal_owner_admissions;

DROP TABLE goal_owner_admissions;
ALTER TABLE goal_owner_admissions_rebuilt RENAME TO goal_owner_admissions;

CREATE UNIQUE INDEX goal_owner_admissions_one_active_generation
ON goal_owner_admissions(thread_id)
WHERE retired_at_ms IS NULL;

CREATE INDEX goal_owner_admissions_dispatch_claim
ON goal_owner_admissions(dispatch_claim_id)
WHERE dispatch_claim_id IS NOT NULL;

CREATE TRIGGER goal_owner_admissions_delete_origins
AFTER DELETE ON thread_goals
BEGIN
    DELETE FROM goal_owner_admissions WHERE thread_id = OLD.thread_id;
END;

CREATE TRIGGER goal_owner_admissions_delete_last_history
AFTER DELETE ON goal_owner_admissions
WHEN NOT EXISTS (
    SELECT 1 FROM goal_owner_admissions WHERE thread_id = OLD.thread_id
)
BEGIN
    DELETE FROM goal_owner_admission_goal_chains WHERE thread_id = OLD.thread_id;
    DELETE FROM goal_owner_admission_origins WHERE thread_id = OLD.thread_id;
END;
