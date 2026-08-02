use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::ThreadSubscriptionTarget;
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadGoal;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadSettings;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnError;
use codex_core::CodexThread;
use codex_core::ThreadConfigSnapshot;
use codex_file_watcher::WatchRegistration;
use codex_protocol::ThreadId;
#[cfg(test)]
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::items::AgentMessageContent as CoreAgentMessageContent;
use codex_protocol::items::TurnItem as CoreTurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_rollout::state_db::StateDbHandle;
use codex_utils_path_uri::LegacyAppPathString;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tracing::error;

type PendingInterruptQueue = Vec<ConnectionRequestId>;

/// Late canonical item completions may be ordered after their enclosing turn terminal event.
/// Keep only a finite set of reducers with unmatched starts so that those completions can still
/// merge their requested provenance into the effective completion item.
const TERMINAL_TURN_HISTORY_LIMIT: usize = 256;

struct RetainedTerminalTurnHistory {
    history: ThreadHistoryBuilder,
    pending_item_ids: HashSet<String>,
}

pub(crate) struct PendingThreadResumeRequest {
    pub(crate) request_id: ConnectionRequestId,
    /// Immutable request identity reserved before this command enters the listener FIFO.
    /// `thread/unsubscribe` removes the matching reservation even before the handler has made a
    /// connection visible, and a replacement resume supersedes an older queued command.
    pub(crate) reservation_id: RequestId,
    pub(crate) history_items: Vec<RolloutItem>,
    pub(crate) config_snapshot: ThreadConfigSnapshot,
    pub(crate) instruction_sources: Vec<LegacyAppPathString>,
    pub(crate) thread_summary: codex_app_server_protocol::Thread,
    pub(crate) emit_thread_goal_update: bool,
    pub(crate) thread_goal_state_db: Option<StateDbHandle>,
    pub(crate) include_turns: bool,
    pub(crate) initial_turns_page:
        Option<codex_app_server_protocol::ThreadResumeInitialTurnsPageParams>,
    pub(crate) paginated_turns: Option<Vec<Turn>>,
    pub(crate) paginated_initial_turns_page: Option<codex_app_server_protocol::TurnsPage>,
    pub(crate) paginated_initial_turns_page_with_active_slot:
        Option<codex_app_server_protocol::TurnsPage>,
    pub(crate) resume_cursor_store: Option<Arc<dyn codex_thread_store::ThreadStore>>,
    pub(crate) redact_resume_payloads: bool,
}

/// Ownership state for a deferred running-thread resume.
///
/// A handler that no longer owns its reservation still owes the client a terminal RPC outcome:
/// an absent reservation means it was canceled, while a different request id means a newer
/// resume superseded it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingThreadResumeReservationState {
    Current,
    Canceled,
    Superseded,
}

/// Immutable ownership of one bounded targetless-warning wait.
///
/// A warning accepted by a listener must not survive that listener being replaced or cleared.
/// The generation is absent only for the no-listener fallback path; registering a listener still
/// invalidates that fallback lease before it can deliver through the new lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TargetlessWarningWaitLease {
    wait_id: u64,
    listener_generation: Option<u64>,
}

impl TargetlessWarningWaitLease {
    pub(crate) fn listener_generation(self) -> Option<u64> {
        self.listener_generation
    }
}

/// Delivery ownership for an extension warning accepted by a thread listener.
///
/// A warning with existing recipients must retain those exact identities across listener FIFO
/// delay. Conversely, a warning accepted while no thread subscription exists is current work
/// with no stale recipient to preserve, so delivery may wait for the first subscriber.
pub(crate) enum ThreadWarningDelivery {
    Captured(Vec<ThreadSubscriptionTarget>),
    AwaitCurrentSubscriber(TargetlessWarningWaitLease),
}

// ThreadListenerCommand is used to perform operations in the context of the thread listener, for serialization purposes.
pub(crate) enum ThreadListenerCommand {
    // SendThreadResumeResponse is used to resume an already running thread by sending the thread's history to the client and atomically subscribing for new updates.
    SendThreadResumeResponse(Box<PendingThreadResumeRequest>),
    // CompleteThreadResume sends captured lifecycle notifications only after the successful
    // response established its subscription. Keeping it as a listener command lets a preceding
    // targetless-warning barrier preserve notification order without delaying the response.
    CompleteThreadResume {
        thread_subscription: ThreadSubscriptionTarget,
        token_usage_turn_id: Option<String>,
        emit_thread_goal_update: bool,
        thread_goal_state_db: Option<StateDbHandle>,
    },
    // EmitThreadGoalUpdated is used to order goal updates with running-thread resume responses and goal clears.
    EmitThreadGoalUpdated {
        turn_id: Option<String>,
        goal: ThreadGoal,
        thread_subscriptions: Vec<ThreadSubscriptionTarget>,
    },
    // EmitWarning is used to order extension warnings with other thread notifications.
    EmitWarning {
        message: String,
        delivery: ThreadWarningDelivery,
    },
    // EmitThreadGoalCleared is used to order app-server goal clears with running-thread resume responses.
    EmitThreadGoalCleared {
        thread_subscriptions: Vec<ThreadSubscriptionTarget>,
    },
    // EmitThreadGoalSnapshot is used to read and emit the latest goal state in the listener order.
    EmitThreadGoalSnapshot {
        state_db: StateDbHandle,
        thread_subscriptions: Vec<ThreadSubscriptionTarget>,
    },
    // ResolveServerRequest is used to notify the client that the request has been resolved.
    // It is executed in the thread listener's context to ensure that the resolved notification is ordered with regard to the request itself.
    ResolveServerRequest {
        request_id: RequestId,
        completion_tx: oneshot::Sender<()>,
    },
}

