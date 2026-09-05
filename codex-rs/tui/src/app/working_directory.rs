//! Local directory transitions and managed worktrees with fresh or preserved conversation history.

use super::session_lifecycle::ThreadAttachPresentation;
use super::*;
use crate::app_server_session::ForkGoalContinuation::DeferUntilNextTurn;
use crate::history_cell::McpInventoryLoadingCell as LoadingCell;
use crate::terminal_visualization_instructions::with_terminal_visualization_instructions;
use codex_app_server_protocol::ThreadBackgroundTerminalsListParams;
use codex_app_server_protocol::ThreadBackgroundTerminalsListResponse as ListResponse;

impl App {
    pub(super) async fn start_managed_worktree(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        mode: crate::app_event::ManagedWorktreeMode,
        name: Option<String>,
    ) {
        if !self.config.features.enabled(Feature::Worktrees) {
            self.chat_widget.add_error_message(
                "Enable worktrees in /experimental to create a worktree.".to_string(),
            );
        } else if self.config.active_project.is_untrusted() {
            self.chat_widget.add_error_message(
                "Cannot create a worktree from an explicitly untrusted source.".to_string(),
            );
        } else if crate::uses_remote_workspace_or_environment(
            &self.app_server_target,
            self.environment_manager.as_ref(),
        ) {
            self.chat_widget.add_error_message(
                "Managed worktrees are only supported for local sessions.".to_string(),
            );
        } else if self
            .primary_thread_id
            .is_none_or(|thread_id| !self.chat_widget.can_change_working_directory(thread_id))
        {
            self.chat_widget.add_error_message(
                "Creating a worktree requires an idle primary session without queued input."
                    .to_string(),
            );
        } else {
            let setup = async {
                let source = self
                    .rebuild_config_for_cwd(self.config.cwd.to_path_buf())
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                anyhow::ensure!(
                    !source.active_project.is_untrusted(),
                    "Cannot create a worktree from an explicitly untrusted source."
                );
                let host = crate::legacy_core::config::load_config_toml_with_layer_stack(
                    &self.config.codex_home,
                    /*cwd*/ None,
                    Vec::new(),
                    codex_config::ConfigLoadOptions::default(),
                )
                .await?;
                let settings = codex_worktree::WorktreeSettings::for_cli(
                    &self.config.codex_home,
                    host.config_toml.desktop.as_ref(),
                )?;
                let manager = codex_worktree::WorktreeManager::new(settings);
                let checkout = manager.create(&codex_worktree::CreateWorktree {
                    source_cwd: self.config.cwd.to_path_buf(),
                    base: None,
                })?;
                anyhow::Ok((manager, checkout))
            }
            .await;
            match setup {
                Ok((manager, checkout)) => {
                    if let Err(error) = self
                        .switch_to_managed_worktree(tui, app_server, manager, checkout, mode, name)
                        .await
                    {
                        self.chat_widget.add_error_message(error.to_string());
                    }
                }
                Err(error) => self.chat_widget.add_error_message(error.to_string()),
            }
        }
    }

    fn working_directory_error(&mut self, message: impl Into<String>) {
        self.chat_widget.add_error_message(message.into());
    }

    pub(super) async fn switch_to_managed_worktree(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        manager: codex_worktree::WorktreeManager,
        checkout: codex_worktree::ManagedWorktree,
        mode: crate::app_event::ManagedWorktreeMode,
        name: Option<String>,
    ) -> Result<()> {
        let previous_thread_id = self.chat_widget.thread_id();
        let cwd = AbsolutePathBuf::from_absolute_path(checkout.cwd.clone())
            .map_err(|error| color_eyre::eyre::eyre!(error.to_string()))?;
        self.change_working_directory_with_managed(
            tui,
            app_server,
            cwd,
            Some((manager, checkout, mode, name)),
        )
        .await;
        if self.chat_widget.thread_id() == previous_thread_id {
            return Err(color_eyre::eyre::eyre!(
                "Could not start a session in the managed worktree"
            ));
        }
        Ok(())
    }

