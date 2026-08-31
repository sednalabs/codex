use crate::runtime::StateRuntime;
use codex_protocol::ThreadId;
use codex_protocol::automatic_turn::AutomaticTurnProvenance;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use log::warn;
use uuid::Uuid;

const OUTCOME_STARTED: &str = "started";
const OUTCOME_RECOVERED: &str = "recovered";
const OUTCOME_REBLOCKED: &str = "policy_blocked_again";
const OUTCOME_EXHAUSTED: &str = "exhausted";
const OUTCOME_FAILED: &str = "failed";
const OUTCOME_ACCEPTED: &str = "accepted";
const OUTCOME_SUPPRESSED: &str = "suppressed";
const PROVENANCE_SOURCE: &str = "server_validated_client_user_message_id";
const EXPECTED_POLICY_RETRY_TEXT: &str = "continue";
const MAX_ATTEMPTS: i64 = 3;

#[derive(Clone, Copy, Debug)]
struct CompletedAutomaticTurn;

impl StateRuntime {
    /// Project automatic-turn metadata only after validating the untrusted client envelope against
    /// this runtime's actual thread and a preceding policy error recorded for that thread.
    ///
    /// The return value is the newly issued server capability when a policy event arms a retry.
    /// Callers that only need the projection may ignore it.
    pub async fn record_automatic_turn_event(
        &self,
        thread_id: ThreadId,
        event: &Event,
    ) -> Option<String> {
        if self.usage_ledger_pool().is_closed() {
            return None;
        }
        match self
            .record_automatic_turn_event_inner(thread_id, event)
            .await
        {
            Ok(capability) => capability,
            Err(err) => {
                warn!("automatic turn usage projection: {err}");
                None
            }
        }
    }

    /// Read the capability for a currently pending policy retry. This is intentionally a
    /// read-only accessor: the ticket is consumed only when the corresponding user message is
    /// projected by `insert_validated_automatic_turn`.
    pub async fn automatic_turn_capability(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
    ) -> Option<String> {
        if trigger_turn_id.is_empty() || self.usage_ledger_pool().is_closed() {
            return None;
        }
        sqlx::query_scalar(
            "SELECT capability FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND trigger_turn_id = ?",
        )
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
        .flatten()
    }

