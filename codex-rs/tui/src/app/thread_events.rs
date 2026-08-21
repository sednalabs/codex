//! Thread event buffering and replay state for the TUI app.
//!
//! This module owns the per-thread event store used when the TUI switches between the main
//! conversation, subagents, and side conversations. It keeps buffered app-server notifications,
//! pending interactive request replay state, active-turn tracking, and saved composer state close
//! together with the replay behavior that consumes them.

use super::*;
use crate::app_event::HistoryBatchEntryResponse;
use std::borrow::Cow;

#[derive(Debug, Clone)]
pub(super) struct ThreadEventSnapshot {
    pub(super) session: Option<ThreadSessionState>,
    pub(super) turns: Vec<Turn>,
    pub(super) events: Vec<ThreadBufferedEvent>,
    pub(super) input_state: Option<ThreadInputState>,
}

#[derive(Debug, Clone)]
pub(super) enum ThreadBufferedEvent {
    Notification(ServerNotification),
    Request(ServerRequest),
    HistoryEntryResponse(HistoryLookupResponse),
    FeedbackSubmission(FeedbackThreadEvent),
}

const PENDING_DELIVERY_MAX_EVENTS: usize = 256;
const PENDING_DELIVERY_MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveDeliveryKind {
    RawResponseItemCompleted,
    FileChangePatchUpdated,
    ServerRequestResolved,
    McpToolCallProgress,
    ThreadRealtimeItemAdded,
    ThreadRealtimeOutputAudioDelta,
    ThreadRealtimeSdp,
    ThreadRealtimeTranscriptDelta,
    ThreadRealtimeTranscriptDone,
    CommandExecOutputDelta,
    ProcessOutputDelta,
    ProcessExited,
}

#[derive(Debug, Default)]
struct PendingDeliveryQueue {
    events: VecDeque<ThreadBufferedEvent>,
    bytes: usize,
}

impl PendingDeliveryQueue {
    fn len(&self) -> usize {
        self.events.len()
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    fn bytes(&self) -> usize {
        self.bytes
    }

    fn push_if_bounded(&mut self, event: ThreadBufferedEvent) {
        let event_bytes = event.estimated_bytes();
        if event_bytes > PENDING_DELIVERY_MAX_BYTES
            || self.events.len() >= PENDING_DELIVERY_MAX_EVENTS
            || self.bytes.saturating_add(event_bytes) > PENDING_DELIVERY_MAX_BYTES
        {
            if let Some(kind) = event.live_delivery_kind() {
                if let Some(existing_index) = self
                    .events
                    .iter()
                    .position(|existing| existing.live_delivery_kind() == Some(kind))
                {
                    let existing_bytes = self.events[existing_index].estimated_bytes();
                    let replacement_bytes = self
                        .bytes
                        .saturating_sub(existing_bytes)
                        .saturating_add(event_bytes);
                    if replacement_bytes <= PENDING_DELIVERY_MAX_BYTES {
                        self.events[existing_index] = event;
                        self.bytes = replacement_bytes;
                    } else {
                        tracing::warn!(
                            ?kind,
                            event_bytes,
                            max_bytes = PENDING_DELIVERY_MAX_BYTES,
                            "dropping oversized live-only notification delivery copy"
                        );
                    }
                    return;
                }

                // Live-only notifications have no store-backed recovery path. Reserve room for
                // the newest notification by evicting oldest delivery copies; ordinary copies
                // remain recoverable from the store at the next snapshot boundary.
                while !self.events.is_empty()
                    && (self.events.len() >= PENDING_DELIVERY_MAX_EVENTS
                        || self.bytes.saturating_add(event_bytes) > PENDING_DELIVERY_MAX_BYTES)
                {
                    let _ = self.pop_front();
                }
                if event_bytes <= PENDING_DELIVERY_MAX_BYTES {
                    self.bytes = self.bytes.saturating_add(event_bytes);
                    self.events.push_back(event);
                } else {
                    tracing::warn!(
                        ?kind,
                        event_bytes,
                        max_bytes = PENDING_DELIVERY_MAX_BYTES,
                        "dropping oversized live-only notification delivery copy"
                    );
                }
                return;
            }
            // The event is already retained by ThreadEventStore. Drop only this live-delivery
            // copy when the explicit safety budget is exhausted. Replay can recover ordinary
            // events from the store; live-only notifications are coalesced above by kind and are
            // otherwise dropped with bounded fail-closed behavior because they have no replay
            // representation.
            return;
        }
        self.bytes = self.bytes.saturating_add(event_bytes);
        self.events.push_back(event);
    }

    fn pop_front(&mut self) -> Option<ThreadBufferedEvent> {
        let event = self.events.pop_front()?;
        self.bytes = self.bytes.saturating_sub(event.estimated_bytes());
        Some(event)
    }

    fn push_front(&mut self, event: ThreadBufferedEvent) {
        self.bytes = self.bytes.saturating_add(event.estimated_bytes());
        self.events.push_front(event);
    }

    fn clear(&mut self) {
        self.events.clear();
        self.bytes = 0;
    }
}

impl ThreadBufferedEvent {
    fn live_delivery_kind(&self) -> Option<LiveDeliveryKind> {
        let Self::Notification(notification) = self else {
            return None;
        };
        Some(match notification {
            ServerNotification::RawResponseItemCompleted(_) => {
                LiveDeliveryKind::RawResponseItemCompleted
            }
            ServerNotification::FileChangePatchUpdated(_) => {
                LiveDeliveryKind::FileChangePatchUpdated
            }
            ServerNotification::ServerRequestResolved(_) => LiveDeliveryKind::ServerRequestResolved,
            ServerNotification::McpToolCallProgress(_) => LiveDeliveryKind::McpToolCallProgress,
            ServerNotification::ThreadRealtimeItemAdded(_) => {
                LiveDeliveryKind::ThreadRealtimeItemAdded
            }
            ServerNotification::ThreadRealtimeOutputAudioDelta(_) => {
                LiveDeliveryKind::ThreadRealtimeOutputAudioDelta
            }
            ServerNotification::ThreadRealtimeSdp(_) => LiveDeliveryKind::ThreadRealtimeSdp,
            ServerNotification::ThreadRealtimeTranscriptDelta(_) => {
                LiveDeliveryKind::ThreadRealtimeTranscriptDelta
            }
            ServerNotification::ThreadRealtimeTranscriptDone(_) => {
                LiveDeliveryKind::ThreadRealtimeTranscriptDone
            }
            ServerNotification::CommandExecOutputDelta(_) => {
                LiveDeliveryKind::CommandExecOutputDelta
            }
            ServerNotification::ProcessOutputDelta(_) => LiveDeliveryKind::ProcessOutputDelta,
            ServerNotification::ProcessExited(_) => LiveDeliveryKind::ProcessExited,
            _ => return None,
        })
    }

