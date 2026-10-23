use crate::error::{MailboxError, MailboxResult};
use crate::message_delivery::envelope_from_row;
use crate::message_rules::{validate_reply_tx, validate_replacement_tx};
use crate::store::Mailbox;
use crate::types::{Envelope, MessageDraft, MessageKind, WorkRef};
use crate::work::exact_work;
use codex_protocol::{ResponseItemId, ThreadId};
use serde::Serialize;
use sqlx::{Row, Sqlite, Transaction};

const MAX_MESSAGE_BYTES: usize = 8_192;
const MAX_RECORD_BYTES: usize = 16_384;
const MAX_KEY_BYTES: usize = 256;
const MAX_TOTAL_MESSAGES: i64 = 100_000;

#[derive(Serialize)]
struct DraftRequest<'a> {
    invocation_key: &'a str,
    sender: ThreadId,
    recipient: ThreadId,
    communication: &'a codex_protocol::protocol::InterAgentCommunication,
    kind: MessageKind,
    activate_if_idle: bool,
    work_ref: &'a Option<WorkRef>,
    reply_to: &'a Option<String>,
    supersedes: &'a Option<String>,
    legacy_attention: bool,
}

impl Mailbox {
    /// Durably accept a message intent. Retries with the same invocation key
    /// return its stable ID; reusing a key for a different request is rejected.
    pub async fn accept_message(&self, draft: MessageDraft) -> MailboxResult<Envelope> {
        let _input_arrival = self.begin_input_arrival();
        let id = match draft.communication.id.as_ref() {
            Some(id) if id.as_str().starts_with("amsg_") => id.to_string(),
            Some(_) => {
                return Err(MailboxError::InvalidInput(
                    "expected stable amsg_* ID".into(),
                ));
            }
            None => new_mailbox_id(),
        };
        let inserted_id = id.clone();
        let envelope = {
            let mut transaction = self.pool.begin().await?;
            let envelope = insert_message_tx(&mut transaction, draft, Some(inserted_id)).await?;
            transaction.commit().await?;
            envelope
        };
        self.invalidate_waits(envelope.recipient);
        self.notify_changed();
        Ok(envelope)
    }

}

