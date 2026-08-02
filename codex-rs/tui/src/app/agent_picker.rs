//! Background refreshes for the multi-agent picker.
//!
//! The picker is intentionally rendered from cached navigation state first. This module then
//! performs the bounded descendant discovery work off the input path and returns one correlated
//! completion event to the single-threaded [`App`] owner.

use super::*;
use crate::app::loaded_threads::find_loaded_subagent_threads_for_primary;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadSortKey;
use codex_app_server_protocol::ThreadSourceKind;
use std::collections::HashSet;

pub(super) const AGENT_PICKER_VIEW_ID: &str = "agent-picker";

/// Match the existing visible-history page rather than turning `/agent` into an unbounded tree
/// walk. Older history remains available through the explicit picker continuation.
const AGENT_PICKER_REFRESH_PAGE_SIZE: u32 = 50;
/// Preserve the existing bounded priority pass for loaded descendants so a current child does not
/// disappear behind a full first page of closed history.
const AGENT_PICKER_LOADED_PRIORITY_PAGE_SIZE: u32 = 100;
const AGENT_PICKER_LOADED_PRIORITY_MAX_PAGES: usize = 2;
/// Older app servers may omit relation acknowledgement. Keep the compatibility proof bounded.
const AGENT_PICKER_UNACKNOWLEDGED_RELATION_PAGE_SIZE: u32 = 100;

/// One correlated background response. A partial response remains useful: fresh loaded metadata
/// is merged even if a separate historical or legacy probe failed, and the failure is surfaced
/// without blanking an already-open picker.
#[derive(Debug)]
pub(crate) struct AgentPickerRefreshResult {
    pub(crate) threads: Vec<Thread>,
    /// `None` means the server did not acknowledge a relation-filtered page, so its cursor is not
    /// safe to expose as the picker continuation. `Some(None)` means the first page is
    /// authoritative and exhausted.
    pub(crate) persisted_next_picker_page_cursor: Option<Option<String>>,
    pub(crate) mark_legacy_relation_fallback_checked: bool,
    pub(crate) errors: Vec<String>,
}

impl App {
    /// Schedules, but does not await, one root-scoped picker refresh. Reopening the picker while
    /// the request is in flight is intentionally a no-op: its cached rows are already visible and
    /// the eventual response will refresh the same root lifecycle.
    pub(super) fn refresh_agent_picker_threads(
        &mut self,
        app_server: &AppServerSession,
        root_thread_id: ThreadId,
    ) {
        let lifecycle_generation = self.thread_lifecycle_generation(root_thread_id);
        let Some(request_generation) = self
            .agent_navigation
            .begin_picker_refresh(root_thread_id, lifecycle_generation)
        else {
            return;
        };
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let needs_legacy_relation_fallback = self
            .agent_navigation
            .needs_legacy_relation_fallback_check();

        tokio::spawn(async move {
            let result = fetch_agent_picker_refresh(
                request_handle,
                root_thread_id,
                needs_legacy_relation_fallback,
            )
            .await;
            app_event_tx.send(AppEvent::AgentPickerThreadsLoaded {
                primary_thread_id: root_thread_id,
                lifecycle_generation,
                request_generation,
                result,
            });
        });
    }

