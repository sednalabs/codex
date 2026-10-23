use crate::error::MailboxError;
use crate::error::MailboxResult;
use crate::types::Continuation;
use crate::types::ContinuationReceipt;
use crate::types::ContinuationState;
use crate::types::Outcome;
use crate::types::OutcomeReceipt;
use crate::types::WorkRef;
use crate::work::exact_work;
use codex_protocol::protocol::InterAgentCommunication;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;

pub(crate) const MAX_CONTINUATIONS_PER_WORK: i64 = 32;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct OutcomeData {
    pub(crate) outcome: Outcome,
    pub(crate) continuation: Continuation,
    pub(crate) response: Option<InterAgentCommunication>,
}

impl OutcomeData {
    pub(crate) fn new(
        outcome: Outcome,
        continuation: Continuation,
        mut response: Option<InterAgentCommunication>,
    ) -> Self {
        if let Some(response) = response.as_mut() {
            response.id = None;
        }
        Self {
            outcome,
            continuation,
            response,
        }
    }
}

pub(crate) async fn persist_continuation(
    transaction: &mut Transaction<'_, Sqlite>,
    work_ref: &WorkRef,
    binding_json: &str,
    state: &str,
    outcome: Outcome,
    source_turn_id: &str,
    response_message_id: Option<&str>,
) -> MailboxResult<()> {
    let existing = sqlx::query(
        "SELECT state, outcome, source_turn_id, response_message_id
         FROM mailbox_continuations WHERE work_id = ? AND binding_json = ? LIMIT 1",
    )
    .bind(&work_ref.id)
    .bind(binding_json)
    .fetch_optional(&mut **transaction)
    .await?;
    let outcome_text = outcome_label(outcome);
    if let Some(existing) = existing {
        let old_state: String = existing.try_get("state")?;
        let old_outcome: String = existing.try_get("outcome")?;
        let old_source_turn: String = existing.try_get("source_turn_id")?;
        let old_response: Option<String> = existing.try_get("response_message_id")?;
        if old_state == "terminal"
            || old_outcome != outcome_text
            || old_source_turn != source_turn_id
            || old_response.as_deref() != response_message_id
        {
            return Err(MailboxError::IdempotencyConflict);
        }
        if old_state == "pending_goal_binding" && state == "deferred" {
            sqlx::query(
                "UPDATE mailbox_continuations SET state = 'deferred'
                 WHERE work_id = ? AND binding_json = ? AND state = 'pending_goal_binding'",
            )
            .bind(&work_ref.id)
            .bind(binding_json)
            .execute(&mut **transaction)
            .await?;
        }
        return Ok(());
    }
    let count_rows = sqlx::query("SELECT 1 FROM mailbox_continuations WHERE work_id = ? LIMIT ?")
        .bind(&work_ref.id)
        .bind(MAX_CONTINUATIONS_PER_WORK + 1)
        .fetch_all(&mut **transaction)
        .await?;
    if count_rows.len() >= MAX_CONTINUATIONS_PER_WORK as usize {
        return Err(MailboxError::Backpressure("work continuation count"));
    }
    sqlx::query(
        "INSERT INTO mailbox_continuations(
            work_id, binding_json, state, outcome, source_turn_id, response_message_id
         ) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&work_ref.id)
    .bind(binding_json)
    .bind(state)
    .bind(outcome_text)
    .bind(source_turn_id)
    .bind(response_message_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn save_outcome_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    work_ref: &WorkRef,
    payload: &str,
    outcome_id: &str,
    outcome: Outcome,
) -> MailboxResult<()> {
    let result = sqlx::query(
        "UPDATE mailbox_work SET outcome_json = ?, outcome_id = ?, dispatch_state = 'dispatched'
         WHERE id = ? AND issuer = ? AND agent = ? AND submission_id = ?
           AND outcome_json IS NULL",
    )
    .bind(payload)
    .bind(outcome_id)
    .bind(&work_ref.id)
    .bind(work_ref.issuer.to_string())
    .bind(work_ref.agent.to_string())
    .bind(&work_ref.submission_id)
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() == 0 {
        return Err(MailboxError::ReferenceMismatch);
    }
    if outcome == Outcome::NeedsAttention {
        sqlx::query(
            "INSERT INTO mailbox_attention(id, recipient) VALUES (?, ?)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(format!("attention:{}", work_ref.id))
        .bind(work_ref.issuer.to_string())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

pub(crate) async fn outcome_receipt_for(
    pool: &SqlitePool,
    work_ref: WorkRef,
    output_id: Option<String>,
    has_response: bool,
) -> MailboxResult<OutcomeReceipt> {
    let row = exact_work(pool, &work_ref).await?;
    let stored: String = row
        .try_get::<Option<String>, _>("outcome_json")?
        .ok_or(MailboxError::ReferenceMismatch)?;
    let mut data: OutcomeData = serde_json::from_str(&stored)?;
    let response_message_id = if has_response {
        output_id.or_else(|| {
            row.try_get::<Option<String>, _>("outcome_id")
                .ok()
                .flatten()
        })
    } else {
        None
    };
    if let (Some(response), Some(id)) = (data.response.as_mut(), response_message_id.as_ref()) {
        response.id = Some(codex_protocol::ResponseItemId::from_server(id.clone()));
    }
    Ok(OutcomeReceipt {
        work_ref,
        id: row.try_get("outcome_id")?,
        outcome: data.outcome,
        continuation: data.continuation,
        response: data.response,
        response_message_id,
        canonical: row.try_get::<i64, _>("outcome_canonical")? != 0,
        observed: row.try_get::<i64, _>("outcome_observed")? != 0,
    })
}

pub(crate) async fn stored_outcome_receipt_for(
    pool: &SqlitePool,
    work_ref: WorkRef,
) -> MailboxResult<OutcomeReceipt> {
    let row = exact_work(pool, &work_ref).await?;
    let has_response =
        serde_json::from_str::<OutcomeData>(&row.try_get::<String, _>("outcome_json")?)?
            .response
            .is_some();
    outcome_receipt_for(pool, work_ref, /*output_id*/ None, has_response).await
}

pub(crate) async fn continuation_receipts_for(
    pool: &SqlitePool,
    work_ref: &WorkRef,
) -> MailboxResult<Vec<ContinuationReceipt>> {
    let rows = sqlx::query(
        "SELECT binding_json, state, outcome, source_turn_id, response_message_id
         FROM mailbox_continuations
         WHERE work_id = ?
         ORDER BY source_turn_id, binding_json LIMIT ?",
    )
    .bind(&work_ref.id)
    .bind(MAX_CONTINUATIONS_PER_WORK + 1)
    .fetch_all(pool)
    .await?;
    if rows.len() > MAX_CONTINUATIONS_PER_WORK as usize {
        return Err(MailboxError::Backpressure("work continuation view limit"));
    }
    rows.iter()
        .map(|row| {
            let state = match row.try_get::<String, _>("state")?.as_str() {
                "pending_goal_binding" => ContinuationState::PendingGoalBinding,
                "deferred" => ContinuationState::Deferred,
                "terminal" => ContinuationState::Terminal,
                _ => {
                    return Err(MailboxError::InvalidInput(
                        "stored continuation state".into(),
                    ));
                }
            };
            Ok(ContinuationReceipt {
                binding: serde_json::from_str(&row.try_get::<String, _>("binding_json")?)?,
                state,
                outcome: parse_outcome(row.try_get::<String, _>("outcome")?.as_str())?,
                source_turn_id: row.try_get("source_turn_id")?,
                response_message_id: row.try_get("response_message_id")?,
            })
        })
        .collect()
}

fn outcome_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Completed => "completed",
        Outcome::Failed => "failed",
        Outcome::Cancelled => "cancelled",
        Outcome::NeedsAttention => "needs_attention",
    }
}

fn parse_outcome(value: &str) -> MailboxResult<Outcome> {
    match value {
        "completed" => Ok(Outcome::Completed),
        "failed" => Ok(Outcome::Failed),
        "cancelled" => Ok(Outcome::Cancelled),
        "needs_attention" => Ok(Outcome::NeedsAttention),
        _ => Err(MailboxError::InvalidInput(
            "stored continuation outcome".into(),
        )),
    }
}
