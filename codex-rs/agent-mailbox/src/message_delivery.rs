use crate::error::{MailboxError, MailboxResult};
use crate::messages::decrement_pending_count;
use crate::store::Mailbox;
use crate::types::{CanonicalConfirmation, Envelope, PendingPage};
use codex_protocol::ThreadId;
use std::future::Future;
use sqlx::Row;

const MAX_PAGE_SIZE: usize = 100;

impl Mailbox {
    /// Return a bounded page of uncanonical messages without consuming them.
    pub async fn pending_page(
        &self,
        recipient: ThreadId,
        after_seq: Option<i64>,
        limit: usize,
    ) -> MailboxResult<PendingPage> {
        validate_page(limit)?;
        let rows = sqlx::query(
            "SELECT * FROM mailbox_messages
             WHERE recipient = ? AND (canonical = 0 OR input_admitted = 0)
               AND superseded_by IS NULL AND withdrawn = 0
               AND (? IS NULL OR seq > ?)
             ORDER BY seq LIMIT ?",
        )
        .bind(recipient.to_string())
        .bind(after_seq)
        .bind(after_seq)
        .bind((limit + 1) as i64)
        .fetch_all(&self.pool)
        .await?;
        page_from_rows(rows, limit)
    }

    /// Convenience first page; use `pending_page` to continue pagination.
    pub async fn pending(&self, recipient: ThreadId, limit: usize) -> MailboxResult<Vec<Envelope>> {
        Ok(self.pending_page(recipient, None, limit).await?.items)
    }

    /// Read a bounded page including canonical records, without consuming them.
    pub async fn read_page(
        &self,
        recipient: ThreadId,
        after_seq: Option<i64>,
        limit: usize,
    ) -> MailboxResult<PendingPage> {
        validate_page(limit)?;
        let rows = sqlx::query(
            "SELECT * FROM mailbox_messages
             WHERE recipient = ? AND superseded_by IS NULL
               AND (? IS NULL OR seq > ?)
             ORDER BY seq LIMIT ?",
        )
        .bind(recipient.to_string())
        .bind(after_seq)
        .bind(after_seq)
        .bind((limit + 1) as i64)
        .fetch_all(&self.pool)
        .await?;
        page_from_rows(rows, limit)
    }

