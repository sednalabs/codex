use crate::error::MailboxError;
use crate::error::MailboxResult;
use crate::messages::decrement_pending_count;
use crate::outcome_store::MAX_CONTINUATIONS_PER_WORK;
use crate::store::Mailbox;
use crate::types::AdmissionReceipt;
use crate::types::DispatchDecision;
use crate::types::DispatchState;
use crate::types::GoalBinding;
use crate::types::NativeWork;
use crate::types::RecoveryEvidence;
use crate::types::WorkRef;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::Transaction;

const MAX_WORK_ITEMS: i64 = 50_000;
const MAX_INPUT_BYTES: usize = 1_000_000;
const MAX_NATIVE_BODY_BYTES: usize = 8_192;
const MAX_KEY_BYTES: usize = 256;
const MAX_TURN_INPUTS: usize = 100;

impl Mailbox {
    /// Resolve only the work covered by this exact canonical result body.
    pub async fn work_for_outcome(
        &self,
        issuer: ThreadId,
        outcome_id: &str,
    ) -> MailboxResult<Vec<WorkRef>> {
        let rows = sqlx::query(
            "SELECT * FROM mailbox_work WHERE issuer = ? AND outcome_id = ? ORDER BY seq LIMIT 101",
        )
        .bind(issuer.to_string())
        .bind(outcome_id)
        .fetch_all(&self.pool)
        .await?;
        if rows.len() > 100 {
            return Err(MailboxError::Backpressure("outcome work reference count"));
        }
        rows.iter().map(work_ref_from_row).collect()
    }

