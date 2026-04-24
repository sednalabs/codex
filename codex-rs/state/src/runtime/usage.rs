use crate::StateRuntime;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::CollabAgentSpawnBeginEvent;
use codex_protocol::protocol::CollabAgentSpawnEndEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::McpToolCallBeginEvent;
use codex_protocol::protocol::McpToolCallEndEvent;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenCountEvent;
use log::warn;
use sqlx::Row;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug)]
struct TurnSnapshot {
    requested_model: Option<String>,
    _requested_provider: Option<String>,
}

#[derive(Clone, Debug)]
struct SpawnRequestState {
    parent_thread_id: ThreadId,
    _requested_model: String,
    _requested_reasoning_effort: String,
}

#[derive(Clone, Debug)]
struct ToolCallState {
    _tool_name: String,
    _server_name: Option<String>,
    _started_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct TokenUsageTotals {
    uncached_input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
}

/// Tracks usage for one thread plus the lineage anchors that tie it back to the
/// downstream usage ledger.
///
/// `parent_thread_id` is the direct session-source parent, `root_thread_id` is the
/// canonical persisted lineage root, and `fork_parent_thread_id` preserves explicit
/// fork ancestry.
pub struct UsageLogger {
    pool: Arc<SqlitePool>,
    thread_id: ThreadId,
    _session_source: SessionSource,
    _parent_thread_id: Option<ThreadId>,
    _root_thread_id: String,
    _fork_parent_thread_id: Option<ThreadId>,
    turn_snapshots: HashMap<String, TurnSnapshot>,
    spawn_requests: HashMap<String, SpawnRequestState>,
    tool_calls: HashMap<String, ToolCallState>,
    last_provider_call_id: Option<String>,
    last_provider_usage: Option<TokenUsageTotals>,
}

impl UsageLogger {
    pub async fn try_new(
        state: Arc<StateRuntime>,
        thread_id: ThreadId,
        source: SessionSource,
        forked_from_id: Option<ThreadId>,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
    ) -> anyhow::Result<Self> {
        let pool = state.usage_pool();
        let parent_thread_id = Self::parent_thread_from_source(&source);
        // Reuse the first persisted root we can find so spawned and forked descendants
        // share one canonical root thread id in `usage_threads`.
        let root_thread_id =
            Self::resolve_root_thread_id(&pool, parent_thread_id.as_ref(), forked_from_id.as_ref())
                .await?;
        let root_thread_id = root_thread_id
            .or_else(|| {
                parent_thread_id
                    .as_ref()
                    .map(std::string::ToString::to_string)
            })
            .or_else(|| {
                forked_from_id
                    .as_ref()
                    .map(std::string::ToString::to_string)
            })
            .unwrap_or_else(|| thread_id.to_string());
        let created_at = Utc::now();
        let source_str = source.to_string();
        sqlx::query(
            r#"
INSERT INTO usage_threads (thread_id, parent_thread_id, root_thread_id, fork_parent_thread_id, agent_nickname, agent_role, source, created_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id) DO UPDATE SET
    parent_thread_id = COALESCE(excluded.parent_thread_id, usage_threads.parent_thread_id),
    root_thread_id = COALESCE(excluded.root_thread_id, usage_threads.root_thread_id),
    fork_parent_thread_id = COALESCE(excluded.fork_parent_thread_id, usage_threads.fork_parent_thread_id),
    agent_nickname = COALESCE(excluded.agent_nickname, usage_threads.agent_nickname),
    agent_role = COALESCE(excluded.agent_role, usage_threads.agent_role),
    source = excluded.source
                "#,
        )
        .bind(thread_id.to_string())
        .bind(parent_thread_id.as_ref().map(std::string::ToString::to_string))
        .bind(root_thread_id.clone())
        .bind(forked_from_id.as_ref().map(std::string::ToString::to_string))
        .bind(agent_nickname.as_deref())
        .bind(agent_role.as_deref())
        .bind(source_str)
        .bind(created_at.to_rfc3339())
        .execute(pool.as_ref())
        .await
        ?;
        Ok(Self {
            pool,
            thread_id,
            _session_source: source,
            _parent_thread_id: parent_thread_id,
            _root_thread_id: root_thread_id.clone(),
            _fork_parent_thread_id: forked_from_id,
            turn_snapshots: HashMap::new(),
            spawn_requests: HashMap::new(),
            tool_calls: HashMap::new(),
            last_provider_call_id: None,
            last_provider_usage: None,
        })
    }