    fn estimated_bytes(&self) -> usize {
        match self {
            Self::Notification(notification) => serde_json::to_vec(notification)
                .map(|bytes| bytes.len())
                .unwrap_or(PENDING_DELIVERY_MAX_BYTES.saturating_add(1)),
            Self::Request(request) => serde_json::to_vec(request)
                .map(|bytes| bytes.len())
                .unwrap_or(PENDING_DELIVERY_MAX_BYTES.saturating_add(1)),
            Self::HistoryEntryResponse(response) => match response {
                HistoryLookupResponse::Entry { entry, .. } => {
                    std::mem::size_of::<HistoryLookupResponse>()
                        .saturating_add(entry.as_ref().map_or(0, String::len))
                }
                HistoryLookupResponse::Batch { entries, .. } => std::mem::size_of::<
                    HistoryLookupResponse,
                >()
                .saturating_add(entries.iter().fold(0usize, |bytes, entry| {
                    bytes.saturating_add(
                        std::mem::size_of::<HistoryBatchEntryResponse>()
                            .saturating_add(entry.entry.as_ref().map_or(0, String::len)),
                    )
                })),
                HistoryLookupResponse::BatchError { .. } => {
                    std::mem::size_of::<HistoryLookupResponse>()
                }
            },
            Self::FeedbackSubmission(feedback) => std::mem::size_of::<FeedbackThreadEvent>()
                .saturating_add(match &feedback.result {
                    Ok(thread_id) | Err(thread_id) => thread_id.len(),
                }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FeedbackThreadEvent {
    pub(super) category: FeedbackCategory,
    pub(super) include_logs: bool,
    pub(super) feedback_audience: FeedbackAudience,
    pub(super) result: Result<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ThreadEventAttachment {
    Live,
    ReplayOnly,
}

#[derive(Debug)]
pub(super) struct ThreadEventStore {
    pub(super) session: Option<ThreadSessionState>,
    hydrated_snapshot: bool,
    pub(super) turns: Vec<Turn>,
    pub(super) buffer: VecDeque<ThreadBufferedEvent>,
    pub(super) pending_interactive_replay: PendingInteractiveReplayState,
    pub(super) active_turn_id: Option<String>,
    pub(super) pending_interrupt_turn_id: Option<String>,
    pub(super) input_state: Option<ThreadInputState>,
    pub(super) capacity: usize,
    pub(super) active: bool,
}

impl ThreadEventStore {
    pub(super) fn event_survives_session_refresh(event: &ThreadBufferedEvent) -> bool {
        matches!(
            event,
            ThreadBufferedEvent::Request(_)
                | ThreadBufferedEvent::Notification(ServerNotification::HookStarted(_))
                | ThreadBufferedEvent::Notification(ServerNotification::HookCompleted(_))
                | ThreadBufferedEvent::Notification(ServerNotification::McpServerStatusUpdated(_))
                | ThreadBufferedEvent::HistoryEntryResponse(_)
                | ThreadBufferedEvent::FeedbackSubmission(_)
        )
    }

    pub(super) fn new(capacity: usize) -> Self {
        Self {
            session: None,
            hydrated_snapshot: false,
            turns: Vec::new(),
            buffer: VecDeque::new(),
            pending_interactive_replay: PendingInteractiveReplayState::default(),
            active_turn_id: None,
            pending_interrupt_turn_id: None,
            input_state: None,
            capacity,
            active: false,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn new_with_session(
        capacity: usize,
        session: ThreadSessionState,
        turns: Vec<Turn>,
    ) -> Self {
        let mut store = Self::new(capacity);
        store.set_session(session, turns);
        store
    }

    pub(super) fn set_inferred_session(&mut self, session: ThreadSessionState) {
        self.session = Some(session);
    }

    pub(super) fn set_session(&mut self, session: ThreadSessionState, turns: Vec<Turn>) {
        self.session = Some(session);
        self.hydrated_snapshot = true;
        self.set_turns(turns);
    }

    pub(super) fn has_hydrated_snapshot(&self) -> bool {
        self.hydrated_snapshot
    }

    pub(super) fn rebase_buffer_after_session_refresh(&mut self) {
        self.buffer.retain(Self::event_survives_session_refresh);
    }

    pub(super) fn set_turns(&mut self, turns: Vec<Turn>) {
        self.active_turn_id = turns
            .iter()
            .rev()
            .find(|turn| matches!(turn.status, TurnStatus::InProgress))
            .map(|turn| turn.id.clone());
        self.turns = turns;
    }

    pub(super) fn push_notification(&mut self, notification: ServerNotification) {
        self.push_notification_inner(Cow::Owned(notification));
    }

    pub(super) fn push_notification_ref(&mut self, notification: &ServerNotification) {
        self.push_notification_inner(Cow::Borrowed(notification));
    }

    fn push_notification_inner(&mut self, notification: Cow<'_, ServerNotification>) {
        self.pending_interactive_replay
            .note_server_notification(notification.as_ref());
        match notification.as_ref() {
            ServerNotification::TurnStarted(turn) => {
                self.active_turn_id = Some(turn.turn.id.clone());
            }
            ServerNotification::TurnCompleted(turn) => {
                if self.active_turn_id.as_deref() == Some(turn.turn.id.as_str()) {
                    self.active_turn_id = None;
                }
                if self.pending_interrupt_turn_id.as_deref() == Some(turn.turn.id.as_str()) {
                    self.pending_interrupt_turn_id = None;
                }
            }
            ServerNotification::ThreadClosed(_) => {
                self.active_turn_id = None;
                self.pending_interrupt_turn_id = None;
            }
            _ => {}
        }

        // These notifications are either handled before routing or ignored by ChatWidget on
        // replay. In particular, raw response items and realtime audio can carry large payloads,
        // so cloning them into every thread's replay buffer only retains data the TUI cannot use.
        if matches!(
            notification.as_ref(),
            ServerNotification::RawResponseItemCompleted(_)
                | ServerNotification::FileChangePatchUpdated(_)
                | ServerNotification::ServerRequestResolved(_)
                | ServerNotification::McpToolCallProgress(_)
                | ServerNotification::ThreadRealtimeItemAdded(_)
                | ServerNotification::ThreadRealtimeOutputAudioDelta(_)
                | ServerNotification::ThreadRealtimeSdp(_)
                | ServerNotification::ThreadRealtimeTranscriptDelta(_)
                | ServerNotification::ThreadRealtimeTranscriptDone(_)
                | ServerNotification::CommandExecOutputDelta(_)
                | ServerNotification::ProcessOutputDelta(_)
                | ServerNotification::ProcessExited(_)
        ) {
            return;
        }

        self.buffer
            .push_back(ThreadBufferedEvent::Notification(notification.into_owned()));
        if self.buffer.len() > self.capacity
            && let Some(removed) = self.buffer.pop_front()
            && let ThreadBufferedEvent::Request(request) = &removed
        {
            self.pending_interactive_replay
                .note_evicted_server_request(request);
        }
    }

    pub(super) fn push_request(&mut self, request: ServerRequest) {
        self.pending_interactive_replay
            .note_server_request(&request);
        self.buffer.push_back(ThreadBufferedEvent::Request(request));
        if self.buffer.len() > self.capacity
            && let Some(removed) = self.buffer.pop_front()
            && let ThreadBufferedEvent::Request(request) = &removed
        {
            self.pending_interactive_replay
                .note_evicted_server_request(request);
        }
    }

    pub(super) fn pending_replay_requests(&self) -> Vec<ServerRequest> {
        self.buffer
            .iter()
            .filter_map(|event| match event {
                ThreadBufferedEvent::Request(request)
                    if self
                        .pending_interactive_replay
                        .should_replay_snapshot_request(request) =>
                {
                    Some(request.clone())
                }
                ThreadBufferedEvent::Request(_)
                | ThreadBufferedEvent::Notification(_)
                | ThreadBufferedEvent::HistoryEntryResponse(_)
                | ThreadBufferedEvent::FeedbackSubmission(_) => None,
            })
            .collect()
    }

    pub(super) fn file_change_changes(
        &self,
        turn_id: &str,
        item_id: &str,
    ) -> Option<Vec<codex_app_server_protocol::FileUpdateChange>> {
        self.buffer
            .iter()
            .rev()
            .find_map(|event| match event {
                ThreadBufferedEvent::Notification(ServerNotification::ItemStarted(
                    notification,
                )) if turn_id_matches(turn_id, &notification.turn_id) => {
                    file_change_item_changes(&notification.item, item_id)
                }
                ThreadBufferedEvent::Notification(ServerNotification::ItemCompleted(
                    notification,
                )) if turn_id_matches(turn_id, &notification.turn_id) => {
                    file_change_item_changes(&notification.item, item_id)
                }
                ThreadBufferedEvent::Request(_)
                | ThreadBufferedEvent::Notification(_)
                | ThreadBufferedEvent::HistoryEntryResponse(_)
                | ThreadBufferedEvent::FeedbackSubmission(_) => None,
            })
            .or_else(|| {
                self.turns
                    .iter()
                    .rev()
                    .filter(|turn| turn_id_matches(turn_id, &turn.id))
                    .flat_map(|turn| turn.items.iter().rev())
                    .find_map(|item| file_change_item_changes(item, item_id))
            })
    }

    pub(super) fn snapshot(&self) -> ThreadEventSnapshot {
        ThreadEventSnapshot {
            session: self.session.clone(),
            turns: self.turns.clone(),
            // Thread switches replay buffered events into a rebuilt ChatWidget. Only replay
            // interactive prompts that are still pending, or answered approvals/input will reappear.
            events: self
                .buffer
                .iter()
                .filter(|event| match event {
                    ThreadBufferedEvent::Request(request) => self
                        .pending_interactive_replay
                        .should_replay_snapshot_request(request),
                    ThreadBufferedEvent::Notification(_)
                    | ThreadBufferedEvent::HistoryEntryResponse(_)
                    | ThreadBufferedEvent::FeedbackSubmission(_) => true,
                })
                .cloned()
                .collect(),
            input_state: self.input_state.clone(),
        }
    }

    pub(super) fn note_outbound_op<T>(&mut self, op: T)
    where
        T: Into<AppCommand>,
    {
        self.pending_interactive_replay.note_outbound_op(op);
    }

    pub(super) fn op_can_change_pending_replay_state<T>(op: T) -> bool
    where
        T: Into<AppCommand>,
    {
        PendingInteractiveReplayState::op_can_change_state(op)
    }

    pub(super) fn has_pending_thread_approvals(&self) -> bool {
        self.pending_interactive_replay
            .has_pending_thread_approvals()
    }

    pub(super) fn side_parent_pending_status(&self) -> Option<SideParentStatus> {
        if self
            .pending_interactive_replay
            .has_pending_thread_user_input()
        {
            Some(SideParentStatus::NeedsInput)
        } else if self
            .pending_interactive_replay
            .has_pending_thread_approvals()
        {
            Some(SideParentStatus::NeedsApproval)
        } else {
            None
        }
    }

    pub(super) fn active_turn_id(&self) -> Option<&str> {
        self.active_turn_id.as_deref()
    }

    pub(super) fn clear_active_turn_id(&mut self) {
        self.active_turn_id = None;
    }
}

fn turn_id_matches(request_turn_id: &str, candidate_turn_id: &str) -> bool {
    request_turn_id.is_empty() || request_turn_id == candidate_turn_id
}

fn file_change_item_changes(
    item: &ThreadItem,
    item_id: &str,
) -> Option<Vec<codex_app_server_protocol::FileUpdateChange>> {
    match item {
        ThreadItem::FileChange { id, changes, .. } if id == item_id => Some(changes.clone()),
        _ => None,
    }
}

#[derive(Debug)]
pub(super) struct ThreadEventChannel {
    pub(super) sender: mpsc::Sender<ThreadBufferedEvent>,
    pub(super) receiver: Option<mpsc::Receiver<ThreadBufferedEvent>>,
    pub(super) store: Arc<Mutex<ThreadEventStore>>,
    /// Delivery copies that could not enter the bounded receiver immediately. The store remains
    /// the sole owner of replay state; this queue is only a live-delivery retry lane and is
    /// discarded whenever a receiver is drained for a snapshot boundary.
    pub(super) pending_delivery: Arc<std::sync::Mutex<PendingDeliveryQueue>>,
    attachment: ThreadEventAttachment,
}

impl ThreadEventChannel {
    pub(super) fn new(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        Self {
            sender,
            receiver: Some(receiver),
            store: Arc::new(Mutex::new(ThreadEventStore::new(capacity))),
            pending_delivery: Arc::new(std::sync::Mutex::new(PendingDeliveryQueue::default())),
            attachment: ThreadEventAttachment::Live,
        }
    }

    pub(super) fn mark_replay_only(&mut self) {
        self.attachment = ThreadEventAttachment::ReplayOnly;
    }

    pub(super) fn attachment(&self) -> ThreadEventAttachment {
        self.attachment
    }

    pub(super) fn rebase_receiver_after_session_refresh(&mut self) {
        self.clear_pending_delivery();
        let Some(receiver) = self.receiver.as_mut() else {
            return;
        };
        Self::discard_event_receiver_after_session_refresh(receiver);
    }

    /// The event store is the sole owner of events that existed before a refreshed session
    /// snapshot. Routing records every event in that store before attempting to notify an active
    /// receiver, so receiver entries are delivery copies, not a second recovery source. Dropping
    /// those copies avoids both duplicate replay and a drain-and-requeue `Full`/`Closed` loss
    /// boundary; arrivals after this drain remain in the receiver for normal live delivery.
    pub(super) fn discard_event_receiver_after_session_refresh(
        receiver: &mut mpsc::Receiver<ThreadBufferedEvent>,
    ) {
        while receiver.try_recv().is_ok() {}
    }

    pub(super) fn clear_pending_delivery(&self) {
        self.pending_delivery
            .lock()
            .expect("pending thread delivery mutex poisoned")
            .clear();
    }

    /// Delivers a live copy without spawning a sender that can cross a receiver/snapshot boundary.
    /// If the bounded channel is full, retain the copy in the channel-local retry lane; the event
    /// itself has already been recorded by `ThreadEventStore`, which is the exactly-once replay
    /// owner.
    pub(super) fn try_send_or_queue(&self, event: ThreadBufferedEvent, thread_id: ThreadId) {
        let mut pending = self
            .pending_delivery
            .lock()
            .expect("pending thread delivery mutex poisoned");
        if !pending.is_empty() {
            pending.push_if_bounded(event);
            return;
        }
        match self.sender.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(event)) => pending.push_if_bounded(event),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("thread {thread_id} event channel closed");
            }
        }
    }

    /// Moves as many queued live-delivery copies as capacity permits into the receiver. This is
    /// called only after the active receiver has been drained, so no asynchronous sender can race
    /// a picker refresh or snapshot replay.
    pub(super) fn flush_pending_delivery(&self) {
        let mut pending = self
            .pending_delivery
            .lock()
            .expect("pending thread delivery mutex poisoned");
        while let Some(event) = pending.pop_front() {
            match self.sender.try_send(event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(event)) => {
                    pending.push_front(event);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    pending.clear();
                    break;
                }
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn new_with_session(
        capacity: usize,
        session: ThreadSessionState,
        turns: Vec<Turn>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        Self {
            sender,
            receiver: Some(receiver),
            store: Arc::new(Mutex::new(ThreadEventStore::new_with_session(
                capacity, session, turns,
            ))),
            pending_delivery: Arc::new(std::sync::Mutex::new(PendingDeliveryQueue::default())),
            attachment: ThreadEventAttachment::Live,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::PathBufExt;
    use crate::test_support::test_path_buf;
    use codex_app_server_protocol::AskForApproval;
    use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
    use codex_app_server_protocol::HookCompletedNotification;
    use codex_app_server_protocol::HookEventName as AppServerHookEventName;
    use codex_app_server_protocol::HookExecutionMode as AppServerHookExecutionMode;
    use codex_app_server_protocol::HookHandlerType as AppServerHookHandlerType;
    use codex_app_server_protocol::HookOutputEntry as AppServerHookOutputEntry;
    use codex_app_server_protocol::HookOutputEntryKind as AppServerHookOutputEntryKind;
    use codex_app_server_protocol::HookRunStatus as AppServerHookRunStatus;
    use codex_app_server_protocol::HookRunSummary as AppServerHookRunSummary;
    use codex_app_server_protocol::HookScope as AppServerHookScope;
    use codex_app_server_protocol::HookStartedNotification;
    use codex_app_server_protocol::McpToolCallProgressNotification;
    use codex_app_server_protocol::RequestId as AppServerRequestId;
    use codex_app_server_protocol::ThreadRealtimeAudioChunk;
    use codex_app_server_protocol::ThreadRealtimeOutputAudioDeltaNotification;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnStartedNotification;
    use codex_config::types::ApprovalsReviewer;
    use codex_protocol::models::PermissionProfile;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn test_thread_session(thread_id: ThreadId, cwd: PathBuf) -> ThreadSessionState {
        ThreadSessionState {
            thread_id,
            forked_from_id: None,
            fork_parent_title: None,
            thread_name: None,
            model: "gpt-test".to_string(),
            model_provider_id: "test-provider".to_string(),
            service_tier: None,
            approval_policy: AskForApproval::Never,
            approvals_reviewer: ApprovalsReviewer::User,
            permission_profile: PermissionProfile::read_only(),
            active_permission_profile: None,
            cwd: cwd.abs(),
            runtime_workspace_roots: Vec::new(),
            instruction_source_paths: Vec::new(),
            reasoning_effort: None,
            collaboration_mode: None,
            personality: None,
            message_history: None,
            network_proxy: None,
            rollout_path: Some(PathBuf::new()),
        }
    }

    fn test_turn(turn_id: &str, status: TurnStatus, items: Vec<ThreadItem>) -> Turn {
        Turn {
            id: turn_id.to_string(),
            items_view: codex_app_server_protocol::TurnItemsView::Full,
            items,
            status,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }
    }

    fn turn_started_notification(thread_id: ThreadId, turn_id: &str) -> ServerNotification {
        ServerNotification::TurnStarted(TurnStartedNotification {
            thread_id: thread_id.to_string(),
            turn: Turn {
                started_at: Some(0),
                ..test_turn(turn_id, TurnStatus::InProgress, Vec::new())
            },
        })
    }

    fn turn_completed_notification(
        thread_id: ThreadId,
        turn_id: &str,
        status: TurnStatus,
    ) -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: thread_id.to_string(),
            turn: Turn {
                completed_at: Some(0),
                duration_ms: Some(1),
                ..test_turn(turn_id, status, Vec::new())
            },
            final_model: None,
            model_snapshot: None,
        })
    }

    fn hook_started_notification(thread_id: ThreadId, turn_id: &str) -> ServerNotification {
        ServerNotification::HookStarted(HookStartedNotification {
            thread_id: thread_id.to_string(),
            turn_id: Some(turn_id.to_string()),
            run: AppServerHookRunSummary {
                id: "user-prompt-submit:0:/tmp/hooks.json".to_string(),
                event_name: AppServerHookEventName::UserPromptSubmit,
                handler_type: AppServerHookHandlerType::Command,
                execution_mode: AppServerHookExecutionMode::Sync,
                scope: AppServerHookScope::Turn,
                source_path: test_path_buf("/tmp/hooks.json").abs(),
                source: codex_app_server_protocol::HookSource::User,
                display_order: 0,
                status: AppServerHookRunStatus::Running,
                status_message: Some("checking go-workflow input policy".to_string()),
                started_at: 1,
                completed_at: None,
                duration_ms: None,
                entries: Vec::new(),
            },
        })
    }

    fn hook_completed_notification(thread_id: ThreadId, turn_id: &str) -> ServerNotification {
        ServerNotification::HookCompleted(HookCompletedNotification {
            thread_id: thread_id.to_string(),
            turn_id: Some(turn_id.to_string()),
            run: AppServerHookRunSummary {
                id: "user-prompt-submit:0:/tmp/hooks.json".to_string(),
                event_name: AppServerHookEventName::UserPromptSubmit,
                handler_type: AppServerHookHandlerType::Command,
                execution_mode: AppServerHookExecutionMode::Sync,
                scope: AppServerHookScope::Turn,
                source_path: test_path_buf("/tmp/hooks.json").abs(),
                source: codex_app_server_protocol::HookSource::User,
                display_order: 0,
                status: AppServerHookRunStatus::Stopped,
                status_message: Some("checking go-workflow input policy".to_string()),
                started_at: 1,
                completed_at: Some(11),
                duration_ms: Some(10),
                entries: vec![
                    AppServerHookOutputEntry {
                        kind: AppServerHookOutputEntryKind::Warning,
                        text: "go-workflow must start from PlanMode".to_string(),
                    },
                    AppServerHookOutputEntry {
                        kind: AppServerHookOutputEntryKind::Stop,
                        text: "prompt blocked".to_string(),
                    },
                ],
            },
        })
    }

    fn exec_approval_request(
        thread_id: ThreadId,
        turn_id: &str,
        item_id: &str,
        approval_id: Option<&str>,
    ) -> ServerRequest {
        ServerRequest::CommandExecutionRequestApproval {
            request_id: AppServerRequestId::Integer(1),
            params: CommandExecutionRequestApprovalParams {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                item_id: item_id.to_string(),
                started_at_ms: 0,
                approval_id: approval_id.map(str::to_string),
                environment_id: None,
                reason: Some("needs approval".to_string()),
                network_approval_context: None,
                command: Some("echo hello".to_string()),
                cwd: Some(test_path_buf("/tmp/project").abs().into()),
                command_actions: None,
                additional_permissions: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                available_decisions: None,
            },
        }
    }

    #[test]
    fn thread_event_store_tracks_active_turn_lifecycle() {
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        assert_eq!(store.active_turn_id(), None);

        let thread_id = ThreadId::new();
        store.push_notification(turn_started_notification(thread_id, "turn-1"));
        assert_eq!(store.active_turn_id(), Some("turn-1"));

        store.push_notification(turn_completed_notification(
            thread_id,
            "turn-2",
            TurnStatus::Completed,
        ));
        assert_eq!(store.active_turn_id(), Some("turn-1"));

        store.push_notification(turn_completed_notification(
            thread_id,
            "turn-1",
            TurnStatus::Interrupted,
        ));
        assert_eq!(store.active_turn_id(), None);
    }

    #[test]
    fn thread_event_store_restores_active_turn_from_snapshot_turns() {
        let thread_id = ThreadId::new();
        let session = test_thread_session(thread_id, test_path_buf("/tmp/project"));
        let turns = vec![
            test_turn("turn-1", TurnStatus::Completed, Vec::new()),
            test_turn("turn-2", TurnStatus::InProgress, Vec::new()),
        ];

        let store =
            ThreadEventStore::new_with_session(/*capacity*/ 8, session.clone(), turns.clone());
        assert_eq!(store.active_turn_id(), Some("turn-2"));

        let mut refreshed_store = ThreadEventStore::new(/*capacity*/ 8);
        refreshed_store.set_session(session, turns);
        assert_eq!(refreshed_store.active_turn_id(), Some("turn-2"));
    }

    #[test]
    fn inferred_session_is_not_a_hydrated_snapshot() {
        let thread_id = ThreadId::new();
        let session = test_thread_session(thread_id, test_path_buf("/tmp/project"));
        let mut store = ThreadEventStore::new(/*capacity*/ 8);

        store.set_inferred_session(session.clone());
        assert!(store.session.is_some());
        assert!(!store.has_hydrated_snapshot());

        store.set_session(session, Vec::new());
        assert!(store.has_hydrated_snapshot());
    }

    #[test]
    fn thread_event_store_clear_active_turn_id_resets_cached_turn() {
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        let thread_id = ThreadId::new();
        store.push_notification(turn_started_notification(thread_id, "turn-1"));

        store.clear_active_turn_id();

        assert_eq!(store.active_turn_id(), None);
    }

    #[test]
    fn thread_event_store_skips_large_replay_irrelevant_notifications() {
        let thread_id = ThreadId::new();
        let mut store = ThreadEventStore::new(/*capacity*/ 2);
        store.push_notification(turn_started_notification(thread_id, "turn-1"));
        store.push_request(exec_approval_request(
            thread_id,
            "turn-1",
            "command-approval",
            /*approval_id*/ None,
        ));
        let large_payload = "x".repeat(1024 * 1024);

        for _ in 0..32 {
            store.push_notification_ref(&ServerNotification::McpToolCallProgress(
                McpToolCallProgressNotification {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "mcp-1".to_string(),
                    message: large_payload.clone(),
                },
            ));
            store.push_notification_ref(&ServerNotification::ThreadRealtimeOutputAudioDelta(
                ThreadRealtimeOutputAudioDeltaNotification {
                    thread_id: thread_id.to_string(),
                    audio: ThreadRealtimeAudioChunk {
                        data: large_payload.clone(),
                        sample_rate: 24_000,
                        num_channels: 1,
                        samples_per_channel: None,
                        item_id: None,
                    },
                },
            ));
        }

        assert_eq!(store.buffer.len(), 2);
        assert!(store.has_pending_thread_approvals());
        assert_eq!(store.active_turn_id(), Some("turn-1"));
    }

    #[test]
    fn thread_event_store_rebase_preserves_resolved_request_state() {
        let thread_id = ThreadId::new();
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        store.push_request(exec_approval_request(
            thread_id,
            "turn-approval",
            "call-approval",
            /*approval_id*/ None,
        ));
        store.push_notification(ServerNotification::ServerRequestResolved(
            codex_app_server_protocol::ServerRequestResolvedNotification {
                request_id: AppServerRequestId::Integer(1),
                thread_id: thread_id.to_string(),
            },
        ));

        store.rebase_buffer_after_session_refresh();

        let snapshot = store.snapshot();
        assert!(snapshot.events.is_empty());
        assert_eq!(store.has_pending_thread_approvals(), false);
    }

    #[test]
    fn thread_event_store_rebase_preserves_hook_notifications() {
        let thread_id = ThreadId::new();
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        store.push_notification(hook_started_notification(thread_id, "turn-hook"));
        store.push_notification(hook_completed_notification(thread_id, "turn-hook"));

        store.rebase_buffer_after_session_refresh();

        let snapshot = store.snapshot();
        let hook_notifications = snapshot
            .events
            .into_iter()
            .map(|event| match event {
                ThreadBufferedEvent::Notification(notification) => {
                    serde_json::to_value(notification).expect("hook notification should serialize")
                }
                other => panic!("expected buffered hook notification, saw: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            hook_notifications,
            vec![
                serde_json::to_value(hook_started_notification(thread_id, "turn-hook"))
                    .expect("hook notification should serialize"),
                serde_json::to_value(hook_completed_notification(thread_id, "turn-hook"))
                    .expect("hook notification should serialize"),
            ]
        );
    }

    #[test]
    fn thread_event_store_rebase_preserves_mcp_startup_notifications() {
        let thread_id = ThreadId::new();
        let notification = ServerNotification::McpServerStatusUpdated(
            codex_app_server_protocol::McpServerStatusUpdatedNotification {
                thread_id: Some(thread_id.to_string()),
                name: "sentry".to_string(),
                status: codex_app_server_protocol::McpServerStartupState::Failed,
                error: Some("sentry is not logged in".to_string()),
                failure_reason: None,
            },
        );
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        store.push_notification_ref(&notification);

        store.rebase_buffer_after_session_refresh();

        let snapshot = store.snapshot();
        let actual = match snapshot.events.as_slice() {
            [ThreadBufferedEvent::Notification(actual)] => actual,
            other => panic!("expected one buffered MCP notification, saw: {other:?}"),
        };
        assert_eq!(
            serde_json::to_value(actual).expect("MCP notification should serialize"),
            serde_json::to_value(notification).expect("MCP notification should serialize"),
        );
    }

    #[test]
    fn session_refresh_uses_the_store_as_the_exactly_once_survivor_owner() {
        let thread_id = ThreadId::new();
        let request = exec_approval_request(
            thread_id,
            "turn-approval",
            "call-approval",
            /*approval_id*/ None,
        );
        let hook = hook_started_notification(thread_id, "turn-hook");
        let mut channel = ThreadEventChannel::new(/*capacity*/ 2);
        {
            let mut store = channel.store.blocking_lock();
            store.push_request(request.clone());
            store.push_notification(hook.clone());
            store.rebase_buffer_after_session_refresh();
        }
        channel
            .sender
            .try_send(ThreadBufferedEvent::Request(request))
            .expect("receiver has capacity for delivery copy");
        channel
            .sender
            .try_send(ThreadBufferedEvent::Notification(hook))
            .expect("receiver has capacity for delivery copy");

        channel.rebase_receiver_after_session_refresh();
        let receiver = channel.receiver.as_mut().expect("receiver is retained");
        assert!(
            receiver.try_recv().is_err(),
            "delivery copies were discarded"
        );

        let snapshot = channel.store.blocking_lock().snapshot();
        assert_eq!(
            snapshot.events.len(),
            2,
            "survivors replay from the store once"
        );
        assert!(matches!(
            snapshot.events[0],
            ThreadBufferedEvent::Request(_)
        ));
        assert!(matches!(
            snapshot.events[1],
            ThreadBufferedEvent::Notification(ServerNotification::HookStarted(_))
        ));
    }

    #[test]
    fn blocked_live_delivery_is_discarded_at_snapshot_boundary() {
        let thread_id = ThreadId::new();
        let request = exec_approval_request(
            thread_id,
            "turn-blocked",
            "call-blocked",
            /*approval_id*/ None,
        );
        let mut channel = ThreadEventChannel::new(/*capacity*/ 1);
        {
            let mut store = channel.store.blocking_lock();
            store.push_request(request.clone());
        }

        // Occupy the bounded receiver, then queue the delivery copy that used to be held by an
        // async `sender.send`. A picker/session snapshot must discard that copy and replay the
        // store-owned request exactly once.
        channel
            .sender
            .try_send(ThreadBufferedEvent::Notification(
                hook_started_notification(thread_id, "turn-blocked"),
            ))
            .expect("receiver should accept the blocking event");
        channel.try_send_or_queue(ThreadBufferedEvent::Request(request), thread_id);
        assert_eq!(
            channel
                .pending_delivery
                .lock()
                .expect("pending delivery mutex")
                .len(),
            1
        );

        channel.rebase_receiver_after_session_refresh();
        let snapshot = channel.store.blocking_lock().snapshot();
        assert_eq!(snapshot.events.len(), 1);
        assert!(matches!(
            snapshot.events[0],
            ThreadBufferedEvent::Request(_)
        ));
        assert!(
            channel
                .pending_delivery
                .lock()
                .expect("pending delivery mutex")
                .is_empty()
        );
        assert!(
            channel
                .receiver
                .as_mut()
                .expect("receiver is retained")
                .try_recv()
                .is_err()
        );
    }

    #[test]
    fn active_history_batch_survives_pending_delivery_boundary() {
        let thread_id = ThreadId::new();
        let batch = HistoryLookupResponse::Batch {
            cursor: codex_message_history::HistoryBatchCursor::new(/*end_offset*/ 4),
            log_id: 7,
            entries: vec![HistoryBatchEntryResponse {
                offset: 3,
                entry: Some("history batch survives detach".to_string()),
            }],
            next_older_cursor: None,
        };
        let mut channel = ThreadEventChannel::new(/*capacity*/ 1);
        {
            let mut store = channel.store.blocking_lock();
            store
                .buffer
                .push_back(ThreadBufferedEvent::HistoryEntryResponse(batch.clone()));
        }
        channel
            .sender
            .try_send(ThreadBufferedEvent::Notification(
                hook_started_notification(thread_id, "turn-batch"),
            ))
            .expect("receiver should accept the blocking event");
        channel.try_send_or_queue(ThreadBufferedEvent::HistoryEntryResponse(batch), thread_id);

        channel.rebase_receiver_after_session_refresh();
        channel
            .store
            .blocking_lock()
            .rebase_buffer_after_session_refresh();
        let snapshot = channel.store.blocking_lock().snapshot();
        assert!(matches!(
            snapshot.events.as_slice(),
            [ThreadBufferedEvent::HistoryEntryResponse(
                HistoryLookupResponse::Batch { .. }
            )]
        ));
    }

    #[test]
    fn pending_delivery_is_bounded_under_sustained_full_receiver() {
        let thread_id = ThreadId::new();
        let mut channel = ThreadEventChannel::new(/*capacity*/ 1);
        channel
            .sender
            .try_send(ThreadBufferedEvent::Notification(
                hook_started_notification(thread_id, "turn-full"),
            ))
            .expect("receiver should accept the blocking event");

        for index in 0..(PENDING_DELIVERY_MAX_EVENTS * 4) {
            channel.try_send_or_queue(
                ThreadBufferedEvent::Notification(hook_started_notification(
                    thread_id,
                    &format!("turn-{index}"),
                )),
                thread_id,
            );
        }

        let pending = channel
            .pending_delivery
            .lock()
            .expect("pending delivery mutex");
        assert!(pending.len() <= PENDING_DELIVERY_MAX_EVENTS);
        assert!(pending.bytes() <= PENDING_DELIVERY_MAX_BYTES);
    }

    #[test]
    fn full_receiver_coalesces_live_only_notifications_instead_of_losing_latest() {
        let thread_id = ThreadId::new();
        let mut channel = ThreadEventChannel::new(/*capacity*/ 1);
        channel
            .sender
            .try_send(ThreadBufferedEvent::Notification(
                hook_started_notification(thread_id, "turn-full"),
            ))
            .expect("receiver should accept the blocking event");

        let live_notification = |message: &str| {
            ThreadBufferedEvent::Notification(ServerNotification::McpToolCallProgress(
                McpToolCallProgressNotification {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-full".to_string(),
                    item_id: "mcp-1".to_string(),
                    message: message.to_string(),
                },
            ))
        };
        channel.try_send_or_queue(live_notification("first"), thread_id);
        for offset in 0..(PENDING_DELIVERY_MAX_EVENTS.saturating_sub(1)) {
            channel.try_send_or_queue(
                ThreadBufferedEvent::HistoryEntryResponse(HistoryLookupResponse::Entry {
                    offset,
                    log_id: offset as u64,
                    entry: Some("replayable".to_string()),
                }),
                thread_id,
            );
        }
        channel.try_send_or_queue(live_notification("latest"), thread_id);

        let pending = channel
            .pending_delivery
            .lock()
            .expect("pending delivery mutex");
        assert_eq!(pending.len(), PENDING_DELIVERY_MAX_EVENTS);
        assert!(pending.events.iter().any(|event| {
            matches!(
                event,
                ThreadBufferedEvent::Notification(
                    ServerNotification::McpToolCallProgress(notification)
                ) if notification.message == "latest"
            )
        }));
    }
}
