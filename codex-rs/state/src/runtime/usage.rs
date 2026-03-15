use crate::StateRuntime;
use chrono::{DateTime, Utc};
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
        let root_thread_id =
            Self::resolve_root_thread_id(&pool, parent_thread_id.as_ref(), forked_from_id.as_ref())
                .await?;
        let root_thread_id = root_thread_id
            .or_else(|| parent_thread_id.as_ref().map(|id| id.to_string()))
            .or_else(|| forked_from_id.as_ref().map(|id| id.to_string()))
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
        .bind(parent_thread_id.as_ref().map(|id| id.to_string()))
        .bind(root_thread_id.clone())
        .bind(forked_from_id.as_ref().map(|id| id.to_string()))
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
            SessionSource::SubAgent(sub) => match sub {
                codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                    parent_thread_id, ..
                } => Some(*parent_thread_id),
                _ => None,
            },
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
        let last_usage = token_count
            .info
            .as_ref()
            .map(|info| info.last_token_usage.clone());
        if last_usage.is_none() {
            return Ok(());
        }
        let usage = last_usage.unwrap();
        let turn_snapshot = turn_id.and_then(|id| self.turn_snapshots.get(id)).cloned();
        let requested_model = turn_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.requested_model.clone())
            .or_else(|| token_count.model_used.clone());
        let provider = token_count.provider.clone();
        let provider_call_id = Uuid::new_v4().to_string();
        let started_at = DateTime::<Utc>::from(Utc::now());
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
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(provider_call_id.clone())
        .bind(self.thread_id.to_string())
        .bind(turn_id.map(str::to_string))
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

    async fn insert_quota_snapshot(
        &self,
        turn_id: Option<&str>,
        snapshot: &RateLimitSnapshot,
    ) -> anyhow::Result<()> {
        if snapshot.primary.is_none() {
            return Ok(());
        }
        let primary = snapshot.primary.as_ref().unwrap();
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
            let child_thread = end.new_thread_id.as_ref().map(|id| id.to_string());
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
    use codex_protocol::protocol::TokenCountEvent;
    use codex_protocol::protocol::TokenUsage;
    use codex_protocol::protocol::TokenUsageInfo;
    use sqlx::SqlitePool;
    use std::time::Duration;
    use tempfile::tempdir;

    #[tokio::test]
    async fn usage_logger_populates_tables() -> Result<()> {
        let tmp_dir = tempdir()?;
        let runtime =
            StateRuntime::init(tmp_dir.path().to_path_buf(), "test-provider".to_string()).await?;
        let thread_id = ThreadId::new();
        let mut logger = UsageLogger::try_new(
            runtime.clone(),
            thread_id,
            SessionSource::Cli,
            None,
            None,
            None,
        )
        .await?;

        let turn_id = "turn-1";
        logger.update_turn_snapshot(
            turn_id,
            Some("requested-model".into()),
            Some("requested-provider".into()),
        );

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
            }),
        };
        logger.record_event(&tool_begin).await;

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
        let rate_limits = RateLimitSnapshot {
            limit_id: None,
            limit_name: Some("primary".to_string()),
            primary: Some(RateLimitWindow {
                used_percent: 12.5,
                window_minutes: Some(60),
                resets_at: Some(0),
            }),
            secondary: None,
            credits: None,
            plan_type: None,
        };
        let token_event = Event {
            id: turn_id.to_string(),
            msg: EventMsg::TokenCount(TokenCountEvent {
                info: Some(info),
                rate_limits: Some(rate_limits),
                provider: Some("test-provider".to_string()),
                model_used: Some("actual-model".to_string()),
            }),
        };
        logger.record_event(&token_event).await;

        let tool_end = Event {
            id: turn_id.to_string(),
            msg: EventMsg::McpToolCallEnd(McpToolCallEndEvent {
                call_id: tool_call_id.to_string(),
                invocation: tool_invocation,
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

        let recorded_thread: String =
            sqlx::query_scalar("SELECT thread_id FROM usage_threads WHERE thread_id = ?")
                .bind(thread_id.to_string())
                .fetch_one(pool)
                .await?;
        assert_eq!(recorded_thread, thread_id.to_string());

        let tool_name: String =
            sqlx::query_scalar("SELECT tool_name FROM usage_tool_calls WHERE tool_call_id = ?")
                .bind(tool_call_id)
                .fetch_one(pool)
                .await?;
        assert_eq!(tool_name, "test-tool");

        let actual_model: String = sqlx::query_scalar(
            "SELECT actual_model_used FROM usage_provider_calls WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(actual_model, "actual-model");

        let quota_source: String = sqlx::query_scalar(
            "SELECT quota_source FROM usage_quota_snapshots WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(quota_source, "primary");

        let spawn_status: String = sqlx::query_scalar(
            "SELECT status FROM usage_spawn_requests WHERE spawn_request_id = ?",
        )
        .bind(spawn_call)
        .fetch_one(pool)
        .await?;
        assert_eq!(spawn_status, format!("{:?}", AgentStatus::Completed(None)));

        let child_thread: String = sqlx::query_scalar(
            "SELECT child_thread_id FROM usage_fork_snapshots WHERE child_thread_id = ?",
        )
        .bind(spawn_child.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(child_thread, spawn_child.to_string());

        Ok(())
    }
}