    /// Capture accepted native work and its input before the adapter activates it.
    /// Retries of the exact issuer-scoped native key return the same immutable ref.
    pub async fn accept_work(&self, mut work: NativeWork) -> MailboxResult<WorkRef> {
        validate_key(&work.native_key, "native key")?;
        validate_key(&work.submission_id, "submission id")?;

        let mut request = work.clone();
        request.input.id = None;
        let body_bytes =
            work.input.content.len() + work.input.encrypted_content.as_ref().map_or(0, String::len);
        if body_bytes > MAX_NATIVE_BODY_BYTES {
            return Err(MailboxError::Backpressure(
                "accepted native body byte limit",
            ));
        }
        let request_json = serde_json::to_string(&request)?;
        if request_json.len() > MAX_INPUT_BYTES {
            return Err(MailboxError::Backpressure("accepted input byte limit"));
        }
        if !work.input.id.as_ref().is_some_and(is_mailbox_id) {
            work.input.id = Some(codex_protocol::ResponseItemId::from_server(new_mailbox_id()));
        }
        let input_message_id =
            work.input
                .id
                .as_ref()
                .map(ToString::to_string)
                .ok_or_else(|| {
                    MailboxError::InvalidInput("accepted native input ID is missing".into())
                })?;
        let input_json = serde_json::to_string(&work.input)?;

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(row) =
            sqlx::query("SELECT * FROM mailbox_work WHERE issuer = ? AND native_key = ?")
                .bind(work.issuer.to_string())
                .bind(&work.native_key)
                .fetch_optional(&mut *transaction)
                .await?
        {
            if row.try_get::<String, _>("request_json")? != request_json {
                return Err(MailboxError::IdempotencyConflict);
            }
            let work_ref = work_ref_from_row(&row)?;
            transaction.commit().await?;
            return Ok(work_ref);
        }

        let duplicate_submission = sqlx::query_scalar::<_, String>(
            "SELECT id FROM mailbox_work WHERE agent = ? AND submission_id = ? LIMIT 1",
        )
        .bind(work.agent.to_string())
        .bind(&work.submission_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if duplicate_submission.is_some() {
            return Err(MailboxError::IdempotencyConflict);
        }
        let duplicate_input = sqlx::query_scalar::<_, String>(
            "SELECT id FROM mailbox_work WHERE input_message_id = ? LIMIT 1",
        )
        .bind(&input_message_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if duplicate_input.is_some() {
            return Err(MailboxError::IdempotencyConflict);
        }
        let count: i64 =
            sqlx::query_scalar("SELECT item_count FROM mailbox_counts WHERE name = 'work'")
                .fetch_one(&mut *transaction)
                .await?;
        if count >= MAX_WORK_ITEMS {
            return Err(MailboxError::Backpressure("retained work-item count"));
        }

        let work_ref = WorkRef {
            id: format!("work_{}", uuid::Uuid::now_v7()),
            issuer: work.issuer,
            agent: work.agent,
            submission_id: work.submission_id,
        };
        let inserted = sqlx::query(
            "INSERT INTO mailbox_work (
                id, issuer, agent, submission_id, native_key, request_json,
                input_json, input_message_id, dispatch_state
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'accepted')",
        )
        .bind(&work_ref.id)
        .bind(work_ref.issuer.to_string())
        .bind(work_ref.agent.to_string())
        .bind(&work_ref.submission_id)
        .bind(&work.native_key)
        .bind(request_json)
        .bind(input_json)
        .bind(input_message_id)
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 1 {
            sqlx::query(
                "UPDATE mailbox_counts SET item_count = item_count + 1 WHERE name = 'work'",
            )
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        self.notify_changed();
        Ok(work_ref)
    }

    /// Return the exact captured native input, including its assigned stable ID.
    pub async fn accepted_input(
        &self,
        work_ref: &WorkRef,
    ) -> MailboxResult<InterAgentCommunication> {
        let row = exact_work(&self.pool, work_ref).await?;
        Ok(serde_json::from_str(
            &row.try_get::<String, _>("input_json")?,
        )?)
    }

    pub async fn work_for_native_key(
        &self,
        issuer: ThreadId,
        native_key: &str,
    ) -> MailboxResult<Option<WorkRef>> {
        let row =
            sqlx::query("SELECT * FROM mailbox_work WHERE issuer = ? AND native_key = ? LIMIT 1")
                .bind(issuer.to_string())
                .bind(native_key)
                .fetch_optional(&self.pool)
                .await?;
        row.as_ref().map(work_ref_from_row).transpose()
    }

    pub async fn work_for_submission(
        &self,
        agent: ThreadId,
        submission_id: &str,
    ) -> MailboxResult<Option<WorkRef>> {
        let row =
            sqlx::query("SELECT * FROM mailbox_work WHERE agent = ? AND submission_id = ? LIMIT 1")
                .bind(agent.to_string())
                .bind(submission_id)
                .fetch_optional(&self.pool)
                .await?;
        row.as_ref().map(work_ref_from_row).transpose()
    }

    /// Resolve a work reference only when the supplied thread pair is exactly
    /// its issuer/agent pair, in either direction.
    pub async fn work_for_message_scope(
        &self,
        sender: ThreadId,
        recipient: ThreadId,
        id: &str,
    ) -> MailboxResult<Option<WorkRef>> {
        let row = sqlx::query("SELECT * FROM mailbox_work WHERE id = ? LIMIT 1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let work_ref = work_ref_from_row(&row)?;
        let pair_matches = (work_ref.issuer == sender && work_ref.agent == recipient)
            || (work_ref.agent == sender && work_ref.issuer == recipient);
        Ok(pair_matches.then_some(work_ref))
    }

    /// Freeze the caller's currently unsurfaced work list at wait entry.
    /// Completed outcomes remain included until their exact observation ack.
    pub async fn issued_work_refs(
        &self,
        issuer: ThreadId,
        agent: Option<ThreadId>,
        limit: usize,
    ) -> MailboxResult<Vec<WorkRef>> {
        if limit == 0 || limit > 100 {
            return Err(MailboxError::InvalidInput(
                "issued work limit must be between 1 and 100".into(),
            ));
        }
        let rows = sqlx::query(
            "SELECT * FROM mailbox_work
             WHERE issuer = ? AND outcome_observed = 0
               AND (? IS NULL OR agent = ?)
             ORDER BY seq LIMIT ?",
        )
        .bind(issuer.to_string())
        .bind(agent.map(|agent| agent.to_string()))
        .bind(agent.map(|agent| agent.to_string()))
        .bind((limit + 1) as i64)
        .fetch_all(&self.pool)
        .await?;
        if rows.len() > limit {
            return Err(MailboxError::Backpressure("issued work selection limit"));
        }
        rows.iter().map(work_ref_from_row).collect()
    }

    /// Atomically claim native activation. Only the first accepted caller may
    /// send; dispatching work requires exact adapter recovery, never a retry send.
    pub async fn begin_dispatch(&self, work_ref: &WorkRef) -> MailboxResult<DispatchDecision> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = exact_work_tx(&mut transaction, work_ref).await?;
        let state = parse_dispatch_state(row.try_get::<String, _>("dispatch_state")?.as_str())?;
        if state != DispatchState::Accepted {
            transaction.commit().await?;
            return Ok(match state {
                DispatchState::Dispatched => DispatchDecision::AlreadyDispatched,
                DispatchState::Dispatching | DispatchState::RecoveryNeeded => {
                    DispatchDecision::RecoveryNeeded
                }
                DispatchState::Accepted => unreachable!("handled above"),
            });
        }
        sqlx::query("UPDATE mailbox_work SET dispatch_state = 'dispatching' WHERE id = ?")
            .bind(&work_ref.id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        self.notify_changed();
        Ok(DispatchDecision::Dispatch)
    }

    /// The native host calls this only while resuming this exact issuer, before it can
    /// accept another invocation. A previous acceptance/dispatch claim cannot be retried
    /// by guessing whether its side effect happened. Retain its input and identity, but
    /// expose recovery-needed instead of leaving an ownerless join pending forever.
    pub async fn interrupt_issuer_dispatches(
        &self,
        issuer: ThreadId,
        limit: usize,
    ) -> MailboxResult<usize> {
        if limit == 0 || limit > 100 {
            return Err(MailboxError::InvalidInput(
                "dispatch recovery limit must be between 1 and 100".into(),
            ));
        }
        let _arrival = self.begin_input_arrival();
        let result = sqlx::query(
            "UPDATE mailbox_work SET dispatch_state = 'recovery_needed'
             WHERE id IN (
               SELECT id FROM mailbox_work
               WHERE issuer = ? AND dispatch_state IN ('accepted', 'dispatching')
                 AND outcome_json IS NULL
                 AND NOT EXISTS (
                   SELECT 1 FROM mailbox_continuations AS continuation
                   WHERE continuation.work_id = mailbox_work.id
                     AND continuation.state IN ('pending_goal_binding', 'deferred')
                 )
               LIMIT ?
             )",
        )
        .bind(issuer.to_string())
        .bind(limit as i64)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() as usize)
    }

    /// Record adapter-confirmed recovery evidence for this exact work.
    /// Returns whether this call changed the dispatch state.
    pub async fn mark_recovery_needed(
        &self,
        work_ref: &WorkRef,
        evidence: RecoveryEvidence,
    ) -> MailboxResult<bool> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = exact_work_tx(&mut transaction, work_ref).await?;
        let state = parse_dispatch_state(row.try_get::<String, _>("dispatch_state")?.as_str())?;
        if state == DispatchState::Accepted
            && !matches!(&evidence, RecoveryEvidence::RecipientIncompatible)
        {
            return Err(MailboxError::ReferenceMismatch);
        }

        let matching_binding_json = match &evidence {
            RecoveryEvidence::DeliveryUnavailable | RecoveryEvidence::RecipientIncompatible => None,
            RecoveryEvidence::ContinuationUnowned { binding } => {
                if binding.parent_thread_id != work_ref.issuer
                    || binding.child_thread_id != work_ref.agent
                {
                    return Err(MailboxError::WrongRecipient);
                }
                validate_key(&binding.goal_id, "goal id")?;
                let rows = sqlx::query(
                    "SELECT binding_json FROM mailbox_continuations
                     WHERE work_id = ? AND state IN ('pending_goal_binding', 'deferred')
                     ORDER BY binding_json LIMIT ?",
                )
                .bind(&work_ref.id)
                .bind(MAX_CONTINUATIONS_PER_WORK + 1)
                .fetch_all(&mut *transaction)
                .await?;
                if rows.len() > MAX_CONTINUATIONS_PER_WORK as usize {
                    return Err(MailboxError::Backpressure("work continuation count"));
                }
                let mut matched = None;
                for row in rows {
                    let stored_json: String = row.try_get("binding_json")?;
                    let stored: GoalBinding = serde_json::from_str(&stored_json)?;
                    if stored == *binding {
                        matched = Some(stored_json);
                        break;
                    }
                }
                matched
            }
        };

        let update = match (&evidence, matching_binding_json) {
            (RecoveryEvidence::DeliveryUnavailable, _) => {
                sqlx::query(
                    "UPDATE mailbox_work SET dispatch_state = 'recovery_needed'
                     WHERE id = ? AND dispatch_state IN ('dispatching', 'dispatched')
                       AND outcome_json IS NULL
                       AND NOT EXISTS (
                         SELECT 1 FROM mailbox_continuations AS continuation
                         WHERE continuation.work_id = mailbox_work.id
                           AND continuation.state IN ('pending_goal_binding', 'deferred')
                       )",
                )
                .bind(&work_ref.id)
                .execute(&mut *transaction)
                .await?
            }
            (RecoveryEvidence::RecipientIncompatible, _) => {
                sqlx::query(
                    "UPDATE mailbox_work SET dispatch_state = 'recovery_needed'
                     WHERE id = ? AND dispatch_state IN ('accepted', 'dispatching', 'dispatched')
                       AND outcome_json IS NULL",
                )
                .bind(&work_ref.id)
                .execute(&mut *transaction)
                .await?
            }
            (RecoveryEvidence::ContinuationUnowned { .. }, Some(binding_json)) => {
                sqlx::query(
                    "UPDATE mailbox_work SET dispatch_state = 'recovery_needed'
                     WHERE id = ? AND dispatch_state IN ('dispatching', 'dispatched')
                       AND outcome_json IS NULL
                       AND EXISTS (
                         SELECT 1 FROM mailbox_continuations AS continuation
                         WHERE continuation.work_id = mailbox_work.id
                           AND continuation.binding_json = ?
                           AND continuation.state IN ('pending_goal_binding', 'deferred')
                       )",
                )
                .bind(&work_ref.id)
                .bind(binding_json)
                .execute(&mut *transaction)
                .await?
            }
            (RecoveryEvidence::ContinuationUnowned { .. }, None) => {
                transaction.commit().await?;
                return Ok(false);
            }
        };
        let changed = update.rows_affected() == 1;
        transaction.commit().await?;
        if changed {
            self.notify_changed();
        }
        Ok(changed)
    }

    pub async fn mark_dispatched(&self, work_ref: &WorkRef) -> MailboxResult<()> {
        let result = sqlx::query(
            "UPDATE mailbox_work SET dispatch_state = 'dispatched'
             WHERE id = ? AND issuer = ? AND agent = ? AND submission_id = ?
               AND dispatch_state = 'dispatching'",
        )
        .bind(&work_ref.id)
        .bind(work_ref.issuer.to_string())
        .bind(work_ref.agent.to_string())
        .bind(&work_ref.submission_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            let row = exact_work(&self.pool, work_ref).await?;
            if parse_dispatch_state(row.try_get::<String, _>("dispatch_state")?.as_str())?
                != DispatchState::Dispatched
            {
                return Err(MailboxError::ReferenceMismatch);
            }
        }
        self.notify_changed();
        Ok(())
    }

    pub async fn dispatch_state(&self, work_ref: &WorkRef) -> MailboxResult<DispatchState> {
        let row = exact_work(&self.pool, work_ref).await?;
        parse_dispatch_state(row.try_get::<String, _>("dispatch_state")?.as_str())
    }

    /// Link exactly the native input IDs admitted to this recipient turn.
    pub async fn record_admission(
        &self,
        recipient: ThreadId,
        source_turn_id: &str,
        input_message_ids: &[String],
    ) -> MailboxResult<AdmissionReceipt> {
        validate_key(source_turn_id, "source turn id")?;
        if input_message_ids.len() > MAX_TURN_INPUTS {
            return Err(MailboxError::Backpressure("turn input count"));
        }
        let mut ids = input_message_ids.to_vec();
        ids.sort_unstable();
        if ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(MailboxError::InvalidInput(
                "duplicate input message ID".into(),
            ));
        }

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut work_refs = Vec::with_capacity(ids.len());
        let mut added = false;
        for input_id in &ids {
            let prior = sqlx::query(
                "SELECT source_turn_id, work_id FROM mailbox_input_admissions
                 WHERE recipient = ? AND input_message_id = ? LIMIT 1",
            )
            .bind(recipient.to_string())
            .bind(input_id)
            .fetch_optional(&mut *transaction)
            .await?;
            if let Some(prior) = prior {
                if prior.try_get::<String, _>("source_turn_id")? != source_turn_id {
                    return Err(MailboxError::IdempotencyConflict);
                }
                if let Some(work_id) = prior.try_get::<Option<String>, _>("work_id")? {
                    let work = sqlx::query("SELECT * FROM mailbox_work WHERE id = ?")
                        .bind(work_id)
                        .fetch_one(&mut *transaction)
                        .await?;
                    work_refs.push(work_ref_from_row(&work)?);
                }
                continue;
            }

            let native_work = sqlx::query(
                "SELECT * FROM mailbox_work WHERE agent = ? AND input_message_id = ? LIMIT 1",
            )
            .bind(recipient.to_string())
            .bind(input_id)
            .fetch_optional(&mut *transaction)
            .await?;
            if let Some(row) = native_work {
                let work_ref = work_ref_from_row(&row)?;
                sqlx::query(
                    "INSERT INTO mailbox_input_admissions(
                         recipient, source_turn_id, input_message_id, work_id
                     ) VALUES (?, ?, ?, ?)",
                )
                .bind(recipient.to_string())
                .bind(source_turn_id)
                .bind(input_id)
                .bind(&work_ref.id)
                .execute(&mut *transaction)
                .await?;
                sqlx::query(
                    "INSERT INTO mailbox_admissions(recipient, source_turn_id, work_id, input_message_id)
                     VALUES (?, ?, ?, ?)",
                )
                .bind(recipient.to_string())
                .bind(source_turn_id)
                .bind(&work_ref.id)
                .bind(input_id)
                .execute(&mut *transaction)
                .await?;
                // The provider established this exact native input. This is stronger
                // evidence than a stale dispatch-recovery marker, without replaying it.
                sqlx::query("UPDATE mailbox_work SET dispatch_state = 'dispatched' WHERE id = ?")
                    .bind(&work_ref.id)
                    .execute(&mut *transaction)
                    .await?;
                work_refs.push(work_ref);
                added = true;
                continue;
            }

            let message = sqlx::query(
                "SELECT canonical, input_admitted, legacy_attention, superseded_by, withdrawn,
                        CASE WHEN kind IN ('\"note\"', '\"progress\"')
                              AND (kind = '\"progress\"' OR legacy_attention = 0)
                             THEN 1 ELSE 0 END AS quiet
                 FROM mailbox_messages WHERE id = ? AND recipient = ? LIMIT 1",
            )
            .bind(input_id)
            .bind(recipient.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(MailboxError::ReferenceMismatch)?;
            if message.try_get::<i64, _>("input_admitted")? != 0 {
                return Err(MailboxError::IdempotencyConflict);
            }
            let canonical = message.try_get::<i64, _>("canonical")? != 0;
            let active = message
                .try_get::<Option<String>, _>("superseded_by")?
                .is_none()
                && message.try_get::<i64, _>("withdrawn")? == 0;
            let quiet = message.try_get::<i64, _>("quiet")? != 0;
            sqlx::query(
                "UPDATE mailbox_messages SET input_admitted = 1
                 WHERE id = ? AND recipient = ? AND input_admitted = 0",
            )
            .bind(input_id)
            .bind(recipient.to_string())
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO mailbox_input_admissions(
                     recipient, source_turn_id, input_message_id, work_id
                 ) VALUES (?, ?, ?, NULL)",
            )
            .bind(recipient.to_string())
            .bind(source_turn_id)
            .bind(input_id)
            .execute(&mut *transaction)
            .await?;
            if canonical && active {
                decrement_pending_count(&mut transaction, &recipient.to_string(), quiet).await?;
            }
            added = true;
        }
        transaction.commit().await?;
        if added {
            self.notify_changed();
        }
        Ok(AdmissionReceipt {
            work_refs,
            already_recorded: !added,
        })
    }
}

