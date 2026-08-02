//! App-server event stream handling for the TUI app.

use super::App;
use super::ThreadLifecycleTarget;
use super::app_server_event_targets::ServerNotificationThreadTarget;
use super::app_server_event_targets::server_notification_thread_target;
use super::app_server_event_targets::server_request_thread_id;
use crate::app_command::AppCommand;
use crate::app_event::AppEvent;
use crate::app_event::ConnectorsSnapshot;
use crate::app_info::app_info_from_api;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::status_account_display_from_auth_mode;
use crate::computer_use_provider::ComputerUseProviderOutcome;
use crate::computer_use_provider::handle_computer_use;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::AuthMode;
use codex_app_server_protocol::RateLimitReachedType;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;

impl App {
    pub(super) fn refresh_mcp_startup_expected_servers_from_config(&mut self) {
        let enabled_config_mcp_servers: Vec<String> = self
            .config
            .mcp_servers
            .get()
            .iter()
            .filter_map(|(name, server)| server.enabled.then_some(name.clone()))
            .collect();
        self.chat_widget
            .set_mcp_startup_expected_servers(enabled_config_mcp_servers);
    }

    pub(super) async fn handle_app_server_event(
        &mut self,
        app_server_client: &AppServerSession,
        event: AppServerEvent,
    ) {
        match event {
            AppServerEvent::Lagged { skipped } => {
                tracing::warn!(
                    skipped,
                    "app-server event consumer lagged; dropping ignored events"
                );
                self.refresh_mcp_startup_expected_servers_from_config();
                self.chat_widget.finish_mcp_startup_after_lag();
            }
            AppServerEvent::ServerNotification(notification) => {
                self.handle_server_notification_event(app_server_client, notification)
                    .await;
            }
            AppServerEvent::ServerRequest(request) => {
                self.handle_server_request_event(app_server_client, request)
                    .await;
            }
            AppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                notification,
            } => {
                self.handle_thread_subscription_notification(
                    app_server_client,
                    thread_subscription_id,
                    notification,
                )
                .await;
            }
            AppServerEvent::ThreadServerRequest {
                thread_subscription_id,
                request,
            } => {
                self.handle_thread_subscription_request(
                    app_server_client,
                    thread_subscription_id,
                    request,
                )
                .await;
            }
            AppServerEvent::Disconnected { message } => {
                tracing::warn!("app-server event stream disconnected: {message}");
                self.chat_widget.add_error_message(message.clone());
                self.app_event_tx.send(AppEvent::FatalExitRequest(message));
            }
        }
    }

    /// Completes the response-to-event registration handshake. App-server
    /// registers and returns the identity before it replays pending requests;
    /// the TUI installs the matching lifecycle before draining frames that may
    /// have arrived around that response.
    pub(super) async fn bind_thread_subscription_and_flush(
        &mut self,
        app_server_client: &AppServerSession,
        thread_id: codex_protocol::ThreadId,
        thread_subscription_id: Option<String>,
    ) {
        let Some(thread_subscription_id) = thread_subscription_id else {
            return;
        };
        self.bind_thread_subscription(thread_id, Some(thread_subscription_id.clone()));

        self.flush_deferred_thread_subscription_events(app_server_client, &thread_subscription_id)
            .await;
    }

    /// Drains only frames bearing the already-bound immutable identity. This
    /// keeps a concurrent attach for another thread or generation deferred
    /// until its own authoritative handshake arrives.
    async fn flush_deferred_thread_subscription_events(
        &mut self,
        app_server_client: &AppServerSession,
        thread_subscription_id: &str,
    ) {
        let deferred = std::mem::take(&mut self.deferred_thread_subscription_events);
        for event in deferred {
            if thread_subscription_id_for_event(&event)
                .is_some_and(|candidate| candidate == thread_subscription_id)
            {
                match event {
                    AppServerEvent::ThreadServerNotification { notification, .. } => {
                        let Some(target) = self
                            .thread_subscription_targets
                            .get(thread_subscription_id)
                            .copied()
                            .map(|binding| match binding {
                                super::ThreadSubscriptionBinding::Active(target)
                                | super::ThreadSubscriptionBinding::Tombstoned(target) => target,
                            })
                        else {
                            self.deferred_thread_subscription_events.push_back(
                                AppServerEvent::ThreadServerNotification {
                                    thread_subscription_id: thread_subscription_id.to_string(),
                                    notification,
                                },
                            );
                            continue;
                        };
                        self.handle_thread_server_notification_at_ingress(
                            app_server_client,
                            target,
                            notification,
                        )
                        .await;
                    }
                    AppServerEvent::ThreadServerRequest { request, .. } => {
                        let Some(target) = self
                            .thread_subscription_targets
                            .get(thread_subscription_id)
                            .copied()
                            .map(|binding| match binding {
                                super::ThreadSubscriptionBinding::Active(target)
                                | super::ThreadSubscriptionBinding::Tombstoned(target) => target,
                            })
                        else {
                            self.deferred_thread_subscription_events.push_back(
                                AppServerEvent::ThreadServerRequest {
                                    thread_subscription_id: thread_subscription_id.to_string(),
                                    request,
                                },
                            );
                            continue;
                        };
                        self.handle_thread_server_request_at_ingress(
                            app_server_client,
                            target,
                            request,
                        )
                        .await;
                    }
                    _ => {}
                }
            } else {
                self.deferred_thread_subscription_events.push_back(event);
            }
        }
    }

    /// A spawned child can be attached by the server without a corresponding
    /// start/resume/fork response. Its tagged `thread/started` notification is
    /// the server-authoritative lifecycle handshake: bind exactly that token
    /// before routing the child metadata and any traffic deferred ahead of it.
    ///
    /// This is deliberately narrower than ordinary thread-id routing. Once a
    /// local lifecycle has been discarded or has any active/tombstoned token,
    /// a different unknown token cannot resurrect it. The normal explicit
    /// attach response remains the only path that can establish that later
    /// lifecycle.
    async fn handle_automatic_thread_subscription_started(
        &mut self,
        app_server_client: &AppServerSession,
        thread_subscription_id: String,
        notification: ServerNotification,
    ) -> bool {
        let ServerNotification::ThreadStarted(started) = &notification else {
            return false;
        };
        let Ok(thread_id) = codex_protocol::ThreadId::from_string(&started.thread.id) else {
            tracing::debug!(
                thread_subscription_id,
                thread_id = %started.thread.id,
                "dropping automatic thread-started subscription with an invalid thread id"
            );
            return true;
        };

        let has_existing_lifecycle = self.thread_subscription_targets.values().any(|binding| {
            matches!(
                binding,
                super::ThreadSubscriptionBinding::Active(target)
                    | super::ThreadSubscriptionBinding::Tombstoned(target)
                    if target.thread_id == thread_id
            )
        });
        if self.thread_is_discarded(thread_id) || has_existing_lifecycle {
            tracing::debug!(
                %thread_id,
                thread_subscription_id,
                "dropping unknown automatic thread-started subscription for an existing lifecycle"
            );
            return true;
        }

        let target = self.thread_lifecycle_target_at_ingress(thread_id);
        self.bind_thread_subscription_to_target(target, Some(thread_subscription_id.clone()));
        self.handle_thread_server_notification_at_ingress(app_server_client, target, notification)
            .await;
        self.flush_deferred_thread_subscription_events(app_server_client, &thread_subscription_id)
            .await;
        true
    }

    async fn handle_thread_subscription_notification(
        &mut self,
        app_server_client: &AppServerSession,
        thread_subscription_id: String,
        notification: ServerNotification,
    ) {
        match self
            .thread_subscription_targets
            .get(&thread_subscription_id)
            .copied()
        {
            Some(super::ThreadSubscriptionBinding::Active(target))
            | Some(super::ThreadSubscriptionBinding::Tombstoned(target)) => {
                self.handle_thread_server_notification_at_ingress(
                    app_server_client,
                    target,
                    notification,
                )
                .await;
            }
            None => {
                if !self
                    .handle_automatic_thread_subscription_started(
                        app_server_client,
                        thread_subscription_id.clone(),
                        notification.clone(),
                    )
                    .await
                {
                    self.deferred_thread_subscription_events.push_back(
                        AppServerEvent::ThreadServerNotification {
                            thread_subscription_id,
                            notification,
                        },
                    );
                }
            }
        }
    }

    async fn handle_thread_subscription_request(
        &mut self,
        app_server_client: &AppServerSession,
        thread_subscription_id: String,
        request: ServerRequest,
    ) {
        match self
            .thread_subscription_targets
            .get(&thread_subscription_id)
            .copied()
        {
            Some(super::ThreadSubscriptionBinding::Active(target)) => {
                self.handle_thread_server_request_at_ingress(app_server_client, target, request)
                    .await;
            }
            Some(super::ThreadSubscriptionBinding::Tombstoned(target)) => {
                if self
                    .rejected_stale_thread_subscription_requests
                    .insert((thread_subscription_id.clone(), request.id().clone()))
                {
                    self.handle_thread_server_request_at_ingress(
                        app_server_client,
                        target,
                        request,
                    )
                    .await;
                } else {
                    tracing::debug!(
                        request_id = ?request.id(),
                        thread_subscription_id,
                        "dropping duplicate stale app-server request"
                    );
                }
            }
            None => {
                self.deferred_thread_subscription_events.push_back(
                    AppServerEvent::ThreadServerRequest {
                        thread_subscription_id,
                        request,
                    },
                );
            }
        }
    }

    async fn handle_server_notification_event(
        &mut self,
        app_server_client: &AppServerSession,
        notification: ServerNotification,
    ) {
        let ingress_target = match server_notification_thread_target(&notification) {
            ServerNotificationThreadTarget::Thread(thread_id) => {
                Some(self.thread_lifecycle_target_at_ingress(thread_id))
            }
            ServerNotificationThreadTarget::InvalidThreadId(_)
            | ServerNotificationThreadTarget::AppScoped
            | ServerNotificationThreadTarget::Global => None,
        };
        self.handle_server_notification_event_at_ingress(
            app_server_client,
            notification,
            ingress_target,
        )
        .await;
    }

    /// Delivers a notification from a listener which already captured its thread lifecycle.
    ///
    /// Listener work must retain this token across asynchronous hand-off; recomputing it here
    /// would let an old subscription event mutate a same-id reattachment.
    pub(super) async fn handle_thread_server_notification_at_ingress(
        &mut self,
        app_server_client: &AppServerSession,
        target: ThreadLifecycleTarget,
        notification: ServerNotification,
    ) {
        self.handle_server_notification_event_at_ingress(
            app_server_client,
            notification,
            Some(target),
        )
        .await;
    }

    async fn handle_server_notification_event_at_ingress(
        &mut self,
        app_server_client: &AppServerSession,
        notification: ServerNotification,
        ingress_target: Option<ThreadLifecycleTarget>,
    ) {
        let thread_target = server_notification_thread_target(&notification);
        if let Some(ingress_target) = ingress_target {
            if !matches!(
                &thread_target,
                ServerNotificationThreadTarget::Thread(thread_id)
                    if *thread_id == ingress_target.thread_id
            ) || !self.thread_accepts_ingress_target(ingress_target)
            {
                tracing::debug!(
                    thread_id = %ingress_target.thread_id,
                    lifecycle_generation = ingress_target.lifecycle_generation,
                    "dropping app-server notification from a stale thread ingress listener"
                );
                return;
            }
        }
        if let ServerNotificationThreadTarget::Thread(thread_id) = &thread_target
            && self.thread_is_discarded(*thread_id)
        {
            tracing::debug!(
                %thread_id,
                "dropping app-server notification for discarded thread lifecycle"
            );
            return;
        }
        match &notification {
            ServerNotification::ServerRequestResolved(notification) => {
                if let Some(ingress_target) = ingress_target
                    && let Some(owner) = self
                        .pending_app_server_requests
                        .thread_target_for_request_id(&notification.request_id)
                    && owner != ingress_target
                {
                    tracing::debug!(
                        thread_id = %ingress_target.thread_id,
                        lifecycle_generation = ingress_target.lifecycle_generation,
                        request_id = ?notification.request_id,
                        "dropping server-request resolution for a different thread lifecycle"
                    );
                    return;
                }
                if let Some(request) = self
                    .pending_app_server_requests
                    .resolve_notification(&notification.request_id)
                {
                    self.chat_widget.dismiss_app_server_request(&request);
                }
            }
            ServerNotification::McpServerStatusUpdated(_) => {
                self.refresh_mcp_startup_expected_servers_from_config();
            }
            ServerNotification::AccountRateLimitsUpdated(notification) => {
                if matches!(
                    notification.rate_limits.rate_limit_reached_type,
                    Some(
                        RateLimitReachedType::WorkspaceOwnerCreditsDepleted
                            | RateLimitReachedType::WorkspaceMemberCreditsDepleted
                            | RateLimitReachedType::WorkspaceOwnerUsageLimitReached
                            | RateLimitReachedType::WorkspaceMemberUsageLimitReached
                    )
                ) || notification.rate_limits.spend_control_reached == Some(true)
                {
                    self.rate_limit_hard_stop_generation =
                        self.rate_limit_hard_stop_generation.wrapping_add(1);
                }
                self.chat_widget
                    .on_rolling_rate_limit_snapshot(notification.rate_limits.clone());
                return;
            }
            ServerNotification::AccountUpdated(notification) => {
                let has_codex_backend_auth = matches!(
                    notification.auth_mode,
                    Some(
                        AuthMode::Chatgpt
                            | AuthMode::ChatgptAuthTokens
                            | AuthMode::AgentIdentity
                            | AuthMode::PersonalAccessToken
                    )
                );
                self.chat_widget.update_account_state(
                    status_account_display_from_auth_mode(
                        notification.auth_mode,
                        notification.plan_type,
                    ),
                    notification.plan_type,
                    notification
                        .auth_mode
                        .is_some_and(AuthMode::has_chatgpt_account),
                    has_codex_backend_auth,
                );
                return;
            }
            ServerNotification::ExternalAgentConfigImportCompleted(notification) => {
                let should_report_completion =
                    app_server_client.consume_external_agent_config_import_completion();
                if let Err(err) = self.refresh_in_memory_config_from_disk().await {
                    tracing::warn!(
                        error = %err,
                        "failed to refresh config after external agent config import"
                    );
                }
                let cwd = self.chat_widget.config_ref().cwd.to_path_buf();
                self.chat_widget.refresh_plugin_mentions();
                self.chat_widget.submit_op(AppCommand::reload_user_config());
                self.fetch_plugins_list(app_server_client, cwd);
                if should_report_completion {
                    self.chat_widget.add_plain_history_lines(
                        crate::external_agent_config_migration_flow::external_agent_config_migration_finished_lines(notification),
                    );
                }
                return;
            }
            ServerNotification::AppListUpdated(notification) => {
                self.chat_widget.on_connectors_loaded(
                    Ok(ConnectorsSnapshot {
                        connectors: notification
                            .data
                            .iter()
                            .cloned()
                            .map(app_info_from_api)
                            .collect(),
                    }),
                    /*is_final*/ false,
                );
                return;
            }
            _ => {}
        }

        match thread_target {
            ServerNotificationThreadTarget::Thread(thread_id) => {
                let result = if self.primary_thread_id == Some(thread_id)
                    || self.primary_thread_id.is_none()
                {
                    self.enqueue_primary_thread_notification(thread_id, notification)
                        .await
                } else {
                    self.enqueue_thread_notification(thread_id, notification)
                        .await
                };

                if let Err(err) = result {
                    tracing::warn!("failed to enqueue app-server notification: {err}");
                }
                return;
            }
            ServerNotificationThreadTarget::InvalidThreadId(thread_id) => {
                tracing::warn!(
                    thread_id,
                    "ignoring app-server notification with invalid thread_id"
                );
                return;
            }
            ServerNotificationThreadTarget::AppScoped => {
                tracing::debug!(
                    "ignoring app-scoped MCP startup notification without a TUI app-level target"
                );
                return;
            }
            ServerNotificationThreadTarget::Global => {}
        }

        self.chat_widget
            .handle_server_notification(notification, /*replay_kind*/ None);
    }

    async fn handle_server_request_event(
        &mut self,
        app_server_client: &AppServerSession,
        request: ServerRequest,
    ) {
        let ingress_target = server_request_thread_id(&request)
            .map(|thread_id| self.thread_lifecycle_target_at_ingress(thread_id));
        self.handle_server_request_event_at_ingress(app_server_client, request, ingress_target)
            .await;
    }

    /// Delivers a request from a listener which already captured its thread lifecycle.
    ///
    /// Unlike notifications, stale requests are explicitly rejected with their original JSON-RPC
    /// id, so the old server-side call cannot remain pending after the UI has moved on.
    pub(super) async fn handle_thread_server_request_at_ingress(
        &mut self,
        app_server_client: &AppServerSession,
        target: ThreadLifecycleTarget,
        request: ServerRequest,
    ) {
        self.handle_server_request_event_at_ingress(app_server_client, request, Some(target))
            .await;
    }

    async fn handle_server_request_event_at_ingress(
        &mut self,
        app_server_client: &AppServerSession,
        request: ServerRequest,
        ingress_target: Option<ThreadLifecycleTarget>,
    ) {
        let thread_id = server_request_thread_id(&request);
        if let Some(ingress_target) = ingress_target
            && (thread_id != Some(ingress_target.thread_id)
                || !self.thread_accepts_ingress_target(ingress_target))
        {
            tracing::debug!(
                thread_id = %ingress_target.thread_id,
                lifecycle_generation = ingress_target.lifecycle_generation,
                request_id = ?request.id(),
                "rejecting app-server request from a stale thread ingress listener"
            );
            if let Err(err) = self
                .reject_app_server_request(
                    app_server_client,
                    request.id().clone(),
                    format!(
                        "The TUI no longer accepts requests for thread {} lifecycle {}.",
                        ingress_target.thread_id, ingress_target.lifecycle_generation
                    ),
                )
                .await
            {
                tracing::warn!(
                    thread_id = %ingress_target.thread_id,
                    error = %err,
                    "failed to reject stale app-server request"
                );
            }
            return;
        }
        if thread_id.is_some_and(|thread_id| self.thread_is_discarded(thread_id)) {
            let thread_id = thread_id.expect("checked as present above");
            tracing::debug!(
                %thread_id,
                request_id = ?request.id(),
                "rejecting app-server request for discarded thread lifecycle"
            );
            if let Err(err) = self
                .reject_app_server_request(
                    app_server_client,
                    request.id().clone(),
                    format!(
                        "The TUI discarded thread {thread_id} before this request could be handled."
                    ),
                )
                .await
            {
                tracing::warn!(
                    %thread_id,
                    error = %err,
                    "failed to reject discarded app-server request"
                );
            }
            return;
        }
        if let ServerRequest::ComputerUseCall { request_id, params } = &request {
            let request_id = request_id.clone();
            match handle_computer_use(params).await {
                ComputerUseProviderOutcome::Handled(response) => {
                    let result = match serde_json::to_value(response) {
                        Ok(result) => result,
                        Err(err) => {
                            tracing::warn!("failed to serialize computer-use response: {err}");
                            return;
                        }
                    };
                    if let Err(err) = app_server_client
                        .resolve_server_request(request_id, result)
                        .await
                    {
                        tracing::warn!("failed to resolve computer-use request: {err}");
                    }
                }
                ComputerUseProviderOutcome::Unavailable => {
                    let message = format!(
                        "No TUI computer-use provider is available for `{}`/`{}`.",
                        params.adapter, params.tool
                    );
                    if let Err(err) = self
                        .reject_app_server_request(app_server_client, request_id, message)
                        .await
                    {
                        tracing::warn!("{err}");
                    }
                }
            }
            return;
        }

        let unsupported = if let Some(target) = ingress_target {
            self.pending_app_server_requests
                .note_thread_server_request_for_lifecycle(target, &request)
        } else {
            self.pending_app_server_requests
                .note_server_request(&request)
        };
        if let Some(unsupported) = unsupported {
            tracing::warn!(
                request_id = ?unsupported.request_id,
                message = unsupported.message,
                "rejecting unsupported app-server request"
            );
            self.chat_widget
                .add_error_message(unsupported.message.clone());
            if let Err(err) = self
                .reject_app_server_request(
                    app_server_client,
                    unsupported.request_id,
                    unsupported.message,
                )
                .await
            {
                tracing::warn!("{err}");
            }
            return;
        }

        let Some(thread_id) = thread_id else {
            tracing::warn!("ignoring threadless app-server request");
            return;
        };

        let result =
            if self.primary_thread_id == Some(thread_id) || self.primary_thread_id.is_none() {
                self.enqueue_primary_thread_request(thread_id, request)
                    .await
            } else {
                self.enqueue_thread_request(thread_id, request).await
            };
        if let Err(err) = result {
            tracing::warn!("failed to enqueue app-server request: {err}");
        }
    }
}

fn thread_subscription_id_for_event(event: &AppServerEvent) -> Option<&str> {
    match event {
        AppServerEvent::ThreadServerNotification {
            thread_subscription_id,
            ..
        }
        | AppServerEvent::ThreadServerRequest {
            thread_subscription_id,
            ..
        } => Some(thread_subscription_id),
        _ => None,
    }
}
