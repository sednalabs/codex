-- One durable control bit per thread; message contents and cursors stay with their source.
CREATE TABLE thread_external_notice_gate (
    thread_id TEXT PRIMARY KEY NOT NULL,
    stopped INTEGER NOT NULL CHECK (stopped IN (0, 1))
);
