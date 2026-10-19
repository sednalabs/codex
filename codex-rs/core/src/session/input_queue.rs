use crate::agent::api::AgentControl;
use crate::agent_communication::PENDING_MAILBOX_MESSAGES;
use crate::codex_thread::CodexThread;
use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_diagnostics::GaugeGuard;
use codex_history::InterAgentCommunicationIntent;
use codex_history::ResponseItemEnvelope;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

/// Host capture metadata belonging to one input, including steers within another turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_order: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "codex_history::UserInputOrigin::is_user"
    )]
    pub origin: codex_history::UserInputOrigin,
}

/// Input consumed by a regular turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
        #[serde(flatten)]
        metadata: UserInputMetadata,
    },
    FunctionCallOutput(#[serde(with = "turn_input_response_item")] ResponseItemEnvelope),
    // Preserve the existing serialized format while carrying injection API metadata
    // through the in-memory queue.
    ResponseItem(#[serde(with = "turn_input_response_item")] ResponseItemEnvelope),
    InterAgentCommunication(InterAgentCommunication),
}

mod turn_input_response_item {
    use super::ResponseItem;
    use super::ResponseItemEnvelope;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serialize;
    use serde::Serializer;
    use serde::ser::Error as _;

    pub(super) fn serialize<S>(
        item: &ResponseItemEnvelope,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if item.metadata.is_some() {
            return Err(S::Error::custom(
                "annotated response items cannot cross the turn-input serialization boundary",
            ));
        }
        item.item.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<ResponseItemEnvelope, D::Error>
    where
        D: Deserializer<'de>,
    {
        ResponseItem::deserialize(deserializer).map(ResponseItemEnvelope::new)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputQueueActivity {
    Mailbox,
    Steer,
}

/// Retained, body-free notification state for the recipient-owned explicit-admission loop.
/// `registration_ready` is latched only after thread-ready/resume lifecycle work completes;
/// `revision` only advances the watch and is never an admission or queue generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExplicitActivitySignal {
    pub(crate) registration_ready: bool,
    pub(crate) revision: u64,
}

/// Body-free provenance attached only to a durably admitted explicit update.
/// `generation` is local to this input queue and distinct from the persisted update identity. A
/// missing generation fails closed for wake delivery while retaining the queued communication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExplicitMailboxActivity {
    pub(crate) update_id: ResponseItemId,
    pub(crate) intent: InterAgentCommunicationIntent,
    pub(crate) sender_thread_id: ThreadId,
    pub(crate) recipient_thread_id: ThreadId,
    pub(crate) sender_path: AgentPath,
    pub(crate) recipient_path: AgentPath,
    pub(crate) generation: Option<u64>,
}

/// Turn-local pending input storage owned by the input queue flow.
#[derive(Default)]
pub(crate) struct TurnInputQueue {
    items: Vec<TurnInput>,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    explicit_activity_tx: watch::Sender<ExplicitActivitySignal>,
    controller: Option<(ThreadId, Arc<dyn AgentControl>, watch::Receiver<bool>)>,
    mailbox_state: Mutex<MailboxQueueState>,
}

struct MailboxQueueState {
    pending: VecDeque<PendingMailboxCommunication>,
    next_local_enqueue_ordinal: Option<u64>,
    pending_explicit_admission_hints: HashSet<ResponseItemId>,
    completed_root_witnesses: HashMap<ResponseItemId, Weak<CodexThread>>,
}

pub(crate) struct PendingMailboxCommunication {
    pub(crate) communication: InterAgentCommunication,
    explicit_activity: Option<ExplicitMailboxActivity>,
    start_options: TurnStartOptions,
    local_enqueue_ordinal: Option<u64>,
    _diagnostics_guard: GaugeGuard,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        let (explicit_activity_tx, _) = watch::channel(ExplicitActivitySignal::default());
        Self {
            activity_tx,
            explicit_activity_tx,
            controller: None,
            mailbox_state: Mutex::new(MailboxQueueState {
                pending: VecDeque::new(),
                next_local_enqueue_ordinal: Some(0),
                pending_explicit_admission_hints: HashSet::new(),
                completed_root_witnesses: HashMap::new(),
            }),
        }
    }

    pub(crate) fn with_controller(thread_id: ThreadId, control: Arc<dyn AgentControl>) -> Self {
        let updates = control.watch_mailbox(thread_id);
        Self {
            controller: Some((thread_id, control, updates)),
            ..Self::new()
        }
    }

    pub(crate) fn mailbox_updates(&self) -> Option<watch::Receiver<bool>> {
        self.controller
            .as_ref()
            .map(|(_, _, updates)| updates.clone())
    }

    pub(crate) async fn notify_mailbox(&self) {
        if self.has_trigger_turn_mailbox_items().await {
            self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        }
    }

    fn read_mailbox(&self, pending: &mut VecDeque<PendingMailboxCommunication>) {
        if let Some((thread_id, control, _)) = &self.controller {
            pending.extend(
                control
                    .take_mailbox(*thread_id)
                    .into_iter()
                    .map(|communication| PendingMailboxCommunication {
                        communication,
                        explicit_activity: None,
                        start_options: TurnStartOptions::default(),
                        local_enqueue_ordinal: None,
                        _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
                    }),
            );
        }
    }

    pub(crate) async fn subscribe_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let activity_rx = self.activity_tx.subscribe();
        let turn_activity = if let Some(turn_state) = turn_state {
            turn_state.lock().await.pending_input.pending_activity()
        } else {
            None
        };
        let pending_activity = if let Some(activity) = turn_activity {
            Some(activity)
        } else if self.has_trigger_turn_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        };
        (activity_rx, pending_activity)
    }

    /// Subscribe before `WaitAgent` snapshots pending mail; the receiver preserves any arrival
    /// across that queue snapshot.
    pub(crate) fn subscribe_explicit_activity(&self) -> watch::Receiver<ExplicitActivitySignal> {
        self.explicit_activity_tx.subscribe()
    }

    /// Notify the recipient loop to rescan durable explicit admissions. The notification carries
    /// no action identity; the retained rollout and current registration checks remain authority.
    pub(crate) fn notify_explicit_admission_rescan(&self) {
        self.explicit_activity_tx.send_modify(|signal| {
            signal.revision = signal.revision.wrapping_add(1);
        });
    }

    /// Latch that manager registration and ready/resume lifecycle work completed. This is a
    /// rescan condition, not authority to deliver or start a turn by itself.
    pub(crate) fn notify_explicit_recipient_ready(&self) {
        self.explicit_activity_tx.send_modify(|signal| {
            signal.registration_ready = true;
            signal.revision = signal.revision.wrapping_add(1);
        });
    }

    /// Record an in-flight durable admission before its cancellable append/flush awaits. This is
    /// only a rescan hint; a later canonical ledger read decides whether anything was admitted.
    pub(crate) async fn begin_explicit_admission_hint(
        &self,
        update_id: ResponseItemId,
        completed_root_witness: Option<&Arc<CodexThread>>,
    ) {
        let mut state = self.mailbox_state.lock().await;
        state
            .pending_explicit_admission_hints
            .insert(update_id.clone());
        if let Some(witness) = completed_root_witness {
            state
                .completed_root_witnesses
                .insert(update_id, Arc::downgrade(witness));
        }
        drop(state);
        self.notify_explicit_admission_rescan();
    }

    /// Resolve only hints whose status is established by a checked flush and canonical readback.
    /// Uncommitted hints lose any ephemeral Completed-root witness; committed witnesses remain
    /// available only until representation or supersession consumes the queued update.
    pub(crate) async fn finish_explicit_admission_reconciliation(
        &self,
        committed_update_ids: &HashSet<ResponseItemId>,
        unavailable_update_ids: &HashSet<ResponseItemId>,
    ) {
        let mut state = self.mailbox_state.lock().await;
        let hints = std::mem::take(&mut state.pending_explicit_admission_hints);
        for update_id in hints {
            if !committed_update_ids.contains(&update_id) {
                state.completed_root_witnesses.remove(&update_id);
            }
        }
        for update_id in unavailable_update_ids {
            state.completed_root_witnesses.remove(update_id);
        }
    }

    pub(crate) async fn forget_completed_root_witness(&self, update_id: &ResponseItemId) {
        self.mailbox_state
            .lock()
            .await
            .completed_root_witnesses
            .remove(update_id);
    }

    pub(crate) async fn completed_root_witness(
        &self,
        update_id: &ResponseItemId,
    ) -> Option<Arc<CodexThread>> {
        self.mailbox_state
            .lock()
            .await
            .completed_root_witnesses
            .get(update_id)
            .and_then(Weak::upgrade)
    }

    pub(crate) async fn pending_actionable_explicit_update_ids(
        &self,
        recipient_thread_id: ThreadId,
        recipient_path: Option<&AgentPath>,
    ) -> Vec<ResponseItemId> {
        let Some(recipient_path) = recipient_path else {
            return Vec::new();
        };
        let mut state = self.mailbox_state.lock().await;
        self.read_mailbox(&mut state.pending);
        state
            .pending
            .iter()
            .filter_map(|mail| {
                let activity = mail.explicit_activity.as_ref()?;
                (activity.recipient_thread_id == recipient_thread_id
                    && &activity.recipient_path == recipient_path
                    && matches!(
                        activity.intent,
                        InterAgentCommunicationIntent::ActionRequired
                            | InterAgentCommunicationIntent::ResultReady
                    ))
                .then(|| activity.update_id.clone())
            })
            .collect()
    }

    pub(crate) async fn pending_explicit_activity_for_recipient(
        &self,
        recipient_thread_id: ThreadId,
        recipient_path: Option<&AgentPath>,
    ) -> Option<ExplicitMailboxActivity> {
        let recipient_path = recipient_path?;
        let mut state = self.mailbox_state.lock().await;
        self.read_mailbox(&mut state.pending);
        state.pending.iter().find_map(|mail| {
            let activity = mail.explicit_activity.as_ref()?;
            if activity.recipient_thread_id != recipient_thread_id
                || &activity.recipient_path != recipient_path
                || !matches!(
                    activity.intent,
                    InterAgentCommunicationIntent::ActionRequired
                        | InterAgentCommunicationIntent::ResultReady
                )
            {
                return None;
            }
            activity.generation.map(|_| activity.clone())
        })
    }

    pub(crate) async fn pending_turn_input_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> Option<InputQueueActivity> {
        match turn_state {
            Some(turn_state) => turn_state.lock().await.pending_input.pending_activity(),
            None => None,
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn deliver_mailbox_communication_to_current_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        communication: InterAgentCommunication,
        expected_turn_id: Option<&str>,
    ) -> bool {
        let active = active_turn.lock().await;
        let Some(active_turn) = active.as_ref().filter(|turn| {
            turn.task.as_ref().is_some_and(|task| {
                expected_turn_id.is_none_or(|id| {
                    task.turn_context.sub_id == id && !task.cancellation_token.is_cancelled()
                })
            })
        }) else {
            return false;
        };
        let mut turn_state = active_turn.turn_state.lock().await;
        if !turn_state.accepts_mailbox_delivery_for_current_turn() {
            return false;
        }
        let trigger_turn = communication.trigger_turn;
        turn_state
            .pending_input
            .items
            .push(TurnInput::InterAgentCommunication(communication));
        if trigger_turn {
            self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        }
        true
    }

    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) {
        self.enqueue_mailbox_communication_with_activity(
            communication,
            start_options,
            /*explicit_activity*/ None,
        )
        .await;
    }

    pub(crate) async fn enqueue_explicit_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
        explicit_activity: ExplicitMailboxActivity,
    ) {
        self.enqueue_mailbox_communication_with_activity(
            communication,
            start_options,
            Some(explicit_activity),
        )
        .await;
    }

    async fn enqueue_mailbox_communication_with_activity(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
        mut explicit_activity: Option<ExplicitMailboxActivity>,
    ) {
        let mut state = self.mailbox_state.lock().await;
        // Mail retained while unloaded precedes new submissions to the loaded session.
        self.read_mailbox(&mut state.pending);
        let local_enqueue_ordinal = state.next_local_enqueue_ordinal;
        state.next_local_enqueue_ordinal = state
            .next_local_enqueue_ordinal
            .and_then(|ordinal| ordinal.checked_add(1));
        if let Some(activity) = explicit_activity.as_mut() {
            activity.generation = local_enqueue_ordinal;
        }
        state.pending.push_back(PendingMailboxCommunication {
            communication,
            explicit_activity: explicit_activity.clone(),
            start_options,
            local_enqueue_ordinal,
            _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
        });
        if let Some(activity) = explicit_activity.as_ref().filter(|activity| {
            matches!(
                activity.intent,
                InterAgentCommunicationIntent::ActionRequired
                    | InterAgentCommunicationIntent::ResultReady
            )
        }) && activity.generation.is_some()
        {
            self.explicit_activity_tx.send_modify(|signal| {
                signal.revision = signal.revision.wrapping_add(1);
            });
        }
        if state
            .pending
            .iter()
            .any(|mail| mail.communication.trigger_turn)
        {
            self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        }
    }

    /// Remove only still-pending entries created by the durable explicit-admission path.
    /// Public/legacy messages with the same ID or payload have no such provenance and remain.
    pub(crate) async fn remove_pending_explicit_updates(
        &self,
        update_ids: &std::collections::HashSet<ResponseItemId>,
    ) -> std::collections::HashSet<ResponseItemId> {
        if update_ids.is_empty() {
            return std::collections::HashSet::new();
        }
        let mut state = self.mailbox_state.lock().await;
        let mut removed = std::collections::HashSet::new();
        state.pending.retain(|mail| {
            let Some(activity) = mail.explicit_activity.as_ref() else {
                return true;
            };
            if update_ids.contains(&activity.update_id) {
                removed.insert(activity.update_id.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        if !self.mailbox_state.lock().await.pending.is_empty() {
            return true;
        }
        self.controller
            .as_ref()
            .is_some_and(|(_, _, updates)| *updates.borrow())
    }

    /// Capture a local enqueue watermark before an agent wait subscribes to activity.
    pub(crate) async fn mailbox_enqueue_watermark(&self) -> Option<u64> {
        // The watermark and pending queue share one lock with enqueue, so it
        // cannot split an enqueue's ordinal assignment from its queue insert.
        self.mailbox_state.lock().await.next_local_enqueue_ordinal
    }

    /// Count newly enqueued, still-pending local communications without draining them. A host
    /// controller exposes only a pending bit, so its contribution makes the count unknown.
    pub(crate) async fn pending_mailbox_communication_count_since(
        &self,
        watermark: Option<u64>,
    ) -> Option<u32> {
        let controller_pending_before = self
            .controller
            .as_ref()
            .is_some_and(|(_, _, updates)| *updates.borrow());
        let state = self.mailbox_state.lock().await;
        let local_count = watermark.and_then(|watermark| {
            let mut count = 0usize;
            for mail in state.pending.iter() {
                let ordinal = mail.local_enqueue_ordinal?;
                if ordinal >= watermark && !mail.communication.trigger_turn {
                    count = count.checked_add(1)?;
                }
            }
            Some(count)
        });
        let controller_pending_after = self
            .controller
            .as_ref()
            .is_some_and(|(_, _, updates)| *updates.borrow());
        observed_pending_mail_count(
            local_count,
            controller_pending_before || controller_pending_after,
        )
    }

    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        let mut state = self.mailbox_state.lock().await;
        // The controller's pending bit includes queue-only mail. Transfer it to the
        // existing queue before classifying wake intent, preserving delivery order.
        self.read_mailbox(&mut state.pending);
        state
            .pending
            .iter()
            .any(|mail| mail.communication.trigger_turn)
    }

    pub(crate) async fn drain_mailbox(&self) -> Vec<PendingMailboxCommunication> {
        let mut state = self.mailbox_state.lock().await;
        self.read_mailbox(&mut state.pending);
        state.pending.drain(..).collect()
    }

    pub(crate) async fn drain_mailbox_input_items(&self) -> (Vec<TurnInput>, TurnStartOptions) {
        let pending_mails = self.drain_mailbox().await;
        // A later follow-up supersedes the earlier choice, including an omitted choice.
        let mut start_options = pending_mails
            .iter()
            .rev()
            .find(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.start_options.clone())
            .unwrap_or_default();
        start_options.parent_turn_id = pending_mails
            .iter()
            .filter(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.start_options.parent_turn_id.as_deref())
            .reduce(|expected, candidate| expected.filter(|id| candidate == Some(*id)))
            .and_then(|id| id.filter(|id| !id.trim().is_empty()).map(str::to_string));
        start_options.root_turn_id = pending_mails
            .iter()
            .find(|mail| mail.communication.trigger_turn)
            .and_then(|mail| {
                mail.start_options
                    .parent_turn_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .and(mail.start_options.root_turn_id.as_deref())
                    .filter(|id| !id.trim().is_empty())
            })
            .map(str::to_string);
        let items = pending_mails
            .into_iter()
            .map(|mail| TurnInput::InterAgentCommunication(mail.communication))
            .collect();
        (items, start_options)
    }

    pub(crate) async fn turn_state_for_sub_id(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) -> Option<Arc<Mutex<TurnState>>> {
        let active = active_turn.lock().await;
        active.as_ref().and_then(|active_turn| {
            active_turn
                .task
                .as_ref()
                .is_some_and(|task| task.turn_context.sub_id == sub_id)
                .then(|| Arc::clone(&active_turn.turn_state))
        })
    }

    /// Signal once a user message is queued for this sampling request.
    pub(crate) async fn watch_user_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
        interrupt: CancellationToken,
    ) -> Option<AbortOnDropHandle<()>> {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await?;
        // Subscribe before inspecting the queue so an arrival cannot be missed.
        let mut activity = self.activity_tx.subscribe();
        Some(AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                if turn_state.lock().await.pending_input.has_user_input() {
                    interrupt.cancel();
                    return;
                }
                if activity.changed().await.is_err() {
                    return;
                }
            }
        })))
    }

    /// Clear any pending waiters and input buffered for the current turn.
    pub(crate) async fn clear_pending(&self, active_turn: &ActiveTurn) {
        let mut turn_state = active_turn.turn_state.lock().await;
        turn_state.clear_pending_waiters();
        turn_state.pending_input.items.clear();
    }

    pub(crate) async fn defer_mailbox_delivery_to_next_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        let mut turn_state = turn_state.lock().await;
        // Explicit same-turn work still needs a follow-up. Queue-only child mail does not: keep
        // it pending so task completion records it for the next turn without sampling again.
        if turn_state.pending_input.items.iter().any(|input| {
            !matches!(
                input,
                TurnInput::InterAgentCommunication(communication) if !communication.trigger_turn
            )
        }) {
            return;
        }
        turn_state.set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);
    }

    pub(crate) async fn accept_mailbox_delivery_for_current_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        self.accept_mailbox_delivery_for_turn_state(turn_state.as_ref())
            .await;
    }

    pub(super) async fn accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) {
        turn_state
            .lock()
            .await
            .accept_mailbox_delivery_for_current_turn();
    }

    pub(super) async fn extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        {
            let mut turn_state = turn_state.lock().await;
            turn_state.pending_input.items.extend(input);
            turn_state.accept_mailbox_delivery_for_current_turn();
        }
        self.activity_tx.send_replace(InputQueueActivity::Steer);
    }

    #[cfg(test)]
    pub(crate) async fn notify_operator_steer_for_test(&self, turn_state: &Mutex<TurnState>) {
        self.extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
            turn_state,
            vec![TurnInput::UserInput {
                content: Vec::new(),
                client_id: None,
                metadata: Default::default(),
            }],
        )
        .await;
    }

    pub(crate) async fn extend_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        turn_state.lock().await.pending_input.items.extend(input);
    }

    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        turn_state.lock().await.pending_input.items.split_off(0)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> (Vec<TurnInput>, TurnStartOptions) {
        let (pending_input, accepts_mailbox_delivery) = {
            let mut active = active_turn.lock().await;
            match active.as_mut() {
                Some(active_turn) => {
                    let mut turn_state = active_turn.turn_state.lock().await;
                    let accepts_mailbox_delivery =
                        turn_state.accepts_mailbox_delivery_for_current_turn();
                    let pending_input = if accepts_mailbox_delivery {
                        turn_state.pending_input.items.split_off(0)
                    } else {
                        Vec::new()
                    };
                    (pending_input, accepts_mailbox_delivery)
                }
                None => (Vec::new(), true),
            }
        };
        if !accepts_mailbox_delivery {
            return (pending_input, TurnStartOptions::default());
        }
        let (mailbox_items, start_options) = self.drain_mailbox_input_items().await;
        if pending_input.is_empty() {
            (mailbox_items, start_options)
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            (pending_input, start_options)
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn has_pending_input(&self, active_turn: &Mutex<Option<ActiveTurn>>) -> bool {
        let (has_turn_pending_input, accepts_mailbox_delivery) = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => {
                    let turn_state = active_turn.turn_state.lock().await;
                    (
                        !turn_state.pending_input.is_empty(),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                    )
                }
                None => (false, true),
            }
        };
        if !accepts_mailbox_delivery {
            return false;
        }
        if has_turn_pending_input {
            return true;
        }
        self.has_pending_mailbox_items().await
    }
}

