use crate::error::{MailboxError, MailboxResult};
use crate::types::{MessageDraft, MessageKind};
use sqlx::{Row, Sqlite, Transaction};

pub(crate) async fn validate_reply_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    request_id: &str,
    draft: &MessageDraft,
) -> MailboxResult<()> {
    let request = sqlx::query(
        "SELECT sender, recipient, kind, canonical FROM mailbox_messages WHERE id = ?",
    )
    .bind(request_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(MailboxError::ReferenceMismatch)?;
    if request.try_get::<String, _>("kind")? != "\"request\"" {
        return Err(MailboxError::InvalidInput("reply_to must name a request".into()));
    }
    if request.try_get::<String, _>("recipient")? != draft.sender.to_string()
        || request.try_get::<String, _>("sender")? != draft.recipient.to_string()
    {
        return Err(MailboxError::WrongRecipient);
    }
    if request.try_get::<i64, _>("canonical")? == 0 {
        return Err(MailboxError::NotCanonical);
    }
    let resolved: Option<String> = sqlx::query_scalar(
        "SELECT id FROM mailbox_messages WHERE reply_to = ? LIMIT 1",
    )
    .bind(request_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if resolved.is_some() {
        return Err(MailboxError::RequestAlreadyResolved);
    }
    Ok(())
}

pub(crate) async fn validate_replacement_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    old_id: &str,
    draft: &MessageDraft,
) -> MailboxResult<()> {
    let old = sqlx::query(
        "SELECT sender, recipient, work_ref_json, kind, canonical, superseded_by, withdrawn
         FROM mailbox_messages WHERE id = ?",
    )
    .bind(old_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(MailboxError::ReferenceMismatch)?;
    let old_work: Option<String> = old.try_get("work_ref_json")?;
    let new_work = draft.work_ref.as_ref().map(serde_json::to_string).transpose()?;
    if old.try_get::<String, _>("sender")? != draft.sender.to_string()
        || old.try_get::<String, _>("recipient")? != draft.recipient.to_string()
        || old_work != new_work
    {
        return Err(MailboxError::WrongRecipient);
    }
    let already_replaced: Option<String> = old.try_get("superseded_by")?;
    let old_kind: String = old.try_get("kind")?;
    let valid_pair = match old_kind.as_str() {
        "\"progress\"" => draft.kind == MessageKind::Progress,
        "\"request\"" => matches!(draft.kind, MessageKind::Request | MessageKind::Note),
        _ => false,
    };
    if !valid_pair
        || old.try_get::<i64, _>("canonical")? != 0
        || already_replaced.is_some()
        || old.try_get::<i64, _>("withdrawn")? != 0
    {
        return Err(MailboxError::ReplacementNotAllowed);
    }
    if old_kind == "\"request\"" {
        let resolved: Option<String> =
            sqlx::query_scalar("SELECT id FROM mailbox_messages WHERE reply_to = ? LIMIT 1")
                .bind(old_id)
                .fetch_optional(&mut **transaction)
                .await?;
        if resolved.is_some() {
            return Err(MailboxError::RequestAlreadyResolved);
        }
    }
    Ok(())
}
