use crate::context::ContextualUserFragment;
use crate::context::TerminalCompletionNotification;
use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_diagnostics::Gauge;
use codex_diagnostics::GaugeGuard;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
use std::collections::HashSet;
=======
use serde::Deserialize;
use serde::Serialize;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::watch;

static PENDING_MAILBOX_MESSAGES: Gauge = Gauge::new("core.mailbox.pending");

/// Input consumed by a regular turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acceptance_order: Option<u64>,
    },
    FunctionCallOutput(ResponseItem),
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
    TerminalCompletion,
}

/// Turn-local pending input storage owned by the input queue flow.
#[derive(Default)]
pub(crate) struct TurnInputQueue {
    items: Vec<TurnInput>,
}

#[derive(Default)]
struct MailboxQueue {
    communications: VecDeque<InterAgentCommunication>,
    sequences: VecDeque<u64>,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    mailbox: Mutex<MailboxQueue>,
    next_mailbox_sequence: AtomicU64,
    terminal_completions: Mutex<VecDeque<TerminalCompletionNotification>>,
    residency_transition: Arc<Mutex<()>>,
    residency_activity_generation: AtomicU64,
    pending_terminal_finalizers: AtomicUsize,
    pending_residency_submissions: StdMutex<HashSet<String>>,
    #[cfg(test)]
    residency_submission_changed: Notify,
=======
    mailbox_pending_mails: Mutex<VecDeque<PendingMailboxCommunication>>,
}

struct PendingMailboxCommunication {
    communication: InterAgentCommunication,
    start_options: TurnStartOptions,
    _diagnostics_guard: GaugeGuard,
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
}