    /// Applies a completed background refresh only when it still belongs to the currently
    /// attached primary session. All stale replies fail closed before they touch cached rows,
    /// cursors, or popup selection state.
    pub(super) async fn apply_agent_picker_thread_refresh(
        &mut self,
        primary_thread_id: ThreadId,
        lifecycle_generation: u64,
        request_generation: u64,
        result: AgentPickerRefreshResult,
    ) {
        let refresh_owns_cursor = self.agent_navigation.picker_refresh_owns_cursor(
            primary_thread_id,
            lifecycle_generation,
            request_generation,
        );
        if !self.agent_navigation.finish_picker_refresh(
            primary_thread_id,
            lifecycle_generation,
            request_generation,
        ) || self.primary_thread_id != Some(primary_thread_id)
            || !self.thread_accepts_lifecycle_generation(primary_thread_id, lifecycle_generation)
        {
            tracing::debug!(
                %primary_thread_id,
                lifecycle_generation,
                request_generation,
                "dropping stale agent-picker refresh"
            );
            return;
        }

        let response_was_authoritatively_exhausted = refresh_owns_cursor
            && result
                .persisted_next_picker_page_cursor
                .is_some_and(|next_cursor| next_cursor.is_none())
            && result.errors.is_empty();
        let mut refreshed_thread_ids = HashSet::new();
        for thread in result.threads {
            self.register_agent_picker_thread_from_backend(
                primary_thread_id,
                thread,
                &mut refreshed_thread_ids,
            );
        }
        if refresh_owns_cursor
            && let Some(next_cursor) = result.persisted_next_picker_page_cursor
        {
            // A reopen must not rewind a continuation that the user already consumed. An
            // authoritative exhausted first page, however, must remove a stale Load more row.
            if next_cursor.is_none() || self.agent_navigation.next_picker_page_cursor().is_none()
            {
                self.agent_navigation.set_next_picker_page_cursor(next_cursor);
            }
        }
        self.agent_navigation.complete_picker_refresh_empty_state(
            response_was_authoritatively_exhausted,
        );
        if result.mark_legacy_relation_fallback_checked {
            self.agent_navigation.mark_legacy_relation_fallback_checked();
        }
        self.sync_active_agent_label();

        if !result.errors.is_empty() {
            let message = result.errors.join("; ");
            tracing::warn!(%message, "agent-picker background refresh was incomplete");
            self.chat_widget
                .add_error_message(format!("Subagent refresh was incomplete: {message}"));
        }

        // Keep the user's selection and filter when the picker is still visible. If it has been
        // dismissed, the cache is updated for the next open without reviving a stale popup.
        if self
            .chat_widget
            .selection_view_search_query(AGENT_PICKER_VIEW_ID)
            .is_some()
        {
            self.render_agent_picker().await;
        }
    }
}

