//! Coordination of one agent tree, independent of where its threads run.
//!
//! The trait and its requests use shared agent types and captured settings. Live threads
//! and turn contexts stay in the runtime. Implementations own membership, loading,
//! delivery and shared resources. These Rust contracts do not define a wire protocol.

use crate::agent::types::AgentExecutionGuard;
use crate::agent::types::AgentMessage;
use crate::agent::types::AgentMetadata;
use crate::agent::types::LiveAgent;
use crate::agent::types::MessageDeliveryMode;
use crate::agent::types::SpawnAgentOptions;
use crate::codex_thread::GuardianRootSnapshot;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::rollout_budget::RolloutBudgetReminder;
use codex_agent_mailbox::MessageKind;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::Result;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use codex_rollout_trace::ThreadTraceContext;
use futures::future::BoxFuture;

// Keep dynamic dispatch a compile-time property of the contract.
const _: Option<&dyn AgentControl> = None;

/// Coordinates agent operations and shared state through a local or host backend.
///
/// Implementations preserve the existing wake modes and keep loading and delivery behind
/// complete operations. Successful delivery means accepted, not read by the model. The
/// local backend retains its current best-effort reporting and runtime observation policy;
/// remote ownership, retries and recovery are separate backend work.
/// Boxed Send futures allow callers to use `Arc<dyn AgentControl>`.
pub trait AgentControl: Send + Sync {
    fn identity(&self) -> SessionId;

    /// Read an existing native reference name without loading the thread.
    fn agent_metadata(&self, agent: ThreadId) -> Option<AgentMetadata> {
        let _ = agent;
        None
    }

