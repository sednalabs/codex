use crate::error::{MailboxError, MailboxResult};
use crate::store::Mailbox;
use crate::types::{ActivationDecision, ActivationDisposition};
use codex_protocol::ThreadId;
use sqlx::Row;

impl Mailbox {
    /// Claim an optional native activation exactly once for an accepted message.
    /// An unresolved claim is recovery-needed, never a retry authorization.
    pub async fn begin_activation(
        &self,
        sender: ThreadId,
        message_id: &str,
    ) -> MailboxResult<ActivationDecision> {
        let mut transaction = self.pool.begin().await?;
        let message = sqlx::query(
            "SELECT id FROM mailbox_messages WHERE id = ? AND sender = ? LIMIT 1",
        )
        .bind(message_id)
        .bind(sender.to_string())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(MailboxError::ReferenceMismatch)?;
        let exact_id: String = message.try_get("id")?;
        let claimed = sqlx::query(
            "UPDATE mailbox_activations SET state = 'activating'
             WHERE message_id = ? AND sender = ? AND state = 'pending'",
        )
        .bind(&exact_id)
        .bind(sender.to_string())
        .execute(&mut *transaction)
        .await?;
        if claimed.rows_affected() == 1 {
            transaction.commit().await?;
            self.notify_changed();
            return Ok(ActivationDecision::Activate);
        }
        let existing = sqlx::query(
            "SELECT sender, state, disposition FROM mailbox_activations WHERE message_id = ?",
        )
        .bind(message_id)
        .fetch_one(&mut *transaction)
        .await?;
        if existing.try_get::<String, _>("sender")? != sender.to_string() {
            return Err(MailboxError::WrongRecipient);
        }
        let decision = match existing.try_get::<String, _>("state")?.as_str() {
            "activating" => ActivationDecision::RecoveryNeeded,
            "decided" => ActivationDecision::AlreadyDecided(parse_activation_disposition(
                existing.try_get::<String, _>("disposition")?.as_str(),
            )?),
            _ => return Err(MailboxError::InvalidInput("stored activation state".into())),
        };
        transaction.commit().await?;
        Ok(decision)
    }

    /// Persist the native adapter's final activation disposition. A lost result
    /// can be retried exactly, but a conflicting disposition is rejected.
    pub async fn record_activation(
        &self,
        sender: ThreadId,
        message_id: &str,
        disposition: ActivationDisposition,
    ) -> MailboxResult<()> {
        let mut transaction = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT sender, state, disposition FROM mailbox_activations WHERE message_id = ?",
        )
        .bind(message_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(MailboxError::ReferenceMismatch)?;
        if current.try_get::<String, _>("sender")? != sender.to_string() {
            return Err(MailboxError::WrongRecipient);
        }
        let disposition_string = activation_disposition_string(disposition);
        let state: String = current.try_get("state")?;
        if state == "decided" {
            let old: String = current.try_get("disposition")?;
            if old != disposition_string {
                return Err(MailboxError::IdempotencyConflict);
            }
            transaction.commit().await?;
            return Ok(());
        }
        if state != "activating" {
            return Err(MailboxError::InvalidInput("stored activation state".into()));
        }
        sqlx::query(
            "UPDATE mailbox_activations SET state = 'decided', disposition = ?
             WHERE message_id = ? AND sender = ? AND state = 'activating'",
        )
        .bind(disposition_string)
        .bind(message_id)
        .bind(sender.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        self.notify_changed();
        Ok(())
    }
}

fn activation_disposition_string(disposition: ActivationDisposition) -> &'static str {
    match disposition {
        ActivationDisposition::NotRequested => "not_requested",
        ActivationDisposition::AlreadyActive => "already_active",
        ActivationDisposition::Activated => "activated",
        ActivationDisposition::Denied => "denied",
        ActivationDisposition::Unknown => "unknown",
    }
}

fn parse_activation_disposition(value: &str) -> MailboxResult<ActivationDisposition> {
    match value {
        "not_requested" => Ok(ActivationDisposition::NotRequested),
        "already_active" => Ok(ActivationDisposition::AlreadyActive),
        "activated" => Ok(ActivationDisposition::Activated),
        "denied" => Ok(ActivationDisposition::Denied),
        "unknown" => Ok(ActivationDisposition::Unknown),
        _ => Err(MailboxError::InvalidInput("stored activation disposition".into())),
    }
}
