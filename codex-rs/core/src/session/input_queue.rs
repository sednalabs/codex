use crate::context::ContextualUserFragment;
use crate::context::TerminalCompletionNotification;
use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::user_input::UserInput;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::Mutex;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
    },
    ResponseItem(ResponseItem),
    InterAgentCommunication(InterAgentCommunication),
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

#[derive(Clone, Default)]
struct MailboxQueue {
    entries: VecDeque<MailboxEntry>,
}

#[derive(Clone)]
struct MailboxEntry {
    communication: InterAgentCommunication,
    sequence: u64,
    enqueued_at_ms: u64,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    mailbox: Mutex<MailboxQueue>,
    pending_mailbox_entries: Mutex<VecDeque<MailboxEntry>>,
    next_mailbox_sequence: AtomicU64,
    terminal_completions: Mutex<VecDeque<TerminalCompletionNotification>>,
    residency_transition: Arc<Mutex<()>>,
    residency_activity_generation: AtomicU64,
    pending_terminal_finalizers: AtomicUsize,
    pending_residency_submissions: StdMutex<HashSet<String>>,
    #[cfg(test)]
    residency_submission_changed: Notify,
}

impl InputQueue {
    const MAX_PENDING_TERMINAL_COMPLETIONS: usize = 64;

    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            mailbox: Mutex::new(MailboxQueue::default()),
            pending_mailbox_entries: Mutex::new(VecDeque::new()),
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
            turn_state.lock().await.pending_input.has_user_input()
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
    ) {
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
        let enqueued_at_ms = current_time_ms();
        mailbox.entries.extend(
            communications
                .into_iter()
                .map(|communication| MailboxEntry {
                    communication,
                    sequence: self.next_mailbox_sequence.fetch_add(1, Ordering::Relaxed),
                    enqueued_at_ms,
                }),
        );
        tracing::trace!(
            target: "codex.native_wait",
            queued_update_count = communication_count,
            enqueued_at_ms,
            "mailbox_updates_enqueued"
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
            .map(|communication| MailboxEntry {
                communication,
                sequence: self.next_mailbox_sequence.fetch_add(1, Ordering::Relaxed),
                enqueued_at_ms: current_time_ms(),
            })
            .collect();
        for entry in queued.into_iter().rev() {
            mailbox.entries.push_front(entry);
        }
        tracing::trace!(
            target: "codex.native_wait",
            queued_update_count = mailbox.entries.len(),
            "mailbox_updates_prepended"
        );
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        !self.mailbox.lock().await.entries.is_empty()
    }

    /// Nondestructive mailbox read used by native wait reporting. The delivery
    /// queue remains untouched so model delivery ordering and ownership are
    /// preserved.
    pub(crate) async fn snapshot_mailbox_communications(
        &self,
    ) -> Vec<(InterAgentCommunication, u64, u64)> {
        let mailbox = self.mailbox.lock().await;
        mailbox
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.communication.clone(),
                    entry.sequence,
                    entry.enqueued_at_ms,
                )
            })
            .collect()
    }

    /// Snapshot mailbox entries that were moved into the active turn before a
    /// native wait subscribed. Startup drains the durable mailbox into
    /// `TurnState` before the model runs; retain the original sequence and
    /// enqueue timestamp separately so wait provenance remains truthful.
    pub(crate) async fn snapshot_pending_mailbox_communications(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> Vec<(InterAgentCommunication, u64, u64)> {
        let pending_communications = {
            let active = active_turn.lock().await;
            let Some(active_turn) = active.as_ref() else {
                return Vec::new();
            };
            active_turn
                .turn_state
                .lock()
                .await
                .pending_input
                .items
                .iter()
                .filter_map(|input| match input {
                    TurnInput::InterAgentCommunication(communication) => {
                        Some(communication.clone())
                    }
                    TurnInput::UserInput { .. } | TurnInput::ResponseItem(_) => None,
                })
                .collect::<Vec<_>>()
        };
        let pending_entries = self.pending_mailbox_entries.lock().await;
        let mut used = vec![false; pending_entries.len()];
        pending_communications
            .into_iter()
            .filter_map(|communication| {
                let index = pending_entries
                    .iter()
                    .enumerate()
                    .find_map(|(index, entry)| {
                        (!used[index] && entry.communication == communication).then_some(index)
                    })?;
                used[index] = true;
                let entry = &pending_entries[index];
                Some((
                    entry.communication.clone(),
                    entry.sequence,
                    entry.enqueued_at_ms,
                ))
            })
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
            .entries
            .iter()
            .any(|entry| entry.communication.trigger_turn)
    }

    /// Returns whether a mailbox message carries an actionable wake signal for
    /// a native wait. Explicit trigger-turn messages are actionable even when
    /// their content is encrypted; result messages are actionable without a
    /// new turn only after a nonempty plaintext payload is available.
    pub(crate) async fn has_actionable_wait_mailbox_items(&self) -> bool {
        self.mailbox
            .lock()
            .await
            .entries
            .iter()
            .any(|entry| is_actionable_wait_communication(&entry.communication))
    }

    /// This predicate is intentionally used for both exact-target and
    /// targetless waits. Ordinary queue-only progress remains durable but must
    /// not wake a parent merely because the mailbox is non-empty.
    ///
    /// Queue-only mailbox messages remain durable, but do not wake a wait that
    /// is observing specific agents.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn has_pending_wait_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> bool {
        let accepts_mailbox_delivery = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => {
                    let turn_state = active_turn.turn_state.lock().await;
                    if turn_state
                        .pending_input
                        .items
                        .iter()
                        .any(|input| match input {
                            TurnInput::InterAgentCommunication(communication) => {
                                is_actionable_wait_communication(communication)
                            }
                            TurnInput::UserInput { .. } | TurnInput::ResponseItem(_) => true,
                        })
                    {
                        return true;
                    }
                    turn_state.accepts_mailbox_delivery_for_current_turn()
                }
                None => true,
            }
        };
        (accepts_mailbox_delivery && self.has_actionable_wait_mailbox_items().await)
            || self.has_pending_terminal_completions().await
    }

    /// Returns the activity class for input that is already pending when a
    /// wait subscribes. This preserves the distinction between a pre-existing
    /// operator steer and mailbox/system activity; the watch channel only
    /// reports changes after subscription and cannot identify this ordering.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn pending_wait_input_activity(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> Option<InputQueueActivity> {
        let (accepts_mailbox_delivery, pending_activity) = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => {
                    let turn_state = active_turn.turn_state.lock().await;
                    let pending_activity =
                        turn_state
                            .pending_input
                            .items
                            .iter()
                            .find_map(|input| match input {
                                TurnInput::UserInput { .. } | TurnInput::ResponseItem(_) => {
                                    Some(InputQueueActivity::Steer)
                                }
                                TurnInput::InterAgentCommunication(communication)
                                    if is_actionable_wait_communication(communication) =>
                                {
                                    Some(InputQueueActivity::Mailbox)
                                }
                                TurnInput::InterAgentCommunication(_) => None,
                            });
                    (
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                        pending_activity,
                    )
                }
                None => (true, None),
            }
        };

        if pending_activity.is_some() {
            return pending_activity;
        }
        if accepts_mailbox_delivery && self.has_actionable_wait_mailbox_items().await {
            return Some(InputQueueActivity::Mailbox);
        }
        if self.has_pending_terminal_completions().await {
            return Some(InputQueueActivity::TerminalCompletion);
        }
        None
    }

    pub(crate) async fn drain_mailbox_input_items(&self) -> Vec<TurnInput> {
        self.drain_mailbox_communications()
            .await
            .into_iter()
            .map(TurnInput::InterAgentCommunication)
            .collect()
    }

    pub(crate) async fn drain_mailbox_communications(&self) -> Vec<InterAgentCommunication> {
        let mut mailbox = self.mailbox.lock().await;
        mailbox
            .entries
            .drain(..)
            .map(|entry| entry.communication)
            .collect()
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
        let pending_input = {
            let mut turn_state = active_turn.turn_state.lock().await;
            turn_state.clear_pending_waiters();
            turn_state.pending_input.items.split_off(0)
        };
        self.forget_pending_mailbox_entries(&pending_input).await;
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
        let pending_input = turn_state.lock().await.pending_input.items.split_off(0);
        self.forget_pending_mailbox_entries(&pending_input).await;
        pending_input
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> Vec<TurnInput> {
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
            return pending_input;
        }
        let mailbox_entries = self.drain_mailbox_entries().await;
        if !mailbox_entries.is_empty() {
            self.pending_mailbox_entries
                .lock()
                .await
                .extend(mailbox_entries.iter().cloned());
        }
        let mailbox_items = mailbox_entries
            .into_iter()
            .map(|entry| TurnInput::InterAgentCommunication(entry.communication));
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
        }
    }

    async fn forget_pending_mailbox_entries(&self, pending_input: &[TurnInput]) {
        let mut pending_entries = self.pending_mailbox_entries.lock().await;
        for communication in pending_input.iter().filter_map(|input| match input {
            TurnInput::InterAgentCommunication(communication) => Some(communication),
            TurnInput::UserInput { .. } | TurnInput::ResponseItem(_) => None,
        }) {
            if let Some(index) = pending_entries
                .iter()
                .position(|entry| entry.communication == *communication)
            {
                pending_entries.remove(index);
            }
        }
    }

    async fn drain_mailbox_entries(&self) -> Vec<MailboxEntry> {
        let mut mailbox = self.mailbox.lock().await;
        mailbox.entries.drain(..).collect()
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

pub(crate) fn is_actionable_wait_communication(communication: &InterAgentCommunication) -> bool {
    communication.trigger_turn
        || (communication.encrypted_content.is_none()
            && !communication.content.is_empty()
            && communication.origin
                == Some(codex_protocol::protocol::AgentCommunicationOrigin::Result))
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

impl TurnInputQueue {
    fn has_user_input(&self) -> bool {
        self.items
            .iter()
            .any(|input| matches!(input, TurnInput::UserInput { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::TerminalCompletionStatus;
    use codex_protocol::AgentPath;
    use pretty_assertions::assert_eq;

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

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "one",
                /*trigger_turn*/ false,
            ))
            .await;
        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "two",
                /*trigger_turn*/ false,
            ))
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
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
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
            .enqueue_mailbox_communication(mail_one.clone())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![
                TurnInput::InterAgentCommunication(mail_one),
                TurnInput::InterAgentCommunication(mail_two)
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_tracks_pending_trigger_turn_mail() {
        let input_queue = InputQueue::new();

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "queued",
                /*trigger_turn*/ false,
            ))
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);
        assert!(!input_queue.has_pending_wait_input(&Mutex::new(None)).await);

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "wake",
                /*trigger_turn*/ true,
            ))
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
        assert!(input_queue.has_pending_wait_input(&Mutex::new(None)).await);
    }

    #[tokio::test]
    async fn input_queue_preserves_mailbox_metadata_when_moved_to_turn_state() {
        let input_queue = InputQueue::new();
        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "wake",
                /*trigger_turn*/ true,
            ))
            .await;

        let active_turn = Mutex::new(Some(ActiveTurn::default()));
        let turn_state = active_turn
            .lock()
            .await
            .as_ref()
            .expect("active turn")
            .turn_state
            .clone();
        let pending_input = input_queue.get_pending_input(&active_turn).await;
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, pending_input)
            .await;

        let snapshot = input_queue
            .snapshot_pending_mailbox_communications(&active_turn)
            .await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].1, 0);
    }

    #[tokio::test]
    async fn input_queue_treats_plaintext_result_as_actionable_wait_mail() {
        let input_queue = InputQueue::new();
        let mut result = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "child completed",
            /*trigger_turn*/ false,
        );
        result.origin = Some(codex_protocol::protocol::AgentCommunicationOrigin::Result);
        input_queue.enqueue_mailbox_communication(result).await;

        assert!(input_queue.has_pending_wait_input(&Mutex::new(None)).await);
    }

    #[tokio::test]
    async fn input_queue_treats_encrypted_trigger_turn_as_actionable_wait_mail() {
        let input_queue = InputQueue::new();
        let encrypted_followup = InterAgentCommunication::new_encrypted(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            Vec::new(),
            "continue".to_string(),
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(encrypted_followup)
            .await;

        assert!(input_queue.has_pending_wait_input(&Mutex::new(None)).await);
    }

    #[tokio::test]
    async fn input_queue_does_not_wake_for_pending_queue_only_turn_input() {
        let input_queue = InputQueue::new();
        let active_turn = Mutex::new(Some(ActiveTurn::default()));
        let turn_state = active_turn
            .lock()
            .await
            .as_ref()
            .expect("active turn")
            .turn_state
            .clone();

        input_queue
            .extend_pending_input_for_turn_state(
                &turn_state,
                vec![TurnInput::InterAgentCommunication(make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    "queued",
                    /*trigger_turn*/ false,
                ))],
            )
            .await;
        assert!(!input_queue.has_pending_wait_input(&active_turn).await);

        input_queue
            .extend_pending_input_for_turn_state(
                &turn_state,
                vec![TurnInput::InterAgentCommunication(make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    "wake",
                    /*trigger_turn*/ true,
                ))],
            )
            .await;
        assert!(input_queue.has_pending_wait_input(&active_turn).await);
    }

    #[tokio::test]
    async fn input_queue_does_not_wake_for_encrypted_or_unavailable_result_mail() {
        let input_queue = InputQueue::new();
        let mut encrypted = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "",
            /*trigger_turn*/ false,
        );
        encrypted.origin = Some(codex_protocol::protocol::AgentCommunicationOrigin::Result);
        encrypted.encrypted_content = Some("opaque".to_string());
        input_queue.enqueue_mailbox_communication(encrypted).await;
        assert!(!input_queue.has_pending_wait_input(&Mutex::new(None)).await);

        let mut unavailable = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "",
            /*trigger_turn*/ false,
        );
        unavailable.origin = Some(codex_protocol::protocol::AgentCommunicationOrigin::Result);
        input_queue.enqueue_mailbox_communication(unavailable).await;
        assert!(!input_queue.has_pending_wait_input(&Mutex::new(None)).await);
    }

    #[tokio::test]
    async fn input_queue_wait_input_allows_terminal_completion_after_mailbox_delivery_deferral() {
        let input_queue = InputQueue::new();
        let active_turn = Mutex::new(Some(ActiveTurn::default()));
        let turn_state = active_turn
            .lock()
            .await
            .as_ref()
            .expect("active turn")
            .turn_state
            .clone();
        turn_state
            .lock()
            .await
            .set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "queued",
                /*trigger_turn*/ false,
            ))
            .await;
        assert!(!input_queue.has_pending_wait_input(&active_turn).await);

        input_queue
            .enqueue_terminal_completion(terminal_completion(
                /*process_id*/ 7,
                uuid::Uuid::new_v4(),
            ))
            .await;
        assert!(input_queue.has_pending_wait_input(&active_turn).await);
    }
}