pub(crate) async fn exact_work<'c, E>(
    executor: E,
    work_ref: &WorkRef,
) -> MailboxResult<sqlx::sqlite::SqliteRow>
where
    E: sqlx::Executor<'c, Database = Sqlite>,
{
    sqlx::query(
        "SELECT * FROM mailbox_work WHERE id = ? AND issuer = ? AND agent = ? AND submission_id = ?",
    )
    .bind(&work_ref.id)
    .bind(work_ref.issuer.to_string())
    .bind(work_ref.agent.to_string())
    .bind(&work_ref.submission_id)
    .fetch_optional(executor)
    .await?
    .ok_or(MailboxError::ReferenceMismatch)
}

pub(crate) async fn exact_work_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    work_ref: &WorkRef,
) -> MailboxResult<sqlx::sqlite::SqliteRow> {
    exact_work(&mut **transaction, work_ref).await
}

pub(crate) fn work_ref_from_row(row: &sqlx::sqlite::SqliteRow) -> MailboxResult<WorkRef> {
    Ok(WorkRef {
        id: row.try_get("id")?,
        issuer: parse_thread_id(row.try_get::<String, _>("issuer")?.as_str())?,
        agent: parse_thread_id(row.try_get::<String, _>("agent")?.as_str())?,
        submission_id: row.try_get("submission_id")?,
    })
}

