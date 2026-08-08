use crate::runtime::StateRuntime;
use codex_protocol::automatic_turn::AutomaticTurnProvenance;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use log::warn;

const OUTCOME_STARTED: &str = "started";
const OUTCOME_RECOVERED: &str = "recovered";
const OUTCOME_REBLOCKED: &str = "cyber_policy_blocked_again";
const OUTCOME_EXHAUSTED: &str = "exhausted";
const OUTCOME_FAILED: &str = "failed";
const PROVENANCE_SOURCE_CLIENT_USER_MESSAGE_ID: &str = "client_user_message_id";

impl StateRuntime {
    /// Project model-opaque automatic-turn provenance from the canonical user-message client id
    /// into the local usage ledger. Unrecognized client ids are intentionally ignored.
    pub async fn record_automatic_turn_event(&self, event: &Event) {
        if self.usage_ledger_pool().is_closed() {
            return;
        }
        if let Err(err) = self.record_automatic_turn_event_inner(event).await {
            warn!("automatic turn usage projection: {err}");
        }
    }

    async fn record_automatic_turn_event_inner(&self, event: &Event) -> anyhow::Result<()> {
        match &event.msg {
            EventMsg::UserMessage(user_message) => {
                let Some(client_id) = user_message.client_id.as_deref() else {
                    return Ok(());
                };
                let Some(provenance) =
                    AutomaticTurnProvenance::from_client_user_message_id(client_id)
                else {
                    return Ok(());
                };
                self.insert_automatic_turn(event.id.as_str(), client_id, &provenance)
                    .await?;
            }
            EventMsg::TurnComplete(turn_complete) => {
                let outcome = turn_complete
                    .error
                    .as_ref()
                    .map(Self::automatic_turn_error_outcome)
                    .unwrap_or(OUTCOME_RECOVERED);
                self.complete_automatic_turn(turn_complete.turn_id.as_str(), outcome)
                    .await?;
            }
            EventMsg::Error(error) => {
                let outcome = Self::automatic_turn_error_outcome(error);
                self.complete_automatic_turn(event.id.as_str(), outcome)
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn insert_automatic_turn(
        &self,
        turn_id: &str,
        client_id: &str,
        provenance: &AutomaticTurnProvenance,
    ) -> anyhow::Result<()> {
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
ON CONFLICT(thread_id, client_user_message_id) DO UPDATE SET
    trigger_turn_id = excluded.trigger_turn_id,
    turn_id = excluded.turn_id,
    origin = excluded.origin,
    attempt = excluded.attempt,
    max_attempts = excluded.max_attempts,
    provenance_source = excluded.provenance_source
"#,
        )
        .bind(provenance.thread_id.as_str())
        .bind(client_id)
        .bind(provenance.trigger_turn_id.as_str())
        .bind(turn_id)
        .bind(provenance.origin.as_str())
        .bind(i64::from(provenance.attempt))
        .bind(i64::from(provenance.max_attempts))
        .bind(PROVENANCE_SOURCE_CLIENT_USER_MESSAGE_ID)
        .bind(OUTCOME_STARTED)
        .execute(self.usage_ledger_pool().as_ref())
        .await?;
        Ok(())
    }

    fn automatic_turn_error_outcome(error: &ErrorEvent) -> &'static str {
        if matches!(error.codex_error_info, Some(CodexErrorInfo::CyberPolicy)) {
            return OUTCOME_REBLOCKED;
        }
        OUTCOME_FAILED
    }

    async fn complete_automatic_turn(&self, turn_id: &str, outcome: &str) -> anyhow::Result<()> {
        if turn_id.is_empty() {
            return Ok(());
        }
        let pool = self.usage_ledger_pool();
        let Some((attempt, max_attempts)) = sqlx::query_as::<_, (i64, i64)>(
            "SELECT attempt, max_attempts FROM usage_automatic_turns WHERE turn_id = ?",
        )
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
            r#"
UPDATE usage_automatic_turns
SET outcome = ?,
    completed_at = COALESCE(completed_at, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
WHERE turn_id = ?
  AND outcome = 'started'
"#,
        )
        .bind(outcome)
        .bind(turn_id)
        .execute(pool.as_ref())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use codex_protocol::ThreadId;
    use codex_protocol::automatic_turn::AutomaticTurnProvenance;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::UserMessageEvent;
    use codex_utils_absolute_path::test_support::PathExt;
    use tempfile::TempDir;
    use tempfile::tempdir;

    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
    struct AutomaticTurnRow {
        thread_id: String,
        trigger_turn_id: String,
        turn_id: String,
        origin: String,
        attempt: i64,
        max_attempts: i64,
        provenance_source: String,
        outcome: String,
        completed_at: Option<String>,
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
        let client_id = AutomaticTurnProvenance::cyber_policy_auto_continue(
            thread_id,
            trigger_turn_id,
            attempt,
            max_attempts,
        )
        .and_then(|provenance| provenance.to_client_user_message_id())
        .expect("valid automatic turn provenance");
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::UserMessage(UserMessageEvent {
                client_id: Some(client_id),
                message: "continue".to_string(),
                ..Default::default()
            }),
        }
    }

    async fn row(runtime: &StateRuntime, turn_id: &str) -> Result<AutomaticTurnRow> {
        Ok(sqlx::query_as(
            r#"
SELECT thread_id, trigger_turn_id, turn_id, origin, attempt, max_attempts,
       provenance_source, outcome, completed_at
FROM usage_automatic_turns
WHERE turn_id = ?
"#,
        )
        .bind(turn_id)
        .fetch_one(runtime.usage_ledger_pool().as_ref())
        .await?)
    }

    #[tokio::test]
    async fn automatic_turn_projection_records_exact_provenance_and_recovery() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        runtime
            .record_automatic_turn_event(&automatic_user_message(
                thread_id,
                "turn-trigger",
                "turn-auto-1",
                1,
                3,
            ))
            .await;

        let started = row(runtime.as_ref(), "turn-auto-1").await?;
        assert_eq!(started.thread_id, thread_id.to_string());
        assert_eq!(started.trigger_turn_id, "turn-trigger");
        assert_eq!(started.origin, "cyber_policy_auto_continue");
        assert_eq!(started.attempt, 1);
        assert_eq!(started.max_attempts, 3);
        assert_eq!(started.provenance_source, "client_user_message_id");
        assert_eq!(started.outcome, "started");
        assert!(started.completed_at.is_none());

        runtime
            .complete_automatic_turn("turn-auto-1", OUTCOME_RECOVERED)
            .await?;

        let recovered = row(runtime.as_ref(), "turn-auto-1").await?;
        assert_eq!(recovered.outcome, "recovered");
        assert!(recovered.completed_at.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn automatic_turn_projection_marks_reblock_and_exhaustion() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        for (turn_id, attempt, expected) in [
            ("turn-auto-2", 2, "cyber_policy_blocked_again"),
            ("turn-auto-3", 3, "exhausted"),
        ] {
            runtime
                .record_automatic_turn_event(&automatic_user_message(
                    thread_id,
                    "turn-trigger",
                    turn_id,
                    attempt,
                    3,
                ))
                .await;
            runtime
                .record_automatic_turn_event(&Event {
                    id: turn_id.to_string(),
                    msg: EventMsg::Error(ErrorEvent {
                        message: "blocked".to_string(),
                        codex_error_info: Some(CodexErrorInfo::CyberPolicy),
                    }),
                })
                .await;
            assert_eq!(row(runtime.as_ref(), turn_id).await?.outcome, expected);
        }
        Ok(())
    }

    #[tokio::test]
    async fn automatic_turn_projection_keeps_first_terminal_outcome() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let turn_id = "turn-auto-first-terminal";
        runtime
            .record_automatic_turn_event(&automatic_user_message(
                thread_id,
                "turn-trigger",
                turn_id,
                2,
                3,
            ))
            .await;
        runtime
            .complete_automatic_turn(turn_id, OUTCOME_REBLOCKED)
            .await?;
        let first = row(runtime.as_ref(), turn_id).await?;
        let first_completed_at = first
            .completed_at
            .clone()
            .expect("first terminal event should set completed_at");
        assert_eq!(first.outcome, OUTCOME_REBLOCKED);

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        runtime
            .complete_automatic_turn(turn_id, OUTCOME_RECOVERED)
            .await?;

        let duplicate = row(runtime.as_ref(), turn_id).await?;
        assert_eq!(duplicate.outcome, OUTCOME_REBLOCKED);
        assert_eq!(
            duplicate.completed_at.as_deref(),
            Some(first_completed_at.as_str())
        );
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_user_message_is_not_projected_as_automatic() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        runtime
            .record_automatic_turn_event(&Event {
                id: "turn-human".to_string(),
                msg: EventMsg::UserMessage(UserMessageEvent {
                    client_id: None,
                    message: "continue".to_string(),
                    ..Default::default()
                }),
            })
            .await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_automatic_turns")
            .fetch_one(runtime.usage_ledger_pool().as_ref())
            .await?;
        assert_eq!(count, 0);
        Ok(())
    }
}