pub(crate) async fn insert_message_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    mut draft: MessageDraft,
    explicit_id: Option<String>,
) -> MailboxResult<Envelope> {
    validate_key(&draft.invocation_key)?;
    draft.legacy_attention &= draft.kind == MessageKind::Note;
    if draft.kind == MessageKind::Progress && draft.reply_to.is_some() {
        return Err(MailboxError::InvalidInput(
            "progress messages cannot be replies".into(),
        ));
    }
    let id = explicit_id
        .or_else(|| draft.communication.id.as_ref().map(ToString::to_string))
        .unwrap_or_else(new_mailbox_id);
    validate_id(&id)?;
    draft.communication.id = Some(ResponseItemId::from_server(id.clone()));

    let mut fingerprint = draft.clone();
    fingerprint.communication.id = None;
    let request_json = serde_json::to_string(&DraftRequest {
        invocation_key: &fingerprint.invocation_key,
        sender: fingerprint.sender,
        recipient: fingerprint.recipient,
        communication: &fingerprint.communication,
        kind: fingerprint.kind,
        activate_if_idle: fingerprint.activate_if_idle,
        work_ref: &fingerprint.work_ref,
        reply_to: &fingerprint.reply_to,
        supersedes: &fingerprint.supersedes,
        legacy_attention: fingerprint.legacy_attention,
    })?;
    let communication_json = serde_json::to_string(&draft.communication)?;
    let body_bytes = draft.communication.content.len()
        + draft.communication.encrypted_content.as_ref().map_or(0, String::len);
    if body_bytes > MAX_MESSAGE_BYTES
        || request_json.len() > MAX_RECORD_BYTES
        || communication_json.len() > MAX_RECORD_BYTES
    {
        return Err(MailboxError::Backpressure("message byte limit"));
    }
    if let Some(row) = sqlx::query(
        "SELECT message_id, request_json FROM mailbox_invocations WHERE invocation_key = ?",
    )
    .bind(&draft.invocation_key)
    .fetch_optional(&mut **transaction)
    .await?
    {
        if row.try_get::<String, _>("request_json")? != request_json {
            return Err(MailboxError::IdempotencyConflict);
        }
        let message_id: String = row.try_get("message_id")?;
        let message = sqlx::query("SELECT * FROM mailbox_messages WHERE id = ?")
            .bind(message_id)
            .fetch_one(&mut **transaction)
            .await?;
        return envelope_from_row(&message);
    }

    if let Some(work_ref) = draft.work_ref.as_ref() {
        let work = exact_work(&mut **transaction, work_ref).await?;
        let sender = draft.sender.to_string();
        let recipient = draft.recipient.to_string();
        let issuer = work.try_get::<String, _>("issuer")?;
        let agent = work.try_get::<String, _>("agent")?;
        if !((sender == issuer && recipient == agent) || (sender == agent && recipient == issuer)) {
            return Err(MailboxError::WrongRecipient);
        }
        if draft.kind == MessageKind::Progress
            && work.try_get::<Option<String>, _>("outcome_json")?.is_some()
        {
            return Err(MailboxError::OutcomeAlreadyRecorded);
        }
    }
    if let Some(reply_to) = draft.reply_to.as_deref() {
        validate_reply_tx(transaction, reply_to, &draft).await?;
    }
    if let Some(supersedes) = draft.supersedes.as_deref() {
        validate_replacement_tx(transaction, supersedes, &draft).await?;
    }

    if draft.supersedes.is_none() && draft.kind == MessageKind::Progress && !draft.activate_if_idle {
        if let Some(work_ref) = draft.work_ref.as_ref() {
            if let Some(existing) = sqlx::query(
                "SELECT * FROM mailbox_messages
                 WHERE sender = ? AND recipient = ? AND work_ref_json = ?
                   AND author_path = ? AND recipient_path = ?
                   AND kind = '\"progress\"' AND canonical = 0 AND input_admitted = 0
                   AND superseded_by IS NULL
                   AND legacy_attention = ?
                   AND NOT EXISTS (
                     SELECT 1 FROM mailbox_messages AS newer
                     WHERE newer.supersedes = mailbox_messages.id
                   )
                 ORDER BY seq DESC LIMIT 1",
            )
            .bind(draft.sender.to_string())
            .bind(draft.recipient.to_string())
            .bind(serde_json::to_string(work_ref)?)
            .bind(serde_json::to_string(&draft.communication.author)?)
            .bind(serde_json::to_string(&draft.communication.recipient)?)
            .bind(i64::from(draft.legacy_attention))
            .fetch_optional(&mut **transaction)
            .await?
            {
                // Replacement gets its own stable ID. Never rewrite the body or
                // idempotency binding behind the previously accepted identifier.
                draft.supersedes = Some(existing.try_get("id")?);
            }
        }
    }

    let collision: Option<String> = sqlx::query_scalar(
        "SELECT id FROM mailbox_messages WHERE id = ? LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(&mut **transaction)
    .await?;
    if collision.is_some() {
        return Err(MailboxError::IdempotencyConflict);
    }
    let pending_count = pending_count_tx(transaction, &draft.recipient.to_string()).await?;
    if pending_count >= crate::messages::MAX_PENDING_PER_RECIPIENT {
        return Err(MailboxError::Backpressure("recipient pending-message count"));
    }
    let total_count: i64 = sqlx::query_scalar(
        "SELECT item_count FROM mailbox_counts WHERE name = 'message'",
    )
        .fetch_one(&mut **transaction)
        .await?;
    if total_count >= MAX_TOTAL_MESSAGES {
        return Err(MailboxError::Backpressure("retained message count"));
    }

    let kind_json = serde_json::to_string(&draft.kind)?;
    sqlx::query(
        "INSERT INTO mailbox_messages (
            id, invocation_key, request_json, sender, recipient, author_path, recipient_path,
            communication_json,
            kind, work_ref_json, work_id, reply_to, supersedes, activate_if_idle, legacy_attention
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&draft.invocation_key)
    .bind(&request_json)
    .bind(draft.sender.to_string())
    .bind(draft.recipient.to_string())
    .bind(serde_json::to_string(&draft.communication.author)?)
    .bind(serde_json::to_string(&draft.communication.recipient)?)
    .bind(&communication_json)
    .bind(kind_json)
    .bind(draft.work_ref.as_ref().map(serde_json::to_string).transpose()?)
    .bind(draft.work_ref.as_ref().map(|work_ref| work_ref.id.as_str()))
    .bind(&draft.reply_to)
    .bind(&draft.supersedes)
    .bind(i64::from(draft.activate_if_idle))
    .bind(i64::from(draft.legacy_attention))
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO mailbox_activations(message_id, sender, state, disposition)
         VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(draft.sender.to_string())
    .bind(if draft.activate_if_idle { "pending" } else { "decided" })
    .bind(if draft.activate_if_idle { None } else { Some("not_requested") })
    .execute(&mut **transaction)
    .await?;
    add_invocation_tx(transaction, &draft.invocation_key, &request_json, &id).await?;
    if let Some(old_id) = draft.supersedes.as_deref() {
        let old = sqlx::query(
            "SELECT recipient, canonical, input_admitted, legacy_attention,
                    CASE WHEN kind IN ('\"note\"', '\"progress\"')
                          AND (kind = '\"progress\"' OR legacy_attention = 0)
                         THEN 1 ELSE 0 END AS quiet
             FROM mailbox_messages
             WHERE id = ? AND superseded_by IS NULL AND withdrawn = 0",
        )
        .bind(old_id)
        .fetch_optional(&mut **transaction)
        .await?;
        let replaced = sqlx::query(
            "UPDATE mailbox_messages SET superseded_by = ?
             WHERE id = ? AND superseded_by IS NULL AND withdrawn = 0",
        )
            .bind(&id)
            .bind(old_id)
            .execute(&mut **transaction)
            .await?;
        if replaced.rows_affected() == 1 {
            if let Some(old) = old {
                let unread = old.try_get::<i64, _>("canonical")? == 0
                    || old.try_get::<i64, _>("input_admitted")? == 0;
                if unread {
                    let recipient: String = old.try_get("recipient")?;
                    let quiet = old.try_get::<i64, _>("quiet")? != 0;
                    decrement_pending_count(transaction, &recipient, quiet).await?;
                }
            }
        }
    }
    sqlx::query("UPDATE mailbox_counts SET item_count = item_count + 1 WHERE name = 'message'")
        .execute(&mut **transaction)
        .await?;
    increment_pending_count(
        transaction,
        &draft.recipient.to_string(),
        is_quiet_kind(draft.kind, draft.legacy_attention),
    )
    .await?;
    if has_attention(&draft) {
        insert_attention_tx(transaction, &id, draft.recipient.to_string().as_str()).await?;
    }
    let row = sqlx::query("SELECT * FROM mailbox_messages WHERE id = ?")
        .bind(&id)
        .fetch_one(&mut **transaction)
        .await?;
    envelope_from_row(&row)
}

async fn add_invocation_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    invocation_key: &str,
    request_json: &str,
    message_id: &str,
) -> MailboxResult<()> {
    sqlx::query(
        "INSERT INTO mailbox_invocations(invocation_key, request_json, message_id)
         VALUES (?, ?, ?)",
    )
    .bind(invocation_key)
    .bind(request_json)
    .bind(message_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_attention_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    message_id: &str,
    recipient: &str,
) -> MailboxResult<()> {
    sqlx::query(
        "INSERT INTO mailbox_attention(id, recipient) VALUES (?, ?)
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(format!("attention:{message_id}"))
    .bind(recipient)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn pending_count_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    recipient: &str,
) -> MailboxResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT item_count FROM mailbox_pending_counts WHERE recipient = ?",
    )
    .bind(recipient)
    .fetch_optional(&mut **transaction)
    .await?
    .unwrap_or(0))
}