    fn parent_thread_from_source(source: &SessionSource) -> Option<ThreadId> {
        match source {
            SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                parent_thread_id,
                ..
            }) => Some(*parent_thread_id),
            _ => None,
        }
    }

    async fn resolve_root_thread_id(
        pool: &SqlitePool,
        parent: Option<&ThreadId>,
        fork_parent: Option<&ThreadId>,
    ) -> anyhow::Result<Option<String>> {
        let candidate = parent.or(fork_parent);
        let Some(id) = candidate else {
            return Ok(None);
        };
        let row = sqlx::query("SELECT root_thread_id FROM usage_threads WHERE thread_id = ?")
            .bind(id.to_string())
            .fetch_optional(pool)
            .await?;
        Ok(row.and_then(|row| row.try_get::<String, _>("root_thread_id").ok()))
    }

    pub fn update_turn_snapshot(
        &mut self,
        turn_id: &str,
        requested_model: Option<String>,
        requested_provider: Option<String>,
    ) {
        if turn_id.is_empty() {
            return;
        }
        self.turn_snapshots.insert(
            turn_id.to_string(),
            TurnSnapshot {
                requested_model,
                _requested_provider: requested_provider,
            },
        );
    }

    pub async fn record_event(&mut self, event: &Event) {
        if self.pool.is_closed() {
            return;
        }
        let turn_id = (!event.id.is_empty()).then(|| event.id.clone());
        match &event.msg {
            EventMsg::TokenCount(token_count) => {
                if let Err(err) = self
                    .handle_token_count(token_count, turn_id.as_deref())
                    .await
                {
                    warn!("usage token count: {err}");
                }
            }
            EventMsg::McpToolCallBegin(begin) => {
                if let Err(err) = self.handle_tool_call_begin(begin, turn_id.as_deref()).await {
                    warn!("usage tool call begin: {err}");
                }
            }
            EventMsg::McpToolCallEnd(end) => {
                if let Err(err) = self.handle_tool_call_end(end).await {
                    warn!("usage tool call end: {err}");
                }
            }
            EventMsg::CollabAgentSpawnBegin(begin) => {
                self.spawn_requests.insert(
                    begin.call_id.clone(),
                    SpawnRequestState {
                        parent_thread_id: begin.sender_thread_id,
                        _requested_model: begin.model.clone(),
                        _requested_reasoning_effort: begin.reasoning_effort.to_string(),
                    },
                );
                if let Err(err) = self.insert_spawn_request(begin).await {
                    warn!("usage spawn begin: {err}");
                }
            }
            EventMsg::CollabAgentSpawnEnd(end) => {
                if let Err(err) = self.handle_spawn_end(end).await {
                    warn!("usage spawn end: {err}");
                }
            }
            EventMsg::TurnComplete(_turn_complete) => {
                if let Some(turn_id) = &turn_id {
                    self.turn_snapshots.remove(turn_id);
                }
            }
            _ => {}
        }
    }

    async fn handle_token_count(
        &mut self,
        token_count: &TokenCountEvent,
        turn_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let Some(usage) = token_count
            .info
            .as_ref()
            .map(|info| info.last_token_usage.clone())
        else {
            return Ok(());
        };
        let turn_snapshot = turn_id.and_then(|id| self.turn_snapshots.get(id)).cloned();
        let requested_model = turn_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.requested_model.clone())
            .or_else(|| token_count.model_used.clone());
        let provider = token_count.provider.clone();
        let spawn_request_id = self.lookup_spawn_request_id().await?;
        let provider_call_id = Uuid::new_v4().to_string();
        let started_at = Utc::now();
        let status = if token_count.info.is_some() {
            "ok"
        } else {
            "error"
        };
        let uncached_input_tokens = (usage.input_tokens - usage.cached_input_tokens).max(0);
        sqlx::query(
            r#"INSERT INTO usage_provider_calls (
            provider_call_id,
            thread_id,
            turn_id,
            spawn_request_id,
            provider,
            requested_model,
            actual_model_used,
            started_at,
            completed_at,
            input_tokens_uncached,
            input_tokens_cached,
            output_tokens,
            total_tokens,
            status
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(provider_call_id.clone())
        .bind(self.thread_id.to_string())
        .bind(turn_id.map(str::to_string))
        .bind(spawn_request_id)
        .bind(provider.clone())
        .bind(requested_model.clone())
        .bind(token_count.model_used.clone())
        .bind(started_at.to_rfc3339())
        .bind(Utc::now().to_rfc3339())
        .bind(uncached_input_tokens)
        .bind(usage.cached_input_tokens)
        .bind(usage.output_tokens)
        .bind(usage.total_tokens)
        .bind(status)
        .execute(self.pool.as_ref())
        .await?;
        self.last_provider_call_id = Some(provider_call_id);
        self.last_provider_usage = Some(TokenUsageTotals {
            uncached_input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
        });
        if let Some(rate_limits) = &token_count.rate_limits {
            self.insert_quota_snapshot(turn_id, rate_limits).await?;
        }
        Ok(())
    }

    async fn lookup_spawn_request_id(&self) -> anyhow::Result<Option<String>> {
        sqlx::query_scalar::<_, String>(
            r#"SELECT spawn_request_id
            FROM usage_spawn_requests
            WHERE child_thread_id = ?
            ORDER BY rowid DESC
            LIMIT 1"#,
        )
        .bind(self.thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(Into::into)
    }

    async fn insert_quota_snapshot(
        &self,
        turn_id: Option<&str>,
        snapshot: &RateLimitSnapshot,
    ) -> anyhow::Result<()> {
        let Some(primary) = snapshot.primary.as_ref() else {
            return Ok(());
        };
        let used = primary.used_percent;
        let remaining = (100.0 - used).max(0.0);
        let plan = snapshot.plan_type.as_ref().map(|plan| format!("{plan:?}"));
        sqlx::query(
            r#"INSERT INTO usage_quota_snapshots (
            snapshot_id,
            thread_id,
            turn_id,
            quota_source,
            quota_percent_remaining,
            quota_percent_used,
            plan
        ) VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(self.thread_id.to_string())
        .bind(turn_id.map(str::to_string))
        .bind(
            snapshot
                .limit_name
                .clone()
                .or_else(|| snapshot.limit_id.clone())
                .unwrap_or_else(|| "primary".to_string()),
        )
        .bind(remaining)
        .bind(used)
        .bind(plan)
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    async fn handle_tool_call_begin(
        &mut self,
        begin: &McpToolCallBeginEvent,
        turn_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let now = Utc::now();
        self.tool_calls.insert(
            begin.call_id.clone(),
            ToolCallState {
                _tool_name: begin.invocation.tool.clone(),
                _server_name: Some(begin.invocation.server.clone()),
                _started_at: now,
            },
        );
        sqlx::query(
            r#"INSERT INTO usage_tool_calls (
            tool_call_id,
            thread_id,
            turn_id,
            tool_name,
            server_name,
            started_at,
            status
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(tool_call_id) DO NOTHING"#,
        )
        .bind(begin.call_id.clone())
        .bind(self.thread_id.to_string())
        .bind(turn_id.map(str::to_string))
        .bind(begin.invocation.tool.clone())
        .bind(Some(begin.invocation.server.clone()))
        .bind(now.to_rfc3339())
        .bind("started")
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    async fn handle_tool_call_end(&mut self, end: &McpToolCallEndEvent) -> anyhow::Result<()> {
        if let Some(_state) = self.tool_calls.remove(&end.call_id) {
            let completed_at = Utc::now();
            let status = if end.is_success() {
                "succeeded"
            } else {
                "failed"
            };
            let duration_ms = end.duration.as_millis() as i64;
            sqlx::query(
                r#"UPDATE usage_tool_calls SET
                completed_at = ?,
                status = ?,
                duration_ms = ?
            WHERE tool_call_id = ?"#,
            )
            .bind(completed_at.to_rfc3339())
            .bind(status)
            .bind(duration_ms)
            .bind(end.call_id.clone())
            .execute(self.pool.as_ref())
            .await?;
        }
        Ok(())
    }

    async fn insert_spawn_request(&self, begin: &CollabAgentSpawnBeginEvent) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO usage_spawn_requests (
            spawn_request_id,
            parent_thread_id,
            requested_model,
            requested_reasoning_effort,
            status,
            created_at
        ) VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(spawn_request_id) DO NOTHING"#,
        )
        .bind(begin.call_id.clone())
        .bind(begin.sender_thread_id.to_string())
        .bind(begin.model.clone())
        .bind(begin.reasoning_effort.to_string())
        .bind("pending")
        .bind(Utc::now().to_rfc3339())
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    async fn handle_spawn_end(&mut self, end: &CollabAgentSpawnEndEvent) -> anyhow::Result<()> {
        if let Some(request) = self.spawn_requests.remove(&end.call_id) {
            let status = format!("{:?}", end.status);
            let child_thread = end
                .new_thread_id
                .as_ref()
                .map(std::string::ToString::to_string);
            let completed_at = Utc::now().to_rfc3339();
            sqlx::query(
                r#"UPDATE usage_spawn_requests SET
                child_thread_id = ?,
                requested_role = ?,
                status = ?,
                completed_at = ?
            WHERE spawn_request_id = ?"#,
            )
            .bind(child_thread.clone())
            .bind(end.new_agent_role.clone())
            .bind(status.clone())
            .bind(completed_at)
            .bind(end.call_id.clone())
            .execute(self.pool.as_ref())
            .await?;
            if let Some(child) = end.new_thread_id {
                self.insert_fork_snapshot(child, request, status).await?;
            }
        }
        Ok(())
    }

    async fn insert_fork_snapshot(
        &self,
        child_thread_id: ThreadId,
        request: SpawnRequestState,
        _request_status: String,
    ) -> anyhow::Result<()> {
        let parent_call_id = self.last_provider_call_id.clone();
        let usage = self.last_provider_usage.clone();
        sqlx::query(
            r#"INSERT INTO usage_fork_snapshots (
            child_thread_id,
            parent_thread_id,
            forked_at,
            parent_last_provider_call_id,
            parent_cumulative_uncached_tokens,
            parent_cumulative_cached_tokens,
            parent_cumulative_output_tokens,
            parent_cumulative_total_tokens
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(child_thread_id) DO UPDATE SET
            parent_last_provider_call_id = COALESCE(excluded.parent_last_provider_call_id, usage_fork_snapshots.parent_last_provider_call_id),
            parent_cumulative_uncached_tokens = COALESCE(excluded.parent_cumulative_uncached_tokens, usage_fork_snapshots.parent_cumulative_uncached_tokens),
            parent_cumulative_cached_tokens = COALESCE(excluded.parent_cumulative_cached_tokens, usage_fork_snapshots.parent_cumulative_cached_tokens),
            parent_cumulative_output_tokens = COALESCE(excluded.parent_cumulative_output_tokens, usage_fork_snapshots.parent_cumulative_output_tokens),
            parent_cumulative_total_tokens = COALESCE(excluded.parent_cumulative_total_tokens, usage_fork_snapshots.parent_cumulative_total_tokens)
        "#,
        )
        .bind(child_thread_id.to_string())
        .bind(request.parent_thread_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind(parent_call_id)
        .bind(usage.as_ref().map(|u| u.uncached_input_tokens))
        .bind(usage.as_ref().map(|u| u.cached_input_tokens))
        .bind(usage.as_ref().map(|u| u.output_tokens))
        .bind(usage.as_ref().map(|u| u.total_tokens))
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DirectionalThreadSpawnEdgeStatus;
    use anyhow::Result;
    use codex_protocol::ThreadId;
    use codex_protocol::mcp::CallToolResult;
    use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::CollabAgentSpawnBeginEvent;
    use codex_protocol::protocol::CollabAgentSpawnEndEvent;
    use codex_protocol::protocol::Event;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::McpInvocation;
    use codex_protocol::protocol::McpToolCallBeginEvent;
    use codex_protocol::protocol::McpToolCallEndEvent;
    use codex_protocol::protocol::RateLimitSnapshot;
    use codex_protocol::protocol::RateLimitWindow;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use codex_protocol::protocol::TokenCountEvent;
    use codex_protocol::protocol::TokenUsage;
    use codex_protocol::protocol::TokenUsageInfo;
    use codex_protocol::protocol::TurnCompleteEvent;
    use pretty_assertions::assert_eq;
    use sqlx::SqlitePool;
    use std::time::Duration;
    use tempfile::TempDir;
    use tempfile::tempdir;

    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
    struct ProviderCallRow {
        provider: Option<String>,
        requested_model: Option<String>,
        actual_model_used: Option<String>,
        input_tokens_uncached: i64,
        input_tokens_cached: i64,
        output_tokens: i64,
        total_tokens: i64,
        status: Option<String>,
    }

    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
    struct ProviderCallRowWithSpawn {
        spawn_request_id: Option<String>,
        provider: Option<String>,
    }

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct QuotaSnapshotRow {
        quota_source: Option<String>,
        quota_percent_remaining: f64,
        quota_percent_used: f64,
    }

    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
    struct ToolCallRow {
        tool_name: String,
        server_name: Option<String>,
        status: Option<String>,
        duration_ms: Option<i64>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
    struct SpawnRequestRow {
        parent_thread_id: String,
        child_thread_id: Option<String>,
        requested_model: Option<String>,
        requested_role: Option<String>,
        requested_reasoning_effort: Option<String>,
        status: Option<String>,
        completed_at: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
    struct ForkSnapshotRow {
        parent_thread_id: String,
        parent_last_provider_call_id: Option<String>,
        parent_cumulative_uncached_tokens: Option<i64>,
        parent_cumulative_cached_tokens: Option<i64>,
        parent_cumulative_output_tokens: Option<i64>,
        parent_cumulative_total_tokens: Option<i64>,
    }

    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
    struct ThreadRow {
        parent_thread_id: Option<String>,
        root_thread_id: Option<String>,
        fork_parent_thread_id: Option<String>,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
        source: Option<String>,
    }

    fn token_count_event(turn_id: &str, include_rate_limit: bool) -> Event {
        let usage = TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 2,
            output_tokens: 3,
            reasoning_output_tokens: 1,
            total_tokens: 16,
        };
        let info = TokenUsageInfo {
            total_token_usage: usage.clone(),
            last_token_usage: usage,
            model_context_window: Some(4096),
        };
        let rate_limits = include_rate_limit.then_some(RateLimitSnapshot {
            limit_id: None,
            limit_name: Some("primary".to_string()),
            primary: Some(RateLimitWindow {
                used_percent: 12.5,
                window_minutes: Some(60),
                resets_at: Some(0),
            }),
            secondary: None,
            credits: None,
            rate_limit_reached_type: None,
            plan_type: None,
        });
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::TokenCount(TokenCountEvent {
                info: Some(info),
                rate_limits,
                provider: Some("test-provider".to_string()),
                model_used: Some("actual-model".to_string()),
            }),
        }
    }

    async fn init_runtime() -> Result<(Arc<StateRuntime>, TempDir)> {
        let tmp_dir = tempdir()?;
        let runtime =
            StateRuntime::init(tmp_dir.path().to_path_buf(), "test-provider".to_string()).await?;
        Ok((runtime, tmp_dir))
    }

    #[tokio::test]
    async fn usage_logger_records_requested_model_and_quota_snapshot() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let mut logger = UsageLogger::try_new(
            runtime.clone(),
            thread_id,
            SessionSource::Cli,
            /*forked_from_id*/ None,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;

        let turn_id = "turn-1";
        logger.update_turn_snapshot(
            turn_id,
            Some("requested-model".into()),
            Some("requested-provider".into()),
        );

        let token_event = token_count_event(turn_id, /*include_rate_limit*/ true);
        logger.record_event(&token_event).await;

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();

        let provider_row: ProviderCallRow = sqlx::query_as(
            r#"
SELECT
  provider,
  requested_model,
  actual_model_used,
  input_tokens_uncached,
  input_tokens_cached,
  output_tokens,
  total_tokens,
  status
FROM usage_provider_calls
WHERE thread_id = ?
"#,
        )
        .bind(thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            provider_row,
            ProviderCallRow {
                provider: Some("test-provider".to_string()),
                requested_model: Some("requested-model".to_string()),
                actual_model_used: Some("actual-model".to_string()),
                input_tokens_uncached: 8,
                input_tokens_cached: 2,
                output_tokens: 3,
                total_tokens: 16,
                status: Some("ok".to_string()),
            }
        );

        let quota_row: QuotaSnapshotRow = sqlx::query_as(
            r#"
SELECT
  quota_source,
  quota_percent_remaining,
  quota_percent_used
FROM usage_quota_snapshots
WHERE thread_id = ?
"#,
        )
        .bind(thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            quota_row,
            QuotaSnapshotRow {
                quota_source: Some("primary".to_string()),
                quota_percent_remaining: 87.5,
                quota_percent_used: 12.5,
            }
        );

        Ok(())
    }

    #[tokio::test]
    async fn usage_logger_clears_turn_snapshot_after_turn_complete() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let mut logger = UsageLogger::try_new(
            runtime.clone(),
            thread_id,
            SessionSource::Cli,
            /*forked_from_id*/ None,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;

        let turn_id = "turn-clear";
        logger.update_turn_snapshot(
            turn_id,
            Some("requested-model".to_string()),
            Some("requested-provider".to_string()),
        );
        logger
            .record_event(&token_count_event(
                turn_id, /*include_rate_limit*/ false,
            ))
            .await;
        logger
            .record_event(&Event {
                id: turn_id.to_string(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: turn_id.to_string(),
                    last_agent_message: None,
                    compaction_events_in_turn: 0,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                }),
            })
            .await;
        logger
            .record_event(&token_count_event(
                turn_id, /*include_rate_limit*/ false,
            ))
            .await;

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();
        let provider_rows: Vec<ProviderCallRow> = sqlx::query_as(
            r#"
SELECT
  provider,
  requested_model,
  actual_model_used,
  input_tokens_uncached,
  input_tokens_cached,
  output_tokens,
  total_tokens,
  status
FROM usage_provider_calls
WHERE thread_id = ?
ORDER BY rowid
"#,
        )
        .bind(thread_id.to_string())
        .fetch_all(pool)
        .await?;
        assert_eq!(
            provider_rows,
            vec![
                ProviderCallRow {
                    provider: Some("test-provider".to_string()),
                    requested_model: Some("requested-model".to_string()),
                    actual_model_used: Some("actual-model".to_string()),
                    input_tokens_uncached: 8,
                    input_tokens_cached: 2,
                    output_tokens: 3,
                    total_tokens: 16,
                    status: Some("ok".to_string()),
                },
                ProviderCallRow {
                    provider: Some("test-provider".to_string()),
                    requested_model: Some("actual-model".to_string()),
                    actual_model_used: Some("actual-model".to_string()),
                    input_tokens_uncached: 8,
                    input_tokens_cached: 2,
                    output_tokens: 3,
                    total_tokens: 16,
                    status: Some("ok".to_string()),
                },
            ]
        );

        Ok(())
    }

    #[tokio::test]
    async fn usage_logger_tracks_tool_call_lifecycle() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let mut logger = UsageLogger::try_new(
            runtime.clone(),
            thread_id,
            SessionSource::Cli,
            /*forked_from_id*/ None,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;

        let turn_id = "turn-tools";
        let tool_call_id = "tool-call";
        let tool_invocation = McpInvocation {
            server: "default-server".to_string(),
            tool: "test-tool".to_string(),
            arguments: None,
        };
        let tool_begin = Event {
            id: turn_id.to_string(),
            msg: EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
                call_id: tool_call_id.to_string(),
                invocation: tool_invocation.clone(),
                mcp_app_resource_uri: None,
            }),
        };
        logger.record_event(&tool_begin).await;

        let tool_end = Event {
            id: turn_id.to_string(),
            msg: EventMsg::McpToolCallEnd(McpToolCallEndEvent {
                call_id: tool_call_id.to_string(),
                invocation: tool_invocation,
                mcp_app_resource_uri: None,
                duration: Duration::from_millis(42),
                result: Ok(CallToolResult {
                    content: vec![],
                    structured_content: None,
                    is_error: None,
                    meta: None,
                }),
            }),
        };
        logger.record_event(&tool_end).await;

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();

        let tool_row: ToolCallRow = sqlx::query_as(
            r#"
SELECT
  tool_name,
  server_name,
  status,
  duration_ms
FROM usage_tool_calls
WHERE tool_call_id = ?
"#,
        )
        .bind(tool_call_id)
        .fetch_one(pool)
        .await?;
        assert_eq!(
            tool_row,
            ToolCallRow {
                tool_name: "test-tool".to_string(),
                server_name: Some("default-server".to_string()),
                status: Some("succeeded".to_string()),
                duration_ms: Some(42),
            }
        );

        Ok(())
    }

    #[tokio::test]
    async fn usage_logger_captures_spawn_request_and_fork_snapshot() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let mut logger = UsageLogger::try_new(
            runtime.clone(),
            thread_id,
            SessionSource::Cli,
            /*forked_from_id*/ None,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;

        let turn_id = "turn-spawn";
        logger
            .record_event(&token_count_event(
                turn_id, /*include_rate_limit*/ false,
            ))
            .await;

        let spawn_call = "spawn-call";
        let spawn_child = ThreadId::new();
        let spawn_begin = Event {
            id: turn_id.to_string(),
            msg: EventMsg::CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent {
                call_id: spawn_call.to_string(),
                sender_thread_id: thread_id,
                prompt: String::new(),
                model: "spawn-model".to_string(),
                reasoning_effort: ReasoningEffortConfig::default(),
            }),
        };
        logger.record_event(&spawn_begin).await;

        let spawn_end = Event {
            id: turn_id.to_string(),
            msg: EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                call_id: spawn_call.to_string(),
                sender_thread_id: thread_id,
                new_thread_id: Some(spawn_child),
                new_agent_nickname: None,
                new_agent_role: None,
                prompt: String::new(),
                model: "spawn-model".to_string(),
                reasoning_effort: ReasoningEffortConfig::default(),
                status: AgentStatus::Completed(None),
            }),
        };
        logger.record_event(&spawn_end).await;

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();

        let mut spawn_row: SpawnRequestRow = sqlx::query_as(
            r#"
SELECT
  parent_thread_id,
  child_thread_id,
  requested_model,
  requested_role,
  requested_reasoning_effort,
  status,
  completed_at
FROM usage_spawn_requests
WHERE spawn_request_id = ?
"#,
        )
        .bind(spawn_call)
        .fetch_one(pool)
        .await?;
        assert!(
            spawn_row.completed_at.is_some(),
            "expected completed_at for spawn row"
        );
        spawn_row.completed_at = Some("<timestamp>".to_string());
        assert_eq!(
            spawn_row,
            SpawnRequestRow {
                parent_thread_id: thread_id.to_string(),
                child_thread_id: Some(spawn_child.to_string()),
                requested_model: Some("spawn-model".to_string()),
                requested_role: None,
                requested_reasoning_effort: Some("medium".to_string()),
                status: Some(format!("{:?}", AgentStatus::Completed(None))),
                completed_at: Some("<timestamp>".to_string()),
            }
        );

        let mut fork_row: ForkSnapshotRow = sqlx::query_as(
            r#"
SELECT
  parent_thread_id,
  parent_last_provider_call_id,
  parent_cumulative_uncached_tokens,
  parent_cumulative_cached_tokens,
  parent_cumulative_output_tokens,
  parent_cumulative_total_tokens
FROM usage_fork_snapshots
WHERE child_thread_id = ?
"#,
        )
        .bind(spawn_child.to_string())
        .fetch_one(pool)
        .await?;
        assert!(
            fork_row.parent_last_provider_call_id.is_some(),
            "expected provider call id in fork snapshot"
        );
        fork_row.parent_last_provider_call_id = Some("<provider_call_id>".to_string());
        assert_eq!(
            fork_row,
            ForkSnapshotRow {
                parent_thread_id: thread_id.to_string(),
                parent_last_provider_call_id: Some("<provider_call_id>".to_string()),
                parent_cumulative_uncached_tokens: Some(8),
                parent_cumulative_cached_tokens: Some(2),
                parent_cumulative_output_tokens: Some(3),
                parent_cumulative_total_tokens: Some(16),
            }
        );

        Ok(())
    }

    #[tokio::test]
    async fn usage_logger_records_spawn_request_id_on_child_provider_calls() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let parent_thread_id = ThreadId::new();
        let mut parent_logger = UsageLogger::try_new(
            runtime.clone(),
            parent_thread_id,
            SessionSource::Cli,
            /*forked_from_id*/ None,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;

        let spawn_call_id = "spawn-provider-link";
        let child_thread_id = ThreadId::new();
        parent_logger
            .record_event(&Event {
                id: "turn-spawn-provider".to_string(),
                msg: EventMsg::CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent {
                    call_id: spawn_call_id.to_string(),
                    sender_thread_id: parent_thread_id,
                    prompt: String::new(),
                    model: "spawn-model".to_string(),
                    reasoning_effort: ReasoningEffortConfig::default(),
                }),
            })
            .await;
        parent_logger
            .record_event(&Event {
                id: "turn-spawn-provider".to_string(),
                msg: EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                    call_id: spawn_call_id.to_string(),
                    sender_thread_id: parent_thread_id,
                    new_thread_id: Some(child_thread_id),
                    new_agent_nickname: Some("child".to_string()),
                    new_agent_role: Some("explorer".to_string()),
                    prompt: String::new(),
                    model: "spawn-model".to_string(),
                    reasoning_effort: ReasoningEffortConfig::default(),
                    status: AgentStatus::Completed(None),
                }),
            })
            .await;

        let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_nickname: Some("child".to_string()),
            agent_role: Some("explorer".to_string()),
            agent_path: None,
        });
        let mut child_logger = UsageLogger::try_new(
            runtime.clone(),
            child_thread_id,
            child_source,
            /*forked_from_id*/ None,
            Some("child".to_string()),
            Some("explorer".to_string()),
        )
        .await?;
        let child_turn_id = "turn-child-token";
        child_logger
            .record_event(&token_count_event(
                child_turn_id,
                /*include_rate_limit*/ false,
            ))
            .await;

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();
        let child_provider_row: ProviderCallRowWithSpawn = sqlx::query_as(
            r#"
SELECT
  spawn_request_id,
  provider
FROM usage_provider_calls
WHERE thread_id = ?
"#,
        )
        .bind(child_thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            child_provider_row,
            ProviderCallRowWithSpawn {
                spawn_request_id: Some(spawn_call_id.to_string()),
                provider: Some("test-provider".to_string()),
            }
        );

        let top_level_thread_id = ThreadId::new();
        let mut top_level_logger = UsageLogger::try_new(
            runtime.clone(),
            top_level_thread_id,
            SessionSource::Cli,
            /*forked_from_id*/ None,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;
        let top_level_turn_id = "turn-top";
        top_level_logger
            .record_event(&token_count_event(
                top_level_turn_id,
                /*include_rate_limit*/ false,
            ))
            .await;
        let top_level_provider_row: ProviderCallRowWithSpawn = sqlx::query_as(
            r#"
SELECT
  spawn_request_id,
  provider
FROM usage_provider_calls
WHERE thread_id = ?
"#,
        )
        .bind(top_level_thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            top_level_provider_row,
            ProviderCallRowWithSpawn {
                spawn_request_id: None,
                provider: Some("test-provider".to_string()),
            }
        );

        Ok(())
    }

    #[tokio::test]
    async fn usage_logger_resolves_root_thread_from_parent_or_fork() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let parent_thread_id = ThreadId::new();
        let _parent_logger = UsageLogger::try_new(
            runtime.clone(),
            parent_thread_id,
            SessionSource::Cli,
            /*forked_from_id*/ None,
            Some("Parent".to_string()),
            Some("default".to_string()),
        )
        .await?;

        let child_thread_id = ThreadId::new();
        let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 2,
            agent_nickname: Some("Copernicus".to_string()),
            agent_role: Some("explorer".to_string()),
            agent_path: None,
        });
        let _child_logger = UsageLogger::try_new(
            runtime.clone(),
            child_thread_id,
            child_source.clone(),
            /*forked_from_id*/ None,
            Some("Copernicus".to_string()),
            Some("explorer".to_string()),
        )
        .await?;

        let fork_thread_id = ThreadId::new();
        let _fork_logger = UsageLogger::try_new(
            runtime.clone(),
            fork_thread_id,
            SessionSource::Cli,
            Some(parent_thread_id),
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();

        let child_row: ThreadRow = sqlx::query_as(
            r#"
SELECT
  parent_thread_id,
  root_thread_id,
  fork_parent_thread_id,
  agent_nickname,
  agent_role,
  source
FROM usage_threads
WHERE thread_id = ?
"#,
        )
        .bind(child_thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            child_row,
            ThreadRow {
                parent_thread_id: Some(parent_thread_id.to_string()),
                root_thread_id: Some(parent_thread_id.to_string()),
                fork_parent_thread_id: None,
                agent_nickname: Some("Copernicus".to_string()),
                agent_role: Some("explorer".to_string()),
                source: Some(child_source.to_string()),
            }
        );

        let fork_row: ThreadRow = sqlx::query_as(
            r#"
SELECT
  parent_thread_id,
  root_thread_id,
  fork_parent_thread_id,
  agent_nickname,
  agent_role,
  source
FROM usage_threads
WHERE thread_id = ?
"#,
        )
        .bind(fork_thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            fork_row,
            ThreadRow {
                parent_thread_id: None,
                root_thread_id: Some(parent_thread_id.to_string()),
                fork_parent_thread_id: Some(parent_thread_id.to_string()),
                agent_nickname: None,
                agent_role: None,
                source: Some(SessionSource::Cli.to_string()),
            }
        );

        Ok(())
    }

    #[tokio::test]
    async fn usage_logger_resolves_root_thread_from_persisted_lineage_after_restart() -> Result<()>
    {
        let tmp_dir = tempdir()?;
        let root_thread_id = ThreadId::new();
        let parent_thread_id = ThreadId::new();
        {
            let runtime =
                StateRuntime::init(tmp_dir.path().to_path_buf(), "test-provider".to_string())
                    .await?;
            let _root_logger = UsageLogger::try_new(
                runtime.clone(),
                root_thread_id,
                SessionSource::Cli,
                /*forked_from_id*/ None,
                Some("Root".to_string()),
                Some("default".to_string()),
            )
            .await?;
            let _parent_logger = UsageLogger::try_new(
                runtime.clone(),
                parent_thread_id,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: root_thread_id,
                    depth: 1,
                    agent_nickname: Some("Parent".to_string()),
                    agent_role: Some("explorer".to_string()),
                    agent_path: None,
                }),
                /*forked_from_id*/ None,
                Some("Parent".to_string()),
                Some("explorer".to_string()),
            )
            .await?;
        }

        let reopened_runtime =
            StateRuntime::init(tmp_dir.path().to_path_buf(), "test-provider".to_string()).await?;
        let child_thread_id = ThreadId::new();
        let _child_logger = UsageLogger::try_new(
            reopened_runtime.clone(),
            child_thread_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 2,
                agent_nickname: Some("Child".to_string()),
                agent_role: Some("worker".to_string()),
                agent_path: None,
            }),
            /*forked_from_id*/ None,
            Some("Child".to_string()),
            Some("worker".to_string()),
        )
        .await?;

        let fork_thread_id = ThreadId::new();
        let _fork_logger = UsageLogger::try_new(
            reopened_runtime.clone(),
            fork_thread_id,
            SessionSource::Cli,
            Some(parent_thread_id),
            Some("Fork".to_string()),
            Some("reviewer".to_string()),
        )
        .await?;

        let pool_arc = reopened_runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();

        let child_row: ThreadRow = sqlx::query_as(
            r#"
SELECT
  parent_thread_id,
  root_thread_id,
  fork_parent_thread_id,
  agent_nickname,
  agent_role,
  source
FROM usage_threads
WHERE thread_id = ?
"#,
        )
        .bind(child_thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            child_row,
            ThreadRow {
                parent_thread_id: Some(parent_thread_id.to_string()),
                root_thread_id: Some(root_thread_id.to_string()),
                fork_parent_thread_id: None,
                agent_nickname: Some("Child".to_string()),
                agent_role: Some("worker".to_string()),
                source: Some(
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                        parent_thread_id,
                        depth: 2,
                        agent_nickname: Some("Child".to_string()),
                        agent_role: Some("worker".to_string()),
                        agent_path: None,
                    })
                    .to_string()
                ),
            }
        );

        let fork_row: ThreadRow = sqlx::query_as(
            r#"
SELECT
  parent_thread_id,
  root_thread_id,
  fork_parent_thread_id,
  agent_nickname,
  agent_role,
  source
FROM usage_threads
WHERE thread_id = ?
"#,
        )
        .bind(fork_thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            fork_row,
            ThreadRow {
                parent_thread_id: None,
                root_thread_id: Some(root_thread_id.to_string()),
                fork_parent_thread_id: Some(parent_thread_id.to_string()),
                agent_nickname: Some("Fork".to_string()),
                agent_role: Some("reviewer".to_string()),
                source: Some(SessionSource::Cli.to_string()),
            }
        );

        Ok(())
    }

    #[tokio::test]
    async fn usage_spawn_lineage_matches_persisted_state_edge_for_child_thread() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let parent_thread_id = ThreadId::new();
        let mut parent_logger = UsageLogger::try_new(
            runtime.clone(),
            parent_thread_id,
            SessionSource::Cli,
            /*forked_from_id*/ None,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;

        let spawn_call = "spawn-lineage-contract";
        let child_thread_id = ThreadId::new();
        let spawn_begin = Event {
            id: "turn-spawn-contract".to_string(),
            msg: EventMsg::CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent {
                call_id: spawn_call.to_string(),
                sender_thread_id: parent_thread_id,
                prompt: String::new(),
                model: "spawn-model".to_string(),
                reasoning_effort: ReasoningEffortConfig::default(),
            }),
        };
        parent_logger.record_event(&spawn_begin).await;

        let spawn_end = Event {
            id: "turn-spawn-contract".to_string(),
            msg: EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                call_id: spawn_call.to_string(),
                sender_thread_id: parent_thread_id,
                new_thread_id: Some(child_thread_id),
                new_agent_nickname: Some("Copernicus".to_string()),
                new_agent_role: Some("explorer".to_string()),
                prompt: String::new(),
                model: "spawn-model".to_string(),
                reasoning_effort: ReasoningEffortConfig::default(),
                status: AgentStatus::Completed(None),
            }),
        };
        parent_logger.record_event(&spawn_end).await;

        runtime
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await?;

        let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_nickname: Some("Copernicus".to_string()),
            agent_role: Some("explorer".to_string()),
            agent_path: None,
        });
        let _child_logger = UsageLogger::try_new(
            runtime.clone(),
            child_thread_id,
            child_source.clone(),
            /*forked_from_id*/ None,
            Some("Copernicus".to_string()),
            Some("explorer".to_string()),
        )
        .await?;

        let children = runtime
            .list_thread_spawn_children_with_status(
                parent_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await?;
        assert_eq!(children, vec![child_thread_id]);

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();

        let spawn_row: SpawnRequestRow = sqlx::query_as(
            r#"
SELECT
  parent_thread_id,
  child_thread_id,
  requested_model,
  requested_role,
  requested_reasoning_effort,
  status,
  completed_at
FROM usage_spawn_requests
WHERE spawn_request_id = ?
"#,
        )
        .bind(spawn_call)
        .fetch_one(pool)
        .await?;
        assert_eq!(
            spawn_row.parent_thread_id,
            parent_thread_id.to_string(),
            "usage spawn request should keep the same parent as the persisted edge"
        );
        assert_eq!(
            spawn_row.child_thread_id,
            Some(child_thread_id.to_string()),
            "usage spawn request should keep the same child as the persisted edge"
        );

        let child_row: ThreadRow = sqlx::query_as(
            r#"
SELECT
  parent_thread_id,
  root_thread_id,
  fork_parent_thread_id,
  agent_nickname,
  agent_role,
  source
FROM usage_threads
WHERE thread_id = ?
"#,
        )
        .bind(child_thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            child_row,
            ThreadRow {
                parent_thread_id: Some(parent_thread_id.to_string()),
                root_thread_id: Some(parent_thread_id.to_string()),
                fork_parent_thread_id: None,
                agent_nickname: Some("Copernicus".to_string()),
                agent_role: Some("explorer".to_string()),
                source: Some(child_source.to_string()),
            }
        );

        Ok(())
    }
}
