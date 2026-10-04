use super::datetime_to_epoch_millis;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;
use sqlx::SqlitePool;
use std::sync::Arc;
use uuid::Uuid;

const MAX_SUPERSEDED_PROGRESS: usize = 64;

#[derive(Clone)]
pub struct AgentMailboxStore {
    pool: Arc<SqlitePool>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxIntent {
    Progress,
    ActionRequired,
    ResultReady,
    Acknowledgement,
}

impl MailboxIntent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::ActionRequired => "action_required",
            Self::ResultReady => "result_ready",
            Self::Acknowledgement => "acknowledgement",
        }
    }

    fn may_supersede_progress(self) -> bool {
        matches!(
            self,
            Self::Progress | Self::ActionRequired | Self::ResultReady
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MailboxStage {
    WaitSignalled,
    WaitReturned,
    ContextCommitted,
    Delivered,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MailboxSendRequest {
    pub sender_instance_id: String,
    pub sender_task_generation: String,
    pub recipient_instance_id: String,
    pub recipient_task_generation: String,
    pub intent: MailboxIntent,
    /// Opaque payload as already protected by the caller's existing privacy layer.
    pub encrypted_payload: String,
    /// Stable tool-call identity when the caller omits an explicit key.
    pub idempotency_key: String,
    pub reply_to: Option<String>,
    pub supersedes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MailboxMessage {
    pub message_id: String,
    pub sender_instance_id: String,
    pub sender_task_generation: String,
    pub recipient_instance_id: String,
    pub recipient_task_generation: String,
    pub intent: MailboxIntent,
    pub encrypted_payload: String,
    pub idempotency_key: String,
    pub reply_to: Option<String>,
    pub enqueue_sequence: i64,
    pub created_at_ms: i64,
    pub wait_signalled_at_ms: Option<i64>,
    pub wait_returned_at_ms: Option<i64>,
    pub context_committed_at_ms: Option<i64>,
    pub delivered_at_ms: Option<i64>,
    pub acknowledged_at_ms: Option<i64>,
    pub acknowledgement_message_id: Option<String>,
    pub superseded_by: Vec<String>,
    pub supersedes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MailboxAcceptOutcome {
    Accepted(MailboxMessage),
    AlreadyAccepted(MailboxMessage),
}

impl AgentMailboxStore {
    pub(crate) fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    /// Durably accept one immutable message and its explicit progress coverage.
    /// This records custody only; it does not signal a waiter or start a turn.
    pub async fn accept(
        &self,
        request: MailboxSendRequest,
    ) -> anyhow::Result<MailboxAcceptOutcome> {
        validate_request(&request)?;
        let mut transaction = self.pool.begin().await?;
        if let Some(existing) = find_by_idempotency(&mut transaction, &request).await? {
            if !same_operation(&existing, &request) {
                anyhow::bail!("mailbox idempotency key conflicts with an accepted operation");
            }
            transaction.commit().await?;
            return Ok(MailboxAcceptOutcome::AlreadyAccepted(existing));
        }

        let reply_target = validate_reply(&mut transaction, &request).await?;
        let covered = validate_supersession(&mut transaction, &request).await?;
        let message_id = Uuid::now_v7().to_string();
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let sequence: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(enqueue_sequence), 0) + 1 FROM agent_mailbox")
                .fetch_one(transaction.as_mut())
                .await?;
        let row = sqlx::query(
            "INSERT INTO agent_mailbox (
                message_id, sender_instance_id, sender_task_generation,
                recipient_instance_id, recipient_task_generation, intent,
                idempotency_key, reply_to, encrypted_payload, enqueue_sequence,
                created_at_ms, acknowledged_at_ms, acknowledgement_message_id
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING *",
        )
        .bind(&message_id)
        .bind(&request.sender_instance_id)
        .bind(&request.sender_task_generation)
        .bind(&request.recipient_instance_id)
        .bind(&request.recipient_task_generation)
        .bind(request.intent.as_str())
        .bind(&request.idempotency_key)
        .bind(&request.reply_to)
        .bind(&request.encrypted_payload)
        .bind(sequence)
        .bind(now_ms)
        .bind(Option::<i64>::None)
        .bind(Option::<String>::None)
        .fetch_one(transaction.as_mut())
        .await?;
        if request.intent == MailboxIntent::Acknowledgement
            && let Some(reply_target) = reply_target
        {
            let affected = sqlx::query(
                "UPDATE agent_mailbox
                 SET acknowledged_at_ms = ?, acknowledgement_message_id = ?
                 WHERE message_id = ? AND acknowledged_at_ms IS NULL",
            )
            .bind(now_ms)
            .bind(&message_id)
            .bind(reply_target)
            .execute(transaction.as_mut())
            .await?
            .rows_affected();
            if affected != 1 {
                anyhow::bail!("mailbox acknowledgement target changed before commit");
            }
        }
        for covered_id in covered {
            sqlx::query(
                "INSERT INTO agent_mailbox_supersessions
                 (covered_message_id, covering_message_id, created_at_ms)
                 VALUES (?, ?, ?)",
            )
            .bind(covered_id)
            .bind(&message_id)
            .bind(now_ms)
            .execute(transaction.as_mut())
            .await?;
        }
        transaction.commit().await?;
        let mut message = message_from_row(&row)?;
        message.supersedes = request.supersedes;
        Ok(MailboxAcceptOutcome::Accepted(message))
    }

    pub async fn get(&self, message_id: &str) -> anyhow::Result<Option<MailboxMessage>> {
        let row = sqlx::query("SELECT * FROM agent_mailbox WHERE message_id = ?")
            .bind(message_id)
            .fetch_optional(self.pool.as_ref())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut message = message_from_row(&row)?;
        message.superseded_by = superseded_by(self.pool.as_ref(), message_id).await?;
        message.supersedes = supersedes(self.pool.as_ref(), message_id).await?;
        Ok(Some(message))
    }

    /// Return bounded pending originals for the exact recipient instance/generation.
    pub async fn list_pending(
        &self,
        recipient_instance_id: &str,
        recipient_task_generation: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MailboxMessage>> {
        let rows = sqlx::query(
            "SELECT m.* FROM agent_mailbox AS m
             WHERE m.recipient_instance_id = ?
               AND m.recipient_task_generation = ?
               AND m.context_committed_at_ms IS NULL
               AND m.delivered_at_ms IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM agent_mailbox_supersessions AS s
                   WHERE s.covered_message_id = m.message_id
               )
             ORDER BY m.enqueue_sequence LIMIT ?",
        )
        .bind(recipient_instance_id)
        .bind(recipient_task_generation)
        .bind(i64::try_from(limit)?)
        .fetch_all(self.pool.as_ref())
        .await?;
        let mut messages = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut message = message_from_row(row)?;
            message.superseded_by = superseded_by(self.pool.as_ref(), &message.message_id).await?;
            message.supersedes = supersedes(self.pool.as_ref(), &message.message_id).await?;
            messages.push(message);
        }
        Ok(messages)
    }

    /// Record an observed lifecycle boundary without inferring any earlier stage.
    pub async fn record_stage(
        &self,
        message_id: &str,
        stage: MailboxStage,
    ) -> anyhow::Result<bool> {
        let query = match stage {
            MailboxStage::WaitSignalled => {
                "UPDATE agent_mailbox SET wait_signalled_at_ms = COALESCE(wait_signalled_at_ms, ?)
             WHERE message_id = ?"
            }
            MailboxStage::WaitReturned => {
                "UPDATE agent_mailbox SET wait_returned_at_ms = COALESCE(wait_returned_at_ms, ?)
             WHERE message_id = ?"
            }
            MailboxStage::ContextCommitted => {
                "UPDATE agent_mailbox SET context_committed_at_ms = COALESCE(context_committed_at_ms, ?)
             WHERE message_id = ?"
            }
            MailboxStage::Delivered => {
                "UPDATE agent_mailbox SET delivered_at_ms = COALESCE(delivered_at_ms, ?)
             WHERE message_id = ?"
            }
        };
        Ok(sqlx::query(query)
            .bind(datetime_to_epoch_millis(Utc::now()))
            .bind(message_id)
            .execute(self.pool.as_ref())
            .await?
            .rows_affected()
            > 0)
    }
}

fn validate_request(request: &MailboxSendRequest) -> anyhow::Result<()> {
    for (field, value) in [
        ("sender_instance_id", request.sender_instance_id.as_str()),
        (
            "sender_task_generation",
            request.sender_task_generation.as_str(),
        ),
        (
            "recipient_instance_id",
            request.recipient_instance_id.as_str(),
        ),
        (
            "recipient_task_generation",
            request.recipient_task_generation.as_str(),
        ),
        ("idempotency_key", request.idempotency_key.as_str()),
    ] {
        if value.trim().is_empty() {
            anyhow::bail!("mailbox {field} must not be empty");
        }
    }
    if request.supersedes.len() > MAX_SUPERSEDED_PROGRESS {
        anyhow::bail!("mailbox supersedes list exceeds {MAX_SUPERSEDED_PROGRESS} entries");
    }
    let mut ids = request.supersedes.clone();
    ids.sort();
    ids.dedup();
    if ids.len() != request.supersedes.len() {
        anyhow::bail!("mailbox supersedes list contains duplicate message IDs");
    }
    if !request.intent.may_supersede_progress() && !request.supersedes.is_empty() {
        anyhow::bail!("acknowledgement cannot supersede progress messages");
    }
    if request.intent == MailboxIntent::Acknowledgement && request.reply_to.is_none() {
        anyhow::bail!("mailbox acknowledgement requires reply_to");
    }
    Ok(())
}

async fn find_by_idempotency(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    request: &MailboxSendRequest,
) -> anyhow::Result<Option<MailboxMessage>> {
    let row = sqlx::query(
        "SELECT * FROM agent_mailbox
         WHERE sender_instance_id = ? AND sender_task_generation = ?
           AND recipient_instance_id = ? AND recipient_task_generation = ?
           AND idempotency_key = ?",
    )
    .bind(&request.sender_instance_id)
    .bind(&request.sender_task_generation)
    .bind(&request.recipient_instance_id)
    .bind(&request.recipient_task_generation)
    .bind(&request.idempotency_key)
    .fetch_optional(transaction.as_mut())
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut message = message_from_row(&row)?;
    message.superseded_by = superseded_by_in_transaction(transaction, &message.message_id).await?;
    message.supersedes = supersedes_in_transaction(transaction, &message.message_id).await?;
    Ok(Some(message))
}

async fn superseded_by_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    message_id: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT covering_message_id FROM agent_mailbox_supersessions
         WHERE covered_message_id = ? ORDER BY created_at_ms, covering_message_id",
    )
    .bind(message_id)
    .fetch_all(transaction.as_mut())
    .await?)
}

