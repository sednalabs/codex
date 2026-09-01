CREATE TABLE goal_owner_admissions (
    thread_id TEXT PRIMARY KEY NOT NULL,
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
    deferred_terminal_disposition TEXT NOT NULL CHECK(deferred_terminal_disposition IN (
        'none',
        'await_user_turn',
        'manual_review'
    )),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK(attempts_started <= max_attempts),
    CHECK((lease_id IS NULL) = (lease_acquired_at_ms IS NULL)),
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
            (terminal_outcome IN ('succeeded', 'rejected', 'exhausted')
                AND lease_id IS NOT NULL
                AND attempts_started > 0
                AND deferred_terminal_disposition = 'none')
            OR (terminal_outcome = 'uncertain'
                AND lease_id IS NOT NULL
                AND attempts_started > 0
                AND deferred_terminal_disposition = 'manual_review')
            OR (terminal_outcome = 'cancelled'
                AND deferred_terminal_disposition IN ('await_user_turn', 'manual_review'))
        ))
    )
);

CREATE TABLE goal_owner_admission_origins (
    thread_id TEXT NOT NULL,
    origin_request_id TEXT NOT NULL CHECK(length(origin_request_id) BETWEEN 1 AND 512),
    goal_id TEXT NOT NULL CHECK(length(goal_id) BETWEEN 1 AND 512),
    origin_turn_id TEXT NOT NULL CHECK(length(origin_turn_id) BETWEEN 1 AND 512),
    denial_class TEXT NOT NULL CHECK(denial_class IN (
        'capacity', 'rate_limited', 'provider_unavailable',
        'policy_denied', 'authentication_denied'
    )),
    deadline_at_ms INTEGER NOT NULL,
    max_attempts INTEGER NOT NULL CHECK(max_attempts >= 1),
    requested_phase TEXT NOT NULL CHECK(requested_phase IN ('dormant', 'pending')),
    PRIMARY KEY(thread_id, origin_request_id)
);

CREATE TRIGGER goal_owner_admissions_delete_origins
AFTER DELETE ON thread_goals
BEGIN
    DELETE FROM goal_owner_admissions WHERE thread_id = OLD.thread_id;
    DELETE FROM goal_owner_admission_origins WHERE thread_id = OLD.thread_id;
END;
