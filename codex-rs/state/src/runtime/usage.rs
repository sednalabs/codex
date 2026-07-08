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
use serde::Serialize;
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
    output_tokens: i64,
    total_tokens: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageThreadRecord {
    pub root_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>,
    pub thread_source: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageAccountContext {
    pub auth_mode: Option<String>,
    pub account_id_hash: Option<String>,
    pub chatgpt_user_id_hash: Option<String>,
    pub account_plan_type: Option<String>,
    pub codex_home_hash: Option<String>,
    pub sqlite_home_hash: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct UsageRateLimitSnapshotRecord<'a> {
    pub thread_id: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub observed_from: &'a str,
    pub account: &'a UsageAccountContext,
    pub rate_limits: &'a [RateLimitSnapshot],
    pub reset_credits_available_count: Option<i64>,
    pub reset_credits_json: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
pub struct UsageRateLimitResetCreditEventRecord<'a> {
    pub event_type: &'a str,
    pub account: &'a UsageAccountContext,
    pub idempotency_key: &'a str,
    pub credit_id: Option<&'a str>,
    pub outcome: Option<&'a str>,
    pub status: &'a str,
    pub error: Option<&'a str>,
    pub metadata_json: Option<&'a str>,
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
        sqlx::query(
            r#"
INSERT INTO usage_threads (thread_id, parent_thread_id, root_thread_id, fork_parent_thread_id, agent_nickname, agent_role, source, thread_source, created_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id) DO UPDATE SET
    parent_thread_id = COALESCE(excluded.parent_thread_id, usage_threads.parent_thread_id),
    root_thread_id = COALESCE(excluded.root_thread_id, usage_threads.root_thread_id),
    fork_parent_thread_id = COALESCE(excluded.fork_parent_thread_id, usage_threads.fork_parent_thread_id),
    agent_nickname = COALESCE(excluded.agent_nickname, usage_threads.agent_nickname),
    agent_role = COALESCE(excluded.agent_role, usage_threads.agent_role),
    source = excluded.source,
    thread_source = COALESCE(excluded.thread_source, usage_threads.thread_source)
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
        let account = UsageAccountContext::default();
        let thread_id = self.thread_id.to_string();
        if let Err(err) = insert_rate_limit_snapshot_rows(
            self.pool.as_ref(),
            UsageRateLimitSnapshotRecord {
                thread_id: Some(thread_id.as_str()),
                turn_id,
                observed_from: "token_count",
                account: &account,
                rate_limits: std::slice::from_ref(snapshot),
                reset_credits_available_count: None,
                reset_credits_json: None,
            },
        )
        .await
        {
            warn!("usage rich rate-limit snapshot: {err}");
        }

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

async fn insert_rate_limit_snapshot_rows(
    pool: &SqlitePool,
    record: UsageRateLimitSnapshotRecord<'_>,
) -> anyhow::Result<()> {
    if record.rate_limits.is_empty() {
        return Ok(());
    }
    let observed_from = non_empty_or_unknown(record.observed_from);
    for snapshot in record.rate_limits {
        let primary = snapshot.primary.as_ref();
        let secondary = snapshot.secondary.as_ref();
        let credits = snapshot.credits.as_ref();
        let individual_limit = snapshot.individual_limit.as_ref();
        let plan = snapshot.plan_type.as_ref().and_then(serialized_enum_string);
        let rate_limit_reached_type = snapshot
            .rate_limit_reached_type
            .as_ref()
            .and_then(serialized_enum_string);
        let snapshot_json = serialized_json_string(snapshot);
        sqlx::query(
            r#"INSERT INTO usage_rate_limit_snapshots (
            snapshot_id,
            thread_id,
            turn_id,
            observed_from,
            auth_mode,
            account_id_hash,
            chatgpt_user_id_hash,
            account_plan_type,
            codex_home_hash,
            sqlite_home_hash,
            limit_id,
            limit_name,
            primary_used_percent,
            primary_window_minutes,
            primary_resets_at,
            secondary_used_percent,
            secondary_window_minutes,
            secondary_resets_at,
            credits_has_credits,
            credits_unlimited,
            credits_balance,
            individual_limit_limit,
            individual_limit_used,
            individual_limit_remaining_percent,
            individual_limit_resets_at,
            plan,
            rate_limit_reached_type,
            reset_credits_available_count,
            reset_credits_json,
            snapshot_json
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(record.thread_id)
        .bind(record.turn_id)
        .bind(observed_from)
        .bind(record.account.auth_mode.as_deref())
        .bind(record.account.account_id_hash.as_deref())
        .bind(record.account.chatgpt_user_id_hash.as_deref())
        .bind(record.account.account_plan_type.as_deref())
        .bind(record.account.codex_home_hash.as_deref())
        .bind(record.account.sqlite_home_hash.as_deref())
        .bind(snapshot.limit_id.as_deref())
        .bind(snapshot.limit_name.as_deref())
        .bind(primary.map(|window| window.used_percent))
        .bind(primary.and_then(|window| window.window_minutes))
        .bind(primary.and_then(|window| window.resets_at))
        .bind(secondary.map(|window| window.used_percent))
        .bind(secondary.and_then(|window| window.window_minutes))
        .bind(secondary.and_then(|window| window.resets_at))
        .bind(credits.map(|value| bool_to_sql(value.has_credits)))
        .bind(credits.map(|value| bool_to_sql(value.unlimited)))
        .bind(credits.and_then(|value| value.balance.as_deref()))
        .bind(individual_limit.map(|value| value.limit.as_str()))
        .bind(individual_limit.map(|value| value.used.as_str()))
        .bind(individual_limit.map(|value| value.remaining_percent))
        .bind(individual_limit.map(|value| value.resets_at))
        .bind(plan.as_deref())
        .bind(rate_limit_reached_type.as_deref())
        .bind(record.reset_credits_available_count)
        .bind(record.reset_credits_json)
        .bind(snapshot_json.as_deref())
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn insert_rate_limit_reset_credit_event(
    pool: &SqlitePool,
    record: UsageRateLimitResetCreditEventRecord<'_>,
) -> anyhow::Result<()> {
    let event_type = non_empty_or_unknown(record.event_type);
    let status = non_empty_or_unknown(record.status);
    sqlx::query(
        r#"INSERT INTO usage_rate_limit_reset_credit_events (
            event_id,
            event_type,
            auth_mode,
            account_id_hash,
            chatgpt_user_id_hash,
            account_plan_type,
            codex_home_hash,
            sqlite_home_hash,
            idempotency_key,
            credit_id,
            outcome,
            status,
            error,
            metadata_json
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(event_type)
    .bind(record.account.auth_mode.as_deref())
    .bind(record.account.account_id_hash.as_deref())
    .bind(record.account.chatgpt_user_id_hash.as_deref())
    .bind(record.account.account_plan_type.as_deref())
    .bind(record.account.codex_home_hash.as_deref())
    .bind(record.account.sqlite_home_hash.as_deref())
    .bind(record.idempotency_key)
    .bind(record.credit_id)
    .bind(record.outcome)
    .bind(status)
    .bind(record.error)
    .bind(record.metadata_json)
    .execute(pool)
    .await?;
    Ok(())
}

fn non_empty_or_unknown(value: &str) -> &str {
    if value.is_empty() { "unknown" } else { value }
}

fn bool_to_sql(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn serialized_enum_string<T: Serialize>(value: &T) -> Option<String> {
    match serde_json::to_value(value).ok()? {
        serde_json::Value::String(value) => Some(value),
        _ => None,
    }
}

fn serialized_json_string<T: Serialize>(value: &T) -> Option<String> {
    serde_json::to_string(value).ok()
}

impl StateRuntime {
    pub async fn record_usage_rate_limit_snapshots(
        &self,
        record: UsageRateLimitSnapshotRecord<'_>,
    ) -> anyhow::Result<()> {
        let pool = self.usage_ledger_pool();
        insert_rate_limit_snapshot_rows(pool.as_ref(), record).await
    }

    pub async fn record_usage_rate_limit_reset_credit_event(
        &self,
        record: UsageRateLimitResetCreditEventRecord<'_>,
    ) -> anyhow::Result<()> {
        let pool = self.usage_ledger_pool();
        insert_rate_limit_reset_credit_event(pool.as_ref(), record).await
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
        .bind(parent_thread_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind(parent_call_id)
        .bind(uncached_tokens)
        .bind(cached_tokens)
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
    use codex_protocol::account::PlanType as AccountPlanType;
    use codex_protocol::mcp::CallToolResult;
    use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::CollabAgentSpawnBeginEvent;
    use codex_protocol::protocol::CollabAgentSpawnEndEvent;
    use codex_protocol::protocol::CreditsSnapshot;
    use codex_protocol::protocol::Event;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::McpInvocation;
    use codex_protocol::protocol::McpToolCallBeginEvent;
    use codex_protocol::protocol::McpToolCallEndEvent;
    use codex_protocol::protocol::RateLimitReachedType;
    use codex_protocol::protocol::RateLimitSnapshot;
    use codex_protocol::protocol::RateLimitWindow;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SpendControlLimitSnapshot;
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
        final_model: Option<String>,
        model_snapshot: Option<String>,
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

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct RichRateLimitSnapshotRow {
        observed_from: String,
        auth_mode: Option<String>,
        account_id_hash: Option<String>,
        chatgpt_user_id_hash: Option<String>,
        account_plan_type: Option<String>,
        limit_id: Option<String>,
        limit_name: Option<String>,
        primary_used_percent: Option<f64>,
        secondary_used_percent: Option<f64>,
        credits_has_credits: Option<i64>,
        credits_unlimited: Option<i64>,
        credits_balance: Option<String>,
        individual_limit_remaining_percent: Option<i64>,
        plan: Option<String>,
        rate_limit_reached_type: Option<String>,
        reset_credits_available_count: Option<i64>,
        reset_credits_json: Option<String>,
        snapshot_json: Option<String>,
    }

    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
    struct ResetCreditEventRow {
        event_type: String,
        auth_mode: Option<String>,
        account_id_hash: Option<String>,
        chatgpt_user_id_hash: Option<String>,
        account_plan_type: Option<String>,
        idempotency_key: String,
        credit_id: Option<String>,
        outcome: Option<String>,
        status: String,
        error: Option<String>,
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

    fn sample_rate_limit_snapshot() -> RateLimitSnapshot {
        RateLimitSnapshot {
            limit_id: Some("codex".to_string()),
            limit_name: Some("primary".to_string()),
            primary: Some(RateLimitWindow {
                used_percent: 12.5,
                window_minutes: Some(60),
                resets_at: Some(0),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 34.5,
                window_minutes: Some(1440),
                resets_at: Some(3600),
            }),
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance: Some("12.34".to_string()),
            }),
            individual_limit: Some(SpendControlLimitSnapshot {
                limit: "100.00".to_string(),
                used: "25.00".to_string(),
                remaining_percent: 75,
                resets_at: 7200,
            }),
            plan_type: Some(AccountPlanType::Pro),
            rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
        }
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
        let rate_limits = include_rate_limit.then_some(sample_rate_limit_snapshot());
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
  final_model,
  model_snapshot,
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
                final_model: None,
                model_snapshot: None,
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

        let rich_row: RichRateLimitSnapshotRow = sqlx::query_as(
            r#"
SELECT
  observed_from,
  auth_mode,
  account_id_hash,
  chatgpt_user_id_hash,
  account_plan_type,
  limit_id,
  limit_name,
  primary_used_percent,
  secondary_used_percent,
  credits_has_credits,
  credits_unlimited,
  credits_balance,
  individual_limit_remaining_percent,
  plan,
  rate_limit_reached_type,
  reset_credits_available_count,
  reset_credits_json,
  snapshot_json
FROM usage_rate_limit_snapshots
WHERE thread_id = ?
"#,
        )
        .bind(thread_id.to_string())
        .fetch_one(pool)
        .await?;
        let snapshot_json = rich_row.snapshot_json.clone();
        assert_eq!(
            rich_row,
            RichRateLimitSnapshotRow {
                observed_from: "token_count".to_string(),
                auth_mode: None,
                account_id_hash: None,
                chatgpt_user_id_hash: None,
                account_plan_type: None,
                limit_id: Some("codex".to_string()),
                limit_name: Some("primary".to_string()),
                primary_used_percent: Some(12.5),
                secondary_used_percent: Some(34.5),
                credits_has_credits: Some(1),
                credits_unlimited: Some(0),
                credits_balance: Some("12.34".to_string()),
                individual_limit_remaining_percent: Some(75),
                plan: Some("pro".to_string()),
                rate_limit_reached_type: Some("rate_limit_reached".to_string()),
                reset_credits_available_count: None,
                reset_credits_json: None,
                snapshot_json: snapshot_json.clone(),
            }
        );
        let snapshot_json = snapshot_json.as_deref().unwrap_or_default();
        assert!(snapshot_json.contains(r#""limit_id":"codex""#));

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
    async fn state_runtime_records_rate_limit_reset_audit_rows() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();
        let account = UsageAccountContext {
            auth_mode: Some("chatgpt".to_string()),
            account_id_hash: Some("account-hash".to_string()),
            chatgpt_user_id_hash: Some("user-hash".to_string()),
            account_plan_type: Some("pro".to_string()),
            codex_home_hash: Some("codex-home-hash".to_string()),
            sqlite_home_hash: Some("sqlite-home-hash".to_string()),
        };
        let snapshots = vec![sample_rate_limit_snapshot()];
        runtime
            .record_usage_rate_limit_snapshots(UsageRateLimitSnapshotRecord {
                thread_id: None,
                turn_id: None,
                observed_from: "account_rate_limits",
                account: &account,
                rate_limits: snapshots.as_slice(),
                reset_credits_available_count: Some(4),
                reset_credits_json: Some(r#"{"availableCount":4}"#),
            })
            .await?;
        runtime
            .record_usage_rate_limit_reset_credit_event(UsageRateLimitResetCreditEventRecord {
                event_type: "consume",
                account: &account,
                idempotency_key: "redeem-1",
                credit_id: Some("credit-1"),
                outcome: Some("reset"),
                status: "success",
                error: None,
                metadata_json: None,
            })
            .await?;

        let rich_row: RichRateLimitSnapshotRow = sqlx::query_as(
            r#"
SELECT
  observed_from,
  auth_mode,
  account_id_hash,
  chatgpt_user_id_hash,
  account_plan_type,
  limit_id,
  limit_name,
  primary_used_percent,
  secondary_used_percent,
  credits_has_credits,
  credits_unlimited,
  credits_balance,
  individual_limit_remaining_percent,
  plan,
  rate_limit_reached_type,
  reset_credits_available_count,
  reset_credits_json,
  snapshot_json
FROM usage_rate_limit_snapshots
WHERE account_id_hash = ?
"#,
        )
        .bind("account-hash")
        .fetch_one(pool)
        .await?;
        let snapshot_json = rich_row.snapshot_json.clone();
        assert_eq!(
            rich_row,
            RichRateLimitSnapshotRow {
                observed_from: "account_rate_limits".to_string(),
                auth_mode: Some("chatgpt".to_string()),
                account_id_hash: Some("account-hash".to_string()),
                chatgpt_user_id_hash: Some("user-hash".to_string()),
                account_plan_type: Some("pro".to_string()),
                limit_id: Some("codex".to_string()),
                limit_name: Some("primary".to_string()),
                primary_used_percent: Some(12.5),
                secondary_used_percent: Some(34.5),
                credits_has_credits: Some(1),
                credits_unlimited: Some(0),
                credits_balance: Some("12.34".to_string()),
                individual_limit_remaining_percent: Some(75),
                plan: Some("pro".to_string()),
                rate_limit_reached_type: Some("rate_limit_reached".to_string()),
                reset_credits_available_count: Some(4),
                reset_credits_json: Some(r#"{"availableCount":4}"#.to_string()),
                snapshot_json,
            }
        );

        let event_row: ResetCreditEventRow = sqlx::query_as(
            r#"
SELECT
  event_type,
  auth_mode,
  account_id_hash,
  chatgpt_user_id_hash,
  account_plan_type,
  idempotency_key,
  credit_id,
  outcome,
  status,
  error
FROM usage_rate_limit_reset_credit_events
WHERE idempotency_key = ?
"#,
        )
        .bind("redeem-1")
        .fetch_one(pool)
        .await?;
        assert_eq!(
            event_row,
            ResetCreditEventRow {
                event_type: "consume".to_string(),
                auth_mode: Some("chatgpt".to_string()),
                account_id_hash: Some("account-hash".to_string()),
                chatgpt_user_id_hash: Some("user-hash".to_string()),
                account_plan_type: Some("pro".to_string()),
                idempotency_key: "redeem-1".to_string(),
                credit_id: Some("credit-1".to_string()),
                outcome: Some("reset".to_string()),
                status: "success".to_string(),
                error: None,
            }
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
                    last_agent_message: None,
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
                final_model: Some("provider-final-model".to_string()),
                model_snapshot: Some("provider-model-snapshot".to_string()),
                input_tokens_uncached: 8,
                input_tokens_cached: 2,
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
                    final_model: None,
                    model_snapshot: None,
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
                    final_model: None,
                    model_snapshot: None,
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
                connector_id: None,
                mcp_app_resource_uri: None,
                link_id: None,
                app_name: None,
                template_id: None,
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
                template_id: None,
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
