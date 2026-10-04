CREATE TABLE agent_mailbox (
    message_id TEXT PRIMARY KEY NOT NULL,
    sender_instance_id TEXT NOT NULL,
    sender_task_generation TEXT NOT NULL,
    recipient_instance_id TEXT NOT NULL,
    recipient_task_generation TEXT NOT NULL,
    intent TEXT NOT NULL CHECK (intent IN ('progress', 'action_required', 'result_ready', 'acknowledgement')),
    idempotency_key TEXT NOT NULL,
    reply_to TEXT,
    encrypted_payload TEXT NOT NULL,
    enqueue_sequence INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    wait_signalled_at_ms INTEGER,
    wait_returned_at_ms INTEGER,
    context_committed_at_ms INTEGER,
    delivered_at_ms INTEGER,
    acknowledged_at_ms INTEGER,
    acknowledgement_message_id TEXT,
    UNIQUE (sender_instance_id, sender_task_generation, recipient_instance_id, recipient_task_generation, idempotency_key),
    CHECK ((acknowledged_at_ms IS NULL) = (acknowledgement_message_id IS NULL))
);

CREATE TABLE agent_mailbox_supersessions (
    covered_message_id TEXT PRIMARY KEY NOT NULL REFERENCES agent_mailbox(message_id),
    covering_message_id TEXT NOT NULL REFERENCES agent_mailbox(message_id),
    created_at_ms INTEGER NOT NULL,
    CHECK (covered_message_id <> covering_message_id)
);

CREATE INDEX idx_agent_mailbox_recipient_pending_sequence
    ON agent_mailbox (recipient_instance_id, recipient_task_generation, enqueue_sequence)
    WHERE context_committed_at_ms IS NULL AND delivered_at_ms IS NULL;

CREATE INDEX idx_agent_mailbox_reply_to
    ON agent_mailbox (reply_to)
    WHERE reply_to IS NOT NULL;