    pub async fn get_message(
        &self,
        recipient: ThreadId,
        id: &str,
    ) -> MailboxResult<Option<Envelope>> {
        let row = sqlx::query(
            "SELECT * FROM mailbox_messages WHERE id = ? AND recipient = ? LIMIT 1",
        )
        .bind(id)
        .bind(recipient.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(envelope_from_row).transpose()
    }

    /// Withdraw an unresolved request authored by this exact native thread.
    /// Withdrawal is durable and idempotent; replies cannot target it afterward.
    pub async fn withdraw_message(&self, sender: ThreadId, id: &str) -> MailboxResult<()> {
        let _input_arrival = self.begin_input_arrival();
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT sender, kind, canonical, input_admitted, legacy_attention,
                    CASE WHEN kind IN ('\"note\"', '\"progress\"')
                          AND (kind = '\"progress\"' OR legacy_attention = 0)
                         THEN 1 ELSE 0 END AS quiet,
                    superseded_by, withdrawn
             FROM mailbox_messages WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(MailboxError::ReferenceMismatch)?;
        if row.try_get::<String, _>("sender")? != sender.to_string() {
            return Err(MailboxError::WrongRecipient);
        }
        if row.try_get::<String, _>("kind")? != "\"request\"" {
            return Err(MailboxError::InvalidInput("only requests may be withdrawn".into()));
        }
        if row.try_get::<i64, _>("withdrawn")? != 0 {
            transaction.commit().await?;
            return Ok(());
        }
        let superseded_by: Option<String> = row.try_get("superseded_by")?;
        if superseded_by.is_some() {
            return Err(MailboxError::ReplacementNotAllowed);
        }
        let reply_exists: Option<String> = sqlx::query_scalar(
            "SELECT id FROM mailbox_messages WHERE reply_to = ? LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&mut *transaction)
        .await?;
        if reply_exists.is_some() {
            return Err(MailboxError::RequestAlreadyResolved);
        }
        let canonical = row.try_get::<i64, _>("canonical")? != 0;
        let input_admitted = row.try_get::<i64, _>("input_admitted")? != 0;
        let quiet = row.try_get::<i64, _>("quiet")? != 0;
        let updated = sqlx::query(
            "UPDATE mailbox_messages SET withdrawn = 1
             WHERE id = ? AND sender = ? AND withdrawn = 0",
        )
        .bind(id)
        .bind(sender.to_string())
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() == 1 && (!canonical || !input_admitted) {
            let recipient: String = sqlx::query_scalar(
                "SELECT recipient FROM mailbox_messages WHERE id = ?",
            )
            .bind(id)
            .fetch_one(&mut *transaction)
            .await?;
            decrement_pending_count(&mut transaction, &recipient, quiet).await?;
        }
        transaction.commit().await?;
        self.notify_changed();
        Ok(())
    }

    /// Count quiet, uncanonical message delivery without reading message bodies.
    pub async fn quiet_count(&self, recipient: ThreadId) -> MailboxResult<usize> {
        let count: i64 = sqlx::query_scalar(
            "SELECT quiet_count FROM mailbox_pending_counts WHERE recipient = ?",
        )
        .bind(recipient.to_string())
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0);
        Ok(count.max(0) as usize)
    }

    /// Return stable IDs for canonical current bodies not yet admitted to a
    /// provider request. Callers join these IDs to their existing raw history.
    pub async fn unadmitted_canonical_ids(
        &self,
        recipient: ThreadId,
        limit: usize,
    ) -> MailboxResult<Vec<String>> {
        if limit == 0 || limit > 4_096 {
            return Err(MailboxError::InvalidInput(
                "unadmitted body limit must be between 1 and 4096".into(),
            ));
        }
        let rows = sqlx::query(
            "SELECT id FROM mailbox_messages
             WHERE recipient = ? AND canonical = 1 AND input_admitted = 0
               AND superseded_by IS NULL AND withdrawn = 0
             ORDER BY seq LIMIT ?",
        )
        .bind(recipient.to_string())
        .bind((limit + 1) as i64)
        .fetch_all(&self.pool)
        .await?;
        if rows.len() > limit {
            return Err(MailboxError::Backpressure("unadmitted canonical body limit"));
        }
        rows.iter()
            .map(|row| row.try_get("id").map_err(Into::into))
            .collect()
    }

    /// Filter provider-supplied AgentMessage IDs through exact mailbox ownership.
    /// Unrelated native history IDs are ignored; only unadmitted IDs are returned.
    pub async fn unadmitted_input_ids(
        &self,
        recipient: ThreadId,
        ids: &[String],
    ) -> MailboxResult<Vec<String>> {
        if ids.len() > 100 {
            return Err(MailboxError::Backpressure("provider input ID chunk limit"));
        }
        let mut selected = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            if !seen.insert(id.as_str()) {
                continue;
            }
            let message: Option<String> = sqlx::query_scalar(
                "SELECT id FROM mailbox_messages
                 WHERE id = ? AND recipient = ? AND input_admitted = 0 LIMIT 1",
            )
            .bind(id)
            .bind(recipient.to_string())
            .fetch_optional(&self.pool)
            .await?;
            let native_work: Option<String> = sqlx::query_scalar(
                "SELECT work.input_message_id FROM mailbox_work AS work
                 WHERE work.input_message_id = ? AND work.agent = ?
                   AND NOT EXISTS (
                     SELECT 1 FROM mailbox_input_admissions AS admitted
                     WHERE admitted.recipient = work.agent
                       AND admitted.input_message_id = work.input_message_id
                   ) LIMIT 1",
            )
            .bind(id)
            .bind(recipient.to_string())
            .fetch_optional(&self.pool)
            .await?;
            if message.is_some() || native_work.is_some() {
                selected.push(id.clone());
            }
        }
        Ok(selected)
    }

