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
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use log::warn;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
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
    cache_write_input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageThreadRecord {
    pub root_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>,
    pub thread_source: Option<String>,
}

/// A compact per-thread usage record intended for an orchestrator's tree view.
///
/// This is deliberately read-only telemetry: it describes persisted lineage and
/// provider observations, and never authorizes a thread resume.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageLineageThread {
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub root_thread_id: String,
    pub fork_parent_thread_id: Option<String>,
    pub spawn_request_id: Option<String>,
    pub thread_source: Option<String>,
    pub lineage_edge_kind: String,
    pub lineage_confidence: String,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub created_at: Option<String>,
    pub last_activity_at: Option<String>,
    pub models_used: Option<String>,
    pub service_tiers_used: Option<String>,
    pub provider_call_count: i64,
    pub unpriced_call_count: i64,
    pub partial: bool,
    pub uncached_input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub provider_reported_credits: Option<f64>,
    pub estimated_total_credits: Option<f64>,
    pub priced_credits_total: Option<f64>,
}

/// A bounded snapshot of a persisted session family. `recommended_user_resume_thread_id`
/// is advisory only and is intentionally limited to direct user threads; subagents and
/// side threads are never selected as a primary continuation by this API.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageLineageTelemetry {
    pub root_thread_id: String,
    pub recommended_user_resume_thread_id: Option<String>,
    pub threads: Vec<UsageLineageThread>,
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
        forked_from_id: Option<ThreadId>,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
    ) -> anyhow::Result<Self> {
        let pool = state.usage_ledger_pool();
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
        .bind(Self::lineage_edge_kind(&source, thread_source.as_ref(), forked_from_id.as_ref()))
        .bind(spawn_request_id)
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

    fn lineage_edge_kind(
        source: &SessionSource,
        thread_source: Option<&ThreadSource>,
        forked_from_id: Option<&ThreadId>,
    ) -> Option<&'static str> {
        if Self::parent_thread_from_source(source).is_some()
            || matches!(thread_source, Some(ThreadSource::Subagent))
        {
            Some("agent_spawn")
        } else if forked_from_id.is_some() {
            Some("fork")
        } else if matches!(thread_source, Some(ThreadSource::User | ThreadSource::Side)) {
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
            EventMsg::TurnComplete(turn_complete) => {
                if let Err(err) = self
                    .handle_turn_complete(turn_complete, turn_id.as_deref())
                    .await
                {
                    warn!("usage turn complete: {err}");
                }
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
            .or_else(|| token_count.model_used.clone())
            .map(|value| value.to_ascii_lowercase());
        let provider = token_count
            .provider
            .as_ref()
            .map(|value| value.to_ascii_lowercase());
        let actual_model_used = token_count
            .model_used
            .as_ref()
            .map(|value| value.to_ascii_lowercase());
        let requested_service_tier = token_count
            .requested_service_tier
            .as_ref()
            .map(|value| value.to_ascii_lowercase());
        let actual_service_tier = token_count
            .actual_service_tier
            .as_ref()
            .map(|value| value.to_ascii_lowercase());
        let billing_surface = token_count
            .billing_surface
            .as_ref()
            .map(|value| value.to_ascii_lowercase());
        let account_plan = token_count
            .account_plan
            .clone()
            .or_else(|| {
                token_count
                    .rate_limits
                    .as_ref()
                    .and_then(|snapshot| snapshot.plan_type.as_ref())
                    .and_then(|plan| serde_json::to_value(plan).ok())
                    .and_then(|value| value.as_str().map(str::to_owned))
            })
            .map(|value| value.to_ascii_lowercase());
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
            requested_service_tier,
            actual_service_tier,
            actual_service_tier_source,
            fast_mode_requested,
            fast_mode_used,
            billing_surface,
            account_plan,
            started_at,
            completed_at,
            input_tokens_uncached,
            input_tokens_cached,
            input_tokens_cache_write,
            output_tokens,
            total_tokens,
            status
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(provider_call_id.clone())
        .bind(self.thread_id.to_string())
        .bind(turn_id.map(str::to_string))
        .bind(spawn_request_id)
        .bind(provider.clone())
        .bind(requested_model.clone())
        .bind(actual_model_used)
        .bind(requested_service_tier)
        .bind(actual_service_tier)
        .bind(token_count.actual_service_tier_source.clone())
        .bind(token_count.fast_mode_requested)
        .bind(token_count.fast_mode_used)
        .bind(billing_surface)
        .bind(account_plan)
        .bind(started_at.to_rfc3339())
        .bind(Utc::now().to_rfc3339())
        .bind(uncached_input_tokens)
        .bind(usage.cached_input_tokens)
        .bind(usage.cache_write_input_tokens)
        .bind(usage.output_tokens)
        .bind(usage.total_tokens)
        .bind(status)
        .execute(self.pool.as_ref())
        .await?;
        self.last_provider_call_id = Some(provider_call_id);
        self.last_provider_usage = Some(TokenUsageTotals {
            uncached_input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_write_input_tokens: usage.cache_write_input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
        });
        if let Some(rate_limits) = &token_count.rate_limits {
            self.insert_quota_snapshot(turn_id, rate_limits).await?;
        }
        Ok(())
    }

    async fn handle_turn_complete(
        &self,
        turn_complete: &TurnCompleteEvent,
        turn_id: Option<&str>,
    ) -> anyhow::Result<()> {
        if turn_complete.final_model.is_none() && turn_complete.model_snapshot.is_none() {
            return Ok(());
        }
        let Some(provider_call_id) = self.last_provider_call_id.as_ref() else {
            return Ok(());
        };
        let event_turn_id = turn_id.unwrap_or(turn_complete.turn_id.as_str());
        sqlx::query(
            r#"UPDATE usage_provider_calls
SET final_model = ?,
    model_snapshot = ?
WHERE provider_call_id = ?
  AND thread_id = ?
  AND (turn_id = ? OR turn_id IS NULL)"#,
        )
        .bind(turn_complete.final_model.as_deref())
        .bind(turn_complete.model_snapshot.as_deref())
        .bind(provider_call_id)
        .bind(self.thread_id.to_string())
        .bind(event_turn_id)
        .execute(self.pool.as_ref())
        .await?;
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

impl StateRuntime {
    /// Read a complete persisted lineage family with direct usage and credit coverage.
    ///
    /// The result is bounded to one `root_thread_id`; callers must explicitly query a
    /// different root rather than accidentally receiving unrelated local sessions.
    pub async fn get_usage_lineage_telemetry(
        &self,
        thread_id: &str,
    ) -> anyhow::Result<Option<UsageLineageTelemetry>> {
        let pool = self.usage_ledger_pool();
        let root_thread_id = sqlx::query_scalar::<_, String>(
            r#"SELECT COALESCE(NULLIF(root_thread_id, ''), thread_id)
FROM usage_threads
WHERE thread_id = ?"#,
        )
        .bind(thread_id)
        .fetch_optional(pool.as_ref())
        .await?;
        let Some(root_thread_id) = root_thread_id else {
            return Ok(None);
        };

        let rows = sqlx::query(
            r#"
WITH provider_metadata AS (
    SELECT
        thread_id,
        group_concat(DISTINCT COALESCE(NULLIF(final_model, ''), NULLIF(actual_model_used, ''), NULLIF(requested_model, ''))) AS models_used,
        group_concat(DISTINCT NULLIF(actual_service_tier, '')) AS service_tiers_used,
        SUM(provider_reported_credits) AS provider_reported_credits
    FROM usage_provider_calls
    GROUP BY thread_id
), tool_activity AS (
    SELECT thread_id, MAX(COALESCE(completed_at, started_at)) AS last_tool_activity_at
    FROM usage_tool_calls
    GROUP BY thread_id
)
SELECT
    t.thread_id,
    t.parent_thread_id,
    COALESCE(NULLIF(t.root_thread_id, ''), t.thread_id) AS root_thread_id,
    t.fork_parent_thread_id,
    t.spawn_request_id,
    t.thread_source,
    t.lineage_edge_kind,
    t.agent_nickname,
    t.agent_role,
    t.created_at,
    CASE
        WHEN c.last_call_at IS NULL THEN a.last_tool_activity_at
        WHEN a.last_tool_activity_at IS NULL THEN c.last_call_at
        WHEN c.last_call_at >= a.last_tool_activity_at THEN c.last_call_at
        ELSE a.last_tool_activity_at
    END AS last_activity_at,
    COALESCE(m.models_used, c.models_used) AS models_used,
    COALESCE(m.service_tiers_used, c.service_tiers_used) AS service_tiers_used,
    COALESCE(c.provider_call_count, 0) AS provider_call_count,
    COALESCE(c.unpriced_call_count, 0) AS unpriced_call_count,
    COALESCE(c.partial, 0) AS partial,
    COALESCE(c.uncached_input_tokens, 0) AS uncached_input_tokens,
    COALESCE(c.cached_input_tokens, 0) AS cached_input_tokens,
    COALESCE(c.cache_write_input_tokens, 0) AS cache_write_input_tokens,
    COALESCE(c.output_tokens, 0) AS output_tokens,
    COALESCE(c.total_tokens, 0) AS total_tokens,
    m.provider_reported_credits,
    c.estimated_total_credits,
    c.priced_credits_total
FROM usage_threads AS t
LEFT JOIN usage_thread_credit_summary AS c ON c.thread_id = t.thread_id
LEFT JOIN provider_metadata AS m ON m.thread_id = t.thread_id
LEFT JOIN tool_activity AS a ON a.thread_id = t.thread_id
WHERE COALESCE(NULLIF(t.root_thread_id, ''), t.thread_id) = ?
ORDER BY COALESCE(last_activity_at, t.created_at) DESC, t.thread_id ASC
"#,
        )
        .bind(&root_thread_id)
        .fetch_all(pool.as_ref())
        .await?;

        let mut threads = Vec::with_capacity(rows.len());
        for row in rows {
            let row_thread_id = row.get::<String, _>("thread_id");
            let parent_thread_id = row.get::<Option<String>, _>("parent_thread_id");
            let fork_parent_thread_id = row.get::<Option<String>, _>("fork_parent_thread_id");
            let thread_source = row.get::<Option<String>, _>("thread_source");
            let persisted_kind = row.get::<Option<String>, _>("lineage_edge_kind");
            let (lineage_edge_kind, lineage_confidence) = match persisted_kind {
                Some(kind) => (kind, "persisted".to_string()),
                None if parent_thread_id.is_some()
                    || thread_source.as_deref() == Some("subagent") =>
                {
                    ("agent_spawn".to_string(), "inferred".to_string())
                }
                None if fork_parent_thread_id.is_some() => {
                    ("fork".to_string(), "inferred".to_string())
                }
                None if row_thread_id == root_thread_id => {
                    ("root".to_string(), "inferred".to_string())
                }
                None => ("unknown".to_string(), "unknown".to_string()),
            };
            threads.push(UsageLineageThread {
                thread_id: row_thread_id,
                parent_thread_id,
                root_thread_id: row.get::<String, _>("root_thread_id"),
                fork_parent_thread_id,
                spawn_request_id: row.get::<Option<String>, _>("spawn_request_id"),
                thread_source,
                lineage_edge_kind,
                lineage_confidence,
                agent_nickname: row.get::<Option<String>, _>("agent_nickname"),
                agent_role: row.get::<Option<String>, _>("agent_role"),
                created_at: row.get::<Option<String>, _>("created_at"),
                last_activity_at: row.get::<Option<String>, _>("last_activity_at"),
                models_used: row.get::<Option<String>, _>("models_used"),
                service_tiers_used: row.get::<Option<String>, _>("service_tiers_used"),
                provider_call_count: row.get::<i64, _>("provider_call_count"),
                unpriced_call_count: row.get::<i64, _>("unpriced_call_count"),
                partial: row.get::<i64, _>("partial") != 0,
                uncached_input_tokens: row.get::<i64, _>("uncached_input_tokens"),
                cached_input_tokens: row.get::<i64, _>("cached_input_tokens"),
                cache_write_input_tokens: row.get::<i64, _>("cache_write_input_tokens"),
                output_tokens: row.get::<i64, _>("output_tokens"),
                total_tokens: row.get::<i64, _>("total_tokens"),
                provider_reported_credits: row.get::<Option<f64>, _>("provider_reported_credits"),
                estimated_total_credits: row.get::<Option<f64>, _>("estimated_total_credits"),
                priced_credits_total: row.get::<Option<f64>, _>("priced_credits_total"),
            });
        }
        let recommended_user_resume_thread_id = threads
            .iter()
            .find(|thread| {
                thread.thread_source.as_deref() == Some("user") && thread.parent_thread_id.is_none()
            })
            .map(|thread| thread.thread_id.clone());
        Ok(Some(UsageLineageTelemetry {
            root_thread_id,
            recommended_user_resume_thread_id,
            threads,
        }))
    }

    pub async fn get_usage_thread_record(
        &self,
        thread_id: &str,
    ) -> anyhow::Result<Option<UsageThreadRecord>> {
        let pool = self.usage_ledger_pool();
        let row = sqlx::query(
            r#"
SELECT
  root_thread_id,
  fork_parent_thread_id,
  thread_source
FROM usage_threads
WHERE thread_id = ?
"#,
        )
        .bind(thread_id)
        .fetch_optional(pool.as_ref())
        .await?;
        Ok(row.map(|row| UsageThreadRecord {
            root_thread_id: row.get::<Option<String>, _>("root_thread_id"),
            fork_parent_thread_id: row.get::<Option<String>, _>("fork_parent_thread_id"),
            thread_source: row.get::<Option<String>, _>("thread_source"),
        }))
    }

    pub async fn get_usage_fork_snapshot_parent_thread_id(
        &self,
        child_thread_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let pool = self.usage_ledger_pool();
        let parent_thread_id = sqlx::query_scalar::<_, String>(
            "SELECT parent_thread_id FROM usage_fork_snapshots WHERE child_thread_id = ?",
        )
        .bind(child_thread_id)
        .fetch_optional(pool.as_ref())
        .await?;
        Ok(parent_thread_id)
    }

    pub async fn latest_usage_provider_display_model(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<String>> {
        let mut models = self
            .latest_usage_provider_display_models(&[thread_id])
            .await?;
        Ok(models.remove(&thread_id))
    }

    pub async fn latest_usage_provider_display_models(
        &self,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<HashMap<ThreadId, String>> {
        if thread_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let pool = self.usage_ledger_pool();
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT thread_id, display_model
FROM (
  SELECT
    thread_id,
    COALESCE(
      NULLIF(final_model, ''),
      NULLIF(model_snapshot, ''),
      NULLIF(actual_model_used, ''),
      NULLIF(requested_model, '')
    ) AS display_model,
    ROW_NUMBER() OVER (
      PARTITION BY thread_id
      ORDER BY completed_at DESC, started_at DESC, provider_call_id DESC
    ) AS row_num
  FROM usage_provider_calls
  WHERE thread_id IN (
"#,
        );
        let mut separated = builder.separated(", ");
        for thread_id in thread_ids {
            separated.push_bind(thread_id.to_string());
        }
        separated.push_unseparated(
            r#")
  AND COALESCE(
    NULLIF(final_model, ''),
    NULLIF(model_snapshot, ''),
    NULLIF(actual_model_used, ''),
    NULLIF(requested_model, '')
  ) IS NOT NULL
)
WHERE row_num = 1
"#,
        );
        let rows = builder.build().fetch_all(pool.as_ref()).await?;
        let mut models = HashMap::with_capacity(rows.len());
        for row in rows {
            let thread_id = row.get::<String, _>("thread_id");
            let Ok(thread_id) = ThreadId::from_string(&thread_id) else {
                continue;
            };
            models.insert(thread_id, row.get::<String, _>("display_model"));
        }
        Ok(models)
    }

    pub async fn record_usage_fork_snapshot(
        &self,
        child_thread_id: ThreadId,
        parent_thread_id: ThreadId,
    ) -> anyhow::Result<()> {
        let pool = self.usage_ledger_pool();
        let usage = sqlx::query(
            r#"SELECT
                provider_call_id,
                input_tokens_uncached,
                input_tokens_cached,
                input_tokens_cache_write,
                output_tokens,
                total_tokens
            FROM usage_provider_calls
            WHERE thread_id = ?
            ORDER BY completed_at DESC, started_at DESC
            LIMIT 1"#,
        )
        .bind(parent_thread_id.to_string())
        .fetch_optional(pool.as_ref())
        .await?;
        let parent_call_id = usage
            .as_ref()
            .map(|row| row.get::<String, _>("provider_call_id"));
        let uncached_tokens = usage.as_ref().map(|row| {
            row.get::<Option<i64>, _>("input_tokens_uncached")
                .unwrap_or_default()
        });
        let cached_tokens = usage.as_ref().map(|row| {
            row.get::<Option<i64>, _>("input_tokens_cached")
                .unwrap_or_default()
        });
        let cache_write_tokens = usage.as_ref().map(|row| {
            row.get::<Option<i64>, _>("input_tokens_cache_write")
                .unwrap_or_default()
        });
        let output_tokens = usage.as_ref().map(|row| {
            row.get::<Option<i64>, _>("output_tokens")
                .unwrap_or_default()
        });
        let total_tokens = usage.as_ref().map(|row| {
            row.get::<Option<i64>, _>("total_tokens")
                .unwrap_or_default()
        });

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
        .bind(parent_thread_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind(parent_call_id)
        .bind(uncached_tokens)
        .bind(cached_tokens)
        .bind(cache_write_tokens)
        .bind(output_tokens)
        .bind(total_tokens)
        .execute(pool.as_ref())
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
    use codex_protocol::account::PlanType;
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
    use codex_utils_absolute_path::test_support::PathExt;
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
        final_model: Option<String>,
        model_snapshot: Option<String>,
        requested_service_tier: Option<String>,
        actual_service_tier: Option<String>,
        actual_service_tier_source: Option<String>,
        fast_mode_requested: Option<bool>,
        fast_mode_used: Option<bool>,
        billing_surface: Option<String>,
        account_plan: Option<String>,
        input_tokens_uncached: i64,
        input_tokens_cached: i64,
        input_tokens_cache_write: i64,
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

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct CreditEstimateRow {
        provider_call_id: String,
        pricing_model: Option<String>,
        actual_service_tier: Option<String>,
        fast_mode_used: Option<bool>,
        rate_id: Option<String>,
        uncached_input_credits: Option<f64>,
        cached_input_credits: Option<f64>,
        output_credits: Option<f64>,
        rate_card_estimated_total_credits: Option<f64>,
        estimated_total_credits: Option<f64>,
        pricing_status: String,
        credit_source: Option<String>,
    }

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct CreditEstimateStatusRow {
        provider_call_id: String,
        pricing_status: String,
        rate_card_estimated_total_credits: Option<f64>,
        estimated_total_credits: Option<f64>,
        credit_source: Option<String>,
    }

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct CreditThreadSummaryRow {
        provider_call_count: i64,
        priced_call_count: i64,
        unpriced_call_count: i64,
        partial: bool,
        estimated_total_credits: Option<f64>,
        priced_credits_total: Option<f64>,
        models_used: Option<String>,
        service_tiers_used: Option<String>,
    }

    struct TestProviderCall<'a> {
        id: &'a str,
        thread_id: &'a str,
        started_at: &'a str,
        requested_model: Option<&'a str>,
        actual_model: Option<&'a str>,
        actual_tier: Option<&'a str>,
        fast_mode_used: Option<bool>,
        billing_surface: &'a str,
        account_plan: Option<&'a str>,
        uncached: i64,
        cached: i64,
        cache_write: i64,
        output: i64,
        total: i64,
        provider_reported_credits: Option<f64>,
    }

    async fn insert_test_provider_call(
        pool: &SqlitePool,
        call: TestProviderCall<'_>,
    ) -> Result<()> {
        sqlx::query(
            r#"
INSERT INTO usage_provider_calls (
  provider_call_id, thread_id, provider, requested_model, actual_model_used,
  actual_service_tier, actual_service_tier_source, fast_mode_used,
  billing_surface, account_plan, started_at, completed_at,
  input_tokens_uncached, input_tokens_cached, input_tokens_cache_write,
  output_tokens, total_tokens, provider_reported_credits, status
) VALUES (?, ?, 'openai', ?, ?, ?, 'runtime_contract', ?, ?, ?, ?, ?,
          ?, ?, ?, ?, ?, ?, 'ok')
"#,
        )
        .bind(call.id)
        .bind(call.thread_id)
        .bind(call.requested_model)
        .bind(call.actual_model)
        .bind(call.actual_tier)
        .bind(call.fast_mode_used)
        .bind(call.billing_surface)
        .bind(call.account_plan)
        .bind(call.started_at)
        .bind(call.started_at)
        .bind(call.uncached)
        .bind(call.cached)
        .bind(call.cache_write)
        .bind(call.output)
        .bind(call.total)
        .bind(call.provider_reported_credits)
        .execute(pool)
        .await?;
        Ok(())
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
        parent_cumulative_cache_write_tokens: Option<i64>,
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
            cache_write_input_tokens: 3,
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
            plan_type: Some(PlanType::Pro),
            individual_limit: None,
            spend_control_reached: None,
        });
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::TokenCount(TokenCountEvent {
                info: Some(info),
                rate_limits,
                provider: Some("test-provider".to_string()),
                model_used: Some("actual-model".to_string()),
                requested_service_tier: Some("priority".to_string()),
                actual_service_tier: Some("priority".to_string()),
                actual_service_tier_source: Some("runtime_contract".to_string()),
                fast_mode_requested: Some(true),
                fast_mode_used: Some(true),
                billing_surface: Some("chatgpt_credits".to_string()),
                account_plan: include_rate_limit.then(|| "pro".to_string()),
            }),
        }
    }

    async fn init_runtime() -> Result<(Arc<StateRuntime>, TempDir)> {
        let tmp_dir = tempdir()?;
        let runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(tmp_dir.path().abs()),
            "test-provider".to_string(),
        )
        .await?;
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
  final_model,
  model_snapshot,
  requested_service_tier,
  actual_service_tier,
  actual_service_tier_source,
  fast_mode_requested,
  fast_mode_used,
  billing_surface,
  account_plan,
  input_tokens_uncached,
  input_tokens_cached,
  input_tokens_cache_write,
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
                final_model: None,
                model_snapshot: None,
                requested_service_tier: Some("priority".to_string()),
                actual_service_tier: Some("priority".to_string()),
                actual_service_tier_source: Some("runtime_contract".to_string()),
                fast_mode_requested: Some(true),
                fast_mode_used: Some(true),
                billing_surface: Some("chatgpt_credits".to_string()),
                account_plan: Some("pro".to_string()),
                input_tokens_uncached: 8,
                input_tokens_cached: 2,
                input_tokens_cache_write: 3,
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

        assert_eq!(
            runtime
                .latest_usage_provider_display_model(thread_id)
                .await?,
            Some("actual-model".to_string())
        );
        let missing_thread_id = ThreadId::new();
        let display_models = runtime
            .latest_usage_provider_display_models(&[thread_id, missing_thread_id])
            .await?;
        assert_eq!(
            display_models.get(&thread_id),
            Some(&"actual-model".to_string())
        );
        assert!(!display_models.contains_key(&missing_thread_id));

        Ok(())
    }

    #[tokio::test]
    async fn credit_views_select_standard_fast_and_half_open_rates() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();
        for (id, model, tier, fast, started_at) in [
            (
                "luna",
                "gpt-5.6-luna",
                "default",
                false,
                "2026-07-28T00:00:00Z",
            ),
            (
                "terra",
                "gpt-5.6-terra",
                "default",
                false,
                "2026-07-28T00:00:00Z",
            ),
            (
                "sol",
                "gpt-5.6-sol",
                "default",
                false,
                "2026-07-28T00:00:00Z",
            ),
            (
                "gpt55-fast",
                "gpt-5.5",
                "priority",
                true,
                "2026-07-28T00:00:00Z",
            ),
            (
                "gpt54-fast",
                "gpt-5.4",
                "priority",
                true,
                "2026-07-28T00:00:00Z",
            ),
            (
                "fast",
                "gpt-5.6-luna",
                "priority",
                true,
                "2026-07-28T00:00:00Z",
            ),
            (
                "before",
                "gpt-5.6-luna",
                "default",
                false,
                "2026-04-01T23:59:59Z",
            ),
            (
                "boundary",
                "gpt-5.6-luna",
                "default",
                false,
                "2026-04-02T00:00:00Z",
            ),
        ] {
            insert_test_provider_call(
                pool,
                TestProviderCall {
                    id,
                    thread_id: "rates",
                    started_at,
                    requested_model: Some(model),
                    actual_model: Some(model),
                    actual_tier: Some(tier),
                    fast_mode_used: Some(fast),
                    billing_surface: "chatgpt_credits",
                    account_plan: Some("pro"),
                    uncached: 1_000_000,
                    cached: 1_000_000,
                    cache_write: 0,
                    output: 1_000_000,
                    total: 9_999_999,
                    provider_reported_credits: None,
                },
            )
            .await?;
        }
        insert_test_provider_call(
            pool,
            TestProviderCall {
                id: "api-priority",
                thread_id: "rates",
                started_at: "2026-07-28T00:00:00Z",
                requested_model: Some("gpt-5.6-luna"),
                actual_model: Some("gpt-5.6-luna"),
                actual_tier: Some("priority"),
                fast_mode_used: Some(false),
                billing_surface: "api_tokens",
                account_plan: None,
                uncached: 1_000_000,
                cached: 1_000_000,
                cache_write: 0,
                output: 1_000_000,
                total: 3_000_000,
                provider_reported_credits: None,
            },
        )
        .await?;

        let rows: Vec<CreditEstimateRow> = sqlx::query_as(
            r#"
SELECT provider_call_id, pricing_model, actual_service_tier, fast_mode_used,
       rate_id, uncached_input_credits, cached_input_credits, output_credits,
       rate_card_estimated_total_credits, estimated_total_credits,
       pricing_status, credit_source
FROM usage_provider_call_credit_estimates
WHERE thread_id = 'rates'
ORDER BY provider_call_id
"#,
        )
        .fetch_all(pool)
        .await?;
        assert_eq!(
            rows,
            vec![
                CreditEstimateRow {
                    provider_call_id: "api-priority".into(),
                    pricing_model: Some("gpt-5.6-luna".into()),
                    actual_service_tier: Some("priority".into()),
                    fast_mode_used: Some(false),
                    rate_id: None,
                    uncached_input_credits: None,
                    cached_input_credits: None,
                    output_credits: None,
                    rate_card_estimated_total_credits: None,
                    estimated_total_credits: None,
                    pricing_status: "rate_card_unknown".into(),
                    credit_source: None,
                },
                CreditEstimateRow {
                    provider_call_id: "before".into(),
                    pricing_model: Some("gpt-5.6-luna".into()),
                    actual_service_tier: Some("default".into()),
                    fast_mode_used: Some(false),
                    rate_id: None,
                    uncached_input_credits: None,
                    cached_input_credits: None,
                    output_credits: None,
                    rate_card_estimated_total_credits: None,
                    estimated_total_credits: None,
                    pricing_status: "rate_card_unknown".into(),
                    credit_source: None,
                },
                CreditEstimateRow {
                    provider_call_id: "boundary".into(),
                    pricing_model: Some("gpt-5.6-luna".into()),
                    actual_service_tier: Some("default".into()),
                    fast_mode_used: Some(false),
                    rate_id: Some("openai-gpt-5.6-luna-standard-20260402".into()),
                    uncached_input_credits: Some(25.0),
                    cached_input_credits: Some(2.5),
                    output_credits: Some(150.0),
                    rate_card_estimated_total_credits: Some(177.5),
                    estimated_total_credits: Some(177.5),
                    pricing_status: "priced_estimate".into(),
                    credit_source: Some("rate_card_estimate".into()),
                },
                CreditEstimateRow {
                    provider_call_id: "fast".into(),
                    pricing_model: Some("gpt-5.6-luna".into()),
                    actual_service_tier: Some("priority".into()),
                    fast_mode_used: Some(true),
                    rate_id: Some("openai-gpt-5.6-luna-fast-20260727".into()),
                    uncached_input_credits: Some(62.5),
                    cached_input_credits: Some(6.25),
                    output_credits: Some(375.0),
                    rate_card_estimated_total_credits: Some(443.75),
                    estimated_total_credits: Some(443.75),
                    pricing_status: "priced_estimate".into(),
                    credit_source: Some("rate_card_estimate".into()),
                },
                CreditEstimateRow {
                    provider_call_id: "gpt54-fast".into(),
                    pricing_model: Some("gpt-5.4".into()),
                    actual_service_tier: Some("priority".into()),
                    fast_mode_used: Some(true),
                    rate_id: Some("openai-gpt-5.4-fast-20260727".into()),
                    uncached_input_credits: Some(125.0),
                    cached_input_credits: Some(12.5),
                    output_credits: Some(750.0),
                    rate_card_estimated_total_credits: Some(887.5),
                    estimated_total_credits: Some(887.5),
                    pricing_status: "priced_estimate".into(),
                    credit_source: Some("rate_card_estimate".into()),
                },
                CreditEstimateRow {
                    provider_call_id: "gpt55-fast".into(),
                    pricing_model: Some("gpt-5.5".into()),
                    actual_service_tier: Some("priority".into()),
                    fast_mode_used: Some(true),
                    rate_id: Some("openai-gpt-5.5-fast-20260727".into()),
                    uncached_input_credits: Some(312.5),
                    cached_input_credits: Some(31.25),
                    output_credits: Some(1875.0),
                    rate_card_estimated_total_credits: Some(2218.75),
                    estimated_total_credits: Some(2218.75),
                    pricing_status: "priced_estimate".into(),
                    credit_source: Some("rate_card_estimate".into()),
                },
                CreditEstimateRow {
                    provider_call_id: "luna".into(),
                    pricing_model: Some("gpt-5.6-luna".into()),
                    actual_service_tier: Some("default".into()),
                    fast_mode_used: Some(false),
                    rate_id: Some("openai-gpt-5.6-luna-standard-20260402".into()),
                    uncached_input_credits: Some(25.0),
                    cached_input_credits: Some(2.5),
                    output_credits: Some(150.0),
                    rate_card_estimated_total_credits: Some(177.5),
                    estimated_total_credits: Some(177.5),
                    pricing_status: "priced_estimate".into(),
                    credit_source: Some("rate_card_estimate".into()),
                },
                CreditEstimateRow {
                    provider_call_id: "sol".into(),
                    pricing_model: Some("gpt-5.6-sol".into()),
                    actual_service_tier: Some("default".into()),
                    fast_mode_used: Some(false),
                    rate_id: Some("openai-gpt-5.6-sol-standard-20260402".into()),
                    uncached_input_credits: Some(125.0),
                    cached_input_credits: Some(12.5),
                    output_credits: Some(750.0),
                    rate_card_estimated_total_credits: Some(887.5),
                    estimated_total_credits: Some(887.5),
                    pricing_status: "priced_estimate".into(),
                    credit_source: Some("rate_card_estimate".into()),
                },
                CreditEstimateRow {
                    provider_call_id: "terra".into(),
                    pricing_model: Some("gpt-5.6-terra".into()),
                    actual_service_tier: Some("default".into()),
                    fast_mode_used: Some(false),
                    rate_id: Some("openai-gpt-5.6-terra-standard-20260402".into()),
                    uncached_input_credits: Some(62.5),
                    cached_input_credits: Some(6.25),
                    output_credits: Some(375.0),
                    rate_card_estimated_total_credits: Some(443.75),
                    estimated_total_credits: Some(443.75),
                    pricing_status: "priced_estimate".into(),
                    credit_source: Some("rate_card_estimate".into()),
                },
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn credit_views_preserve_uncertainty_and_partial_totals() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();
        for call in [
            TestProviderCall {
                id: "priced",
                thread_id: "partial",
                started_at: "2026-07-28T00:00:00Z",
                requested_model: Some("gpt-5.6-luna"),
                actual_model: Some("gpt-5.6-luna"),
                actual_tier: Some("default"),
                fast_mode_used: Some(false),
                billing_surface: "chatgpt_credits",
                account_plan: Some("pro"),
                uncached: 1_000_000,
                cached: 0,
                cache_write: 0,
                output: 0,
                total: 99,
                provider_reported_credits: None,
            },
            TestProviderCall {
                id: "missing-model",
                thread_id: "partial",
                started_at: "2026-07-28T00:00:00Z",
                requested_model: Some("gpt-5.6-luna"),
                actual_model: None,
                actual_tier: Some("default"),
                fast_mode_used: Some(false),
                billing_surface: "chatgpt_credits",
                account_plan: Some("pro"),
                uncached: 1,
                cached: 0,
                cache_write: 0,
                output: 0,
                total: 1,
                provider_reported_credits: None,
            },
            TestProviderCall {
                id: "missing-tier",
                thread_id: "partial",
                started_at: "2026-07-28T00:00:00Z",
                requested_model: Some("gpt-5.6-terra"),
                actual_model: Some("gpt-5.6-terra"),
                actual_tier: None,
                fast_mode_used: Some(false),
                billing_surface: "chatgpt_credits",
                account_plan: Some("pro"),
                uncached: 1,
                cached: 0,
                cache_write: 0,
                output: 0,
                total: 1,
                provider_reported_credits: None,
            },
            TestProviderCall {
                id: "unknown-model",
                thread_id: "partial",
                started_at: "2026-07-28T00:00:00Z",
                requested_model: Some("unknown"),
                actual_model: Some("unknown"),
                actual_tier: Some("default"),
                fast_mode_used: Some(false),
                billing_surface: "chatgpt_credits",
                account_plan: Some("pro"),
                uncached: 1,
                cached: 0,
                cache_write: 0,
                output: 0,
                total: 1,
                provider_reported_credits: None,
            },
            TestProviderCall {
                id: "unknown-tier",
                thread_id: "partial",
                started_at: "2026-07-28T00:00:00Z",
                requested_model: Some("gpt-5.6-sol"),
                actual_model: Some("gpt-5.6-sol"),
                actual_tier: Some("flex"),
                fast_mode_used: Some(false),
                billing_surface: "chatgpt_credits",
                account_plan: Some("pro"),
                uncached: 1,
                cached: 0,
                cache_write: 0,
                output: 0,
                total: 1,
                provider_reported_credits: None,
            },
            TestProviderCall {
                id: "unknown-fast-mode",
                thread_id: "partial",
                started_at: "2026-07-28T00:00:00Z",
                requested_model: Some("gpt-5.6-luna"),
                actual_model: Some("gpt-5.6-luna"),
                actual_tier: Some("default"),
                fast_mode_used: None,
                billing_surface: "chatgpt_credits",
                account_plan: Some("pro"),
                uncached: 1_000_000,
                cached: 0,
                cache_write: 0,
                output: 0,
                total: 1_000_000,
                provider_reported_credits: None,
            },
            TestProviderCall {
                id: "cache-write",
                thread_id: "partial",
                started_at: "2026-07-28T00:00:00Z",
                requested_model: Some("gpt-5.6-luna"),
                actual_model: Some("gpt-5.6-luna"),
                actual_tier: Some("default"),
                fast_mode_used: Some(false),
                billing_surface: "chatgpt_credits",
                account_plan: Some("pro"),
                uncached: 1_000_000,
                cached: 0,
                cache_write: 50,
                output: 0,
                total: 1_000_050,
                provider_reported_credits: None,
            },
            TestProviderCall {
                id: "reported",
                thread_id: "partial",
                started_at: "2026-07-28T00:00:00Z",
                requested_model: Some("gpt-5.6-luna"),
                actual_model: Some("gpt-5.6-luna"),
                actual_tier: Some("default"),
                fast_mode_used: Some(false),
                billing_surface: "chatgpt_credits",
                account_plan: Some("pro"),
                uncached: 1_000_000,
                cached: 0,
                cache_write: 0,
                output: 0,
                total: 1_000_000,
                provider_reported_credits: Some(9.0),
            },
        ] {
            insert_test_provider_call(pool, call).await?;
        }

        let statuses: Vec<CreditEstimateStatusRow> = sqlx::query_as(
            r#"
SELECT provider_call_id, pricing_status, rate_card_estimated_total_credits,
       estimated_total_credits, credit_source
FROM usage_provider_call_credit_estimates
WHERE thread_id = 'partial'
ORDER BY provider_call_id
"#,
        )
        .fetch_all(pool)
        .await?;
        assert_eq!(
            statuses,
            vec![
                CreditEstimateStatusRow {
                    provider_call_id: "cache-write".into(),
                    pricing_status: "token_breakdown_incomplete".into(),
                    rate_card_estimated_total_credits: None,
                    estimated_total_credits: None,
                    credit_source: None,
                },
                CreditEstimateStatusRow {
                    provider_call_id: "missing-model".into(),
                    pricing_status: "actual_model_missing".into(),
                    rate_card_estimated_total_credits: None,
                    estimated_total_credits: None,
                    credit_source: None,
                },
                CreditEstimateStatusRow {
                    provider_call_id: "missing-tier".into(),
                    pricing_status: "actual_tier_missing".into(),
                    rate_card_estimated_total_credits: None,
                    estimated_total_credits: None,
                    credit_source: None,
                },
                CreditEstimateStatusRow {
                    provider_call_id: "priced".into(),
                    pricing_status: "priced_estimate".into(),
                    rate_card_estimated_total_credits: Some(25.0),
                    estimated_total_credits: Some(25.0),
                    credit_source: Some("rate_card_estimate".into()),
                },
                CreditEstimateStatusRow {
                    provider_call_id: "reported".into(),
                    pricing_status: "provider_reported".into(),
                    rate_card_estimated_total_credits: Some(25.0),
                    estimated_total_credits: Some(9.0),
                    credit_source: Some("provider_reported".into()),
                },
                CreditEstimateStatusRow {
                    provider_call_id: "unknown-fast-mode".into(),
                    pricing_status: "fast_rate_unknown".into(),
                    rate_card_estimated_total_credits: None,
                    estimated_total_credits: None,
                    credit_source: None,
                },
                CreditEstimateStatusRow {
                    provider_call_id: "unknown-model".into(),
                    pricing_status: "model_rate_missing".into(),
                    rate_card_estimated_total_credits: None,
                    estimated_total_credits: None,
                    credit_source: None,
                },
                CreditEstimateStatusRow {
                    provider_call_id: "unknown-tier".into(),
                    pricing_status: "tier_rate_missing".into(),
                    rate_card_estimated_total_credits: None,
                    estimated_total_credits: None,
                    credit_source: None,
                },
            ]
        );

        let summary: CreditThreadSummaryRow = sqlx::query_as(
            r#"
SELECT provider_call_count, priced_call_count, unpriced_call_count, partial,
       estimated_total_credits, priced_credits_total, models_used, service_tiers_used
FROM usage_thread_credit_summary
WHERE thread_id = 'partial'
"#,
        )
        .fetch_one(pool)
        .await?;
        assert_eq!(summary.provider_call_count, 8);
        assert_eq!(summary.priced_call_count, 2);
        assert_eq!(summary.unpriced_call_count, 6);
        assert!(summary.partial);
        assert_eq!(summary.estimated_total_credits, None);
        assert_eq!(summary.priced_credits_total, Some(34.0));

        let mut models = summary
            .models_used
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .collect::<Vec<_>>();
        models.sort_unstable();
        assert_eq!(
            models,
            vec!["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra", "unknown"]
        );
        let mut service_tiers = summary
            .service_tiers_used
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .collect::<Vec<_>>();
        service_tiers.sort_unstable();
        assert_eq!(service_tiers, vec!["default", "flex"]);

        let overlap = sqlx::query(
            r#"
INSERT INTO usage_codex_credit_rates (
  rate_id, provider, model, service_tier, speed_mode, rate_card_kind,
  credits_per_1m_uncached_input, credits_per_1m_cached_input,
  credits_per_1m_output, effective_from, source_url, source_observed_at
) VALUES (
  'overlap', 'openai', 'gpt-5.6-luna', 'default', 'standard',
  'codex_token_based', 1, 1, 1, '2026-07-01T00:00:00Z',
  'https://example.invalid', '2026-07-27T00:00:00Z'
)
"#,
        )
        .execute(pool)
        .await
        .expect_err("overlapping rate intervals must fail");
        assert!(
            overlap
                .to_string()
                .contains("ambiguous Codex credit rate interval")
        );
        Ok(())
    }

    #[tokio::test]
    async fn usage_logger_records_provider_final_model_identity() -> Result<()> {
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

        let turn_id = "turn-identity";
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
                    started_at: None,
                    last_agent_message: None,
                    error: None,
                    compaction_events_in_turn: 0,
                    final_model: Some("provider-final-model".to_string()),
                    model_snapshot: Some("provider-model-snapshot".to_string()),
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                }),
            })
            .await;

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();
        let provider_row: ProviderCallRow = sqlx::query_as(
            r#"
SELECT
  provider,
  requested_model,
  actual_model_used,
  final_model,
  model_snapshot,
  requested_service_tier,
  actual_service_tier,
  actual_service_tier_source,
  fast_mode_requested,
  fast_mode_used,
  billing_surface,
  account_plan,
  input_tokens_uncached,
  input_tokens_cached,
  input_tokens_cache_write,
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
                final_model: Some("provider-final-model".to_string()),
                model_snapshot: Some("provider-model-snapshot".to_string()),
                requested_service_tier: Some("priority".to_string()),
                actual_service_tier: Some("priority".to_string()),
                actual_service_tier_source: Some("runtime_contract".to_string()),
                fast_mode_requested: Some(true),
                fast_mode_used: Some(true),
                billing_surface: Some("chatgpt_credits".to_string()),
                account_plan: None,
                input_tokens_uncached: 8,
                input_tokens_cached: 2,
                input_tokens_cache_write: 3,
                output_tokens: 3,
                total_tokens: 16,
                status: Some("ok".to_string()),
            }
        );

        assert_eq!(
            runtime
                .latest_usage_provider_display_model(thread_id)
                .await?,
            Some("provider-final-model".to_string())
        );
        let missing_thread_id = ThreadId::new();
        let display_models = runtime
            .latest_usage_provider_display_models(&[thread_id, missing_thread_id])
            .await?;
        assert_eq!(
            display_models.get(&thread_id),
            Some(&"provider-final-model".to_string())
        );
        assert!(!display_models.contains_key(&missing_thread_id));

        Ok(())
    }

    #[tokio::test]
    async fn usage_logger_records_thread_source_marker() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let parent_thread_id = ThreadId::new();
        let side_thread_id = ThreadId::new();
        let _logger = UsageLogger::try_new_with_thread_source(
            runtime.clone(),
            side_thread_id,
            SessionSource::Cli,
            Some(ThreadSource::Side),
            Some(parent_thread_id),
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();
        let row: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            r#"
SELECT
  root_thread_id,
  fork_parent_thread_id,
  thread_source
FROM usage_threads
WHERE thread_id = ?
"#,
        )
        .bind(side_thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            row,
            (
                Some(parent_thread_id.to_string()),
                Some(parent_thread_id.to_string()),
                Some("side".to_string())
            )
        );

        Ok(())
    }

    #[tokio::test]
    async fn usage_lineage_telemetry_keeps_subagents_out_of_resume_recommendations() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let root_thread_id = ThreadId::new();
        let mut root_logger = UsageLogger::try_new_with_thread_source(
            runtime.clone(),
            root_thread_id,
            SessionSource::Cli,
            Some(ThreadSource::User),
            None,
            None,
            None,
        )
        .await?;
        root_logger
            .record_event(&token_count_event("turn-root", true))
            .await;

        let child_thread_id = ThreadId::new();
        let _child_logger = UsageLogger::try_new_with_thread_source(
            runtime.clone(),
            child_thread_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_thread_id,
                depth: 1,
                agent_nickname: Some("Child".to_string()),
                agent_role: Some("explorer".to_string()),
                agent_path: None,
            }),
            Some(ThreadSource::Subagent),
            None,
            Some("Child".to_string()),
            Some("explorer".to_string()),
        )
        .await?;

        let side_thread_id = ThreadId::new();
        let _side_logger = UsageLogger::try_new_with_thread_source(
            runtime.clone(),
            side_thread_id,
            SessionSource::Cli,
            Some(ThreadSource::Side),
            Some(root_thread_id),
            None,
            None,
        )
        .await?;

        let telemetry = runtime
            .get_usage_lineage_telemetry(&root_thread_id.to_string())
            .await?
            .expect("the root usage thread should exist");
        assert_eq!(telemetry.root_thread_id, root_thread_id.to_string());
        assert_eq!(telemetry.threads.len(), 3);
        assert_eq!(
            telemetry.recommended_user_resume_thread_id,
            Some(root_thread_id.to_string()),
            "only direct user threads are eligible for the advisory continuation"
        );
        let child = telemetry
            .threads
            .iter()
            .find(|thread| thread.thread_id == child_thread_id.to_string())
            .expect("child should be included in the root family");
        assert_eq!(child.lineage_edge_kind, "agent_spawn");
        assert_eq!(child.lineage_confidence, "persisted");
        assert_eq!(child.agent_role.as_deref(), Some("explorer"));
        let root = telemetry
            .threads
            .iter()
            .find(|thread| thread.thread_id == root_thread_id.to_string())
            .expect("root should be included in the root family");
        assert_eq!(root.provider_call_count, 1);
        assert_eq!(root.total_tokens, 16);

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
                    started_at: None,
                    last_agent_message: None,
                    error: None,
                    compaction_events_in_turn: 0,
                    final_model: None,
                    model_snapshot: None,
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
  final_model,
  model_snapshot,
  requested_service_tier,
  actual_service_tier,
  actual_service_tier_source,
  fast_mode_requested,
  fast_mode_used,
  billing_surface,
  account_plan,
  input_tokens_uncached,
  input_tokens_cached,
  input_tokens_cache_write,
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
                    final_model: None,
                    model_snapshot: None,
                    requested_service_tier: Some("priority".to_string()),
                    actual_service_tier: Some("priority".to_string()),
                    actual_service_tier_source: Some("runtime_contract".to_string()),
                    fast_mode_requested: Some(true),
                    fast_mode_used: Some(true),
                    billing_surface: Some("chatgpt_credits".to_string()),
                    account_plan: None,
                    input_tokens_uncached: 8,
                    input_tokens_cached: 2,
                    input_tokens_cache_write: 3,
                    output_tokens: 3,
                    total_tokens: 16,
                    status: Some("ok".to_string()),
                },
                ProviderCallRow {
                    provider: Some("test-provider".to_string()),
                    requested_model: Some("actual-model".to_string()),
                    actual_model_used: Some("actual-model".to_string()),
                    final_model: None,
                    model_snapshot: None,
                    requested_service_tier: Some("priority".to_string()),
                    actual_service_tier: Some("priority".to_string()),
                    actual_service_tier_source: Some("runtime_contract".to_string()),
                    fast_mode_requested: Some(true),
                    fast_mode_used: Some(true),
                    billing_surface: Some("chatgpt_credits".to_string()),
                    account_plan: None,
                    input_tokens_uncached: 8,
                    input_tokens_cached: 2,
                    input_tokens_cache_write: 3,
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
                connector_id: None,
                mcp_app_resource_uri: None,
                link_id: None,
                app_name: None,
                action_name: None,
                plugin_id: None,
            }),
        };
        logger.record_event(&tool_begin).await;

        let tool_end = Event {
            id: turn_id.to_string(),
            msg: EventMsg::McpToolCallEnd(McpToolCallEndEvent {
                call_id: tool_call_id.to_string(),
                invocation: tool_invocation,
                connector_id: None,
                mcp_app_resource_uri: None,
                link_id: None,
                app_name: None,
                action_name: None,
                plugin_id: None,
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
                started_at_ms: 0,
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
                completed_at_ms: 0,
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
  parent_cumulative_cache_write_tokens,
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
                parent_cumulative_cache_write_tokens: Some(3),
                parent_cumulative_output_tokens: Some(3),
                parent_cumulative_total_tokens: Some(16),
            }
        );

        Ok(())
    }

    #[tokio::test]
    async fn state_runtime_records_direct_fork_snapshot_from_provider_rows() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let parent_thread_id = ThreadId::new();
        let child_thread_id = ThreadId::new();
        let mut parent_logger = UsageLogger::try_new(
            runtime.clone(),
            parent_thread_id,
            SessionSource::Cli,
            /*forked_from_id*/ None,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;
        parent_logger
            .record_event(&token_count_event(
                "turn-direct-fork",
                /*include_rate_limit*/ false,
            ))
            .await;

        runtime
            .record_usage_fork_snapshot(child_thread_id, parent_thread_id)
            .await?;

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();
        let mut fork_row: ForkSnapshotRow = sqlx::query_as(
            r#"
SELECT
  parent_thread_id,
  parent_last_provider_call_id,
  parent_cumulative_uncached_tokens,
  parent_cumulative_cached_tokens,
  parent_cumulative_cache_write_tokens,
  parent_cumulative_output_tokens,
  parent_cumulative_total_tokens
FROM usage_fork_snapshots
WHERE child_thread_id = ?
"#,
        )
        .bind(child_thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert!(
            fork_row.parent_last_provider_call_id.is_some(),
            "expected provider call id in direct fork snapshot"
        );
        fork_row.parent_last_provider_call_id = Some("<provider_call_id>".to_string());
        assert_eq!(
            fork_row,
            ForkSnapshotRow {
                parent_thread_id: parent_thread_id.to_string(),
                parent_last_provider_call_id: Some("<provider_call_id>".to_string()),
                parent_cumulative_uncached_tokens: Some(8),
                parent_cumulative_cached_tokens: Some(2),
                parent_cumulative_cache_write_tokens: Some(3),
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
                    started_at_ms: 0,
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
                    completed_at_ms: 0,
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
            let runtime = StateRuntime::init(
                crate::SqliteConfig::new_for_testing(tmp_dir.path().abs()),
                "test-provider".to_string(),
            )
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

        let reopened_runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(tmp_dir.path().abs()),
            "test-provider".to_string(),
        )
        .await?;
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
                started_at_ms: 0,
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
                completed_at_ms: 0,
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

        let lineage_row: (Option<String>, Option<String>) = sqlx::query_as(
            r#"SELECT spawn_request_id, lineage_edge_kind
FROM usage_threads
WHERE thread_id = ?"#,
        )
        .bind(child_thread_id.to_string())
        .fetch_one(pool)
        .await?;
        assert_eq!(
            lineage_row,
            (
                Some(spawn_call.to_string()),
                Some("agent_spawn".to_string())
            ),
            "the child logger should retain the concrete spawn request without a reverse lookup"
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