/// Per-conversation accumulation of the latest states e.g. error message while a turn runs.
#[derive(Default, Clone)]
pub(crate) struct TurnSummary {
    pub(crate) started_at: Option<i64>,
    pub(crate) command_execution_started: HashSet<String>,
    pub(crate) last_error: Option<TurnError>,
    pub(crate) last_agent_message: Option<ThreadItem>,
}

#[derive(Default)]
pub(crate) struct ThreadState {
    pub(crate) pending_interrupts: PendingInterruptQueue,
    pub(crate) pending_rollbacks: Option<ConnectionRequestId>,
    pub(crate) turn_summary: TurnSummary,
    pub(crate) last_terminal_turn_id: Option<String>,
    pub(crate) cancel_tx: Option<oneshot::Sender<()>>,
    pub(crate) experimental_raw_events: bool,
    pub(crate) listener_generation: u64,
    last_thread_settings: Option<ThreadSettings>,
    listener_command_tx: Option<mpsc::UnboundedSender<ThreadListenerCommand>>,
    current_turn_history: ThreadHistoryBuilder,
    current_turn_started_item_ids: HashMap<String, HashSet<String>>,
    terminal_turn_histories: HashMap<String, RetainedTerminalTurnHistory>,
    terminal_turn_history_order: VecDeque<String>,
    terminal_turn_item_snapshots: HashMap<(String, String), ThreadItem>,
    terminal_turn_item_snapshot_order: VecDeque<(String, String)>,
    listener_thread: Option<Weak<CodexThread>>,
    watch_registration: WatchRegistration,
}

impl ThreadState {
    pub(crate) fn listener_matches(&self, conversation: &Arc<CodexThread>) -> bool {
        self.listener_thread
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|existing| Arc::ptr_eq(&existing, conversation))
    }

    pub(crate) fn set_listener(
        &mut self,
        cancel_tx: oneshot::Sender<()>,
        conversation: &Arc<CodexThread>,
        watch_registration: WatchRegistration,
        thread_settings_baseline: ThreadSettings,
    ) -> (mpsc::UnboundedReceiver<ThreadListenerCommand>, u64) {
        if let Some(previous) = self.cancel_tx.replace(cancel_tx) {
            let _ = previous.send(());
        }
        self.listener_generation = self.listener_generation.wrapping_add(1);
        self.last_thread_settings = Some(thread_settings_baseline);
        let (listener_command_tx, listener_command_rx) = mpsc::unbounded_channel();
        self.listener_command_tx = Some(listener_command_tx);
        self.listener_thread = Some(Arc::downgrade(conversation));
        self.watch_registration = watch_registration;
        (listener_command_rx, self.listener_generation)
    }

    pub(crate) fn clear_listener(&mut self) {
        if let Some(cancel_tx) = self.cancel_tx.take() {
            let _ = cancel_tx.send(());
        }
        self.listener_command_tx = None;
        self.current_turn_history.reset();
        self.current_turn_started_item_ids.clear();
        self.terminal_turn_histories.clear();
        self.terminal_turn_history_order.clear();
        self.terminal_turn_item_snapshots.clear();
        self.terminal_turn_item_snapshot_order.clear();
        self.listener_thread = None;
        self.watch_registration = WatchRegistration::default();
    }

    pub(crate) fn set_experimental_raw_events(&mut self, enabled: bool) {
        self.experimental_raw_events = enabled;
    }

    pub(crate) fn listener_command_tx(
        &self,
    ) -> Option<mpsc::UnboundedSender<ThreadListenerCommand>> {
        self.listener_command_tx.clone()
    }

    pub(crate) fn active_turn_snapshot(&self) -> Option<Turn> {
        self.current_turn_history.active_turn_snapshot()
    }

    /// Returns a canonical lifecycle item's materialized snapshot after it has
    /// passed through the per-thread history reducer.
    pub(crate) fn turn_item_snapshot(&self, turn_id: &str, item_id: &str) -> Option<ThreadItem> {
        self.terminal_turn_item_snapshots
            .get(&(turn_id.to_string(), item_id.to_string()))
            .cloned()
            .or_else(|| {
                self.terminal_turn_histories
                    .get(turn_id)
                    .and_then(|retained| retained.history.turn_item_snapshot(turn_id, item_id))
            })
            .or_else(|| {
                self.current_turn_history
                    .turn_item_snapshot(turn_id, item_id)
            })
    }

    pub(crate) fn track_current_turn_event(&mut self, event_turn_id: &str, event: &EventMsg) {
        if let EventMsg::TurnStarted(payload) = event {
            self.turn_summary.started_at = payload.started_at;
        }
        if let EventMsg::ItemCompleted(payload) = event
            && let CoreTurnItem::AgentMessage(item) = &payload.item
            && matches!(item.phase, Some(MessagePhase::FinalAnswer) | None)
            && item.content.iter().any(|content| {
                matches!(content, CoreAgentMessageContent::Text { text } if !text.trim().is_empty())
            })
        {
            self.turn_summary.last_agent_message =
                Some(ThreadItem::from(CoreTurnItem::AgentMessage(item.clone())));
        }

        if let EventMsg::ItemStarted(payload) = event {
            self.current_turn_started_item_ids
                .entry(payload.turn_id.clone())
                .or_default()
                .insert(payload.item.id());
        }

        if let EventMsg::ItemCompleted(payload) = event {
            let item_id = payload.item.id();
            if let Some(retained) = self.terminal_turn_histories.get_mut(&payload.turn_id) {
                let (snapshot, finished) = {
                    retained.history.handle_event(event);
                    let snapshot = retained
                        .history
                        .turn_item_snapshot(&payload.turn_id, &item_id);
                    retained.pending_item_ids.remove(&item_id);
                    (snapshot, retained.pending_item_ids.is_empty())
                };
                if let Some(snapshot) = snapshot {
                    self.insert_terminal_turn_item_snapshot(
                        payload.turn_id.clone(),
                        item_id,
                        snapshot,
                    );
                }
                if finished {
                    self.remove_terminal_turn_history(&payload.turn_id);
                }
                return;
            }

            self.current_turn_started_item_ids
                .get_mut(&payload.turn_id)
                .map(|item_ids| item_ids.remove(&item_id));
            if self.last_terminal_turn_id.as_deref() == Some(payload.turn_id.as_str()) {
                // This completion has no retained canonical start, so forwarding its native
                // payload is correct. Do not reopen a finished reducer just to materialize it.
                return;
            }
        }

        self.current_turn_history.handle_event(event);
        let terminal_turn_id = match event {
            EventMsg::TurnComplete(payload) => Some(payload.turn_id.as_str()),
            EventMsg::TurnAborted(payload) => payload.turn_id.as_deref().or(Some(event_turn_id)),
            _ => None,
        };
        if let Some(terminal_turn_id) = terminal_turn_id {
            self.last_terminal_turn_id = Some(terminal_turn_id.to_string());
            let pending_item_ids = self
                .current_turn_started_item_ids
                .remove(terminal_turn_id)
                .unwrap_or_default();
            if pending_item_ids.is_empty() {
                self.current_turn_history.reset();
            } else {
                let history = std::mem::take(&mut self.current_turn_history);
                self.insert_terminal_turn_history(
                    terminal_turn_id.to_string(),
                    history,
                    pending_item_ids,
                );
            }
        }
    }

    fn insert_terminal_turn_history(
        &mut self,
        turn_id: String,
        history: ThreadHistoryBuilder,
        pending_item_ids: HashSet<String>,
    ) {
        self.remove_terminal_turn_history(&turn_id);
        self.terminal_turn_histories.insert(
            turn_id.clone(),
            RetainedTerminalTurnHistory {
                history,
                pending_item_ids,
            },
        );
        self.terminal_turn_history_order.push_back(turn_id);
        while self.terminal_turn_history_order.len() > TERMINAL_TURN_HISTORY_LIMIT {
            if let Some(expired_turn_id) = self.terminal_turn_history_order.pop_front() {
                self.terminal_turn_histories.remove(&expired_turn_id);
            }
        }
    }

    fn remove_terminal_turn_history(&mut self, turn_id: &str) {
        self.terminal_turn_histories.remove(turn_id);
        self.terminal_turn_history_order
            .retain(|existing_turn_id| existing_turn_id != turn_id);
    }

    fn insert_terminal_turn_item_snapshot(
        &mut self,
        turn_id: String,
        item_id: String,
        item: ThreadItem,
    ) {
        let key = (turn_id, item_id);
        self.terminal_turn_item_snapshots.remove(&key);
        self.terminal_turn_item_snapshot_order
            .retain(|existing_key| existing_key != &key);
        self.terminal_turn_item_snapshots.insert(key.clone(), item);
        self.terminal_turn_item_snapshot_order.push_back(key);
        while self.terminal_turn_item_snapshot_order.len() > TERMINAL_TURN_HISTORY_LIMIT {
            if let Some(expired_key) = self.terminal_turn_item_snapshot_order.pop_front() {
                self.terminal_turn_item_snapshots.remove(&expired_key);
            }
        }
    }

    pub(crate) fn note_thread_settings(&mut self, thread_settings: ThreadSettings) -> bool {
        let changed = self.last_thread_settings.as_ref() != Some(&thread_settings);
        self.last_thread_settings = Some(thread_settings);
        changed
    }
}

