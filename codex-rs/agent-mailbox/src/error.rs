use thiserror::Error;

pub type MailboxResult<T> = Result<T, MailboxError>;

#[derive(Debug, Error)]
pub enum MailboxError {
    #[error("mailbox SQLite operation failed: {0}")]
    Sqlite(#[from] sqlx::Error),
    #[error("mailbox JSON encoding failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid mailbox input: {0}")]
    InvalidInput(String),
    #[error("mailbox idempotency key was reused with different input")]
    IdempotencyConflict,
    #[error("mailbox reference does not identify the exact stored work or message")]
    ReferenceMismatch,
    #[error("the addressed request already has a durable reply")]
    RequestAlreadyResolved,
    #[error("mailbox item is not canonically admitted yet")]
    NotCanonical,
    #[error("mailbox item is not owned by the requested recipient")]
    WrongRecipient,
    #[error("mailbox item cannot be replaced after admission or canonicalization")]
    ReplacementNotAllowed,
    #[error("a terminal work outcome has already been recorded")]
    OutcomeAlreadyRecorded,
    #[error("mailbox backpressure limit reached: {0}")]
    Backpressure(&'static str),
    #[error("mailbox schema version {found} is newer than supported version {supported}")]
    NewerSchema { found: i64, supported: i64 },
    #[error("mailbox cursor is malformed")]
    InvalidCursor,
}
