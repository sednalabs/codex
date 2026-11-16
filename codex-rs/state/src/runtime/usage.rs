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
use codex_protocol::protocol::RawResponseCompletedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadSource;
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
    requested_provider: Option<String>,
    requested_service_tier: Option<String>,
}

#[derive(Clone, Debug)]
struct SpawnRequestState {
    parent_thread_id: ThreadId,
    _requested_model: Option<String>,
    _requested_reasoning_effort: Option<String>,
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
    cache_write_input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
}

#[derive(Debug, sqlx_macros::FromRow)]
struct StoredProviderUsage {
    provider_call_id: String,
    input_tokens_uncached: Option<i64>,
    input_tokens_cached: Option<i64>,
    input_tokens_cache_write: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
}

impl StoredProviderUsage {
    fn into_totals(self) -> Option<TokenUsageTotals> {
        Some(TokenUsageTotals {
            uncached_input_tokens: self.input_tokens_uncached?,
            cached_input_tokens: self.input_tokens_cached?,
            cache_write_input_tokens: self.input_tokens_cache_write?,
            output_tokens: self.output_tokens?,
            total_tokens: self.total_tokens?,
        })
    }
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
        Self::try_new_with_thread_source(
            state,
            thread_id,
            source,
            /*thread_source*/ None,
            /*parent_thread_id*/ None,
            forked_from_id,
            agent_nickname,
            agent_role,
        )
        .await
    }

    pub async fn try_new_with_thread_source(
        state: Arc<StateRuntime>,
        thread_id: ThreadId,
        source: SessionSource,
        thread_source: Option<ThreadSource>,
        parent_thread_id: Option<ThreadId>,
        forked_from_id: Option<ThreadId>,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
    ) -> anyhow::Result<Self> {
        let pool = state.usage_ledger_pool().await?;
        let parent_thread_id =
            parent_thread_id.or_else(|| Self::parent_thread_from_source(&source));
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
        let thread_source_str = thread_source.as_ref().map(ThreadSource::as_str);
        // The parent may have persisted the spawn completion before this child logger
        // starts. Capture that concrete request id at creation time, rather than making
        // every downstream reader reverse-join the spawn request table.
        let spawn_request_id = sqlx::query_scalar::<_, String>(
            "SELECT spawn_request_id FROM usage_spawn_requests WHERE child_thread_id = ? ORDER BY rowid DESC LIMIT 1",
        )
        .bind(thread_id.to_string())
        .fetch_optional(pool.as_ref())
        .await?;
        sqlx::query(
            r#"
INSERT INTO usage_threads (thread_id, parent_thread_id, root_thread_id, fork_parent_thread_id, agent_nickname, agent_role, source, thread_source, lineage_edge_kind, spawn_request_id, created_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id) DO UPDATE SET
    parent_thread_id = COALESCE(excluded.parent_thread_id, usage_threads.parent_thread_id),
    root_thread_id = COALESCE(excluded.root_thread_id, usage_threads.root_thread_id),
    fork_parent_thread_id = COALESCE(excluded.fork_parent_thread_id, usage_threads.fork_parent_thread_id),
    agent_nickname = COALESCE(excluded.agent_nickname, usage_threads.agent_nickname),
    agent_role = COALESCE(excluded.agent_role, usage_threads.agent_role),
    source = excluded.source,
    thread_source = COALESCE(excluded.thread_source, usage_threads.thread_source),
    lineage_edge_kind = COALESCE(excluded.lineage_edge_kind, usage_threads.lineage_edge_kind),
    spawn_request_id = COALESCE(excluded.spawn_request_id, usage_threads.spawn_request_id)
                "#,
        )
        .bind(thread_id.to_string())
        .bind(parent_thread_id.as_ref().map(std::string::ToString::to_string))
        .bind(root_thread_id.clone())
        .bind(forked_from_id.as_ref().map(std::string::ToString::to_string))
        .bind(agent_nickname.as_deref())
        .bind(agent_role.as_deref())
        .bind(source_str)
        .bind(thread_source_str)
        .bind(Self::lineage_edge_kind(&source, thread_source.as_ref(), parent_thread_id.as_ref(), forked_from_id.as_ref()))
        .bind(spawn_request_id)
        .bind(created_at.to_rfc3339())
        .execute(pool.as_ref())
        .await
        ?;
        // Reconnect and resume must retain the last recorded provider response.
        // TokenCount alone no longer generates provider rows, and a stale response
        // redelivery must not displace the persisted latest completion.
        let last_recorded = sqlx::query_as::<_, StoredProviderUsage>(
            r#"SELECT provider_call_id, input_tokens_uncached, input_tokens_cached,
                      input_tokens_cache_write, output_tokens, total_tokens
               FROM usage_provider_calls
               WHERE thread_id = ?
               ORDER BY completed_at DESC, started_at DESC, rowid DESC
               LIMIT 1"#,
        )
        .bind(thread_id.to_string())
        .fetch_optional(pool.as_ref())
        .await?;
        let last_provider_call_id = last_recorded
            .as_ref()
            .map(|stored| stored.provider_call_id.clone());
        let last_provider_usage = last_recorded.and_then(StoredProviderUsage::into_totals);
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
            last_provider_call_id,
            last_provider_usage,
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

    fn lineage_edge_kind(
        source: &SessionSource,
        thread_source: Option<&ThreadSource>,
        parent_thread_id: Option<&ThreadId>,
        forked_from_id: Option<&ThreadId>,
    ) -> Option<&'static str> {
        if parent_thread_id.is_some()
            || Self::parent_thread_from_source(source).is_some()
            || matches!(thread_source, Some(ThreadSource::Subagent))
        {
            Some("agent_spawn")
        } else if forked_from_id.is_some() {
            Some("fork")
        } else if matches!(thread_source, Some(ThreadSource::User)) {
            Some("root")
        } else {
            None
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
        requested_service_tier: Option<String>,
    ) {
        if turn_id.is_empty() {
            return;
        }
        self.turn_snapshots.insert(
            turn_id.to_string(),
            TurnSnapshot {
                requested_model,
                requested_provider,
                requested_service_tier,
            },
        );
    }

    pub async fn record_event(&mut self, event: &Event) {
        if self.pool.is_closed() {
            return;
        }
        let turn_id = (!event.id.is_empty()).then(|| event.id.clone());
        match &event.msg {
            EventMsg::RawResponseCompleted(response) => {
                if let Err(err) = self
                    .handle_raw_response_completed(response, turn_id.as_deref())
                    .await
                {
                    warn!("usage raw response completed: {err}");
                }
            }
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
                // Upstream reports effective spawn settings, not explicit requested settings.
                let requested_model = None;
                let requested_reasoning_effort = None;
                self.spawn_requests.insert(
                    begin.call_id.clone(),
                    SpawnRequestState {
                        parent_thread_id: begin.sender_thread_id,
                        _requested_model: requested_model.clone(),
                        _requested_reasoning_effort: requested_reasoning_effort.clone(),
                    },
                );
                if let Err(err) = self
                    .insert_spawn_request(begin, requested_model, requested_reasoning_effort)
                    .await
                {
                    warn!("usage spawn begin: {err}");
                }
            }
            EventMsg::CollabAgentSpawnEnd(end) => {
                if let Err(err) = self.handle_spawn_end(end).await {
                    warn!("usage spawn end: {err}");
                }
            }
            EventMsg::TurnComplete(_) => {
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
        if let Some(rate_limits) = &token_count.rate_limits {
            self.insert_quota_snapshot(turn_id, rate_limits).await?;
        }
        Ok(())
    }

    async fn handle_raw_response_completed(
        &mut self,
        response: &RawResponseCompletedEvent,
        turn_id: Option<&str>,
    ) -> anyhow::Result<()> {
        if response.response_id.is_empty() {
            self.last_provider_call_id = None;
            self.last_provider_usage = None;
            warn!("usage raw response completed: missing response id; provider call not recorded");
            return Ok(());
        }

        let turn_snapshot = turn_id.and_then(|id| self.turn_snapshots.get(id)).cloned();
        let requested_model = turn_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.requested_model.clone())
            .map(|value| value.to_ascii_lowercase());
        let provider = turn_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.requested_provider.clone())
            .map(|value| value.to_ascii_lowercase());
        let requested_service_tier = turn_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.requested_service_tier.clone());
        let spawn_request_id = self.lookup_spawn_request_id().await?;
        // Scope the provider response identity to its thread while keeping it
        // deterministic across repeated delivery and logger restarts.
        let provider_call_id = format!("response:{}:{}", self.thread_id, response.response_id);
        // A previously recorded older response can be replayed after a newer
        // one. Refresh its durable row, but do not make it the in-memory last
        // call used by turn completion, token metadata or fork snapshots.
        let already_recorded: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM usage_provider_calls WHERE provider_call_id = ?)",
        )
        .bind(&provider_call_id)
        .fetch_one(self.pool.as_ref())
        .await?;
        let now = Utc::now().to_rfc3339();
        let (
            uncached_input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            total_tokens,
            status,
        ) = match response.token_usage.as_ref() {
            Some(usage) => (
                Some(
                    usage
                        .input_tokens
                        .saturating_sub(usage.cached_input_tokens)
                        .max(0),
                ),
                Some(usage.cached_input_tokens),
                Some(usage.cache_write_input_tokens),
                Some(usage.output_tokens),
                Some(usage.total_tokens),
                "ok",
            ),
            None => (None, None, None, None, None, "provider_usage_missing"),
        };

        sqlx::query(
            r#"INSERT INTO usage_provider_calls (
                provider_call_id, thread_id, turn_id, spawn_request_id, provider,
                requested_model, requested_service_tier, started_at, completed_at, input_tokens_uncached,
                input_tokens_cached, input_tokens_cache_write, output_tokens,
                total_tokens, status
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(provider_call_id) DO UPDATE SET
                turn_id = COALESCE(usage_provider_calls.turn_id, excluded.turn_id),
                spawn_request_id = COALESCE(usage_provider_calls.spawn_request_id, excluded.spawn_request_id),
                provider = COALESCE(usage_provider_calls.provider, excluded.provider),
                requested_model = COALESCE(usage_provider_calls.requested_model, excluded.requested_model),
                requested_service_tier = COALESCE(usage_provider_calls.requested_service_tier, excluded.requested_service_tier),
                -- Preserve the first observed completion time on redelivery.
                input_tokens_uncached = COALESCE(excluded.input_tokens_uncached, usage_provider_calls.input_tokens_uncached),
                input_tokens_cached = COALESCE(excluded.input_tokens_cached, usage_provider_calls.input_tokens_cached),
                input_tokens_cache_write = COALESCE(excluded.input_tokens_cache_write, usage_provider_calls.input_tokens_cache_write),
                output_tokens = COALESCE(excluded.output_tokens, usage_provider_calls.output_tokens),
                total_tokens = COALESCE(excluded.total_tokens, usage_provider_calls.total_tokens),
                status = CASE
                    WHEN usage_provider_calls.status = 'ok'
                     AND excluded.status = 'provider_usage_missing'
                    THEN usage_provider_calls.status
                    ELSE excluded.status
                END
            WHERE usage_provider_calls.thread_id = excluded.thread_id"#,
        )
        .bind(&provider_call_id)
        .bind(self.thread_id.to_string())
        .bind(turn_id.map(str::to_string))
        .bind(spawn_request_id)
        .bind(provider)
        .bind(requested_model)
        .bind(requested_service_tier)
        .bind(&now)
        .bind(&now)
        .bind(uncached_input_tokens)
        .bind(cached_input_tokens)
        .bind(cache_write_input_tokens)
        .bind(output_tokens)
        .bind(total_tokens)
        .bind(status)
        .execute(self.pool.as_ref())
        .await?;

        if !already_recorded
            || self.last_provider_call_id.is_none()
            || self.last_provider_call_id.as_deref() == Some(provider_call_id.as_str())
        {
            self.last_provider_call_id = Some(provider_call_id.clone());
            self.last_provider_usage = match response.token_usage.as_ref() {
                Some(usage) => Some(TokenUsageTotals {
                    uncached_input_tokens: usage
                        .input_tokens
                        .saturating_sub(usage.cached_input_tokens)
                        .max(0),
                    cached_input_tokens: usage.cached_input_tokens,
                    cache_write_input_tokens: usage.cache_write_input_tokens,
                    output_tokens: usage.output_tokens,
                    total_tokens: usage.total_tokens,
                }),
                None => {
                    // The persisted row may already have measured usage from
                    // an earlier delivery of this response.
                    let stored = sqlx::query_as::<_, StoredProviderUsage>(
                        "SELECT provider_call_id, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens FROM usage_provider_calls WHERE provider_call_id = ?",
                    )
                    .bind(&provider_call_id)
                    .fetch_one(self.pool.as_ref())
                    .await?;
                    stored.into_totals()
                }
            };
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

    async fn insert_spawn_request(
        &self,
        begin: &CollabAgentSpawnBeginEvent,
        requested_model: Option<String>,
        requested_reasoning_effort: Option<String>,
    ) -> anyhow::Result<()> {
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
        .bind(requested_model)
        .bind(requested_reasoning_effort)
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
                // The child logger can start before or after this event. Stamp the
                // concrete request id either way so lineage does not rely on an
                // eventually-consistent reverse lookup in usage_spawn_requests.
                sqlx::query(
                    r#"UPDATE usage_threads
SET spawn_request_id = ?,
    lineage_edge_kind = COALESCE(lineage_edge_kind, 'agent_spawn')
WHERE thread_id = ?"#,
                )
                .bind(end.call_id.clone())
                .bind(child.to_string())
                .execute(self.pool.as_ref())
                .await?;
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
            parent_cumulative_cache_write_tokens,
            parent_cumulative_output_tokens,
            parent_cumulative_total_tokens
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(child_thread_id) DO UPDATE SET
            parent_last_provider_call_id = COALESCE(excluded.parent_last_provider_call_id, usage_fork_snapshots.parent_last_provider_call_id),
            parent_cumulative_uncached_tokens = COALESCE(excluded.parent_cumulative_uncached_tokens, usage_fork_snapshots.parent_cumulative_uncached_tokens),
            parent_cumulative_cached_tokens = COALESCE(excluded.parent_cumulative_cached_tokens, usage_fork_snapshots.parent_cumulative_cached_tokens),
            parent_cumulative_cache_write_tokens = COALESCE(excluded.parent_cumulative_cache_write_tokens, usage_fork_snapshots.parent_cumulative_cache_write_tokens),
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
        .bind(usage.as_ref().map(|u| u.cache_write_input_tokens))
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
    use codex_protocol::protocol::TokenUsage;
    use codex_utils_absolute_path::test_support::PathExt;

    #[tokio::test]
    async fn raw_response_upsert_preserves_identity_metadata_and_missing_usage()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(dir.path().abs()),
            "test".to_string(),
        )
        .await?;
        let thread = ThreadId::new();
        let mut logger = UsageLogger::try_new(
            runtime.clone(),
            thread,
            SessionSource::Cli,
            None,
            None,
            None,
        )
        .await?;
        logger.update_turn_snapshot(
            "turn",
            Some("model".into()),
            Some("provider".into()),
            Some("tier".into()),
        );
        let event = |usage| Event {
            id: "turn".into(),
            msg: EventMsg::RawResponseCompleted(RawResponseCompletedEvent {
                response_id: "response".into(),
                token_usage: usage,
                usage_metadata: None,
            }),
        };
        logger
            .record_event(&event(Some(TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 3,
                cache_write_input_tokens: 2,
                output_tokens: 4,
                total_tokens: 14,
                reasoning_output_tokens: 0,
                codex_rollout_budget_units: None,
            })))
            .await;
        logger.record_event(&event(None)).await;
        let row: (i64, String, String, String, i64, i64, i64, String) = sqlx::query_as(
            "SELECT COUNT(*), requested_model, provider, requested_service_tier, input_tokens_uncached, input_tokens_cached, output_tokens, status FROM usage_provider_calls WHERE thread_id = ?"
        ).bind(thread.to_string()).fetch_one(logger.pool.as_ref()).await?;
        assert_eq!(
            row,
            (
                1,
                "model".into(),
                "provider".into(),
                "tier".into(),
                7,
                3,
                4,
                "ok".into()
            )
        );
        runtime.close().await;
        Ok(())
    }
}
