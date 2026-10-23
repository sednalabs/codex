use crate::error::{MailboxError, MailboxResult};
use crate::message_delivery::envelope_from_row;
use crate::store::Mailbox;
use crate::types::{
    Attention, Dependency, DependencyMode, ObservationTarget, ReturnWhen, Selection,
    RequestDisposition, RequestSnapshot, Selector, Snapshot, SnapshotItem, SnapshotState,
    WorkSnapshot,
};
use crate::outcome_store::{continuation_receipts_for, stored_outcome_receipt_for};
use crate::work::{parse_dispatch_state, work_ref_from_row};
use codex_protocol::ThreadId;
use sqlx::Row;
use std::collections::{HashMap, HashSet};

const MAX_SELECTIONS: usize = 100;
const MAX_OBSERVATIONS: usize = 100;
const MAX_PAGE_SIZE: usize = 100;

impl Mailbox {
    /// Resolve caller-issued work and exact reply IDs without consuming them.
    /// Completed work remains visible until its exact outcome is acknowledged.
    pub async fn snapshot(&self, selection: &Selection) -> MailboxResult<Snapshot> {
        if selection.selectors.is_empty() || selection.selectors.len() > MAX_SELECTIONS {
            return Err(MailboxError::InvalidInput(
                "snapshot selector count must be between 1 and 100".into(),
            ));
        }
        if selection.limit == 0 || selection.limit > MAX_PAGE_SIZE {
            return Err(MailboxError::InvalidInput(
                "snapshot limit must be between 1 and 100".into(),
            ));
        }
        if selection.limit < selection.selectors.len() {
            return Err(MailboxError::InvalidInput(
                "snapshot limit must cover every selected item".into(),
            ));
        }
        let mut items = Vec::new();
        let mut matched = 0;
        let mut satisfied = 0;
        let mut unique = HashSet::new();
        for selector in &selection.selectors {
            let key = match selector {
                Selector::Work(id) => format!("work:{id}"),
                Selector::Reply(id) => format!("reply:{id}"),
            };
            if !unique.insert(key) {
                continue;
            }
            match selector {
                Selector::Work(id) => {
                    let row = sqlx::query(
                        "SELECT * FROM mailbox_work
                         WHERE id = ? AND issuer = ? AND (? IS NULL OR agent = ?) LIMIT 1",
                    )
                    .bind(id)
                    .bind(selection.caller.to_string())
                    .bind(selection.agent.map(|agent| agent.to_string()))
                    .bind(selection.agent.map(|agent| agent.to_string()))
                    .fetch_optional(&self.pool)
                    .await?;
                    let Some(row) = row else {
                        continue;
                    };
                    matched += 1;
                    let work_ref = work_ref_from_row(&row)?;
                    let observed = row.try_get::<i64, _>("outcome_observed")? != 0;
                    let input: String = row.try_get("input_json")?;
                    let outcome = if row.try_get::<Option<String>, _>("outcome_json")?.is_some() {
                        Some(stored_outcome_receipt_for(&self.pool, work_ref.clone()).await?)
                    } else {
                        None
                    };
                    if outcome.is_some() {
                        satisfied += 1;
                    }
                    let continuations = continuation_receipts_for(&self.pool, &work_ref).await?;
                    if items.len() < selection.limit {
                        items.push(SnapshotItem::Work(WorkSnapshot {
                            work_ref,
                            input: serde_json::from_str(&input)?,
                            dispatch_state: parse_dispatch_state(
                                row.try_get::<String, _>("dispatch_state")?.as_str(),
                            )?,
                            outcome,
                            continuations,
                            observed,
                        }));
                    }
                }
                Selector::Reply(id) => {
                    let row = sqlx::query(
                        "SELECT * FROM mailbox_messages
                         WHERE id = ? AND sender = ? AND kind = '\"request\"'
                           AND (? IS NULL OR recipient = ?) LIMIT 1",
                    )
                    .bind(id)
                    .bind(selection.caller.to_string())
                    .bind(selection.agent.map(|agent| agent.to_string()))
                    .bind(selection.agent.map(|agent| agent.to_string()))
                    .fetch_optional(&self.pool)
                    .await?;
                    let Some(row) = row else {
                        continue;
                    };
                    matched += 1;
                    let reply_row = sqlx::query(
                        "SELECT * FROM mailbox_messages
                         WHERE reply_to = ? AND recipient = ? ORDER BY seq LIMIT 1",
                    )
                    .bind(id)
                    .bind(selection.caller.to_string())
                    .fetch_optional(&self.pool)
                    .await?;
                    let reply = reply_row.as_ref().map(envelope_from_row).transpose()?;
                    let replacement: Option<String> = row.try_get("superseded_by")?;
                    let withdrawn = row.try_get::<i64, _>("withdrawn")? != 0;
                    let disposition = if withdrawn {
                        RequestDisposition::Withdrawn
                    } else if replacement.is_some() {
                        RequestDisposition::Superseded
                    } else if reply.is_some() {
                        RequestDisposition::Replied
                    } else {
                        RequestDisposition::Open
                    };
                    let observed = reply_row
                        .as_ref()
                        .map(|reply| reply.try_get::<i64, _>("observed").unwrap_or(0) != 0)
                        .unwrap_or(false);
                    if disposition != RequestDisposition::Open {
                        satisfied += 1;
                    }
                    if items.len() < selection.limit {
                        items.push(SnapshotItem::Request(RequestSnapshot {
                            request_id: id.clone(),
                            request: envelope_from_row(&row)?,
                            reply,
                            disposition,
                            replacement,
                            observed,
                        }));
                    }
                }
            }
        }
        let state = if matched == 0 {
            SnapshotState::NoMatchingWork
        } else if match selection.return_when {
            ReturnWhen::Any => satisfied > 0,
            ReturnWhen::All => satisfied == unique.len() && matched == unique.len(),
        } {
            SnapshotState::AlreadySatisfied
        } else {
            SnapshotState::Pending
        };
        Ok(Snapshot { state, items })
    }