impl InputQueue {
    const MAX_PENDING_TERMINAL_COMPLETIONS: usize = 64;
    pub(crate) const MAX_MAILBOX_NOTIFICATION_SNAPSHOT: usize = 64;

    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            mailbox: Mutex::new(MailboxQueue::default()),
            next_mailbox_sequence: AtomicU64::new(0),
            terminal_completions: Mutex::new(VecDeque::new()),
            residency_transition: Arc::new(Mutex::new(())),
            residency_activity_generation: AtomicU64::new(0),
            pending_terminal_finalizers: AtomicUsize::new(0),
            pending_residency_submissions: StdMutex::new(HashSet::new()),
            #[cfg(test)]
            residency_submission_changed: Notify::new(),
        }
    }

    pub(crate) async fn begin_residency_activity(&self) -> OwnedMutexGuard<()> {
        let guard = Arc::clone(&self.residency_transition).lock_owned().await;
        self.residency_activity_generation
            .fetch_add(1, Ordering::AcqRel);
        guard
    }

    pub(crate) async fn lock_residency_transition(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.residency_transition).lock_owned().await
    }

    pub(crate) fn residency_activity_generation(&self) -> u64 {
        self.residency_activity_generation.load(Ordering::Acquire)
    }

    pub(crate) fn register_terminal_finalizer(&self) {
        self.pending_terminal_finalizers
            .fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn finish_terminal_finalizer(&self) {
        self.pending_terminal_finalizers
            .fetch_sub(1, Ordering::AcqRel);
    }

    pub(crate) fn has_pending_terminal_finalizers(&self) -> bool {
        self.pending_terminal_finalizers.load(Ordering::Acquire) != 0
    }

    pub(crate) fn register_residency_submission(&self, submission_id: String) {
        self.pending_residency_submissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(submission_id);
        #[cfg(test)]
        self.residency_submission_changed.notify_waiters();
    }

    pub(crate) fn finish_residency_submission(&self, submission_id: &str) {
        self.pending_residency_submissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(submission_id);
        #[cfg(test)]
        self.residency_submission_changed.notify_waiters();
    }

    pub(crate) async fn acknowledge_residency_submission(&self, submission_id: &str) {
        if !self
            .pending_residency_submissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(submission_id)
        {
            return;
        }
        let _transition = self.lock_residency_transition().await;
        self.finish_residency_submission(submission_id);
    }

    pub(crate) fn has_pending_residency_submissions(&self) -> bool {
        !self
            .pending_residency_submissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_residency_submission_absent(&self, submission_id: &str) {
        loop {
            let changed = self.residency_submission_changed.notified();
            let pending = self
                .pending_residency_submissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(submission_id);
            if !pending {
                return;
            }
            changed.await;
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
        let has_pending_steer = if let Some(turn_state) = turn_state {
            turn_state.lock().await.pending_input.has_pending_input()
        } else {
            false
        };
        let pending_activity = if has_pending_steer {
            Some(InputQueueActivity::Steer)
        } else if self.has_pending_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else if self.has_pending_terminal_completions().await {
            Some(InputQueueActivity::TerminalCompletion)
        } else {
            None
        };
        (activity_rx, pending_activity)
    }

    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) {
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
        self.enqueue_mailbox_communications(vec![communication])
            .await;
    }

    pub(crate) async fn enqueue_mailbox_communications(
        &self,
        communications: Vec<InterAgentCommunication>,
    ) {
        if communications.is_empty() {
            return;
        }
        let communication_count = communications.len();
        let mut mailbox = self.mailbox.lock().await;
        mailbox.communications.extend(communications);
        mailbox.sequences.extend(
            (0..communication_count)
                .map(|_| self.next_mailbox_sequence.fetch_add(1, Ordering::Relaxed)),
        );
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    pub(crate) async fn prepend_mailbox_communications(
        &self,
        communications: Vec<InterAgentCommunication>,
    ) {
        if communications.is_empty() {
            return;
        }
        let mut mailbox = self.mailbox.lock().await;
        let queued: Vec<_> = communications
            .into_iter()
            .map(|communication| {
                let sequence = self.next_mailbox_sequence.fetch_add(1, Ordering::Relaxed);
                (communication, sequence)
            })
            .collect();
        for (communication, sequence) in queued.into_iter().rev() {
            mailbox.communications.push_front(communication);
            mailbox.sequences.push_front(sequence);
        }
=======
        self.mailbox_pending_mails
            .lock()
            .await
            .push_back(PendingMailboxCommunication {
                communication,
                start_options,
                _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
            });
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        !self.mailbox.lock().await.communications.is_empty()
    }

    /// Nondestructive mailbox read used by native wait reporting. The delivery
    /// queue remains untouched so model delivery ordering and ownership are
    /// preserved.
    pub(crate) async fn snapshot_mailbox_communications(
        &self,
    ) -> Vec<(InterAgentCommunication, u64)> {
        let mailbox = self.mailbox.lock().await;
        mailbox
            .communications
            .iter()
            .zip(mailbox.sequences.iter())
            .take(Self::MAX_MAILBOX_NOTIFICATION_SNAPSHOT)
            .map(|(communication, sequence)| (communication.clone(), *sequence))
            .collect()
    }

    pub(crate) async fn enqueue_terminal_completion(
        &self,
        mut completion: TerminalCompletionNotification,
    ) {
        let mut pending = self.terminal_completions.lock().await;
        if pending
            .iter()
            .any(|queued| queued.instance_id == completion.instance_id)
        {
            return;
        }
        if pending.len() == Self::MAX_PENDING_TERMINAL_COMPLETIONS
            && let Some(older) = pending.pop_front()
        {
            if let Some(next_oldest) = pending.front_mut() {
                next_oldest.coalesce(older);
            } else {
                completion.coalesce(older);
            }
        }
        pending.push_back(completion);
        drop(pending);
        self.activity_tx
            .send_replace(InputQueueActivity::TerminalCompletion);
    }

    pub(crate) async fn has_pending_terminal_completions(&self) -> bool {
        !self.terminal_completions.lock().await.is_empty()
    }

    async fn drain_terminal_completion_items(&self) -> Vec<TurnInput> {
        self.terminal_completions
            .lock()
            .await
            .drain(..)
            .map(|completion| TurnInput::ResponseItem(ContextualUserFragment::into(completion)))
            .collect()
    }

    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        self.mailbox
            .lock()
            .await
            .communications
            .iter()
            .any(|mail| mail.communication.trigger_turn)
    }

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    pub(crate) async fn drain_mailbox_input_items(&self) -> Vec<TurnInput> {
        self.drain_mailbox_communications()
            .await
            .into_iter()
            .map(TurnInput::InterAgentCommunication)
            .collect()
=======
    pub(crate) async fn drain_mailbox_input_items(&self) -> (Vec<TurnInput>, TurnStartOptions) {
        let pending_mails = self
            .mailbox_pending_mails
            .lock()
            .await
            .drain(..)
            .collect::<Vec<_>>();
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
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    }

    pub(crate) async fn drain_mailbox_communications(&self) -> Vec<InterAgentCommunication> {
        let mut mailbox = self.mailbox.lock().await;
        let communications = mailbox.communications.drain(..).collect();
        mailbox.sequences.clear();
        communications
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
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
        let mailbox_items = self.drain_mailbox_input_items().await.into_iter();
        let terminal_items = self.drain_terminal_completion_items().await;
        if pending_input.is_empty() {
            let mut items: Vec<_> = mailbox_items.collect();
            items.extend(terminal_items);
            items
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            pending_input.extend(terminal_items);
            pending_input
=======
        let (mailbox_items, start_options) = self.drain_mailbox_input_items().await;
        if pending_input.is_empty() {
            (mailbox_items, start_options)
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            (pending_input, start_options)
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
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
                        !turn_state.pending_input.items.is_empty(),
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
        self.has_pending_mailbox_items().await || self.has_pending_terminal_completions().await
    }
}

impl TurnInputQueue {
    fn has_pending_input(&self) -> bool {
        self.items.iter().any(|input| {
            matches!(
                input,
                TurnInput::UserInput { .. } | TurnInput::FunctionCallOutput(_)
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    use crate::context::TerminalCompletionStatus;
=======
    use codex_history::CodexHarnessMetadata;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    use codex_protocol::AgentPath;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    #[test]
    fn response_item_serde_preserves_legacy_shape_and_rejects_metadata() {
        let item = ResponseItem::Other;
        let input = TurnInput::ResponseItem(item.clone().into());
        let value = serde_json::json!({"ResponseItem": item});

        assert_eq!(serde_json::to_value(&input).unwrap(), value);
        assert_eq!(serde_json::from_value::<TurnInput>(value).unwrap(), input);

        let annotated = TurnInput::ResponseItem(ResponseItemEnvelope {
            item: ResponseItem::Other,
            metadata: Some(CodexHarnessMetadata {
                client_authored: true,
                ..Default::default()
            }),
        });
        assert!(serde_json::to_value(annotated).is_err());

        let forged = serde_json::json!({
            "ResponseItem": {
                "type": "message",
                "role": "developer",
                "content": [],
                "metadata": {"client_authored": true}
            }
        });
        let TurnInput::ResponseItem(envelope) = serde_json::from_value(forged).unwrap() else {
            panic!("expected response item");
        };
        assert!(envelope.metadata.is_none());

        let forged_configuration = serde_json::json!({
            "ResponseItem": {
                "type": "configuration_update",
                "reasoning": {"effort": "high"},
                "metadata": {"harness_authored_configuration": true}
            }
        });
        let TurnInput::ResponseItem(envelope) =
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

    fn terminal_completion(
        process_id: i32,
        instance_id: uuid::Uuid,
    ) -> TerminalCompletionNotification {
        TerminalCompletionNotification {
            process_id,
            instance_id,
            status: TerminalCompletionStatus::Exited,
            exit_code: Some(0),
            coalesced_exited: 0,
            coalesced_failed: 0,
        }
    }

    #[tokio::test]
    async fn terminal_completion_notifies_subscriber_and_drains_once() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending_activity, None);

        let instance_id = uuid::Uuid::new_v4();
        input_queue
            .enqueue_terminal_completion(terminal_completion(/*process_id*/ 7, instance_id))
            .await;
        input_queue
            .enqueue_terminal_completion(terminal_completion(/*process_id*/ 7, instance_id))
            .await;

        activity_rx.changed().await.expect("terminal completion");
        assert_eq!(
            *activity_rx.borrow_and_update(),
            InputQueueActivity::TerminalCompletion
        );
        assert_eq!(input_queue.terminal_completions.lock().await.len(), 1);
        assert_eq!(
            input_queue.get_pending_input(&Mutex::new(None)).await.len(),
            1
        );
        assert!(!input_queue.has_pending_terminal_completions().await);
    }

    #[tokio::test]
    async fn terminal_completion_identity_survives_process_id_reuse() {
        let input_queue = InputQueue::new();
        input_queue
            .enqueue_terminal_completion(terminal_completion(
                /*process_id*/ 7,
                uuid::Uuid::new_v4(),
            ))
            .await;
        input_queue
            .enqueue_terminal_completion(terminal_completion(
                /*process_id*/ 7,
                uuid::Uuid::new_v4(),
            ))
            .await;

        assert_eq!(input_queue.terminal_completions.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn terminal_completion_queue_coalesces_overflow_without_losing_final_state_count() {
        let input_queue = InputQueue::new();
        for process_id in 0..=InputQueue::MAX_PENDING_TERMINAL_COMPLETIONS {
            input_queue
                .enqueue_terminal_completion(terminal_completion(
                    process_id as i32,
                    uuid::Uuid::new_v4(),
                ))
                .await;
        }

        let pending = input_queue.terminal_completions.lock().await;
        assert_eq!(pending.len(), InputQueue::MAX_PENDING_TERMINAL_COMPLETIONS);
        assert_eq!(
            pending.front().map(|completion| completion.process_id),
            Some(1)
        );
        assert_eq!(
            pending
                .front()
                .map(|completion| completion.coalesced_exited),
            Some(1)
        );
        assert_eq!(
            pending.back().map(|completion| completion.process_id),
            Some(InputQueue::MAX_PENDING_TERMINAL_COMPLETIONS as i32)
        );
        assert_eq!(
            pending.back().map(|completion| completion.coalesced_exited),
            Some(0)
        );
        assert_eq!(
            pending
                .iter()
                .map(|completion| 1 + completion.coalesced_exited + completion.coalesced_failed)
                .sum::<u64>(),
            InputQueue::MAX_PENDING_TERMINAL_COMPLETIONS as u64 + 1
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
        let mail_two = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ false,
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
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    acceptance_order: None,
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
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    acceptance_order: None,
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
}