async fn supersedes_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    message_id: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT covered_message_id FROM agent_mailbox_supersessions
         WHERE covering_message_id = ? ORDER BY created_at_ms, covered_message_id",
    )
    .bind(message_id)
    .fetch_all(transaction.as_mut())
    .await?)
}

fn same_operation(message: &MailboxMessage, request: &MailboxSendRequest) -> bool {
    message.sender_instance_id == request.sender_instance_id
        && message.sender_task_generation == request.sender_task_generation
        && message.recipient_instance_id == request.recipient_instance_id
        && message.recipient_task_generation == request.recipient_task_generation
        && message.intent == request.intent
        && message.encrypted_payload == request.encrypted_payload
        && message.idempotency_key == request.idempotency_key
        && message.reply_to == request.reply_to
        && message.supersedes.len() == request.supersedes.len()
        && request
            .supersedes
            .iter()
            .all(|message_id| message.supersedes.contains(message_id))
}

async fn validate_reply(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    request: &MailboxSendRequest,
) -> anyhow::Result<Option<String>> {
    let Some(reply_to) = request.reply_to.as_deref() else {
        return Ok(None);
    };
    let row = sqlx::query("SELECT * FROM agent_mailbox WHERE message_id = ?")
        .bind(reply_to)
        .fetch_optional(transaction.as_mut())
        .await?
        .ok_or_else(|| anyhow::anyhow!("mailbox reply_to is not a stored visible message"))?;
    let original = message_from_row(&row)?;
    if original.sender_instance_id != request.recipient_instance_id
        || original.sender_task_generation != request.recipient_task_generation
        || original.recipient_instance_id != request.sender_instance_id
        || original.recipient_task_generation != request.sender_task_generation
    {
        anyhow::bail!("mailbox reply_to endpoints do not match the current participants");
    }
    if request.intent == MailboxIntent::Acknowledgement && original.acknowledged_at_ms.is_some() {
        anyhow::bail!("mailbox message is already acknowledged");
    }
    Ok(Some(reply_to.to_string()))
}

