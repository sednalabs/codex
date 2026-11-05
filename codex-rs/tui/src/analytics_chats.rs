//! Bounded, account-scoped discovery for the Analytics Chats view.
//!
//! The state index is not a complete history authority. Callers must label its
//! results as partial even if the current page has no continuation cursor.

use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadSortKey;
use codex_backend_client::AnalyticsSession;
use codex_backend_client::TaskUsage;
use codex_backend_client::TaskUsageStatus;
use codex_backend_client::TaskUsageThread;
use codex_backend_client::ThreadUsage;
use futures::StreamExt;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;

#[cfg(test)]
#[path = "analytics_chats_tests.rs"]
mod tests;

const MAX_ROOTS: u32 = 100;
const ESTIMATE_BUDGET: Duration = Duration::from_secs(/*secs*/ 3);

#[derive(Debug)]
pub(crate) struct Chat {
    pub(crate) title: String,
    pub(crate) usage: Option<ThreadUsage>,
    pub(crate) local_usage: Option<LocalUsage>,
}

impl Chat {
    /// Local titles may belong to a different account. Reveal only an identified row.
    pub(crate) fn display_title(&self) -> &str {
        if self.usage.is_some() {
            &self.title
        } else {
            "Chat usage unavailable"
        }
    }
}

#[derive(Debug)]
pub(crate) struct Chats {
    /// Ranked only within this indexed page, never a global ranking.
    pub(crate) rows: Vec<Chat>,
    pub(crate) next_cursor: Option<String>,
    /// The local index is potentially incomplete, including on its last page.
    pub(crate) partial_history: bool,
    /// At least one listed chat has no available estimate.
    pub(crate) partial_estimates: bool,
}

#[derive(Debug)]
pub(crate) struct ConsumerChat {
    title: String,
    pub(crate) usage: Option<TaskUsage>,
    pub(crate) local_usage: Option<LocalUsage>,
}

/// A partial, read-only view of the existing local usage ledger.
#[derive(Debug)]
pub(crate) struct LocalUsage {
    pub(crate) threads: Vec<LocalUsageThread>,
    pub(crate) truncated: bool,
}

#[derive(Debug)]
pub(crate) struct LocalUsageThread {
    pub(crate) kind: String,
    pub(crate) models: Option<String>,
    pub(crate) estimated_credits: Option<f64>,
    pub(crate) partial: bool,
}

impl ConsumerChat {
    pub(crate) fn display_title(&self) -> &str {
        if self
            .usage
            .as_ref()
            .is_some_and(|usage| usage.data_status != TaskUsageStatus::Unavailable)
        {
            &self.title
        } else {
            "Chat usage unavailable"
        }
    }
}

#[derive(Debug)]
pub(crate) struct ConsumerChats {
    pub(crate) rows: Vec<ConsumerChat>,
    pub(crate) next_cursor: Option<String>,
    /// Descendants and roots come from a potentially incomplete local index.
    pub(crate) partial_history: bool,
    pub(crate) partial_usage: bool,
}

#[derive(Debug)]
pub(crate) enum AccountChats {
    Workspace(Chats),
    Consumer(ConsumerChats),
}

