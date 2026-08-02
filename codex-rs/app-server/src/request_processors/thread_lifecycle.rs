use super::*;

use crate::extensions::send_thread_warning;
use crate::extensions::spawn_thread_warning_barrier_resolution;
use crate::outgoing_message::ThreadSubscriptionTarget;
use codex_protocol::config_types::MultiAgentMode;

pub(super) const THREAD_UNLOADING_DELAY: Duration = Duration::from_secs(30 * 60);

#[derive(Clone)]
pub(super) struct ListenerTaskContext {
    pub(super) thread_manager: Arc<ThreadManager>,
    pub(super) thread_state_manager: ThreadStateManager,
    pub(super) outgoing: Arc<OutgoingMessageSender>,
    pub(super) pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
    pub(super) thread_watch_manager: ThreadWatchManager,
    pub(super) thread_list_state_permit: Arc<Semaphore>,
    pub(super) fallback_model_provider: String,
    pub(super) codex_home: PathBuf,
    pub(super) skills_watcher: Arc<SkillsWatcher>,
}

struct UnloadingState {
    delay: Duration,
    has_subscribers_rx: watch::Receiver<bool>,
    has_subscribers: (bool, Instant),
    thread_status_rx: watch::Receiver<ThreadStatus>,
    is_active: (bool, Instant),
}

impl UnloadingState {
    async fn new(
        listener_task_context: &ListenerTaskContext,
        thread_id: ThreadId,
        delay: Duration,
    ) -> Option<Self> {
        let has_subscribers_rx = listener_task_context
            .thread_state_manager
            .subscribe_to_has_connections(thread_id)
            .await?;
        let thread_status_rx = listener_task_context
            .thread_watch_manager
            .subscribe(thread_id)
            .await?;
        let has_subscribers = (*has_subscribers_rx.borrow(), Instant::now());
        let is_active = (
            matches!(*thread_status_rx.borrow(), ThreadStatus::Active { .. }),
            Instant::now(),
        );
        Some(Self {
            delay,
            has_subscribers_rx,
            has_subscribers,
            thread_status_rx,
            is_active,
        })
    }

    fn unloading_target(&self) -> Option<Instant> {
        match (self.has_subscribers, self.is_active) {
            ((false, has_no_subscribers_since), (false, is_inactive_since)) => {
                Some(std::cmp::max(has_no_subscribers_since, is_inactive_since) + self.delay)
            }
            _ => None,
        }
    }

    fn sync_receiver_values(&mut self) {
        let has_subscribers = *self.has_subscribers_rx.borrow();
        if self.has_subscribers.0 != has_subscribers {
            self.has_subscribers = (has_subscribers, Instant::now());
        }

        let is_active = matches!(*self.thread_status_rx.borrow(), ThreadStatus::Active { .. });
        if self.is_active.0 != is_active {
            self.is_active = (is_active, Instant::now());
        }
    }

    fn should_unload_now(&mut self) -> bool {
        self.sync_receiver_values();
        self.unloading_target()
            .is_some_and(|target| target <= Instant::now())
    }

    fn note_thread_activity_observed(&mut self) {
        if !self.is_active.0 {
            self.is_active = (false, Instant::now());
        }
    }

    async fn wait_for_unloading_trigger(&mut self) -> bool {
        loop {
            self.sync_receiver_values();
            let unloading_target = self.unloading_target();
            if let Some(target) = unloading_target
                && target <= Instant::now()
            {
                return true;
            }
            let unloading_sleep = async {
                if let Some(target) = unloading_target {
                    tokio::time::sleep_until(target.into()).await;
                } else {
                    futures::future::pending::<()>().await;
                }
            };
            tokio::select! {
                _ = unloading_sleep => return true,
                changed = self.has_subscribers_rx.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                    self.sync_receiver_values();
                },
                changed = self.thread_status_rx.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                    self.sync_receiver_values();
                },
            }
        }
    }
}

pub(super) enum ThreadShutdownResult {
    Complete,
    SubmitFailed,
    TimedOut,
}

pub(super) enum EnsureConversationListenerResult {
    Attached,
    ConnectionClosed,
}

/// Whether `thread/start` may publish its connection-bound receipt and lifecycle notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ThreadStartAttachmentPublication {
    Publish,
    Suppress,
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "listener subscription must be serialized against pending unloads"
)]
pub(super) async fn ensure_conversation_listener(
    listener_task_context: ListenerTaskContext,
    conversation_id: ThreadId,
    connection_id: ConnectionId,
    raw_events_enabled: bool,
    expected_subscription_id: Option<&str>,
) -> Result<EnsureConversationListenerResult, JSONRPCErrorError> {
    let conversation = match listener_task_context
        .thread_manager
        .get_thread(conversation_id)
        .await
    {
        Ok(conv) => conv,
        Err(_) => {
            return Err(invalid_request(format!(
                "thread not found: {conversation_id}"
            )));
        }
    };
    let (thread_state, thread_subscription_id) = {
        let pending_thread_unloads = listener_task_context.pending_thread_unloads.lock().await;
        if pending_thread_unloads.contains(&conversation_id) {
            return Err(invalid_request(format!(
                "thread {conversation_id} is closing; retry after the thread is closed"
            )));
        }
        // An automatic attachment can be the first route by which a thread
        // reaches this connection (for example, a spawned child). Establish
        // server-owned identity before publishing the connection to a running
        // listener, so no thread event can observe a subscribed connection
        // without an immutable subscription identity.
        let thread_subscription_id = match expected_subscription_id {
            Some(expected_subscription_id) => {
                if !listener_task_context
                    .outgoing
                    .thread_subscription_matches(
                        connection_id,
                        conversation_id,
                        expected_subscription_id,
                    )
                    .await
                {
                    return Ok(EnsureConversationListenerResult::ConnectionClosed);
                }
                expected_subscription_id.to_string()
            }
            None => listener_task_context
                .outgoing
                .ensure_thread_subscription(connection_id, conversation_id)
                .await,
        };
        let Some(thread_state) = listener_task_context
            .thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                conversation_id,
                connection_id,
                raw_events_enabled,
                Some(thread_subscription_id.clone()),
            )
            .await
        else {
            listener_task_context
                .outgoing
                .unregister_thread_subscription_if_matches(
                    connection_id,
                    conversation_id,
                    &thread_subscription_id,
                )
                .await;
            return Ok(EnsureConversationListenerResult::ConnectionClosed);
        };
        if !listener_task_context
            .outgoing
            .thread_subscription_matches(connection_id, conversation_id, &thread_subscription_id)
            .await
        {
            let _ = listener_task_context
                .thread_state_manager
                .unsubscribe_connection_from_thread_if_subscription_matches(
                    conversation_id,
                    connection_id,
                    &thread_subscription_id,
                )
                .await;
            return Ok(EnsureConversationListenerResult::ConnectionClosed);
        }
        (thread_state, thread_subscription_id)
    };
    if let Err(error) = ensure_listener_task_running(
        listener_task_context.clone(),
        conversation_id,
        conversation,
        thread_state,
    )
    .await
    {
        rollback_failed_thread_attach(
            &listener_task_context.thread_state_manager,
            &listener_task_context.outgoing,
            conversation_id,
            connection_id,
            &thread_subscription_id,
        )
        .await;
        return Err(error);
    }
    Ok(EnsureConversationListenerResult::Attached)
}

/// Removes the connection-local state created by a failed start, resume, or fork
/// attachment before the caller can publish a response containing its token.
///
/// Drop the failed outgoing identity first: listener fanout uses that map, so this fences any
/// event accepted while hydration, cursor construction, or response assembly failed. The
/// state-manager removal then makes that failed attachment ineligible for future fanout. If the
/// state manager still owns an earlier live identity, restore it only while the outgoing map is
/// unclaimed so a concurrent replacement cannot be overwritten.
pub(super) async fn rollback_failed_thread_attach(
    thread_state_manager: &ThreadStateManager,
    outgoing: &Arc<OutgoingMessageSender>,
    thread_id: ThreadId,
    connection_id: ConnectionId,
    expected_subscription_id: &str,
) {
    rollback_failed_thread_attach_with_predecessor(
        thread_state_manager,
        outgoing,
        thread_id,
        connection_id,
        expected_subscription_id,
        None,
    )
    .await;
}