pub(crate) async fn resolve_server_request_on_thread_listener(
    thread_state: &Arc<Mutex<ThreadState>>,
    outgoing: &ThreadScopedOutgoingMessageSender,
    request_id: RequestId,
) {
    let (completion_tx, completion_rx) = oneshot::channel();
    let listener_command_tx = {
        let state = thread_state.lock().await;
        state.listener_command_tx()
    };
    let Some(listener_command_tx) = listener_command_tx else {
        outgoing
            .discard_thread_request_resolution_targets(&request_id)
            .await;
        error!("failed to remove pending client request: thread listener is not running");
        return;
    };

    let cleanup_request_id = request_id.clone();
    if listener_command_tx
        .send(ThreadListenerCommand::ResolveServerRequest {
            request_id,
            completion_tx,
        })
        .is_err()
    {
        outgoing
            .discard_thread_request_resolution_targets(&cleanup_request_id)
            .await;
        error!(
            "failed to remove pending client request: thread listener command channel is closed"
        );
        return;
    }

    if let Err(err) = completion_rx.await {
        outgoing
            .discard_thread_request_resolution_targets(&cleanup_request_id)
            .await;
        error!("failed to remove pending client request: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ApprovalsReviewer;
    use codex_app_server_protocol::AskForApproval;
    use codex_app_server_protocol::CurrentTimeReadParams;
    use codex_app_server_protocol::SandboxPolicy;
    use codex_app_server_protocol::ServerRequestPayload;
    use codex_protocol::config_types::CollaborationMode;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::config_types::Settings;
    use codex_protocol::items::CollabAgentTool as CoreCollabAgentTool;
    use codex_protocol::items::CollabAgentToolCallItem as CoreCollabAgentToolCallItem;
    use codex_protocol::items::CollabAgentToolCallStatus as CoreCollabAgentToolCallStatus;
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::ItemStartedEvent;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::TurnAbortedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    #[test]
    fn note_thread_settings_reports_only_effective_changes() {
        let mut state = ThreadState::default();
        let initial = thread_settings("mock-model");
        let updated = thread_settings("mock-model-2");

        let results = vec![
            state.note_thread_settings(initial.clone()),
            state.note_thread_settings(initial),
            state.note_thread_settings(updated.clone()),
            state.note_thread_settings(updated),
        ];

        assert_eq!(results, vec![true, false, true, false]);
    }

    #[tokio::test]
    async fn targetless_warning_wait_is_invalidated_on_restart_failure_and_teardown() {
        let thread_state_manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();

        let first_wait_id = thread_state_manager
            .try_begin_targetless_warning_wait(thread_id, Some(1))
            .expect("the first targetless warning should own the bounded wait");
        assert!(
            thread_state_manager
                .try_begin_targetless_warning_wait(thread_id, Some(1))
                .is_none(),
            "additional warnings must not create unbounded concurrent waits"
        );
        assert!(
            thread_state_manager.targetless_warning_wait_is_current(thread_id, first_wait_id)
        );

        let (replacement_listener_tx, _replacement_listener_rx) = mpsc::unbounded_channel();
        thread_state_manager.register_listener_command_tx(thread_id, 2, replacement_listener_tx);
        assert!(
            !thread_state_manager.targetless_warning_wait_is_current(thread_id, first_wait_id),
            "a replacement listener must fence an old waiter before it can deliver"
        );

        let replacement_wait_id = thread_state_manager
            .try_begin_targetless_warning_wait(thread_id, Some(2))
            .expect("the replacement listener should be able to claim a fresh bounded wait");
        assert!(
            !thread_state_manager.unregister_listener_command_tx_if_generation(thread_id, 1),
            "an old listener must not unregister its replacement"
        );
        assert!(
            thread_state_manager.targetless_warning_wait_is_current(thread_id, replacement_wait_id),
            "an old listener cleanup must not invalidate its replacement's warning lease"
        );

        thread_state_manager.unregister_listener_command_tx(thread_id);
        assert!(
            !thread_state_manager
                .targetless_warning_wait_is_current(thread_id, replacement_wait_id),
            "a listener failure/clear must fence its waiter before a later lifecycle can deliver"
        );

        let (teardown_listener_tx, _teardown_listener_rx) = mpsc::unbounded_channel();
        thread_state_manager.register_listener_command_tx(thread_id, 3, teardown_listener_tx);
        let teardown_wait_id = thread_state_manager
            .try_begin_targetless_warning_wait(thread_id, Some(3))
            .expect("the later listener should be able to claim a fresh bounded wait");

        thread_state_manager.remove_thread_state(thread_id).await;
        assert!(
            !thread_state_manager
                .targetless_warning_wait_is_current(thread_id, teardown_wait_id),
            "a teardown must fence a waiter before it can deliver to a later lifecycle"
        );
    }

    #[tokio::test]
    async fn missing_or_closed_listener_discards_thread_request_resolution_targets() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(crate::outgoing_message::OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let scoped_outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            thread_id,
        );
        let thread_state = Arc::new(Mutex::new(ThreadState::default()));

        let (missing_listener_request_id, _missing_listener_waiter) = scoped_outgoing
            .send_request(ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                thread_id: thread_id.to_string(),
            }))
            .await;
        let _ = outgoing_rx
            .recv()
            .await
            .expect("request should be queued before missing-listener cleanup");
        resolve_server_request_on_thread_listener(
            &thread_state,
            &scoped_outgoing,
            missing_listener_request_id.clone(),
        )
        .await;
        assert!(
            outgoing
                .take_thread_request_resolution_targets(&missing_listener_request_id)
                .await
                .is_none(),
            "a missing listener must not retain a successful request's resolution recipients"
        );

        let (closed_listener_tx, closed_listener_rx) = mpsc::unbounded_channel();
        drop(closed_listener_rx);
        thread_state.lock().await.listener_command_tx = Some(closed_listener_tx);
        let (closed_listener_request_id, _closed_listener_waiter) = scoped_outgoing
            .send_request(ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                thread_id: thread_id.to_string(),
            }))
            .await;
        let _ = outgoing_rx
            .recv()
            .await
            .expect("request should be queued before closed-listener cleanup");
        resolve_server_request_on_thread_listener(
            &thread_state,
            &scoped_outgoing,
            closed_listener_request_id.clone(),
        )
        .await;
        assert!(
            outgoing
                .take_thread_request_resolution_targets(&closed_listener_request_id)
                .await
                .is_none(),
            "a closed listener command channel must discard retained recipients exactly once"
        );
    }

    #[test]
    fn terminal_turn_retains_spawn_start_for_late_completion() {
        for terminal_event in [
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                started_at: None,
                last_agent_message: None,
                compaction_events_in_turn: 0,
                final_model: None,
                model_snapshot: None,
                provider_usage: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("turn-1".to_string()),
                started_at: None,
                reason: TurnAbortReason::Interrupted,
                provider_usage: None,
                completed_at: None,
                duration_ms: None,
            }),
        ] {
            let mut state = ThreadState::default();
            let thread_id = ThreadId::new();
            state.track_current_turn_event(
                "turn-1",
                &EventMsg::ItemStarted(ItemStartedEvent {
                    thread_id,
                    turn_id: "turn-1".to_string(),
                    item: canonical_spawn_item(
                        "spawn-1",
                        CoreCollabAgentToolCallStatus::InProgress,
                        /*model*/ None,
                        Some("gpt-requested"),
                        Some(ReasoningEffort::High),
                    ),
                    started_at_ms: 0,
                }),
            );
            let terminal_event_id = if matches!(&terminal_event, EventMsg::TurnAborted(_)) {
                "abort-envelope-id"
            } else {
                "turn-1"
            };
            state.track_current_turn_event(terminal_event_id, &terminal_event);
            state.track_current_turn_event(
                "turn-1",
                &EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id,
                    turn_id: "turn-1".to_string(),
                    item: canonical_spawn_item(
                        "spawn-1",
                        CoreCollabAgentToolCallStatus::Completed,
                        Some("gpt-effective"),
                        /*requested_model*/ None,
                        /*requested_reasoning_effort*/ None,
                    ),
                    completed_at_ms: 1,
                }),
            );

            let Some(ThreadItem::CollabAgentToolCall {
                status,
                model,
                reasoning_effort,
                requested_model,
                requested_reasoning_effort,
                ..
            }) = state.turn_item_snapshot("turn-1", "spawn-1")
            else {
                panic!("late spawn completion should have a materialized item");
            };
            assert_eq!(
                status,
                codex_app_server_protocol::CollabAgentToolCallStatus::Completed
            );
            assert_eq!(model.as_deref(), Some("gpt-effective"));
            assert_eq!(reasoning_effort, Some(ReasoningEffort::Low));
            assert_eq!(requested_model.as_deref(), Some("gpt-requested"));
            assert_eq!(requested_reasoning_effort, Some(ReasoningEffort::High));
        }
    }

    #[test]
    fn terminal_turn_history_retention_is_bounded() {
        let mut state = ThreadState::default();
        for index in 0..=TERMINAL_TURN_HISTORY_LIMIT {
            state.insert_terminal_turn_history(
                format!("turn-{index}"),
                ThreadHistoryBuilder::new(),
                HashSet::from([format!("item-{index}")]),
            );
        }

        assert_eq!(
            state.terminal_turn_histories.len(),
            TERMINAL_TURN_HISTORY_LIMIT
        );
        assert!(!state.terminal_turn_histories.contains_key("turn-0"));
        assert!(
            state
                .terminal_turn_histories
                .contains_key(&format!("turn-{TERMINAL_TURN_HISTORY_LIMIT}"))
        );
    }

    fn canonical_spawn_item(
        id: &str,
        status: CoreCollabAgentToolCallStatus,
        model: Option<&str>,
        requested_model: Option<&str>,
        requested_reasoning_effort: Option<ReasoningEffort>,
    ) -> CoreTurnItem {
        CoreTurnItem::CollabAgentToolCall(CoreCollabAgentToolCallItem {
            id: id.to_string(),
            tool: CoreCollabAgentTool::SpawnAgent,
            status,
            sender_thread_id: ThreadId::new(),
            receiver_thread_ids: Vec::new(),
            receiver_agents: Vec::new(),
            prompt: Some("inspect the repository".to_string()),
            model: model.map(str::to_string),
            reasoning_effort: match status {
                CoreCollabAgentToolCallStatus::InProgress => None,
                CoreCollabAgentToolCallStatus::Completed
                | CoreCollabAgentToolCallStatus::Failed => Some(ReasoningEffort::Low),
            },
            requested_model: requested_model.map(str::to_string),
            requested_reasoning_effort,
            agents_states: HashMap::new(),
        })
    }

    fn thread_settings(model: &str) -> ThreadSettings {
        ThreadSettings {
            cwd: AbsolutePathBuf::from_absolute_path("/tmp").expect("absolute path"),
            approval_policy: AskForApproval::OnRequest,
            approvals_reviewer: ApprovalsReviewer::User,
            sandbox_policy: SandboxPolicy::ReadOnly {
                network_access: false,
            },
            active_permission_profile: None,
            model: model.to_string(),
            model_provider: "mock_provider".to_string(),
            service_tier: None,
            effort: None,
            summary: None,
            collaboration_mode: CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: model.to_string(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            },
            multi_agent_mode: MultiAgentMode::ExplicitRequestOnly,
            personality: None,
        }
    }
}

