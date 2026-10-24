use crate::error::MailboxError;
use crate::error::MailboxResult;
use crate::outcome_messages::ensure_terminal_responses;
use crate::outcome_messages::insert_response_group;
use crate::outcome_messages::response_groups;
use crate::outcome_messages::validate_auto_response_size;
use crate::outcome_store::MAX_CONTINUATIONS_PER_WORK;
use crate::outcome_store::OutcomeData;
use crate::outcome_store::outcome_receipt_for;
use crate::outcome_store::persist_continuation;
use crate::outcome_store::save_outcome_tx;
use crate::store::Mailbox;
use crate::types::Continuation;
use crate::types::GoalBinding;
use crate::types::Outcome;
use crate::types::OutcomeReceipt;
use crate::types::TurnOutcomeReceipt;
use crate::types::WorkRef;
use crate::work::ensure_mailbox_id;
use crate::work::exact_work_tx;
use crate::work::new_mailbox_id;
use crate::work::work_ref_from_row;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use sqlx::Row;

const MAX_TURN_INPUTS: usize = 100;
const MAX_KEY_BYTES: usize = 256;

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
        let _input_arrival = self.begin_input_arrival();
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

        if matches!(
            &continuation,
            Continuation::None | Continuation::Terminal(_)
        ) {
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
                .bind(MAX_CONTINUATIONS_PER_WORK + 1)
                .fetch_all(&mut *transaction)
                .await?;
                if unresolved.len() > MAX_CONTINUATIONS_PER_WORK as usize {
                    return Err(MailboxError::Backpressure("work continuation count"));
                }
                if unresolved.is_empty()
                    || binding_json
                        .as_ref()
                        .is_some_and(|binding| unresolved.contains(binding))
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
            let binding = continuation_binding(&continuation).ok_or_else(|| {
                MailboxError::InvalidInput("nonterminal continuation is missing its binding".into())
            })?;
            let binding_json = serde_json::to_string(binding)?;
            for work_ref in &refs {
                let unresolved = sqlx::query_scalar::<_, String>(
                    "SELECT binding_json FROM mailbox_continuations
                     WHERE work_id = ? AND state IN ('pending_goal_binding', 'deferred')
                     ORDER BY binding_json LIMIT ?",
                )
                .bind(&work_ref.id)
                .bind(MAX_CONTINUATIONS_PER_WORK + 1)
                .fetch_all(&mut *transaction)
                .await?;
                if unresolved.len() > MAX_CONTINUATIONS_PER_WORK as usize {
                    return Err(MailboxError::Backpressure("work continuation count"));
                }
                if unresolved
                    .iter()
                    .any(|old_binding| old_binding != &binding_json)
                {
                    return Err(MailboxError::ReferenceMismatch);
                }
            }
            for group in &mut groups {
                let _response_id = insert_response_group(
                    &mut transaction,
                    group,
                    format!(
                        "continuation:{}:{}:{}",
                        recipient, source_turn_id, group.issuer
                    ),
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
            for group in &groups {
                if group.response.is_some() {
                    self.invalidate_waits(group.issuer);
                }
            }
            self.notify_changed();
            return Ok(TurnOutcomeReceipt {
                outcomes: Vec::new(),
                continuation_work: refs,
                no_matching_work: false,
                already_recorded: false,
            });
        }

        ensure_terminal_responses(&mut groups, &refs, outcome, &continuation, source_turn_id)?;
        validate_auto_response_size(&groups)?;

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
                    format!(
                        "turn-outcome:{}:{}:{}",
                        recipient, source_turn_id, group.issuer
                    ),
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
        for group in &groups {
            if group.response.is_some() {
                self.invalidate_waits(group.issuer);
            }
        }
        if !already_recorded || binding_json.is_some() {
            self.notify_changed();
        }
        let mut receipts = Vec::with_capacity(refs.len());
        for work_ref in refs {
            let group_id = groups
                .iter()
                .find(|group| group.issuer == work_ref.issuer)
                .and_then(|group| group.output_id.clone());
            let has_response = groups
                .iter()
                .find(|group| group.issuer == work_ref.issuer)
                .is_some_and(|group| group.response.is_some());
            receipts.push(outcome_receipt_for(&self.pool, work_ref, group_id, has_response).await?);
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
        let _input_arrival = self.begin_input_arrival();
        if is_nonterminal(&continuation) {
            return Err(MailboxError::InvalidInput(
                "pending continuation requires admitted turn linkage".into(),
            ));
        }
        if let Continuation::Terminal(binding) = &continuation {
            validate_binding_for_work(binding, work_ref)?;
        }
        let mut transaction = self.pool.begin().await?;
        let mut groups = response_groups(
            &mut transaction,
            std::slice::from_ref(work_ref),
            response.as_ref(),
        )
        .await?;
        let group = groups.pop().ok_or(MailboxError::ReferenceMismatch)?;
        let mut groups = vec![group];
        ensure_terminal_responses(
            &mut groups,
            std::slice::from_ref(work_ref),
            outcome,
            &continuation,
            &work_ref.id,
        )?;
        let mut group = groups.pop().ok_or(MailboxError::ReferenceMismatch)?;
        validate_auto_response_size(std::slice::from_ref(&group))?;
        let has_response = group.response.is_some();
        let data = OutcomeData::new(outcome, continuation.clone(), group.response.clone());
        let payload = serde_json::to_string(&data)?;
        let row = exact_work_tx(&mut transaction, work_ref).await?;
        if let Some(old) = row.try_get::<Option<String>, _>("outcome_json")? {
            if old != payload {
                return Err(MailboxError::OutcomeAlreadyRecorded);
            }
            let id: String = row.try_get("outcome_id")?;
            transaction.commit().await?;
            return outcome_receipt_for(&self.pool, work_ref.clone(), Some(id), has_response).await;
        }
        if let Continuation::Terminal(binding) = &continuation {
            let binding_json = serde_json::to_string(binding)?;
            let unresolved = sqlx::query_scalar::<_, String>(
                "SELECT binding_json FROM mailbox_continuations
                 WHERE work_id = ? AND state IN ('pending_goal_binding', 'deferred')
                 ORDER BY binding_json LIMIT ?",
            )
            .bind(&work_ref.id)
            .bind(MAX_CONTINUATIONS_PER_WORK + 1)
            .fetch_all(&mut *transaction)
            .await?;
            if unresolved.len() > MAX_CONTINUATIONS_PER_WORK as usize {
                return Err(MailboxError::Backpressure("work continuation count"));
            }
            if !unresolved.is_empty() && !unresolved.contains(&binding_json) {
                return Err(MailboxError::ReferenceMismatch);
            }
        }
        group.output_id = insert_response_group(
            &mut transaction,
            &mut group,
            format!("work-outcome:{}", work_ref.id),
            outcome,
        )
        .await?;
        let output_id = group.output_id.clone().unwrap_or_else(new_mailbox_id);
        ensure_mailbox_id(&output_id)?;
        save_outcome_tx(&mut transaction, work_ref, &payload, &output_id, outcome).await?;
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
        self.invalidate_waits(group.issuer);
        self.notify_changed();
        outcome_receipt_for(&self.pool, work_ref.clone(), Some(output_id), has_response).await
    }
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

fn validate_binding_for_agent(binding: &GoalBinding, agent: ThreadId) -> MailboxResult<()> {
    if binding.child_thread_id != agent {
        return Err(MailboxError::WrongRecipient);
    }
    validate_native_binding(binding)
}

fn validate_native_binding(binding: &GoalBinding) -> MailboxResult<()> {
    if binding.incarnation.is_none() {
        return Err(MailboxError::InvalidInput(
            "native goal incarnation is required".into(),
        ));
    }
    validate_key(&binding.goal_id)
}

fn validate_binding_for_work(binding: &GoalBinding, work_ref: &WorkRef) -> MailboxResult<()> {
    if binding.parent_thread_id != work_ref.issuer || binding.child_thread_id != work_ref.agent {
        return Err(MailboxError::WrongRecipient);
    }
    validate_native_binding(binding)
}

fn validate_key(key: &str) -> MailboxResult<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        Err(MailboxError::InvalidInput(
            "invalid native turn or binding key".into(),
        ))
    } else {
        Ok(())
    }
}