async fn fetch_agent_picker_refresh(
    request_handle: AppServerRequestHandle,
    root_thread_id: ThreadId,
    needs_legacy_relation_fallback: bool,
) -> AgentPickerRefreshResult {
    let mut threads = Vec::new();
    let mut errors = Vec::new();
    let mut loaded_priority_completed = true;

    let mut loaded_cursor = None;
    let mut loaded_pages_consumed = 0;
    while loaded_pages_consumed < AGENT_PICKER_LOADED_PRIORITY_MAX_PAGES {
        let response = request_thread_loaded_list(
            &request_handle,
            ThreadLoadedListParams {
                cursor: loaded_cursor,
                limit: Some(AGENT_PICKER_LOADED_PRIORITY_PAGE_SIZE),
                ancestor_thread_id: Some(root_thread_id.to_string()),
            },
        )
        .await;
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                loaded_priority_completed = false;
                errors.push(format!("loaded descendant lookup failed: {err}"));
                break;
            }
        };
        if !response.ancestor_filter_applied {
            // An unacknowledged response can be a global loaded list. Never turn it into a
            // thread/read sweep; bounded persisted/legacy queries below retain safe discovery.
            break;
        }

        for raw_thread_id in response.data {
            let Ok(thread_id) = ThreadId::from_string(&raw_thread_id) else {
                loaded_priority_completed = false;
                errors.push("loaded descendant lookup returned an invalid thread id".to_string());
                continue;
            };
            match request_thread_read(&request_handle, thread_id).await {
                Ok(thread) => threads.push(thread),
                Err(err) => {
                    loaded_priority_completed = false;
                    errors.push(format!("loaded descendant metadata read failed: {err}"));
                }
            }
        }

        loaded_pages_consumed += 1;
        loaded_cursor = response.next_cursor;
        if loaded_cursor.is_none() {
            break;
        }
    }

    let persisted = request_thread_list(
        &request_handle,
        ThreadListParams {
            cursor: None,
            limit: Some(AGENT_PICKER_REFRESH_PAGE_SIZE),
            sort_key: Some(ThreadSortKey::UpdatedAt),
            sort_direction: Some(SortDirection::Desc),
            model_providers: None,
            source_kinds: None,
            thread_sources: None,
            archived: Some(false),
            is_pinned: None,
            cwd: None,
            use_state_db_only: true,
            search_term: None,
            parent_thread_id: None,
            ancestor_thread_id: Some(root_thread_id.to_string()),
        },
    )
    .await;
    let persisted_next_picker_page_cursor = match persisted {
        Ok(response) if response.ancestor_filter_applied => {
            threads.extend(response.data);
            Some(response.next_cursor)
        }
        Ok(_response) => {
            // Match the Final27 fail-closed rule: a cursor from an unacknowledged relationship
            // response may name an unrelated global page and is never displayed.
            match request_thread_list(
                &request_handle,
                ThreadListParams {
                    cursor: None,
                    limit: Some(AGENT_PICKER_UNACKNOWLEDGED_RELATION_PAGE_SIZE),
                    sort_key: Some(ThreadSortKey::UpdatedAt),
                    sort_direction: Some(SortDirection::Desc),
                    model_providers: None,
                    source_kinds: None,
                    thread_sources: None,
                    archived: Some(false),
                    is_pinned: None,
                    cwd: None,
                    use_state_db_only: true,
                    search_term: None,
                    parent_thread_id: None,
                    ancestor_thread_id: Some(root_thread_id.to_string()),
                },
            )
            .await
            {
                Ok(response) => {
                    let descendant_thread_ids = find_loaded_subagent_threads_for_primary(
                        response.data.clone(),
                        root_thread_id,
                    )
                    .into_iter()
                    .map(|thread| thread.thread_id)
                    .collect::<HashSet<_>>();
                    threads.extend(response.data.into_iter().filter(|thread| {
                        ThreadId::from_string(&thread.id)
                            .is_ok_and(|thread_id| descendant_thread_ids.contains(&thread_id))
                    }));
                }
                Err(err) => errors.push(format!(
                    "compatibility descendant lookup failed: {err}"
                )),
            }
            None
        }
        Err(err) => {
            errors.push(format!("persisted descendant lookup failed: {err}"));
            None
        }
    };

    let mut legacy_relation_fallback_completed = !needs_legacy_relation_fallback;
    if needs_legacy_relation_fallback {
        match request_thread_list(
            &request_handle,
            ThreadListParams {
                cursor: None,
                limit: Some(AGENT_PICKER_REFRESH_PAGE_SIZE),
                sort_key: Some(ThreadSortKey::UpdatedAt),
                sort_direction: Some(SortDirection::Desc),
                model_providers: None,
                source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
                thread_sources: None,
                archived: Some(false),
                is_pinned: None,
                cwd: None,
                use_state_db_only: false,
                search_term: None,
                parent_thread_id: None,
                ancestor_thread_id: None,
            },
        )
        .await
        {
            Ok(response) => {
                let descendant_thread_ids = find_loaded_subagent_threads_for_primary(
                    response.data.clone(),
                    root_thread_id,
                )
                .into_iter()
                .map(|thread| thread.thread_id)
                .collect::<HashSet<_>>();
                threads.extend(response.data.into_iter().filter(|thread| {
                    ThreadId::from_string(&thread.id)
                        .is_ok_and(|thread_id| descendant_thread_ids.contains(&thread_id))
                }));
                legacy_relation_fallback_completed = true;
            }
            Err(err) => errors.push(format!("legacy descendant lookup failed: {err}")),
        }
    }

    // Preserve the existing priority order while making duplicate sources harmless.
    let mut seen_thread_ids = HashSet::new();
    threads.retain(|thread| seen_thread_ids.insert(thread.id.clone()));

    AgentPickerRefreshResult {
        threads,
        persisted_next_picker_page_cursor,
        mark_legacy_relation_fallback_checked: loaded_priority_completed
            && legacy_relation_fallback_completed,
        errors,
    }
}

async fn request_thread_list(
    request_handle: &AppServerRequestHandle,
    params: ThreadListParams,
) -> Result<ThreadListResponse, String> {
    request_handle
        .request_typed(ClientRequest::ThreadList {
            request_id: RequestId::String(Uuid::new_v4().to_string()),
            params,
        })
        .await
        .map_err(|err| err.to_string())
}

async fn request_thread_loaded_list(
    request_handle: &AppServerRequestHandle,
    params: ThreadLoadedListParams,
) -> Result<ThreadLoadedListResponse, String> {
    request_handle
        .request_typed(ClientRequest::ThreadLoadedList {
            request_id: RequestId::String(Uuid::new_v4().to_string()),
            params,
        })
        .await
        .map_err(|err| err.to_string())
}

async fn request_thread_read(
    request_handle: &AppServerRequestHandle,
    thread_id: ThreadId,
) -> Result<Thread, String> {
    request_handle
        .request_typed::<ThreadReadResponse>(ClientRequest::ThreadRead {
            request_id: RequestId::String(Uuid::new_v4().to_string()),
            params: ThreadReadParams {
                thread_id: thread_id.to_string(),
                include_turns: false,
            },
        })
        .await
        .map(|response| response.thread)
        .map_err(|err| err.to_string())
}