/// Rolls back a provisional attachment with the predecessor captured before B overwrote the
/// state-manager identity. That capture is necessary for the running-resume path, where the
/// listener may be live on A while B is being hydrated.
pub(super) async fn rollback_failed_thread_attach_with_predecessor(
    thread_state_manager: &ThreadStateManager,
    outgoing: &Arc<OutgoingMessageSender>,
    thread_id: ThreadId,
    connection_id: ConnectionId,
    expected_subscription_id: &str,
    predecessor_subscription_id: Option<&str>,
) {
    let removed_failed_subscription = outgoing
        .unregister_thread_subscription_if_matches(
            connection_id,
            thread_id,
            expected_subscription_id,
        )
        .await;
    if let Some(predecessor_subscription_id) = predecessor_subscription_id
        && thread_state_manager
            .restore_connection_subscription_if_matches(
                thread_id,
                connection_id,
                expected_subscription_id,
                predecessor_subscription_id,
            )
            .await
    {
        // Only restore transport A after state-manager A won the exact-current check. If C
        // claimed the outgoing map in the meantime, this is a no-op and C remains authoritative.
        let _ = outgoing
            .restore_thread_subscription_if_unclaimed(
                connection_id,
                thread_id,
                predecessor_subscription_id.to_string(),
            )
            .await;
        return;
    }
    if removed_failed_subscription {
        let _ = thread_state_manager
            .unsubscribe_connection_from_thread_if_subscription_matches(
                thread_id,
                connection_id,
                expected_subscription_id,
            )
            .await;
    }

    if let Some(active_subscription_id) = thread_state_manager
        .connection_thread_subscription_id(thread_id, connection_id)
        .await
        .filter(|subscription_id| subscription_id.as_str() != expected_subscription_id)
    {
        // A failed provisional resume can have displaced this map entry while an older attachment
        // is still live in ThreadStateManager. Put that active owner back only when a competing
        // replacement has not claimed the outgoing map in the meantime.
        if outgoing
            .restore_thread_subscription_if_unclaimed(
                connection_id,
                thread_id,
                active_subscription_id.clone(),
            )
            .await
            && thread_state_manager
                .connection_thread_subscription_id(thread_id, connection_id)
                .await
                .as_deref()
                != Some(active_subscription_id.as_str())
        {
            outgoing
                .unregister_thread_subscription_if_matches(
                    connection_id,
                    thread_id,
                    &active_subscription_id,
                )
                .await;
        }
    }
}

/// Gates `thread/start` publication on listener attachment and rolls back the pre-minted
/// connection-local identity on every non-publishing path. A successful start may replay traffic
/// before its receipt, so the identity must exist before attachment; it must never escape in a
/// response or `ThreadStarted` notification if attachment did not succeed.
pub(super) async fn gate_thread_start_listener_attachment(
    listener_result: Result<EnsureConversationListenerResult, JSONRPCErrorError>,
    thread_state_manager: &ThreadStateManager,
    outgoing: &Arc<OutgoingMessageSender>,
    thread_id: ThreadId,
    connection_id: ConnectionId,
    thread_subscription_id: &str,
) -> Result<ThreadStartAttachmentPublication, JSONRPCErrorError> {
    match listener_result {
        Ok(EnsureConversationListenerResult::Attached) => {
            Ok(ThreadStartAttachmentPublication::Publish)
        }
        Ok(EnsureConversationListenerResult::ConnectionClosed) => {
            rollback_failed_thread_attach(
                thread_state_manager,
                outgoing,
                thread_id,
                connection_id,
                thread_subscription_id,
            )
            .await;
            Ok(ThreadStartAttachmentPublication::Suppress)
        }
        Err(error) => {
            rollback_failed_thread_attach(
                thread_state_manager,
                outgoing,
                thread_id,
                connection_id,
                thread_subscription_id,
            )
            .await;
            Err(error)
        }
    }
}

pub(super) fn log_listener_attach_result(
    result: Result<EnsureConversationListenerResult, JSONRPCErrorError>,
    thread_id: ThreadId,
    connection_id: ConnectionId,
    thread_kind: &'static str,
) {
    match result {
        Ok(EnsureConversationListenerResult::Attached) => {}
        Ok(EnsureConversationListenerResult::ConnectionClosed) => {
            tracing::debug!(
                thread_id = %thread_id,
                connection_id = ?connection_id,
                "skipping auto-attach for closed connection"
            );
        }
        Err(err) => {
            tracing::warn!(
                "failed to attach listener for {thread_kind} {thread_id}: {message}",
                message = err.message
            );
        }
    }
}

pub(super) async fn ensure_listener_task_running(
    listener_task_context: ListenerTaskContext,
    conversation_id: ThreadId,
    conversation: Arc<CodexThread>,
    thread_state: Arc<Mutex<ThreadState>>,
) -> Result<(), JSONRPCErrorError> {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let Some(mut unloading_state) = UnloadingState::new(
        &listener_task_context,
        conversation_id,
        THREAD_UNLOADING_DELAY,
    )
    .await
    else {
        return Err(invalid_request(format!(
            "thread {conversation_id} is closing; retry after the thread is closed"
        )));
    };
    let config = conversation.config().await;
    let environments = conversation.environment_selections().await;
    let watch_registration = listener_task_context
        .skills_watcher
        .register_thread_config(
            config.as_ref(),
            listener_task_context.thread_manager.as_ref(),
            &environments,
        )
        .await;
    let thread_settings_baseline =
        thread_settings_from_config_snapshot(&conversation.config_snapshot().await);
    let (mut listener_command_rx, listener_generation, listener_command_tx) = {
        let mut thread_state = thread_state.lock().await;
        if thread_state.listener_matches(&conversation) {
            return Ok(());
        }
        let (listener_command_rx, listener_generation) = thread_state.set_listener(
            cancel_tx,
            &conversation,
            watch_registration,
            thread_settings_baseline,
        );
        let Some(listener_command_tx) = thread_state.listener_command_tx() else {
            tracing::warn!(
                "thread listener command sender missing immediately after listener registration"
            );
            return Ok(());
        };
        (listener_command_rx, listener_generation, listener_command_tx)
    };
    listener_task_context
        .thread_state_manager
        .invalidate_targetless_warning_wait_before_listener_generation(
            conversation_id,
            listener_generation,
        );
    listener_task_context
        .outgoing
        .release_thread_outbound_barriers_before_generation(conversation_id, listener_generation)
        .await;
    listener_task_context
        .thread_state_manager
        .register_listener_command_tx(conversation_id, listener_generation, listener_command_tx);
    let ListenerTaskContext {
        outgoing,
        thread_manager,
        thread_state_manager,
        pending_thread_unloads,
        thread_watch_manager,
        thread_list_state_permit,
        fallback_model_provider,
        codex_home,
        ..
    } = listener_task_context;
    let outgoing_for_task = Arc::clone(&outgoing);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut cancel_rx => {
                    // Listener was superseded or the thread is being torn down.
                    break;
                }
                listener_command = listener_command_rx.recv() => {
                    let Some(listener_command) = listener_command else {
                        break;
                    };
                    let command_outcome = handle_thread_listener_command(
                        conversation_id,
                        &conversation,
                        codex_home.as_path(),
                        &thread_state_manager,
                        &thread_state,
                        &thread_watch_manager,
                        &outgoing_for_task,
                        &pending_thread_unloads,
                        listener_generation,
                        listener_command,
                    )
                    .await;
                    if let ThreadListenerCommandOutcome::PostCommit(post_commit_command) =
                        command_outcome
                    {
                        let post_commit_outcome = handle_thread_listener_command(
                            conversation_id,
                            &conversation,
                            codex_home.as_path(),
                            &thread_state_manager,
                            &thread_state,
                            &thread_watch_manager,
                            &outgoing_for_task,
                            &pending_thread_unloads,
                            listener_generation,
                            post_commit_command,
                        )
                        .await;
                        debug_assert!(matches!(
                            post_commit_outcome,
                            ThreadListenerCommandOutcome::Complete
                        ));
                    }
                }
                event = conversation.next_event() => {
                    let event = match event {
                        Ok(event) => event,
                        Err(err) => {
                            tracing::warn!("thread.next_event() failed with: {err}");
                            break;
                        }
                    };

                    // This is listener ingress. Capture the exact
                    // connection-scoped identities before any other await or
                    // event bookkeeping can yield: a later unsubscribe and
                    // reattach of the same connection must not relabel this
                    // already accepted event with its replacement token.
                    let thread_subscriptions = outgoing_for_task
                        .thread_subscription_targets_for_thread(conversation_id)
                        .await;

                    // Track the event before emitting any typed translations
                    // so thread-local state such as raw event opt-in stays
                    // synchronized with the conversation.
                    let raw_events_enabled = {
                        let mut thread_state = thread_state.lock().await;
                        thread_state.track_current_turn_event(&event.id, &event.msg);
                        thread_state.experimental_raw_events
                    };
                    if matches!(
                        &event.msg,
                        EventMsg::RawResponseItem(_) | EventMsg::RawResponseCompleted(_)
                    ) && !raw_events_enabled
                    {
                        continue;
                    }
                    let thread_outgoing = ThreadScopedOutgoingMessageSender::from_captured_thread_subscriptions(
                        outgoing_for_task.clone(),
                        thread_subscriptions,
                        conversation_id,
                    );

                    apply_bespoke_event_handling(
                        event.clone(),
                        conversation_id,
                        conversation.clone(),
                        thread_manager.clone(),
                        thread_outgoing,
                        thread_state.clone(),
                        thread_watch_manager.clone(),
                        thread_list_state_permit.clone(),
                        fallback_model_provider.clone(),
                    )
                    .await;
                }
                unloading_watchers_open = unloading_state.wait_for_unloading_trigger() => {
                    if !unloading_watchers_open {
                        break;
                    }
                    if !unloading_state.should_unload_now() {
                        continue;
                    }
                    if matches!(conversation.agent_status().await, AgentStatus::Running) {
                        unloading_state.note_thread_activity_observed();
                        continue;
                    }
                    {
                        let mut pending_thread_unloads = pending_thread_unloads.lock().await;
                        if pending_thread_unloads.contains(&conversation_id) {
                            continue;
                        }
                        if !unloading_state.should_unload_now() {
                            continue;
                        }
                        pending_thread_unloads.insert(conversation_id);
                    }
                    unload_thread_without_subscribers(
                        thread_manager.clone(),
                        outgoing_for_task.clone(),
                        pending_thread_unloads.clone(),
                        thread_state_manager.clone(),
                        thread_watch_manager.clone(),
                        conversation_id,
                        conversation.clone(),
                    )
                    .await;
                    break;
                }
            }
        }

        let cleared_listener = {
            let mut thread_state = thread_state.lock().await;
            if thread_state.listener_generation != listener_generation {
                false
            } else {
                thread_state_manager.unregister_listener_command_tx_if_generation(
                    conversation_id,
                    listener_generation,
                );
                thread_state.clear_listener();
                true
            }
        };
        if cleared_listener {
            outgoing_for_task
                .release_thread_outbound_barrier_for_listener_generation(
                    conversation_id,
                    listener_generation,
                )
                .await;
        }
    });
    Ok(())
}