async fn increment_pending_count(
    transaction: &mut Transaction<'_, Sqlite>,
    recipient: &str,
    quiet: bool,
) -> MailboxResult<()> {
    sqlx::query(
        "INSERT INTO mailbox_pending_counts(recipient, item_count, quiet_count)
         VALUES (?, 1, ?)
         ON CONFLICT(recipient) DO UPDATE SET
           item_count = item_count + 1,
           quiet_count = quiet_count + excluded.quiet_count",
    )
    .bind(recipient)
    .bind(i64::from(quiet))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn decrement_pending_count(
    transaction: &mut Transaction<'_, Sqlite>,
    recipient: &str,
    quiet: bool,
) -> MailboxResult<()> {
    sqlx::query(
        "UPDATE mailbox_pending_counts SET
           item_count = MAX(item_count - 1, 0),
           quiet_count = MAX(quiet_count - ?, 0)
         WHERE recipient = ?",
    )
    .bind(i64::from(quiet))
    .bind(recipient)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn validate_key(key: &str) -> MailboxResult<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(MailboxError::InvalidInput("invalid invocation key".into()));
    }
    Ok(())
}

pub(crate) fn is_quiet_kind(kind: MessageKind, legacy_attention: bool) -> bool {
    matches!(kind, MessageKind::Note | MessageKind::Progress)
        && !(kind == MessageKind::Note && legacy_attention)
}

fn has_attention(draft: &MessageDraft) -> bool {
    draft.legacy_attention || matches!(draft.kind, MessageKind::Request | MessageKind::Alert)
}

fn validate_id(id: &str) -> MailboxResult<()> {
    if id.starts_with("amsg_") && id.len() > 5 && id.len() <= 256 {
        Ok(())
    } else {
        Err(MailboxError::InvalidInput("expected stable amsg_* ID".into()))
    }
}

pub(crate) const MAX_PENDING_PER_RECIPIENT: i64 = 4_096;

fn new_mailbox_id() -> String {
    ResponseItemId::with_suffix("amsg", uuid::Uuid::now_v7()).to_string()
}
