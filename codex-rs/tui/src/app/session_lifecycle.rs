//! Session, resume, fork, and subagent selection lifecycle for the TUI app.
//!
//! This module owns the high-level transitions between app-server threads: starting fresh sessions,
//! resuming/forking saved sessions, replacing ChatWidget instances, and maintaining the agent picker
//! cache used for multi-agent navigation.

use super::*;
use crate::app_server_session::source_agent_path;
use crate::app_server_session::thread_blocks_direct_input;
use crate::multi_agents::AgentPickerThreadUsage;
use crate::multi_agents::format_agent_picker_item_description;
use crate::multi_agents::format_agent_picker_item_selected_description;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadSortKey;
use codex_app_server_protocol::ThreadSourceKind;
use codex_config::types::ResumeCwdMode;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsage as ProtocolTokenUsage;
use std::collections::HashMap;
use std::collections::HashSet;

pub(super) const AGENT_PICKER_PAGE_SIZE: u32 = 50;
const AGENT_PICKER_VIEW_ID: &str = "agent-picker";

#[derive(Clone, Copy)]
pub(super) enum ThreadAttachPresentation {
    SessionLineage,
    PromptEdit,
}

/// Reports whether a loaded-thread backfill completed and which descendants already had their
/// liveness metadata refreshed, allowing the picker to skip duplicate `thread/read` requests.
#[derive(Default)]
pub(super) struct LoadedSubagentBackfill {
    pub(super) completed: bool,
    pub(super) refreshed_thread_ids: HashSet<ThreadId>,
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
                && channel.attachment() == ThreadEventAttachment::Live
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

    /// Loads one continuation page without dropping the active `closed` filter.
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

