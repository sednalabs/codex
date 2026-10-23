use crate::error::{MailboxError, MailboxResult};
use crate::messages::insert_message_tx;
use crate::store::Mailbox;
use crate::types::{
    Continuation, ContinuationReceipt, ContinuationState, GoalBinding, MessageDraft,
    MessageKind, Outcome, OutcomeReceipt, TurnOutcomeReceipt, WorkRef,
};
use crate::work::{
    ensure_mailbox_id, exact_work, exact_work_tx, new_mailbox_id, work_ref_from_row,
};
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::{AgentPath, ThreadId};
use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

const MAX_TURN_INPUTS: usize = 100;
const MAX_KEY_BYTES: usize = 256;
const MAX_AUTO_RESPONSE_BYTES: usize = 16_384;
const MAX_RESPONSE_BYTES: usize = 8_192;
const MAX_CONTINUATIONS_PER_WORK: i64 = 32;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct OutcomeData {
    outcome: Outcome,
    continuation: Continuation,
    response: Option<InterAgentCommunication>,
}

impl OutcomeData {
    fn new(
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

struct ResponseGroup {
    issuer: ThreadId,
    agent: ThreadId,
    author: AgentPath,
    recipient: AgentPath,
    refs: Vec<WorkRef>,
    response: Option<InterAgentCommunication>,
    output_id: Option<String>,
}

impl Mailbox {
    /// Freeze and record only the work admitted to this native turn. A pending
    /// or deferred goal binding is stored separately and is not a terminal outcome.
    pub async fn record_turn_outcome(
        &self,
        recipient: ThreadId,
        source_turn_id: &str,
        outcome: Outcome,
        response: Option<InterAgentCommunication>,
        continuation: Continuation,
    ) -> MailboxResult<TurnOutcomeReceipt> {
        validate_key(source_turn_id)?;
        let mut transaction = self.pool.begin().await?;
        let admitted_rows = sqlx::query(
            "SELECT work.* FROM mailbox_work AS work
             JOIN mailbox_admissions AS admission ON admission.work_id = work.id
             WHERE admission.recipient = ? AND admission.source_turn_id = ?
             ORDER BY work.seq LIMIT ?",
        )
        .bind(recipient.to_string())
        .bind(source_turn_id)
        .bind((MAX_TURN_INPUTS + 1) as i64)
        .fetch_all(&mut *transaction)
        .await?;
        if admitted_rows.len() > MAX_TURN_INPUTS {
            return Err(MailboxError::Backpressure("turn outcome input count"));
        }
        let mut refs = admitted_rows
            .iter()
            .map(work_ref_from_row)
            .collect::<MailboxResult<Vec<_>>>()?;

        if matches!(&continuation, Continuation::None | Continuation::Terminal(_)) {
            let binding_json = match &continuation {
                Continuation::Terminal(binding) => Some(serde_json::to_string(binding)?),
                _ => None,
            };
            let mut eligible = Vec::with_capacity(refs.len());
            for work_ref in refs {
                let unresolved = sqlx::query_scalar::<_, String>(
                    "SELECT binding_json FROM mailbox_continuations
                     WHERE work_id = ? AND state IN ('pending_goal_binding', 'deferred')
                     ORDER BY binding_json LIMIT ?",
                )
                .bind(&work_ref.id)
                .bind((MAX_CONTINUATIONS_PER_WORK + 1) as i64)
                .fetch_all(&mut *transaction)
                .await?;
                if unresolved.len() > MAX_CONTINUATIONS_PER_WORK as usize {
                    return Err(MailboxError::Backpressure("work continuation count"));
                }
                if unresolved.is_empty()
                    || binding_json.as_ref().is_some_and(|binding| unresolved.contains(binding))
                {
                    eligible.push(work_ref);
                }
            }
            refs = eligible;
        }

        if let Continuation::Terminal(binding) = &continuation {
            validate_binding_for_agent(binding, recipient)?;
            let binding_json = serde_json::to_string(binding)?;
            let linked = sqlx::query(
                "SELECT work.* FROM mailbox_continuations AS continuation
                 JOIN mailbox_work AS work ON work.id = continuation.work_id
                 WHERE continuation.binding_json = ?
                   AND continuation.state IN ('pending_goal_binding', 'deferred')
                   AND work.agent = ? AND work.outcome_json IS NULL
                 ORDER BY work.seq LIMIT ?",
            )
            .bind(&binding_json)
            .bind(recipient.to_string())
            .bind((MAX_TURN_INPUTS + 1) as i64)
            .fetch_all(&mut *transaction)
            .await?;
            if linked.len() > MAX_TURN_INPUTS {
                return Err(MailboxError::Backpressure("continuation settlement count"));
            }
            for row in linked {
                let work_ref = work_ref_from_row(&row)?;
                if !refs.iter().any(|existing| existing.id == work_ref.id) {
                    refs.push(work_ref);
                }
            }
            if refs.len() > MAX_TURN_INPUTS {
                return Err(MailboxError::Backpressure("turn outcome input count"));
            }
        }

        if refs.is_empty() {
            transaction.commit().await?;
            return Ok(TurnOutcomeReceipt {
                outcomes: Vec::new(),
                continuation_work: Vec::new(),
                no_matching_work: true,
                already_recorded: false,
            });
        }
        for work_ref in &refs {
            if work_ref.agent != recipient {
                return Err(MailboxError::WrongRecipient);
            }
            if let Some(binding) = continuation_binding(&continuation) {
                validate_binding_for_work(binding, work_ref)?;
            }
        }

        let mut groups = response_groups(&mut transaction, &refs, response.as_ref()).await?;
        validate_auto_response_size(&groups)?;
        if is_nonterminal(&continuation) {
            let binding = continuation_binding(&continuation)
                .expect("nonterminal continuation always carries a binding");
            let binding_json = serde_json::to_string(binding)?;
            for work_ref in &refs {
                let unresolved = sqlx::query_scalar::<_, String>(
                    "SELECT binding_json FROM mailbox_continuations
                     WHERE work_id = ? AND state IN ('pending_goal_binding', 'deferred')
                     ORDER BY binding_json LIMIT ?",
                )
                .bind(&work_ref.id)
                .bind((MAX_CONTINUATIONS_PER_WORK + 1) as i64)
                .fetch_all(&mut *transaction)
                .await?;
                if unresolved.len() > MAX_CONTINUATIONS_PER_WORK as usize {
                    return Err(MailboxError::Backpressure("work continuation count"));
                }
                if unresolved.iter().any(|old_binding| old_binding != &binding_json) {
                    return Err(MailboxError::ReferenceMismatch);
                }
            }
            for group in &mut groups {
                let _response_id = insert_response_group(
                    &mut transaction,
                    group,
                    format!("continuation:{}:{}:{}", recipient, source_turn_id, group.issuer),
                    outcome,
                )
                .await?;
            }
            let state = continuation_state(&continuation);
            for work_ref in &refs {
                let row = exact_work_tx(&mut transaction, work_ref).await?;
                if row.try_get::<Option<String>, _>("outcome_json")?.is_some() {
                    return Err(MailboxError::OutcomeAlreadyRecorded);
                }
                let response_id = groups
                    .iter()
                    .find(|group| group.issuer == work_ref.issuer)
                    .and_then(|group| group.output_id.as_deref());
                persist_continuation(
                    &mut transaction,
                    work_ref,
                    &binding_json,
                    state,
                    outcome,
                    source_turn_id,
                    response_id,
                )
                .await?;
            }
            transaction.commit().await?;
            self.notify_changed();
            return Ok(TurnOutcomeReceipt {
                outcomes: Vec::new(),
                continuation_work: refs,
                no_matching_work: false,
                already_recorded: false,
            });
        }

        let binding_json = match &continuation {
            Continuation::Terminal(binding) => Some(serde_json::to_string(binding)?),
            _ => None,
        };
        let mut payloads = Vec::with_capacity(refs.len());
        let mut existing_ids = Vec::with_capacity(refs.len());
        let mut already_recorded = true;
        for work_ref in &refs {
            let response = groups
                .iter()
                .find(|group| group.issuer == work_ref.issuer)
                .and_then(|group| group.response.clone());
            let data = OutcomeData::new(outcome, continuation.clone(), response);
            let payload = serde_json::to_string(&data)?;
            let row = exact_work_tx(&mut transaction, work_ref).await?;
            let old: Option<String> = row.try_get("outcome_json")?;
            let old_id: Option<String> = row.try_get("outcome_id")?;
            if let Some(old) = old {
                if old != payload {
                    return Err(MailboxError::OutcomeAlreadyRecorded);
                }
                existing_ids.push(old_id);
            } else {
                already_recorded = false;
                existing_ids.push(None);
            }
            payloads.push(payload);
        }

        for group in &mut groups {
            let group_has_existing = refs.iter().enumerate().any(|(index, work_ref)| {
                work_ref.issuer == group.issuer && existing_ids[index].is_some()
            });
            let distinct_existing = refs
                .iter()
                .enumerate()
                .filter(|(_, work_ref)| work_ref.issuer == group.issuer)
                .filter_map(|(index, _)| existing_ids[index].as_deref())
                .collect::<std::collections::HashSet<_>>();
            if distinct_existing.len() > 1 {
                return Err(MailboxError::ReferenceMismatch);
            }
            group.output_id = distinct_existing.iter().next().map(|id| (*id).to_string());
            if group.output_id.is_none() {
                group.output_id = insert_response_group(
                    &mut transaction,
                    group,
                    format!("turn-outcome:{}:{}:{}", recipient, source_turn_id, group.issuer),
                    outcome,
                )
                .await?;
                if group.output_id.is_none() {
                    group.output_id = Some(new_mailbox_id());
                }
            } else if group.response.is_some() && group_has_existing {
                // Existing work outcome data owns the ID; its response message was
                // inserted in the same transaction as the original outcome.
            }
            ensure_mailbox_id(
                group
                    .output_id
                    .as_deref()
                    .ok_or(MailboxError::ReferenceMismatch)?,
            )?;
        }

        for (index, work_ref) in refs.iter().enumerate() {
            let output_id = groups
                .iter()
                .find(|group| group.issuer == work_ref.issuer)
                .and_then(|group| group.output_id.as_deref())
                .ok_or(MailboxError::ReferenceMismatch)?;
            if existing_ids[index].is_none() {
                save_outcome_tx(
                    &mut transaction,
                    work_ref,
                    &payloads[index],
                    output_id,
                    outcome,
                )
                .await?;
            }
        }
        if let Some(binding_json) = binding_json.as_deref() {
            for work_ref in &refs {
                sqlx::query(
                    "UPDATE mailbox_continuations SET state = 'terminal'
                     WHERE work_id = ? AND binding_json = ?
                       AND state IN ('pending_goal_binding', 'deferred')",
                )
                .bind(&work_ref.id)
                .bind(binding_json)
                .execute(&mut *transaction)
                .await?;
            }
        }
        transaction.commit().await?;
        if !already_recorded || binding_json.is_some() {
            self.notify_changed();
        }
        let mut receipts = Vec::with_capacity(refs.len());
        for work_ref in refs {
            let group_id = groups
                .iter()
                .find(|group| group.issuer == work_ref.issuer)
                .and_then(|group| group.output_id.clone());
            let response_id = groups
                .iter()
                .find(|group| group.issuer == work_ref.issuer)
                .and_then(|group| group.response.as_ref().map(|_| group.output_id.clone()))
                .flatten();
            receipts.push(
                outcome_receipt_for(&self.pool, work_ref, group_id, response_id).await?,
            );
        }
        Ok(TurnOutcomeReceipt {
            outcomes: receipts,
            continuation_work: Vec::new(),
            no_matching_work: false,
            already_recorded,
        })
    }

    /// Record one exact terminal result. Pending/deferred work must use a turn
    /// admission so its continuation link is associated with the accepted turn.
    pub async fn record_outcome(
        &self,
        work_ref: &WorkRef,
        outcome: Outcome,
        response: Option<InterAgentCommunication>,
        continuation: Continuation,
    ) -> MailboxResult<OutcomeReceipt> {
        if is_nonterminal(&continuation) {
            return Err(MailboxError::InvalidInput(
                "pending continuation requires admitted turn linkage".into(),
            ));
        }
        if let Continuation::Terminal(binding) = &continuation {
            validate_binding_for_work(binding, work_ref)?;
        }
        let mut transaction = self.pool.begin().await?;
        let mut groups = response_groups(&mut transaction, &[work_ref.clone()], response.as_ref()).await?;
        let mut group = groups.pop().ok_or(MailboxError::ReferenceMismatch)?;
        validate_auto_response_size(std::slice::from_ref(&group))?;
        let data = OutcomeData::new(outcome, continuation.clone(), group.response.clone());
        let payload = serde_json::to_string(&data)?;
        let row = exact_work_tx(&mut transaction, work_ref).await?;
        if let Some(old) = row.try_get::<Option<String>, _>("outcome_json")? {
            if old != payload {
                return Err(MailboxError::OutcomeAlreadyRecorded);
            }
            let id: String = row.try_get("outcome_id")?;
            transaction.commit().await?;
            return outcome_receipt_for(&self.pool, work_ref.clone(), Some(id), response.is_some()).await;
        }
        if let Continuation::Terminal(binding) = &continuation {
            let binding_json = serde_json::to_string(binding)?;
            let unresolved: Option<String> = sqlx::query_scalar(
                "SELECT binding_json FROM mailbox_continuations
                 WHERE work_id = ? AND state IN ('pending_goal_binding', 'deferred') LIMIT 1",
            )
            .bind(&work_ref.id)
            .fetch_optional(&mut *transaction)
            .await?;
            if unresolved.as_deref().is_some_and(|stored| stored != binding_json) {
                return Err(MailboxError::ReferenceMismatch);
            }
        }
        group.output_id = insert_response_group(
            &mut transaction,
            &group,
            format!("work-outcome:{}", work_ref.id),
            outcome,
        )
        .await?;
        let output_id = group.output_id.as_deref().unwrap_or_else(|| new_mailbox_id());
        ensure_mailbox_id(output_id)?;
        save_outcome_tx(&mut transaction, work_ref, &payload, output_id, outcome).await?;
        if let Continuation::Terminal(binding) = &continuation {
            sqlx::query(
                "UPDATE mailbox_continuations SET state = 'terminal'
                 WHERE work_id = ? AND binding_json = ?
                   AND state IN ('pending_goal_binding', 'deferred')",
            )
            .bind(&work_ref.id)
            .bind(serde_json::to_string(binding)?)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        self.notify_changed();
        outcome_receipt_for(&self.pool, work_ref.clone(), Some(output_id.to_string()), response.is_some()).await
    }
}

async fn response_groups(
    transaction: &mut Transaction<'_, Sqlite>,
    refs: &[WorkRef],
    response: Option<&InterAgentCommunication>,
) -> MailboxResult<Vec<ResponseGroup>> {
    let mut groups: Vec<ResponseGroup> = Vec::new();
    for work_ref in refs {
        let row = exact_work_tx(transaction, work_ref).await?;
        let input: InterAgentCommunication =
            serde_json::from_str(&row.try_get::<String, _>("input_json")?)?;
        let group = groups
            .iter_mut()
            .find(|group| group.issuer == work_ref.issuer);
        if let Some(group) = group {
            if group.agent != work_ref.agent
                || group.author != input.recipient
                || group.recipient != input.author
            {
                return Err(MailboxError::InvalidInput(
                    "issuer has inconsistent accepted communication paths".into(),
                ));
            }
            group.refs.push(work_ref.clone());
        } else {
            let mut response = response.cloned();
            if let Some(response) = response.as_mut() {
                response.author = input.recipient.clone();
                response.recipient = input.author.clone();
                response.id = None;
            }
            groups.push(ResponseGroup {
                issuer: work_ref.issuer,
                agent: work_ref.agent,
                author: input.recipient,
                recipient: input.author,
                refs: vec![work_ref.clone()],
                response,
                output_id: None,
            });
        }
    }
    Ok(groups)
}

fn validate_auto_response_size(groups: &[ResponseGroup]) -> MailboxResult<()> {
    let mut total = 0_usize;
    for group in groups {
        if let Some(response) = group.response.as_ref() {
            let bytes = response.content.len() + response.encrypted_content.as_ref().map_or(0, String::len);
            if bytes > MAX_RESPONSE_BYTES {
                return Err(MailboxError::Backpressure("response message byte limit"));
            }
            total = total.saturating_add(bytes);
        }
    }
    if total > MAX_AUTO_RESPONSE_BYTES {
        return Err(MailboxError::Backpressure("automatic response aggregate byte limit"));
    }
    Ok(())
}

async fn insert_response_group(
    transaction: &mut Transaction<'_, Sqlite>,
    group: &mut ResponseGroup,
    invocation_key: String,
    outcome: Outcome,
) -> MailboxResult<Option<String>> {
    let Some(mut communication) = group.response.clone() else {
        return Ok(None);
    };
    communication.author = group.author.clone();
    communication.recipient = group.recipient.clone();
    communication.id = None;
    let envelope = insert_message_tx(
        transaction,
        MessageDraft {
            invocation_key,
            sender: group.agent,
            recipient: group.issuer,
            communication,
            kind: MessageKind::Note,
            activate_if_idle: false,
            work_ref: None,
            reply_to: None,
            supersedes: None,
            legacy_attention: outcome == Outcome::NeedsAttention,
        },
        None,
    )
    .await?;
    group.output_id = Some(envelope.id.clone());
    Ok(group.output_id.clone())
}

async fn persist_continuation(
    transaction: &mut Transaction<'_, Sqlite>,
    work_ref: &WorkRef,
    binding_json: &str,
    state: &str,
    outcome: Outcome,
    source_turn_id: &str,
    response_message_id: Option<&str>,
) -> MailboxResult<()> {
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM mailbox_continuations WHERE work_id = ? AND binding_json = ? LIMIT 1",
    )
    .bind(&work_ref.id)
    .bind(binding_json)
    .fetch_optional(&mut **transaction)
    .await?;
    if exists.is_none() {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mailbox_continuations WHERE work_id = ?",
        )
        .bind(&work_ref.id)
        .fetch_one(&mut **transaction)
        .await?;
        if count >= MAX_CONTINUATIONS_PER_WORK {
            return Err(MailboxError::Backpressure("work continuation count"));
        }
    }
    sqlx::query(
        "INSERT INTO mailbox_continuations(
            work_id, binding_json, state, outcome, source_turn_id, response_message_id
         ) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(work_id, binding_json) DO UPDATE SET
           state = CASE
             WHEN mailbox_continuations.state = 'terminal' THEN 'terminal'
             WHEN excluded.state = 'deferred' THEN 'deferred'
             ELSE mailbox_continuations.state
           END,
           outcome = excluded.outcome,
           response_message_id = COALESCE(excluded.response_message_id,
                                          mailbox_continuations.response_message_id)",
    )
    .bind(&work_ref.id)
    .bind(binding_json)
    .bind(state)
    .bind(outcome_label(outcome))
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
        "UPDATE mailbox_work SET outcome_json = ?, outcome_id = ?
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
        output_id.or_else(|| row.try_get::<Option<String>, _>("outcome_id").ok().flatten())
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
    let has_response = serde_json::from_str::<OutcomeData>(
        &row.try_get::<String, _>("outcome_json")?,
    )?
    .response
    .is_some();
    outcome_receipt_for(pool, work_ref, None, has_response).await
}

