use crate::StateRuntime;
use crate::sqlite::SqliteWriteQueryExt;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TokenUsageRecord;
use sqlx::SqlitePool;
use std::sync::Arc;

/// Whether token usage was reported by a completed or incomplete response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageCallStatus {
    Completed,
    Incomplete,
}

impl UsageCallStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "ok",
            Self::Incomplete => "incomplete",
        }
    }
}

/// Projects upstream durable per-response token records into the existing local usage ledger.
/// The rollout record remains authoritative if the optional database write is unavailable.
pub struct UsageLogger {
    pool: Arc<SqlitePool>,
    thread_id: ThreadId,
}

impl UsageLogger {
    pub async fn try_new(
        state: &Arc<StateRuntime>,
        thread_id: ThreadId,
        source: &SessionSource,
        thread_source: Option<&ThreadSource>,
        parent_thread_id: Option<ThreadId>,
        fork_parent_thread_id: Option<ThreadId>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(pool) = state.usage_ledger_pool() else {
            return Ok(None);
        };
        let parent_id = parent_thread_id
            .as_ref()
            .or(fork_parent_thread_id.as_ref())
            .map(ToString::to_string);
        let inherited_root = if let Some(parent_id) = parent_id.as_deref() {
            sqlx::query_scalar::<_, String>(
                "SELECT root_thread_id FROM usage_threads WHERE thread_id = ?",
            )
            .bind(parent_id)
            .fetch_optional(pool.as_ref())
            .await?
        } else {
            None
        };
        let root_thread_id = inherited_root
            .or_else(|| parent_id.clone())
            .unwrap_or_else(|| thread_id.to_string());
        let created_at = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
INSERT INTO usage_threads (
    thread_id, parent_thread_id, root_thread_id, fork_parent_thread_id,
    source, thread_source, created_at
)
VALUES (?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id) DO UPDATE SET
    parent_thread_id = COALESCE(excluded.parent_thread_id, usage_threads.parent_thread_id),
    root_thread_id = COALESCE(excluded.root_thread_id, usage_threads.root_thread_id),
    fork_parent_thread_id = COALESCE(excluded.fork_parent_thread_id, usage_threads.fork_parent_thread_id),
    source = excluded.source,
    thread_source = COALESCE(excluded.thread_source, usage_threads.thread_source)
            "#,
        )
        .bind(thread_id.to_string())
        .bind(parent_thread_id.as_ref().map(ToString::to_string))
        .bind(root_thread_id)
        .bind(fork_parent_thread_id.as_ref().map(ToString::to_string))
        .bind(source.to_string())
        .bind(thread_source.map(ThreadSource::as_str))
        .bind(created_at)
        .execute_with_busy_retry(pool.as_ref())
        .await?;
        Ok(Some(Self { pool, thread_id }))
    }

    /// Store one response's usage, never its cumulative turn/thread snapshots.
    /// `provider` and actual model/debit remain absent unless the upstream record reports them.
    pub async fn record_response(
        &self,
        record: &TokenUsageRecord,
        status: UsageCallStatus,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            record.thread_id == self.thread_id,
            "usage response thread does not match its session"
        );
        if record.response_id.trim().is_empty() {
            anyhow::bail!("usage response has no response id");
        }
        let response_id = format!("{}:{}", self.thread_id, record.response_id);
        // The rollout record is written after response completion and has no provider start time.
        // Keep the timestamp useful for local windows without implying a measured call duration.
        let observed_at = Utc::now().to_rfc3339();
        let usage = &record.usage;
        let uncached_input_tokens = usage
            .input_tokens
            .saturating_sub(usage.cached_input_tokens)
            .max(0);
        let requested_model = record
            .requested_model
            .as_deref()
            .map(str::to_ascii_lowercase);
        let actual_model_used = record
            .actual_model_used
            .as_deref()
            .map(str::to_ascii_lowercase);
        sqlx::query(
            r#"
INSERT INTO usage_provider_calls (
    provider_call_id, thread_id, turn_id, request_id, provider, requested_model,
    actual_model_used, started_at, completed_at, input_tokens_uncached,
    input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status
)
VALUES (?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(provider_call_id) DO UPDATE SET
    turn_id = excluded.turn_id,
    request_id = excluded.request_id,
    requested_model = COALESCE(excluded.requested_model, usage_provider_calls.requested_model),
    actual_model_used = COALESCE(excluded.actual_model_used, usage_provider_calls.actual_model_used),
    completed_at = excluded.completed_at,
    input_tokens_uncached = excluded.input_tokens_uncached,
    input_tokens_cached = excluded.input_tokens_cached,
    input_tokens_cache_write = excluded.input_tokens_cache_write,
    output_tokens = excluded.output_tokens,
    total_tokens = excluded.total_tokens,
    status = excluded.status
            "#,
        )
        .bind(response_id)
        .bind(self.thread_id.to_string())
        .bind(&record.turn_id)
        .bind(&record.response_id)
        .bind(requested_model)
        .bind(actual_model_used)
        .bind(&observed_at)
        .bind(&observed_at)
        .bind(uncached_input_tokens)
        .bind(usage.cached_input_tokens)
        .bind(usage.cache_write_input_tokens)
        .bind(usage.output_tokens)
        .bind(usage.total_tokens)
        .bind(status.as_str())
        .execute_with_busy_retry(self.pool.as_ref())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteConfig;
    use crate::runtime::test_support::unique_temp_dir;
    use codex_protocol::SessionId;
    use codex_protocol::protocol::SubAgentSource;
    use codex_protocol::protocol::TokenUsage;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn response_projection_is_idempotent_and_keeps_requested_unknown_actual_and_tokens() {
        let sqlite_home = unique_temp_dir();
        let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
            let _ = std::fs::remove_dir_all(sqlite_home);
        });
        tokio::fs::create_dir_all(sqlite_home.as_path())
            .await
            .expect("create isolated sqlite home");
        let state = StateRuntime::init(
            SqliteConfig::new_for_testing(sqlite_home.as_path().abs()),
            "openai".to_string(),
        )
        .await
        .expect("initialize state runtime and existing usage ledger schema");
        let root_thread_id = ThreadId::new();
        let child_thread_id = ThreadId::new();
        let root_logger = UsageLogger::try_new(
            &state,
            root_thread_id,
            &SessionSource::Cli,
            Some(&ThreadSource::User),
            None,
            None,
        )
        .await
        .expect("create root usage logger")
        .expect("usage ledger should be available");
        let root_usage = TokenUsage {
            input_tokens: 6,
            cached_input_tokens: 1,
            output_tokens: 2,
            total_tokens: 8,
            ..TokenUsage::default()
        };
        let root_record = TokenUsageRecord {
            thread_id: root_thread_id,
            turn_id: "root-turn".to_string(),
            session_id: SessionId::from(root_thread_id),
            root_turn_id: "root-turn".to_string(),
            response_id: "root-response".to_string(),
            requested_model: Some("gpt-5.6-sol".to_string()),
            actual_model_used: None,
            usage: root_usage.clone(),
            turn_token_usage: root_usage.clone(),
            thread_token_usage: root_usage,
        };
        root_logger
            .record_response(&root_record, UsageCallStatus::Completed)
            .await
            .expect("record root response usage");
        let child_logger = UsageLogger::try_new(
            &state,
            child_thread_id,
            &SessionSource::SubAgent(SubAgentSource::Other("test".to_string())),
            Some(&ThreadSource::Subagent),
            Some(root_thread_id),
            None,
        )
        .await
        .expect("create subagent usage logger")
        .expect("usage ledger should be available");
        let usage = TokenUsage {
            input_tokens: 12,
            cached_input_tokens: 4,
            cache_write_input_tokens: 2,
            output_tokens: 3,
            total_tokens: 15,
            ..TokenUsage::default()
        };
        let child_record = TokenUsageRecord {
            thread_id: child_thread_id,
            turn_id: "child-turn".to_string(),
            session_id: SessionId::from(child_thread_id),
            root_turn_id: "root-turn".to_string(),
            response_id: "child-response".to_string(),
            requested_model: Some("gpt-5.3-codex-spark".to_string()),
            actual_model_used: None,
            usage: usage.clone(),
            turn_token_usage: usage.clone(),
            thread_token_usage: usage,
        };
        let duplicate_child_logger = UsageLogger::try_new(
            &state,
            child_thread_id,
            &SessionSource::SubAgent(SubAgentSource::Other("test".to_string())),
            Some(&ThreadSource::Subagent),
            Some(root_thread_id),
            None,
        )
        .await
        .expect("create second writer for the subagent thread")
        .expect("usage ledger should be available");
        let (first_write, duplicate_write) = tokio::join!(
            child_logger.record_response(&child_record, UsageCallStatus::Incomplete),
            duplicate_child_logger.record_response(&child_record, UsageCallStatus::Incomplete),
        );
        first_write.expect("record incomplete response usage");
        duplicate_write.expect("concurrent duplicate response should be idempotent");
        let pool = state
            .usage_ledger_pool()
            .expect("usage pool remains available");
        let row = sqlx::query(
            r#"SELECT thread_id, turn_id, requested_model, actual_model_used,
                      input_tokens_uncached, input_tokens_cached, input_tokens_cache_write,
                      output_tokens, total_tokens, provider_reported_credits, request_id, status
               FROM usage_provider_calls"#,
        )
        .fetch_one(pool.as_ref())
        .await
        .expect("projected call row");
        assert_eq!(
            (
                row.get::<String, _>("thread_id"),
                row.get::<String, _>("turn_id"),
                row.get::<String, _>("requested_model"),
                row.get::<Option<String>, _>("actual_model_used"),
                row.get::<i64, _>("input_tokens_uncached"),
                row.get::<i64, _>("input_tokens_cached"),
                row.get::<i64, _>("input_tokens_cache_write"),
                row.get::<i64, _>("output_tokens"),
                row.get::<i64, _>("total_tokens"),
                row.get::<Option<f64>, _>("provider_reported_credits"),
                row.get::<String, _>("request_id"),
                row.get::<String, _>("status"),
            ),
            (
                child_thread_id.to_string(),
                "child-turn".to_string(),
                "gpt-5.3-codex-spark".to_string(),
                None,
                8,
                4,
                2,
                3,
                15,
                None,
                "child-response".to_string(),
                "incomplete".to_string(),
            )
        );
        let lineage = sqlx::query(
            "SELECT parent_thread_id, root_thread_id FROM usage_threads WHERE thread_id = ?",
        )
        .bind(child_thread_id.to_string())
        .fetch_one(pool.as_ref())
        .await
        .expect("subagent lineage row");
        assert_eq!(
            (
                lineage.get::<Option<String>, _>("parent_thread_id"),
                lineage.get::<Option<String>, _>("root_thread_id"),
            ),
            (
                Some(root_thread_id.to_string()),
                Some(root_thread_id.to_string()),
            )
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_provider_calls")
                .fetch_one(pool.as_ref())
                .await
                .expect("provider call count"),
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT requested_model FROM usage_provider_calls WHERE thread_id = ?",
            )
            .bind(root_thread_id.to_string())
            .fetch_one(pool.as_ref())
            .await
            .expect("root model attribution"),
            "gpt-5.6-sol"
        );
        state.close().await;
    }

    #[tokio::test]
    async fn unavailable_usage_database_is_not_rebuilt_or_required_for_state_runtime() {
        let sqlite_home = unique_temp_dir();
        let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
            let _ = std::fs::remove_dir_all(sqlite_home);
        });
        tokio::fs::create_dir_all(sqlite_home.as_path())
            .await
            .expect("create isolated sqlite home");
        let sqlite = SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
        let usage_path = sqlite.usage_db_path();
        let sentinel = b"preserve an unreadable existing usage database";
        tokio::fs::write(&usage_path, sentinel)
            .await
            .expect("seed an unreadable usage database");

        let state = StateRuntime::init(sqlite, "openai".to_string())
            .await
            .expect("an unavailable optional ledger must not prevent runtime startup");

        assert!(state.usage_ledger_pool().is_none());
        assert_eq!(
            tokio::fs::read(usage_path)
                .await
                .expect("existing usage file should remain present"),
            sentinel
        );
        state.close().await;
    }
}