struct ThreadEntry {
    state: Arc<Mutex<ThreadState>>,
    connection_ids: HashSet<ConnectionId>,
    /// Identity that established each connection's current listener attachment. Explicit
    /// start/resume/fork cleanup must match this before removing a shared connection.
    connection_subscription_ids: HashMap<ConnectionId, String>,
    has_connections_watcher: watch::Sender<bool>,
}

impl Default for ThreadEntry {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(ThreadState::default())),
            connection_ids: HashSet::new(),
            connection_subscription_ids: HashMap::new(),
            has_connections_watcher: watch::channel(false).0,
        }
    }
}

impl ThreadEntry {
    fn update_has_connections(&self) {
        let _ = self.has_connections_watcher.send_if_modified(|current| {
            let prev = *current;
            *current = !self.connection_ids.is_empty();
            prev != *current
        });
    }
}

#[derive(Default)]
struct ThreadStateManagerInner {
    live_connections: HashMap<ConnectionId, ConnectionCapabilities>,
    threads: HashMap<ThreadId, ThreadEntry>,
    thread_ids_by_connection: HashMap<ConnectionId, HashSet<ThreadId>>,
    pending_thread_resume_reservations: HashMap<(ThreadId, ConnectionId), RequestId>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ConnectionCapabilities {
    pub(crate) request_attestation: bool,
}

#[derive(Clone, Default)]
pub(crate) struct ThreadStateManager {
    state: Arc<Mutex<ThreadStateManagerInner>>,
    // Extension event sinks are synchronous, so they need an await-free way to
    // enqueue work on the active per-thread listener.
    listener_commands: Arc<StdMutex<HashMap<ThreadId, ListenerCommandRegistration>>>,
    // Targetless warnings wait outside the listener loop. Keep at most one bounded waiter per
    // thread and give it a lease so teardown or a later waiter cannot be mistaken for the old
    // lifecycle when its subscriber watch wakes.
    targetless_warning_waits: Arc<StdMutex<HashMap<ThreadId, TargetlessWarningWaitLease>>>,
    next_targetless_warning_wait_id: Arc<AtomicU64>,
}

struct ListenerCommandRegistration {
    generation: u64,
    tx: mpsc::UnboundedSender<ThreadListenerCommand>,
}

impl ThreadStateManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn connection_initialized(
        &self,
        connection_id: ConnectionId,
        capabilities: ConnectionCapabilities,
    ) {
        self.state
            .lock()
            .await
            .live_connections
            .insert(connection_id, capabilities);
    }