pub(super) async fn wait_for_thread_shutdown(thread: &Arc<CodexThread>) -> ThreadShutdownResult {
    match tokio::time::timeout(Duration::from_secs(10), thread.shutdown_and_wait()).await {
        Ok(Ok(())) => ThreadShutdownResult::Complete,
        Ok(Err(_)) => ThreadShutdownResult::SubmitFailed,
        Err(_) => ThreadShutdownResult::TimedOut,
    }
}

pub(super) async fn unload_thread_without_subscribers(
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
) {
    info!("thread {thread_id} has no subscribers and is idle; shutting down");

    // Any pending app-server -> client requests for this thread can no longer be
    // answered; cancel their callbacks before shutdown/unload.
    outgoing
        .release_thread_outbound_barrier_for_teardown(thread_id)
        .await;
    outgoing
        .cancel_requests_for_thread(thread_id, /*error*/ None)
        .await;
    thread_state_manager.remove_thread_state(thread_id).await;

    tokio::spawn(async move {
        match wait_for_thread_shutdown(&thread).await {
            ThreadShutdownResult::Complete => {
                if thread_manager.remove_thread(&thread_id).await.is_none() {
                    info!("thread {thread_id} was already removed before teardown finalized");
                    thread_watch_manager
                        .remove_thread(&thread_id.to_string())
                        .await;
                    pending_thread_unloads.lock().await.remove(&thread_id);
                    return;
                }
                thread_watch_manager
                    .remove_thread(&thread_id.to_string())
                    .await;
                let notification = ThreadClosedNotification {
                    thread_id: thread_id.to_string(),
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadClosed(notification))
                    .await;
                pending_thread_unloads.lock().await.remove(&thread_id);
            }
            ThreadShutdownResult::SubmitFailed => {
                pending_thread_unloads.lock().await.remove(&thread_id);
                warn!("failed to submit Shutdown to thread {thread_id}");
            }
            ThreadShutdownResult::TimedOut => {
                pending_thread_unloads.lock().await.remove(&thread_id);
                warn!("thread {thread_id} shutdown timed out; leaving thread loaded");
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_thread_listener_command(
    conversation_id: ThreadId,
    conversation: &Arc<CodexThread>,
    codex_home: &Path,
    thread_state_manager: &ThreadStateManager,
    thread_state: &Arc<Mutex<ThreadState>>,
    thread_watch_manager: &ThreadWatchManager,
    outgoing: &Arc<OutgoingMessageSender>,
    pending_thread_unloads: &Arc<Mutex<HashSet<ThreadId>>>,
    listener_generation: u64,
    listener_command: ThreadListenerCommand,
) -> ThreadListenerCommandOutcome {
    match listener_command {
        ThreadListenerCommand::SendThreadResumeResponse(resume_request) => {
            let mut post_commit_command = None;
            handle_pending_thread_resume_request(
                conversation_id,
                conversation,
                codex_home,
                thread_state_manager,
                thread_state,
                thread_watch_manager,
                outgoing,
                pending_thread_unloads,
                &mut post_commit_command,
                *resume_request,
            )
            .await;
            if let Some(post_commit_command) = post_commit_command {
                return ThreadListenerCommandOutcome::PostCommit(post_commit_command);
            }
        }
        ThreadListenerCommand::CompleteThreadResume {
            thread_subscription,
            token_usage_turn_id,
            emit_thread_goal_update,
            thread_goal_state_db,
        } => {
            complete_committed_thread_resume(
                conversation_id,
                conversation,
                outgoing,
                thread_subscription,
                token_usage_turn_id,
                emit_thread_goal_update,
                thread_goal_state_db,
            )
            .await;
        }
        ThreadListenerCommand::EmitThreadGoalUpdated {
            turn_id,
            goal,
            thread_subscriptions,
        } => {
            send_captured_thread_goal_notification(
                outgoing,
                &thread_subscriptions,
                ServerNotification::ThreadGoalUpdated(ThreadGoalUpdatedNotification {
                    thread_id: conversation_id.to_string(),
                    turn_id,
                    goal,
                }),
            )
            .await;
        }
        ThreadListenerCommand::EmitWarning {
            message,
            delivery,
        } => {
            match delivery {
                crate::thread_state::ThreadWarningDelivery::Captured(thread_subscriptions) => {
                    send_thread_warning(outgoing, &thread_subscriptions, conversation_id, message)
                        .await;
                }
                crate::thread_state::ThreadWarningDelivery::AwaitCurrentSubscriber(lease) => {
                    if !thread_state_manager
                        .targetless_warning_wait_is_current(conversation_id, lease)
                    {
                        return ThreadListenerCommandOutcome::Complete;
                    }
                    // Extension ingress already installed the central transport barrier before
                    // this command was queued. Keep the listener free to process following
                    // commands while the detached waiter resolves the warning.
                    spawn_thread_warning_barrier_resolution(
                        Arc::clone(outgoing),
                        thread_state_manager.clone(),
                        conversation_id,
                        message,
                        lease,
                    );
                }
            }
        }
        ThreadListenerCommand::EmitThreadGoalCleared {
            thread_subscriptions,
        } => {
            send_captured_thread_goal_notification(
                outgoing,
                &thread_subscriptions,
                ServerNotification::ThreadGoalCleared(ThreadGoalClearedNotification {
                    thread_id: conversation_id.to_string(),
                }),
            )
            .await;
        }
        ThreadListenerCommand::EmitThreadGoalSnapshot {
            state_db,
            thread_subscriptions,
        } => {
            send_thread_goal_snapshot_notification(
                outgoing,
                &thread_subscriptions,
                conversation_id,
                &state_db,
            )
            .await;
        }
        ThreadListenerCommand::ResolveServerRequest {
            request_id,
            completion_tx,
        } => {
            resolve_pending_server_request(conversation_id, outgoing, request_id).await;
            let _ = completion_tx.send(());
        }
    }
    ThreadListenerCommandOutcome::Complete
}

enum ThreadListenerCommandOutcome {
    Complete,
    PostCommit(ThreadListenerCommand),
}

#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "running-thread resume subscription must be serialized against pending unloads"
)]
pub(super) async fn handle_pending_thread_resume_request(
    conversation_id: ThreadId,
    conversation: &Arc<CodexThread>,
    _codex_home: &Path,
    thread_state_manager: &ThreadStateManager,
    thread_state: &Arc<Mutex<ThreadState>>,
    thread_watch_manager: &ThreadWatchManager,
    outgoing: &Arc<OutgoingMessageSender>,
    pending_thread_unloads: &Arc<Mutex<HashSet<ThreadId>>>,
    post_commit_command: &mut Option<ThreadListenerCommand>,
    mut pending: crate::thread_state::PendingThreadResumeRequest,
) {
    let reservation_id = pending.reservation_id.clone();
    if !pending_thread_resume_is_current_or_reply(
        thread_state_manager,
        outgoing,
        conversation_id,
        &pending.request_id,
        &reservation_id,
    )
    .await
    {
        return;
    }
    let active_turn = {
        let state = thread_state.lock().await;
        state.active_turn_snapshot()
    };
    tracing::debug!(
        thread_id = %conversation_id,
        request_id = ?pending.request_id,
        active_turn_present = active_turn.is_some(),
        active_turn_id = ?active_turn.as_ref().map(|turn| turn.id.as_str()),
        active_turn_status = ?active_turn.as_ref().map(|turn| &turn.status),
        "composing running thread resume response"
    );
    let has_live_in_progress_turn =
        matches!(conversation.agent_status().await, AgentStatus::Running)
            || active_turn
                .as_ref()
                .is_some_and(|turn| matches!(turn.status, TurnStatus::InProgress));

    let request_id = pending.request_id;
    let connection_id = request_id.connection_id;
    let mut thread = pending.thread_summary;
    if pending.include_turns {
        if let Some(turns) = pending.paginated_turns.take() {
            thread.turns = turns;
        } else {
            populate_thread_turns_from_history(
                &mut thread,
                &pending.history_items,
                /*active_turn*/ None,
            );
        }
        if let Some(active_turn) = active_turn.as_ref() {
            merge_turn_history_with_active_turn(&mut thread.turns, active_turn.clone());
        }
    }

    let thread_status = thread_watch_manager
        .loaded_status_for_thread(&thread.id)
        .await;

    set_thread_status_and_interrupt_stale_turns(
        &mut thread,
        thread_status.clone(),
        has_live_in_progress_turn,
    );
    crate::extensions::app_server_hooks().augment_thread_resume(
        &mut thread,
        active_turn.as_ref(),
        has_live_in_progress_turn,
    );
    let token_usage_turn_id = pending
        .include_turns
        .then(|| restored_token_usage_turn_id(&pending.history_items, &thread));
    let mut initial_turns_page = if let Some(mut page) = pending.paginated_initial_turns_page.take()
    {
        if let (Some(active_turn), Some(params)) =
            (active_turn, pending.initial_turns_page.as_ref())
        {
            let sort_direction = params.sort_direction.unwrap_or(SortDirection::Desc);
            let active_turn_is_in_page = page.data.iter().any(|turn| turn.id == active_turn.id);
            if matches!(sort_direction, SortDirection::Desc)
                && !active_turn_is_in_page
                && let Some(page_with_active_slot) =
                    pending.paginated_initial_turns_page_with_active_slot.take()
            {
                page = page_with_active_slot;
            }
            merge_active_turn_into_page(&mut page, active_turn, params);
        }
        super::thread_processor::normalize_thread_turns_status(
            &mut page.data,
            thread_status,
            has_live_in_progress_turn,
        );
        Some(page)
    } else if let Some(params) = pending.initial_turns_page.as_ref() {
        match super::thread_processor::build_thread_resume_initial_turns_page(
            &pending.history_items,
            thread.status.clone(),
            has_live_in_progress_turn,
            active_turn,
            params,
        ) {
            Ok(page) => Some(page),
            Err(error) => {
                if pending_thread_resume_is_current_or_reply(
                    thread_state_manager,
                    outgoing,
                    conversation_id,
                    &request_id,
                    &reservation_id,
                )
                .await
                {
                    outgoing.send_error(request_id, error).await;
                    thread_state_manager
                        .clear_pending_thread_resume_if_matches(
                            conversation_id,
                            connection_id,
                            &reservation_id,
                        )
                        .await;
                }
                return;
            }
        }
    } else {
        None
    };
    if pending.redact_resume_payloads {
        redact_thread_resume_payloads(&mut thread.turns);
        if let Some(initial_turns_page) = initial_turns_page.as_mut() {
            redact_thread_resume_payloads(&mut initial_turns_page.data);
        }
    }

    // Install the identity before the live connection becomes eligible for
    // replayed traffic. `try_add_connection_to_thread` can make a running
    // listener fan out immediately.
    if !pending_thread_resume_is_current_or_reply(
        thread_state_manager,
        outgoing,
        conversation_id,
        &request_id,
        &reservation_id,
    )
    .await
    {
        return;
    }
    let thread_subscription_id = outgoing
        .register_thread_subscription(connection_id, conversation_id)
        .await;
    let thread_subscription = ThreadSubscriptionTarget::captured(
        connection_id,
        conversation_id,
        thread_subscription_id.clone(),
    );
    let connection_added = {
        let pending_thread_unloads = pending_thread_unloads.lock().await;
        if pending_thread_unloads.contains(&conversation_id) {
            Err(/*is_closing*/ true)
        } else if !thread_state_manager
            .pending_thread_resume_matches(conversation_id, connection_id, &reservation_id)
            .await
        {
            Err(/*is_closing*/ false)
        } else {
            Ok(
                thread_state_manager
                    .try_add_connection_to_thread_with_subscription(
                        conversation_id,
                        connection_id,
                        Some(thread_subscription_id.clone()),
                    )
                    .await,
            )
        }
    };
    let connection_added = match connection_added {
        Ok(connection_added) => connection_added,
        Err(is_closing) => {
            outgoing
                .unregister_thread_subscription_if_matches(
                    connection_id,
                    conversation_id,
                    &thread_subscription_id,
                )
                .await;
            if is_closing {
                if pending_thread_resume_is_current_or_reply(
                    thread_state_manager,
                    outgoing,
                    conversation_id,
                    &request_id,
                    &reservation_id,
                )
                .await
                {
                    outgoing
                        .send_error(
                            request_id,
                            invalid_request(format!(
                                "thread {conversation_id} is closing; retry thread/resume \
                                 after the thread is closed"
                            )),
                        )
                        .await;
                    thread_state_manager
                        .clear_pending_thread_resume_if_matches(
                            conversation_id,
                            connection_id,
                            &reservation_id,
                        )
                        .await;
                }
            } else {
                let _ = pending_thread_resume_is_current_or_reply(
                    thread_state_manager,
                    outgoing,
                    conversation_id,
                    &request_id,
                    &reservation_id,
                )
                .await;
            }
            return;
        }
    };
    let Some(connection_added) = connection_added else {
        outgoing
            .unregister_thread_subscription_if_matches(
                connection_id,
                conversation_id,
                &thread_subscription_id,
            )
            .await;
        tracing::debug!(
            thread_id = %conversation_id,
            connection_id = ?connection_id,
            "skipping running thread resume for closed connection"
        );
        if pending_thread_resume_is_current_or_reply(
            thread_state_manager,
            outgoing,
            conversation_id,
            &request_id,
            &reservation_id,
        )
        .await
        {
            thread_state_manager
                .clear_pending_thread_resume_if_matches(
                    conversation_id,
                    connection_id,
                    &reservation_id,
                )
                .await;
        }
        return;
    };
    let predecessor_subscription_id = connection_added.predecessor_subscription_id;

    if !pending_thread_resume_is_current_or_reply(
        thread_state_manager,
        outgoing,
        conversation_id,
        &request_id,
        &reservation_id,
    )
    .await
    {
        rollback_failed_thread_attach_with_predecessor(
            thread_state_manager,
            outgoing,
            conversation_id,
            connection_id,
            &thread_subscription_id,
            predecessor_subscription_id.as_deref(),
        )
        .await;
        return;
    }

    let (turns_backwards_cursor, items_backwards_cursor) = if let Some(thread_store) =
        pending.resume_cursor_store.as_ref()
    {
        match super::thread_processor::ThreadRequestProcessor::paginated_resume_backwards_cursors(
            thread_store.as_ref(),
            conversation_id,
        )
        .await
        {
            Ok(cursors) => cursors,
            Err(error) => {
                rollback_failed_thread_attach_with_predecessor(
                    thread_state_manager,
                    outgoing,
                    conversation_id,
                    connection_id,
                    &thread_subscription_id,
                    predecessor_subscription_id.as_deref(),
                )
                .await;
                if pending_thread_resume_is_current_or_reply(
                    thread_state_manager,
                    outgoing,
                    conversation_id,
                    &request_id,
                    &reservation_id,
                )
                .await
                {
                    outgoing.send_error(request_id, error).await;
                    thread_state_manager
                        .clear_pending_thread_resume_if_matches(
                            conversation_id,
                            connection_id,
                            &reservation_id,
                        )
                        .await;
                }
                return;
            }
        }
    } else {
        (None, None)
    };

    // Cursor construction can hydrate from storage. Honor an unsubscribe that raced with that
    // work before composing any receipt, lifecycle snapshot, or replay traffic.
    if !pending_thread_resume_is_current_or_reply(
        thread_state_manager,
        outgoing,
        conversation_id,
        &request_id,
        &reservation_id,
    )
    .await
    {
        rollback_failed_thread_attach_with_predecessor(
            thread_state_manager,
            outgoing,
            conversation_id,
            connection_id,
            &thread_subscription_id,
            predecessor_subscription_id.as_deref(),
        )
        .await;
        return;
    }

    let config_snapshot = pending.config_snapshot;
    let sandbox = config_snapshot.sandbox_policy().into();
    let cwd = config_snapshot.cwd().clone();
    let ThreadConfigSnapshot {
        model,
        model_provider_id,
        service_tier,
        approval_policy,
        approvals_reviewer,
        active_permission_profile,
        workspace_roots,
        reasoning_effort,
        originator,
        ..
    } = config_snapshot;
    let instruction_sources = pending.instruction_sources;
    let active_permission_profile =
        thread_response_active_permission_profile(active_permission_profile);
    let session_id = conversation.session_configured().session_id.to_string();
    thread.session_id = session_id;
    let response = ThreadResumeResponse {
        thread,
        thread_subscription_id: Some(thread_subscription_id.clone()),
        model,
        model_provider: model_provider_id,
        service_tier,
        cwd,
        runtime_workspace_roots: workspace_roots,
        instruction_sources,
        approval_policy: approval_policy.into(),
        approvals_reviewer: approvals_reviewer.into(),
        sandbox,
        active_permission_profile,
        reasoning_effort,
        multi_agent_mode: MultiAgentMode::ExplicitRequestOnly,
        initial_turns_page,
        turns_backwards_cursor,
        items_backwards_cursor,
    };
    // This is the logical commit point for a deferred running-thread resume. Once it wins, the
    // success response and every captured follow-up belong to this lifecycle; a later
    // unsubscribe or replacement owns its own work and must not make us roll back this response.
    if !commit_pending_thread_resume_or_reply(
        thread_state_manager,
        outgoing,
        conversation_id,
        &request_id,
        &reservation_id,
    )
    .await
    {
        rollback_failed_thread_attach_with_predecessor(
            thread_state_manager,
            outgoing,
            conversation_id,
            connection_id,
            &thread_subscription_id,
            predecessor_subscription_id.as_deref(),
        )
        .await;
        return;
    }
    if !outgoing
        .thread_subscription_matches(connection_id, conversation_id, &thread_subscription_id)
        .await
    {
        rollback_failed_thread_attach_with_predecessor(
            thread_state_manager,
            outgoing,
            conversation_id,
            connection_id,
            &thread_subscription_id,
            predecessor_subscription_id.as_deref(),
        )
        .await;
        outgoing
            .send_error(
                request_id,
                invalid_request(
                    "thread/resume subscription was superseded before publication completed",
                ),
            )
            .await;
        return;
    }
    outgoing
        .send_response_with_thread_originator(request_id, response, originator)
        .await;
    // Keep post-response notifications in the listener state machine. It publishes them
    // immediately in the usual case, or places them after a preceding targetless-warning
    // barrier without withholding the response that established this subscription.
    *post_commit_command = Some(ThreadListenerCommand::CompleteThreadResume {
            thread_subscription,
            token_usage_turn_id,
            emit_thread_goal_update: pending.emit_thread_goal_update,
            thread_goal_state_db: pending.thread_goal_state_db,
        });
}

/// Emits only the captured post-response effects of a committed running-thread resume. This is
/// deliberately a distinct listener command so a prior targetless-warning barrier can release it
/// after the warning while response/attachment liveness remains immediate.
async fn complete_committed_thread_resume(
    conversation_id: ThreadId,
    conversation: &Arc<CodexThread>,
    outgoing: &Arc<OutgoingMessageSender>,
    thread_subscription: ThreadSubscriptionTarget,
    token_usage_turn_id: Option<String>,
    emit_thread_goal_update: bool,
    thread_goal_state_db: Option<StateDbHandle>,
) {
    // Match cold resume: metadata-only resume should attach the listener without paying the cost
    // of turn reconstruction for historical usage replay.
    if let Some(token_usage_turn_id) = token_usage_turn_id {
        send_thread_token_usage_update_to_subscription(
            outgoing,
            &thread_subscription,
            conversation_id,
            conversation.as_ref(),
            token_usage_turn_id,
        )
        .await;
    }
    if emit_thread_goal_update {
        if let Some(state_db) = thread_goal_state_db {
            send_thread_goal_snapshot_notification(
                outgoing,
                std::slice::from_ref(&thread_subscription),
                conversation_id,
                &state_db,
            )
            .await;
        } else {
            tracing::warn!(
                thread_id = %conversation_id,
                "state db unavailable when reading thread goal for running thread resume"
            );
        }
    }
    outgoing
        .replay_requests_to_thread_subscription(&thread_subscription)
        .await;
    // App-server owns resume response and snapshot ordering, so wait until replay completes
    // before letting extensions react to the idle thread.
    if emit_thread_goal_update {
        conversation.emit_thread_idle_lifecycle_if_idle().await;
    }
}

/// Deferred running-thread resume has already returned `Handled` to the request processor. If
/// listener ordering later loses its reservation, send the original request one terminal error
/// instead of silently abandoning it. The winning replacement retains its distinct reservation.
async fn pending_thread_resume_is_current_or_reply(
    thread_state_manager: &ThreadStateManager,
    outgoing: &Arc<OutgoingMessageSender>,
    thread_id: ThreadId,
    request_id: &ConnectionRequestId,
    reservation_id: &RequestId,
) -> bool {
    match thread_state_manager
        .pending_thread_resume_reservation_state(
            thread_id,
            request_id.connection_id,
            reservation_id,
        )
        .await
    {
        crate::thread_state::PendingThreadResumeReservationState::Current => true,
        reservation_state => {
            reject_lost_pending_thread_resume(
                outgoing,
                thread_id,
                request_id,
                reservation_state,
            )
            .await;
            false
        }
    }
}

/// Atomically removes the provisional reservation immediately before the normal success
/// response. This seals the successful lifecycle so post-response cleanup cannot retract it;
/// cancellation or replacement that wins first still receives the original terminal error.
async fn commit_pending_thread_resume_or_reply(
    thread_state_manager: &ThreadStateManager,
    outgoing: &Arc<OutgoingMessageSender>,
    thread_id: ThreadId,
    request_id: &ConnectionRequestId,
    reservation_id: &RequestId,
) -> bool {
    match thread_state_manager
        .commit_pending_thread_resume(thread_id, request_id.connection_id, reservation_id)
        .await
    {
        crate::thread_state::PendingThreadResumeReservationState::Current => true,
        reservation_state => {
            reject_lost_pending_thread_resume(outgoing, thread_id, request_id, reservation_state)
                .await;
            false
        }
    }
}

async fn reject_lost_pending_thread_resume(
    outgoing: &Arc<OutgoingMessageSender>,
    thread_id: ThreadId,
    request_id: &ConnectionRequestId,
    reservation_state: crate::thread_state::PendingThreadResumeReservationState,
) {
    let message = match reservation_state {
        crate::thread_state::PendingThreadResumeReservationState::Canceled => {
            tracing::debug!(
                thread_id = %thread_id,
                request_id = ?request_id,
                "rejecting canceled running thread resume before listener effects"
            );
            format!("thread {thread_id} resume was canceled before it could complete")
        }
        crate::thread_state::PendingThreadResumeReservationState::Superseded => {
            tracing::debug!(
                thread_id = %thread_id,
                request_id = ?request_id,
                "rejecting superseded running thread resume before listener effects"
            );
            format!("thread {thread_id} resume was superseded by a newer thread/resume request")
        }
        crate::thread_state::PendingThreadResumeReservationState::Current => {
            unreachable!("a current reservation must not be rejected")
        }
    };
    outgoing
        .send_error(request_id.clone(), invalid_request(message))
        .await;
}

pub(super) async fn send_thread_goal_snapshot_notification(
    outgoing: &Arc<OutgoingMessageSender>,
    thread_subscriptions: &[ThreadSubscriptionTarget],
    thread_id: ThreadId,
    state_db: &StateDbHandle,
) {
    match state_db.thread_goals().get_thread_goal(thread_id).await {
        Ok(Some(goal)) => {
            send_captured_thread_goal_notification(
                outgoing,
                thread_subscriptions,
                ServerNotification::ThreadGoalUpdated(ThreadGoalUpdatedNotification {
                    thread_id: thread_id.to_string(),
                    turn_id: None,
                    goal: api_thread_goal_from_state(goal),
                }),
            )
            .await;
        }
        Ok(None) => {
            send_captured_thread_goal_notification(
                outgoing,
                thread_subscriptions,
                ServerNotification::ThreadGoalCleared(ThreadGoalClearedNotification {
                    thread_id: thread_id.to_string(),
                }),
            )
            .await;
        }
        Err(err) => {
            tracing::warn!(
                thread_id = %thread_id,
                "failed to read thread goal for resume snapshot: {err}"
            );
        }
    }
}

/// Sends a goal notification through the identities captured at the command or
/// snapshot ingress. Goal state reads and listener FIFO delivery can both
/// yield, so this is the single path that prevents a delayed event from
/// acquiring a replacement subscription token.
pub(super) async fn send_captured_thread_goal_notification(
    outgoing: &Arc<OutgoingMessageSender>,
    thread_subscriptions: &[ThreadSubscriptionTarget],
    notification: ServerNotification,
) {
    outgoing
        .send_server_notification_to_thread_subscriptions(thread_subscriptions, notification)
        .await;
}

pub(crate) fn populate_thread_turns_from_history(
    thread: &mut Thread,
    items: &[RolloutItem],
    active_turn: Option<&Turn>,
) {
    let mut turns = build_legacy_api_turns_from_rollout_items(items);
    if let Some(active_turn) = active_turn {
        merge_turn_history_with_active_turn(&mut turns, active_turn.clone());
    }
    thread.turns = turns;
}

pub(super) async fn resolve_pending_server_request(
    conversation_id: ThreadId,
    outgoing: &Arc<OutgoingMessageSender>,
    request_id: RequestId,
) {
    let thread_id = conversation_id.to_string();
    let Some(thread_subscriptions) = outgoing
        .take_thread_request_resolution_targets(&request_id)
        .await
    else {
        tracing::debug!(
            ?request_id,
            %conversation_id,
            "dropping resolution without captured thread request targets"
        );
        return;
    };
    outgoing
        .send_server_notification_to_thread_subscriptions(
            &thread_subscriptions,
            ServerNotification::ServerRequestResolved(ServerRequestResolvedNotification {
                thread_id,
                request_id,
            }),
        )
        .await;
}

pub(super) fn merge_turn_history_with_active_turn(turns: &mut Vec<Turn>, active_turn: Turn) {
    turns.retain(|turn| turn.id != active_turn.id);
    turns.push(active_turn);
}

fn merge_active_turn_into_page(
    page: &mut codex_app_server_protocol::TurnsPage,
    mut active_turn: Turn,
    params: &codex_app_server_protocol::ThreadResumeInitialTurnsPageParams,
) {
    super::thread_processor::apply_thread_turns_items_view(
        std::slice::from_mut(&mut active_turn),
        params.items_view.unwrap_or(TurnItemsView::Summary),
    );
    let sort_direction = params.sort_direction.unwrap_or(SortDirection::Desc);
    let page_size = super::thread_processor::thread_turns_page_size(params.limit);
    let active_turn_is_in_page = page.data.iter().any(|turn| turn.id == active_turn.id);
    page.data.retain(|turn| turn.id != active_turn.id);
    match sort_direction {
        SortDirection::Asc
            if active_turn_is_in_page
                || (page.data.len() < page_size && page.next_cursor.is_none()) =>
        {
            page.data.push(active_turn);
        }
        SortDirection::Asc => {}
        SortDirection::Desc => page.data.insert(0, active_turn),
    }
}

pub(super) fn set_thread_status_and_interrupt_stale_turns(
    thread: &mut Thread,
    loaded_status: ThreadStatus,
    has_live_in_progress_turn: bool,
) {
    let status = resolve_thread_status(loaded_status, has_live_in_progress_turn);
    if !matches!(status, ThreadStatus::Active { .. }) {
        for turn in &mut thread.turns {
            if matches!(turn.status, TurnStatus::InProgress) {
                turn.status = TurnStatus::Interrupted;
            }
        }
    }
    thread.status = status;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outgoing_message::ConnectionRequestId;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use crate::thread_state::ConnectionCapabilities;
    use codex_app_server_protocol::RequestId;
    use codex_app_server_protocol::ThreadGoal;
    use codex_app_server_protocol::ThreadGoalStatus;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    fn goal(thread_id: ThreadId, objective: &str) -> ThreadGoal {
        ThreadGoal {
            thread_id: thread_id.to_string(),
            objective: objective.to_string(),
            status: ThreadGoalStatus::Active,
            token_budget: None,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn canceled_queued_resume_receives_one_terminal_error_before_listener_effects() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let request_id = RequestId::Integer(51);
        let request = ConnectionRequestId {
            connection_id,
            request_id: request_id.clone(),
        };
        let thread_state_manager = ThreadStateManager::new();
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        thread_state_manager
            .reserve_pending_thread_resume(thread_id, connection_id, request_id.clone())
            .await;
        assert!(
            thread_state_manager
                .cancel_pending_thread_resume(thread_id, connection_id)
                .await
        );

        assert!(
            !pending_thread_resume_is_current_or_reply(
                &thread_state_manager,
                &outgoing,
                thread_id,
                &request,
                &request_id,
            )
            .await
        );
        let OutgoingEnvelope::ToConnection { message, .. } = timeout(
            Duration::from_millis(100),
            outgoing_rx.recv(),
        )
        .await
        .expect("canceled deferred resume must promptly terminate")
        .expect("canceled deferred resume must send an error")
        else {
            panic!("expected a terminal resume error");
        };
        let OutgoingMessage::Error(error) = message else {
            panic!("canceled resume must not publish a resume receipt or replay");
        };
        assert_eq!(error.id, request_id);
        assert!(error.error.message.contains("canceled"));
        assert!(
            timeout(Duration::from_millis(10), outgoing_rx.recv())
                .await
                .is_err(),
            "one canceled deferred resume must not emit a duplicate terminal response"
        );
        assert!(!thread_state_manager.has_subscribers(thread_id).await);
    }

    #[tokio::test]
    async fn superseded_queued_resume_receives_one_error_without_touching_replacement() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let old_request_id = RequestId::Integer(52);
        let replacement_request_id = RequestId::Integer(53);
        let old_request = ConnectionRequestId {
            connection_id,
            request_id: old_request_id.clone(),
        };
        let thread_state_manager = ThreadStateManager::new();
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        thread_state_manager
            .reserve_pending_thread_resume(thread_id, connection_id, old_request_id.clone())
            .await;
        thread_state_manager
            .reserve_pending_thread_resume(
                thread_id,
                connection_id,
                replacement_request_id.clone(),
            )
            .await;

        assert!(
            !pending_thread_resume_is_current_or_reply(
                &thread_state_manager,
                &outgoing,
                thread_id,
                &old_request,
                &old_request_id,
            )
            .await
        );
        let OutgoingEnvelope::ToConnection { message, .. } = timeout(
            Duration::from_millis(100),
            outgoing_rx.recv(),
        )
        .await
        .expect("superseded deferred resume must promptly terminate")
        .expect("superseded deferred resume must send an error")
        else {
            panic!("expected a terminal resume error");
        };
        let OutgoingMessage::Error(error) = message else {
            panic!("superseded resume must not publish a late receipt or replay");
        };
        assert_eq!(error.id, old_request_id);
        assert!(error.error.message.contains("superseded"));
        assert!(
            thread_state_manager
                .pending_thread_resume_matches(
                    thread_id,
                    connection_id,
                    &replacement_request_id,
                )
                .await,
            "an old deferred handler must not clear or reply for the replacement"
        );
        assert!(
            timeout(Duration::from_millis(10), outgoing_rx.recv())
                .await
                .is_err(),
            "the old handler must send exactly one terminal outcome"
        );
    }

    #[tokio::test]
    async fn committed_running_resume_preserves_its_success_lifecycle_across_later_overlap() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let committed_request_id = RequestId::Integer(54);
        let replacement_request_id = RequestId::Integer(55);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        thread_state_manager
            .reserve_pending_thread_resume(
                thread_id,
                connection_id,
                committed_request_id.clone(),
            )
            .await;
        let committed_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(committed_subscription_id.clone()),
            )
            .await
            .expect("the committed lifecycle should attach before responding");

        assert_eq!(
            thread_state_manager
                .commit_pending_thread_resume(thread_id, connection_id, &committed_request_id)
                .await,
            crate::thread_state::PendingThreadResumeReservationState::Current,
            "the final pre-response check must atomically become the success commit"
        );

        // A newer resume and an unsubscribe-like cancellation happen while the old success is
        // leaving the transport. They own their later lifecycle, not cleanup of the committed
        // response's A token.
        thread_state_manager
            .reserve_pending_thread_resume(
                thread_id,
                connection_id,
                replacement_request_id.clone(),
            )
            .await;
        let replacement_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(replacement_subscription_id.clone()),
            )
            .await
            .expect("the replacement lifecycle should own its attachment");
        assert!(
            thread_state_manager
                .cancel_pending_thread_resume(thread_id, connection_id)
                .await,
            "the later cancellation should affect only the later provisional reservation"
        );

        rollback_failed_thread_attach(
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &committed_subscription_id,
        )
        .await;
        assert!(
            outgoing
                .thread_subscription_matches(
                    connection_id,
                    thread_id,
                    &replacement_subscription_id,
                )
                .await,
            "any owned provisional cleanup must not retract the replacement attachment"
        );
        assert!(
            thread_state_manager.has_subscribers(thread_id).await,
            "a committed response must not be made to point at a torn-down attachment"
        );
    }

    #[tokio::test]
    async fn failed_post_attach_hydration_removes_subscription_and_fences_traffic() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(thread_subscription_id.clone()),
            )
            .await
            .expect("test connection should attach");

        rollback_failed_thread_attach(
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &thread_subscription_id,
        )
        .await;

        assert!(!thread_state_manager.has_subscribers(thread_id).await);
        assert!(
            outgoing
                .thread_subscription_target_for_connection(connection_id, thread_id)
                .await
                .is_none()
        );
        let captured_after_rollback = outgoing
            .thread_subscription_targets_for_thread(thread_id)
            .await;
        assert!(captured_after_rollback.is_empty());
        send_captured_thread_goal_notification(
            &outgoing,
            &captured_after_rollback,
            ServerNotification::ThreadGoalCleared(ThreadGoalClearedNotification {
                thread_id: thread_id.to_string(),
            }),
        )
        .await;
        assert!(
            outgoing_rx.try_recv().is_err(),
            "a failed attach must not leave a token that can emit tagged traffic"
        );
    }

    #[tokio::test]
    async fn failed_provisional_resume_restores_the_still_active_subscription_identity() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        let active_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(active_subscription_id.clone()),
            )
            .await
            .expect("the original subscription should remain active");

        // A provisional resume mints B in the outgoing map before it completes, but does not
        // replace the live A state-manager attachment. Its later failure must put A back.
        let provisional_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        assert_ne!(provisional_subscription_id, active_subscription_id);
        rollback_failed_thread_attach(
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &provisional_subscription_id,
        )
        .await;

        assert!(
            outgoing
                .thread_subscription_matches(connection_id, thread_id, &active_subscription_id)
                .await,
            "rollback must restore the token that still owns the active state-manager attachment"
        );
        assert_eq!(
            thread_state_manager
                .connection_thread_subscription_id(thread_id, connection_id)
                .await,
            Some(active_subscription_id),
            "rollback must not tear down the active A attachment while cleaning provisional B"
        );
    }

    #[tokio::test]
    async fn failed_provisional_resume_never_overwrites_a_competing_replacement() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        let active_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(active_subscription_id.clone()),
            )
            .await
            .expect("the original subscription should remain active");
        let failed_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let competing_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;

        rollback_failed_thread_attach(
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &failed_subscription_id,
        )
        .await;
        assert!(
            outgoing
                .thread_subscription_matches(
                    connection_id,
                    thread_id,
                    &competing_subscription_id,
                )
                .await,
            "a failed B rollback must not overwrite a newer outgoing C owner"
        );

        rollback_failed_thread_attach(
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &competing_subscription_id,
        )
        .await;
        assert!(
            outgoing
                .thread_subscription_matches(connection_id, thread_id, &active_subscription_id)
                .await,
            "once C fails too, the transaction may restore active A"
        );
    }

    #[tokio::test]
    async fn failed_running_resume_attach_restores_its_captured_predecessor_for_later_events() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        let active_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_add_connection_to_thread_with_subscription(
                thread_id,
                connection_id,
                Some(active_subscription_id.clone()),
            )
            .await
            .expect("A should establish the active running-thread attachment");

        let provisional_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let provisional_attachment = thread_state_manager
            .try_add_connection_to_thread_with_subscription(
                thread_id,
                connection_id,
                Some(provisional_subscription_id.clone()),
            )
            .await
            .expect("the real running-resume attach should install B");
        assert_eq!(
            provisional_attachment.predecessor_subscription_id.as_deref(),
            Some(active_subscription_id.as_str()),
            "B must capture A before it overwrites the state-manager token"
        );

        rollback_failed_thread_attach_with_predecessor(
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &provisional_subscription_id,
            provisional_attachment.predecessor_subscription_id.as_deref(),
        )
        .await;
        assert_eq!(
            thread_state_manager
                .connection_thread_subscription_id(thread_id, connection_id)
                .await,
            Some(active_subscription_id.clone()),
            "failed B hydration must restore active A in the real state-manager path"
        );
        let active_target = outgoing
            .thread_subscription_target_for_connection(connection_id, thread_id)
            .await
            .expect("failed B rollback must restore A in the outgoing map");
        send_captured_thread_goal_notification(
            &outgoing,
            &[active_target],
            ServerNotification::ThreadGoalCleared(ThreadGoalClearedNotification {
                thread_id: thread_id.to_string(),
            }),
        )
        .await;
        let OutgoingEnvelope::ToConnection {
            message: OutgoingMessage::ThreadScopedNotification(notification),
            ..
        } = outgoing_rx.recv().await.expect("later event should reach restored A")
        else {
            panic!("expected tagged event through restored A");
        };
        assert_eq!(notification.thread_subscription_id, active_subscription_id);
    }

    #[tokio::test]
    async fn failed_running_resume_attach_cannot_restore_a_over_concurrent_c() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        let active_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_add_connection_to_thread_with_subscription(
                thread_id,
                connection_id,
                Some(active_subscription_id.clone()),
            )
            .await
            .expect("A should attach");
        let failed_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let failed_attachment = thread_state_manager
            .try_add_connection_to_thread_with_subscription(
                thread_id,
                connection_id,
                Some(failed_subscription_id.clone()),
            )
            .await
            .expect("B should attach provisionally");
        let replacement_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_add_connection_to_thread_with_subscription(
                thread_id,
                connection_id,
                Some(replacement_subscription_id.clone()),
            )
            .await
            .expect("C should supersede B before B failure cleanup");

        rollback_failed_thread_attach_with_predecessor(
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &failed_subscription_id,
            failed_attachment.predecessor_subscription_id.as_deref(),
        )
        .await;
        assert_eq!(
            thread_state_manager
                .connection_thread_subscription_id(thread_id, connection_id)
                .await,
            Some(replacement_subscription_id.clone()),
            "B rollback must not resurrect A after C became current"
        );
        assert!(
            outgoing
                .thread_subscription_matches(
                    connection_id,
                    thread_id,
                    &replacement_subscription_id,
                )
                .await,
            "B rollback must not overwrite C in the outgoing map"
        );
    }

    #[tokio::test]
    async fn failed_old_attach_cannot_unregister_a_replacement_subscription() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        let old_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(old_subscription_id.clone()),
            )
            .await
            .expect("old attach should establish its owned connection state");

        // A second overlapping attach replaces the token and its connection ownership before the
        // first attempt learns that its later hydration failed.
        let replacement_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(replacement_subscription_id.clone()),
            )
            .await
            .expect("replacement attach should supersede the old ownership");

        rollback_failed_thread_attach(
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &old_subscription_id,
        )
        .await;

        assert_eq!(
            outgoing
                .thread_subscription_target_for_connection(connection_id, thread_id)
                .await
                .map(|target| target.thread_subscription_id),
            Some(replacement_subscription_id),
            "old cleanup must not unregister the replacement token"
        );
        assert!(
            thread_state_manager.has_subscribers(thread_id).await,
            "old cleanup must not unsubscribe the replacement listener"
        );
    }

    #[tokio::test]
    async fn unsubscribe_cancels_queued_running_resume_before_connection_registration() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        let reservation_id = RequestId::Integer(41);

        thread_state_manager
            .reserve_pending_thread_resume(thread_id, connection_id, reservation_id.clone())
            .await;
        assert!(
            thread_state_manager
                .cancel_pending_thread_resume(thread_id, connection_id)
                .await,
            "unsubscribe must observe a listener-queued resume without a visible subscriber"
        );
        assert!(
            !thread_state_manager
                .pending_thread_resume_matches(thread_id, connection_id, &reservation_id)
                .await,
            "the queued handler must see its reservation was canceled before minting a token"
        );
        assert!(!thread_state_manager.has_subscribers(thread_id).await);
    }

    #[tokio::test]
    async fn unsubscribe_cancels_hydrating_resume_without_tearing_down_replacement() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let old_reservation_id = RequestId::Integer(42);
        thread_state_manager
            .reserve_pending_thread_resume(
                thread_id,
                connection_id,
                old_reservation_id.clone(),
            )
            .await;
        let old_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(old_subscription_id.clone()),
            )
            .await
            .expect("hydrating resume should attach before its cursor read");

        assert!(
            thread_state_manager
                .cancel_pending_thread_resume(thread_id, connection_id)
                .await,
            "unsubscribe must cancel an in-progress hydration reservation"
        );
        rollback_failed_thread_attach(
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &old_subscription_id,
        )
        .await;
        assert!(!thread_state_manager.has_subscribers(thread_id).await);

        let replacement_reservation_id = RequestId::Integer(43);
        thread_state_manager
            .reserve_pending_thread_resume(
                thread_id,
                connection_id,
                replacement_reservation_id.clone(),
            )
            .await;
        let replacement_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(replacement_subscription_id.clone()),
            )
            .await
            .expect("replacement resume should attach");

        // The old cancellation cleanup runs after a replacement has taken ownership. It must
        // match the old token rather than removing the current listener or reservation.
        rollback_failed_thread_attach(
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &old_subscription_id,
        )
        .await;
        assert!(
            thread_state_manager
                .pending_thread_resume_matches(
                    thread_id,
                    connection_id,
                    &replacement_reservation_id,
                )
                .await
        );
        assert_eq!(
            outgoing
                .thread_subscription_target_for_connection(connection_id, thread_id)
                .await
                .map(|target| target.thread_subscription_id),
            Some(replacement_subscription_id)
        );
        assert!(thread_state_manager.has_subscribers(thread_id).await);
    }

    #[tokio::test]
    async fn thread_start_connection_closed_attach_suppresses_receipt_and_started_event() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(thread_subscription_id.clone()),
            )
            .await
            .expect("test connection should begin attached");

        assert_eq!(
            gate_thread_start_listener_attachment(
                Ok(EnsureConversationListenerResult::ConnectionClosed),
                &thread_state_manager,
                &outgoing,
                thread_id,
                connection_id,
                &thread_subscription_id,
            )
            .await
            .expect("a closed connection is not an RPC failure"),
            ThreadStartAttachmentPublication::Suppress
        );
        assert!(!thread_state_manager.has_subscribers(thread_id).await);
        assert!(
            outgoing
                .thread_subscription_target_for_connection(connection_id, thread_id)
                .await
                .is_none(),
            "a suppressed start must not return a dangling subscription receipt"
        );
        assert!(
            outgoing_rx.try_recv().is_err(),
            "a suppressed start must return before publishing ThreadStarted"
        );
    }

    #[tokio::test]
    async fn thread_start_attach_error_rolls_back_before_receipt_or_started_event() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed_with_subscription(
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
                Some(thread_subscription_id.clone()),
            )
            .await
            .expect("test connection should begin attached");

        let error = gate_thread_start_listener_attachment(
            Err(invalid_request("listener attach failed")),
            &thread_state_manager,
            &outgoing,
            thread_id,
            connection_id,
            &thread_subscription_id,
        )
        .await
        .expect_err("listener attach failure must abort thread/start publication");

        assert_eq!(error.message, "listener attach failed");
        assert!(!thread_state_manager.has_subscribers(thread_id).await);
        assert!(
            outgoing
                .thread_subscription_target_for_connection(connection_id, thread_id)
                .await
                .is_none(),
            "a failed start must not expose a dangling subscription receipt"
        );
        assert!(
            outgoing_rx.try_recv().is_err(),
            "a failed start must return before publishing ThreadStarted"
        );
    }

    #[tokio::test]
    async fn captured_goal_update_clear_and_snapshot_fence_replaced_subscriptions() {
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        let old_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let old_targets = outgoing
            .thread_subscription_targets_for_thread(thread_id)
            .await;
        let current_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let current_targets = outgoing
            .thread_subscription_targets_for_thread(thread_id)
            .await;

        send_captured_thread_goal_notification(
            &outgoing,
            &old_targets,
            ServerNotification::ThreadGoalUpdated(ThreadGoalUpdatedNotification {
                thread_id: thread_id.to_string(),
                turn_id: None,
                goal: goal(thread_id, "ordered-update"),
            }),
        )
        .await;
        send_captured_thread_goal_notification(
            &outgoing,
            &old_targets,
            ServerNotification::ThreadGoalCleared(ThreadGoalClearedNotification {
                thread_id: thread_id.to_string(),
            }),
        )
        .await;
        send_captured_thread_goal_notification(
            &outgoing,
            &old_targets,
            ServerNotification::ThreadGoalUpdated(ThreadGoalUpdatedNotification {
                thread_id: thread_id.to_string(),
                turn_id: None,
                goal: goal(thread_id, "resume-snapshot"),
            }),
        )
        .await;
        send_captured_thread_goal_notification(
            &outgoing,
            &current_targets,
            ServerNotification::ThreadGoalUpdated(ThreadGoalUpdatedNotification {
                thread_id: thread_id.to_string(),
                turn_id: None,
                goal: goal(thread_id, "current-update"),
            }),
        )
        .await;

        let mut delivered = Vec::new();
        for _ in 0..4 {
            let OutgoingEnvelope::ToConnection {
                connection_id: delivered_connection_id,
                message: OutgoingMessage::ThreadScopedNotification(notification),
                ..
            } = outgoing_rx
                .recv()
                .await
                .expect("goal notification should enqueue")
            else {
                panic!("expected captured thread-scoped goal notification");
            };
            assert_eq!(delivered_connection_id, connection_id);
            let name = match notification.envelope.notification {
                ServerNotification::ThreadGoalUpdated(notification) => notification.goal.objective,
                ServerNotification::ThreadGoalCleared(_) => "cleared".to_string(),
                notification => panic!("expected goal notification, got {notification:?}"),
            };
            delivered.push((notification.thread_subscription_id, name));
        }

        assert_eq!(
            delivered,
            vec![
                (old_subscription_id.clone(), "ordered-update".to_string()),
                (old_subscription_id.clone(), "cleared".to_string()),
                (old_subscription_id, "resume-snapshot".to_string()),
                (current_subscription_id, "current-update".to_string()),
            ]
        );
    }
}