    /// Return the server-selected trigger identity and capability for the current policy event.
    /// Trigger identities may carry an occurrence suffix when a regular turn id is reused.
    pub async fn automatic_turn_capability_for_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> Option<(String, String)> {
        if turn_id.is_empty() || self.usage_ledger_pool().is_closed() {
            return None;
        }
        sqlx::query_as(
            "SELECT trigger_turn_id, capability FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND (trigger_turn_id = ? OR trigger_turn_id LIKE ?) LIMIT 1",
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .bind(format!("{turn_id}#%"))
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
        .flatten()
    }

    async fn record_automatic_turn_event_inner(
        &self,
        thread_id: ThreadId,
        event: &Event,
    ) -> anyhow::Result<Option<String>> {
        match &event.msg {
            EventMsg::ItemCompleted(item_event) => {
                if let TurnItem::UserMessage(item) = &item_event.item {
                    self.record_user_message(thread_id, &item_event.turn_id, item)
                        .await?;
                }
                Ok(None)
            }
            // This is the legacy projection of ItemCompleted. The canonical item event above is
            // preferred. Legacy events do not carry structured content, so an envelope in this
            // shape is intentionally not eligible for projection.
            EventMsg::UserMessage(_) => Ok(None),
            EventMsg::TurnComplete(turn_complete) => {
                let outcome = turn_complete
                    .error
                    .as_ref()
                    .map(Self::automatic_turn_error_outcome)
                    .unwrap_or(OUTCOME_RECOVERED);
                let completed = self
                    .complete_automatic_turn(thread_id, turn_complete.turn_id.as_str(), outcome)
                    .await?;
                if let Some(error) = turn_complete.error.as_ref()
                    && error
                        .codex_error_info
                        .as_ref()
                        .is_some_and(|info| matches!(info, &CodexErrorInfo::CyberPolicy))
                {
                    return self
                        .record_policy_trigger(thread_id, turn_complete.turn_id.as_str(), completed)
                        .await;
                }
                Ok(None)
            }
            EventMsg::Error(error) => {
                let outcome = Self::automatic_turn_error_outcome(error);
                let completed = self
                    .complete_automatic_turn(thread_id, event.id.as_str(), outcome)
                    .await?;
                if error
                    .codex_error_info
                    .as_ref()
                    .is_some_and(|info| matches!(info, &CodexErrorInfo::CyberPolicy))
                {
                    return self
                        .record_policy_trigger(thread_id, event.id.as_str(), completed)
                        .await;
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    async fn record_user_message(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        item: &codex_protocol::items::UserMessageItem,
    ) -> anyhow::Result<()> {
        let Some(client_id) = item.client_id.as_deref() else {
            return Ok(());
        };
        let Some(provenance) = AutomaticTurnProvenance::decode_client_user_message_id(client_id)
        else {
            return Ok(());
        };
        // A decoded envelope is still untrusted. Invalid envelopes are deliberately ignored and
        // never treated as manual provenance.
        let _ = self
            .insert_validated_automatic_turn(thread_id, turn_id, client_id, &provenance, item)
            .await?;
        Ok(())
    }

    async fn insert_validated_automatic_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        client_id: &str,
        provenance: &AutomaticTurnProvenance,
        item: &codex_protocol::items::UserMessageItem,
    ) -> anyhow::Result<bool> {
        if !is_expected_policy_retry_content(item) {
            return Ok(false);
        }
        let pool = self.usage_ledger_pool();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let actual_thread_id = thread_id.to_string();

        // Idempotent delivery of the exact same client id is a no-op. This check occurs before
        // consulting eligibility so a replay cannot consume or replace a later ticket.
        let already_projected: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM usage_automatic_turns WHERE thread_id = ? AND client_user_message_id = ?",
        )
        .bind(&actual_thread_id)
        .bind(client_id)
        .fetch_optional(&mut *tx)
        .await?;
        if already_projected.is_some() {
            tx.commit().await?;
            return Ok(true);
        }

        let Some((trigger_turn_id, generation, expected_attempt, max_attempts, capability)) =
            sqlx::query_as::<_, (String, i64, i64, i64, String)>(
                "SELECT trigger_turn_id, generation, next_attempt, max_attempts, capability FROM usage_automatic_turn_eligibility WHERE thread_id = ?",
            )
            .bind(&actual_thread_id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            return Ok(false);
        };

        // The parser only establishes that the envelope is well formed. These comparisons bind it
        // to server-owned state and prevent forged thread, trigger, attempt, max, capability, or
        // replay values from becoming telemetry.
        if provenance.thread_id != actual_thread_id
            || provenance.trigger_turn_id != trigger_turn_id
            || i64::from(provenance.attempt) != expected_attempt
            || i64::from(provenance.max_attempts) != max_attempts
            || provenance.capability != capability
        {
            return Ok(false);
        }

        sqlx::query(
            r#"
INSERT INTO usage_automatic_turns (
    thread_id,
    client_user_message_id,
    trigger_turn_id,
    turn_id,
    origin,
    generation,
    capability,
    attempt,
    max_attempts,
    provenance_source,
    outcome
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id, client_user_message_id) DO NOTHING
"#,
        )
        .bind(&actual_thread_id)
        .bind(client_id)
        .bind(&trigger_turn_id)
        .bind(turn_id)
        .bind(provenance.origin.as_str())
        .bind(generation)
        .bind(&capability)
        .bind(expected_attempt)
        .bind(max_attempts)
        .bind(PROVENANCE_SOURCE)
        .bind(OUTCOME_STARTED)
        .execute(&mut *tx)
        .await?;

        // Consume the eligibility ticket atomically. A replay after this point has no ticket and
        // therefore cannot mint a second attempt or steal a later chain generation.
        sqlx::query(
            "DELETE FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND trigger_turn_id = ? AND generation = ? AND next_attempt = ? AND capability = ?",
        )
        .bind(&actual_thread_id)
        .bind(&trigger_turn_id)
        .bind(generation)
        .bind(expected_attempt)
        .bind(&capability)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE usage_automatic_turn_triggers SET outcome = ? WHERE thread_id = ? AND trigger_turn_id = ? AND generation = ? AND attempt = ?",
        )
        .bind(OUTCOME_ACCEPTED)
        .bind(&actual_thread_id)
        .bind(&trigger_turn_id)
        .bind(generation)
        .bind(expected_attempt)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn record_policy_trigger(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
        completed: Option<CompletedAutomaticTurn>,
    ) -> anyhow::Result<Option<String>> {
        if trigger_turn_id.is_empty() {
            return Ok(None);
        }
        let pool = self.usage_ledger_pool();
        let actual_thread_id = thread_id.to_string();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

        sqlx::query(
            "INSERT INTO usage_automatic_turn_chains (thread_id) VALUES (?) ON CONFLICT(thread_id) DO NOTHING",
        )
        .bind(&actual_thread_id)
        .execute(&mut *tx)
        .await?;
        let generation: i64 = sqlx::query_scalar(
            "SELECT generation FROM usage_automatic_turn_chains WHERE thread_id = ?",
        )
        .bind(&actual_thread_id)
        .fetch_one(&mut *tx)
        .await?;

        // A pending ticket is authoritative evidence that a later/reordered event must not
        // replace the intended operation. It remains available until the intended envelope is
        // accepted or an explicit manual successful turn advances the generation.
        let pending = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM usage_automatic_turn_eligibility WHERE thread_id = ? LIMIT 1",
        )
        .bind(&actual_thread_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if pending && completed.is_none() {
            let _ = insert_suppressed_trigger(
                &mut tx,
                &actual_thread_id,
                trigger_turn_id,
                generation,
                /*attempt*/ 1,
            )
            .await?;
            tx.commit().await?;
            return Ok(None);
        }

        let previous = sqlx::query_as::<_, (i64, String, String, i64)>(
            "SELECT attempt, outcome, trigger_turn_id, generation FROM usage_automatic_turns WHERE thread_id = ? AND generation = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(&actual_thread_id)
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;
        let next_attempt = match previous.as_ref() {
            Some((attempt, outcome, _, _)) if outcome == OUTCOME_REBLOCKED => *attempt + 1,
            Some((_, outcome, _, _)) if outcome == OUTCOME_EXHAUSTED => MAX_ATTEMPTS + 1,
            _ => 1,
        };
        if previous
            .as_ref()
            .is_some_and(|(_, outcome, _, _)| outcome == OUTCOME_EXHAUSTED)
            && completed.is_none()
        {
            tx.commit().await?;
            return Ok(None);
        }

        // Reusing a regular turn id is valid across attempts. Once the automatic row for that
        // turn has just completed, issue an occurrence-qualified trigger identity; otherwise the
        // exact previously seen identity is treated as a duplicate/stale event.
        let raw_trigger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM usage_automatic_turn_triggers WHERE thread_id = ? AND trigger_turn_id = ?)",
        )
        .bind(&actual_thread_id)
        .bind(trigger_turn_id)
        .fetch_one(&mut *tx)
        .await?;
        if raw_trigger_exists && completed.is_none() {
            let _ = insert_suppressed_trigger(
                &mut tx,
                &actual_thread_id,
                trigger_turn_id,
                generation,
                next_attempt,
            )
            .await?;
            tx.commit().await?;
            return Ok(None);
        }
        let trigger_identity = if completed.is_some() && raw_trigger_exists {
            format!("{trigger_turn_id}#{next_attempt}")
        } else {
            trigger_turn_id.to_string()
        };

        // A trigger is seen before a ticket is issued. This closes the duplicate/stale path,
        // including events which arrive after a prior ticket was consumed.
        let trigger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM usage_automatic_turn_triggers WHERE thread_id = ? AND trigger_turn_id = ? AND generation = ? AND attempt = ?)",
        )
        .bind(&actual_thread_id)
        .bind(&trigger_identity)
        .bind(generation)
        .bind(next_attempt)
        .fetch_one(&mut *tx)
        .await?;
        if trigger_exists && completed.is_none() {
            tx.commit().await?;
            return Ok(None);
        }

        let capability = if next_attempt <= MAX_ATTEMPTS {
            let capability = Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO usage_automatic_turn_eligibility (thread_id, trigger_turn_id, generation, next_attempt, max_attempts, capability) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&actual_thread_id)
            .bind(&trigger_identity)
            .bind(generation)
            .bind(next_attempt)
            .bind(MAX_ATTEMPTS)
            .bind(&capability)
            .execute(&mut *tx)
            .await?;
            Some(capability)
        } else {
            None
        };