    /// Reserves a queued running-thread resume before it reaches listener execution. A newer
    /// resume on the same connection/thread intentionally replaces the old reservation, making
    /// the older command a no-op when it eventually runs.
    pub(crate) async fn reserve_pending_thread_resume(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        request_id: RequestId,
    ) {
        self.state
            .lock()
            .await
            .pending_thread_resume_reservations
            .insert((thread_id, connection_id), request_id);
    }

    /// Returns whether this running-thread resume still owns its reservation.
    pub(crate) async fn pending_thread_resume_matches(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        request_id: &RequestId,
    ) -> bool {
        self.state
            .lock()
            .await
            .pending_thread_resume_reservations
            .get(&(thread_id, connection_id))
            .is_some_and(|current_request_id| current_request_id == request_id)
    }

    /// Classifies a deferred resume's ownership without treating an older command as the owner
    /// of a newer replacement. Listener handlers use this to send one terminal response for
    /// every request that was accepted for deferred delivery but later lost its reservation.
    pub(crate) async fn pending_thread_resume_reservation_state(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        request_id: &RequestId,
    ) -> PendingThreadResumeReservationState {
        let state = self.state.lock().await;
        match state
            .pending_thread_resume_reservations
            .get(&(thread_id, connection_id))
        {
            Some(current_request_id) if current_request_id == request_id => {
                PendingThreadResumeReservationState::Current
            }
            Some(_) => PendingThreadResumeReservationState::Superseded,
            None => PendingThreadResumeReservationState::Canceled,
        }
    }

