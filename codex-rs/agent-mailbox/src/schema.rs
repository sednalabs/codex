use crate::error::MailboxResult;
use sqlx::Row;
use sqlx::SqlitePool;

const SCHEMA_VERSION: i64 = 5;

pub(crate) async fn initialize(pool: &SqlitePool) -> MailboxResult<()> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_schema (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            version INTEGER NOT NULL
        )",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_counts (
            name TEXT PRIMARY KEY,
            item_count INTEGER NOT NULL
        )",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO mailbox_counts(name, item_count) VALUES ('work', 0), ('message', 0)
         ON CONFLICT(name) DO NOTHING",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_pending_counts (
            recipient TEXT PRIMARY KEY,
            item_count INTEGER NOT NULL,
            quiet_count INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(&mut *transaction)
    .await?;

    let version: Option<i64> =
        sqlx::query_scalar("SELECT version FROM mailbox_schema WHERE singleton = 1")
            .fetch_optional(&mut *transaction)
            .await?;
    if let Some(version) = version
        && version > SCHEMA_VERSION
    {
        return Err(crate::MailboxError::NewerSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_work (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            id TEXT NOT NULL UNIQUE,
            issuer TEXT NOT NULL,
            agent TEXT NOT NULL,
            submission_id TEXT NOT NULL,
            native_key TEXT NOT NULL,
            request_json TEXT NOT NULL,
            input_json TEXT NOT NULL,
            input_message_id TEXT NOT NULL,
            dispatch_state TEXT NOT NULL,
            outcome_json TEXT,
            outcome_id TEXT,
            outcome_canonical INTEGER NOT NULL DEFAULT 0,
            outcome_observed INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(&mut *transaction)
    .await?;

    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS mailbox_work_submission_unique
         ON mailbox_work(agent, submission_id)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS mailbox_work_input_id
         ON mailbox_work(input_message_id)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS mailbox_work_native_key
         ON mailbox_work(issuer, native_key)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_work_submission
         ON mailbox_work(agent, submission_id, seq)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_work_agent_input
         ON mailbox_work(agent, input_message_id, seq)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_work_issuer_agent
         ON mailbox_work(issuer, agent, seq)",
    )
    .execute(&mut *transaction)
    .await?;
    for index in [
        "CREATE INDEX IF NOT EXISTS mailbox_work_unobserved ON mailbox_work(issuer, outcome_observed, seq)",
        "CREATE INDEX IF NOT EXISTS mailbox_work_agent_unobserved ON mailbox_work(issuer, agent, outcome_observed, seq)",
        "CREATE INDEX IF NOT EXISTS mailbox_work_outcome ON mailbox_work(issuer, outcome_id, seq)",
        "CREATE INDEX IF NOT EXISTS mailbox_work_inflight ON mailbox_work(issuer, dispatch_state, seq) WHERE outcome_json IS NULL",
    ] {
        sqlx::query(index).execute(&mut *transaction).await?;
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_admissions (
            recipient TEXT NOT NULL,
            source_turn_id TEXT NOT NULL,
            work_id TEXT NOT NULL REFERENCES mailbox_work(id),
            input_message_id TEXT NOT NULL,
            PRIMARY KEY (recipient, source_turn_id, input_message_id),
            UNIQUE (work_id)
        )",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_input_admissions (
            recipient TEXT NOT NULL,
            source_turn_id TEXT NOT NULL,
            input_message_id TEXT NOT NULL,
            work_id TEXT REFERENCES mailbox_work(id),
            PRIMARY KEY (recipient, input_message_id),
            UNIQUE (work_id)
        )",
    )
    .execute(&mut *transaction)
    .await?;
    if version.is_some_and(|version| version < SCHEMA_VERSION) {
        sqlx::query(
            "INSERT OR IGNORE INTO mailbox_input_admissions(
                 recipient, source_turn_id, input_message_id, work_id
             ) SELECT recipient, source_turn_id, input_message_id, work_id
               FROM mailbox_admissions",
        )
        .execute(&mut *transaction)
        .await?;
    }
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_input_admissions_turn
         ON mailbox_input_admissions(recipient, source_turn_id, input_message_id)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_continuations (
            work_id TEXT NOT NULL REFERENCES mailbox_work(id),
            binding_json TEXT NOT NULL,
            state TEXT NOT NULL,
            outcome TEXT NOT NULL DEFAULT 'needs_attention',
            source_turn_id TEXT NOT NULL,
            response_message_id TEXT,
            PRIMARY KEY (work_id, binding_json)
        )",
    )
    .execute(&mut *transaction)
    .await?;
    ensure_column(&mut transaction, "mailbox_continuations", "outcome").await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_continuations_binding
         ON mailbox_continuations(binding_json, state, work_id)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_admissions_turn
         ON mailbox_admissions(recipient, source_turn_id, work_id)",
    )
    .execute(&mut *transaction)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_messages (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            id TEXT NOT NULL UNIQUE,
            invocation_key TEXT NOT NULL,
            request_json TEXT NOT NULL,
            sender TEXT NOT NULL,
            recipient TEXT NOT NULL,
            author_path TEXT NOT NULL DEFAULT '',
            recipient_path TEXT NOT NULL DEFAULT '',
            communication_json TEXT NOT NULL,
            kind TEXT NOT NULL,
            work_ref_json TEXT,
            work_id TEXT REFERENCES mailbox_work(id),
            reply_to TEXT,
            supersedes TEXT,
            superseded_by TEXT,
            withdrawn INTEGER NOT NULL DEFAULT 0,
            activate_if_idle INTEGER NOT NULL DEFAULT 0,
            input_admitted INTEGER NOT NULL DEFAULT 0,
            legacy_attention INTEGER NOT NULL DEFAULT 0,
            canonical INTEGER NOT NULL DEFAULT 0,
            observed INTEGER NOT NULL DEFAULT 0,
            FOREIGN KEY (reply_to) REFERENCES mailbox_messages(id),
            FOREIGN KEY (supersedes) REFERENCES mailbox_messages(id),
            FOREIGN KEY (superseded_by) REFERENCES mailbox_messages(id)
        )",
    )
    .execute(&mut *transaction)
    .await?;
    ensure_column(&mut transaction, "mailbox_messages", "withdrawn").await?;
    ensure_column(&mut transaction, "mailbox_messages", "activate_if_idle").await?;
    ensure_column(&mut transaction, "mailbox_messages", "input_admitted").await?;
    ensure_column(&mut transaction, "mailbox_messages", "author_path").await?;
    ensure_column(&mut transaction, "mailbox_messages", "recipient_path").await?;
    ensure_column(&mut transaction, "mailbox_messages", "work_id").await?;
    ensure_column(&mut transaction, "mailbox_pending_counts", "quiet_count").await?;
    if version.is_some_and(|version| version < SCHEMA_VERSION) {
        sqlx::query(
            "UPDATE mailbox_pending_counts SET
               item_count = (
                 SELECT COUNT(*) FROM mailbox_messages AS message
                 WHERE message.recipient = mailbox_pending_counts.recipient
                   AND (message.canonical = 0 OR message.input_admitted = 0)
                   AND message.superseded_by IS NULL
                   AND message.withdrawn = 0
               ),
               quiet_count = (
                 SELECT COUNT(*) FROM mailbox_messages AS message
                 WHERE message.recipient = mailbox_pending_counts.recipient
                   AND (message.canonical = 0 OR message.input_admitted = 0)
                   AND message.superseded_by IS NULL
                   AND message.withdrawn = 0
                   AND message.kind IN ('\"note\"', '\"progress\"')
                   AND (message.kind = '\"progress\"' OR message.legacy_attention = 0)
               )",
        )
        .execute(&mut *transaction)
        .await?;
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_invocations (
            invocation_key TEXT PRIMARY KEY,
            request_json TEXT NOT NULL,
            message_id TEXT NOT NULL REFERENCES mailbox_messages(id)
        )",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_messages_pending
         ON mailbox_messages(recipient, canonical, seq)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_messages_active_unread
         ON mailbox_messages(recipient, seq)
         WHERE (canonical = 0 OR input_admitted = 0)
           AND superseded_by IS NULL AND withdrawn = 0",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_messages_unadmitted
         ON mailbox_messages(recipient, canonical, input_admitted, seq)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_messages_work
         ON mailbox_messages(recipient, seq)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_messages_admitted_work
         ON mailbox_messages(recipient, work_id, input_admitted, seq)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_messages_reply_lookup
         ON mailbox_messages(reply_to, recipient, seq)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS mailbox_messages_reply_once
         ON mailbox_messages(reply_to) WHERE reply_to IS NOT NULL",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_attention (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            id TEXT NOT NULL UNIQUE,
            recipient TEXT NOT NULL,
            canonical INTEGER NOT NULL DEFAULT 0,
            observed INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(&mut *transaction)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mailbox_activations (
            message_id TEXT PRIMARY KEY REFERENCES mailbox_messages(id),
            sender TEXT NOT NULL,
            state TEXT NOT NULL,
            disposition TEXT
        )",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO mailbox_activations(message_id, sender, state, disposition)
         SELECT id, sender, 'decided', 'not_requested' FROM mailbox_messages
         WHERE activate_if_idle = 0",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS mailbox_attention_pending
         ON mailbox_attention(recipient, canonical, observed, seq)",
    )
    .execute(&mut *transaction)
    .await?;

    sqlx::query(
        "INSERT INTO mailbox_schema(singleton, version) VALUES (1, ?)
         ON CONFLICT(singleton) DO UPDATE SET version = excluded.version",
    )
    .bind(SCHEMA_VERSION)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn ensure_column(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    column: &str,
) -> MailboxResult<()> {
    let pragma = match table {
        "mailbox_continuations" => "PRAGMA table_info(mailbox_continuations)",
        "mailbox_messages" => "PRAGMA table_info(mailbox_messages)",
        "mailbox_pending_counts" => "PRAGMA table_info(mailbox_pending_counts)",
        _ => {
            return Err(crate::MailboxError::InvalidInput(
                "unsupported mailbox schema table".into(),
            ));
        }
    };
    let rows = sqlx::query(pragma).fetch_all(&mut **transaction).await?;
    let exists = rows
        .iter()
        .map(|row| row.try_get::<String, _>("name"))
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == column);
    if !exists {
        let alter = match (table, column) {
            ("mailbox_continuations", "outcome") => {
                "ALTER TABLE mailbox_continuations ADD COLUMN outcome TEXT NOT NULL DEFAULT 'needs_attention'"
            }
            ("mailbox_messages", "withdrawn") => {
                "ALTER TABLE mailbox_messages ADD COLUMN withdrawn INTEGER NOT NULL DEFAULT 0"
            }
            ("mailbox_messages", "activate_if_idle") => {
                "ALTER TABLE mailbox_messages ADD COLUMN activate_if_idle INTEGER NOT NULL DEFAULT 0"
            }
            ("mailbox_messages", "input_admitted") => {
                "ALTER TABLE mailbox_messages ADD COLUMN input_admitted INTEGER NOT NULL DEFAULT 0"
            }
            ("mailbox_messages", "author_path") => {
                "ALTER TABLE mailbox_messages ADD COLUMN author_path TEXT NOT NULL DEFAULT ''"
            }
            ("mailbox_messages", "recipient_path") => {
                "ALTER TABLE mailbox_messages ADD COLUMN recipient_path TEXT NOT NULL DEFAULT ''"
            }
            ("mailbox_messages", "work_id") => {
                "ALTER TABLE mailbox_messages ADD COLUMN work_id TEXT REFERENCES mailbox_work(id)"
            }
            ("mailbox_pending_counts", "quiet_count") => {
                "ALTER TABLE mailbox_pending_counts ADD COLUMN quiet_count INTEGER NOT NULL DEFAULT 0"
            }
            _ => {
                return Err(crate::MailboxError::InvalidInput(
                    "unsupported mailbox schema column".into(),
                ));
            }
        };
        sqlx::query(alter).execute(&mut **transaction).await?;
    }
    Ok(())
}