        let outcome = if capability.is_some() {
            "pending"
        } else {
            OUTCOME_EXHAUSTED
        };
        sqlx::query(
            "INSERT INTO usage_automatic_turn_triggers (thread_id, trigger_turn_id, generation, attempt, outcome) VALUES (?, ?, ?, ?, ?) ON CONFLICT(thread_id, trigger_turn_id, generation, attempt) DO UPDATE SET outcome = excluded.outcome",
        )
        .bind(&actual_thread_id)
        .bind(&trigger_identity)
        .bind(generation)
        .bind(next_attempt)
        .bind(outcome)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(capability)
    }

    fn automatic_turn_error_outcome(error: &ErrorEvent) -> &'static str {
        if error
            .codex_error_info
            .as_ref()
            .is_some_and(|info| matches!(info, &CodexErrorInfo::CyberPolicy))
        {
            OUTCOME_REBLOCKED
        } else {
            OUTCOME_FAILED
        }
    }

    async fn complete_automatic_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        outcome: &str,
    ) -> anyhow::Result<Option<CompletedAutomaticTurn>> {
        if turn_id.is_empty() {
            return Ok(None);
        }
        let pool = self.usage_ledger_pool();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some((id, attempt, max_attempts, generation, trigger_turn_id)) =
            sqlx::query_as::<_, (i64, i64, i64, i64, String)>(
                "SELECT id, attempt, max_attempts, generation, trigger_turn_id FROM usage_automatic_turns WHERE thread_id = ? AND turn_id = ? AND outcome = 'started' ORDER BY id DESC LIMIT 1",
            )
            .bind(thread_id.to_string())
            .bind(turn_id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            // A successful/non-automatic turn is the explicit reset boundary for a stale pending
            // ticket. Policy errors are handled by the caller after this no-op completion.
            if outcome != OUTCOME_REBLOCKED {
                let thread_id = thread_id.to_string();
                reset_chain_in_transaction(&mut tx, &thread_id).await?;
            }
            tx.commit().await?;
            return Ok(None);
        };
        let outcome = if outcome == OUTCOME_REBLOCKED && attempt >= max_attempts {
            OUTCOME_EXHAUSTED
        } else {
            outcome
        };
        sqlx::query(
            "UPDATE usage_automatic_turns SET outcome = ?, completed_at = COALESCE(completed_at, strftime('%Y-%m-%dT%H:%M:%fZ','now')) WHERE id = ? AND outcome = 'started'",
        )
        .bind(outcome)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE usage_automatic_turn_triggers SET outcome = ? WHERE thread_id = ? AND trigger_turn_id = ? AND generation = ? AND attempt = ?",
        )
        .bind(outcome)
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .bind(generation)
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
        if outcome == OUTCOME_RECOVERED || outcome == OUTCOME_FAILED {
            let thread_id = thread_id.to_string();
            reset_chain_in_transaction(&mut tx, &thread_id).await?;
        }
        tx.commit().await?;
        Ok(Some(CompletedAutomaticTurn))
    }
}