    /// Atomically commits a deferred running-thread resume before its success response is sent.
    ///
    /// `Current` means this caller removed its own provisional reservation and now owns a stable
    /// published lifecycle. A later unsubscribe or replacement is a new lifecycle operation; it
    /// cannot make this handler treat an already committed success as a failed attach.
    pub(crate) async fn commit_pending_thread_resume(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        request_id: &RequestId,
    ) -> PendingThreadResumeReservationState {
        let mut state = self.state.lock().await;
        match state
            .pending_thread_resume_reservations
            .get(&(thread_id, connection_id))
        {
            Some(current_request_id) if current_request_id == request_id => {
                state
                    .pending_thread_resume_reservations
                    .remove(&(thread_id, connection_id));
                PendingThreadResumeReservationState::Current
            }
            Some(_) => PendingThreadResumeReservationState::Superseded,
            None => PendingThreadResumeReservationState::Canceled,
        }
    }

    /// Cancels any queued or in-progress resume for this connection/thread. This deliberately
    /// does not require a visible subscriber: `thread/unsubscribe` is authoritative while a
    /// resume is still in the listener FIFO or hydrating its response.
    pub(crate) async fn cancel_pending_thread_resume(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
    ) -> bool {
        self.state
            .lock()
            .await
            .pending_thread_resume_reservations
            .remove(&(thread_id, connection_id))
            .is_some()
    }

    /// Finishes a resume only if it still owns the reservation, preserving a replacement that
    /// was queued while the old command was completing.
    pub(crate) async fn clear_pending_thread_resume_if_matches(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        request_id: &RequestId,
    ) -> bool {
        let mut state = self.state.lock().await;
        if state
            .pending_thread_resume_reservations
            .get(&(thread_id, connection_id))
            .is_some_and(|current_request_id| current_request_id == request_id)
        {
            state
                .pending_thread_resume_reservations
                .remove(&(thread_id, connection_id));
            true
        } else {
            false
        }
    }

    pub(crate) async fn first_attestation_capable_connection_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Option<ConnectionId> {
        let state = self.state.lock().await;
        state
            .threads
            .get(&thread_id)?
            .connection_ids
            .iter()
            .filter_map(|connection_id| {
                state
                    .live_connections
                    .get(connection_id)?
                    .request_attestation
                    .then_some(*connection_id)
            })
            .min_by_key(|connection_id| connection_id.0)
    }

    pub(crate) async fn wait_for_thread_subscriber(&self, thread_id: ThreadId) {
        let mut has_connections = {
            let mut state = self.state.lock().await;
            state
                .threads
                .entry(thread_id)
                .or_default()
                .has_connections_watcher
                .subscribe()
        };
        while !*has_connections.borrow_and_update() {
            if has_connections.changed().await.is_err() {
                break;
            }
        }
    }

    pub(crate) async fn subscribed_connection_ids(&self, thread_id: ThreadId) -> Vec<ConnectionId> {
        let state = self.state.lock().await;
        state
            .threads
            .get(&thread_id)
            .map(|thread_entry| thread_entry.connection_ids.iter().copied().collect())
            .unwrap_or_default()
    }

    pub(crate) async fn thread_state(&self, thread_id: ThreadId) -> Arc<Mutex<ThreadState>> {
        let mut state = self.state.lock().await;
        state.threads.entry(thread_id).or_default().state.clone()
    }

    pub(crate) fn current_listener_command_tx(
        &self,
        thread_id: ThreadId,
    ) -> Option<mpsc::UnboundedSender<ThreadListenerCommand>> {
        self.listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .map(|registration| registration.tx.clone())
    }

    /// Captures the sender and listener generation together so synchronous extension ingress can
    /// claim a targetless-warning lease before it queues any listener command.
    pub(crate) fn current_listener_command_tx_with_generation(
        &self,
        thread_id: ThreadId,
    ) -> Option<(u64, mpsc::UnboundedSender<ThreadListenerCommand>)> {
        self.listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .map(|registration| (registration.generation, registration.tx.clone()))
    }

