//! Turn-scoped state and active turn metadata scaffolding.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use codex_extension_api::ExtensionData;
use codex_protocol::computer_use::ComputerUseResponse;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_rmcp_client::ElicitationResponse;
use codex_sandboxing::policy_transforms::merge_permission_profiles;
use rmcp::model::RequestId;
use tokio::sync::oneshot;

use crate::agent::control::AgentExecutionGuard;
use crate::mcp_tool_call::McpToolApprovalMetadata;
use crate::session::TurnInput;
use crate::session::TurnInputQueue;
use crate::session::turn_context::TurnContext;
use crate::tasks::AnySessionTask;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::TokenUsage;

/// Metadata about the currently running turn.
pub(crate) struct ActiveTurn {
    pub(crate) task: Option<RunningTask>,
    pub(crate) turn_state: Arc<Mutex<TurnState>>,
}

/// Whether mailbox deliveries should still be folded into the current turn.
///
/// State machine:
/// - A turn starts in `CurrentTurn`, so queued child mail can join the next
///   model request for that turn.
/// - After user-visible terminal output is recorded, we switch to `NextTurn`
///   to leave late child mail queued instead of extending an already shown
///   answer.
/// - If the same task later gets explicit same-turn work again (a steered user
///   prompt or a tool call after an untagged preamble), we reopen `CurrentTurn`
///   so that pending child mail is drained into that follow-up request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum MailboxDeliveryPhase {
    /// Incoming mailbox messages can still be consumed by the current turn.
    #[default]
    CurrentTurn,
    /// The current turn already emitted visible final answer text; mailbox
    /// messages should remain queued for a later turn.
    NextTurn,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TurnLocalContinuationInputState {
    #[default]
    None,
    Held,
    Requeued,
    /// The requeued claim was drained into a continuation's model-visible input, but hooks have
    /// not yet committed whether that input was accepted. Keeping this distinct from `Requeued`
    /// lets an ordinary pre-record exit restore the claim without duplicating an item that is
    /// still in the pending-input queue.
    Drained,
    /// The continuation (or its finalization path) is running input hooks. Abort cleanup waits
    /// for this state to commit so it cannot process the same claim concurrently.
    HookProcessing,
    Consumed,
}