impl AccountChats {
    /// Only attach local data for IDs verified by the current account's backend.
    pub(crate) async fn attach_local_usage(
        &mut self,
        db: Option<&codex_state::StateRuntime>,
        session: &AnalyticsSession,
    ) -> Result<(), String> {
        let Some(db) = db else {
            return Ok(());
        };
        let ids: Vec<(usize, String)> = match self {
            Self::Workspace(page) => page
                .rows
                .iter()
                .enumerate()
                .filter_map(|(i, row)| row.usage.as_ref().map(|usage| (i, usage.thread_id.clone())))
                .collect(),
            Self::Consumer(page) => page
                .rows
                .iter()
                .enumerate()
                .filter_map(|(i, row)| {
                    row.usage
                        .as_ref()
                        .filter(|usage| usage.data_status != TaskUsageStatus::Unavailable)
                        .map(|usage| (i, usage.thread_id.clone()))
                })
                .collect(),
        };
        session.ensure_identity().await?;
        let queries = ids.into_iter().map(|(index, id)| async move {
            let data = db.get_usage_lineage_telemetry(&id).await.ok().flatten()?;
            if data.root_thread_id != id {
                return None;
            }
            let truncated = data.threads.len() > 12;
            let mut family = data.threads;
            family.sort_by_key(|thread| thread.thread_id != id);
            let threads = family
                .into_iter()
                .take(12)
                .map(|thread| {
                    let kind = if thread.thread_id == id {
                        "Root".to_string()
                    } else {
                        match thread.lineage_edge_kind.as_str() {
                            "agent_spawn" => "Subagent".to_string(),
                            "fork" => "Fork".to_string(),
                            _ => "Other thread".to_string(),
                        }
                    };
                    LocalUsageThread {
                        kind,
                        models: thread.models_used,
                        estimated_credits: thread.estimated_total_credits,
                        partial: thread.partial
                            || thread.unpriced_call_count > 0
                            || thread.lineage_confidence != "persisted",
                    }
                })
                .collect();
            Some((index, LocalUsage { threads, truncated }))
        });
        let mut pending = futures::stream::iter(queries).buffer_unordered(/*n*/ 4);
        let deadline = tokio::time::Instant::now() + ESTIMATE_BUDGET;
        while let Ok(Some(result)) = tokio::time::timeout_at(deadline, pending.next()).await {
            if let Some((index, usage)) = result {
                match self {
                    Self::Workspace(page) => page.rows[index].local_usage = Some(usage),
                    Self::Consumer(page) => page.rows[index].local_usage = Some(usage),
                }
            }
        }
        drop(pending);
        session.ensure_identity().await
    }
}

/// Select a supported read-only endpoint from the authenticated server plan.
pub(crate) async fn read_account(
    handle: &AppServerRequestHandle,
    session: &AnalyticsSession,
    cursor: Option<String>,
) -> Result<AccountChats, String> {
    use codex_protocol::account::PlanType;
    match session.account().plan_type {
        Some(PlanType::Business | PlanType::EnterpriseCbpUsageBased) => {
            read(handle, session, cursor)
                .await
                .map(AccountChats::Workspace)
        }
        Some(plan) if plan != PlanType::Unknown && !plan.is_workspace_account() => {
            read_consumer(handle, session, cursor)
                .await
                .map(AccountChats::Consumer)
        }
        _ => Err("Chats usage is unavailable for this account plan.".to_string()),
    }
}

/// Query consumer task usage only for individually verified descendant groups.
pub(crate) async fn read_consumer(
    handle: &AppServerRequestHandle,
    session: &AnalyticsSession,
    cursor: Option<String>,
) -> Result<ConsumerChats, String> {
    use codex_protocol::account::PlanType;
    if !matches!(
        session.account().plan_type,
        Some(plan) if plan != PlanType::Unknown && !plan.is_workspace_account()
    ) {
        return Err("Consumer task usage is unavailable for this account plan.".to_string());
    }
    session.ensure_identity().await?;
    let page = tokio::time::timeout(Duration::from_secs(/*secs*/ 60), roots(handle, cursor)).await;
    session.ensure_identity().await?;
    let (threads, next_cursor) =
        page.map_err(|_| "Chat listing timed out. Select Retry.".to_string())??;
    let mut pending = futures::stream::iter(threads.iter().map(|root| descendants(handle, root)))
        .buffer_unordered(/*n*/ 2);
    let deadline = tokio::time::Instant::now() + ESTIMATE_BUDGET;
    let mut queries = Vec::new();
    while let Ok(Some(result)) = tokio::time::timeout_at(deadline, pending.next()).await {
        if let Ok(query) = result {
            queries.push(query);
        }
    }
    drop(pending);
    session.ensure_identity().await?;

    let mut seen = HashSet::new();
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut count = 0;
    for query in queries {
        let ids = std::iter::once(&query.thread_id).chain(&query.descendant_thread_ids);
        if ids.clone().any(|id| !seen.insert(id.clone())) {
            return Err("Task descendant groups overlap.".to_string());
        }
        let size = 1 + query.descendant_thread_ids.len();
        if count + size > 1_000 || batch.len() == 100 {
            batches.push(std::mem::take(&mut batch));
            count = 0;
        }
        count += size;
        batch.push(query);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }

    let requests = batches.into_iter().map(|threads| async move {
        session
            .request(|client| {
                let threads = threads.clone();
                async move { client.get_task_usage(&threads).await }
            })
            .await
    });
    let mut pending = futures::stream::iter(requests).buffer_unordered(/*n*/ 2);
    let deadline = tokio::time::Instant::now() + ESTIMATE_BUDGET;
    let mut usage = HashMap::new();
    while let Ok(Some(result)) = tokio::time::timeout_at(deadline, pending.next()).await {
        session.ensure_identity().await?;
        match result {
            Ok(response) => usage.extend(
                response
                    .threads
                    .into_iter()
                    .map(|task| (task.thread_id.clone(), task)),
            ),
            Err(error)
                if error
                    .status()
                    .is_some_and(|status| matches!(status.as_u16(), 401 | 403)) =>
            {
                return Err("Sign in again to view task usage.".to_string());
            }
            Err(_) => {}
        }
    }
    drop(pending);
    session.ensure_identity().await?;
    let rows: Vec<_> = threads
        .into_iter()
        .map(|thread| ConsumerChat {
            usage: usage.remove(&thread.id),
            local_usage: None,
            title: thread
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| {
                    if thread.preview.trim().is_empty() {
                        "Untitled chat".to_string()
                    } else {
                        thread.preview
                    }
                }),
        })
        .collect();
    let partial_usage = rows.iter().any(|row| {
        row.usage
            .as_ref()
            .is_none_or(|usage| usage.data_status != TaskUsageStatus::Available)
    });
    Ok(ConsumerChats {
        rows,
        next_cursor,
        partial_history: true,
        partial_usage,
    })
}

