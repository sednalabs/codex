//! Discover recent local roots and load business chat credit estimates.

use super::client::Live;
use super::client::Session;
use super::data;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadSortKey;
use codex_backend_client::ThreadUsage;
use std::collections::HashMap;
use std::sync::Arc;

pub(super) struct Chat {
    pub title: String,
    pub usage: Option<ThreadUsage>,
}

impl Chat {
    /// Local history can span accounts; expose titles only with a returned account estimate.
    pub(super) fn display_title(&self) -> &str {
        if self.usage.is_some() {
            &self.title
        } else {
            "Chat usage unavailable"
        }
    }
}

#[derive(Default)]
pub(super) struct Chats {
    pub rows: Vec<Chat>,
    pub truncated: bool,
    pub next_cursor: Option<String>,
}

pub(super) struct RootThreads {
    pub threads: Vec<codex_app_server_protocol::Thread>,
    pub truncated: bool,
    pub next_cursor: Option<String>,
}

pub(super) const MAX_RECENT_ROOT_THREADS: usize = 100;

pub(super) async fn read(
    handle: AppServerRequestHandle,
    live: Arc<Live>,
    cursor: Option<String>,
) -> Result<Option<Chats>, String> {
    let session = live.session().await?;
    if !super::models::thread_usage_supported(session.backend.account().plan_type) {
        return Ok(None);
    }
    live.ensure_identity().await?;
    let roots = tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 60),
        roots(&handle, cursor),
    )
    .await
    .map_err(|_| "Chat listing timed out. Press R to retry.".to_string());
    live.ensure_identity().await?;
    let roots = roots??;
    let truncated = roots.truncated;
    let next_cursor = roots.next_cursor;
    let threads = roots.threads;
    if threads.is_empty() {
        return Ok(Some(Chats {
            rows: Vec::new(),
            truncated,
            next_cursor,
        }));
    }
    let ids = threads
        .iter()
        .map(|thread| thread.id.as_str())
        .collect::<Vec<_>>();
    // Keep estimate time separate from local history discovery.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(/*secs*/ 25);
    let mut data = Vec::new();
    for pair in ids.chunks(/*chunk_size*/ 200) {
        let midpoint = pair.len().min(/*other*/ 100);
        let (first, second) = tokio::join!(
            tokio::time::timeout_at(deadline, estimates(session, &pair[..midpoint])),
            tokio::time::timeout_at(deadline, estimates(session, &pair[midpoint..])),
        );
        live.ensure_identity().await?;
        for batch in [first, second].into_iter().flatten() {
            data.extend(batch?);
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    live.ensure_identity().await?;
    let mut usage: HashMap<_, _> = data
        .into_iter()
        .map(|row| (row.thread_id.clone(), row))
        .collect();
    let mut rows = threads
        .into_iter()
        .map(|thread| Chat {
            usage: usage.remove(&thread.id),
            title: thread
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| {
                    if thread.preview.trim().is_empty() {
                        "Untitled chat".into()
                    } else {
                        thread.preview
                    }
                }),
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| {
        std::cmp::Reverse(
            row.usage
                .as_ref()
                .map(|usage| usage.estimated_usage_credits_micros),
        )
    });
    Ok(Some(Chats {
        rows,
        truncated,
        next_cursor,
    }))
}

/// One bounded, scan-and-repair page of local roots, shared by task and credit reporting.
pub(super) async fn roots(
    handle: &AppServerRequestHandle,
    cursor: Option<String>,
) -> Result<RootThreads, String> {
    let cutoff = chrono::Utc::now().timestamp() - 30 * 24 * 60 * 60;
    // UpdatedAt sorting requires a filesystem mtime scan across the whole rollout tree. Use
    // creation-ordered ThreadList pages instead, retaining its scan-and-repair behavior for the
    // returned page. The UI exposes the cursor so older active roots remain reachable.
    let page: ThreadListResponse = handle
        .request_typed(ClientRequest::ThreadList {
            request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
            params: ThreadListParams {
                cursor: cursor.clone(),
                ..list_params(/*cursor*/ None)
            },
        })
        .await
        .map_err(data::error)?;
    if page.next_cursor.as_deref() == cursor.as_deref() && cursor.is_some() {
        return Err("Chat listing repeated a cursor. Press R to retry.".into());
    }
    let next_cursor = page.next_cursor;
    let mut threads = page
        .data
        .into_iter()
        .filter(|thread| {
            thread.updated_at >= cutoff && !thread.ephemeral && thread.parent_thread_id.is_none()
        })
        .collect::<Vec<_>>();
    // The page is ordered by creation time for bounded discovery; rank its eligible roots by
    // recent activity, without implying this is a global recent-activity ranking.
    threads.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    threads.truncate(MAX_RECENT_ROOT_THREADS);
    let truncated = cursor.is_some() || next_cursor.is_some();
    Ok(RootThreads {
        threads,
        truncated,
        next_cursor,
    })
}

pub(super) fn list_params(cursor: Option<String>) -> ThreadListParams {
    ThreadListParams {
        excluded_thread_ids: None,
        attached_to_thread_id: None,
        limit: Some(MAX_RECENT_ROOT_THREADS as u32),
        sort_key: Some(ThreadSortKey::CreatedAt),
        sort_direction: Some(SortDirection::Desc),
        model_providers: Some(Vec::new()),
        source_kinds: None,
        originators: None,
        archived: Some(false),
        cursor,
        section_id: None,
        project_id: None,
        cwd: None,
        use_state_db_only: false,
        search_term: None,
        parent_thread_id: None,
        ancestor_thread_id: None,
    }
}

/// Isolate unavailable estimates within the existing concurrency slot; keep healthy rows.
async fn estimates(session: &Session, ids: &[&str]) -> Result<Vec<ThreadUsage>, String> {
    let mut pending = if ids.is_empty() {
        Vec::new()
    } else {
        vec![ids]
    };
    let mut rows = Vec::new();
    // At most 15 requests per batch; leave remaining estimates unavailable during an outage.
    for _ in 0..15 {
        let Some(ids) = pending.pop() else { break };
        match session
            .backend
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
            Err(error) => return Err(super::client::request_error(error)),
        }
    }
    Ok(rows)
}

#[cfg(test)]
#[path = "chats_tests.rs"]
mod tests;