    pub(super) async fn change_working_directory(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        cwd: AbsolutePathBuf,
    ) {
        self.change_working_directory_with_managed(
            tui, app_server, cwd, /*managed_worktree*/ None,
        )
        .await;
    }

    async fn change_working_directory_with_managed(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        cwd: AbsolutePathBuf,
        managed_worktree: Option<(
            codex_worktree::WorktreeManager,
            codex_worktree::ManagedWorktree,
            crate::app_event::ManagedWorktreeMode,
            Option<String>,
        )>,
    ) {
        if self.config.ephemeral || !cwd.as_path().is_dir() {
            return self.working_directory_error("This task cannot be safely replaced.");
        }
        let Some(thread_id) = self.chat_widget.thread_id() else {
            return;
        };
        let cells = &self.transcript_cells;
        if cells.iter().any(|cell| cell.as_any().is::<LoadingCell>()) {
            return self.working_directory_error("MCP inventory is still loading.");
        }
        let agents = self.agent_navigation.ordered_threads();
        let closed_agents: HashSet<_> = agents
            .iter()
            .filter_map(|(id, agent)| agent.is_closed.then_some(*id))
            .collect();
        let active = self.thread_event_channels.iter().any(|(id, channel)| {
            *id != thread_id
                && !closed_agents.contains(id)
                && !channel
                    .store
                    .try_lock()
                    .is_ok_and(|store| store.active_turn_id().is_none())
        });
        if active || agents.iter().any(|(t, a)| *t != thread_id && a.is_running) {
            return self.working_directory_error("Cannot change: another agent is running.");
        }
        let open_agents: Vec<_> = agents
            .iter()
            .filter_map(|(id, agent)| (!agent.is_closed).then_some(*id))
            .collect();
        let mut config = match self.rebuild_config_for_cwd(cwd.to_path_buf()).await {
            Ok(config) => config,
            Err(err) => return self.working_directory_error(format!("Cannot load {cwd:?}: {err}")),
        };
        if config.active_project.trust_level.is_none() {
            return self.working_directory_error("This directory is not trusted; run Codex there.");
        }
        if let Some((_, checkout, crate::app_event::ManagedWorktreeMode::Fork, _)) =
            managed_worktree.as_ref()
            && with_terminal_visualization_instructions(
                &self.config,
                self.config.developer_instructions.clone(),
            ) != with_terminal_visualization_instructions(
                &config,
                config.developer_instructions.clone(),
            )
        {
            return self.working_directory_error(format!(
                "Cannot fork into this worktree because developer instructions differ. Start a new conversation instead. An unused checkout was created at {}; remove it with `git worktree remove <checkout-path>` from the source repository.",
                checkout.root.display()
            ));
        }
        if let Some(profile) = self.runtime_permission_profile_override.as_ref()
            && profile.active_permission_profile.is_some()
            && (!profile.matches_config(&config)
                || config.permissions.profile_workspace_roots()
                    != self.config.permissions.profile_workspace_roots())
        {
            return self.working_directory_error("Permission profile has different settings.");
        }
        if let Some(profile) = self.runtime_permission_profile_override.as_ref()
            && profile.turn_override == RuntimePermissionProfileTurnOverride::Preserve
            && profile.active_permission_profile.is_none()
            && !crate::app_server_session::permission_profile_is_safely_represented_by_sandbox_mode(
                &profile.permission_profile,
                cwd.as_path(),
            )
        {
            return self.working_directory_error("Permission profile cannot be preserved by /cd.");
        }
        self.apply_runtime_policy_overrides(&mut config, RuntimePolicyOverrideScope::All);
        if self.runtime_permission_profile_override.is_some() {
            let reviewer = self.config.approvals_reviewer;
            let reviewers = &config.config_layer_stack.requirements().approvals_reviewer;
            if let Err(error) = reviewers.can_set(&reviewer) {
                return self.working_directory_error(format!("Approvals reviewer: {error}"));
            }
            config.approvals_reviewer = reviewer;
        }
        let actual = config.permissions.approval_policy.value();
        let approval = self
            .runtime_approval_policy_override
            .map(RuntimeApprovalPolicyOverride::policy);
        let profile = self.runtime_permission_profile_override.as_ref();
        if approval.is_some_and(|p| actual != p.to_core())
            || profile.is_some_and(|profile| !profile.matches_config(&config))
        {
            return;
        }
        let local_settings = crate::local_settings::LocalSettings::from(&config);
        let keymap = match RuntimeKeymap::from_config(&local_settings.tui.keymap) {
            Ok(keymap) => keymap,
            Err(error) => return self.chat_widget.add_error_message(error),
        };
        config.service_tier = self.chat_widget.configured_service_tier();
        let is_new_worktree = matches!(
            managed_worktree.as_ref().map(|(_, _, mode, _)| mode),
            Some(crate::app_event::ManagedWorktreeMode::New)
        );
        let rollout = self.chat_widget.rollout_path();
        let has_rollout = rollout.as_deref().is_some_and(rollout_path_is_resumable);
        let channels = &self.thread_event_channels;
        if !has_rollout
            && !is_new_worktree
            && (!self.chat_widget.token_usage().is_zero()
                || channels.get(&thread_id).is_some_and(|channel| {
                    channel.store.try_lock().map_or(/*default*/ true, |store| {
                        !store.turns.is_empty()
                            || store.buffer.iter().any(|event| {
                                matches!(
                                    event,
                                    ThreadBufferedEvent::Notification(notification)
                                        if matches!(notification.as_ref(),
                                            ServerNotification::TurnStarted(_)
                                            | ServerNotification::TurnCompleted(_))
                                )
                            })
                    })
                }))
        {
            return self.working_directory_error("Conversation history is not saved.");
        }
        let mut ids: HashSet<_> = channels
            .keys()
            .copied()
            .filter(|id| !closed_agents.contains(id))
            .collect();
        ids.extend(open_agents);
        let descendants = ids.iter().copied().filter(|id| *id != thread_id);
        for tracked_id in std::iter::once(thread_id).chain(descendants) {
            let request = ClientRequest::ThreadBackgroundTerminalsList {
                request_id: app_server.next_request_id(),
                params: ThreadBackgroundTerminalsListParams {
                    thread_id: tracked_id.to_string(),
                    cursor: None,
                    limit: Some(1),
                },
            };
            let handle = app_server.request_handle();
            let result = handle.request_typed::<ListResponse>(request).await;
            if !matches!(result, Ok(response) if response.data.is_empty()) {
                return self.working_directory_error("Active background terminals block /cd.");
            }
        }
        if is_new_worktree {
            apply_managed_new_thread_defaults(
                &mut config,
                app_server.managed_new_thread_defaults(),
                &self.cli_kv_overrides,
                &self.harness_overrides,
            );
        }
        let preserve_history = has_rollout && !is_new_worktree;
        let transitioned = if preserve_history {
            app_server
                .fork_thread_at(
                    &local_settings,
                    config.clone(),
                    thread_id,
                    /*last_turn_id*/ None,
                    /*before_turn_id*/ None,
                    DeferUntilNextTurn,
                )
                .await
        } else {
            app_server
                .start_thread_with_session_start_source(
                    &local_settings,
                    &config,
                    /*session_start_source*/ None,
                    /*remote_cwd_override*/ None,
                )
                .await
        };
        let mut transitioned = match transitioned {
            Ok(value) => value,
            Err(e) => return self.working_directory_error(format!("Failed to change: {e}")),
        };
        let session = &transitioned.session;
        if session.thread_id == thread_id
            || crate::session_resume::cwds_differ(session.cwd.as_path(), cwd.as_path())
            || session.runtime_workspace_roots != config.workspace_roots
            || session.approval_policy.to_core() != config.permissions.approval_policy.value()
            || session.approvals_reviewer != config.approvals_reviewer
            || session.active_permission_profile != config.permissions.active_permission_profile()
        {
            if session.thread_id != thread_id {
                let _ = app_server.thread_unsubscribe(session.thread_id).await;
                if preserve_history {
                    let _ = app_server.thread_archive(session.thread_id).await;
                }
            }
            return self.working_directory_error("Requested directory or permissions not applied.");
        }
        if let Some((manager, checkout, _, _)) = managed_worktree.as_ref()
            && let Err(error) =
                manager.bind_thread(&checkout.root, &transitioned.session.thread_id.to_string())
        {
            let replacement_id = transitioned.session.thread_id;
            let _ = app_server.thread_unsubscribe(replacement_id).await;
            if preserve_history {
                let _ = app_server.thread_archive(replacement_id).await;
            }
            return self.working_directory_error(format!(
                "Cannot register managed worktree ownership: {error}"
            ));
        }
        let name_error = if let Some(name) = managed_worktree
            .as_ref()
            .and_then(|(_, _, _, name)| name.as_ref())
        {
            match app_server
                .thread_set_name(transitioned.session.thread_id, name.clone())
                .await
            {
                Ok(()) => {
                    transitioned.session.thread_name = Some(name.clone());
                    None
                }
                Err(error) => Some(format!("Failed to name the worktree session: {error}")),
            }
        } else {
            None
        };
        if let Err(error) = app_server.thread_unsubscribe(thread_id).await {
            let replacement_id = transitioned.session.thread_id;
            let _ = app_server.thread_unsubscribe(replacement_id).await;
            if preserve_history {
                let _ = app_server.thread_archive(replacement_id).await;
            }
            return self.working_directory_error(format!("Cannot change directories: {error}"));
        }
        for tracked_id in ids.into_iter().filter(|id| *id != thread_id) {
            if let Err(error) = app_server.thread_unsubscribe(tracked_id).await {
                tracing::warn!("failed to unsubscribe tracked thread {tracked_id}: {error}");
            }
        }
        self.local_settings = local_settings;
        self.config = config;
        self.file_search
            .update_search_dir(self.config.cwd.to_path_buf());
        let notify = &self.local_settings.tui.notification_settings;
        tui.set_notification_settings(notify.method, notify.condition);
        if let Err(error) = tui.clear_ambient_pet_image() {
            tracing::warn!(%error, "failed to clear ambient pet image");
        }
        let attach = App::replace_chat_widget_with_app_server_thread;
        let (lineage, message) = (ThreadAttachPresentation::SessionLineage, None);
        if let Err(error) = attach(self, tui, transitioned, lineage, message).await {
            return self.working_directory_error(format!("Could not restore session: {error}"));
        }
        if let Some(error) = name_error {
            self.chat_widget.add_error_message(error);
        }
        self.cancel_pending_key_chord();
        self.keymap = keymap;
        self.merge_startup_warnings(tui, &history_cell::StartupWarningsCell::default());
        self.restore_runtime_theme_from_config();
        self.runtime_working_directory_override = Some(cwd.to_path_buf());
        if let Some(message) = project_config_warning(&self.config) {
            self.chat_widget.add_warning_message(message);
        }
        let message = format!("Working directory changed to: {}", cwd.display());
        self.chat_widget.add_info_message(message, /*hint*/ None);
        if !self.config.bypass_hook_trust {
            let load_review = crate::startup_hooks_review::load_startup_hooks_review_entry;
            let hooks = load_review(app_server.request_handle(), cwd.to_path_buf()).await;
            if hooks.hooks.iter().any(crate::hooks_rpc::hook_needs_review) {
                self.chat_widget.open_hooks_browser(hooks);
            }
        }
        tui.frame_requester().schedule_frame();
    }
}