pub(crate) async fn continuation_receipts_for(
    pool: &SqlitePool,
    work_ref: &WorkRef,
) -> MailboxResult<Vec<ContinuationReceipt>> {
    let rows = sqlx::query(
        "SELECT binding_json, state, outcome, source_turn_id, response_message_id
         FROM mailbox_continuations
         WHERE work_id = ? AND state IN ('pending_goal_binding', 'deferred')
         ORDER BY source_turn_id LIMIT ?",
    )
    .bind(&work_ref.id)
    .bind((MAX_CONTINUATIONS_PER_WORK + 1) as i64)
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
                _ => return Err(MailboxError::InvalidInput("stored continuation state".into())),
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

fn is_nonterminal(continuation: &Continuation) -> bool {
    matches!(
        continuation,
        Continuation::PendingGoalBinding(_) | Continuation::Deferred(_)
    )
}

fn continuation_binding(continuation: &Continuation) -> Option<&GoalBinding> {
    match continuation {
        Continuation::None => None,
        Continuation::PendingGoalBinding(binding)
        | Continuation::Deferred(binding)
        | Continuation::Terminal(binding) => Some(binding),
    }
}

fn continuation_state(continuation: &Continuation) -> &'static str {
    match continuation {
        Continuation::PendingGoalBinding(_) => "pending_goal_binding",
        Continuation::Deferred(_) => "deferred",
        _ => "terminal",
    }
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
        _ => Err(MailboxError::InvalidInput("stored continuation outcome".into())),
    }
}

fn validate_binding_for_agent(binding: &GoalBinding, agent: ThreadId) -> MailboxResult<()> {
    if binding.child_thread_id != agent {
        return Err(MailboxError::WrongRecipient);
    }
    validate_key(&binding.source_turn_id)
}

fn validate_binding_for_work(binding: &GoalBinding, work_ref: &WorkRef) -> MailboxResult<()> {
    if binding.parent_thread_id != work_ref.issuer || binding.child_thread_id != work_ref.agent {
        return Err(MailboxError::WrongRecipient);
    }
    validate_key(&binding.goal_id)?;
    validate_key(&binding.source_turn_id)
}

fn validate_key(key: &str) -> MailboxResult<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        Err(MailboxError::InvalidInput("invalid native turn or binding key".into()))
    } else {
        Ok(())
    }
}