    async fn render_agent_picker(&mut self) {
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
            let name = entry
                .agent_path
                .as_deref()
                .map(str::trim)
                .filter(|agent_path| !is_primary && !agent_path.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| {
                    format_agent_picker_item_name(
                        entry.agent_nickname.as_deref(),
                        entry.agent_role.as_deref(),
                        is_primary,
                    )
                });
            let usage = self.agent_picker_thread_usage(thread_id, entry).await;
            let description = format_agent_picker_item_description(thread_id, entry, &usage);
            let selected_description =
                format_agent_picker_item_selected_description(thread_id, entry, &usage);
            let status_terms = if entry.is_running {
                "live active open"
            } else {
                if entry.is_closed && !is_primary {
                    closed_agent_count += 1;
                }
                "closed stale inactive finished"
            };
            let search_value =
                format!("{name} {description} {selected_description} {status_terms}");
            items.push(SelectionItem {
                name,
                name_prefix_spans: agent_picker_status_dot_spans(entry.is_closed),
                description: Some(description),
                selected_description: Some(selected_description),
                is_current: self.active_thread_id == Some(thread_id),
                hidden_when_unfiltered: !is_primary
                    && self.active_thread_id != Some(thread_id)
                    && !entry.is_running,
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
        let has_replay_channel = self.thread_event_channels.contains_key(&thread_id);
        match app_server
            .thread_read(thread_id, /*include_turns*/ false)
            .await
        {
            Ok(thread) => {
                let is_parent_owned = thread_blocks_direct_input(&thread);
                let agent_path = source_agent_path(&thread.source);
                let is_running = matches!(
                    thread.status,
                    codex_app_server_protocol::ThreadStatus::Active { .. }
                );
                let is_closed = matches!(
                    thread.status,
                    codex_app_server_protocol::ThreadStatus::NotLoaded
                );
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
                    is_closed,
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
                self.sync_agent_picker_identity(thread_id);
                if is_running {
                    self.agent_navigation.mark_running(thread_id);
                } else {
                    self.agent_navigation
                        .set_running(thread_id, /*is_running*/ false);
                }
                true
            }
            Err(err) => {
                if Self::is_terminal_thread_read_error(&err) && !has_replay_channel {
                    self.agent_navigation.remove(thread_id);
                    return false;
                }
                let is_closed = Self::closed_state_for_thread_read_error(
                    &err,
                    existing_entry.as_ref().map(|entry| entry.is_closed),
                );
                if let Some(entry) = existing_entry {
                    self.upsert_agent_picker_thread(
                        thread_id,
                        entry.agent_nickname,
                        entry.agent_role,
                        is_closed,
                    );
                } else {
                    self.upsert_agent_picker_thread(
                        thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                        is_closed,
                    );
                }
                self.agent_navigation
                    .set_running(thread_id, /*is_running*/ false);
                true
            }
        }
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
        if self.thread_event_channels.contains_key(&thread_id) {
            return Ok(true);
        }

        let (session, turns, live_attached) = match app_server
            .resume_thread(self.config.clone(), thread_id, self.resume_model_settings())
            .await
        {
            Ok(started) => {
                if started.blocks_direct_input {
                    self.agent_navigation.mark_parent_owned(thread_id);
                }
                (started.session, started.turns, true)
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
                (session, turns, false)
            }
        };
        let channel = self.ensure_thread_channel(thread_id);
        if !live_attached {
            channel.mark_replay_only();
        }
        let mut store = channel.store.lock().await;
        store.set_session(session, turns);
        Ok(live_attached)
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
        if self.should_attach_live_thread_for_selection(thread_id) {
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
        } else if !self.thread_event_channels.contains_key(&thread_id) && is_replay_only {
            self.chat_widget
                .add_error_message(format!("Agent thread {thread_id} is no longer available."));
            return Ok(());
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
        if blocks_direct_input {
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

    pub(super) fn should_attach_live_thread_for_selection(&self, thread_id: ThreadId) -> bool {
        !self.thread_event_channels.contains_key(&thread_id)
            && self
                .agent_navigation
                .get(&thread_id)
                .is_none_or(|entry| !entry.is_closed)
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

    pub(super) fn reset_thread_event_state(&mut self) {
        self.abort_all_thread_event_listeners();
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
                if let Err(err) = app_server.thread_unsubscribe(thread_id).await {
                    tracing::warn!(
                        thread_id = %thread_id,
                        "failed to unsubscribe stale startup thread: {err}"
                    );
                }
                self.discard_thread_local_state(thread_id).await;
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
                self.enqueue_primary_thread_session(started.session, started.turns)
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
        started: AppServerStartedThread,
        presentation: ThreadAttachPresentation,
        initial_user_message: Option<crate::chatwidget::UserMessage>,
    ) -> Result<()> {
        // Initial messages are for freshly attached primary threads only. Thread switches and
        // resume/fork flows pass `None` so they cannot replay old history and then auto-submit a new
        // user turn by accident.
        self.reset_thread_event_state();
        let init = self.chatwidget_init_for_forked_or_resumed_thread(
            tui,
            self.config.clone(),
            initial_user_message,
        );
        self.replace_chat_widget(ChatWidget::new_with_app_event(init));
        if started.blocks_direct_input {
            self.mark_primary_thread_parent_owned(started.session.thread_id);
        }
        self.enqueue_primary_thread_session_with_presentation(
            started.session,
            started.turns,
            presentation,
        )
        .await?;
        Ok(())
    }

    /// Fetches one bounded page of persisted descendants while independently retaining every
    /// currently loaded descendant. Loaded sessions are registered first so a transient persisted
    /// list failure cannot hide a healthy sidecar.
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
        let loaded_priority_completed = if is_continuation {
            true
        } else {
            self.backfill_loaded_priority_subagent_threads(
                app_server,
                primary_thread_id,
                &mut refreshed_thread_ids,
            )
            .await
        };

        let response = match app_server
            .thread_list(ThreadListParams {
                cursor,
                limit: Some(AGENT_PICKER_PAGE_SIZE),
                sort_key: Some(ThreadSortKey::UpdatedAt),
                sort_direction: Some(SortDirection::Desc),
                model_providers: None,
                source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
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
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(%err, "failed to list persisted descendants for subagent backfill");
                if is_continuation && Self::is_invalid_thread_list_cursor_error(&err) {
                    self.agent_navigation.set_next_picker_page_cursor(None);
                }
                self.sync_active_agent_label();
                return LoadedSubagentBackfill {
                    completed: false,
                    refreshed_thread_ids,
                };
            }
        };

        let response_next_cursor = response.next_cursor;
        let (scoped_threads, page_scope_proven) =
            Self::locally_verify_descendant_page(app_server, primary_thread_id, response.data)
                .await;
        let cursor_scope_proven = if page_scope_proven && response_next_cursor.is_some() {
            Self::server_honors_ancestor_filter(app_server).await
        } else {
            page_scope_proven
        };
        let next_cursor = cursor_scope_proven
            .then_some(response_next_cursor)
            .flatten();
        if is_continuation
            || next_cursor.is_none()
            || self.agent_navigation.next_picker_page_cursor().is_none()
        {
            self.agent_navigation
                .set_next_picker_page_cursor(next_cursor);
        }
        for thread in scoped_threads {
            self.register_agent_picker_thread_from_backend(
                primary_thread_id,
                thread,
                &mut refreshed_thread_ids,
            );
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

    /// Older app servers may ignore the experimental ancestor filter. Prove every returned row's
    /// parent chain locally before exposing it, and report whether the whole page was scoped so an
    /// opaque continuation cursor from an unscoped response is never retained.
    async fn locally_verify_descendant_page(
        app_server: &mut AppServerSession,
        primary_thread_id: ThreadId,
        threads: Vec<Thread>,
    ) -> (Vec<Thread>, bool) {
        let mut parent_by_thread_id = HashMap::new();
        let mut malformed_thread_ids = HashSet::new();
        for thread in &threads {
            let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
                continue;
            };
            match Self::thread_parent_id(thread) {
                Ok(parent_thread_id) => {
                    parent_by_thread_id.insert(thread_id, parent_thread_id);
                }
                Err(()) => {
                    malformed_thread_ids.insert(thread_id);
                }
            }
        }

        let mut scoped_thread_ids = HashSet::new();
        let mut page_scope_proven = true;
        for thread in &threads {
            if !Self::is_thread_spawn_source(thread) {
                page_scope_proven = false;
                tracing::warn!(
                    thread_id = %thread.id,
                    "discarding persisted descendant with a non-spawn source"
                );
                continue;
            }
            let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
                page_scope_proven = false;
                continue;
            };
            let mut current_thread_id = thread_id;
            let mut visited = HashSet::new();
            let scoped = loop {
                if current_thread_id == primary_thread_id {
                    break thread_id != primary_thread_id;
                }
                if !visited.insert(current_thread_id)
                    || malformed_thread_ids.contains(&current_thread_id)
                {
                    break false;
                }
                if !parent_by_thread_id.contains_key(&current_thread_id) {
                    let Ok(parent_thread) = app_server
                        .thread_read(current_thread_id, /*include_turns*/ false)
                        .await
                    else {
                        break false;
                    };
                    if parent_thread.id != current_thread_id.to_string() {
                        break false;
                    }
                    match Self::thread_parent_id(&parent_thread) {
                        Ok(parent_thread_id) => {
                            parent_by_thread_id.insert(current_thread_id, parent_thread_id);
                        }
                        Err(()) => {
                            malformed_thread_ids.insert(current_thread_id);
                            break false;
                        }
                    }
                }
                let Some(parent_thread_id) = parent_by_thread_id
                    .get(&current_thread_id)
                    .copied()
                    .flatten()
                else {
                    break false;
                };
                current_thread_id = parent_thread_id;
            };
            if scoped {
                scoped_thread_ids.insert(thread_id);
            } else {
                page_scope_proven = false;
                tracing::warn!(
                    thread_id = %thread_id,
                    primary_thread_id = %primary_thread_id,
                    "discarding persisted subagent outside the active thread tree"
                );
            }
        }

        let scoped_threads = threads
            .into_iter()
            .filter(|thread| {
                ThreadId::from_string(&thread.id)
                    .is_ok_and(|thread_id| scoped_thread_ids.contains(&thread_id))
            })
            .collect();
        (scoped_threads, page_scope_proven)
    }

    /// `ThreadListResponse` does not acknowledge experimental filters. Before retaining an opaque
    /// continuation cursor, send a state-only canary for an unknown ancestor: a compatible server
    /// returns an exhausted empty page, while an older server that ignores the field repeats its
    /// broad subagent result. Rows remain locally verified independently of this capability probe.
    async fn server_honors_ancestor_filter(app_server: &mut AppServerSession) -> bool {
        let probe_ancestor_thread_id = ThreadId::new();
        match app_server
            .thread_list(ThreadListParams {
                cursor: None,
                limit: Some(1),
                sort_key: Some(ThreadSortKey::UpdatedAt),
                sort_direction: Some(SortDirection::Desc),
                model_providers: None,
                source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
                thread_sources: None,
                archived: Some(false),
                is_pinned: None,
                cwd: None,
                use_state_db_only: true,
                search_term: None,
                parent_thread_id: None,
                ancestor_thread_id: Some(probe_ancestor_thread_id.to_string()),
            })
            .await
        {
            Ok(response) if response.data.is_empty() && response.next_cursor.is_none() => true,
            Ok(_) => {
                tracing::warn!(
                    "discarding agent picker continuation because ancestor filtering was not enforced"
                );
                false
            }
            Err(err) => {
                tracing::warn!(
                    %err,
                    "discarding agent picker continuation because ancestor filtering was unavailable"
                );
                false
            }
        }
    }

    fn thread_parent_id(thread: &Thread) -> Result<Option<ThreadId>, ()> {
        thread
            .parent_thread_id
            .as_deref()
            .map(ThreadId::from_string)
            .transpose()
            .map_err(|_| ())
    }

    fn is_thread_spawn_source(thread: &Thread) -> bool {
        matches!(
            &thread.source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        )
    }

    fn register_agent_picker_thread_from_backend(
        &mut self,
        primary_thread_id: ThreadId,
        thread: Thread,
        refreshed_thread_ids: &mut HashSet<ThreadId>,
    ) {
        if !Self::is_thread_spawn_source(&thread) {
            tracing::warn!(
                thread_id = %thread.id,
                "ignoring persisted descendant with a non-spawn source during subagent backfill"
            );
            return;
        }
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
            .is_some_and(|channel| channel.attachment() == ThreadEventAttachment::Live);
        let is_running = matches!(
            &thread.status,
            codex_app_server_protocol::ThreadStatus::Active { .. }
        );
        let is_closed = !has_live_channel
            && matches!(
                &thread.status,
                codex_app_server_protocol::ThreadStatus::NotLoaded
            );
        if thread_blocks_direct_input(&thread) {
            self.agent_navigation.mark_parent_owned(thread_id);
        }
        self.upsert_agent_picker_thread(
            thread_id,
            thread.agent_nickname,
            thread.agent_role,
            is_closed,
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
        self.sync_agent_picker_identity(thread_id);
        if !has_live_channel {
            if is_running {
                self.agent_navigation.mark_running(thread_id);
            } else {
                self.agent_navigation
                    .set_running(thread_id, /*is_running*/ false);
            }
            refreshed_thread_ids.insert(thread_id);
        }
    }

    async fn backfill_loaded_priority_subagent_threads(
        &mut self,
        app_server: &mut AppServerSession,
        primary_thread_id: ThreadId,
        refreshed_thread_ids: &mut HashSet<ThreadId>,
    ) -> bool {
        let loaded_thread_ids = match app_server
            .thread_loaded_list(ThreadLoadedListParams {
                cursor: None,
                limit: None,
            })
            .await
        {
            Ok(response) => response.data,
            Err(err) => {
                tracing::debug!(%err, "loaded descendant priority lookup was unavailable");
                return false;
            }
        };

        let mut loaded_metadata_completed = true;
        let mut threads = Vec::with_capacity(loaded_thread_ids.len());
        for thread_id in loaded_thread_ids {
            let Ok(thread_id) = ThreadId::from_string(&thread_id) else {
                loaded_metadata_completed = false;
                continue;
            };
            if thread_id == primary_thread_id {
                continue;
            }
            match app_server
                .thread_read(thread_id, /*include_turns*/ false)
                .await
            {
                Ok(thread) => threads.push(thread),
                Err(err) => {
                    loaded_metadata_completed = false;
                    tracing::debug!(thread_id = %thread_id, %err, "loaded descendant metadata read failed");
                }
            }
        }

        let descendants =
            find_loaded_subagent_threads_for_primary(threads.clone(), primary_thread_id)
                .into_iter()
                .map(|thread| thread.thread_id)
                .collect::<HashSet<_>>();
        for thread in threads {
            if ThreadId::from_string(&thread.id)
                .is_ok_and(|thread_id| descendants.contains(&thread_id))
            {
                self.register_agent_picker_thread_from_backend(
                    primary_thread_id,
                    thread,
                    refreshed_thread_ids,
                );
            }
        }
        loaded_metadata_completed
    }

    async fn backfill_loaded_legacy_subagent_threads(
        &mut self,
        app_server: &mut AppServerSession,
        primary_thread_id: ThreadId,
        refreshed_thread_ids: &mut HashSet<ThreadId>,
    ) -> bool {
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut scanned_threads = Vec::new();
        loop {
            let response = match app_server
                .thread_list(ThreadListParams {
                    cursor,
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
                Ok(response) => response,
                Err(err) => {
                    tracing::debug!(%err, "legacy subagent relation repair was unavailable");
                    return false;
                }
            };
            scanned_threads.extend(response.data);
            let Some(next_cursor) = response.next_cursor else {
                break;
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                tracing::debug!(
                    cursor = %next_cursor,
                    "legacy subagent relation repair returned a repeated cursor"
                );
                return false;
            }
            cursor = Some(next_cursor);
        }
        let descendants =
            find_loaded_subagent_threads_for_primary(scanned_threads.clone(), primary_thread_id)
                .into_iter()
                .map(|thread| thread.thread_id)
                .collect::<HashSet<_>>();
        for thread in scanned_threads {
            if ThreadId::from_string(&thread.id)
                .is_ok_and(|thread_id| descendants.contains(&thread_id))
            {
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
    /// subagents), performs a full `backfill_loaded_subagent_threads` and retries. This ensures the
    /// first next/previous keypress in a resumed remote session discovers subagents on demand
    /// without requiring the user to wait for a proactive fetch.
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
                match self
                    .replace_chat_widget_with_app_server_thread(
                        tui,
                        resumed,
                        ThreadAttachPresentation::SessionLineage,
                        /*initial_user_message*/ None,
                    )
                    .await
                {
                    Ok(()) => {
                        self.backfill_loaded_subagent_threads(app_server).await;
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