fn parse_thread_id(value: &str) -> MailboxResult<ThreadId> {
    ThreadId::from_string(value).map_err(|_| MailboxError::InvalidInput("stored thread ID".into()))
}

pub(crate) fn parse_dispatch_state(value: &str) -> MailboxResult<DispatchState> {
    match value {
        "accepted" => Ok(DispatchState::Accepted),
        "dispatching" => Ok(DispatchState::Dispatching),
        "dispatched" => Ok(DispatchState::Dispatched),
        "recovery_needed" => Ok(DispatchState::RecoveryNeeded),
        _ => Err(MailboxError::InvalidInput("stored dispatch state".into())),
    }
}

fn validate_key(value: &str, label: &'static str) -> MailboxResult<()> {
    if value.is_empty() || value.len() > MAX_KEY_BYTES {
        return Err(MailboxError::InvalidInput(format!("invalid {label}")));
    }
    Ok(())
}

pub(crate) fn new_mailbox_id() -> String {
    codex_protocol::ResponseItemId::with_suffix("amsg", uuid::Uuid::now_v7()).to_string()
}

fn is_mailbox_id(id: &codex_protocol::ResponseItemId) -> bool {
    id.as_str().starts_with("amsg_") && id.as_str().len() > 5
}

pub(crate) fn ensure_mailbox_id(id: &str) -> MailboxResult<()> {
    if id.starts_with("amsg_") && id.len() > 5 {
        Ok(())
    } else {
        Err(MailboxError::InvalidInput(
            "expected stable amsg_* ID".into(),
        ))
    }
}