async fn validate_supersession(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    request: &MailboxSendRequest,
) -> anyhow::Result<Vec<String>> {
    let mut covered = Vec::with_capacity(request.supersedes.len());
    for message_id in &request.supersedes {
        let row = sqlx::query("SELECT * FROM agent_mailbox WHERE message_id = ?")
            .bind(message_id)
            .fetch_optional(transaction.as_mut())
            .await?
            .ok_or_else(|| anyhow::anyhow!("mailbox supersedes target does not exist"))?;
        let prior = message_from_row(&row)?;
        if prior.intent != MailboxIntent::Progress {
            anyhow::bail!("only explicit progress messages may be superseded");
        }
        if prior.sender_instance_id != request.sender_instance_id
            || prior.sender_task_generation != request.sender_task_generation
            || prior.recipient_instance_id != request.recipient_instance_id
            || prior.recipient_task_generation != request.recipient_task_generation
        {
            anyhow::bail!("mailbox supersedes target is outside the exact message scope");
        }
        let already_covered: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_mailbox_supersessions WHERE covered_message_id = ?)",
        )
        .bind(message_id)
        .fetch_one(transaction.as_mut())
        .await?;
        if already_covered {
            anyhow::bail!("mailbox progress target already has an explicit covering message");
        }
        covered.push(message_id.clone());
    }
    Ok(covered)
}

