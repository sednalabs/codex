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
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::Result;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use codex_rollout_trace::ThreadTraceContext;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::broadcast;
use tokio::sync::watch;

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
    fn take_mailbox(
        &self,
        agent: ThreadId,
    ) -> Vec<codex_protocol::protocol::InterAgentCommunication>;

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
    pub readiness: AgentReadiness,
    /// Typed reason used to choose guidance in the parent notification.
    pub error_info: Option<CodexErrorInfo>,
}

/// A goal extension may publish this marker only after accounting the matching turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalTurnMarker {
    pub goal_id: String,
    pub turn_id: String,
    pub readiness: GoalTurnReadiness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalTurnReadiness {
    Continuing,
    ActionRequired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentReadiness {
    Pending,
    GoalContinuing { goal_id: String, turn_id: String },
    ActionRequired,
    Terminal,
}

impl AgentReadiness {
    pub fn wakes_wait(&self) -> bool {
        matches!(self, Self::ActionRequired | Self::Terminal)
    }
}

#[derive(Clone, Debug)]
pub struct AgentOutcomeSnapshot {
    pub turn_id: Option<String>,
    pub status: AgentStatus,
    pub readiness: AgentReadiness,
}

impl Default for AgentOutcomeSnapshot {
    fn default() -> Self {
        Self {
            turn_id: None,
            status: AgentStatus::PendingInit,
            readiness: AgentReadiness::Pending,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentWaitReturnWhen {
    Any,
    All,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentWaitResult {
    pub(crate) all_targets: bool,
    pub(crate) outcomes: Vec<(ThreadId, AgentOutcomeSnapshot)>,
}

struct ActiveAgentWait {
    targets: Vec<ThreadId>,
    return_when: AgentWaitReturnWhen,
    outcomes: HashMap<ThreadId, AgentOutcomeSnapshot>,
    tx: watch::Sender<Option<AgentWaitResult>>,
}

#[derive(Default)]
pub(crate) struct AgentWaitRegistry {
    current_turns: HashMap<ThreadId, String>,
    snapshots: HashMap<ThreadId, AgentOutcomeSnapshot>,
    revisions: HashMap<ThreadId, u64>,
    waits: HashMap<u64, ActiveAgentWait>,
    next_wait_id: u64,
}

pub(crate) type SharedAgentWaitRegistry = Arc<Mutex<AgentWaitRegistry>>;

pub(crate) struct AgentWaitRegistration {
    id: u64,
    registry: SharedAgentWaitRegistry,
    revisions: HashMap<ThreadId, u64>,
    receiver: watch::Receiver<Option<AgentWaitResult>>,
}

impl AgentWaitRegistration {
    pub(crate) fn current(&mut self) -> Option<AgentWaitResult> {
        self.receiver.borrow_and_update().clone()
    }

    pub(crate) async fn changed(
        &mut self,
    ) -> std::result::Result<Option<AgentWaitResult>, watch::error::RecvError> {
        self.receiver.changed().await?;
        Ok(self.current())
    }

    /// Seed a just-registered target from its current raw status without allowing a
    /// stale read to override typed publication that raced the lookup.
    pub(crate) fn seed_raw_status(&mut self, target: ThreadId, status: AgentStatus) {
        let Some(readiness) = raw_terminal_readiness(&status) else {
            return;
        };
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(wait) = registry.waits.get(&self.id) else {
            return;
        };
        if !wait.targets.contains(&target) || wait.outcomes.contains_key(&target) {
            return;
        }
        let revision_changed = registry.revisions.get(&target).copied().unwrap_or_default()
            != self.revisions.get(&target).copied().unwrap_or_default();
        let snapshot = registry.snapshots.get(&target).cloned();
        let outcome = if revision_changed {
            snapshot.filter(|snapshot| snapshot.readiness.wakes_wait())
        } else {
            match snapshot {
                Some(snapshot) if snapshot.status == status && snapshot.readiness.wakes_wait() => {
                    Some(snapshot)
                }
                Some(snapshot) if snapshot.turn_id.is_some() => None,
                _ => Some(AgentOutcomeSnapshot {
                    turn_id: None,
                    status,
                    readiness,
                }),
            }
        };
        if let Some(outcome) = outcome {
            let Some(wait) = registry.waits.get_mut(&self.id) else {
                return;
            };
            wait.outcomes.insert(target, outcome);
            AgentOutcomePublisher::complete_wait(wait);
        }
    }
}

impl Drop for AgentWaitRegistration {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .waits
            .remove(&self.id);
    }
}

/// Per-thread publisher backed by the local tree's shared snapshot/wait registry.
pub struct AgentOutcomePublisher {
    thread_id: ThreadId,
    tx: watch::Sender<AgentOutcomeSnapshot>,
    actionable_tx: broadcast::Sender<AgentOutcomeSnapshot>,
    registry: SharedAgentWaitRegistry,
    last_reported_turn: Mutex<Option<String>>,
}

impl AgentOutcomePublisher {
    pub(crate) fn new(thread_id: ThreadId, registry: SharedAgentWaitRegistry) -> Self {
        let initial = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshots
            .get(&thread_id)
            .cloned()
            .unwrap_or_default();
        let (tx, _) = watch::channel(initial);
        let (actionable_tx, _) = broadcast::channel(64);
        Self {
            thread_id,
            tx,
            actionable_tx,
            registry,
            last_reported_turn: Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> AgentOutcomeSnapshot {
        self.tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<AgentOutcomeSnapshot> {
        self.tx.subscribe()
    }

    pub fn subscribe_actionable(&self) -> broadcast::Receiver<AgentOutcomeSnapshot> {
        self.actionable_tx.subscribe()
    }

    pub fn publish(&self, snapshot: AgentOutcomeSnapshot) -> AgentOutcomeSnapshot {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.publish_locked(&mut registry, snapshot)
    }

    pub(crate) fn publish_interrupted(
        &self,
        mut snapshot: AgentOutcomeSnapshot,
    ) -> AgentOutcomeSnapshot {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = registry
            .snapshots
            .get(&self.thread_id)
            .cloned()
            .unwrap_or_default();
        if previous.turn_id.is_some() && previous.turn_id != snapshot.turn_id {
            return previous;
        }
        if previous.turn_id == snapshot.turn_id
            && previous.readiness.wakes_wait()
            && !snapshot.readiness.wakes_wait()
        {
            snapshot.readiness = previous.readiness;
        }
        self.publish_locked(&mut registry, snapshot)
    }

    fn publish_locked(
        &self,
        registry: &mut AgentWaitRegistry,
        snapshot: AgentOutcomeSnapshot,
    ) -> AgentOutcomeSnapshot {
        if let Some(turn_id) = snapshot.turn_id.as_ref() {
            if snapshot.status == AgentStatus::Running
                && matches!(snapshot.readiness, AgentReadiness::Pending)
            {
                registry
                    .current_turns
                    .insert(self.thread_id, turn_id.clone());
            } else {
                match registry.current_turns.entry(self.thread_id) {
                    std::collections::hash_map::Entry::Occupied(entry)
                        if entry.get() != turn_id =>
                    {
                        return registry
                            .snapshots
                            .get(&self.thread_id)
                            .cloned()
                            .unwrap_or_default();
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {}
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(turn_id.clone());
                    }
                }
            }
        }
        registry.snapshots.insert(self.thread_id, snapshot.clone());
        let revision = registry.revisions.entry(self.thread_id).or_default();
        *revision = revision.wrapping_add(1);
        self.tx.send_replace(snapshot.clone());
        if snapshot.readiness.wakes_wait() {
            let _ = self.actionable_tx.send(snapshot.clone());
            for wait in registry.waits.values_mut() {
                if wait.targets.contains(&self.thread_id) {
                    wait.outcomes
                        .entry(self.thread_id)
                        .or_insert_with(|| snapshot.clone());
                    Self::complete_wait(wait);
                }
            }
        }
        snapshot
    }

    fn complete_wait(wait: &ActiveAgentWait) {
        if wait.tx.borrow().is_some() {
            return;
        }
        let all_ready = wait
            .targets
            .iter()
            .all(|target| wait.outcomes.contains_key(target));
        if wait.return_when == AgentWaitReturnWhen::All && !all_ready {
            return;
        }
        if wait.outcomes.is_empty() {
            return;
        }
        let outcomes = wait
            .targets
            .iter()
            .filter_map(|target| {
                wait.outcomes
                    .get(target)
                    .cloned()
                    .map(|outcome| (*target, outcome))
            })
            .collect();
        let _ = wait.tx.send_replace(Some(AgentWaitResult {
            all_targets: wait.return_when == AgentWaitReturnWhen::All,
            outcomes,
        }));
    }

    pub fn mark_reported(&self, turn_id: &str) -> bool {
        let mut last = self
            .last_reported_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.as_deref() == Some(turn_id) {
            return false;
        }
        *last = Some(turn_id.to_string());
        true
    }
}

pub(crate) fn register_agent_wait(
    registry: &SharedAgentWaitRegistry,
    targets: Vec<ThreadId>,
    return_when: AgentWaitReturnWhen,
) -> AgentWaitRegistration {
    let mut state = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut id = state.next_wait_id.max(1);
    while state.waits.contains_key(&id) {
        id = id.wrapping_add(1).max(1);
    }
    state.next_wait_id = id.wrapping_add(1).max(1);
    let revisions = targets
        .iter()
        .map(|target| {
            (
                *target,
                state.revisions.get(target).copied().unwrap_or_default(),
            )
        })
        .collect();
    let (tx, receiver) = watch::channel(None);
    let wait = ActiveAgentWait {
        targets,
        return_when,
        outcomes: HashMap::new(),
        tx,
    };
    state.waits.insert(id, wait);
    AgentWaitRegistration {
        id,
        registry: registry.clone(),
        revisions,
        receiver,
    }
}

fn raw_terminal_readiness(status: &AgentStatus) -> Option<AgentReadiness> {
    match status {
        AgentStatus::Completed(_) | AgentStatus::Shutdown => Some(AgentReadiness::Terminal),
        AgentStatus::Errored(_) | AgentStatus::NotFound => Some(AgentReadiness::ActionRequired),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted => None,
    }
}

/// Settings shared by the tree. A service tier of `None` restores the default tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentConfigUpdate {
    ServiceTier(Option<String>),
}

#[cfg(test)]
mod agent_wait_registry_tests {
    use super::*;

    fn snapshot(
        turn_id: &str,
        status: AgentStatus,
        readiness: AgentReadiness,
    ) -> AgentOutcomeSnapshot {
        AgentOutcomeSnapshot {
            turn_id: Some(turn_id.to_string()),
            status,
            readiness,
        }
    }

    #[test]
    fn active_wait_latches_terminal_before_later_turn_start() {
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let target = ThreadId::new();
        let publisher = AgentOutcomePublisher::new(target, registry.clone());
        publisher.publish(snapshot(
            "turn-1",
            AgentStatus::Running,
            AgentReadiness::Pending,
        ));
        let mut active = register_agent_wait(&registry, vec![target], AgentWaitReturnWhen::Any);

        publisher.publish(snapshot(
            "turn-1",
            AgentStatus::Completed(None),
            AgentReadiness::Terminal,
        ));
        publisher.publish(snapshot(
            "turn-2",
            AgentStatus::Running,
            AgentReadiness::Pending,
        ));
        publisher.publish(snapshot(
            "turn-1",
            AgentStatus::Completed(None),
            AgentReadiness::Terminal,
        ));

        let result = active.current().expect("active wait retains turn-1 result");
        assert_eq!(result.outcomes.len(), 1);
        assert_eq!(result.outcomes[0].1.turn_id.as_deref(), Some("turn-1"));
        let mut fresh = register_agent_wait(&registry, vec![target], AgentWaitReturnWhen::Any);
        assert!(
            fresh.current().is_none(),
            "fresh wait must not inherit turn-1 result"
        );
        assert_eq!(publisher.snapshot().turn_id.as_deref(), Some("turn-2"));
        drop(active);
        assert_eq!(registry.lock().unwrap().waits.len(), 1);
        drop(fresh);
        assert!(registry.lock().unwrap().waits.is_empty());
    }

    #[test]
    fn registration_seeds_existing_actionable_snapshot() {
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let target = ThreadId::new();
        let publisher = AgentOutcomePublisher::new(target, registry.clone());
        publisher.publish(snapshot(
            "turn-1",
            AgentStatus::Errored("failed".to_string()),
            AgentReadiness::ActionRequired,
        ));

        let mut wait = register_agent_wait(&registry, vec![target], AgentWaitReturnWhen::Any);
        wait.seed_raw_status(target, AgentStatus::Errored("failed".to_string()));
        let result = wait
            .current()
            .expect("registration seeds actionable snapshot");
        assert_eq!(
            result.outcomes[0].1.readiness,
            AgentReadiness::ActionRequired
        );
    }

    #[test]
    fn raw_terminal_seed_is_unbound_and_does_not_claim_goal_identity() {
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let target = ThreadId::new();
        let mut wait = register_agent_wait(&registry, vec![target], AgentWaitReturnWhen::Any);
        wait.seed_raw_status(
            target,
            AgentStatus::Completed(Some("restored result".to_string())),
        );

        let result = wait
            .current()
            .expect("restored terminal status is observable");
        assert_eq!(result.outcomes[0].1.turn_id.as_deref(), None);
        assert_eq!(result.outcomes[0].1.readiness, AgentReadiness::Terminal);
        assert!(matches!(
            &result.outcomes[0].1.status,
            AgentStatus::Completed(_)
        ));
    }

    #[test]
    fn live_turn_reset_wins_over_stale_raw_terminal_seed() {
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let target = ThreadId::new();
        let publisher = AgentOutcomePublisher::new(target, registry.clone());
        publisher.publish(snapshot(
            "old-turn",
            AgentStatus::Completed(None),
            AgentReadiness::Terminal,
        ));
        let mut wait = register_agent_wait(&registry, vec![target], AgentWaitReturnWhen::Any);
        publisher.publish(snapshot(
            "new-turn",
            AgentStatus::Running,
            AgentReadiness::Pending,
        ));
        wait.seed_raw_status(target, AgentStatus::Completed(None));
        assert!(
            wait.current().is_none(),
            "old raw final cannot survive a typed new-turn reset"
        );

        publisher.publish(snapshot(
            "new-turn",
            AgentStatus::Completed(None),
            AgentReadiness::Terminal,
        ));
        assert_eq!(
            wait.current().expect("new final is latched").outcomes[0]
                .1
                .turn_id
                .as_deref(),
            Some("new-turn")
        );
    }

    #[test]
    fn quiet_goal_snapshot_is_not_promoted_by_raw_completed_status() {
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let target = ThreadId::new();
        let publisher = AgentOutcomePublisher::new(target, registry.clone());
        publisher.publish(snapshot(
            "goal-turn",
            AgentStatus::Completed(None),
            AgentReadiness::GoalContinuing {
                goal_id: "goal-1".to_string(),
                turn_id: "goal-turn".to_string(),
            },
        ));
        let mut wait = register_agent_wait(&registry, vec![target], AgentWaitReturnWhen::Any);
        wait.seed_raw_status(target, AgentStatus::Completed(None));

        assert!(
            wait.current().is_none(),
            "raw status cannot erase trusted quiet-goal binding"
        );
    }

    #[test]
    fn all_wait_latches_each_target_until_all_are_actionable() {
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let first = ThreadId::new();
        let second = ThreadId::new();
        let first_publisher = AgentOutcomePublisher::new(first, registry.clone());
        let second_publisher = AgentOutcomePublisher::new(second, registry.clone());
        first_publisher.publish(snapshot(
            "first-turn",
            AgentStatus::Running,
            AgentReadiness::Pending,
        ));
        second_publisher.publish(snapshot(
            "second-turn",
            AgentStatus::Running,
            AgentReadiness::Pending,
        ));
        let mut wait =
            register_agent_wait(&registry, vec![first, second], AgentWaitReturnWhen::All);

        first_publisher.publish(snapshot(
            "first-turn",
            AgentStatus::Completed(None),
            AgentReadiness::Terminal,
        ));
        assert!(wait.current().is_none());
        second_publisher.publish(snapshot(
            "second-turn",
            AgentStatus::Errored("failed".to_string()),
            AgentReadiness::ActionRequired,
        ));
        let result = wait.current().expect("all targets are latched");
        assert!(result.all_targets);
        assert_eq!(result.outcomes.len(), 2);
    }

    #[test]
    fn goal_continuation_is_quiet_but_action_required_wakes_wait() {
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let target = ThreadId::new();
        let publisher = AgentOutcomePublisher::new(target, registry.clone());
        publisher.publish(snapshot(
            "turn-1",
            AgentStatus::Running,
            AgentReadiness::Pending,
        ));
        let mut wait = register_agent_wait(&registry, vec![target], AgentWaitReturnWhen::Any);
        publisher.publish(snapshot(
            "turn-1",
            AgentStatus::Interrupted,
            AgentReadiness::Pending,
        ));
        assert!(
            wait.current().is_none(),
            "ordinary interruption remains quiet"
        );
        publisher.publish(snapshot(
            "turn-1",
            AgentStatus::Completed(None),
            AgentReadiness::GoalContinuing {
                goal_id: "goal-1".to_string(),
                turn_id: "turn-1".to_string(),
            },
        ));
        assert!(wait.current().is_none());
        publisher.publish(snapshot(
            "turn-1",
            AgentStatus::Interrupted,
            AgentReadiness::ActionRequired,
        ));
        assert_eq!(
            wait.current().expect("action required wakes").outcomes[0]
                .1
                .readiness,
            AgentReadiness::ActionRequired
        );
    }

    #[test]
    fn publication_racing_registration_is_seeded_or_latched() {
        use std::sync::Barrier;
        use std::thread;

        for _ in 0..32 {
            let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
            let target = ThreadId::new();
            let publisher = Arc::new(AgentOutcomePublisher::new(target, registry.clone()));
            publisher.publish(snapshot(
                "turn-1",
                AgentStatus::Running,
                AgentReadiness::Pending,
            ));
            let barrier = Arc::new(Barrier::new(3));
            let register_registry = registry.clone();
            let register_barrier = barrier.clone();
            let register_thread = thread::spawn(move || {
                register_barrier.wait();
                register_agent_wait(&register_registry, vec![target], AgentWaitReturnWhen::Any)
            });
            let publish_barrier = barrier.clone();
            let publish_thread = thread::spawn(move || {
                publish_barrier.wait();
                publisher.publish(snapshot(
                    "turn-1",
                    AgentStatus::Completed(None),
                    AgentReadiness::Terminal,
                ));
            });
            barrier.wait();
            let mut registration = register_thread.join().expect("wait registration");
            publish_thread.join().expect("outcome publication");
            assert!(
                registration.current().is_some(),
                "concurrent terminal outcome cannot be lost"
            );
        }
    }
}