    pub(crate) fn register_listener_command_tx(
        &self,
        thread_id: ThreadId,
        listener_generation: u64,
        tx: mpsc::UnboundedSender<ThreadListenerCommand>,
    ) {
        self.cancel_targetless_warning_wait(thread_id);
        self.listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                thread_id,
                ListenerCommandRegistration {
                    generation: listener_generation,
                    tx,
                },
            );
        tracing::debug!(
            %thread_id,
            listener_generation,
            "registered thread listener command sender and invalidated prior warning wait"
        );
    }

    pub(crate) fn unregister_listener_command_tx(&self, thread_id: ThreadId) {
        self.listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&thread_id);
        self.cancel_targetless_warning_wait(thread_id);
    }

    /// Drops a listener sender only when the exiting task still owns the registered generation.
    /// A stale task must not remove a replacement's command path or invalidate its warning lease.
    pub(crate) fn unregister_listener_command_tx_if_generation(
        &self,
        thread_id: ThreadId,
        listener_generation: u64,
    ) -> bool {
        let removed = {
            let mut listener_commands = self
                .listener_commands
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if listener_commands
                .get(&thread_id)
                .is_some_and(|registration| registration.generation == listener_generation)
            {
                listener_commands.remove(&thread_id);
                true
            } else {
                false
            }
        };
        if removed {
            self.cancel_targetless_warning_wait(thread_id);
        }
        removed
    }

    /// Claims the single bounded targetless-warning wait for a thread. Additional warnings while
    /// one is waiting are coalesced rather than creating an unbounded task backlog. Listener
    /// callers record their generation so a restart or clear fences their old detached waiter.
    pub(crate) fn try_begin_targetless_warning_wait(
        &self,
        thread_id: ThreadId,
        listener_generation: Option<u64>,
    ) -> Option<TargetlessWarningWaitLease> {
        let mut waits = self
            .targetless_warning_waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if waits.contains_key(&thread_id) {
            return None;
        }
        let lease = TargetlessWarningWaitLease {
            wait_id: self
                .next_targetless_warning_wait_id
                .fetch_add(1, Ordering::Relaxed),
            listener_generation,
        };
        waits.insert(thread_id, lease);
        Some(lease)
    }

    /// Returns whether this bounded waiter still belongs to the active thread lifecycle and
    /// listener generation.
    pub(crate) fn targetless_warning_wait_is_current(
        &self,
        thread_id: ThreadId,
        lease: TargetlessWarningWaitLease,
    ) -> bool {
        self.targetless_warning_waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .is_some_and(|current_lease| {
                current_lease.wait_id == lease.wait_id
                    && current_lease.listener_generation == lease.listener_generation
            })
    }

    /// Releases one targetless-warning waiter without disturbing a newer lease.
    pub(crate) fn finish_targetless_warning_wait(
        &self,
        thread_id: ThreadId,
        lease: TargetlessWarningWaitLease,
    ) {
        let mut waits = self
            .targetless_warning_waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if waits
            .get(&thread_id)
            .is_some_and(|current_lease| {
                current_lease.wait_id == lease.wait_id
                    && current_lease.listener_generation == lease.listener_generation
            })
        {
            waits.remove(&thread_id);
        }
    }

    /// Fences an older listener's detached warning waiter before the replacement publishes its
    /// command sender. This closes the window where the replacement has taken the live listener
    /// slot but an old waiter could still decide to deliver its warning.
    pub(crate) fn invalidate_targetless_warning_wait_before_listener_generation(
        &self,
        thread_id: ThreadId,
        next_listener_generation: u64,
    ) {
        let mut waits = self
            .targetless_warning_waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if waits.get(&thread_id).is_some_and(|lease| {
            lease
                .listener_generation
                .is_some_and(|listener_generation| listener_generation < next_listener_generation)
        }) {
            waits.remove(&thread_id);
        }
    }

    fn clear_targetless_warning_waits(&self) {
        self.targetless_warning_waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn cancel_targetless_warning_wait(&self, thread_id: ThreadId) {
        self.targetless_warning_waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&thread_id);
    }

    pub(crate) async fn remove_thread_state(&self, thread_id: ThreadId) {
        let thread_state = {
            let mut state = self.state.lock().await;
            let thread_state = state
                .threads
                .remove(&thread_id)
                .map(|thread_entry| thread_entry.state);
            state.thread_ids_by_connection.retain(|_, thread_ids| {
                thread_ids.remove(&thread_id);
                !thread_ids.is_empty()
            });
            state
                .pending_thread_resume_reservations
                .retain(|(candidate_thread_id, _), _| *candidate_thread_id != thread_id);
            thread_state
        };
        self.unregister_listener_command_tx(thread_id);
        self.cancel_targetless_warning_wait(thread_id);

        if let Some(thread_state) = thread_state {
            let mut thread_state = thread_state.lock().await;
            tracing::debug!(
                thread_id = %thread_id,
                listener_generation = thread_state.listener_generation,
                had_listener = thread_state.cancel_tx.is_some(),
                had_active_turn = thread_state.active_turn_snapshot().is_some(),
                "clearing thread listener during thread-state teardown"
            );
            thread_state.clear_listener();
        }
    }

    pub(crate) async fn clear_all_listeners(&self) {
        self.clear_targetless_warning_waits();
        let thread_states = {
            let state = self.state.lock().await;
            state
                .threads
                .iter()
                .map(|(thread_id, thread_entry)| (*thread_id, thread_entry.state.clone()))
                .collect::<Vec<_>>()
        };

        for (thread_id, thread_state) in thread_states {
            self.unregister_listener_command_tx(thread_id);
            let mut thread_state = thread_state.lock().await;
            tracing::debug!(
                thread_id = %thread_id,
                listener_generation = thread_state.listener_generation,
                had_listener = thread_state.cancel_tx.is_some(),
                had_active_turn = thread_state.active_turn_snapshot().is_some(),
                "clearing thread listener during app-server shutdown"
            );
            thread_state.clear_listener();
        }
    }

    pub(crate) async fn unsubscribe_connection_from_thread(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
    ) -> bool {
        {
            let mut state = self.state.lock().await;
            if !state.threads.contains_key(&thread_id) {
                return false;
            }

            if !state
                .thread_ids_by_connection
                .get(&connection_id)
                .is_some_and(|thread_ids| thread_ids.contains(&thread_id))
            {
                return false;
            }

            if let Some(thread_ids) = state.thread_ids_by_connection.get_mut(&connection_id) {
                thread_ids.remove(&thread_id);
                if thread_ids.is_empty() {
                    state.thread_ids_by_connection.remove(&connection_id);
                }
            }
            if let Some(thread_entry) = state.threads.get_mut(&thread_id) {
                thread_entry.connection_ids.remove(&connection_id);
                thread_entry.connection_subscription_ids.remove(&connection_id);
                thread_entry.update_has_connections();
            }
        };

        true
    }

    /// Removes an explicit attachment only when its immutable subscription identity still owns
    /// this connection/thread pair. A newer overlapping attach replaces this id before it becomes
    /// live, so an older rollback cannot tear down that newer listener.
    pub(crate) async fn unsubscribe_connection_from_thread_if_subscription_matches(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        expected_subscription_id: &str,
    ) -> bool {
        let mut state = self.state.lock().await;
        let Some(thread_entry) = state.threads.get(&thread_id) else {
            return false;
        };
        if thread_entry
            .connection_subscription_ids
            .get(&connection_id)
            .is_none_or(|subscription_id| subscription_id != expected_subscription_id)
        {
            return false;
        }

        if let Some(thread_ids) = state.thread_ids_by_connection.get_mut(&connection_id) {
            thread_ids.remove(&thread_id);
            if thread_ids.is_empty() {
                state.thread_ids_by_connection.remove(&connection_id);
            }
        }
        if let Some(thread_entry) = state.threads.get_mut(&thread_id) {
            thread_entry.connection_ids.remove(&connection_id);
            thread_entry.connection_subscription_ids.remove(&connection_id);
            thread_entry.update_has_connections();
        }
        true
    }

    #[cfg(test)]
    pub(crate) async fn has_subscribers(&self, thread_id: ThreadId) -> bool {
        self.state
            .lock()
            .await
            .threads
            .get(&thread_id)
            .is_some_and(|thread_entry| !thread_entry.connection_ids.is_empty())
    }

    pub(crate) async fn try_ensure_connection_subscribed(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        experimental_raw_events: bool,
    ) -> Option<Arc<Mutex<ThreadState>>> {
        self.try_ensure_connection_subscribed_with_subscription(
            thread_id,
            connection_id,
            experimental_raw_events,
            None,
        )
        .await
    }

    /// Adds a listener connection while recording the token that owns an explicit attachment.
    /// Background callers without an explicit token pass `None` and retain the legacy semantics.
    pub(crate) async fn try_ensure_connection_subscribed_with_subscription(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        experimental_raw_events: bool,
        subscription_id: Option<String>,
    ) -> Option<Arc<Mutex<ThreadState>>> {
        let thread_state = {
            let mut state = self.state.lock().await;
            if !state.live_connections.contains_key(&connection_id) {
                return None;
            }
            state
                .thread_ids_by_connection
                .entry(connection_id)
                .or_default()
                .insert(thread_id);
            let thread_entry = state.threads.entry(thread_id).or_default();
            thread_entry.connection_ids.insert(connection_id);
            match subscription_id {
                Some(subscription_id) => {
                    thread_entry
                        .connection_subscription_ids
                        .insert(connection_id, subscription_id);
                }
                None => {
                    thread_entry.connection_subscription_ids.remove(&connection_id);
                }
            }
            thread_entry.update_has_connections();
            thread_entry.state.clone()
        };
        {
            let mut thread_state_guard = thread_state.lock().await;
            if experimental_raw_events {
                thread_state_guard.set_experimental_raw_events(/*enabled*/ true);
            }
        }
        Some(thread_state)
    }

    pub(crate) async fn try_add_connection_to_thread(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
    ) -> bool {
        self.try_add_connection_to_thread_with_subscription(thread_id, connection_id, None)
            .await
    }

    /// Adds a connection to an existing listener and records explicit attachment ownership.
    pub(crate) async fn try_add_connection_to_thread_with_subscription(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        subscription_id: Option<String>,
    ) -> bool {
        let mut state = self.state.lock().await;
        if !state.live_connections.contains_key(&connection_id) {
            return false;
        }
        state
            .thread_ids_by_connection
            .entry(connection_id)
            .or_default()
            .insert(thread_id);
        let thread_entry = state.threads.entry(thread_id).or_default();
        thread_entry.connection_ids.insert(connection_id);
        match subscription_id {
            Some(subscription_id) => {
                thread_entry
                    .connection_subscription_ids
                    .insert(connection_id, subscription_id);
            }
            None => {
                thread_entry.connection_subscription_ids.remove(&connection_id);
            }
        }
        thread_entry.update_has_connections();
        true
    }

    pub(crate) async fn remove_connection(&self, connection_id: ConnectionId) -> Vec<ThreadId> {
        {
            let mut state = self.state.lock().await;
            state.live_connections.remove(&connection_id);
            state
                .pending_thread_resume_reservations
                .retain(|(_, candidate_connection_id), _| {
                    *candidate_connection_id != connection_id
                });
            let thread_ids = state
                .thread_ids_by_connection
                .remove(&connection_id)
                .unwrap_or_default();
            for thread_id in &thread_ids {
                if let Some(thread_entry) = state.threads.get_mut(thread_id) {
                    thread_entry.connection_ids.remove(&connection_id);
                    thread_entry.connection_subscription_ids.remove(&connection_id);
                    thread_entry.update_has_connections();
                }
            }
            thread_ids
                .into_iter()
                .filter(|thread_id| {
                    state
                        .threads
                        .get(thread_id)
                        .is_some_and(|thread_entry| thread_entry.connection_ids.is_empty())
                })
                .collect::<Vec<_>>()
        }
    }

    pub(crate) async fn subscribe_to_has_connections(
        &self,
        thread_id: ThreadId,
    ) -> Option<watch::Receiver<bool>> {
        let state = self.state.lock().await;
        state
            .threads
            .get(&thread_id)
            .map(|thread_entry| thread_entry.has_connections_watcher.subscribe())
    }
}