    /// Acknowledge exact canonical outcomes, replies, or one-shot attention.
    /// Work acknowledgements are issuer-bound; replies/attention are recipient-bound.
    pub async fn acknowledge_observed(
        &self,
        recipient: ThreadId,
        targets: &[ObservationTarget],
    ) -> MailboxResult<()> {
        if targets.is_empty() || targets.len() > MAX_OBSERVATIONS {
            return Err(MailboxError::InvalidInput(
                "observation count must be between 1 and 100".into(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        for target in targets {
            match target {
                ObservationTarget::Work(work_ref) => {
                    if work_ref.issuer != recipient {
                        return Err(MailboxError::WrongRecipient);
                    }
                    let row = sqlx::query(
                        "SELECT outcome_canonical, outcome_json, outcome_id FROM mailbox_work
                         WHERE id = ? AND issuer = ? AND agent = ? AND submission_id = ?",
                    )
                    .bind(&work_ref.id)
                    .bind(recipient.to_string())
                    .bind(work_ref.agent.to_string())
                    .bind(&work_ref.submission_id)
                    .fetch_optional(&mut *transaction)
                    .await?
                    .ok_or(MailboxError::ReferenceMismatch)?;
                    if row.try_get::<Option<String>, _>("outcome_json")?.is_none() {
                        return Err(MailboxError::ReferenceMismatch);
                    }
                    if row.try_get::<i64, _>("outcome_canonical")? == 0 {
                        return Err(MailboxError::NotCanonical);
                    }
                    sqlx::query(
                        "UPDATE mailbox_work SET outcome_observed = 1
                         WHERE id = ? AND issuer = ? AND agent = ? AND submission_id = ?",
                    )
                    .bind(&work_ref.id)
                    .bind(recipient.to_string())
                    .bind(work_ref.agent.to_string())
                    .bind(&work_ref.submission_id)
                    .execute(&mut *transaction)
                    .await?;
                    // This notification describes the same exact admitted result body,
                    // not a separate request or an inference of business acceptance.
                    sqlx::query(
                        "UPDATE mailbox_attention SET observed = 1
                         WHERE recipient = ? AND canonical = 1 AND id IN (?, ?)",
                    )
                    .bind(recipient.to_string())
                    .bind(format!("attention:{}", work_ref.id))
                    .bind(format!("attention:{}", row.try_get::<String, _>("outcome_id")?))
                    .execute(&mut *transaction)
                    .await?;
                }
                ObservationTarget::Reply(id) => {
                    let result = sqlx::query(
                        "UPDATE mailbox_messages SET observed = 1
                         WHERE id = ? AND recipient = ? AND reply_to IS NOT NULL AND canonical = 1",
                    )
                    .bind(id)
                    .bind(recipient.to_string())
                    .execute(&mut *transaction)
                    .await?;
                    ensure_observation_target(
                        &mut transaction,
                        result.rows_affected(),
                        id,
                        recipient.to_string().as_str(),
                    )
                    .await?;
                }
                ObservationTarget::Attention(id) => {
                    acknowledge_attention(&mut transaction, id, recipient.to_string().as_str()).await?;
                }
            }
        }
        transaction.commit().await?;
        self.notify_changed();
        Ok(())
    }

    /// Non-destructive bounded view of separate one-shot attention obligations.
    pub async fn pending_attention(
        &self,
        recipient: ThreadId,
        after_seq: Option<i64>,
        limit: usize,
    ) -> MailboxResult<Vec<Attention>> {
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(MailboxError::InvalidInput(
                "attention limit must be between 1 and 100".into(),
            ));
        }
        let rows = sqlx::query(
            "SELECT attention.* FROM mailbox_attention AS attention
             LEFT JOIN mailbox_messages AS message
               ON attention.id = 'attention:' || message.id
             WHERE attention.recipient = ? AND attention.observed = 0
               AND (message.id IS NULL OR (message.superseded_by IS NULL AND message.withdrawn = 0))
               AND (? IS NULL OR attention.seq > ?)
             ORDER BY attention.seq LIMIT ?",
        )
        .bind(recipient.to_string())
        .bind(after_seq)
        .bind(after_seq)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(Attention {
                    id: row.try_get("id")?,
                    recipient: ThreadId::from_string(row.try_get::<String, _>("recipient")?.as_str())
                        .map_err(|_| MailboxError::InvalidInput("stored attention thread ID".into()))?,
                    canonical: row.try_get::<i64, _>("canonical")? != 0,
                    observed: row.try_get::<i64, _>("observed")? != 0,
                })
            })
            .collect()
    }

    /// First-page attention wake view; obligations remain until exact ack.
    pub async fn attention(
        &self,
        recipient: ThreadId,
        limit: usize,
    ) -> MailboxResult<Vec<Attention>> {
        self.pending_attention(recipient, None, limit).await
    }
}

async fn acknowledge_attention(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    recipient: &str,
) -> MailboxResult<()> {
    let result = sqlx::query(
        "UPDATE mailbox_attention SET observed = 1
         WHERE id = ? AND recipient = ? AND canonical = 1",
    )
    .bind(id)
    .bind(recipient)
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() == 0 {
        let row = sqlx::query(
            "SELECT canonical FROM mailbox_attention WHERE id = ? AND recipient = ?",
        )
        .bind(id)
        .bind(recipient)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(MailboxError::ReferenceMismatch)?;
        if row.try_get::<i64, _>("canonical")? == 0 {
            return Err(MailboxError::NotCanonical);
        }
    }
    Ok(())
}

async fn ensure_observation_target(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    rows_affected: u64,
    id: &str,
    recipient: &str,
) -> MailboxResult<()> {
    if rows_affected > 0 {
        return Ok(());
    }
    let row = sqlx::query(
        "SELECT canonical, reply_to FROM mailbox_messages WHERE id = ? AND recipient = ?",
    )
    .bind(id)
    .bind(recipient)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(MailboxError::ReferenceMismatch)?;
    if row.try_get::<Option<String>, _>("reply_to")?.is_none() {
        return Err(MailboxError::ReferenceMismatch);
    }
    if row.try_get::<i64, _>("canonical")? == 0 {
        return Err(MailboxError::NotCanonical);
    }
    Ok(())
}

/// Return true only when every choice at every reachable peer is trapped in a
/// closed waiting component. Unknown peers and open exits make the result false.
pub fn is_provably_closed_peer_cycle(start: &str, dependencies: &[Dependency]) -> bool {
    let mut by_owner = HashMap::new();
    for dependency in dependencies {
        if dependency.peers.is_empty() || by_owner.contains_key(dependency.owner.as_str()) {
            return false;
        }
        by_owner.insert(dependency.owner.as_str(), dependency);
    }
    let mut closed: HashSet<&str> = by_owner.keys().copied().collect();
    loop {
        let previous_len = closed.len();
        let previous = closed.clone();
        closed.retain(|owner| {
            let Some(dependency) = by_owner.get(owner) else {
                return false;
            };
            match dependency.mode {
                DependencyMode::Any => dependency
                    .peers
                    .iter()
                    .all(|peer| previous.contains(peer.as_str())),
                DependencyMode::All => dependency
                    .peers
                    .iter()
                    .any(|peer| previous.contains(peer.as_str())),
            }
        });
        if closed.len() == previous_len {
            break;
        }
    }
    closed.contains(start)
}
