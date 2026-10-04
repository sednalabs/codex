//! Typed, content-free observations; join identifiers are never shortened.
use std::time::SystemTime;

use super::usage::UsageAccountScope;

pub const MAX_RECORDS: usize = 2048;
pub const MAX_OWNED_DYNAMIC_BYTES: usize = 2_097_152;
pub const MAX_IDENTIFIER_BYTES: usize = 256;
pub const MAX_TARGETS_PER_EVENT: usize = 64;
pub const MAX_COVERAGE_MARKS_PER_EVENT: usize = 64;
/// Literal identifying the accepted Session completion observation seam.
pub const PROVIDER_COMPLETION_PRODUCER_BOUNDARY: &str = "session.record_observed_response_completed";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureMode { Off, Session }
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SourcePlane { RustCollab, AppServer, ExternalHostEnvelope, ExistingUsageLedger, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationQuality { Owned, ObservedOnly, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum EventKind {
    OperationValidated, WaitSubscribed, WaitBlockingStarted, WaitSelected, WaitCompleted,
    WaitAbandoned, SleepSelected, MessageAccepted, MessageEnqueued, MessageDrained,
    InputRecorded, ActivityPublished, OutcomePublished, StatusQueryObserved,
    WindowBoundaryObserved, RecoveryBoundaryObserved, SchedulerEligibilityObserved,
    TaskRegistered, TurnStartPublicationObserved, ProviderCompletionObserved, Unknown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum WaitPrimitive { V2Wait, ClockSleep, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum TargetMode { Targeted, Untargeted, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ReturnWhen { Any, All, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum TargetReferenceKind { ExposedAgentPath, ThreadId, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SelectedOutcome {
    TargetTerminal, TargetActionRequired, MailboxTurnRequested, OperatorSteer, Timeout,
    ExplicitCancellation, UnknownAbandoned, UnknownStreamLoss, SleepInterrupted,
    SleepCompleted, Unknown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitPhase { Validated, Subscribed, Blocking, Selected, Completed, Abandoned, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryIntent { QueueOnly, TriggerTurn, Result, Steer, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SleepPendingActivity { QueueOnly, TriggerTurn, Steer, Other, Unknown, None }
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Readiness { Pending, GoalContinuing, Terminal, ActionRequired, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueScope { TurnLocal, Session, CompositeNonAtomic, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverageField {
    ProducerVersion, OperationId, ThreadId, TurnId, RootThreadId, ParentThreadId,
    ForkParentThreadId, WindowId, RequestedTimeout, EffectiveTimeout, TargetSet,
    SelectedProducer, QueueState, SemanticAcknowledgement, ProviderObservedIdentity, Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnknownReason { NotExposed, UnsupportedProducer, PreCapture, Evicted, Lost, Truncated, Legacy, Ambiguous }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoverageMark { pub field: CoverageField, pub unknown: Option<UnknownReason> }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SleepSelection { AlreadyPending, ActivityChanged, TimeProviderCompleted, TimeProviderFailed, AbandonedUnknown, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EligibilityBasis { PendingTriggerTurn, QueueOnlyDurableSleep, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerOutcome { NotEligibleObserved, ActiveTurnPresentObserved, ReservationLostObserved, TaskRegistered, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerPhase { Eligibility, ReservationAccepted, ReservationLost, CohortDrained, TaskRegistered, TurnStartProducerPublication, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FollowupSelection { Kept, Replaced, Unknown }
/// Receipt emitted at the existing provider-completion observation boundary.
/// It is not evidence that a provider result was durably stored unless the
/// outcome is `Inserted` and a persisted call identity is present.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderLedgerWriteOutcome { Inserted, Duplicate, FailedUnknown, NotConfigured }

/// Stable event identity within one recorder lifetime; not an agent identity.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct EventIdentity { pub capture_instance_id: String, pub sequence: u64 }
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct ReadinessObservation { pub state: Readiness, pub target_turn_id: Option<String> }
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusQueryObservation {
    /// Fingerprints contain only allowlisted metadata, never body content.
    pub request_fingerprint: Option<String>,
    pub result_fingerprint: Option<String>,
    pub readiness: Option<ReadinessObservation>,
    pub request_projection: Option<StatusRequestProjection>,
    pub result_projection: Option<StatusResultProjection>,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StatusRequestProjection {
    pub path_prefix: Option<String>,
    pub requested_agent_ids: Vec<String>,
    pub complete: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StatusActorProjection {
    pub agent_id: String,
    pub canonical_path: String,
    pub configured_model: Option<String>,
    pub configured_reasoning_effort: Option<String>,
    pub raw_status_tag: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StatusResultProjection {
    pub observed_actor_count: Option<u64>,
    /// Canonically ordered allowlisted actor rows; never message content.
    pub actors: Vec<StatusActorProjection>,
    pub complete: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueObservation { pub scope: QueueScope, pub transition_sequence: Option<u64>, pub queue_before: Option<u64>, pub queue_after: Option<u64> }
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SleepObservation {
    pub pending_activity: SleepPendingActivity, pub selection: SleepSelection,
    pub requested_duration_ns: Option<u64>, pub operation_duration_ns: Option<u64>,
    pub blocked_duration_ns: Option<u64>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitObservation {
    pub wait_id: String, pub phase: WaitPhase, pub primitive: WaitPrimitive,
    pub return_when: ReturnWhen,
    pub helper_id: Option<String>, pub helper_version: Option<String>,
    pub requested_timeout_ms: Option<i64>, pub effective_timeout_ms: Option<u64>,
    pub target_mode: TargetMode, pub any_targets: Option<bool>,
    pub target_ids: Vec<String>, pub target_set_complete: bool,
    pub resolved_target_kind: TargetReferenceKind,
    pub requested_target_ids: Vec<String>, pub requested_target_kind: TargetReferenceKind,
    pub requested_target_set_complete: bool, pub resolved_target_set_complete: Option<bool>,
    pub subscribed_readiness: Vec<ReadinessObservation>,
    pub selected_readiness: Vec<ReadinessObservation>,
    pub blocked_start_offset_ns: Option<u64>, pub blocked_end_offset_ns: Option<u64>,
    pub operation_duration_ns: Option<u64>, pub blocked_duration_ns: Option<u64>,
    pub selected_outcome: SelectedOutcome, pub selected_producer: Option<EventIdentity>,
    pub selected_target_id: Option<String>, pub selected_target_turn_id: Option<String>,
    pub continuation_of_wait_id: Option<String>,
    pub request_fingerprint: Option<String>, pub result_fingerprint: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageObservation {
    pub submission_id: Option<String>, pub sender_thread_id: Option<String>,
    pub recipient_thread_id: Option<String>, pub intent: DeliveryIntent,
    pub accepted_event: Option<EventIdentity>, pub enqueued_event: Option<EventIdentity>,
    pub drained_event: Option<EventIdentity>, pub input_recorded_event: Option<EventIdentity>,
    pub activity_event: Option<EventIdentity>, pub selected_by_waits: Vec<String>,
    pub scheduled_turn_id: Option<String>, pub semantic_acknowledgement: Option<bool>,
    pub followup_selection: FollowupSelection, pub superseded_submission_ids: Vec<String>,
    pub superseded_selection_complete: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchedulerObservation {
    pub primitive: String, pub correlation_id: Option<String>,
    pub pending_mail_observed: Option<bool>, pub trigger_turn_mail_observed: Option<bool>,
    pub durable_sleep_observed: Option<bool>, pub idle_reservation_accepted: Option<bool>,
    pub reservation_still_matches: Option<bool>, pub eligibility_basis: EligibilityBasis,
    pub drained_message_cohort_ids: Vec<String>, pub drained_cohort_complete: bool,
    pub drained_cohort_contains_trigger_turn_mail: Option<bool>, pub phase: SchedulerPhase,
    pub task_registration_event: Option<EventIdentity>, pub scheduled_turn_id: Option<String>,
    pub turn_start_publication_event: Option<EventIdentity>, pub outcome: SchedulerOutcome,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalOperationObservation {
    pub call_id: String,
    /// Host-observed request/return duration, never actual blocked duration.
    pub returned_duration_ns: Option<u64>,
}
/// Allowlisted provider-completion identifiers and the existing ledger
/// writer's receipt; response bodies and usage amounts are intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCallObservation {
    pub provider: String,
    pub response_id: String,
    /// Present only when the existing ledger writer returned Inserted.
    pub persisted_provider_call_id: Option<String>,
    /// Distinguishes an explicitly unscoped writer row from unavailable scope.
    pub ledger_response_scope: UsageAccountScope,
    pub ledger_write_outcome: ProviderLedgerWriteOutcome,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventInput {
    pub source_plane: SourcePlane, pub producer_boundary: String,
    pub producer_version: Option<String>, pub kind: EventKind, pub quality: ObservationQuality,
    pub wall_correlation: SystemTime, pub monotonic_offset_ns: Option<u64>,
    pub operation_id: Option<String>, pub thread_id: Option<String>, pub turn_id: Option<String>,
    pub root_thread_id: Option<String>, pub parent_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>, pub window_id: Option<String>,
    pub window_number: Option<u64>, pub previous_window_id: Option<String>,
    pub wait: Option<WaitObservation>, pub sleep: Option<SleepObservation>,
    pub readiness: Option<ReadinessObservation>, pub status_query: Option<StatusQueryObservation>,
    pub queue: Option<QueueObservation>, pub field_coverage: Vec<CoverageMark>,
    pub message: Option<MessageObservation>, pub scheduler: Option<SchedulerObservation>,
    pub external_operation: Option<ExternalOperationObservation>,
    pub provider_call: Option<ProviderCallObservation>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedEvent { pub identity: EventIdentity, pub input: EventInput, pub truncated: bool }
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LossCounts { pub contention: u64, pub capacity: u64, pub disabled: u64, pub invalid_identity: u64 }
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecorderSnapshot {
    pub capture_instance_id: String,
    /// Gaps may represent loss or an allocated but not yet appended event.
    pub high_water: u64,
    pub events: Vec<RecordedEvent>, pub losses: LossCounts,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidCaptureId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SleepTimeline {
    pub capture_instance_id: String,
    pub source_plane: SourcePlane,
    pub thread_id: Option<String>,
    pub operation_id: Option<String>,
    pub pending_activity: SleepPendingActivity,
    pub selection: SleepSelection,
    pub requested_duration_ns: Option<u64>,
    pub operation_duration_ns: Option<u64>,
    pub blocked_duration_ns: Option<u64>,
    pub complete: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchedulerTimeline {
    pub capture_instance_id: String,
    pub source_plane: SourcePlane,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub correlation_id: Option<String>,
    pub eligible: Option<bool>,
    pub eligibility_basis: EligibilityBasis,
    pub pending_mail_observed: Option<bool>,
    pub trigger_turn_mail_observed: Option<bool>,
    pub durable_sleep_observed: Option<bool>,
    pub reservation_accepted: Option<bool>,
    pub reservation_still_matches: Option<bool>,
    pub reservation_lost: Option<bool>,
    pub task_registered: Option<bool>,
    pub turn_start_published: Option<bool>,
    pub drained_cohort_ids: Vec<String>,
    pub drained_cohort_complete: bool,
    pub drained_cohort_contains_trigger_turn_mail: Option<bool>,
    pub outcome: SchedulerOutcome,
    pub complete: bool,
    pub conflicting: bool,
    pub incomplete: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalDuration {
    pub identity: EventIdentity,
    pub source_plane: SourcePlane,
    pub thread_id: Option<String>,
    pub operation_id: Option<String>,
    pub call_id: String,
    pub observed_request_return_ns: Option<u64>,
    pub quality: ObservationQuality,
    pub conflicting: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueTimeline {
    pub capture_instance_id: String,
    pub source_plane: SourcePlane,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub observation: QueueObservation,
}
