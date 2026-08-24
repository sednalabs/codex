CREATE TABLE goal_owner_admission_goal_chains (
    thread_id TEXT NOT NULL,
    goal_id TEXT NOT NULL CHECK(length(goal_id) BETWEEN 1 AND 512),
    attempts_started INTEGER NOT NULL DEFAULT 0 CHECK(attempts_started >= 0),
    max_attempts INTEGER NOT NULL CHECK(max_attempts >= 1),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY(thread_id, goal_id),
    CHECK(attempts_started <= max_attempts)
);

CREATE TABLE goal_owner_admissions (
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
    PRIMARY KEY(thread_id, generation),
    FOREIGN KEY(thread_id, goal_id) REFERENCES goal_owner_admission_goal_chains(thread_id, goal_id),
    CHECK(attempts_started <= max_attempts),
    CHECK((lease_id IS NULL) = (lease_acquired_at_ms IS NULL)),
    CHECK((lease_id IS NULL) = (lease_cancellation_epoch IS NULL)),
    CHECK((retired_at_ms IS NULL) = (retirement_reason IS NULL)),
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
            OR (terminal_outcome IN ('exhausted', 'cancelled')
                AND lease_id IS NULL
                AND deferred_terminal_disposition = 'await_user_turn')
        ))
    )
);

CREATE UNIQUE INDEX goal_owner_admissions_one_active_generation
ON goal_owner_admissions(thread_id)
WHERE retired_at_ms IS NULL;

CREATE TABLE goal_owner_admission_origins (
    thread_id TEXT NOT NULL,
    origin_request_id TEXT NOT NULL CHECK(length(origin_request_id) BETWEEN 1 AND 512),
    generation INTEGER NOT NULL CHECK(generation >= 1),
    goal_id TEXT NOT NULL CHECK(length(goal_id) BETWEEN 1 AND 512),
    origin_turn_id TEXT NOT NULL CHECK(length(origin_turn_id) BETWEEN 1 AND 512),
    denial_class TEXT NOT NULL,
    configured_provider_key TEXT,
    requested_model TEXT,
    effective_provider_id TEXT,
    effective_model TEXT,
    intended_request_kind TEXT NOT NULL,
    successor_turn_id TEXT NOT NULL,
    logical_successor_request_id TEXT NOT NULL,
    decision_id TEXT NOT NULL,
    account_context_fingerprint TEXT,
    deadline_at_ms INTEGER NOT NULL,
    max_attempts INTEGER NOT NULL,
    requested_phase TEXT NOT NULL,
    PRIMARY KEY(thread_id, origin_request_id),
    UNIQUE(thread_id, generation)
);

CREATE TRIGGER goal_owner_admissions_delete_origins
AFTER DELETE ON thread_goals
BEGIN
    DELETE FROM goal_owner_admissions WHERE thread_id = OLD.thread_id;
    DELETE FROM goal_owner_admission_goal_chains WHERE thread_id = OLD.thread_id;
    DELETE FROM goal_owner_admission_origins WHERE thread_id = OLD.thread_id;
END;