fn is_expected_policy_retry_content(item: &codex_protocol::items::UserMessageItem) -> bool {
    matches!(
        item.content.as_slice(),
        [UserInput::Text { text, text_elements }] if text == EXPECTED_POLICY_RETRY_TEXT && text_elements.is_empty()
    )
}

async fn insert_suppressed_trigger(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: &str,
    trigger_turn_id: &str,
    generation: i64,
    attempt: i64,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "INSERT INTO usage_automatic_turn_triggers (thread_id, trigger_turn_id, generation, attempt, outcome) VALUES (?, ?, ?, ?, ?) ON CONFLICT(thread_id, trigger_turn_id, generation, attempt) DO NOTHING",
    )
    .bind(thread_id)
    .bind(trigger_turn_id)
    .bind(generation)
    .bind(attempt)
    .bind(OUTCOME_SUPPRESSED)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn reset_chain_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO usage_automatic_turn_chains (thread_id) VALUES (?) ON CONFLICT(thread_id) DO NOTHING",
    )
    .bind(thread_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE usage_automatic_turn_chains SET generation = generation + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE thread_id = ?",
    )
    .bind(thread_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM usage_automatic_turn_eligibility WHERE thread_id = ?")
        .bind(thread_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use codex_protocol::items::UserMessageItem;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_utils_absolute_path::test_support::PathExt;
    use tempfile::TempDir;
    use tempfile::tempdir;

    #[derive(Debug, sqlx::FromRow)]
    struct AutomaticTurnRow {
        attempt: i64,
        max_attempts: i64,
        outcome: String,
    }

    async fn init_runtime() -> Result<(std::sync::Arc<StateRuntime>, TempDir)> {
        let tmp_dir = tempdir()?;
        let runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(tmp_dir.path().abs()),
            "test-provider".to_string(),
        )
        .await?;
        Ok((runtime, tmp_dir))
    }

    async fn capability(runtime: &StateRuntime, thread_id: ThreadId, trigger: &str) -> String {
        runtime
            .automatic_turn_capability(thread_id, trigger)
            .await
            .expect("policy trigger should issue capability")
    }

    fn automatic_user_message(
        thread_id: ThreadId,
        trigger_turn_id: &str,
        turn_id: &str,
        attempt: u8,
        max_attempts: u8,
        capability: &str,
    ) -> Event {
        let client_id = AutomaticTurnProvenance::policy_retry(
            thread_id,
            trigger_turn_id,
            attempt,
            max_attempts,
            capability,
        )
        .and_then(|provenance| provenance.to_client_user_message_id())
        .expect("valid automatic turn provenance");
        let mut item = UserMessageItem::new(&[UserInput::Text {
            text: EXPECTED_POLICY_RETRY_TEXT.to_string(),
            text_elements: Vec::new(),
        }]);
        item.client_id = Some(client_id);
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: turn_id.to_string(),
                item: TurnItem::UserMessage(item),
                completed_at_ms: 0,
            }),
        }
    }

    async fn rows(
        runtime: &StateRuntime,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> Result<Vec<AutomaticTurnRow>> {
        Ok(sqlx::query_as(
            "SELECT attempt, max_attempts, outcome FROM usage_automatic_turns WHERE thread_id = ? AND turn_id = ? ORDER BY id",
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .fetch_all(runtime.usage_ledger_pool().as_ref())
        .await?)
    }

    fn policy_error(turn_id: &str) -> Event {
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::Error(ErrorEvent {
                message: "blocked".to_string(),
                codex_error_info: Some(CodexErrorInfo::CyberPolicy),
            }),
        }
    }

    #[tokio::test]
    async fn trusted_projection_requires_capability_and_recovers() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let forged = automatic_user_message(
            thread_id, "trigger", "turn", /*attempt*/ 1, /*max_attempts*/ 3, "wrong",
        );
        runtime
            .record_automatic_turn_event(thread_id, &forged)
            .await;
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger"))
            .await;
        let cap = capability(runtime.as_ref(), thread_id, "trigger").await;
        runtime
            .record_automatic_turn_event(
                thread_id,
                &automatic_user_message(
                    thread_id, "trigger", "turn", /*attempt*/ 1, /*max_attempts*/ 3, &cap,
                ),
            )
            .await;
        let projected = rows(runtime.as_ref(), thread_id, "turn").await?;
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].attempt, 1);
        assert_eq!(projected[0].max_attempts, 3);
        assert_eq!(projected[0].outcome, OUTCOME_STARTED);
        runtime
            .record_automatic_turn_event(
                thread_id,
                &Event {
                    id: "turn".to_string(),
                    msg: EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: "turn".to_string(),
                        last_agent_message: Some("done".to_string()),
                        error: None,
                        started_at: None,
                        compaction_events_in_turn: 0,
                        final_model: None,
                        model_snapshot: None,
                        provider_usage: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    }),
                },
            )
            .await;
        assert_eq!(
            rows(runtime.as_ref(), thread_id, "turn").await?[0].outcome,
            OUTCOME_RECOVERED
        );
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_trigger_and_replay_are_ignored() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger"))
            .await;
        let cap = capability(runtime.as_ref(), thread_id, "trigger").await;
        let valid = automatic_user_message(
            thread_id, "trigger", "turn", /*attempt*/ 1, /*max_attempts*/ 3, &cap,
        );
        runtime.record_automatic_turn_event(thread_id, &valid).await;
        runtime.record_automatic_turn_event(thread_id, &valid).await;
        // A repeated policy event arrives after the ticket was consumed but before any new turn;
        // no second ticket is minted.
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger"))
            .await;
        assert!(
            sqlx::query_scalar::<_, i64>(
                "SELECT 1 FROM usage_automatic_turn_eligibility WHERE thread_id = ?",
            )
            .bind(thread_id.to_string())
            .fetch_optional(runtime.usage_ledger_pool().as_ref())
            .await?
            .is_none()
        );
        assert_eq!(rows(runtime.as_ref(), thread_id, "turn").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn successful_manual_turn_resets_interrupted_pending_chain() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger"))
            .await;
        let old_capability = capability(runtime.as_ref(), thread_id, "trigger").await;

        runtime
            .record_automatic_turn_event(
                thread_id,
                &Event {
                    id: "interrupted".to_string(),
                    msg: EventMsg::TurnAborted(codex_protocol::protocol::TurnAbortedEvent {
                        turn_id: Some("interrupted".to_string()),
                        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                        provider_usage: None,
                        started_at: None,
                        completed_at: None,
                        duration_ms: None,
                    }),
                },
            )
            .await;

        let manual_item = UserMessageItem::new(&[UserInput::Text {
            text: "manual".to_string(),
            text_elements: Vec::new(),
        }]);
        runtime
            .record_automatic_turn_event(
                thread_id,
                &Event {
                    id: "manual".to_string(),
                    msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                        thread_id,
                        turn_id: "manual".to_string(),
                        item: TurnItem::UserMessage(manual_item),
                        completed_at_ms: 0,
                    }),
                },
            )
            .await;
        runtime
            .record_automatic_turn_event(
                thread_id,
                &Event {
                    id: "manual".to_string(),
                    msg: EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: "manual".to_string(),
                        last_agent_message: Some("done".to_string()),
                        error: None,
                        started_at: None,
                        compaction_events_in_turn: 0,
                        final_model: None,
                        model_snapshot: None,
                        provider_usage: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    }),
                },
            )
            .await;

        assert!(
            runtime
                .automatic_turn_capability(thread_id, "trigger")
                .await
                .is_none()
        );
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger-new"))
            .await;
        let new_capability = capability(runtime.as_ref(), thread_id, "trigger-new").await;
        assert_ne!(old_capability, new_capability);
        Ok(())
    }

    #[tokio::test]
    async fn repeated_attempts_share_regular_turn_id_and_exhaust() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let turn_id = "same-turn";
        runtime
            .record_automatic_turn_event(thread_id, &policy_error(turn_id))
            .await;
        for index in 0..3 {
            let attempt = (index + 1) as u8;
            let (trigger, cap) = runtime
                .automatic_turn_capability_for_turn(thread_id, turn_id)
                .await
                .expect("current attempt should have a capability");
            runtime
                .record_automatic_turn_event(
                    thread_id,
                    &automatic_user_message(
                        thread_id, &trigger, turn_id, /*attempt*/ attempt,
                        /*max_attempts*/ 3, &cap,
                    ),
                )
                .await;
            runtime
                .record_automatic_turn_event(thread_id, &policy_error(turn_id))
                .await;
        }
        let projected = rows(runtime.as_ref(), thread_id, turn_id).await?;
        assert_eq!(projected.len(), 3);
        assert_eq!(
            projected.iter().map(|row| row.attempt).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(
            projected
                .iter()
                .map(|row| row.outcome.as_str())
                .collect::<Vec<_>>(),
            [OUTCOME_REBLOCKED, OUTCOME_REBLOCKED, OUTCOME_EXHAUSTED]
        );
        assert!(
            sqlx::query(
                "SELECT 1 FROM usage_automatic_turn_eligibility WHERE thread_id = ? LIMIT 1",
            )
            .bind(thread_id.to_string())
            .fetch_optional(runtime.usage_ledger_pool().as_ref())
            .await?
            .is_none()
        );
        Ok(())
    }
}
