use crate::runtime::StateRuntime;
use codex_protocol::ThreadId;
use codex_protocol::automatic_turn::AutomaticTurnProvenance;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use log::warn;

const OUTCOME_STARTED: &str = "started";
const OUTCOME_RECOVERED: &str = "recovered";
const OUTCOME_REBLOCKED: &str = "policy_blocked_again";
const OUTCOME_EXHAUSTED: &str = "exhausted";
const OUTCOME_FAILED: &str = "failed";
const PROVENANCE_SOURCE: &str = "server_validated_client_user_message_id";
const MAX_ATTEMPTS: i64 = 3;

impl StateRuntime {
    /// Project automatic-turn metadata only after validating the untrusted client envelope against
    /// this runtime's actual thread and a preceding policy error recorded for that thread.
    pub async fn record_automatic_turn_event(&self, thread_id: ThreadId, event: &Event) {
        if self.usage_ledger_pool().is_closed() {
            return;
        }
        if let Err(err) = self
            .record_automatic_turn_event_inner(thread_id, event)
            .await
        {
            warn!("automatic turn usage projection: {err}");
        }
    }

    async fn record_automatic_turn_event_inner(
        &self,
        thread_id: ThreadId,
        event: &Event,
    ) -> anyhow::Result<()> {
        match &event.msg {
            EventMsg::ItemCompleted(item_event) => {
                if let TurnItem::UserMessage(item) = &item_event.item {
                    self.record_user_message(
                        thread_id,
                        &item_event.turn_id,
                        item.client_id.as_deref(),
                    )
                    .await?;
                }
            }
            // This is the legacy projection of ItemCompleted. The canonical item event above is
            // preferred, but accepting the legacy shape keeps old event producers compatible.
            EventMsg::UserMessage(user_message) => {
                self.record_user_message(thread_id, &event.id, user_message.client_id.as_deref())
                    .await?;
            }
            EventMsg::TurnComplete(turn_complete) => {
                let outcome = turn_complete
                    .error
                    .as_ref()
                    .map(Self::automatic_turn_error_outcome)
                    .unwrap_or(OUTCOME_RECOVERED);
                self.complete_automatic_turn(thread_id, turn_complete.turn_id.as_str(), outcome)
                    .await?;
                if let Some(error) = turn_complete.error.as_ref()
                    && matches!(error.codex_error_info, Some(CodexErrorInfo::CyberPolicy))
                {
                    self.record_policy_trigger(thread_id, turn_complete.turn_id.as_str())
                        .await?;
                }
            }
            EventMsg::Error(error) => {
                let outcome = Self::automatic_turn_error_outcome(error);
                self.complete_automatic_turn(thread_id, event.id.as_str(), outcome)
                    .await?;
                if matches!(error.codex_error_info, Some(CodexErrorInfo::CyberPolicy)) {
                    self.record_policy_trigger(thread_id, event.id.as_str())
                        .await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn record_user_message(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        client_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let Some(client_id) = client_id else {
            return Ok(());
        };
        let Some(provenance) = AutomaticTurnProvenance::decode_client_user_message_id(client_id)
        else {
            return Ok(());
        };
        self.insert_validated_automatic_turn(thread_id, turn_id, client_id, &provenance)
            .await
    }

    async fn insert_validated_automatic_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        client_id: &str,
        provenance: &AutomaticTurnProvenance,
    ) -> anyhow::Result<()> {
        let pool = self.usage_ledger_pool();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let actual_thread_id = thread_id.to_string();
        let Some((trigger_turn_id, expected_attempt, max_attempts)) = sqlx::query_as::<
            _,
            (String, i64, i64),
        >(
            "SELECT trigger_turn_id, next_attempt, max_attempts FROM usage_automatic_turn_eligibility WHERE thread_id = ?",
        )
        .bind(&actual_thread_id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(());
        };

        // The parser only establishes that the envelope is well formed. These comparisons bind it
        // to server-owned state and prevent forged thread, trigger, attempt, or replay values from
        // becoming telemetry.
        if provenance.thread_id != actual_thread_id
            || provenance.trigger_turn_id != trigger_turn_id
            || i64::from(provenance.attempt) != expected_attempt
            || i64::from(provenance.max_attempts) != max_attempts
        {
            return Ok(());
        }

        sqlx::query(
            r#"
INSERT INTO usage_automatic_turns (
    thread_id,
    client_user_message_id,
    trigger_turn_id,
    turn_id,
    origin,
    attempt,
    max_attempts,
    provenance_source,
    outcome
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id, client_user_message_id) DO NOTHING
"#,
        )
        .bind(&actual_thread_id)
        .bind(client_id)
        .bind(&trigger_turn_id)
        .bind(turn_id)
        .bind(provenance.origin.as_str())
        .bind(expected_attempt)
        .bind(max_attempts)
        .bind(PROVENANCE_SOURCE)
        .bind(OUTCOME_STARTED)
        .execute(&mut *tx)
        .await?;

        // Consume the eligibility ticket regardless of whether the client envelope was a duplicate
        // delivery. This makes replay a no-op and leaves exactly one row per accepted attempt.
        sqlx::query(
            "DELETE FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND trigger_turn_id = ? AND next_attempt = ?",
        )
        .bind(&actual_thread_id)
        .bind(&trigger_turn_id)
        .bind(expected_attempt)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn record_policy_trigger(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
    ) -> anyhow::Result<()> {
        if trigger_turn_id.is_empty() {
            return Ok(());
        }
        let pool = self.usage_ledger_pool();
        let actual_thread_id = thread_id.to_string();
        let previous = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT attempt, outcome, trigger_turn_id FROM usage_automatic_turns WHERE thread_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(&actual_thread_id)
        .fetch_optional(pool.as_ref())
        .await?;
        let next_attempt = match previous {
            Some((attempt, outcome, _)) if outcome == OUTCOME_REBLOCKED => attempt + 1,
            Some((_, outcome, previous_trigger))
                if outcome == OUTCOME_EXHAUSTED && previous_trigger == trigger_turn_id =>
            {
                return Ok(());
            }
            _ => 1,
        };
        if next_attempt > MAX_ATTEMPTS {
            return Ok(());
        }

        // A duplicate policy event for the same turn must not mint another attempt. A new trigger
        // replaces a stale ticket only after the prior ticket has been consumed.
        sqlx::query(
            r#"
INSERT INTO usage_automatic_turn_eligibility (
    thread_id, trigger_turn_id, next_attempt, max_attempts
) VALUES (?, ?, ?, ?)
ON CONFLICT(thread_id) DO UPDATE SET
    trigger_turn_id = excluded.trigger_turn_id,
    next_attempt = excluded.next_attempt,
    max_attempts = excluded.max_attempts
WHERE usage_automatic_turn_eligibility.trigger_turn_id != excluded.trigger_turn_id
"#,
        )
        .bind(&actual_thread_id)
        .bind(trigger_turn_id)
        .bind(next_attempt)
        .bind(MAX_ATTEMPTS)
        .execute(pool.as_ref())
        .await?;
        Ok(())
    }

    fn automatic_turn_error_outcome(error: &ErrorEvent) -> &'static str {
        if matches!(error.codex_error_info, Some(CodexErrorInfo::CyberPolicy)) {
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
    ) -> anyhow::Result<()> {
        if turn_id.is_empty() {
            return Ok(());
        }
        let pool = self.usage_ledger_pool();
        let Some((id, attempt, max_attempts)) = sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT id, attempt, max_attempts FROM usage_automatic_turns WHERE thread_id = ? AND turn_id = ? AND outcome = 'started' ORDER BY id DESC LIMIT 1",
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .fetch_optional(pool.as_ref())
        .await?
        else {
            return Ok(());
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
        .execute(pool.as_ref())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use codex_protocol::items::UserMessageItem;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_protocol::user_input::UserInput;
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

    fn automatic_user_message(
        thread_id: ThreadId,
        trigger_turn_id: &str,
        turn_id: &str,
        attempt: u8,
        max_attempts: u8,
    ) -> Event {
        let client_id = AutomaticTurnProvenance::policy_retry(
            thread_id,
            trigger_turn_id,
            attempt,
            max_attempts,
        )
        .and_then(|provenance| provenance.to_client_user_message_id())
        .expect("valid automatic turn provenance");
        let mut item = UserMessageItem::new(&[UserInput::Text {
            text: "continue".to_string(),
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
    async fn trusted_projection_requires_preceding_policy_event_and_recovers() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let valid = automatic_user_message(thread_id, "trigger", "turn", 1, 3);

        runtime.record_automatic_turn_event(thread_id, &valid).await;
        assert!(rows(runtime.as_ref(), thread_id, "turn").await?.is_empty());

        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger"))
            .await;
        runtime.record_automatic_turn_event(thread_id, &valid).await;
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
    async fn forged_thread_trigger_attempt_prefix_and_replay_are_ignored() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger"))
            .await;

        let forged = [
            AutomaticTurnProvenance::policy_retry(ThreadId::new(), "trigger", 1, 3)
                .expect("well-formed forged thread"),
            AutomaticTurnProvenance::policy_retry(thread_id, "forged-trigger", 1, 3)
                .expect("well-formed forged trigger"),
            AutomaticTurnProvenance::policy_retry(thread_id, "trigger", 2, 3)
                .expect("well-formed forged attempt"),
        ];
        for provenance in forged {
            let mut event = automatic_user_message(
                thread_id,
                &provenance.trigger_turn_id,
                "forged-turn",
                provenance.attempt,
                provenance.max_attempts,
            );
            if let EventMsg::ItemCompleted(item_event) = &mut event.msg
                && let TurnItem::UserMessage(item) = &mut item_event.item
            {
                item.client_id = provenance.to_client_user_message_id();
            }
            runtime.record_automatic_turn_event(thread_id, &event).await;
        }
        let bad_prefix = Event {
            id: "bad-prefix".to_string(),
            msg: EventMsg::UserMessage(codex_protocol::protocol::UserMessageEvent {
                client_id: Some("not-an-automatic-turn-envelope".to_string()),
                message: "continue".to_string(),
                ..Default::default()
            }),
        };
        runtime
            .record_automatic_turn_event(thread_id, &bad_prefix)
            .await;

        let valid = automatic_user_message(thread_id, "trigger", "turn", 1, 3);
        runtime.record_automatic_turn_event(thread_id, &valid).await;
        runtime.record_automatic_turn_event(thread_id, &valid).await;
        assert_eq!(rows(runtime.as_ref(), thread_id, "turn").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn repeated_attempts_can_share_turn_id_and_exhaust() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let turn_id = "same-turn";
        for (index, trigger) in ["same-turn", "same-turn", "same-turn"]
            .into_iter()
            .enumerate()
        {
            let attempt = (index + 1) as u8;
            runtime
                .record_automatic_turn_event(thread_id, &policy_error(trigger))
                .await;
            runtime
                .record_automatic_turn_event(
                    thread_id,
                    &automatic_user_message(thread_id, trigger, turn_id, attempt, 3),
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