    /// Resolve an ID or a name relative to the caller's captured source, without loading.
    /// The local backend lazily registers callers with no parent before resolving, including
    /// for direct IDs. Keeping resolution separate preserves tool error and analytics ordering.
    fn resolve<'a>(
        &'a self,
        caller: ThreadId,
        parent: Option<ThreadId>,
        source: &'a SessionSource,
        target: &'a str,
    ) -> BoxFuture<'a, Result<ThreadId>>;

    /// Start a child and accept its initial input, returning its effective settings.
    /// Retain provisional-child ownership until this future returns its acceptance receipt.
    /// Once returned, caller cancellation does not retract the accepted child. Subsequent
    /// tool-result or transport publication belongs to the caller, outside this boundary.
    /// V2 snapshots resolve effort against captured startup metadata without changing the
    /// child's configured effort (for example, Ultra still enables proactive behavior).
    fn spawn(
        &self,
        request: SpawnRequest,
    ) -> BoxFuture<'_, Result<(LiveAgent, ThreadConfigSnapshot)>>;

    /// Resolve, reload if needed and accept input. Agent messages retain their attribution
    /// and wake mode: queue-only messages do not start work and follow-ups cannot target
    /// the root. Legacy user input can address loaded threads outside the agent registry.
    fn send(&self, request: SendRequest) -> BoxFuture<'_, Result<DeliveryReceipt>>;

    /// Take queued, non-turn-starting mail in order, without loading the recipient.
    /// This in-memory operation performs no I/O; returning transfers ownership to the caller.
    fn take_mailbox(&self, agent: ThreadId) -> Vec<crate::PendingAgentMessage>;

    /// Read durable V2 mail without consuming its SQLite record. The returned
    /// copy is only an input candidate; canonical history settlement remains
    /// recipient-owned and must be explicitly acknowledged after flush.
    fn mailbox_pending_inputs(
        &self,
        _agent: ThreadId,
    ) -> BoxFuture<'_, Result<Vec<crate::PendingAgentMessage>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Resolve only exact stable IDs from a provider prompt to durable accepted
    /// inputs. Unrelated AgentMessage history is ignored.
    fn mailbox_inputs_for_ids(
        &self,
        agent: ThreadId,
        ids: Vec<String>,
    ) -> BoxFuture<'_, Result<Vec<crate::PendingAgentMessage>>> {
        let _ = (agent, ids);
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Return durable inputs not yet admitted to a provider request so compaction
    /// can preserve their exact bodies in the replacement history.
    fn mailbox_unadmitted_inputs(
        &self,
        _agent: ThreadId,
    ) -> BoxFuture<'_, Result<Vec<crate::PendingAgentMessage>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Confirm an exact recipient-owned message only after canonical append and
    /// successful rollout flush. Work inputs are settled by provider admission.
    fn mailbox_confirm_input_canonical<'a>(
        &'a self,
        agent: ThreadId,
        id: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        let _ = (agent, id);
        Box::pin(async { Ok(()) })
    }

    /// Record only mailbox-owned IDs present in the provider request accepted at
    /// ResponseEvent::Created.
    fn mailbox_record_admission<'a>(
        &'a self,
        agent: ThreadId,
        turn_id: &'a str,
        ids: Vec<String>,
    ) -> BoxFuture<'a, Result<()>> {
        let _ = (agent, turn_id, ids);
        Box::pin(async { Ok(()) })
    }

    /// Expose the already-open local durable store to native V2 wait tools.
    /// Other backends may leave it unavailable without changing legacy waits.
    fn mailbox_v2(&self) -> Option<std::sync::Arc<codex_agent_mailbox::Mailbox>> {
        None
    }

    /// Open the recipient's local durable store only for an enabled native V2
    /// session; feature-off and nonlocal implementations retain legacy waits.
    fn mailbox_v2_open<'a>(
        &'a self,
        config: &'a Config,
    ) -> BoxFuture<'a, Result<Option<std::sync::Arc<codex_agent_mailbox::Mailbox>>>> {
        let _ = config;
        Box::pin(async { Ok(None) })
    }

    /// Observe whether unread mail is available. Subscribe before the first read so
    /// arrivals cannot be missed; notifications do not consume mail or start a turn.
    fn watch_mailbox(&self, agent: ThreadId) -> tokio::sync::watch::Receiver<bool>;

    /// Load a recorded V2 child through its live immediate parent, without sending input.
    /// Implementations validate ownership and restore the child under the parent's current
    /// authority. Success makes the child available for attachment through its thread manager.
    fn ensure_child_loaded(&self, parent: ThreadId, child: ThreadId) -> BoxFuture<'_, Result<()>>;

    /// Stop current work and return the pre-interrupt snapshot. V2 rejects root/self
    /// targets and tolerates known unloaded agents; other modes retain direct-ID interruption.
    fn interrupt(
        &self,
        caller: ThreadId,
        target: AgentTarget,
        version: MultiAgentVersion,
    ) -> BoxFuture<'_, Result<AgentInfo>>;

    /// List loaded agents using the caller's captured source to resolve a path prefix.
    /// The local backend lazily registers callers with no parent. Callers own formatting.
    fn list<'a>(
        &'a self,
        caller: ThreadId,
        parent: Option<ThreadId>,
        source: &'a SessionSource,
        path_prefix: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<LiveAgent>>>;

    /// Known direct children for V2 model context, including unloaded agents. Loaded
    /// children come first, alphabetically within each group; an unknown parent yields none.
    /// This reads existing membership without registering the parent or loading children.
    fn child_agent_paths(&self, parent: ThreadId) -> BoxFuture<'_, Vec<AgentPath>>;

    /// Check capacity before accepting work. This advisory check does not reserve a slot.
    fn check_turn_admission(
        &self,
        version: MultiAgentVersion,
        source: &SessionSource,
    ) -> Result<()>;

    /// Track a turn's execution until its guard drops. Local admission keeps the existing
    /// separate capacity check and running count; it does not atomically reserve capacity.
    /// Root and non-V2 turns return no guard.
    fn admit_turn(
        &self,
        version: MultiAgentVersion,
        source: &SessionSource,
    ) -> Option<AgentExecutionGuard>;

    /// Account for one inference response, including compaction. Each call records usage;
    /// callers report it once. `SessionBudgetExceeded` means the usage was recorded and
    /// the shared budget is now exhausted.
    fn record_usage(&self, usage: TokenUsage) -> BoxFuture<'_, Result<()>>;

    /// Report the terminal result to the parent and completion activity to the task
    /// initiator. Local delivery remains best effort and uses the reporting runtime's
    /// diagnostic trace; it is not deduplicated.
    fn turn_finished<'a>(
        &'a self,
        outcome: AgentTurnOutcome,
        trace: &'a ThreadTraceContext,
    ) -> BoxFuture<'a, ()>;

    /// Read the latest shared service tier for use at normal runtime config update points.
    fn service_tier(&self) -> Option<String>;

    /// Publish a shared setting synchronously with the runtime's root-owned config update.
    fn propagate_config_update(&self, update: AgentConfigUpdate);

    /// Read the existing bounded root evidence for a worker. The local backend returns
    /// `None` for the root, a non-V2 tree, or an unavailable root runtime.
    fn get_guardian_package(&self, agent: ThreadId) -> BoxFuture<'_, Option<GuardianRootSnapshot>>;

    fn pending_budget_reminder<'a>(
        &'a self,
        agent: ThreadId,
        window: &'a str,
    ) -> BoxFuture<'a, Option<RolloutBudgetReminder>>;

    /// Acknowledge only after inserting the reminder into the agent's history.
    fn mark_budget_reminder_delivered<'a>(
        &'a self,
        agent: ThreadId,
        window: &'a str,
        reminder: RolloutBudgetReminder,
    ) -> BoxFuture<'a, ()>;
}