    /// Load only causal envelopes attached to work admitted in this exact
    /// native turn. Unscoped quiet notes are intentionally excluded.
    pub async fn automatic_inputs(
        &self,
        recipient: ThreadId,
        source_turn_id: &str,
        limit: usize,
    ) -> MailboxResult<Vec<Envelope>> {
        if limit == 0 || limit > 2 {
            return Err(MailboxError::InvalidInput(
                "automatic input limit must be between 1 and 2".into(),
            ));
        }
        let rows = sqlx::query(
            "SELECT message.* FROM mailbox_messages AS message
             JOIN mailbox_work AS work ON work.id = message.work_id
             JOIN mailbox_admissions AS admission ON admission.work_id = work.id
             WHERE admission.recipient = ? AND admission.source_turn_id = ?
               AND work.agent = ? AND message.recipient = ?
               AND message.input_admitted = 0 AND message.superseded_by IS NULL
               AND message.withdrawn = 0
             ORDER BY message.seq LIMIT ?",
        )
        .bind(recipient.to_string())
        .bind(source_turn_id)
        .bind(recipient.to_string())
        .bind(recipient.to_string())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        let envelopes = rows
            .iter()
            .map(envelope_from_row)
            .collect::<MailboxResult<Vec<_>>>()?;
        let bytes = envelopes.iter().fold(0_usize, |total, envelope| {
            total
                .saturating_add(envelope.communication.content.len())
                .saturating_add(
                    envelope
                        .communication
                        .encrypted_content
                        .as_ref()
                        .map_or(0, String::len),
                )
        });
        if bytes > 16_384 {
            return Err(MailboxError::Backpressure("automatic input byte limit"));
        }
        Ok(envelopes)
    }

    /// Exact bounded list of accepted native-work input IDs not yet admitted.
    pub async fn unadmitted_native_input_ids(
        &self,
        recipient: ThreadId,
        limit: usize,
    ) -> MailboxResult<Vec<String>> {
        if limit == 0 || limit > 4_096 {
            return Err(MailboxError::InvalidInput(
                "unadmitted native input limit must be between 1 and 4096".into(),
            ));
        }
        let rows = sqlx::query(
            "SELECT work.input_message_id FROM mailbox_work AS work
             WHERE work.agent = ? AND NOT EXISTS (
               SELECT 1 FROM mailbox_input_admissions AS admitted
               WHERE admitted.recipient = work.agent
                 AND admitted.input_message_id = work.input_message_id
             )
             ORDER BY work.seq LIMIT ?",
        )
        .bind(recipient.to_string())
        .bind((limit + 1) as i64)
        .fetch_all(&self.pool)
        .await?;
        if rows.len() > limit {
            return Err(MailboxError::Backpressure("unadmitted native input limit"));
        }
        rows.iter()
            .map(|row| row.try_get("input_message_id").map_err(Into::into))
            .collect()
    }

