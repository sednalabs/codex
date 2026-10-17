-- Empty account_scope denotes an unobserved stable provider-account identity.
-- In all cases, session/thread remains part of the response-id key.
CREATE TABLE usage_response_idempotency (
    provider TEXT NOT NULL,
    account_scope TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    response_id TEXT NOT NULL,
    provider_call_id TEXT NOT NULL UNIQUE,
    PRIMARY KEY (provider, account_scope, thread_id, response_id)
);