/// Load one indexed page and bound optional backend estimates independently.
pub(crate) async fn read(
    handle: &AppServerRequestHandle,
    session: &AnalyticsSession,
    cursor: Option<String>,
) -> Result<Chats, String> {
    use codex_protocol::account::PlanType;
    if !matches!(
        session.account().plan_type,
        Some(PlanType::Business | PlanType::EnterpriseCbpUsageBased)
    ) {
        return Err("Chat estimates are unavailable for this account plan.".to_string());
    }
    session.ensure_identity().await?;
    let page = tokio::time::timeout(Duration::from_secs(/*secs*/ 60), roots(handle, cursor)).await;
    session.ensure_identity().await?;
    let (threads, next_cursor) =
        page.map_err(|_| "Chat listing timed out. Select Retry.".to_string())??;
    let ids: Vec<_> = threads.iter().map(|thread| thread.id.as_str()).collect();
    let deadline = tokio::time::Instant::now() + ESTIMATE_BUDGET;
    let mut data = Vec::new();
    for batch in ids.chunks(/*chunk_size*/ 100) {
        match tokio::time::timeout_at(deadline, estimates(session, batch)).await {
            Ok(Ok(rows)) => data.extend(rows),
            Ok(Err(error))
                if error
                    .status()
                    .is_some_and(|status| matches!(status.as_u16(), 401 | 403)) =>
            {
                return Err("Sign in again to view chat estimates.".to_string());
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
    session.ensure_identity().await?;
    let mut usage: HashMap<_, _> = data
        .into_iter()
        .map(|row| (row.thread_id.clone(), row))
        .collect();
    let mut rows: Vec<_> = threads
        .into_iter()
        .map(|thread| Chat {
            usage: usage.remove(&thread.id),
            local_usage: None,
            title: thread
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| {
                    if thread.preview.trim().is_empty() {
                        "Untitled chat".to_string()
                    } else {
                        thread.preview
                    }
                }),
        })
        .collect();
    rows.sort_by_key(|row| {
        std::cmp::Reverse(
            row.usage
                .as_ref()
                .map(|item| item.estimated_usage_credits_micros),
        )
    });
    let partial_estimates = rows.iter().any(|row| row.usage.is_none());
    Ok(Chats {
        rows,
        next_cursor,
        partial_history: true,
        partial_estimates,
    })
}

async fn roots(
    handle: &AppServerRequestHandle,
    cursor: Option<String>,
) -> Result<(Vec<codex_app_server_protocol::Thread>, Option<String>), String> {
    let response: ThreadListResponse = handle
        .request_typed(ClientRequest::ThreadList {
            request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
            params: list_params(cursor.clone()),
        })
        .await
        .map_err(|_| "Chat listing unavailable. Select Retry.".to_string())?;
    if cursor.is_some() && response.next_cursor == cursor {
        return Err("Chat listing repeated a cursor. Select Retry.".to_string());
    }
    let cutoff = chrono::Utc::now().timestamp() - 30 * 24 * 60 * 60;
    let mut threads: Vec<_> = response
        .data
        .into_iter()
        .filter(|thread| {
            thread.updated_at >= cutoff && !thread.ephemeral && thread.parent_thread_id.is_none()
        })
        .collect();
    threads.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok((threads, response.next_cursor))
}

fn list_params(cursor: Option<String>) -> ThreadListParams {
    ThreadListParams {
        cursor,
        limit: Some(MAX_ROOTS),
        sort_key: Some(ThreadSortKey::CreatedAt),
        sort_direction: Some(SortDirection::Desc),
        model_providers: Some(Vec::new()),
        source_kinds: None,
        thread_sources: None,
        archived: Some(false),
        is_pinned: None,
        cwd: None,
        use_state_db_only: true,
        search_term: None,
        parent_thread_id: None,
        ancestor_thread_id: None,
    }
}

/// Collect an indexed descendant family only when the server confirms filtering.
/// A successful indexed result still does not prove full historical coverage.
async fn descendants(
    handle: &AppServerRequestHandle,
    root: &codex_app_server_protocol::Thread,
) -> Result<TaskUsageThread, String> {
    let mut ids = HashSet::new();
    let mut parents = HashMap::new();
    for archived in [false, true] {
        let mut cursor = None;
        let mut seen = HashSet::new();
        loop {
            let page: ThreadListResponse = handle
                .request_typed(ClientRequest::ThreadList {
                    request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
                    params: ThreadListParams {
                        ancestor_thread_id: Some(root.id.clone()),
                        archived: Some(archived),
                        cursor,
                        ..list_params(None)
                    },
                })
                .await
                .map_err(|_| "Task descendants are unavailable.".to_string())?;
            if page.ancestor_filter_applied != Some(true)
                || page.relation_limit_reached == Some(true)
            {
                return Err("Task descendant coverage is unavailable.".to_string());
            }
            for thread in page.data {
                let parent = thread
                    .parent_thread_id
                    .filter(|_| thread.id != root.id)
                    .ok_or("Task descendant discovery is incomplete.")?;
                if !ids.insert(thread.id.clone()) {
                    return Err("Task descendant was repeated.".to_string());
                }
                parents.insert(thread.id, parent);
            }
            if ids.len() >= 1_000 {
                return Err("Task exceeds the reporting limit.".to_string());
            }
            match page.next_cursor {
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                Some(_) => return Err("Task listing repeated a cursor.".to_string()),
                None => break,
            }
        }
    }
    for id in &ids {
        let mut current = id;
        let mut chain = HashSet::new();
        while current != &root.id {
            if !chain.insert(current) {
                return Err("Task descendant cycle.".to_string());
            }
            current = parents
                .get(current)
                .ok_or("Task descendant discovery is incomplete.")?;
        }
    }
    let mut descendant_thread_ids = ids.into_iter().collect::<Vec<_>>();
    descendant_thread_ids.sort();
    Ok(TaskUsageThread {
        thread_id: root.id.clone(),
        created_at: (root.created_at > 0)
            .then(|| chrono::DateTime::from_timestamp(root.created_at, /*nsecs*/ 0))
            .flatten()
            .map(|time| time.to_rfc3339()),
        descendant_thread_ids,
    })
}

/// Split temporary 503s within a fixed request budget; missing rows stay missing.
async fn estimates(
    session: &AnalyticsSession,
    ids: &[&str],
) -> Result<Vec<ThreadUsage>, codex_backend_client::RequestError> {
    let mut pending = if ids.is_empty() {
        Vec::new()
    } else {
        vec![ids]
    };
    let mut rows = Vec::new();
    for _ in 0..15 {
        let Some(ids) = pending.pop() else { break };
        match session
            .request(|client| async move { client.get_threads_usage(ids).await })
            .await
        {
            Ok(batch) => rows.extend(batch),
            Err(error) if error.status().is_some_and(|status| status.as_u16() == 503) => {
                if ids.len() > 1 {
                    let midpoint = ids.len().div_ceil(/*rhs*/ 2);
                    pending.push(&ids[midpoint..]);
                    pending.push(&ids[..midpoint]);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(rows)
}
