use crate::context::ContextualUserFragment;
use crate::context::TerminalCompletionNotification;
use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::RunningTaskPhase;
use crate::tasks::TaskContinuationContext;
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
use tokio::sync::Mutex;
use tokio::sync::MutexGuard;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

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

pub(crate) struct ContinuationPendingInput {
    pub(crate) input: Vec<TurnInput>,
    pub(crate) claim_owned: bool,
}

/// Turn-local pending input storage owned by the input queue flow.
#[derive(Default)]
pub(crate) struct TurnInputQueue {
    items: Vec<TurnInput>,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    task_transition: Arc<Mutex<()>>,
    mailbox_pending_mails: Mutex<VecDeque<InterAgentCommunication>>,
    terminal_completions: Mutex<VecDeque<TerminalCompletionNotification>>,
    residency_transition: Arc<Mutex<()>>,
    residency_activity_generation: AtomicU64,
    pending_terminal_finalizers: AtomicUsize,
    pending_residency_submissions: StdMutex<HashSet<String>>,
    #[cfg(test)]
    residency_submission_changed: Notify,
    #[cfg(test)]
    pending_turn_input_transfer_source_count: AtomicUsize,
    #[cfg(test)]
    pending_turn_input_transfer_waiting_for_source: Notify,
}

impl InputQueue {
    const MAX_PENDING_TERMINAL_COMPLETIONS: usize = 64;

    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            task_transition: Arc::new(Mutex::new(())),
            mailbox_pending_mails: Mutex::new(VecDeque::new()),
            terminal_completions: Mutex::new(VecDeque::new()),
            residency_transition: Arc::new(Mutex::new(())),
            residency_activity_generation: AtomicU64::new(0),
            pending_terminal_finalizers: AtomicUsize::new(0),
            pending_residency_submissions: StdMutex::new(HashSet::new()),
            #[cfg(test)]
            residency_submission_changed: Notify::new(),
            #[cfg(test)]
            pending_turn_input_transfer_source_count: AtomicUsize::new(0),
            #[cfg(test)]
            pending_turn_input_transfer_waiting_for_source: Notify::new(),
        }
    }

    pub(crate) async fn lock_task_transition(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.task_transition).lock_owned().await
    }
    /// Lock mailbox before terminal; acquisition is non-consuming until commit.
    #[expect(clippy::await_holding_invalid_type, reason = "atomic input transfer")]
    pub(crate) async fn pending_turn_input_transfer(&self) -> PendingTurnInputTransfer<'_> {
        #[cfg(test)]
        self.pending_turn_input_transfer_source_count
            .fetch_add(1, Ordering::SeqCst);
        #[cfg(test)]
        self.pending_turn_input_transfer_waiting_for_source
            .notify_one();
        let mailbox = self.mailbox_pending_mails.lock().await;
        #[cfg(test)]
        self.pending_turn_input_transfer_source_count
            .fetch_add(1, Ordering::SeqCst);
        #[cfg(test)]
        self.pending_turn_input_transfer_waiting_for_source
            .notify_one();
        let terminal = self.terminal_completions.lock().await;
        PendingTurnInputTransfer { mailbox, terminal }
    }

    #[cfg(test)]
    pub(crate) async fn lock_terminal_for_test(
        &self,
    ) -> MutexGuard<'_, VecDeque<TerminalCompletionNotification>> {
        self.terminal_completions.lock().await
    }

    #[cfg(test)]
    pub(crate) async fn lock_mailbox_for_test(
        &self,
    ) -> MutexGuard<'_, VecDeque<InterAgentCommunication>> {
        self.mailbox_pending_mails.lock().await
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_pending_turn_input_transfer_source(&self, expected: usize) {
        while self
            .pending_turn_input_transfer_source_count
            .load(Ordering::SeqCst)
            < expected
        {
            self.pending_turn_input_transfer_waiting_for_source
                .notified()
                .await;
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
        self.mailbox_pending_mails
            .lock()
            .await
            .extend(communications);
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    pub(crate) async fn prepend_mailbox_communications(
        &self,
        communications: Vec<InterAgentCommunication>,
    ) {
        if communications.is_empty() {
            return;
        }
        let mut pending = self.mailbox_pending_mails.lock().await;
        for communication in communications.into_iter().rev() {
            pending.push_front(communication);
        }
        drop(pending);
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        !self.mailbox_pending_mails.lock().await.is_empty()
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
        self.mailbox_pending_mails
            .lock()
            .await
            .iter()
            .any(|mail| mail.trigger_turn)
    }

    pub(crate) async fn drain_mailbox_input_items(&self) -> Vec<TurnInput> {
        self.drain_mailbox_communications()
            .await
            .into_iter()
            .map(TurnInput::InterAgentCommunication)
            .collect()
    }

    pub(crate) async fn drain_mailbox_communications(&self) -> Vec<InterAgentCommunication> {
        self.mailbox_pending_mails.lock().await.drain(..).collect()
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

    #[cfg(test)]
    pub(crate) async fn clone_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        turn_state.lock().await.pending_input.items.clone()
    }

    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        turn_state.lock().await.pending_input.items.split_off(0)
    }

    pub(crate) async fn take_pending_input_for_turn_state_and_begin_continuation_processing(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> (Vec<TurnInput>, bool) {
        let mut turn_state = turn_state.lock().await;
        let pending_input = turn_state.pending_input.items.split_off(0);
        let processing_claim = turn_state
            .begin_turn_local_continuation_input_processing(pending_input.clone());
        (pending_input, processing_claim)
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
                        let pending_input = turn_state.pending_input.items.split_off(0);
                        if turn_state.turn_local_continuation_input_is_requeued() {
                            turn_state
                                .mark_turn_local_continuation_input_drained(pending_input.clone());
                        }
                        pending_input
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
        }
    }

    /// Drains input for one captured continuation only after atomically validating that its task
    /// still owns the active turn. Mailbox and terminal queues are locked first to preserve the
    /// same order used by task publication, then the task-transition lock prevents replacement
    /// between validation and the canonical queue drain.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "captured continuation validation and input transfer remain atomic"
    )]
    pub(crate) async fn get_pending_input_for_continuation(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        continuation: &TaskContinuationContext,
        cancellation_token: &CancellationToken,
    ) -> Option<ContinuationPendingInput> {
        let mut transfer = self.pending_turn_input_transfer().await;
        if cancellation_token.is_cancelled() {
            return None;
        }
        let _transition = self.lock_task_transition().await;
        let mut active = active_turn.lock().await;
        let active_turn = active.as_mut()?;
        let task = active_turn.task.as_ref()?;
        if task.identity != continuation.task_identity
            || !matches!(
                task.phase,
                RunningTaskPhase::Running | RunningTaskPhase::Finalizing
            )
            || task.cancellation_token.is_cancelled()
            || task
                .turn_state
                .as_ref()
                .is_none_or(|task_state| !Arc::ptr_eq(task_state, &continuation.turn_state))
            || !Arc::ptr_eq(&active_turn.turn_state, &continuation.turn_state)
        {
            return None;
        }

        let mut turn_state = continuation.turn_state.lock().await;
        if !turn_state.accepts_mailbox_delivery_for_current_turn() {
            return Some(ContinuationPendingInput {
                input: Vec::new(),
                claim_owned: false,
            });
        }
        let mut input = turn_state.pending_input.items.split_off(0);
        input.extend(
            transfer
                .mailbox
                .drain(..)
                .map(TurnInput::InterAgentCommunication),
        );
        input.extend(
            transfer.terminal.drain(..).map(|completion| {
                TurnInput::ResponseItem(ContextualUserFragment::into(completion))
            }),
        );
        let claim_owned = turn_state.mark_turn_local_continuation_input_drained(input.clone());
        Some(ContinuationPendingInput { input, claim_owned })
    }

    /// Checks pending input for one captured continuation without consulting a replacement
    /// task's state or consuming either canonical session queue.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "captured continuation validation and input inspection remain atomic"
    )]
    pub(crate) async fn has_pending_input_for_continuation(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        continuation: &TaskContinuationContext,
        cancellation_token: &CancellationToken,
    ) -> Option<bool> {
        let mailbox = self.mailbox_pending_mails.lock().await;
        let terminal = self.terminal_completions.lock().await;
        if cancellation_token.is_cancelled() {
            return None;
        }
        let _transition = self.lock_task_transition().await;
        let active = active_turn.lock().await;
        let active_turn = active.as_ref()?;
        let task = active_turn.task.as_ref()?;
        if task.identity != continuation.task_identity
            || !matches!(
                task.phase,
                RunningTaskPhase::Running | RunningTaskPhase::Finalizing
            )
            || task.cancellation_token.is_cancelled()
            || task
                .turn_state
                .as_ref()
                .is_none_or(|task_state| !Arc::ptr_eq(task_state, &continuation.turn_state))
            || !Arc::ptr_eq(&active_turn.turn_state, &continuation.turn_state)
        {
            return None;
        }
        let turn_state = continuation.turn_state.lock().await;
        if !turn_state.accepts_mailbox_delivery_for_current_turn() {
            return Some(false);
        }
        Some(!turn_state.pending_input.items.is_empty() || !mailbox.is_empty() || !terminal.is_empty())
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

pub(crate) struct PendingTurnInputTransfer<'a> {
    mailbox: MutexGuard<'a, VecDeque<InterAgentCommunication>>,
    terminal: MutexGuard<'a, VecDeque<TerminalCompletionNotification>>,
}