async fn superseded_by(pool: &SqlitePool, message_id: &str) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT covering_message_id FROM agent_mailbox_supersessions
         WHERE covered_message_id = ? ORDER BY created_at_ms, covering_message_id",
    )
    .bind(message_id)
    .fetch_all(pool)
    .await?)
}

async fn supersedes(pool: &SqlitePool, message_id: &str) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT covered_message_id FROM agent_mailbox_supersessions
         WHERE covering_message_id = ? ORDER BY created_at_ms, covered_message_id",
    )
    .bind(message_id)
    .fetch_all(pool)
    .await?)
}

fn message_from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<MailboxMessage> {
    let intent = match row.try_get::<String, _>("intent")?.as_str() {
        "progress" => MailboxIntent::Progress,
        "action_required" => MailboxIntent::ActionRequired,
        "result_ready" => MailboxIntent::ResultReady,
        "acknowledgement" => MailboxIntent::Acknowledgement,
        unknown => anyhow::bail!("unknown stored mailbox intent {unknown}"),
    };
    Ok(MailboxMessage {
        message_id: row.try_get("message_id")?,
        sender_instance_id: row.try_get("sender_instance_id")?,
        sender_task_generation: row.try_get("sender_task_generation")?,
        recipient_instance_id: row.try_get("recipient_instance_id")?,
        recipient_task_generation: row.try_get("recipient_task_generation")?,
        intent,
        encrypted_payload: row.try_get("encrypted_payload")?,
        idempotency_key: row.try_get("idempotency_key")?,
        reply_to: row.try_get("reply_to")?,
        enqueue_sequence: row.try_get("enqueue_sequence")?,
        created_at_ms: row.try_get("created_at_ms")?,
        wait_signalled_at_ms: row.try_get("wait_signalled_at_ms")?,
        wait_returned_at_ms: row.try_get("wait_returned_at_ms")?,
        context_committed_at_ms: row.try_get("context_committed_at_ms")?,
        delivered_at_ms: row.try_get("delivered_at_ms")?,
        acknowledged_at_ms: row.try_get("acknowledged_at_ms")?,
        acknowledgement_message_id: row.try_get("acknowledgement_message_id")?,
        superseded_by: Vec::new(),
        supersedes: Vec::new(),
    })
}

#[cfg(test)]
#[path = "agent_mailbox_tests.rs"]
mod tests;
