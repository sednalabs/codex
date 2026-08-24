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
    provider_id TEXT CHECK(provider_id IS NULL OR length(provider_id) BETWEEN 1 AND 512),
    requested_model TEXT CHECK(requested_model IS NULL OR length(requested_model) BETWEEN 1 AND 512),
    effective_model TEXT CHECK(effective_model IS NULL OR length(effective_model) BETWEEN 1 AND 512),
    account_domain TEXT CHECK(account_domain IS NULL OR length(account_domain) BETWEEN 1 AND 255),
    deadline_at_ms INTEGER NOT NULL,
    attempts_started INTEGER NOT NULL DEFAULT 0 CHECK(attempts_started >= 0),
    max_attempts INTEGER NOT NULL CHECK(max_attempts >= 1),
    cancellation_epoch INTEGER NOT NULL DEFAULT 0 CHECK(cancellation_epoch >= 0),
    phase TEXT NOT NULL CHECK(phase IN ('dormant', 'pending', 'in_flight', 'terminal')),
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
        OR (phase = 'in_flight'
            AND terminal_outcome = 'none'
            AND lease_id IS NOT NULL
            AND deferred_terminal_disposition = 'none')
        OR (phase = 'terminal'
            AND terminal_outcome <> 'none')
    )
);