impl PendingTurnInputTransfer<'_> {
    pub(crate) fn commit_into(&mut self, turn_state: &mut TurnState) {
        if !turn_state.accepts_mailbox_delivery_for_current_turn() {
            return;
        }
        turn_state.pending_input.items.extend(
            self.mailbox
                .drain(..)
                .map(TurnInput::InterAgentCommunication),
        );
        turn_state.pending_input.items.extend(
            self.terminal.drain(..).map(|completion| {
                TurnInput::ResponseItem(ContextualUserFragment::into(completion))
            }),
        );
    }
}

impl TurnInputQueue {
    pub(crate) fn clone_items(&self) -> Vec<TurnInput> {
        self.items.clone()
    }

    fn has_user_input(&self) -> bool {
        self.items
            .iter()
            .any(|input| matches!(input, TurnInput::UserInput { .. }))
    }

    pub(crate) fn has_non_empty_user_input(&self) -> bool {
        self.items.iter().any(
            |input| matches!(input, TurnInput::UserInput { content, .. } if !content.is_empty()),
        )
    }

    pub(crate) fn take_for_turn_local_continuation(&mut self) -> Vec<TurnInput> {
        self.items.split_off(0)
    }

    pub(crate) fn restore_turn_local_continuation(&mut self, input: Vec<TurnInput>) {
        self.items.splice(0..0, input);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::TerminalCompletionStatus;
    use codex_protocol::AgentPath;
    use pretty_assertions::assert_eq;
    use std::future::Future;

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
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "deterministic source lock"
    )]
    async fn dropping_pending_turn_transfer_keeps_both_canonical_queues() {
        let input_queue = InputQueue::new();
        let mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "mail",
            /*trigger_turn*/ true,
        );
        let terminal = terminal_completion(/*process_id*/ 7, uuid::Uuid::new_v4());
        input_queue
            .enqueue_mailbox_communication(mail.clone())
            .await;
        input_queue
            .enqueue_terminal_completion(terminal.clone())
            .await;

        let mailbox = input_queue.mailbox_pending_mails.lock().await;
        let mut waiting_for_mailbox = Box::pin(input_queue.pending_turn_input_transfer());
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                waiting_for_mailbox.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        drop(waiting_for_mailbox);
        drop(mailbox);

        let terminal_lock = input_queue.terminal_completions.lock().await;
        let mut waiting_for_terminal = Box::pin(input_queue.pending_turn_input_transfer());
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                waiting_for_terminal.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        drop(waiting_for_terminal);
        drop(terminal_lock);
        assert_eq!(
            input_queue.mailbox_pending_mails.lock().await.pop_front(),
            Some(mail)
        );
        assert_eq!(
            input_queue.terminal_completions.lock().await.pop_front(),
            Some(terminal)
        );
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

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "wake",
                /*trigger_turn*/ true,
            ))
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }

    #[tokio::test]
    async fn drained_continuation_claim_is_restored_once_after_pre_record_exit() {
        let input_queue = InputQueue::new();
        let turn_state = Arc::new(Mutex::new(TurnState::default()));
        let active_turn = Mutex::new(Some(ActiveTurn {
            task: None,
            turn_state: Arc::clone(&turn_state),
        }));
        let input = vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "restore after hook exit".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }];

        turn_state
            .lock()
            .await
            .requeue_turn_local_continuation_input(input.clone());
        assert_eq!(input, input_queue.get_pending_input(&active_turn).await);

        // The continuation has drained the claim but exited before input recording. Restoration
        // must put the claim back even if a newer steer arrived in the meantime.
        let newer_input = vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "newer steer".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }];
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, newer_input.clone())
            .await;
        turn_state
            .lock()
            .await
            .restore_turn_local_continuation_input(input.clone());
        let mut expected = input.clone();
        expected.extend(newer_input);
        assert_eq!(
            expected,
            input_queue
                .clone_pending_input_for_turn_state(&turn_state)
                .await
        );

        // A second cleanup notification must not duplicate a claim that was already restored.
        turn_state
            .lock()
            .await
            .restore_turn_local_continuation_input(input.clone());
        assert_eq!(
            expected,
            input_queue
                .clone_pending_input_for_turn_state(&turn_state)
                .await
        );
    }
}
