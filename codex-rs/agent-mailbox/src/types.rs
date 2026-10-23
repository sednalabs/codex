use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::ThreadId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRef {
    pub id: String,
    pub issuer: ThreadId,
    pub agent: ThreadId,
    pub submission_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeWork {
    pub native_key: String,
    pub issuer: ThreadId,
    pub agent: ThreadId,
    pub submission_id: String,
    pub input: InterAgentCommunication,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Note,
    Progress,
    Request,
    Alert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationDisposition {
    NotRequested,
    AlreadyActive,
    Activated,
    Denied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivationDecision {
    Activate,
    AlreadyDecided(ActivationDisposition),
    RecoveryNeeded,
}

impl Default for MessageKind {
    fn default() -> Self {
        Self::Note
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageDraft {
    pub invocation_key: String,
    pub sender: ThreadId,
    pub recipient: ThreadId,
    pub communication: InterAgentCommunication,
    pub kind: MessageKind,
    pub activate_if_idle: bool,
    pub work_ref: Option<WorkRef>,
    pub reply_to: Option<String>,
    pub supersedes: Option<String>,
    pub legacy_attention: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    pub invocation_key: String,
    pub sender: ThreadId,
    pub recipient: ThreadId,
    pub communication: InterAgentCommunication,
    pub kind: MessageKind,
    pub activate_if_idle: bool,
    pub work_ref: Option<WorkRef>,
    pub reply_to: Option<String>,
    pub supersedes: Option<String>,
    pub legacy_attention: bool,
    pub canonical: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Failed,
    Cancelled,
    NeedsAttention,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "goal_id", rename_all = "snake_case")]
pub enum Continuation {
    None,
    PendingGoalBinding(GoalBinding),
    Deferred(GoalBinding),
    Terminal(GoalBinding),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalBinding {
    pub goal_id: String,
    pub parent_thread_id: ThreadId,
    pub child_thread_id: ThreadId,
    pub control_generation: u64,
    pub revision: u64,
    pub source_turn_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Accepted,
    Dispatching,
    Dispatched,
    RecoveryNeeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchDecision {
    Dispatch,
    AlreadyDispatched,
    RecoveryNeeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionReceipt {
    pub work_refs: Vec<WorkRef>,
    pub already_recorded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeReceipt {
    pub work_ref: WorkRef,
    pub id: String,
    pub outcome: Outcome,
    pub continuation: Continuation,
    pub response: Option<InterAgentCommunication>,
    pub response_message_id: Option<String>,
    pub canonical: bool,
    pub observed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnOutcomeReceipt {
    pub outcomes: Vec<OutcomeReceipt>,
    pub continuation_work: Vec<WorkRef>,
    pub no_matching_work: bool,
    pub already_recorded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ObservationTarget {
    Work(WorkRef),
    Reply(String),
    Attention(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attention {
    pub id: String,
    pub recipient: ThreadId,
    pub canonical: bool,
    pub observed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPage {
    pub items: Vec<Envelope>,
    pub next_cursor: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalConfirmation {
    /// The adapter appended and flushed this stable ID to canonical history.
    AppendedAndFlushed,
    /// The adapter found this stable ID already present and verified the flush.
    AlreadyPresentAndFlushed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnWhen {
    Any,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    Work(String),
    Reply(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestDisposition {
    Open,
    Replied,
    Superseded,
    Withdrawn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestSnapshot {
    pub request_id: String,
    pub request: Envelope,
    pub reply: Option<Envelope>,
    pub disposition: RequestDisposition,
    pub replacement: Option<String>,
    pub observed: bool,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub caller: ThreadId,
    pub agent: Option<ThreadId>,
    pub selectors: Vec<Selector>,
    pub return_when: ReturnWhen,
    pub limit: usize,
}

pub type SnapshotRequest = Selection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotState {
    Pending,
    AlreadySatisfied,
    NoMatchingWork,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotItem {
    Work(WorkSnapshot),
    Request(RequestSnapshot),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkSnapshot {
    pub work_ref: WorkRef,
    pub input: InterAgentCommunication,
    pub dispatch_state: DispatchState,
    pub outcome: Option<OutcomeReceipt>,
    pub continuations: Vec<ContinuationReceipt>,
    pub observed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationState {
    PendingGoalBinding,
    Deferred,
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuationReceipt {
    pub binding: GoalBinding,
    pub state: ContinuationState,
    pub outcome: Outcome,
    pub source_turn_id: String,
    pub response_message_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub state: SnapshotState,
    pub items: Vec<SnapshotItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    pub owner: String,
    pub peers: Vec<String>,
    pub mode: DependencyMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyMode {
    Any,
    All,
}