/// References resolve relative to the registered caller. IDs retain each operation's
/// existing lookup policy, including legacy access to unregistered loaded threads.
#[derive(Clone, Debug)]
pub enum AgentTarget {
    Id(ThreadId),
    Reference(String),
}

/// Observes existing registry metadata and runtime snapshots without loading an agent.
/// A known identity survives unloading; unloaded does not mean completed. Missing agents
/// are operation errors, not loaded snapshots with `AgentStatus::NotFound`.
#[derive(Clone, Debug)]
pub enum AgentInfo {
    Loaded {
        agent: LiveAgent,
        config: Box<ThreadConfigSnapshot>,
    },
    /// Membership is known, but no runtime is loaded. Metadata identifies the known agent.
    Unloaded(AgentMetadata),
}

impl AgentInfo {
    pub fn metadata(&self) -> &AgentMetadata {
        match self {
            Self::Loaded { agent, .. } => &agent.metadata,
            Self::Unloaded(metadata) => metadata,
        }
    }

    pub fn status(&self) -> Option<&AgentStatus> {
        match self {
            Self::Loaded { agent, .. } => Some(&agent.status),
            Self::Unloaded(_) => None,
        }
    }
}

/// User input starts or steers a turn; agent messages retain their sender and wake mode.
pub enum AgentInput {
    UserInput(Vec<UserInput>),
    Message {
        message: AgentMessage,
        mode: MessageDeliveryMode,
        /// Exact native invocation identity for durable V2 acceptance. Legacy and
        /// non-native callers leave this unset and retain their existing delivery path.
        native_key: Option<String>,
        /// Explicit V2 durable message class. Unset retains the default quiet note.
        message_kind: Option<MessageKind>,
        /// Exact stable request-envelope id this message replies to.
        reply_to: Option<String>,
    },
}

pub struct SpawnRequest {
    pub caller: ThreadId,
    pub config: Config,
    /// Spawning starts work; message input must use `TriggerTurn`.
    pub input: AgentInput,
    pub source: SessionSource,
    pub options: SpawnAgentOptions,
}

pub struct SendRequest {
    pub caller: ThreadId,
    pub target: AgentTarget,
    /// Captured caller settings used if the recipient must be restored.
    pub resume_config: Config,
    pub input: AgentInput,
    pub start_options: TurnStartOptions,
}

pub struct DeliveryReceipt {
    pub thread_id: ThreadId,
    /// Recipient identity captured during delivery, for activity and tool output.
    pub metadata: AgentMetadata,
    /// Acceptance identifier, not evidence that the recipient processed the input.
    pub submission_id: String,
}

pub struct AgentTurnOutcome {
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub source: SessionSource,
    pub parent_turn_id: Option<String>,
    pub initiating_agent_path: Option<AgentPath>,
    pub status: AgentStatus,
    /// Typed reason used to choose guidance in the parent notification.
    pub error_info: Option<CodexErrorInfo>,
    /// Goal identity/revision captured when this exact child turn was admitted.
    /// `Err` means capture failed and must not be inferred from terminal state.
    pub goal_control_snapshot_at_start: std::result::Result<AgentGoalControlSnapshot, String>,
    /// Exact lower-level goal projection captured when the terminal event is reduced.
    /// `Err` means the native goal state could not be read and must not be inferred.
    pub logical_goal: std::result::Result<Option<AgentLogicalGoal>, String>,
}

/// Small immutable guard against applying a terminal turn to later goal control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentGoalControlSnapshot {
    pub goal_id: Option<String>,
    pub control_generation: u64,
}

impl AgentGoalControlSnapshot {
    pub(crate) fn matches(&self, goal: Option<&AgentLogicalGoal>) -> bool {
        match (self.goal_id.as_deref(), goal) {
            (Some(goal_id), Some(goal)) => {
                goal.goal_id == goal_id && goal.control_generation == self.control_generation
            }
            (None, None) => self.control_generation == 0,
            // A goal created by the executing turn is a legitimate update; external creation
            // starts at generation one and therefore cannot be mistaken for that case.
            (None, Some(goal)) => self.control_generation == 0 && goal.control_generation == 0,
            (Some(_), None) => false,
        }
    }
}

/// Minimal typed goal state needed to bind durable work continuation. The core
/// goal store is authoritative for identity/status; mailbox policy stays in the
/// adapter rather than rebuilding a second goal-notification controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLogicalGoal {
    pub goal_id: String,
    pub status: ThreadGoalStatus,
    pub control_generation: u64,
}

/// Settings shared by the tree. A service tier of `None` restores the default tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentConfigUpdate {
    ServiceTier(Option<String>),
}