impl Default for ActiveTurn {
    fn default() -> Self {
        Self {
            task: None,
            turn_state: Arc::new(Mutex::new(TurnState::default())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskKind {
    Regular,
    Review,
    Compact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TaskIdentity(pub(crate) uuid::Uuid);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunningTaskPhase {
    Preparing,
    Running,
    /// The task has returned and its terminal hooks/disposition are still owned by the
    /// task-finalization path.  Keeping this phase published in the active slot prevents a
    /// replacement from orphaning the finalizer or observing a stale terminal event.
    Finalizing,
}

pub(crate) struct RunningTask {
    pub(crate) done: Arc<Notify>,
    pub(crate) identity: TaskIdentity,
    pub(crate) phase: RunningTaskPhase,
    pub(crate) kind: TaskKind,
    pub(crate) task: Arc<dyn AnySessionTask>,
    pub(crate) initial_input: Option<Vec<TurnInput>>,
    pub(crate) cancellation_token: CancellationToken,
    /// The drop-aborts wrapper is detached when finalization begins.  The remote abort handle is
    /// retained so forced resolution can stop the detached task without making its drop race with
    /// HookProcessing.
    pub(crate) handle: Option<AbortOnDropHandle<()>>,
    pub(crate) abort_handle: Option<tokio::task::AbortHandle>,
    pub(crate) turn_context: Arc<TurnContext>,
    pub(crate) turn_extension_data: Arc<ExtensionData>,
    /// The exact turn state this task owns. Set during publication so completion and
    /// continuation paths can reject stale task/state pairs.
    pub(crate) turn_state: Option<Arc<Mutex<TurnState>>>,
    pub(crate) _agent_execution_guard: Option<AgentExecutionGuard>,
    // Timer recorded when the task drops to capture the full turn duration.
    pub(crate) _timer: Option<codex_otel::Timer>,
}

/// Mutable state for a single turn.
pub(crate) struct TurnState {
    pending_approvals: HashMap<String, oneshot::Sender<ReviewDecision>>,
    pending_request_permissions: HashMap<String, PendingRequestPermissions>,
    pending_user_input: HashMap<String, oneshot::Sender<RequestUserInputResponse>>,
    pending_elicitations: HashMap<(String, RequestId), oneshot::Sender<ElicitationResponse>>,
    mcp_tool_approval_metadata: HashMap<String, McpToolApprovalMetadata>,
    pending_dynamic_tools: HashMap<String, oneshot::Sender<DynamicToolResponse>>,
    pending_computer_use: HashMap<String, oneshot::Sender<ComputerUseResponse>>,
    pub(crate) pending_input: TurnInputQueue,
    /// Input claimed for a same-task continuation remains here until the continuation commits
    /// it. Keeping the claim in turn state lets task-abort cleanup recover it even if the task
    /// future is cancelled before it can return a disposition to the completion path.
    turn_local_continuation_input: Option<Vec<TurnInput>>,
    turn_local_continuation_input_state: TurnLocalContinuationInputState,
    turn_local_continuation_input_changed: Arc<Notify>,
    finalization_state: TurnFinalizationState,
    finalization_changed: Arc<Notify>,
    mailbox_delivery_phase: MailboxDeliveryPhase,
    granted_permissions_by_environment_id: HashMap<String, AdditionalPermissionProfile>,
    compaction_events_in_turn: u32,
    strict_auto_review_enabled: bool,
    pub(crate) tool_calls: u64,
    pub(crate) has_memory_citation: bool,
    pub(crate) token_usage_at_turn_start: TokenUsage,
}

/// Ownership protocol for the one terminal finalizer associated with a turn.
///
/// `Owned` is held from task completion through the final hook/disposition commit.  An aborting
/// replacement first requests that owner to stop, then moves it to `ForcedAbort` after a bounded
/// wait.  Once the state is terminal, a stale completion path cannot publish `TurnComplete`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TurnFinalizationState {
    #[default]
    Open,
    Owned,
    AbortRequested,
    ForcedAbort,
    Published,
    Aborted,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            pending_approvals: HashMap::new(),
            pending_request_permissions: HashMap::new(),
            pending_user_input: HashMap::new(),
            pending_elicitations: HashMap::new(),
            mcp_tool_approval_metadata: HashMap::new(),
            pending_dynamic_tools: HashMap::new(),
            pending_computer_use: HashMap::new(),
            pending_input: TurnInputQueue::default(),
            turn_local_continuation_input: None,
            turn_local_continuation_input_state: TurnLocalContinuationInputState::None,
            turn_local_continuation_input_changed: Arc::new(Notify::new()),
            finalization_state: TurnFinalizationState::Open,
            finalization_changed: Arc::new(Notify::new()),
            mailbox_delivery_phase: MailboxDeliveryPhase::CurrentTurn,
            granted_permissions_by_environment_id: HashMap::new(),
            compaction_events_in_turn: 0,
            strict_auto_review_enabled: false,
            tool_calls: 0,
            has_memory_citation: false,
            token_usage_at_turn_start: TokenUsage::default(),
        }
    }
}

pub(crate) struct PendingRequestPermissions {
    pub(crate) tx_response: oneshot::Sender<RequestPermissionsResponse>,
    pub(crate) requested_permissions: RequestPermissionProfile,
    pub(crate) environment: TurnEnvironmentSelection,
}

impl TurnState {
    /// Claims terminal finalization for the task that is currently active.
    pub(crate) fn claim_finalization(&mut self) -> bool {
        if self.finalization_state != TurnFinalizationState::Open {
            return false;
        }
        self.finalization_state = TurnFinalizationState::Owned;
        self.finalization_changed.notify_waiters();
        true
    }

    /// Requests that the current finalizer stop before publishing completion.
    pub(crate) fn request_finalization_abort(&mut self) -> bool {
        match self.finalization_state {
            TurnFinalizationState::Owned => {
                self.finalization_state = TurnFinalizationState::AbortRequested;
                self.finalization_changed.notify_waiters();
                true
            }
            TurnFinalizationState::AbortRequested
            | TurnFinalizationState::ForcedAbort
            | TurnFinalizationState::Aborted => true,
            TurnFinalizationState::Open | TurnFinalizationState::Published => false,
        }
    }

    /// Terminally settles an uncooperative finalizer after the bounded abort grace period.
    pub(crate) fn force_finalization_abort(&mut self) -> bool {
        match self.finalization_state {
            TurnFinalizationState::Owned | TurnFinalizationState::AbortRequested => {
                self.finalization_state = TurnFinalizationState::ForcedAbort;
                self.finalization_changed.notify_waiters();
                true
            }
            TurnFinalizationState::ForcedAbort | TurnFinalizationState::Aborted => true,
            TurnFinalizationState::Open | TurnFinalizationState::Published => false,
        }
    }

    /// Commits a terminal completion publication.  An abort request wins if it arrived first.
    pub(crate) fn publish_finalization(&mut self) -> bool {
        if self.finalization_state != TurnFinalizationState::Owned {
            return false;
        }
        self.finalization_state = TurnFinalizationState::Published;
        self.finalization_changed.notify_waiters();
        true
    }

    /// Commits the aborted terminal disposition and prevents a later stale completion.
    pub(crate) fn commit_finalization_abort(&mut self) -> bool {
        match self.finalization_state {
            TurnFinalizationState::Owned
            | TurnFinalizationState::AbortRequested
            | TurnFinalizationState::ForcedAbort => {
                self.finalization_state = TurnFinalizationState::Aborted;
                self.finalization_changed.notify_waiters();
                true
            }
            TurnFinalizationState::Aborted => true,
            TurnFinalizationState::Open | TurnFinalizationState::Published => false,
        }
    }

    pub(crate) fn finalization_changed(&self) -> Arc<Notify> {
        Arc::clone(&self.finalization_changed)
    }

    pub(crate) fn finalization_allows_progress(&self) -> bool {
        self.finalization_state == TurnFinalizationState::Owned
    }

    pub(crate) fn finalization_is_forced_abort(&self) -> bool {
        matches!(
            self.finalization_state,
            TurnFinalizationState::ForcedAbort | TurnFinalizationState::Aborted
        )
    }

    pub(crate) fn finalization_is_terminal(&self) -> bool {
        matches!(
            self.finalization_state,
            TurnFinalizationState::Published | TurnFinalizationState::Aborted
        )
    }

    pub(crate) fn finalization_is_published(&self) -> bool {
        self.finalization_state == TurnFinalizationState::Published
    }

    pub(crate) fn insert_pending_approval(
        &mut self,
        key: String,
        tx: oneshot::Sender<ReviewDecision>,
    ) -> Option<oneshot::Sender<ReviewDecision>> {
        self.pending_approvals.insert(key, tx)
    }

    pub(crate) fn remove_pending_approval(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<ReviewDecision>> {
        self.pending_approvals.remove(key)
    }

    pub(crate) fn clear_pending_waiters(&mut self) {
        self.pending_approvals.clear();
        self.pending_request_permissions.clear();
        self.pending_user_input.clear();
        self.pending_elicitations.clear();
        self.mcp_tool_approval_metadata.clear();
        self.pending_dynamic_tools.clear();
        self.pending_computer_use.clear();
    }

    pub(crate) fn insert_pending_request_permissions(
        &mut self,
        key: String,
        pending_request_permissions: PendingRequestPermissions,
    ) -> Option<PendingRequestPermissions> {
        self.pending_request_permissions
            .insert(key, pending_request_permissions)
    }

    pub(crate) fn remove_pending_request_permissions(
        &mut self,
        key: &str,
    ) -> Option<PendingRequestPermissions> {
        self.pending_request_permissions.remove(key)
    }

    pub(crate) fn insert_pending_user_input(
        &mut self,
        key: String,
        tx: oneshot::Sender<RequestUserInputResponse>,
    ) -> Option<oneshot::Sender<RequestUserInputResponse>> {
        self.pending_user_input.insert(key, tx)
    }

    pub(crate) fn remove_pending_user_input(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<RequestUserInputResponse>> {
        self.pending_user_input.remove(key)
    }

    pub(crate) fn insert_pending_elicitation(
        &mut self,
        server_name: String,
        request_id: RequestId,
        tx: oneshot::Sender<ElicitationResponse>,
    ) -> Option<oneshot::Sender<ElicitationResponse>> {
        self.pending_elicitations
            .insert((server_name, request_id), tx)
    }

    pub(crate) fn remove_pending_elicitation(
        &mut self,
        server_name: &str,
        request_id: &RequestId,
    ) -> Option<oneshot::Sender<ElicitationResponse>> {
        self.pending_elicitations
            .remove(&(server_name.to_string(), request_id.clone()))
    }

    pub(crate) fn insert_mcp_tool_approval_metadata(
        &mut self,
        call_id: String,
        metadata: McpToolApprovalMetadata,
    ) {
        self.mcp_tool_approval_metadata.insert(call_id, metadata);
    }

    pub(crate) fn mcp_tool_approval_metadata(
        &self,
        call_id: &str,
    ) -> Option<McpToolApprovalMetadata> {
        self.mcp_tool_approval_metadata.get(call_id).cloned()
    }

    pub(crate) fn insert_pending_dynamic_tool(
        &mut self,
        key: String,
        tx: oneshot::Sender<DynamicToolResponse>,
    ) -> Option<oneshot::Sender<DynamicToolResponse>> {
        self.pending_dynamic_tools.insert(key, tx)
    }

    pub(crate) fn remove_pending_dynamic_tool(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<DynamicToolResponse>> {
        self.pending_dynamic_tools.remove(key)
    }

    pub(crate) fn insert_pending_computer_use(
        &mut self,
        key: String,
        tx: oneshot::Sender<ComputerUseResponse>,
    ) -> Option<oneshot::Sender<ComputerUseResponse>> {
        self.pending_computer_use.insert(key, tx)
    }

    pub(crate) fn remove_pending_computer_use(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<ComputerUseResponse>> {
        self.pending_computer_use.remove(key)
    }

    pub(crate) fn accept_mailbox_delivery_for_current_turn(&mut self) {
        self.set_mailbox_delivery_phase(MailboxDeliveryPhase::CurrentTurn);
    }

    pub(crate) fn accepts_mailbox_delivery_for_current_turn(&self) -> bool {
        self.mailbox_delivery_phase == MailboxDeliveryPhase::CurrentTurn
    }

    pub(crate) fn take_turn_local_continuation_input(&mut self) -> Option<Vec<TurnInput>> {
        let has_eligible_input = self.mailbox_delivery_phase == MailboxDeliveryPhase::CurrentTurn
            && self.pending_input.has_non_empty_user_input();
        has_eligible_input.then(|| {
            let input = self.pending_input.take_for_turn_local_continuation();
            self.turn_local_continuation_input = Some(input.clone());
            self.turn_local_continuation_input_state = TurnLocalContinuationInputState::Held;
            input
        })
    }

    pub(crate) fn requeue_turn_local_continuation_input(&mut self, input: Vec<TurnInput>) {
        if self.turn_local_continuation_input_state != TurnLocalContinuationInputState::Requeued {
            self.pending_input
                .restore_turn_local_continuation(input.clone());
        }
        self.turn_local_continuation_input = Some(input);
        self.turn_local_continuation_input_state = TurnLocalContinuationInputState::Requeued;
    }

    pub(crate) fn restore_turn_local_continuation_input(&mut self, input: Vec<TurnInput>) {
        match self.turn_local_continuation_input_state {
            TurnLocalContinuationInputState::Held | TurnLocalContinuationInputState::Drained => {
                self.pending_input.restore_turn_local_continuation(input);
                // The claim includes any newer input that was already queued behind the original
                // continuation input. Keeping that complete claim here lets abort cleanup recover
                // A+B atomically even after the active task has been removed.
                self.turn_local_continuation_input = Some(self.pending_input.clone_items());
                self.turn_local_continuation_input_state =
                    TurnLocalContinuationInputState::Requeued;
            }
            TurnLocalContinuationInputState::None
            | TurnLocalContinuationInputState::Requeued
            | TurnLocalContinuationInputState::HookProcessing
            | TurnLocalContinuationInputState::Consumed => {}
        }
    }

    pub(crate) fn finish_turn_local_continuation_input(&mut self) {
        self.turn_local_continuation_input = None;
        self.turn_local_continuation_input_state = TurnLocalContinuationInputState::None;
    }

    pub(crate) fn mark_turn_local_continuation_input_consumed(&mut self) {
        if matches!(
            self.turn_local_continuation_input_state,
            TurnLocalContinuationInputState::Requeued
                | TurnLocalContinuationInputState::Drained
                | TurnLocalContinuationInputState::HookProcessing
        ) {
            self.turn_local_continuation_input_state = TurnLocalContinuationInputState::Consumed;
            self.turn_local_continuation_input_changed.notify_one();
        }
    }

    pub(crate) fn mark_turn_local_continuation_input_drained(
        &mut self,
        input: Vec<TurnInput>,
    ) -> bool {
        let has_claim = input.iter().any(|input| {
            matches!(input, TurnInput::UserInput { content, .. } if !content.is_empty())
        }) || self.turn_local_continuation_input_state == TurnLocalContinuationInputState::Requeued;
        if !has_claim {
            return false;
        }
        self.turn_local_continuation_input = Some(input);
        self.turn_local_continuation_input_state = TurnLocalContinuationInputState::Drained;
        true
    }

    pub(crate) fn begin_turn_local_continuation_input_processing(
        &mut self,
        input: Vec<TurnInput>,
    ) -> bool {
        if !matches!(
            self.turn_local_continuation_input_state,
            TurnLocalContinuationInputState::Requeued | TurnLocalContinuationInputState::Drained
        ) {
            return false;
        }
        self.turn_local_continuation_input = Some(input);
        self.turn_local_continuation_input_state =
            TurnLocalContinuationInputState::HookProcessing;
        true
    }

    /// Takes the input still owned by a continuation that was interrupted before it committed a
    /// disposition. The pending-input queue is cleared separately by abort cleanup.
    pub(crate) fn take_turn_local_continuation_input_for_abort(
        &mut self,
    ) -> Option<Vec<TurnInput>> {
        if !matches!(
            self.turn_local_continuation_input_state,
            TurnLocalContinuationInputState::Held
                | TurnLocalContinuationInputState::Requeued
                | TurnLocalContinuationInputState::Drained
        ) {
            return None;
        }
        self.turn_local_continuation_input_state = TurnLocalContinuationInputState::None;
        self.turn_local_continuation_input.take()
    }

    pub(crate) fn turn_local_continuation_input_is_hook_processing(&self) -> bool {
        self.turn_local_continuation_input_state
            == TurnLocalContinuationInputState::HookProcessing
    }

    pub(crate) fn turn_local_continuation_input_changed(&self) -> Arc<Notify> {
        Arc::clone(&self.turn_local_continuation_input_changed)
    }

    pub(crate) fn turn_local_continuation_input_is_claimed(&self) -> bool {
        !matches!(
            self.turn_local_continuation_input_state,
            TurnLocalContinuationInputState::None | TurnLocalContinuationInputState::Consumed
        )
    }

    pub(crate) fn turn_local_continuation_input_is_requeued(&self) -> bool {
        self.turn_local_continuation_input_state == TurnLocalContinuationInputState::Requeued
    }

    pub(crate) fn turn_local_continuation_input_was_requeued(&self) -> bool {
        matches!(
            self.turn_local_continuation_input_state,
            TurnLocalContinuationInputState::Requeued
                | TurnLocalContinuationInputState::Drained
                | TurnLocalContinuationInputState::HookProcessing
        )
    }

    pub(crate) fn turn_local_continuation_input_was_consumed(&self) -> bool {
        self.turn_local_continuation_input_state == TurnLocalContinuationInputState::Consumed
    }

    pub(crate) fn set_mailbox_delivery_phase(&mut self, phase: MailboxDeliveryPhase) {
        self.mailbox_delivery_phase = phase;
    }

    pub(crate) fn record_granted_permissions(
        &mut self,
        environment_id: &str,
        permissions: AdditionalPermissionProfile,
    ) {
        let granted_permissions = merge_permission_profiles(
            self.granted_permissions_by_environment_id
                .get(environment_id),
            Some(&permissions),
        );
        if let Some(granted_permissions) = granted_permissions {
            self.granted_permissions_by_environment_id
                .insert(environment_id.to_string(), granted_permissions);
        }
    }

    pub(crate) fn granted_permissions(
        &self,
        environment_id: &str,
    ) -> Option<AdditionalPermissionProfile> {
        self.granted_permissions_by_environment_id
            .get(environment_id)
            .cloned()
    }

    pub(crate) fn take_compaction_events_in_turn(&mut self) -> u32 {
        std::mem::take(&mut self.compaction_events_in_turn)
    }

    pub(crate) fn enable_strict_auto_review(&mut self) {
        self.strict_auto_review_enabled = true;
    }

    pub(crate) fn strict_auto_review_enabled(&self) -> bool {
        self.strict_auto_review_enabled
    }
}
