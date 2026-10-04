//! Bounded, content-free observations for repository-owned control-plane paths.
//!
//! This module is a recorder/reducer only. Producers must report the branch
//! they actually observed; this API does not infer causality from timing.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

pub const MAX_RECORDS: usize = 2048;
pub const MAX_OWNED_DYNAMIC_BYTES: usize = 2_097_152;
pub const MAX_IDENTIFIER_BYTES: usize = 256;
pub const MAX_TARGETS_PER_EVENT: usize = 64;
pub const MAX_COVERAGE_MARKS_PER_EVENT: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureMode {
    Off,
    Session,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SourcePlane {
    RustCollab,
    AppServer,
    ExternalHostEnvelope,
    ExistingUsageLedger,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationQuality {
    Owned,
    ObservedOnly,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum EventKind {
    OperationValidated,
    WaitSubscribed,
    WaitBlockingStarted,
    WaitSelected,
    WaitCompleted,
    WaitAbandoned,
    SleepSelected,
    MessageAccepted,
    MessageEnqueued,
    MessageDrained,
    InputRecorded,
    ActivityPublished,
    OutcomePublished,
    StatusQueryObserved,
    WindowBoundaryObserved,
    RecoveryBoundaryObserved,
    SchedulerEligibilityObserved,
    TaskRegistered,
    TurnStartPublicationObserved,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitPrimitive {
    V2Wait,
    ClockSleep,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetMode {
    Targeted,
    Untargeted,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SelectedOutcome {
    TargetTerminal,
    TargetActionRequired,
    MailboxTurnRequested,
    OperatorSteer,
    Timeout,
    ExplicitCancellation,
    UnknownAbandoned,
    UnknownStreamLoss,
    SleepInterrupted,
    SleepCompleted,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitPhase {
    Validated,
    Subscribed,
    Blocking,
    Selected,
    Completed,
    Abandoned,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryIntent {
    QueueOnly,
    TriggerTurn,
    Result,
    Steer,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SleepPendingActivity {
    QueueOnly,
    TriggerTurn,
    Steer,
    Other,
    Unknown,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
    Pending,
    GoalContinuing,
    Terminal,
    ActionRequired,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadinessObservation {
    pub state: Readiness,
    pub target_turn_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusQueryObservation {
    /// A content-free fingerprint of the allowlisted request metadata.
    pub request_fingerprint: String,
    /// Absent when the returned readiness metadata was not exposed.
    pub result_fingerprint: Option<String>,
    pub readiness: Option<ReadinessObservation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueScope {
    TurnLocal,
    Session,
    CompositeNonAtomic,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueObservation {
    pub scope: QueueScope,
    pub transition_sequence: Option<u64>,
    pub queue_before: Option<u64>,
    pub queue_after: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverageField {
    ProducerVersion,
    OperationId,
    ThreadId,
    TurnId,
    RootThreadId,
    ParentThreadId,
    ForkParentThreadId,
    WindowId,
    RequestedTimeout,
    EffectiveTimeout,
    TargetSet,
    SelectedProducer,
    QueueState,
    SemanticAcknowledgement,
    ProviderObservedIdentity,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnknownReason {
    NotExposed,
    UnsupportedProducer,
    PreCapture,
    Evicted,
    Lost,
    Truncated,
    Legacy,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoverageMark {
    pub field: CoverageField,
    pub unknown: Option<UnknownReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SleepSelection {
    AlreadyPending,
    ActivityChanged,
    TimeProviderCompleted,
    TimeProviderFailed,
    AbandonedUnknown,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SleepObservation {
    pub pending_activity: SleepPendingActivity,
    pub selection: SleepSelection,
    pub requested_duration_ns: Option<u64>,
    pub operation_duration_ns: Option<u64>,
    pub blocked_duration_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EligibilityBasis {
    PendingTriggerTurn,
    QueueOnlyDurableSleep,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerOutcome {
    NotEligibleObserved,
    ActiveTurnPresentObserved,
    ReservationLostObserved,
    TaskRegistered,
    Unknown,
}

/// Stable identity within one recorder lifetime; it is not an agent identity.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct EventIdentity {
    pub capture_instance_id: String,
    pub sequence: u64,
}

/// Allowlisted metadata for one observed event. Missing fields remain absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventInput {
    pub source_plane: SourcePlane,
    pub producer_boundary: String,
    pub producer_version: Option<String>,
    pub kind: EventKind,
    pub quality: ObservationQuality,
    pub wall_correlation: SystemTime,
    pub monotonic_offset_ns: Option<u64>,
    pub operation_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub root_thread_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>,
    pub window_id: Option<String>,
    pub window_number: Option<u64>,
    pub previous_window_id: Option<String>,
    pub wait: Option<WaitObservation>,
    pub sleep: Option<SleepObservation>,
    pub readiness: Option<ReadinessObservation>,
    pub status_query: Option<StatusQueryObservation>,
    pub queue: Option<QueueObservation>,
    pub field_coverage: Vec<CoverageMark>,
    pub message: Option<MessageObservation>,
    pub scheduler: Option<SchedulerObservation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitObservation {
    pub wait_id: String,
    pub phase: WaitPhase,
    pub primitive: WaitPrimitive,
    pub requested_timeout_ms: Option<i64>,
    pub effective_timeout_ms: Option<u64>,
    pub any_targets: Option<bool>,
    pub target_ids: Vec<String>,
    pub target_set_complete: bool,
    pub target_mode: TargetMode,
    pub blocked_start_offset_ns: Option<u64>,
    pub blocked_end_offset_ns: Option<u64>,
    pub operation_duration_ns: Option<u64>,
    pub blocked_duration_ns: Option<u64>,
    pub selected_outcome: SelectedOutcome,
    pub selected_producer: Option<EventIdentity>,
    pub selected_target_id: Option<String>,
    pub continuation_of_wait_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageObservation {
    pub submission_id: Option<String>,
    pub sender_thread_id: Option<String>,
    pub recipient_thread_id: Option<String>,
    pub intent: DeliveryIntent,
    pub accepted_event: Option<EventIdentity>,
    pub enqueued_event: Option<EventIdentity>,
    pub drained_event: Option<EventIdentity>,
    pub input_recorded_event: Option<EventIdentity>,
    pub activity_event: Option<EventIdentity>,
    pub selected_by_waits: Vec<String>,
    pub scheduled_turn_id: Option<String>,
    pub semantic_acknowledgement: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchedulerObservation {
    pub primitive: String,
    pub correlation_id: Option<String>,
    pub pending_mail_observed: Option<bool>,
    pub trigger_turn_mail_observed: Option<bool>,
    pub durable_sleep_observed: Option<bool>,
    pub idle_reservation_accepted: Option<bool>,
    pub reservation_still_matches: Option<bool>,
    pub eligibility_basis: EligibilityBasis,
    pub drained_message_cohort_ids: Vec<String>,
    pub drained_cohort_complete: bool,
    pub drained_cohort_contains_trigger_turn_mail: Option<bool>,
    pub phase: SchedulerPhase,
    pub task_registration_event: Option<EventIdentity>,
    pub scheduled_turn_id: Option<String>,
    pub turn_start_publication_event: Option<EventIdentity>,
    pub outcome: SchedulerOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerPhase {
    Eligibility,
    ReservationAccepted,
    ReservationLost,
    CohortDrained,
    TaskRegistered,
    TurnStartProducerPublication,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedEvent {
    pub identity: EventIdentity,
    pub input: EventInput,
    pub identifiers_truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LossCounts {
    pub contention: u64,
    pub capacity: u64,
    pub disabled: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecorderSnapshot {
    pub capture_instance_id: String,
    /// Highest allocated sequence; gaps may be loss or an in-flight capture.
    pub high_water: u64,
    pub events: Vec<RecordedEvent>,
    pub losses: LossCounts,
}

struct RecorderState {
    events: Vec<RecordedEvent>,
    owned_bytes: usize,
}

/// A recorder belongs to one runtime/session handle; it is never process-global.
pub struct ControlPlaneRecorder {
    enabled: AtomicBool,
    capture_instance_id: String,
    next_sequence: AtomicU64,
    contention_loss: AtomicU64,
    capacity_loss: AtomicU64,
    disabled_loss: AtomicU64,
    state: Mutex<RecorderState>,
}

impl ControlPlaneRecorder {
    pub fn new(mode: CaptureMode, capture_instance_id: &str) -> Self {
        Self {
            enabled: AtomicBool::new(mode == CaptureMode::Session),
            capture_instance_id: bounded(capture_instance_id),
            next_sequence: AtomicU64::new(1),
            contention_loss: AtomicU64::new(0),
            capacity_loss: AtomicU64::new(0),
            disabled_loss: AtomicU64::new(0),
            state: Mutex::new(RecorderState {
                events: Vec::new(),
                owned_bytes: 0,
            }),
        }
    }

    /// Capture is synchronous, bounded and nonblocking. It never waits for the
    /// recorder lock; lost observations are counted separately.
    pub fn record(&self, mut input: EventInput) -> Option<EventIdentity> {
        if !self.enabled.load(Ordering::Acquire) {
            self.disabled_loss.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let identity = EventIdentity {
            capture_instance_id: self.capture_instance_id.clone(),
            sequence: self.next_sequence.fetch_add(1, Ordering::Relaxed),
        };
        let truncated = bound_input(&mut input);
        let size = dynamic_bytes(&input) + self.capture_instance_id.len();
        let Ok(mut state) = self.state.try_lock() else {
            self.contention_loss.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if !self.enabled.load(Ordering::Acquire) {
            self.disabled_loss.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        if state.events.len() >= MAX_RECORDS
            || size > MAX_OWNED_DYNAMIC_BYTES.saturating_sub(state.owned_bytes)
        {
            self.capacity_loss.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        state.owned_bytes += size;
        state.events.push(RecordedEvent {
            identity: identity.clone(),
            input,
            identifiers_truncated: truncated,
        });
        Some(identity)
    }

    pub fn capture_instance_id(&self) -> &str {
        &self.capture_instance_id
    }

    pub fn mode(&self) -> CaptureMode {
        if self.enabled.load(Ordering::Acquire) {
            CaptureMode::Session
        } else {
            CaptureMode::Off
        }
    }

    /// Explicit configuration transition: stop capture and discard retained
    /// observations. Unlike hot-path capture, this administrative method may
    /// wait for an active recorder operation to leave the lock.
    pub fn disable_and_clear(&self) {
        self.enabled.store(false, Ordering::Release);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.events.clear();
        state.owned_bytes = 0;
    }

    pub fn snapshot(&self) -> Option<RecorderSnapshot> {
        self.state.try_lock().ok().map(|state| RecorderSnapshot {
            capture_instance_id: self.capture_instance_id.clone(),
            high_water: self.next_sequence.load(Ordering::Acquire).saturating_sub(1),
            events: state.events.clone(),
            losses: self.losses(),
        })
    }

    pub fn losses(&self) -> LossCounts {
        LossCounts {
            contention: self.contention_loss.load(Ordering::Relaxed),
            capacity: self.capacity_loss.load(Ordering::Relaxed),
            disabled: self.disabled_loss.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Summary {
    pub event_count: usize,
    pub duplicate_count: usize,
    pub conflicting_identity_count: usize,
    pub by_source: BTreeMap<SourcePlane, usize>,
    pub by_kind: BTreeMap<EventKind, usize>,
    pub waits_by_outcome: BTreeMap<SelectedOutcome, usize>,
    pub conflicting_wait_count: usize,
    pub repeated_status_query_groups: usize,
    pub unknown_root_events: usize,
    pub truncated_events: usize,
}

impl Summary {
    pub fn reduce(events: &[RecordedEvent]) -> Self {
        let mut summary = Self::default();
        let mut seen = BTreeMap::<EventIdentity, (&RecordedEvent, bool)>::new();
        for event in events {
            if let Some((previous, conflicting)) = seen.get_mut(&event.identity) {
                if *previous == event {
                    summary.duplicate_count += 1;
                } else {
                    summary.conflicting_identity_count += 1;
                    *conflicting = true;
                }
                continue;
            }
            seen.insert(event.identity.clone(), (event, false));
        }
        let mut waits = BTreeMap::<(&str, &str), (&RecordedEvent, bool)>::new();
        for (event, conflicting) in seen.values() {
            if *conflicting {
                continue;
            }
            if let Some(wait) = &event.input.wait {
                if matches!(wait.phase, WaitPhase::Completed | WaitPhase::Abandoned) {
                    let key = (event.identity.capture_instance_id.as_str(), wait.wait_id.as_str());
                    if let Some((previous, was_conflicting)) = waits.get_mut(&key) {
                        if previous.input.wait.as_ref().map(|value| value.selected_outcome)
                            != Some(wait.selected_outcome)
                        {
                            *was_conflicting = true;
                        }
                    } else {
                        waits.insert(key, (event, false));
                    }
                }
            }
            summary.event_count += 1;
            *summary.by_source.entry(event.input.source_plane).or_default() += 1;
            *summary.by_kind.entry(event.input.kind).or_default() += 1;
            if event.input.root_thread_id.is_none() {
                summary.unknown_root_events += 1;
            }
            if event.identifiers_truncated {
                summary.truncated_events += 1;
            }
        }
        for (event, conflicting) in waits.values() {
            if *conflicting {
                summary.conflicting_wait_count += 1;
            } else if let Some(wait) = &event.input.wait {
                *summary
                    .waits_by_outcome
                    .entry(wait.selected_outcome)
                    .or_default() += 1;
            }
        }
        let mut queries = BTreeMap::<(SourcePlane, &str, &str), BTreeMap<&str, usize>>::new();
        for (event, conflicting) in seen.values() {
            if *conflicting {
                continue;
            }
            let (Some(operation_id), Some(query)) =
                (event.input.operation_id.as_deref(), event.input.status_query.as_ref())
            else {
                continue;
            };
            let Some(result_fingerprint) = query.result_fingerprint.as_deref() else {
                continue;
            };
            *queries
                .entry((
                    event.input.source_plane,
                    query.request_fingerprint.as_str(),
                    result_fingerprint,
                ))
                .or_default()
                .entry(operation_id)
                .or_default() += 1;
        }
        summary.repeated_status_query_groups = queries
            .values()
            .filter(|operations| operations.len() > 1)
            .count();
        summary
    }

    pub fn partitions_conserve(&self) -> bool {
        self.by_source.values().sum::<usize>() == self.event_count
            && self.by_kind.values().sum::<usize>() == self.event_count
    }
}

fn bounded(value: &str) -> String {
    let mut end = value.len().min(MAX_IDENTIFIER_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn bound_option(value: &mut Option<String>) -> bool {
    if let Some(text) = value {
        let shortened = bounded(text);
        let changed = shortened.len() != text.len();
        *text = shortened;
        changed
    } else {
        false
    }
}

fn bound_input(input: &mut EventInput) -> bool {
    let mut truncated = false;
    truncated |= shorten(&mut input.producer_boundary);
    truncated |= bound_option(&mut input.producer_version);
    truncated |= bound_option(&mut input.operation_id);
    truncated |= bound_option(&mut input.thread_id);
    truncated |= bound_option(&mut input.turn_id);
    truncated |= bound_option(&mut input.root_thread_id);
    truncated |= bound_option(&mut input.parent_thread_id);
    truncated |= bound_option(&mut input.fork_parent_thread_id);
    truncated |= bound_option(&mut input.window_id);
    truncated |= bound_option(&mut input.previous_window_id);
    if let Some(wait) = &mut input.wait {
        truncated |= shorten(&mut wait.wait_id);
        truncated |= bound_option(&mut wait.selected_target_id);
        truncated |= bound_option(&mut wait.continuation_of_wait_id);
        for target in wait.target_ids.iter_mut().take(MAX_TARGETS_PER_EVENT) {
            truncated |= shorten(target);
        }
        if wait.target_ids.len() > MAX_TARGETS_PER_EVENT {
            wait.target_ids.truncate(MAX_TARGETS_PER_EVENT);
            wait.target_set_complete = false;
            truncated = true;
        }
        if let Some(identity) = &mut wait.selected_producer {
            truncated |= shorten(&mut identity.capture_instance_id);
        }
    }
    if let Some(message) = &mut input.message {
        truncated |= bound_option(&mut message.submission_id);
        truncated |= bound_option(&mut message.sender_thread_id);
        truncated |= bound_option(&mut message.recipient_thread_id);
        truncated |= bound_option(&mut message.scheduled_turn_id);
        for id in message.selected_by_waits.iter_mut().take(MAX_TARGETS_PER_EVENT) {
            truncated |= shorten(id);
        }
        if message.selected_by_waits.len() > MAX_TARGETS_PER_EVENT {
            message.selected_by_waits.truncate(MAX_TARGETS_PER_EVENT);
            truncated = true;
        }
        for identity in [
            &mut message.accepted_event,
            &mut message.enqueued_event,
            &mut message.drained_event,
            &mut message.input_recorded_event,
            &mut message.activity_event,
        ] {
            if let Some(identity) = identity {
                truncated |= shorten(&mut identity.capture_instance_id);
            }
        }
    }
    if let Some(scheduler) = &mut input.scheduler {
        truncated |= shorten(&mut scheduler.primitive);
        truncated |= bound_option(&mut scheduler.correlation_id);
        truncated |= bound_option(&mut scheduler.scheduled_turn_id);
        for id in scheduler
            .drained_message_cohort_ids
            .iter_mut()
            .take(MAX_TARGETS_PER_EVENT)
        {
            truncated |= shorten(id);
        }
        for identity in [
            &mut scheduler.task_registration_event,
            &mut scheduler.turn_start_publication_event,
        ] {
            if let Some(identity) = identity {
                truncated |= shorten(&mut identity.capture_instance_id);
            }
        }
        if scheduler.drained_message_cohort_ids.len() > MAX_TARGETS_PER_EVENT {
            scheduler.drained_message_cohort_ids.truncate(MAX_TARGETS_PER_EVENT);
            scheduler.drained_cohort_complete = false;
            truncated = true;
        }
    }
    if let Some(query) = &mut input.status_query {
        truncated |= shorten(&mut query.request_fingerprint);
        truncated |= bound_option(&mut query.result_fingerprint);
        if let Some(readiness) = &mut query.readiness {
            truncated |= bound_option(&mut readiness.target_turn_id);
        }
    }
    if let Some(readiness) = &mut input.readiness {
        truncated |= bound_option(&mut readiness.target_turn_id);
    }
    if input.field_coverage.len() > MAX_COVERAGE_MARKS_PER_EVENT {
        input.field_coverage.truncate(MAX_COVERAGE_MARKS_PER_EVENT);
        truncated = true;
    }
    truncated
}

fn shorten(value: &mut String) -> bool {
    let shortened = bounded(value);
    let changed = shortened.len() != value.len();
    *value = shortened;
    changed
}

fn identity_bytes(identity: &EventIdentity) -> usize {
    identity.capture_instance_id.len() + std::mem::size_of_val(&identity.sequence)
}

fn dynamic_bytes(input: &EventInput) -> usize {
    let mut size = input.producer_boundary.len();
    for value in [
        &input.producer_version,
        &input.operation_id,
        &input.thread_id,
        &input.turn_id,
        &input.root_thread_id,
        &input.parent_thread_id,
        &input.fork_parent_thread_id,
        &input.window_id,
        &input.previous_window_id,
    ] {
        size += value.as_ref().map_or(0, String::len);
    }
    if let Some(wait) = &input.wait {
        size += wait.wait_id.len() + wait.selected_target_id.as_ref().map_or(0, String::len);
        size += wait.continuation_of_wait_id.as_ref().map_or(0, String::len);
        size += wait.target_ids.iter().map(String::len).sum::<usize>();
        size += wait.selected_producer.as_ref().map_or(0, identity_bytes);
    }
    if input.sleep.is_some() {
        size += 3 * std::mem::size_of::<Option<u64>>();
    }
    if let Some(message) = &input.message {
        size += message.submission_id.as_ref().map_or(0, String::len);
        size += message.sender_thread_id.as_ref().map_or(0, String::len);
        size += message.recipient_thread_id.as_ref().map_or(0, String::len);
        size += message.scheduled_turn_id.as_ref().map_or(0, String::len);
        size += message.selected_by_waits.iter().map(String::len).sum::<usize>();
        for identity in [
            &message.accepted_event,
            &message.enqueued_event,
            &message.drained_event,
            &message.input_recorded_event,
            &message.activity_event,
        ] {
            size += identity.as_ref().map_or(0, identity_bytes);
        }
    }
    if let Some(scheduler) = &input.scheduler {
        size += scheduler.primitive.len();
        size += scheduler.correlation_id.as_ref().map_or(0, String::len);
        size += scheduler.scheduled_turn_id.as_ref().map_or(0, String::len);
        size += scheduler
            .drained_message_cohort_ids
            .iter()
            .map(String::len)
            .sum::<usize>();
        for identity in [
            &scheduler.task_registration_event,
            &scheduler.turn_start_publication_event,
        ] {
            size += identity.as_ref().map_or(0, identity_bytes);
        }
    }
    if let Some(query) = &input.status_query {
        size += query.request_fingerprint.len();
        size += query.result_fingerprint.as_ref().map_or(0, String::len);
        size += query
            .readiness
            .as_ref()
            .and_then(|readiness| readiness.target_turn_id.as_ref())
            .map_or(0, String::len);
    }
    size += input
        .readiness
        .as_ref()
        .and_then(|readiness| readiness.target_turn_id.as_ref())
        .map_or(0, String::len);
    size
}