    /// Mark canonical admission only after the adapter has appended and flushed
    /// the stable ID. This operation is idempotent and recipient-bound.
    pub async fn mark_canonical(&self, recipient: ThreadId, id: &str) -> MailboxResult<()> {
        let mut transaction = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT canonical, input_admitted, legacy_attention, superseded_by, withdrawn,
                    CASE WHEN kind IN ('\"note\"', '\"progress\"')
                          AND (kind = '\"progress\"' OR legacy_attention = 0)
                         THEN 1 ELSE 0 END AS quiet
             FROM mailbox_messages
             WHERE id = ? AND recipient = ?",
        )
        .bind(id)
        .bind(recipient.to_string())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(MailboxError::WrongRecipient)?;
        let was_canonical = current.try_get::<i64, _>("canonical")? != 0;
        let input_admitted = current.try_get::<i64, _>("input_admitted")? != 0;
        let quiet = current.try_get::<i64, _>("quiet")? != 0;
        let is_current = current.try_get::<Option<String>, _>("superseded_by")?.is_none()
            && current.try_get::<i64, _>("withdrawn")? == 0;
        let result = sqlx::query(
            "UPDATE mailbox_messages SET canonical = 1
             WHERE id = ? AND recipient = ? AND canonical = 0",
        )
        .bind(id)
        .bind(recipient.to_string())
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() > 0 && !was_canonical && input_admitted && is_current {
            decrement_pending_count(&mut transaction, &recipient.to_string(), quiet).await?;
        }
        sqlx::query("UPDATE mailbox_attention SET canonical = 1 WHERE id = ?")
            .bind(format!("attention:{id}"))
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "UPDATE mailbox_work SET outcome_canonical = 1
             WHERE outcome_id = ? AND issuer = ?",
        )
        .bind(id)
        .bind(recipient.to_string())
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE mailbox_attention SET canonical = 1 WHERE recipient = ? AND id IN (
               SELECT 'attention:' || id FROM mailbox_work WHERE outcome_id = ? AND issuer = ?
             )",
        )
        .bind(recipient.to_string())
        .bind(id)
        .bind(recipient.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        self.notify_changed();
        Ok(())
    }

    /// Mark a no-response or typed outcome after the adapter has appended its
    /// exact `OutcomeReceipt.id` and flushed canonical history.
    pub async fn mark_outcome_canonical(
        &self,
        recipient: ThreadId,
        id: &str,
    ) -> MailboxResult<()> {
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE mailbox_work SET outcome_canonical = 1
             WHERE outcome_id = ? AND issuer = ? AND outcome_json IS NOT NULL",
        )
        .bind(id)
        .bind(recipient.to_string())
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() == 0 {
            return Err(MailboxError::WrongRecipient);
        }
        sqlx::query(
            "UPDATE mailbox_attention SET canonical = 1
             WHERE id IN (
               SELECT 'attention:' || id FROM mailbox_work
               WHERE outcome_id = ? AND issuer = ?
             )",
        )
        .bind(id)
        .bind(recipient.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        self.notify_changed();
        Ok(())
    }

    /// Reconcile one bounded page using a stable-ID-aware adapter callback.
    /// The callback must return only after append+flush or exact-ID readback.
    pub async fn reconcile_pending<F, Fut>(
        &self,
        recipient: ThreadId,
        limit: usize,
        mut append_and_flush: F,
    ) -> MailboxResult<usize>
    where
        F: FnMut(Envelope) -> Fut,
        Fut: Future<Output = MailboxResult<CanonicalConfirmation>>,
    {
        let page = self.pending_page(recipient, None, limit).await?;
        let mut reconciled = 0;
        for envelope in page.items {
            let id = envelope.id.clone();
            let _confirmation = append_and_flush(envelope).await?;
            self.mark_canonical(recipient, &id).await?;
            reconciled += 1;
        }
        Ok(reconciled)
    }
}

pub(crate) fn page_from_rows(
    rows: Vec<sqlx::sqlite::SqliteRow>,
    limit: usize,
) -> MailboxResult<PendingPage> {
    let has_more = rows.len() > limit;
    let visible = rows.into_iter().take(limit).collect::<Vec<_>>();
    let next_cursor = if has_more {
        visible
            .last()
            .map(|row| row.try_get::<i64, _>("seq"))
            .transpose()?
    } else {
        None
    };
    let items = visible
        .iter()
        .map(envelope_from_row)
        .collect::<MailboxResult<Vec<_>>>()?;
    Ok(PendingPage { items, next_cursor })
}

pub(crate) fn envelope_from_row(row: &sqlx::sqlite::SqliteRow) -> MailboxResult<Envelope> {
    let sender = ThreadId::from_string(row.try_get::<String, _>("sender")?.as_str())
        .map_err(|_| MailboxError::InvalidInput("stored sender thread ID".into()))?;
    let recipient = ThreadId::from_string(row.try_get::<String, _>("recipient")?.as_str())
        .map_err(|_| MailboxError::InvalidInput("stored recipient thread ID".into()))?;
    let work_ref: Option<String> = row.try_get("work_ref_json")?;
    Ok(Envelope {
        id: row.try_get("id")?,
        invocation_key: row.try_get("invocation_key")?,
        sender,
        recipient,
        communication: serde_json::from_str(&row.try_get::<String, _>("communication_json")?)?,
        kind: serde_json::from_str(&row.try_get::<String, _>("kind")?)?,
        activate_if_idle: row.try_get::<i64, _>("activate_if_idle")? != 0,
        work_ref: work_ref.map(|value| serde_json::from_str(&value)).transpose()?,
        reply_to: row.try_get("reply_to")?,
        supersedes: row.try_get("supersedes")?,
        legacy_attention: row.try_get::<i64, _>("legacy_attention")? != 0,
        canonical: row.try_get::<i64, _>("canonical")? != 0,
    })
}

fn validate_page(limit: usize) -> MailboxResult<()> {
    if limit == 0 || limit > MAX_PAGE_SIZE {
        return Err(MailboxError::InvalidInput(format!(
            "page size must be between 1 and {MAX_PAGE_SIZE}"
        )));
    }
    Ok(())
}
