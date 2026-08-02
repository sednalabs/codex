//! Session, resume, fork, and subagent selection lifecycle for the TUI app.
//!
//! This module owns the high-level transitions between app-server threads: starting fresh sessions,
//! resuming/forking saved sessions, replacing ChatWidget instances, and maintaining the agent picker
//! cache used for multi-agent navigation.

use super::*;
use crate::app::loaded_threads::find_loaded_subagent_threads_for_primary;
use crate::app_server_session::source_agent_path;
use crate::app_server_session::thread_blocks_direct_input;
use crate::multi_agents::AgentPickerThreadUsage;
use crate::multi_agents::format_agent_picker_item_description;
use crate::multi_agents::format_agent_picker_item_label;
use crate::multi_agents::format_agent_picker_item_selected_description;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadSortKey;
use codex_app_server_protocol::ThreadSourceKind;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadStatusChangedNotification;
use codex_config::types::ResumeCwdMode;
use codex_protocol::protocol::TokenUsage as ProtocolTokenUsage;
use std::collections::HashSet;

const AGENT_PICKER_PAGE_SIZE: u32 = 50;
/// One bounded relation page for servers that predate `ancestorFilterApplied` but support the
/// older `thread/list.ancestorThreadId` filter. The app server clamps a single list request at
/// 100 rows, and this path never performs a per-id metadata sweep.
const AGENT_PICKER_UNACKNOWLEDGED_RELATION_PAGE_SIZE: u32 = 100;
/// Keep every loaded-descendant priority request finite. The list protocol now caps this request,
/// but an explicit client value makes the picker bound clear and compatible with older servers.
const AGENT_PICKER_LOADED_PRIORITY_PAGE_SIZE: u32 = 100;
/// Consume at most two loaded-list continuation pages before the picker opens. This makes up to
/// 200 current descendants visible without turning the priority path into an unbounded scan.
const AGENT_PICKER_LOADED_PRIORITY_MAX_PAGES: usize = 2;
const AGENT_PICKER_VIEW_ID: &str = "agent-picker";

#[derive(Clone, Copy)]
pub(super) enum ThreadAttachPresentation {
    SessionLineage,
    PromptEdit,
}

/// Reports whether a bounded descendant backfill completed and which rows already carried fresh
/// liveness metadata, allowing the picker to skip duplicate `thread/read` requests.
#[derive(Default)]
pub(super) struct LoadedSubagentBackfill {
    pub(super) completed: bool,
    pub(super) refreshed_thread_ids: HashSet<ThreadId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AgentPickerThreadStatus {
    pub(super) is_running: bool,
    pub(super) is_closed: bool,
    pub(super) has_system_error: bool,
}

/// Classifies the app-server status used by persisted descendant backfill and targeted liveness
/// refreshes. `SystemError` remains selectable because it can still have a saved transcript.
pub(super) fn agent_picker_thread_status(
    status: &ThreadStatus,
    has_live_channel: bool,
) -> AgentPickerThreadStatus {
    AgentPickerThreadStatus {
        is_running: matches!(status, ThreadStatus::Active { .. }),
        is_closed: !has_live_channel && matches!(status, ThreadStatus::NotLoaded),
        has_system_error: matches!(status, ThreadStatus::SystemError),
    }
}

impl App {
    pub(super) async fn open_agent_picker(&mut self, app_server: &mut AppServerSession) {
        let backfill = self.backfill_loaded_subagent_threads(app_server).await;
        // V2 subagents are identified by canonical paths observed from activity events or loaded
        // thread metadata. A buffered active turn is positive liveness evidence; a completed
        // snapshot is terminal evidence. An empty store does not clear a successful spawn hint.
        let path_backed_thread_ids: Vec<_> = self
            .agent_navigation
            .ordered_path_backed_subagent_threads(self.primary_thread_id)
            .into_iter()
            .filter_map(|(thread_id, entry)| (!entry.is_closed).then_some(thread_id))
            .collect();
        for thread_id in path_backed_thread_ids.iter().copied() {
            if let Some(channel) = self.thread_event_channels.get(&thread_id)
                && channel.has_live_attachment()
            {
                let (has_active_turn, has_terminal_snapshot) = {
                    let store = channel.store.lock().await;
                    (
                        store.active_turn_id().is_some(),
                        store
                            .turns
                            .last()
                            .is_some_and(|turn| !matches!(turn.status, TurnStatus::InProgress)),
                    )
                };
                if has_active_turn {
                    self.agent_navigation.mark_running(thread_id);
                } else if has_terminal_snapshot {
                    self.agent_navigation.mark_stopped(thread_id);
                }
            } else if !backfill.refreshed_thread_ids.contains(&thread_id) {
                self.refresh_agent_picker_thread_liveness(app_server, thread_id)
                    .await;
            }
        }
        let path_backed_threads = self
            .agent_navigation
            .ordered_path_backed_subagent_threads(self.primary_thread_id);
        if !path_backed_threads.is_empty() {
            let running_threads: Vec<_> = path_backed_threads
                .into_iter()
                .filter_map(|(thread_id, entry)| {
                    if !entry.is_running || entry.is_closed {
                        return None;
                    }
                    Some((thread_id, entry.agent_path.as_deref()?.trim().to_string()))
                })
                .collect();
            let mut entries = Vec::new();
            for (thread_id, agent_path) in running_threads {
                let preview = if let Some(channel) = self.thread_event_channels.get(&thread_id) {
                    let store = channel.store.lock().await;
                    super::agent_status_feed::AgentStatusThreadPreview::from_store(
                        agent_path, &store,
                    )
                } else {
                    super::agent_status_feed::AgentStatusThreadPreview::empty(agent_path)
                };
                entries.push(preview);
            }

            self.chat_widget
                .add_to_history(super::agent_status_feed::AgentStatusHistoryCell::new(
                    entries,
                ));
        }

        let mut thread_ids = self.agent_navigation.tracked_thread_ids();
        for thread_id in self.thread_event_channels.keys().copied() {
            if !thread_ids.contains(&thread_id) {
                thread_ids.push(thread_id);
            }
        }
        for thread_id in thread_ids {
            if path_backed_thread_ids.contains(&thread_id)
                || self.side_threads.contains_key(&thread_id)
                || backfill.refreshed_thread_ids.contains(&thread_id)
                || self
                    .agent_navigation
                    .get(&thread_id)
                    .is_some_and(|entry| entry.is_closed)
            {
                continue;
            }
            if !self
                .refresh_agent_picker_thread_liveness(app_server, thread_id)
                .await
            {
                continue;
            }
        }

        self.render_agent_picker().await;
    }

    /// Loads one continuation page of persisted descendants without widening
    /// the list request or dropping the user's current `closed` filter.
    pub(super) async fn load_more_agent_picker_page(&mut self, app_server: &mut AppServerSession) {
        let Some(cursor) = self.agent_navigation.next_picker_page_cursor() else {
            return;
        };
        let backfill = self
            .backfill_agent_picker_page(app_server, Some(cursor))
            .await;
        if backfill.completed || self.agent_navigation.next_picker_page_cursor().is_none() {
            self.render_agent_picker().await;
        }
    }