fn observed_pending_mail_count(
    local_count: Option<usize>,
    controller_pending: bool,
) -> Option<u32> {
    if controller_pending {
        return None;
    }
    u32::try_from(local_count?).ok()
}

impl TurnInputQueue {
    fn has_user_input(&self) -> bool {
        self.items
            .iter()
            .any(|input| matches!(input, TurnInput::UserInput { .. }))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn pending_activity(&self) -> Option<InputQueueActivity> {
        if self.items.iter().any(|input| {
            matches!(
                input,
                TurnInput::UserInput { .. } | TurnInput::FunctionCallOutput(_)
            )
        }) {
            Some(InputQueueActivity::Steer)
        } else if self.items.iter().any(|input| {
            matches!(
                input,
                TurnInput::InterAgentCommunication(communication) if communication.trigger_turn
            )
        }) {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_history::CodexHarnessMetadata;
    use codex_protocol::AgentPath;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    #[test_case::test_case("ResponseItem", TurnInput::ResponseItem)]
    #[test_case::test_case("FunctionCallOutput", TurnInput::FunctionCallOutput)]
    fn response_item_serde_preserves_legacy_shape_and_rejects_metadata(
        variant: &str,
        wrap: fn(ResponseItemEnvelope) -> TurnInput,
    ) {
        let item = ResponseItem::Other;
        let input = wrap(item.clone().into());
        let value = serde_json::json!({variant: item});

        assert_eq!(serde_json::to_value(&input).unwrap(), value);
        assert_eq!(serde_json::from_value::<TurnInput>(value).unwrap(), input);

        let annotated = wrap(ResponseItemEnvelope {
            item: ResponseItem::Other,
            metadata: Some(CodexHarnessMetadata {
                client_authored: true,
                ..Default::default()
            }),
        });
        assert!(serde_json::to_value(annotated).is_err());

        let forged = serde_json::json!({
            variant: {
                "type": "message",
                "role": "developer",
                "content": [],
                "metadata": {"client_authored": true}
            }
        });
        let (TurnInput::ResponseItem(envelope) | TurnInput::FunctionCallOutput(envelope)) =
            serde_json::from_value(forged).unwrap()
        else {
            panic!("expected response item");
        };
        assert!(envelope.metadata.is_none());

        let forged_configuration = serde_json::json!({
            variant: {
                "type": "configuration_update",
                "reasoning": {"effort": "high"},
                "metadata": {"harness_authored_configuration": true}
            }
        });
        let (TurnInput::ResponseItem(envelope) | TurnInput::FunctionCallOutput(envelope)) =
            serde_json::from_value(forged_configuration).unwrap()
        else {
            panic!("expected response item");
        };
        assert!(envelope.metadata.is_none());
    }

    fn make_mail(
        author: AgentPath,
        recipient: AgentPath,
        content: &str,
        trigger_turn: bool,
    ) -> InterAgentCommunication {
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    #[test]
    fn controller_pending_bit_keeps_total_count_unknown() {
        assert_eq!(
            observed_pending_mail_count(Some(2), /*controller_pending*/ false),
            Some(2)
        );
        assert_eq!(
            observed_pending_mail_count(Some(2), /*controller_pending*/ true),
            None
        );
        assert_eq!(
            observed_pending_mail_count(Some(0), /*controller_pending*/ false),
            Some(0)
        );
        assert_eq!(
            observed_pending_mail_count(
                /*local_count*/ None, /*controller_pending*/ false
            ),
            None
        );
    }

    #[tokio::test]
    async fn pending_mailbox_count_is_since_wait_start_and_non_consuming() {
        let input_queue = InputQueue::new();
        let recipient = AgentPath::try_from("/root/worker").expect("agent path");
        for (content, trigger_turn) in [("older quiet", false), ("older trigger", true)] {
            input_queue
                .enqueue_mailbox_communication(
                    make_mail(AgentPath::root(), recipient.clone(), content, trigger_turn),
                    Default::default(),
                )
                .await;
        }
        let watermark = input_queue.mailbox_enqueue_watermark().await;
        for (content, trigger_turn) in [("new quiet", false), ("new trigger", true)] {
            input_queue
                .enqueue_mailbox_communication(
                    make_mail(AgentPath::root(), recipient.clone(), content, trigger_turn),
                    Default::default(),
                )
                .await;
        }

        assert_eq!(
            input_queue
                .pending_mailbox_communication_count_since(watermark)
                .await,
            Some(1)
        );
        assert!(input_queue.has_pending_mailbox_items().await);
        assert_eq!(input_queue.drain_mailbox().await.len(), 4);
        assert_eq!(
            input_queue
                .pending_mailbox_communication_count_since(watermark)
                .await,
            Some(0)
        );
    }

    #[tokio::test]
    async fn mailbox_count_is_unknown_after_ordinal_overflow() {
        let input_queue = InputQueue::new();
        input_queue
            .mailbox_state
            .lock()
            .await
            .next_local_enqueue_ordinal = Some(u64::MAX - 1);
        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    "last numbered update",
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        let watermark = input_queue.mailbox_enqueue_watermark().await;
        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    "final ordinal update",
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    "unnumbered update",
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        assert_eq!(
            input_queue
                .pending_mailbox_communication_count_since(watermark)
                .await,
            None
        );
    }

    #[tokio::test]
    async fn input_queue_notifies_mailbox_subscribers() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending_activity, None);

        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_one, Default::default())
            .await;
        assert!(!activity_rx.has_changed().expect("mailbox subscription"));
        assert!(input_queue.has_pending_mailbox_items().await);
        assert_eq!(
            input_queue.subscribe_activity(/*turn_state*/ None).await.1,
            None
        );
        let mail_two = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(mail_two, Default::default())
            .await;

        activity_rx.changed().await.expect("mailbox update");
        assert_eq!(
            *activity_rx.borrow_and_update(),
            InputQueueActivity::Mailbox
        );
    }

    #[tokio::test]
    async fn input_queue_notifies_steer_subscribers() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;
        assert_eq!(pending_activity, None);
        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    "quiet update",
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        assert!(!activity_rx.has_changed().expect("quiet mail stays pending"));

        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    metadata: Default::default(),
                    content: vec![UserInput::Text {
                        text: "steer".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        activity_rx.changed().await.expect("steer update");
        assert_eq!(*activity_rx.borrow_and_update(), InputQueueActivity::Steer);
        assert_eq!(input_queue.drain_mailbox().await.len(), 1);
    }

    #[tokio::test]
    async fn input_queue_reports_already_pending_steer() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let passive_output = serde_json::from_value(serde_json::json!({
            "ResponseItem": {"type": "function_call_output", "name": "notify", "output": "passive"}
        }))
        .unwrap();
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, vec![passive_output])
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            None
        );
        let communication = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "already pending mail",
            /*trigger_turn*/ false,
        );
        input_queue
            .extend_pending_input_for_turn_state(
                &turn_state,
                vec![TurnInput::InterAgentCommunication(communication)],
            )
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            None
        );
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    metadata: Default::default(),
                    content: vec![UserInput::Text {
                        text: "already pending".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        let (_activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;

        assert_eq!(pending_activity, Some(InputQueueActivity::Steer));
    }

    #[tokio::test]
    async fn input_queue_drains_mailbox_in_delivery_order() {
        let input_queue = InputQueue::new();
        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mail_two = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ true,
        );

        input_queue
            .enqueue_mailbox_communication(mail_one.clone(), Default::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone(), Default::default())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await.0,
            vec![
                TurnInput::InterAgentCommunication(mail_one),
                TurnInput::InterAgentCommunication(mail_two)
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_uses_unambiguous_trigger_parent_and_first_root() {
        let (parent, peer, root, root2) = (Some("a"), Some("b"), Some("r"), Some("s"));
        for (pending_mails, expected_parent_turn_id, expected_root_turn_id) in [
            (Vec::new(), None, None),
            (vec![(false, Some("q"), root)], None, None),
            (vec![(true, Some(""), root)], None, None),
            (vec![(true, Some("   "), root)], None, None),
            (vec![(true, None, root)], None, None),
            (vec![(true, parent, None)], parent, None),
            (vec![(true, parent, Some(""))], parent, None),
            (vec![(true, parent, root), (true, peer, root)], None, root),
            (vec![(true, parent, root), (true, peer, root2)], None, root),
            (vec![(true, parent, root), (true, None, root)], None, root),
            (
                vec![(true, parent, root), (true, parent, root)],
                parent,
                root,
            ),
            (
                vec![(false, Some("q"), root2), (true, parent, root)],
                parent,
                root,
            ),
        ] {
            let input_queue = InputQueue::new();
            for (trigger_turn, parent_turn_id, root_turn_id) in pending_mails {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        TurnStartOptions {
                            parent_turn_id: parent_turn_id.map(str::to_string),
                            root_turn_id: root_turn_id.map(str::to_string),
                            ..Default::default()
                        },
                    )
                    .await;
            }
            let (_, start_options) = input_queue.drain_mailbox_input_items().await;
            assert_eq!(
                start_options.parent_turn_id.as_deref(),
                expected_parent_turn_id
            );
            assert_eq!(start_options.root_turn_id.as_deref(), expected_root_turn_id);
        }
    }

    #[tokio::test]
    async fn input_queue_uses_latest_followup_choice_and_ignores_queue_only_mail() {
        use codex_protocol::turn_input::CyberAccessProgram;

        for latest in [Some(CyberAccessProgram::Standard), None] {
            let input_queue = InputQueue::new();
            for (trigger_turn, program) in [
                (true, Some(CyberAccessProgram::DaybreakBlue)),
                (true, latest),
                (false, Some(CyberAccessProgram::DaybreakRed)),
            ] {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        TurnStartOptions {
                            cyber_access_program: program,
                            ..Default::default()
                        },
                    )
                    .await;
            }
            let (_, start_options) = input_queue.drain_mailbox_input_items().await;
            assert_eq!(start_options.cyber_access_program, latest);
        }
    }

    #[tokio::test]
    async fn input_queue_tracks_pending_trigger_turn_mail() {
        let input_queue = InputQueue::new();

        let queued_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "queued",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(queued_mail, Default::default())
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        let trigger_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "wake",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(trigger_mail, Default::default())
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }

    fn explicit_test_activity(
        update_id: ResponseItemId,
        intent: InterAgentCommunicationIntent,
        sender_thread_id: ThreadId,
        recipient_thread_id: ThreadId,
    ) -> ExplicitMailboxActivity {
        ExplicitMailboxActivity {
            update_id,
            intent,
            sender_thread_id,
            recipient_thread_id,
            sender_path: AgentPath::try_from("/root/worker").expect("sender path"),
            recipient_path: AgentPath::root(),
            generation: None,
        }
    }

    fn explicit_test_mail(update_id: ResponseItemId) -> InterAgentCommunication {
        let mut communication = InterAgentCommunication::new(
            AgentPath::try_from("/root/worker").expect("sender path"),
            AgentPath::root(),
            Vec::new(),
            "private message body is not copied into activity".to_string(),
            /*trigger_turn*/ false,
        );
        communication.id = Some(update_id);
        communication
    }

    #[tokio::test]
    async fn explicit_action_snapshot_is_targeted_and_subscribe_before_snapshot_is_wake_safe() {
        let queue = InputQueue::new();
        let sender = ThreadId::new();
        let recipient = ThreadId::new();
        let unrelated = ThreadId::new();
        let sender_path = AgentPath::try_from("/root/worker").expect("sender path");
        let update_id = ResponseItemId::new("explicit-action-1");
        let (legacy_activity_rx, _) = queue.subscribe_activity(/*turn_state*/ None).await;
        let mut explicit_activity_rx = queue.subscribe_explicit_activity();
        let pending = queue
            .pending_explicit_activity_for_recipient(recipient, Some(&AgentPath::root()))
            .await;
        assert_eq!(pending, None);

        queue
            .enqueue_explicit_mailbox_communication(
                explicit_test_mail(update_id.clone()),
                Default::default(),
                explicit_test_activity(
                    update_id.clone(),
                    InterAgentCommunicationIntent::ActionRequired,
                    sender,
                    recipient,
                ),
            )
            .await;

        explicit_activity_rx
            .changed()
            .await
            .expect("explicit action notification");
        let signal = *explicit_activity_rx.borrow_and_update();
        assert!(!signal.registration_ready);
        assert_eq!(signal.revision, 1);
        assert!(
            !legacy_activity_rx
                .has_changed()
                .expect("legacy activity channel remains open"),
            "queue-only explicit actions do not wake shared mailbox or Sleep waiters"
        );
        let pending = queue
            .pending_explicit_activity_for_recipient(recipient, Some(&AgentPath::root()))
            .await
            .expect("matching pending explicit action");
        assert_eq!(pending.update_id, update_id);
        assert_eq!(
            pending.intent,
            InterAgentCommunicationIntent::ActionRequired
        );
        assert_eq!(pending.sender_thread_id, sender);
        assert_eq!(pending.recipient_thread_id, recipient);
        assert_eq!(pending.sender_path, sender_path);
        assert_eq!(pending.generation, Some(0));
        assert!(
            queue
                .pending_explicit_activity_for_recipient(unrelated, Some(&AgentPath::root()))
                .await
                .is_none()
        );
        assert!(
            queue
                .pending_explicit_activity_for_recipient(
                    ThreadId::new(),
                    Some(&AgentPath::root()),
                )
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn explicit_rescan_state_survives_late_subscription_without_waking() {
        let queue = InputQueue::new();
        queue.notify_explicit_admission_rescan();

        let mut early_activity_rx = queue.subscribe_explicit_activity();
        let hint = *early_activity_rx.borrow_and_update();
        assert!(!hint.registration_ready);
        assert_eq!(hint.revision, 1);
        queue.notify_explicit_admission_rescan();
        early_activity_rx
            .changed()
            .await
            .expect("rescan notification is retained for an existing subscriber");
        assert!(!early_activity_rx.borrow_and_update().registration_ready);
        drop(early_activity_rx);

        // Both state changes happen with no receiver, as on a late-created submission loop.
        queue.notify_explicit_recipient_ready();
        queue.notify_explicit_admission_rescan();
        let mut late_activity_rx = queue.subscribe_explicit_activity();
        let late_signal = *late_activity_rx.borrow_and_update();
        assert!(late_signal.registration_ready);
        assert_eq!(late_signal.revision, 4);
        assert!(
            queue
                .pending_explicit_activity_for_recipient(
                    ThreadId::new(),
                    Some(&AgentPath::root()),
                )
                .await
                .is_none(),
            "a rescan hint without an addressed actionable item is not a wake"
        );
    }

    #[tokio::test]
    async fn explicit_snapshot_catches_preexisting_matching_action_but_not_progress() {
        let queue = InputQueue::new();
        let sender = ThreadId::new();
        let recipient = ThreadId::new();
        let sender_path = AgentPath::try_from("/root/worker").expect("sender path");
        let action_id = ResponseItemId::new("explicit-action-pending");
        let progress_id = ResponseItemId::new("explicit-progress-pending");
        queue
            .enqueue_explicit_mailbox_communication(
                explicit_test_mail(progress_id.clone()),
                Default::default(),
                explicit_test_activity(
                    progress_id,
                    InterAgentCommunicationIntent::Progress,
                    sender,
                    recipient,
                ),
            )
            .await;
        queue
            .enqueue_explicit_mailbox_communication(
                explicit_test_mail(action_id.clone()),
                Default::default(),
                explicit_test_activity(
                    action_id.clone(),
                    InterAgentCommunicationIntent::ResultReady,
                    sender,
                    recipient,
                ),
            )
            .await;

        let _explicit_activity_rx = queue.subscribe_explicit_activity();
        let pending = queue
            .pending_explicit_activity_for_recipient(recipient, Some(&AgentPath::root()))
            .await;
        assert_eq!(pending.map(|activity| activity.update_id), Some(action_id));
    }

    #[tokio::test]
    async fn explicit_snapshot_rejects_a_stale_registered_recipient_path() {
        let queue = InputQueue::new();
        let sender = ThreadId::new();
        let recipient = ThreadId::new();
        let update_id = ResponseItemId::new("recipient-path-mismatch");
        queue
            .enqueue_explicit_mailbox_communication(
                explicit_test_mail(update_id.clone()),
                Default::default(),
                explicit_test_activity(
                    update_id.clone(),
                    InterAgentCommunicationIntent::ActionRequired,
                    sender,
                    recipient,
                ),
            )
            .await;

        let replacement_path = AgentPath::try_from("/root/replacement").expect("valid path");
        assert!(
            queue
                .pending_explicit_activity_for_recipient(recipient, Some(&replacement_path))
                .await
                .is_none(),
            "a queued identity must not wake a different current registry path"
        );
        assert_eq!(
            queue
                .pending_explicit_activity_for_recipient(recipient, Some(&AgentPath::root()))
                .await
                .map(|activity| activity.update_id),
            Some(update_id)
        );
    }

    #[tokio::test]
    async fn pruning_explicit_entries_does_not_remove_legacy_message_with_same_id() {
        let queue = InputQueue::new();
        let sender = ThreadId::new();
        let recipient = ThreadId::new();
        let update_id = ResponseItemId::new("shared-public-id");
        let explicit_mail = explicit_test_mail(update_id.clone());
        queue
            .enqueue_explicit_mailbox_communication(
                explicit_mail.clone(),
                Default::default(),
                explicit_test_activity(
                    update_id.clone(),
                    InterAgentCommunicationIntent::Progress,
                    sender,
                    recipient,
                ),
            )
            .await;
        queue
            .enqueue_mailbox_communication(explicit_mail.clone(), Default::default())
            .await;

        let removed = queue
            .remove_pending_explicit_updates(&std::collections::HashSet::from([update_id.clone()]))
            .await;
        assert_eq!(removed, std::collections::HashSet::from([update_id]));
        assert_eq!(
            queue.drain_mailbox_input_items().await.0,
            vec![TurnInput::InterAgentCommunication(explicit_mail)]
        );
    }
}