    pub(crate) async fn render_agent_picker(&mut self) {
        let has_non_primary_agent_thread = self
            .agent_navigation
            .has_non_primary_thread(self.primary_thread_id);
        if !self.config.features.enabled(Feature::Collab) && !has_non_primary_agent_thread {
            self.chat_widget.open_multi_agent_enable_prompt();
            return;
        }

        if self.agent_navigation.is_empty() {
            self.chat_widget
                .add_info_message("No agents available yet.".to_string(), /*hint*/ None);
            return;
        }

        let prior_search_query = self
            .chat_widget
            .selection_view_search_query(AGENT_PICKER_VIEW_ID);
        let mut initial_selected_idx = None;
        let mut items = Vec::new();
        let mut closed_agent_count = 0;
        for (idx, (thread_id, entry)) in self
            .agent_navigation
            .ordered_threads()
            .into_iter()
            .enumerate()
        {
            if self.active_thread_id == Some(thread_id) {
                initial_selected_idx = Some(idx);
            }
            let id = thread_id;
            let is_primary = self.primary_thread_id == Some(thread_id);
            let name = format_agent_picker_item_label(
                entry.agent_nickname.as_deref(),
                entry.agent_role.as_deref(),
                entry.agent_path.as_deref(),
                is_primary,
            );
            let usage = self.agent_picker_thread_usage(thread_id, entry).await;
            let description = format_agent_picker_item_description(thread_id, entry, &usage);
            let selected_description =
                format_agent_picker_item_selected_description(thread_id, entry, &usage);
            let status_terms = match (entry.is_running, entry.has_system_error, entry.is_closed) {
                (true, _, _) => "live active open",
                (false, true, _) => "system error failed inspect replay",
                (false, false, true) => {
                    if !is_primary {
                        closed_agent_count += 1;
                    }
                    "closed stale finished"
                }
                (false, false, false) => "idle inactive open",
            };
            let search_value =
                format!("{name} {description} {selected_description} {status_terms}");
            items.push(SelectionItem {
                name,
                name_prefix_spans: agent_picker_status_dot_spans(
                    entry.is_closed,
                    entry.has_system_error,
                ),
                description: Some(description),
                selected_description: Some(selected_description),
                is_current: self.active_thread_id == Some(thread_id),
                hidden_when_unfiltered: !is_primary && entry.is_closed,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::SelectAgentThread(id));
                })],
                dismiss_on_select: true,
                search_value: Some(search_value),
                ..Default::default()
            });
        }

        let has_more_closed_agents = self.agent_navigation.next_picker_page_cursor().is_some();
        if has_more_closed_agents {
            items.push(SelectionItem {
                name: "Load more historical sidecars".to_string(),
                description: Some(
                    "Load the next bounded page, including older finished subagents.".to_string(),
                ),
                selected_description: Some(
                    "Load the next bounded page without clearing this filter.".to_string(),
                ),
                hidden_when_unfiltered: true,
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::LoadMoreAgentPickerPage);
                })],
                // Continuation refreshes the active picker instead of selecting a thread, so it
                // must keep the modal and its search query open.
                dismiss_on_select: false,
                search_value: Some("closed stale finished historical older more page".to_string()),
                ..Default::default()
            });
        }

        let closed_suffix = if closed_agent_count == 1 {
            "sidecar"
        } else {
            "sidecars"
        };
        let closed_page_hint = if has_more_closed_agents {
            " More are available in the next page."
        } else {
            ""
        };
        let subtitle = if closed_agent_count == 0 && !has_more_closed_agents {
            AgentNavigationState::picker_subtitle()
        } else {
            format!(
                "{} {closed_agent_count} closed {closed_suffix} cached.{closed_page_hint}",
                AgentNavigationState::picker_subtitle()
            )
        };
        self.chat_widget.replace_or_show_selection_view(
            AGENT_PICKER_VIEW_ID,
            SelectionViewParams {
                view_id: Some(AGENT_PICKER_VIEW_ID),
                title: Some("Subagents".to_string()),
                subtitle: Some(subtitle),
                footer_hint: Some(standard_popup_hint_line()),
                is_searchable: true,
                initial_search_query: prior_search_query,
                search_placeholder: Some("Search agents or type 'closed'".to_string()),
                items,
                initial_selected_idx,
                ..Default::default()
            },
        );
    }

    async fn agent_picker_thread_usage(
        &self,
        thread_id: ThreadId,
        entry: &crate::multi_agents::AgentPickerThreadEntry,
    ) -> AgentPickerThreadUsage {
        let mut usage = AgentPickerThreadUsage {
            model: entry.model.clone(),
            reasoning_effort: entry.reasoning_effort.clone(),
            task_name: entry.task_name.clone().or_else(|| entry.agent_path.clone()),
            ..Default::default()
        };

        if let Some(channel) = self.thread_event_channels.get(&thread_id) {
            let store = channel.store.lock().await;
            if let Some(session) = &store.session {
                if usage.model.is_none() && !session.model.trim().is_empty() {
                    usage.model = Some(session.model.clone());
                }
                if usage.reasoning_effort.is_none() {
                    usage.reasoning_effort = session.reasoning_effort.clone();
                }
                usage.approval_policy = Some(session.approval_policy);
                usage.approvals_reviewer = Some(session.approvals_reviewer);
                usage.sandbox_policy = session
                    .permission_profile
                    .to_legacy_sandbox_policy(session.cwd.as_path())
                    .ok()
                    .map(Into::into);
            }
        }

        if self.active_thread_id == Some(thread_id) {
            let token_usage = self.chat_widget.token_usage();
            usage.token_usage = ProtocolTokenUsage {
                input_tokens: token_usage.input_tokens,
                cached_input_tokens: token_usage.cached_input_tokens,
                cache_write_input_tokens: token_usage.cache_write_input_tokens,
                output_tokens: token_usage.output_tokens,
                reasoning_output_tokens: token_usage.reasoning_output_tokens,
                total_tokens: token_usage.total_tokens,
            };
        }

        usage
    }

    pub(super) fn is_terminal_thread_read_error(err: &color_eyre::Report) -> bool {
        err.chain()
            .any(|cause| cause.to_string().contains("thread not loaded:"))
    }

    fn is_invalid_thread_list_cursor_error(err: &color_eyre::Report) -> bool {
        err.chain()
            .any(|cause| cause.to_string().contains("invalid cursor:"))
    }

    pub(super) fn closed_state_for_thread_read_error(
        err: &color_eyre::Report,
        existing_is_closed: Option<bool>,
    ) -> bool {
        Self::is_terminal_thread_read_error(err) || existing_is_closed.unwrap_or(false)
    }

    pub(super) fn can_fallback_from_include_turns_error(err: &color_eyre::Report) -> bool {
        err.chain().any(|cause| {
            let message = cause.to_string();
            message.contains("includeTurns is unavailable before first user message")
                || message.contains("ephemeral threads do not support includeTurns")
        })
    }

    /// Updates cached picker metadata and then mirrors any visible-label change into the footer.
    ///
    /// These two writes stay paired so the picker rows and contextual footer continue to describe
    /// the same displayed thread after nickname or role updates.
    pub(super) fn upsert_agent_picker_thread(
        &mut self,
        thread_id: ThreadId,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
        is_closed: bool,
    ) {
        self.chat_widget.set_collab_agent_metadata(
            thread_id,
            agent_nickname.clone(),
            agent_role.clone(),
        );
        self.agent_navigation.upsert(
            thread_id,
            agent_nickname,
            agent_role,
            is_closed,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        self.sync_agent_picker_identity(thread_id);
        self.sync_active_agent_label();
    }

    /// Applies a pushed app-server status transition, which is newer than a later picker backfill
    /// read and can therefore explicitly recover or close a row that was marked failed by an
    /// errored activity.
    pub(super) fn apply_agent_picker_thread_status_change(
        &mut self,
        thread_id: ThreadId,
        notification: &ThreadStatusChangedNotification,
    ) {
        let has_live_channel = self
            .thread_event_channels
            .get(&thread_id)
            .is_some_and(ThreadEventChannel::has_live_attachment);
        self.apply_agent_picker_thread_status_change_with_liveness(
            thread_id,
            notification,
            has_live_channel,
        );
    }

    /// Applies a watcher status using liveness observed before any notification-local channel was
    /// allocated. This keeps a synthetic buffering channel from falsely proving `NotLoaded` is
    /// still live.
    pub(super) fn apply_agent_picker_thread_status_change_with_liveness(
        &mut self,
        thread_id: ThreadId,
        notification: &ThreadStatusChangedNotification,
        has_live_channel: bool,
    ) {
        let status = agent_picker_thread_status(&notification.status, has_live_channel);
        if !self.agent_navigation.accepts_thread_status_change(
            thread_id,
            status.has_system_error,
            notification.status_revision,
            status.is_closed,
        ) {
            return;
        }
        if status.is_closed {
            if self.agent_navigation.get(&thread_id).is_some() {
                self.mark_agent_picker_thread_closed(thread_id);
            }
            return;
        }
        if self.agent_navigation.get(&thread_id).is_none() {
            return;
        }

        self.agent_navigation.reopen_after_newer_status(thread_id);
        self.agent_navigation
            .set_system_error(thread_id, status.has_system_error);
        if status.is_running {
            self.agent_navigation.mark_running(thread_id);
        } else {
            self.agent_navigation
                .set_running(thread_id, /*is_running*/ false);
        }
    }

    pub(super) fn sync_agent_picker_identity(&mut self, thread_id: ThreadId) {
        let Some(entry) = self.agent_navigation.get(&thread_id).cloned() else {
            return;
        };
        self.chat_widget.set_collab_agent_identity(
            thread_id,
            crate::multi_agents::AgentMetadata {
                agent_nickname: entry.agent_nickname,
                agent_role: entry.agent_role,
                agent_path: entry.agent_path,
                model: entry.model,
                reasoning_effort: entry.reasoning_effort,
            },
        );
    }

    /// Persists the app-server's authoritative ownership flag and updates the active composer.
    pub(super) fn mark_primary_thread_parent_owned(&mut self, thread_id: ThreadId) {
        self.agent_navigation.mark_parent_owned(thread_id);
        self.chat_widget.set_parent_owned_thread();
    }

    /// Marks a cached picker thread closed and recomputes the contextual footer label.
    ///
    /// Closing a thread is not the same as removing it: users can still inspect finished agent
    /// transcripts, and the stable next/previous traversal order should not collapse around them.
    pub(super) fn mark_agent_picker_thread_closed(&mut self, thread_id: ThreadId) {
        self.agent_navigation.mark_closed(thread_id);
        self.sync_active_agent_label();
    }

    pub(super) async fn refresh_agent_picker_thread_liveness(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> bool {
        let existing_entry = self.agent_navigation.get(&thread_id).cloned();
        let error_observation_generation = self
            .agent_navigation
            .system_error_observation_generation(thread_id);
        let has_replay_channel = self.thread_event_channels.contains_key(&thread_id);
        match app_server
            .thread_read(thread_id, /*include_turns*/ false)
            .await
        {
            Ok(thread) => {
                let is_parent_owned = thread_blocks_direct_input(&thread);
                let agent_path = source_agent_path(&thread.source);
                let status =
                    agent_picker_thread_status(&thread.status, /*has_live_channel*/ false);
                self.upsert_agent_picker_thread(
                    thread_id,
                    thread.agent_nickname.or_else(|| {
                        existing_entry
                            .as_ref()
                            .and_then(|entry| entry.agent_nickname.clone())
                    }),
                    thread.agent_role.or_else(|| {
                        existing_entry
                            .as_ref()
                            .and_then(|entry| entry.agent_role.clone())
                    }),
                    status.is_closed,
                );
                if is_parent_owned {
                    self.agent_navigation.mark_parent_owned(thread_id);
                }
                self.agent_navigation.set_agent_path(thread_id, agent_path);
                self.agent_navigation.update_identity(
                    thread_id,
                    thread.model.clone(),
                    thread.reasoning_effort.clone(),
                    Some(thread.model_provider.clone()),
                    thread.name.clone(),
                );
                self.agent_navigation.set_timestamps(
                    thread_id,
                    Some(thread.created_at),
                    Some(thread.updated_at),
                );
                if status.has_system_error {
                    self.agent_navigation
                        .confirm_system_error_from_authoritative_status(
                            thread_id, /*status_revision*/ None,
                        );
                } else if !status.is_closed {
                    self.agent_navigation
                        .clear_system_error_from_authoritative_read(
                            thread_id,
                            error_observation_generation,
                        );
                }
                self.sync_agent_picker_identity(thread_id);
                let keeps_system_error = self
                    .agent_navigation
                    .get(&thread_id)
                    .is_some_and(|entry| entry.has_system_error);
                if status.is_running && !keeps_system_error {
                    self.agent_navigation.mark_running(thread_id);
                } else {
                    self.agent_navigation
                        .set_running(thread_id, /*is_running*/ false);
                }
                true
            }
            Err(err) => self.handle_agent_picker_thread_liveness_read_error(
                thread_id,
                has_replay_channel,
                &err,
            ),
        }
    }

    /// Keeps the last authoritative picker state on a non-terminal liveness-read failure.
    ///
    /// In particular, `upsert_agent_picker_thread` initializes a fresh status record. Preserve a
    /// previously observed `SystemError` while the next `thread/read` is unavailable instead of
    /// presenting a transient transport failure as a recovered agent.
    pub(super) fn handle_agent_picker_thread_liveness_read_error(
        &mut self,
        thread_id: ThreadId,
        has_replay_channel: bool,
        err: &color_eyre::Report,
    ) -> bool {
        let existing_entry = self.agent_navigation.get(&thread_id).cloned();
        let is_terminal = Self::is_terminal_thread_read_error(err);
        if is_terminal && !has_replay_channel {
            self.agent_navigation.remove(thread_id);
            return false;
        }
        let is_closed = Self::closed_state_for_thread_read_error(
            err,
            existing_entry.as_ref().map(|entry| entry.is_closed),
        );
        // A terminal not-loaded read is authoritative closure, even when the replay channel keeps
        // the row available for inspection. Do not let a previous SystemError shadow its `closed`
        // search status; non-terminal reads still preserve the most recently known error state.
        let has_system_error = !is_terminal
            && existing_entry
                .as_ref()
                .is_some_and(|entry| entry.has_system_error);
        if let Some(entry) = existing_entry {
            self.upsert_agent_picker_thread(
                thread_id,
                entry.agent_nickname,
                entry.agent_role,
                is_closed,
            );
        } else {
            self.upsert_agent_picker_thread(
                thread_id, /*agent_nickname*/ None, /*agent_role*/ None, is_closed,
            );
        }
        self.agent_navigation
            .set_system_error(thread_id, has_system_error);
        self.agent_navigation
            .set_running(thread_id, /*is_running*/ false);
        true
    }

    /// Materializes a live thread into local replay state when the picker knows about it but the
    /// TUI has not cached a local event channel yet.
    ///
    /// Resume-time backfill intentionally avoids creating empty placeholder channels, because those
    /// placeholders make stale `/agent` entries open blank transcripts. When a user later selects a
    /// still-live discovered thread, attach it on demand with a real resumed snapshot.
    pub(super) async fn attach_live_thread_for_selection(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<bool> {
        if self
            .thread_event_channels
            .get(&thread_id)
            .is_some_and(|channel| {
                channel.attachment() != ThreadEventAttachment::NotificationBuffer
            })
        {
            return Ok(true);
        }

        let (session, turns, live_attached, thread_subscription_id) = match app_server
            .resume_thread(self.config.clone(), thread_id, self.resume_model_settings())
            .await
        {
            Ok(started) => {
                if started.blocks_direct_input {
                    self.agent_navigation.mark_parent_owned(thread_id);
                }
                (
                    started.session,
                    started.turns,
                    true,
                    started.thread_subscription_id,
                )
            }
            Err(resume_err) => {
                tracing::warn!(
                    thread_id = %thread_id,
                    error = %resume_err,
                    "failed to resume live thread for selection; falling back to thread/read"
                );
                let (thread, turns) = match app_server
                    .thread_read(thread_id, /*include_turns*/ true)
                    .await
                {
                    Ok(thread) => {
                        let turns = thread.turns.clone();
                        (thread, turns)
                    }
                    Err(err) if Self::can_fallback_from_include_turns_error(&err) => {
                        let thread = app_server
                            .thread_read(thread_id, /*include_turns*/ false)
                            .await?;
                        (thread, Vec::new())
                    }
                    Err(err) => return Err(err),
                };
                if turns.is_empty() {
                    // A `thread/read` fallback without turns would create a blank local replay
                    // channel with no live listener attached, which blocks later real re-attach.
                    return Err(color_eyre::eyre::eyre!(
                        "Agent thread {thread_id} is not yet available for replay or live attach."
                    ));
                }
                let mut session = self.session_state_for_thread_read(thread_id, &thread).await;
                // `thread/read` can seed replay state, but it does not attach the app-server
                // listener that `thread/resume` establishes, so treat this path as replay-only.
                session.model.clear();
                (session, turns, false, None)
            }
        };
        // A successful explicit resume/read is positive recovery evidence. It is the only path
        // in this selection flow allowed to replace a previous local discard tombstone.
        self.mark_thread_attached(thread_id);
        let channel = self.ensure_thread_channel(thread_id);
        if live_attached {
            channel.mark_live();
        } else {
            channel.mark_replay_only();
        }
        let mut store = channel.store.lock().await;
        store.set_session(session, turns);
        // The authoritative resume/read snapshot already contains ordinary lifecycle and
        // transcript state. Keep only request-like state that the snapshot cannot replace
        // before the picker replays this channel into a fresh ChatWidget.
        store.rebase_buffer_after_session_refresh();
        drop(store);
        if live_attached {
            self.bind_thread_subscription_and_flush(app_server, thread_id, thread_subscription_id)
                .await;
        }
        Ok(live_attached)
    }

    /// Materializes a closed saved thread for transcript replay without reviving it in the
    /// app-server thread manager.
    pub(super) async fn attach_replay_thread_for_selection(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<()> {
        let thread = app_server
            .thread_read(thread_id, /*include_turns*/ true)
            .await?;
        let turns = thread.turns.clone();
        let mut session = self.session_state_for_thread_read(thread_id, &thread).await;
        // `thread/read` returns saved state only; never present it as a live app-server attach.
        session.model.clear();

        // The user explicitly opened this saved transcript; allow it to reappear only as a
        // replay-only recovery, never as a passive notification-driven attachment.
        self.mark_thread_attached(thread_id);
        let channel = self.ensure_thread_channel(thread_id);
        channel.mark_replay_only();
        let mut store = channel.store.lock().await;
        store.set_session(session, turns);
        // `thread/read(includeTurns)` is authoritative for a closed transcript too. Avoid
        // replaying its pre-attach notification buffer on top of the returned turns.
        store.rebase_buffer_after_session_refresh();
        Ok(())
    }

    /// Replaces the chat widget and re-seeds the new widget's collab metadata from the navigation
    /// cache.
    ///
    /// Thread switches reconstruct the `ChatWidget`, which loses the `collab_agent_metadata` map.
    /// This helper copies every known nickname/role from `AgentNavigationState` into the
    /// replacement widget so that replayed collab items render agent names immediately.
    pub(super) fn replace_chat_widget(&mut self, mut chat_widget: ChatWidget) {
        // Transfer the last-written terminal title to the replacement widget
        // so it knows what OSC title is currently displayed. Without this, the
        // new widget would redundantly clear and rewrite the same title, causing
        // a visible flicker in some terminals.
        let previous_terminal_title = self.chat_widget.last_terminal_title.take();
        if chat_widget.last_terminal_title.is_none() {
            chat_widget.last_terminal_title = previous_terminal_title;
        }
        chat_widget.remote_connection = self.chat_widget.remote_connection.clone();
        for (thread_id, entry) in self.agent_navigation.ordered_threads() {
            chat_widget.set_collab_agent_identity(
                thread_id,
                crate::multi_agents::AgentMetadata {
                    agent_nickname: entry.agent_nickname.clone(),
                    agent_role: entry.agent_role.clone(),
                    agent_path: entry.agent_path.clone(),
                    model: entry.model.clone(),
                    reasoning_effort: entry.reasoning_effort.clone(),
                },
            );
        }
        self.chat_widget = chat_widget;
        self.sync_active_agent_label();
    }

    pub(super) async fn select_agent_thread(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<()> {
        if self.active_thread_id == Some(thread_id) {
            return Ok(());
        }

        // A tracked side thread stays loaded until it is explicitly discarded and already has a
        // replay channel, so another liveness read cannot add anything before selection.
        if !(self.side_threads.contains_key(&thread_id)
            && self.thread_event_channels.contains_key(&thread_id)
            || self
                .refresh_agent_picker_thread_liveness(app_server, thread_id)
                .await)
        {
            self.chat_widget
                .add_error_message(format!("Agent thread {thread_id} is no longer available."));
            return Ok(());
        }
        let mut is_replay_only = self
            .agent_navigation
            .get(&thread_id)
            .is_some_and(|entry| entry.is_closed);
        let mut attached_replay_only = false;
        if is_replay_only {
            if let Err(err) = self
                .attach_replay_thread_for_selection(app_server, thread_id)
                .await
            {
                self.chat_widget.add_error_message(format!(
                    "Failed to load saved transcript for agent thread {thread_id}: {err}"
                ));
                return Ok(());
            }
        } else if self.should_attach_live_thread_for_selection(thread_id) {
            match self
                .attach_live_thread_for_selection(app_server, thread_id)
                .await
            {
                Ok(live_attached) => {
                    attached_replay_only = !live_attached;
                    if attached_replay_only {
                        is_replay_only = true;
                    }
                }
                Err(err) => {
                    self.chat_widget.add_error_message(format!(
                        "Failed to attach to agent thread {thread_id}: {err}"
                    ));
                    return Ok(());
                }
            }
        }
        let previous_thread_id = self.active_thread_id;
        self.store_active_thread_receiver().await;
        self.active_thread_id = None;
        let Some((receiver, mut snapshot)) = self.activate_thread_for_replay(thread_id).await
        else {
            self.chat_widget
                .add_error_message(format!("Agent thread {thread_id} is already active."));
            if let Some(previous_thread_id) = previous_thread_id {
                self.activate_thread_channel(previous_thread_id).await;
            }
            return Ok(());
        };

        self.refresh_snapshot_session_if_needed(
            app_server,
            thread_id,
            is_replay_only,
            &mut snapshot,
        )
        .await;
        let blocks_direct_input = self.agent_navigation.is_parent_owned(thread_id);

        self.active_thread_id = Some(thread_id);
        self.active_thread_rx = Some(receiver);

        let init = self.chatwidget_init_for_forked_or_resumed_thread(
            tui,
            self.config.clone(),
            /*initial_user_message*/ None,
        );
        self.replace_chat_widget(ChatWidget::new_with_app_event(init));
        if is_replay_only {
            self.chat_widget.set_replay_only_thread();
        } else if blocks_direct_input {
            self.chat_widget.set_parent_owned_thread();
        }

        self.reset_for_thread_switch(tui)?;
        self.replay_thread_snapshot(snapshot, !is_replay_only);
        if is_replay_only {
            let message = if attached_replay_only {
                format!(
                    "Agent thread {thread_id} could not be resumed live. Replaying saved transcript."
                )
            } else {
                format!("Agent thread {thread_id} is closed. Replaying saved transcript.")
            };
            self.chat_widget.add_info_message(message, /*hint*/ None);
        }
        self.drain_active_thread_events(tui).await?;
        self.refresh_pending_thread_approvals().await;

        Ok(())
    }

    /// Returns whether selection still needs a channel materialized from the saved session.
    ///
    /// Closed and system-error descendants are intentionally included: their persisted rollouts
    /// remain inspectable even though they cannot attach as live agents.
    pub(super) fn should_attach_live_thread_for_selection(&self, thread_id: ThreadId) -> bool {
        self.thread_event_channels
            .get(&thread_id)
            .is_none_or(|channel| channel.attachment() == ThreadEventAttachment::NotificationBuffer)
    }

    pub(super) fn reset_for_thread_switch(&mut self, tui: &mut tui::Tui) -> Result<()> {
        self.reset_transcript_state_after_clear();
        tui.clear_pending_history_lines();
        Self::clear_terminal_for_thread_switch(&mut tui.terminal)?;
        Ok(())
    }

    pub(super) fn clear_terminal_for_thread_switch<B>(
        terminal: &mut crate::custom_terminal::Terminal<B>,
    ) -> Result<()>
    where
        B: Backend + Write,
    {
        terminal.clear_scrollback_and_visible_screen_ansi()?;
        let mut area = terminal.viewport_area;
        if area.y > 0 {
            area.y = 0;
            terminal.set_viewport_area(area);
        }
        Ok(())
    }

    pub(super) async fn reset_thread_event_state(&mut self, app_server: Option<&AppServerSession>) {
        let mut thread_ids = HashSet::new();
        thread_ids.extend(self.thread_event_channels.keys().copied());
        thread_ids.extend(self.side_threads.keys().copied());
        thread_ids.extend(
            self.pending_primary_events
                .iter()
                .map(|event| event.thread_id),
        );
        thread_ids.extend(self.pending_app_server_requests.pending_thread_ids());
        thread_ids.extend(
            [
                self.active_thread_id,
                self.primary_thread_id,
                self.chat_widget.thread_id(),
            ]
            .into_iter()
            .flatten(),
        );
        for thread_id in thread_ids {
            if self.thread_is_discarded(thread_id) {
                continue;
            }
            if let Some(app_server) = app_server {
                self.discard_thread_local_state(app_server, thread_id).await;
            } else {
                self.mark_thread_discarded(thread_id);
                self.pending_app_server_requests.clear_thread(thread_id);
            }
        }
        self.thread_event_channels.clear();
        self.agent_navigation.clear();
        self.side_threads.clear();
        self.active_thread_id = None;
        self.active_thread_rx = None;
        self.primary_thread_id = None;
        self.last_subagent_backfill_attempt = None;
        self.primary_session_configured = None;
        self.pending_primary_events.clear();
        self.pending_app_server_requests.clear();
        self.pending_startup_thread_start = false;
        self.chat_widget.set_pending_thread_approvals(Vec::new());
        self.sync_active_agent_label();
    }

    pub(super) async fn handle_startup_thread_started(
        &mut self,
        app_server: &mut AppServerSession,
        result: Result<AppServerStartedThread, String>,
    ) -> Result<()> {
        if !self.pending_startup_thread_start {
            if let Ok(started) = result {
                let thread_id = started.session.thread_id;
                // The start response may race queued listener traffic even
                // though this startup result is no longer wanted. Bind then
                // tombstone it so those original frames are rejected rather
                // than deferred forever or attributed to a later same-id UI.
                self.mark_thread_attached(thread_id);
                self.bind_thread_subscription(thread_id, started.thread_subscription_id);
                if let Err(err) = app_server.thread_unsubscribe(thread_id).await {
                    tracing::warn!(
                        thread_id = %thread_id,
                        "failed to unsubscribe stale startup thread: {err}"
                    );
                }
                self.discard_thread_local_state(app_server, thread_id).await;
            }
            return Ok(());
        }

        self.pending_startup_thread_start = false;
        self.chat_widget
            .set_queue_submissions_until_session_configured(/*queue*/ false);
        match result {
            Ok(started) => {
                if started.blocks_direct_input {
                    self.mark_primary_thread_parent_owned(started.session.thread_id);
                }
                self.enqueue_primary_thread_session_with_presentation_and_server(
                    Some(app_server),
                    started.thread_subscription_id,
                    started.session,
                    started.turns,
                    ThreadAttachPresentation::SessionLineage,
                )
                .await?;
                self.chat_widget.maybe_send_next_queued_input();
            }
            Err(err) => {
                return Err(color_eyre::eyre::eyre!(
                    "Failed to start a fresh session through the app server: {err}"
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn start_fresh_session_with_summary_hint(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        session_start_source: Option<ThreadStartSource>,
        initial_user_message: Option<crate::chatwidget::UserMessage>,
        new_thread_name: Option<String>,
    ) {
        // Start a fresh in-memory session while preserving resumability via persisted rollout
        // history. If an initial message is provided, `enqueue_primary_thread_session` suppresses it
        // until the new session is configured and any replayed turns have been rendered.
        self.refresh_in_memory_config_from_disk_best_effort("starting a new thread")
            .await;
        let model = self.chat_widget.current_model().to_string();
        let mut config = self.fresh_session_config();
        apply_managed_new_thread_defaults(
            &mut config,
            app_server.managed_new_thread_defaults(),
            &self.cli_kv_overrides,
            &self.harness_overrides,
        );
        let summary = session_summary(
            self.chat_widget.token_usage(),
            self.chat_widget.thread_id(),
            self.chat_widget.thread_name(),
            self.chat_widget.rollout_path().as_deref(),
        );
        self.shutdown_current_thread(app_server).await;
        let tracked_thread_ids: Vec<ThreadId> =
            self.thread_event_channels.keys().copied().collect();
        for thread_id in tracked_thread_ids {
            if self.thread_is_replay_only(thread_id) {
                continue;
            }
            if let Err(err) = app_server.thread_unsubscribe(thread_id).await {
                tracing::warn!("failed to unsubscribe tracked thread {thread_id}: {err}");
            }
        }
        self.config = config.clone();
        match app_server
            .start_thread_with_session_start_source(&config, session_start_source)
            .await
        {
            Ok(mut started) => {
                let name_error = if let Some(name) = new_thread_name {
                    match app_server
                        .thread_set_name(started.session.thread_id, name.clone())
                        .await
                    {
                        Ok(()) => {
                            started.session.thread_name = Some(name);
                            None
                        }
                        Err(err) => Some(format!("Failed to name the new session: {err}")),
                    }
                } else {
                    None
                };
                if let Err(err) = self
                    .replace_chat_widget_with_app_server_thread(
                        tui,
                        app_server,
                        started,
                        ThreadAttachPresentation::SessionLineage,
                        initial_user_message,
                    )
                    .await
                {
                    self.chat_widget.add_error_message(format!(
                        "Failed to attach to fresh app-server thread: {err}"
                    ));
                } else {
                    if let Some(err) = name_error {
                        self.chat_widget.add_error_message(err);
                    }
                    if let Some(summary) = summary {
                        let mut lines: Vec<Line<'static>> = Vec::new();
                        if let Some(usage_line) = summary.usage_line {
                            lines.push(usage_line.into());
                        }
                        if let Some(command) = summary.resume_hint {
                            let spans =
                                vec!["To continue this session, run ".into(), command.cyan()];
                            lines.push(spans.into());
                        }
                        self.chat_widget.add_plain_history_lines(lines);
                    }
                }
            }
            Err(err) => {
                self.chat_widget.add_error_message(format!(
                    "Failed to start a fresh session through the app server: {err}"
                ));
                self.config.model = Some(model);
            }
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) async fn replace_chat_widget_with_app_server_thread(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &AppServerSession,
        started: AppServerStartedThread,
        presentation: ThreadAttachPresentation,
        initial_user_message: Option<crate::chatwidget::UserMessage>,
    ) -> Result<()> {
        self.prepare_chat_widget_for_app_server_thread(tui, app_server, initial_user_message)
            .await;
        if started.blocks_direct_input {
            self.mark_primary_thread_parent_owned(started.session.thread_id);
        }
        self.enqueue_primary_thread_session_with_presentation_and_server(
            Some(app_server),
            started.thread_subscription_id,
            started.session,
            started.turns,
            presentation,
        )
        .await?;
        Ok(())
    }

    /// Clears thread-local state and installs a fresh widget before attaching an app-server
    /// thread. Resume uses this setup independently so it can hydrate descendant identity before
    /// replaying persisted primary turns.
    async fn prepare_chat_widget_for_app_server_thread(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &AppServerSession,
        initial_user_message: Option<crate::chatwidget::UserMessage>,
    ) {
        // Initial messages are for freshly attached primary threads only. Thread switches and
        // resume/fork flows pass `None` so they cannot replay old history and then auto-submit a new
        // user turn by accident.
        self.reset_thread_event_state(Some(app_server)).await;
        let init = self.chatwidget_init_for_forked_or_resumed_thread(
            tui,
            self.config.clone(),
            initial_user_message,
        );
        self.replace_chat_widget(ChatWidget::new_with_app_event(init));
    }

    /// Fetches one bounded page of persisted descendants from the app server and registers them
    /// in the navigation cache and chat widget metadata.
    ///
    /// Called when opening the `/agent` picker and after resuming a thread so that the picker and
    /// keyboard navigation are pre-populated even if the TUI did not witness the original spawn
    /// events. Fresh and forked threads cannot have pre-existing descendants.
    ///
    /// The historical page is deliberately small and relation-filtered at the app server so
    /// opening `/agent` does not enumerate every saved descendant and then issue one
    /// `thread/read` request per row. The loaded-descendant priority query uses one finite
    /// relation page, while historical sidecars remain bounded and on demand under the `closed`
    /// filter.
    pub(super) async fn backfill_loaded_subagent_threads(
        &mut self,
        app_server: &mut AppServerSession,
    ) -> LoadedSubagentBackfill {
        self.backfill_agent_picker_page(app_server, /*cursor*/ None)
            .await
    }

    async fn backfill_agent_picker_page(
        &mut self,
        app_server: &mut AppServerSession,
        cursor: Option<String>,
    ) -> LoadedSubagentBackfill {
        let Some(primary_thread_id) = self.primary_thread_id else {
            return LoadedSubagentBackfill::default();
        };

        let is_continuation = cursor.is_some();
        let mut refreshed_thread_ids = HashSet::new();
        // Current loaded descendants are independent from the persisted relation index. Fetch
        // them before the historical page so a transient state-db failure cannot hide a healthy
        // live/idle sidecar from the picker or keyboard navigation.
        let loaded_priority_completed = if !is_continuation {
            self.backfill_loaded_priority_subagent_threads(
                app_server,
                primary_thread_id,
                &mut refreshed_thread_ids,
            )
            .await
        } else {
            true
        };
        let response = match app_server
            .thread_list(ThreadListParams {
                cursor,
                limit: Some(AGENT_PICKER_PAGE_SIZE),
                sort_key: Some(ThreadSortKey::UpdatedAt),
                sort_direction: Some(SortDirection::Desc),
                model_providers: None,
                source_kinds: None,
                thread_sources: None,
                archived: Some(false),
                is_pinned: None,
                cwd: None,
                // The state database carries the descendant relationship and current liveness,
                // making this a bounded index query instead of a rollout scan plus per-id reads.
                use_state_db_only: true,
                search_term: None,
                parent_thread_id: None,
                ancestor_thread_id: Some(primary_thread_id.to_string()),
            })
            .await
        {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(
                    %err,
                    "failed to list persisted descendants for subagent backfill"
                );
                // A continuation cursor names a row in a relation set that may have shrunk since
                // the picker was last opened. An invalid cursor cannot succeed on retry, so clear
                // it rather than leaving a permanently clickable "Load more" item. Other errors
                // remain retryable.
                if is_continuation && Self::is_invalid_thread_list_cursor_error(&err) {
                    self.agent_navigation
                        .set_next_picker_page_cursor(/*next_cursor*/ None);
                }
                self.sync_active_agent_label();
                return LoadedSubagentBackfill {
                    // Keep the persisted-page failure retryable, even if the independent loaded
                    // priority query succeeded.
                    completed: false,
                    refreshed_thread_ids,
                };
            }
        };

        let ancestor_filter_applied = response.ancestor_filter_applied;

        // `thread/list.ancestorThreadId` predates the acknowledgement on this response. When an
        // older server applies that stable filter but omits the acknowledgement, read one bounded
        // compatibility window and still locally reconstruct every accepted relationship. This
        // keeps a currently loaded descendant visible when it falls just beyond the ordinary 50
        // row picker page, without trusting an unacknowledged response or reading global loaded
        // ids one at a time.
        let unacknowledged_relation_threads = if !ancestor_filter_applied && !is_continuation {
            match app_server
                .thread_list(ThreadListParams {
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
                    ancestor_thread_id: Some(primary_thread_id.to_string()),
                })
                .await
            {
                Ok(response) => Some(response.data),
                Err(err) => {
                    tracing::debug!(
                        %err,
                        "bounded compatibility relation lookup was unavailable"
                    );
                    // The ordinary 50-row response cannot safely stand in for this required
                    // relation window: it may be full of newer closed descendants while a loaded
                    // child falls just beyond it. Match persisted-page failures by leaving this
                    // backfill incomplete, so keyboard navigation does not cache the attempt and
                    // can retry without widening the query or reading global metadata.
                    self.sync_active_agent_label();
                    return LoadedSubagentBackfill {
                        completed: false,
                        refreshed_thread_ids,
                    };
                }
            }
        } else {
            None
        };

        // A normal `/agent` reopen refreshes the first page for liveness but must not rewind an
        // available cached continuation after the user has already loaded page two. A first page
        // with no continuation is authoritative, though: it means the relation set has shrunk
        // (or is exhausted), so clear any stale "Load more" action before the user can click it.
        if ancestor_filter_applied
            && (is_continuation
                || response.next_cursor.is_none()
                || self.agent_navigation.next_picker_page_cursor().is_none())
        {
            self.agent_navigation
                .set_next_picker_page_cursor(response.next_cursor.clone());
        } else if !ancestor_filter_applied {
            // An older server can silently ignore an unknown ancestorThreadId. Its cursor names
            // an unfiltered global list, so do not present it as a descendant continuation.
            self.agent_navigation
                .set_next_picker_page_cursor(/*next_cursor*/ None);
        }

        // The historical relation page is sorted by update time, so it can be filled with closed
        // descendants. The loaded priority set was already merged above, so open and idle
        // sidecars stay visible and keyboard-reachable without making finished history part of
        // the default picker view.
        if ancestor_filter_applied {
            for thread in response.data {
                self.register_agent_picker_thread_from_backend(
                    primary_thread_id,
                    thread,
                    &mut refreshed_thread_ids,
                );
            }
        } else {
            tracing::debug!(
                "persisted descendant response did not acknowledge the ancestor filter; \
                 locally verifying returned threads"
            );
            let relation_threads = unacknowledged_relation_threads.unwrap_or(response.data);
            let descendant_thread_ids = find_loaded_subagent_threads_for_primary(
                relation_threads.clone(),
                primary_thread_id,
            )
            .into_iter()
            .map(|thread| thread.thread_id)
            .collect::<HashSet<_>>();
            for thread in relation_threads {
                let is_descendant = ThreadId::from_string(&thread.id)
                    .is_ok_and(|thread_id| descendant_thread_ids.contains(&thread_id));
                if is_descendant {
                    self.register_agent_picker_thread_from_backend(
                        primary_thread_id,
                        thread,
                        &mut refreshed_thread_ids,
                    );
                }
            }
        }
        let legacy_fallback_completed =
            if !is_continuation && self.agent_navigation.needs_legacy_relation_fallback_check() {
                let completed = self
                    .backfill_loaded_legacy_subagent_threads(
                        app_server,
                        primary_thread_id,
                        &mut refreshed_thread_ids,
                    )
                    .await;
                if loaded_priority_completed && completed {
                    self.agent_navigation
                        .mark_legacy_relation_fallback_checked();
                }
                completed
            } else {
                true
            };
        self.sync_active_agent_label();

        LoadedSubagentBackfill {
            completed: loaded_priority_completed && legacy_fallback_completed,
            refreshed_thread_ids,
        }
    }

    /// Applies one app-server thread row to picker state while preserving any attached live channel.
    pub(super) fn register_agent_picker_thread_from_backend(
        &mut self,
        primary_thread_id: ThreadId,
        thread: Thread,
        refreshed_thread_ids: &mut HashSet<ThreadId>,
    ) {
        let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
            tracing::warn!(
                "ignoring persisted descendant with invalid id during subagent backfill"
            );
            return;
        };
        if thread_id == primary_thread_id {
            return;
        }

        let has_live_channel = self
            .thread_event_channels
            .get(&thread_id)
            .is_some_and(ThreadEventChannel::has_live_attachment);
        let status = agent_picker_thread_status(&thread.status, has_live_channel);
        if thread_blocks_direct_input(&thread) {
            self.agent_navigation.mark_parent_owned(thread_id);
        }
        self.upsert_agent_picker_thread(
            thread_id,
            thread.agent_nickname,
            thread.agent_role,
            status.is_closed,
        );
        self.agent_navigation
            .set_agent_path(thread_id, source_agent_path(&thread.source));
        self.agent_navigation.update_identity(
            thread_id,
            thread.model,
            thread.reasoning_effort,
            Some(thread.model_provider),
            thread.name,
        );
        self.agent_navigation.set_timestamps(
            thread_id,
            Some(thread.created_at),
            Some(thread.updated_at),
        );
        if status.has_system_error {
            self.agent_navigation
                .confirm_system_error_from_authoritative_status(
                    thread_id, /*status_revision*/ None,
                );
        }
        self.sync_agent_picker_identity(thread_id);
        // A live channel can have an empty store after a successful spawn. Only apply server
        // status for channels that would otherwise need another liveness read.
        if !has_live_channel {
            let keeps_system_error = self
                .agent_navigation
                .get(&thread_id)
                .is_some_and(|entry| entry.has_system_error);
            if status.is_running && !keeps_system_error {
                self.agent_navigation.mark_running(thread_id);
            } else {
                self.agent_navigation
                    .set_running(thread_id, /*is_running*/ false);
            }
            refreshed_thread_ids.insert(thread_id);
        }
    }

    /// Merges every currently loaded descendant ahead of historical picker rows.
    ///
    /// The app-server confirms that it filtered by the spawn-tree relationship before these ids
    /// are trusted. This deliberately consumes only a small, fixed number of finite loaded-list
    /// pages: a picker open must not traverse every loaded candidate or the primary's persisted
    /// historical subtree. Re-registration is idempotent and preserves first-seen navigation
    /// order.
    async fn backfill_loaded_priority_subagent_threads(
        &mut self,
        app_server: &mut AppServerSession,
        primary_thread_id: ThreadId,
        refreshed_thread_ids: &mut HashSet<ThreadId>,
    ) -> bool {
        let mut loaded_metadata_completed = true;
        let mut loaded_descendant_count = 0;
        let mut cursor = None;
        let mut pages_consumed = 0;

        while pages_consumed < AGENT_PICKER_LOADED_PRIORITY_MAX_PAGES {
            let response = match app_server
                .thread_loaded_list(ThreadLoadedListParams {
                    cursor,
                    limit: Some(AGENT_PICKER_LOADED_PRIORITY_PAGE_SIZE),
                    ancestor_thread_id: Some(primary_thread_id.to_string()),
                })
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    tracing::debug!(
                        %err,
                        "loaded descendant priority lookup was unavailable"
                    );
                    return false;
                }
            };
            if !response.ancestor_filter_applied {
                // Older servers silently ignore an unknown ancestorThreadId and return every
                // loaded thread. Do not turn that global id list into an unbounded thread/read
                // sweep. The bounded persisted and legacy relation paths below remain
                // responsible for old-server discovery, and an unrelated metadata failure must
                // not make this primary retry.
                tracing::debug!(
                    "loaded descendant response did not acknowledge the ancestor filter; \
                     skipping untrusted global metadata reads"
                );
                return true;
            }

            for thread_id in response.data {
                let Ok(thread_id) = ThreadId::from_string(&thread_id) else {
                    loaded_metadata_completed = false;
                    continue;
                };
                match app_server
                    .thread_read(thread_id, /*include_turns*/ false)
                    .await
                {
                    Ok(thread) => {
                        // The app server acknowledged the requested ancestor relation. Do not
                        // reconstruct it from only the loaded metadata here: an unloaded
                        // intermediary would otherwise hide a returned loaded nested descendant.
                        self.register_agent_picker_thread_from_backend(
                            primary_thread_id,
                            thread,
                            refreshed_thread_ids,
                        );
                        loaded_descendant_count += 1;
                    }
                    Err(err) => {
                        // A listed thread whose metadata cannot be read has not been covered by
                        // this priority pass. Leave it incomplete so the next first-page refresh
                        // retries this bounded loaded relation page while legacy scan-and-repair
                        // still runs below.
                        loaded_metadata_completed = false;
                        tracing::debug!(
                            %err,
                            %thread_id,
                            "loaded descendant priority metadata read failed"
                        );
                    }
                }
            }

            pages_consumed += 1;
            cursor = response.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        if loaded_descendant_count > 0 {
            tracing::debug!(
                descendants = loaded_descendant_count,
                pages = pages_consumed,
                "used bounded loaded-descendant priority pages for subagent metadata"
            );
        }
        if cursor.is_some() {
            tracing::debug!(
                pages = AGENT_PICKER_LOADED_PRIORITY_MAX_PAGES,
                page_size = AGENT_PICKER_LOADED_PRIORITY_PAGE_SIZE,
                "left additional loaded descendants for a later bounded picker refresh"
            );
        }

        loaded_metadata_completed
    }

    /// Handles pre-index legacy sessions that may only be visible through rollout repair.
    ///
    /// The normal path is the state-db descendant query above, complemented once per picker
    /// session by one bounded subagent-only scan-and-repair page. The separate loaded-descendant
    /// priority path runs on every first-page refresh, so current open/idle descendants are not
    /// obscured by finished history while legacy repair remains bounded and retryable.
    async fn backfill_loaded_legacy_subagent_threads(
        &mut self,
        app_server: &mut AppServerSession,
        primary_thread_id: ThreadId,
        refreshed_thread_ids: &mut HashSet<ThreadId>,
    ) -> bool {
        // A saved rollout predating spawn-edge persistence can be absent from the loaded-thread
        // priority page. Run this one bounded, subagent-only scan-and-repair page so
        // mixed-generation history gets a chance to repair every recent edge, then apply just
        // the descendants of this primary thread.
        let scanned_threads = match app_server
            .thread_list(ThreadListParams {
                cursor: None,
                limit: Some(AGENT_PICKER_PAGE_SIZE),
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
            })
            .await
        {
            Ok(response) => response.data,
            Err(err) => {
                tracing::debug!(
                    %err,
                    "legacy rollout scan-and-repair compatibility lookup was unavailable"
                );
                return false;
            }
        };
        let descendant_thread_ids =
            find_loaded_subagent_threads_for_primary(scanned_threads.clone(), primary_thread_id)
                .into_iter()
                .map(|thread| thread.thread_id)
                .collect::<HashSet<_>>();
        if descendant_thread_ids.is_empty() {
            return true;
        }

        tracing::debug!(
            descendants = descendant_thread_ids.len(),
            "used bounded rollout scan-and-repair fallback for legacy subagent metadata"
        );
        for thread in scanned_threads {
            let is_descendant = ThreadId::from_string(&thread.id)
                .is_ok_and(|thread_id| descendant_thread_ids.contains(&thread_id));
            if is_descendant {
                self.register_agent_picker_thread_from_backend(
                    primary_thread_id,
                    thread,
                    refreshed_thread_ids,
                );
            }
        }
        true
    }

    /// Returns the adjacent thread id for keyboard navigation, backfilling from the server if the
    /// local cache has no neighbor.
    ///
    /// Tries the fast path first: ask `AgentNavigationState` directly. If it returns `None` (no
    /// adjacent entry exists, typically because the cache was never populated with remote
    /// subagents), fetches the first bounded descendant page and retries. This ensures the first
    /// next/previous keypress in a resumed remote session discovers recent subagents on demand
    /// without requiring an unbounded preload.
    pub(super) async fn adjacent_thread_id_with_backfill(
        &mut self,
        app_server: &mut AppServerSession,
        direction: AgentNavigationDirection,
    ) -> Option<ThreadId> {
        let current_thread = self.current_displayed_thread_id();
        if let Some(thread_id) = self
            .agent_navigation
            .adjacent_thread_id(current_thread, direction)
        {
            return Some(thread_id);
        }

        let primary_thread_id = self.primary_thread_id?;
        if self.last_subagent_backfill_attempt == Some(primary_thread_id) {
            return None;
        }

        if self
            .backfill_loaded_subagent_threads(app_server)
            .await
            .completed
        {
            self.last_subagent_backfill_attempt = Some(primary_thread_id);
        }
        self.agent_navigation
            .adjacent_thread_id(self.current_displayed_thread_id(), direction)
    }

    pub(super) fn fresh_session_config(&self) -> Config {
        let mut config = self.config.clone();
        config.service_tier = self.chat_widget.configured_service_tier();
        config
    }
    pub(super) async fn resume_target_session(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        target_session: SessionTarget,
    ) -> Result<AppRunControl> {
        if self.ignore_same_thread_resume(&target_session) {
            tui.frame_requester().schedule_frame();
            return Ok(AppRunControl::Continue);
        }

        self.refresh_in_memory_config_from_disk_best_effort("resuming a thread")
            .await;
        let cwd_override = self
            .harness_overrides
            .cwd
            .as_deref()
            .or_else(|| app_server.remote_cwd_override());
        let resume_cwd_mode = crate::session_resume::effective_resume_cwd_mode(
            self.config.tui_resume_cwd,
            cwd_override,
        );
        let remembered_current_cwd = cwd_override.unwrap_or(self.launch_cwd.as_path());
        let current_cwd = if matches!(resume_cwd_mode, Some(ResumeCwdMode::Current)) {
            remembered_current_cwd.to_path_buf()
        } else {
            self.config.cwd.to_path_buf()
        };
        let uses_remote_workspace_or_environment = crate::uses_remote_workspace_or_environment(
            &self.app_server_target,
            &self.environment_manager,
        );
        if uses_remote_workspace_or_environment
            && self.harness_overrides.cwd.is_none()
            && app_server.remote_cwd_override().is_none()
            && matches!(resume_cwd_mode, Some(ResumeCwdMode::Current))
        {
            self.chat_widget.add_error_message(
                "`tui.resume_cwd = \"current\"` requires `--cd` when using a remote workspace"
                    .to_string(),
            );
            return Ok(AppRunControl::Continue);
        }
        let resume_cwd = if self.app_server_target.uses_remote_workspace() {
            current_cwd.clone()
        } else {
            let outcome = crate::session_resume::resolve_cwd_for_resume_or_fork(
                tui,
                &self.config,
                self.state_db.as_deref(),
                &target_session,
                CwdPromptAction::Resume,
                crate::session_resume::ResumeCwdContext {
                    current_cwd: &current_cwd,
                    remembered_current_cwd,
                    allow_remember_current: !uses_remote_workspace_or_environment
                        || cwd_override.is_some(),
                    mode: resume_cwd_mode,
                },
            )
            .await;
            match outcome {
                Err(err) => {
                    self.chat_widget.add_error_message(format!(
                        "Failed to determine working directory for resume: {err}"
                    ));
                    return Ok(AppRunControl::Continue);
                }
                Ok(crate::session_resume::ResolveCwdOutcome::Continue(Some(cwd))) => cwd,
                Ok(crate::session_resume::ResolveCwdOutcome::Continue(None)) => current_cwd.clone(),
                Ok(crate::session_resume::ResolveCwdOutcome::Exit) => {
                    return Ok(AppRunControl::Exit(ExitReason::UserRequested));
                }
            }
        };

        let (config_current_cwd, config_resume_cwd) =
            if self.app_server_target.uses_remote_workspace() {
                let local_config_cwd = self.config.cwd.to_path_buf();
                (local_config_cwd.clone(), local_config_cwd)
            } else {
                (current_cwd, resume_cwd)
            };
        let mut resume_config = match self
            .rebuild_config_for_resume_or_fallback(&config_current_cwd, config_resume_cwd)
            .await
        {
            Ok(cfg) => cfg,
            Err(err) => {
                self.chat_widget.add_error_message(format!(
                    "Failed to rebuild configuration for resume: {err}"
                ));
                return Ok(AppRunControl::Continue);
            }
        };
        self.apply_runtime_policy_overrides(&mut resume_config);

        let summary = session_summary(
            self.chat_widget.token_usage(),
            self.chat_widget.thread_id(),
            self.chat_widget.thread_name(),
            self.chat_widget.rollout_path().as_deref(),
        );
        match app_server
            .resume_thread(
                resume_config.clone(),
                target_session.thread_id,
                self.resume_model_settings(),
            )
            .await
        {
            Ok(resumed) => {
                let resumed_thread_id = resumed.session.thread_id;
                self.shutdown_current_thread(app_server).await;
                self.config = resume_config;
                tui.set_notification_settings(
                    self.config.tui_notifications.method,
                    self.config.tui_notifications.condition,
                );
                self.file_search
                    .update_search_dir(self.config.cwd.to_path_buf());
                // `replay_thread_turns` stores rendered lines. Seed the primary entry, then
                // hydrate descendant metadata before it renders historical Spawn/Wait cells so
                // their friendly paths and effective identities are retained in the new widget.
                self.prepare_chat_widget_for_app_server_thread(
                    tui, app_server, /*initial_user_message*/ None,
                )
                .await;
                self.primary_thread_id = Some(resumed_thread_id);
                self.upsert_agent_picker_thread(
                    resumed_thread_id,
                    /*agent_nickname*/ None,
                    /*agent_role*/ None,
                    /*is_closed*/ false,
                );
                if resumed.blocks_direct_input {
                    self.mark_primary_thread_parent_owned(resumed_thread_id);
                }
                self.backfill_loaded_subagent_threads(app_server).await;
                match self
                    .enqueue_primary_thread_session_with_presentation_and_server(
                        Some(app_server),
                        resumed.thread_subscription_id,
                        resumed.session,
                        resumed.turns,
                        ThreadAttachPresentation::SessionLineage,
                    )
                    .await
                {
                    Ok(()) => {
                        if let Some(summary) = summary {
                            let mut lines: Vec<Line<'static>> = Vec::new();
                            if let Some(usage_line) = summary.usage_line {
                                lines.push(usage_line.into());
                            }
                            if let Some(command) = summary.resume_hint {
                                let spans =
                                    vec!["To continue this session, run ".into(), command.cyan()];
                                lines.push(spans.into());
                            }
                            self.chat_widget.add_plain_history_lines(lines);
                        }
                        self.maybe_prompt_resume_paused_goal_after_resume(
                            app_server,
                            resumed_thread_id,
                        )
                        .await;
                    }
                    Err(err) => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to attach to resumed app-server thread: {err}"
                        ));
                    }
                }
            }
            Err(err) => {
                let path_display = target_session.display_label();
                self.chat_widget.add_error_message(format!(
                    "Failed to resume session from {path_display}: {err}"
                ));
            }
        }

        Ok(AppRunControl::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_thread_read_error_detection_matches_not_loaded_errors() {
        let err = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read failed: thread not loaded: thr_123"
        );

        assert!(App::is_terminal_thread_read_error(&err));
    }

    #[test]
    fn terminal_thread_read_error_detection_ignores_transient_failures() {
        let err = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read transport error: broken pipe"
        );

        assert!(!App::is_terminal_thread_read_error(&err));
    }

    #[test]
    fn closed_state_for_thread_read_error_preserves_live_state_without_cache_on_transient_error() {
        let err = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read transport error: broken pipe"
        );

        assert!(!App::closed_state_for_thread_read_error(
            &err, /*existing_is_closed*/ None
        ));
    }

    #[test]
    fn closed_state_for_thread_read_error_marks_terminal_uncached_threads_closed() {
        let err = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read failed: thread not loaded: thr_123"
        );

        assert!(App::closed_state_for_thread_read_error(
            &err, /*existing_is_closed*/ None
        ));
    }

    #[test]
    fn include_turns_fallback_detection_handles_unmaterialized_and_ephemeral_threads() {
        let unmaterialized = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read failed: thread thr_123 is not materialized yet; includeTurns is unavailable before first user message"
        );
        let ephemeral = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read failed: ephemeral threads do not support includeTurns"
        );

        assert!(App::can_fallback_from_include_turns_error(&unmaterialized));
        assert!(App::can_fallback_from_include_turns_error(&ephemeral));
    }
}
