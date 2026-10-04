use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::types::*;
use super::usage::UsageAccountScope;

const MAX_STRUCTURAL_BYTES: usize = 4_194_304;

#[derive(Default)]
struct State { events: Vec<RecordedEvent>, payload_bytes: usize, structural_bytes: usize }

/// Session/runtime-owned capture. Hot-path writes never wait for the lock.
pub struct ControlPlaneRecorder {
    enabled: AtomicBool,
    capture_instance_id: String,
    next_sequence: AtomicU64,
    contention_loss: AtomicU64,
    capacity_loss: AtomicU64,
    disabled_loss: AtomicU64,
    invalid_identity_loss: AtomicU64,
    state: Mutex<State>,
}
impl ControlPlaneRecorder {
    pub fn new(mode: CaptureMode, capture_instance_id: &str) -> Result<Self, InvalidCaptureId> {
        if !valid_id(capture_instance_id) { return Err(InvalidCaptureId); }
        Ok(Self {
            enabled: AtomicBool::new(mode == CaptureMode::Session),
            capture_instance_id: compact(capture_instance_id.to_owned()),
            next_sequence: AtomicU64::new(1),
            contention_loss: AtomicU64::new(0), capacity_loss: AtomicU64::new(0),
            disabled_loss: AtomicU64::new(0), invalid_identity_loss: AtomicU64::new(0),
            state: Mutex::new(State::default()),
        })
    }
    pub fn record(&self, mut input: EventInput) -> Option<EventIdentity> {
        if !self.enabled.load(Ordering::Acquire) {
            self.disabled_loss.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let version_incomplete = bound_producer_version(&mut input);
        let projection_incomplete = sanitize_status_projection(&mut input);
        if !valid_input_ids(&input) {
            self.invalid_identity_loss.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let provider_incomplete = sanitize_provider_call(&mut input);
        let truncated = bound_and_compact(&mut input) || projection_incomplete
            || version_incomplete || provider_incomplete;
        let payload = payload_capacity(&input) + self.capture_instance_id.capacity();
        let structural = structural_size(&input) + std::mem::size_of::<RecordedEvent>();
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let Ok(mut state) = self.state.try_lock() else {
            self.contention_loss.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if !self.enabled.load(Ordering::Acquire) {
            self.disabled_loss.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        if state.events.len() >= MAX_RECORDS
            || payload > MAX_OWNED_DYNAMIC_BYTES.saturating_sub(state.payload_bytes)
            || structural > MAX_STRUCTURAL_BYTES.saturating_sub(state.structural_bytes)
        {
            self.capacity_loss.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let identity = EventIdentity {
            capture_instance_id: self.capture_instance_id.clone(),
            sequence,
        };
        state.payload_bytes += payload;
        state.structural_bytes += structural;
        state.events.push(RecordedEvent { identity: identity.clone(), input, truncated });
        Some(identity)
    }
    pub fn capture_instance_id(&self) -> &str { &self.capture_instance_id }
    pub fn mode(&self) -> CaptureMode {
        if self.enabled.load(Ordering::Acquire) { CaptureMode::Session } else { CaptureMode::Off }
    }
    /// Explicit config transition; unlike record, this operation may wait.
    pub fn disable_and_clear(&self) {
        self.enabled.store(false, Ordering::Release);
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = State::default();
    }
    pub fn snapshot(&self) -> Option<RecorderSnapshot> {
        self.state.try_lock().ok().map(|state| RecorderSnapshot {
            capture_instance_id: self.capture_instance_id.clone(),
            high_water: self.next_sequence.load(Ordering::Acquire).saturating_sub(1),
            events: state.events.clone(), losses: self.losses(),
        })
    }
    pub fn losses(&self) -> LossCounts {
        LossCounts {
            contention: self.contention_loss.load(Ordering::Relaxed),
            capacity: self.capacity_loss.load(Ordering::Relaxed),
            disabled: self.disabled_loss.load(Ordering::Relaxed),
            invalid_identity: self.invalid_identity_loss.load(Ordering::Relaxed),
        }
    }
}

fn valid_id(value: &str) -> bool { !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES }
fn check(value: &Option<String>) -> bool { value.as_deref().is_none_or(valid_id) }
fn valid_event_identity(value: &Option<EventIdentity>) -> bool {
    value.as_ref().is_none_or(|id| valid_id(&id.capture_instance_id) && id.sequence > 0)
}
fn valid_input_ids(input: &EventInput) -> bool {
    [
        &input.operation_id, &input.thread_id, &input.turn_id, &input.root_thread_id,
        &input.parent_thread_id, &input.fork_parent_thread_id, &input.window_id,
        &input.previous_window_id,
    ].into_iter().all(check)
        && input.wait.as_ref().is_none_or(|wait| {
            valid_id(&wait.wait_id) && check(&wait.helper_id) && check(&wait.helper_version)
                && check(&wait.selected_target_id) && check(&wait.selected_target_turn_id)
                && check(&wait.continuation_of_wait_id) && check(&wait.request_fingerprint)
                && check(&wait.result_fingerprint)
                && wait.target_ids.iter().chain(&wait.requested_target_ids).take(2 * MAX_TARGETS_PER_EVENT).all(|id| valid_id(id))
                && wait.subscribed_readiness.iter().chain(&wait.selected_readiness).take(2 * MAX_TARGETS_PER_EVENT)
                    .all(|r| check(&r.target_turn_id))
                && wait.subscribed_readiness.iter().chain(&wait.selected_readiness)
                    .all(|row| check(&row.target_turn_id))
                && valid_event_identity(&wait.selected_producer)
        })
        && input.message.as_ref().is_none_or(|message| {
            [
                &message.submission_id, &message.sender_thread_id, &message.recipient_thread_id,
                &message.scheduled_turn_id,
            ].into_iter().all(check)
                && message.selected_by_waits.iter().take(MAX_TARGETS_PER_EVENT).all(|id| valid_id(id))
                && message.superseded_submission_ids.iter().take(MAX_TARGETS_PER_EVENT).all(|id| valid_id(id))
                && [
                    &message.accepted_event, &message.enqueued_event, &message.drained_event,
                    &message.input_recorded_event, &message.activity_event,
                ].into_iter().all(valid_event_identity)
        })
        && input.scheduler.as_ref().is_none_or(|scheduler| {
            check(&scheduler.correlation_id) && check(&scheduler.scheduled_turn_id)
                && scheduler.drained_message_cohort_ids.iter().take(MAX_TARGETS_PER_EVENT).all(|id| valid_id(id))
                && [
                    &scheduler.task_registration_event, &scheduler.turn_start_publication_event,
                ].into_iter().all(valid_event_identity)
        })
        && input.status_query.as_ref().is_none_or(|query| {
            check(&query.request_fingerprint) && check(&query.result_fingerprint)
                && query.readiness.as_ref().is_none_or(|r| check(&r.target_turn_id))
                && query.request_projection.as_ref().is_none_or(|request| {
                    check(&request.path_prefix) && request.requested_agent_ids.iter().all(|id| valid_id(id))
                })
                && query.result_projection.as_ref().is_none_or(|result| result.actors.iter().all(|actor| {
                    valid_id(&actor.agent_id) && valid_id(&actor.canonical_path)
                        && check(&actor.configured_model) && check(&actor.configured_reasoning_effort)
                        && check(&actor.raw_status_tag)
                }))
        })
        && input.readiness.as_ref().is_none_or(|r| check(&r.target_turn_id))
        && input.external_operation.as_ref().is_none_or(|op| valid_id(&op.call_id))
        && input.provider_call.as_ref().is_none_or(|provider| {
            let provider_id = |value: &str| valid_id(value) && !value.chars().any(char::is_control);
            provider_id(&provider.provider) && provider_id(&provider.response_id)
                && provider.persisted_provider_call_id.as_deref().is_none_or(provider_id)
                && match &provider.ledger_response_scope {
                    UsageAccountScope::KnownScope(scope) => provider_id(scope),
                    UsageAccountScope::WriterUnscoped | UsageAccountScope::Unknown => true,
                }
        })
}
fn compact(value: String) -> String { value.into_boxed_str().into_string() }
fn compact_limited(value: String, limit: usize) -> (String, bool) {
    let mut value = value;
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) { end -= 1; }
    let truncated = end != value.len();
    value.truncate(end);
    (compact(value), truncated)
}
fn compact_vec<T>(values: &mut Vec<T>, limit: usize) -> bool {
    let truncated = values.len() > limit;
    values.truncate(limit);
    *values = std::mem::take(values).into_boxed_slice().into_vec();
    truncated
}
fn compact_option(value: &mut Option<String>) {
    if let Some(text) = value { *text = compact(std::mem::take(text)); }
}
fn compact_readiness(value: &mut ReadinessObservation) { compact_option(&mut value.target_turn_id); }
fn bound_producer_version(input: &mut EventInput) -> bool {
    if input.producer_version.as_ref().is_some_and(|value| value.len() > MAX_IDENTIFIER_BYTES) {
        input.producer_version = None;
        if input.field_coverage.len() < MAX_COVERAGE_MARKS_PER_EVENT {
            input.field_coverage.push(CoverageMark {
                field: CoverageField::ProducerVersion,
                unknown: Some(UnknownReason::Truncated),
            });
        }
        true
    } else { false }
}
fn sanitize_provider_call(input: &mut EventInput) -> bool {
    let Some(provider) = &mut input.provider_call else { return false };
    let consistent = match provider.ledger_write_outcome {
        ProviderLedgerWriteOutcome::Inserted => provider.persisted_provider_call_id.is_some(),
        ProviderLedgerWriteOutcome::Duplicate | ProviderLedgerWriteOutcome::FailedUnknown
            | ProviderLedgerWriteOutcome::NotConfigured => provider.persisted_provider_call_id.is_none(),
    };
    if consistent { return false; }
    provider.persisted_provider_call_id = None;
    if input.field_coverage.len() < MAX_COVERAGE_MARKS_PER_EVENT {
        input.field_coverage.push(CoverageMark {
            field: CoverageField::ProviderObservedIdentity,
            unknown: Some(UnknownReason::Ambiguous),
        });
    }
    true
}
fn sanitize_status_projection(input: &mut EventInput) -> bool {
    let mut incomplete = false;
    let Some(query) = &mut input.status_query else { return false };
    for fingerprint in [&mut query.request_fingerprint, &mut query.result_fingerprint] {
        if fingerprint.as_ref().is_some_and(|value| !valid_id(value)) {
            *fingerprint = None;
            incomplete = true;
        }
    }
    if let Some(request) = &mut query.request_projection {
        let dropped = compact_vec(&mut request.requested_agent_ids, MAX_TARGETS_PER_EVENT);
        request.complete &= !dropped;
        incomplete |= dropped;
        if request.path_prefix.as_ref().is_some_and(|value| !valid_id(value)) {
            request.path_prefix = None;
            request.complete = false;
            incomplete = true;
        }
        request.requested_agent_ids.sort_unstable();
    }
    if let Some(result) = &mut query.result_projection {
        let dropped = compact_vec(&mut result.actors, MAX_TARGETS_PER_EVENT);
        result.complete &= !dropped;
        incomplete |= dropped;
        let invalid = result.actors.iter().any(|actor| {
            !valid_id(&actor.agent_id) || !valid_id(&actor.canonical_path)
                || !check(&actor.configured_model) || !check(&actor.configured_reasoning_effort)
                || !check(&actor.raw_status_tag)
        });
        if invalid {
            compact_vec(&mut result.actors, 0);
            result.complete = false;
            incomplete = true;
        }
        result.actors.sort_unstable();
        if result.actors.windows(2).any(|rows| rows[0].agent_id == rows[1].agent_id) {
            result.complete = false;
            incomplete = true;
        }
        if result.complete && result.observed_actor_count != Some(result.actors.len() as u64) {
            result.complete = false;
            incomplete = true;
        }
    }
    incomplete
}
fn bound_and_compact(input: &mut EventInput) -> bool {
    let mut truncated = false;
    let (boundary, clipped) = compact_limited(std::mem::take(&mut input.producer_boundary), MAX_IDENTIFIER_BYTES);
    input.producer_boundary = boundary;
    truncated |= clipped;
    compact_option(&mut input.producer_version);
    for value in [
        &mut input.operation_id, &mut input.thread_id, &mut input.turn_id,
        &mut input.root_thread_id, &mut input.parent_thread_id,
        &mut input.fork_parent_thread_id, &mut input.window_id,
        &mut input.previous_window_id,
    ] { compact_option(value); }
    if let Some(wait) = &mut input.wait {
        compact_option(&mut wait.helper_id); compact_option(&mut wait.helper_version);
        compact_option(&mut wait.selected_target_id); compact_option(&mut wait.selected_target_turn_id);
        compact_option(&mut wait.continuation_of_wait_id);
        let resolved_truncated = compact_vec(&mut wait.target_ids, MAX_TARGETS_PER_EVENT);
        let requested_truncated = compact_vec(&mut wait.requested_target_ids, MAX_TARGETS_PER_EVENT);
        wait.target_set_complete &= !resolved_truncated;
        wait.resolved_target_set_complete = wait.resolved_target_set_complete.map(|complete| complete && !resolved_truncated);
        wait.requested_target_set_complete &= !requested_truncated;
        truncated |= resolved_truncated || requested_truncated;
        truncated |= compact_vec(&mut wait.subscribed_readiness, MAX_TARGETS_PER_EVENT);
        truncated |= compact_vec(&mut wait.selected_readiness, MAX_TARGETS_PER_EVENT);
        compact_option(&mut wait.request_fingerprint); compact_option(&mut wait.result_fingerprint);
        for r in wait.subscribed_readiness.iter_mut().chain(&mut wait.selected_readiness) {
            compact_readiness(r);
        }
        for id in wait.target_ids.iter_mut().chain(&mut wait.requested_target_ids) { *id = compact(std::mem::take(id)); }
        for id in [&mut wait.wait_id] { *id = compact(std::mem::take(id)); }
        compact_event_identity(&mut wait.selected_producer);
    }
    if let Some(message) = &mut input.message {
        compact_option(&mut message.submission_id); compact_option(&mut message.sender_thread_id);
        compact_option(&mut message.recipient_thread_id); compact_option(&mut message.scheduled_turn_id);
        truncated |= compact_vec(&mut message.selected_by_waits, MAX_TARGETS_PER_EVENT);
        let superseded_truncated = compact_vec(&mut message.superseded_submission_ids, MAX_TARGETS_PER_EVENT);
        message.superseded_selection_complete &= !superseded_truncated;
        truncated |= superseded_truncated;
        for id in message.selected_by_waits.iter_mut().chain(&mut message.superseded_submission_ids) { *id = compact(std::mem::take(id)); }
        for id in [&mut message.accepted_event, &mut message.enqueued_event, &mut message.drained_event,
            &mut message.input_recorded_event, &mut message.activity_event] { compact_event_identity(id); }
    }
    if let Some(scheduler) = &mut input.scheduler {
        let (primitive, clipped) = compact_limited(std::mem::take(&mut scheduler.primitive), MAX_IDENTIFIER_BYTES);
        scheduler.primitive = primitive; truncated |= clipped;
        let cohort_truncated = compact_vec(&mut scheduler.drained_message_cohort_ids, MAX_TARGETS_PER_EVENT);
        scheduler.drained_cohort_complete &= !cohort_truncated;
        truncated |= cohort_truncated;
        compact_option(&mut scheduler.correlation_id); compact_option(&mut scheduler.scheduled_turn_id);
        for id in scheduler.drained_message_cohort_ids.iter_mut() { *id = compact(std::mem::take(id)); }
        compact_event_identity(&mut scheduler.task_registration_event);
        compact_event_identity(&mut scheduler.turn_start_publication_event);
    }
    if let Some(query) = &mut input.status_query {
        compact_option(&mut query.request_fingerprint);
        compact_option(&mut query.result_fingerprint);
        if let Some(readiness) = &mut query.readiness { compact_readiness(readiness); }
        if let Some(request) = &mut query.request_projection {
            compact_option(&mut request.path_prefix);
            for id in request.requested_agent_ids.iter_mut() { *id = compact(std::mem::take(id)); }
        }
        if let Some(result) = &mut query.result_projection {
            for actor in result.actors.iter_mut() {
                actor.agent_id = compact(std::mem::take(&mut actor.agent_id));
                actor.canonical_path = compact(std::mem::take(&mut actor.canonical_path));
                compact_option(&mut actor.configured_model);
                compact_option(&mut actor.configured_reasoning_effort);
                compact_option(&mut actor.raw_status_tag);
            }
            result.actors = std::mem::take(&mut result.actors).into_boxed_slice().into_vec();
        }
    }
    if let Some(readiness) = &mut input.readiness { compact_readiness(readiness); }
    if let Some(operation) = &mut input.external_operation {
        operation.call_id = compact(std::mem::take(&mut operation.call_id));
    }
    if let Some(provider) = &mut input.provider_call {
        provider.provider = compact(std::mem::take(&mut provider.provider));
        provider.response_id = compact(std::mem::take(&mut provider.response_id));
        compact_option(&mut provider.persisted_provider_call_id);
        if let UsageAccountScope::KnownScope(scope) = &mut provider.ledger_response_scope {
            *scope = compact(std::mem::take(scope));
        }
    }
    truncated |= compact_vec(&mut input.field_coverage, MAX_COVERAGE_MARKS_PER_EVENT);
    truncated
}
fn compact_event_identity(value: &mut Option<EventIdentity>) {
    if let Some(identity) = value { identity.capture_instance_id = compact(std::mem::take(&mut identity.capture_instance_id)); }
}
fn payload_capacity(input: &EventInput) -> usize {
    let mut bytes = input.producer_boundary.capacity()
        + input.producer_version.as_ref().map_or(0, String::capacity);
    for value in [
        &input.operation_id, &input.thread_id, &input.turn_id, &input.root_thread_id,
        &input.parent_thread_id, &input.fork_parent_thread_id, &input.window_id,
        &input.previous_window_id,
    ] { bytes += value.as_ref().map_or(0, String::capacity); }
    if let Some(wait) = &input.wait {
        bytes += wait.wait_id.capacity() + wait.helper_id.as_ref().map_or(0, String::capacity)
            + wait.helper_version.as_ref().map_or(0, String::capacity)
            + wait.selected_target_id.as_ref().map_or(0, String::capacity)
            + wait.selected_target_turn_id.as_ref().map_or(0, String::capacity)
            + wait.continuation_of_wait_id.as_ref().map_or(0, String::capacity);
        bytes += wait.target_ids.iter().map(String::capacity).sum::<usize>();
        bytes += wait.requested_target_ids.iter().map(String::capacity).sum::<usize>();
        for row in wait.subscribed_readiness.iter().chain(&wait.selected_readiness) {
            bytes += row.target_turn_id.as_ref().map_or(0, String::capacity);
        }
        bytes += wait.request_fingerprint.as_ref().map_or(0, String::capacity)
            + wait.result_fingerprint.as_ref().map_or(0, String::capacity);
        bytes += identity_capacity(&wait.selected_producer);
    }
    if let Some(message) = &input.message {
        for value in [&message.submission_id, &message.sender_thread_id, &message.recipient_thread_id,
            &message.scheduled_turn_id] { bytes += value.as_ref().map_or(0, String::capacity); }
        bytes += message.selected_by_waits.iter().map(String::capacity).sum::<usize>();
        bytes += message.superseded_submission_ids.iter().map(String::capacity).sum::<usize>();
        for id in [&message.accepted_event, &message.enqueued_event, &message.drained_event,
            &message.input_recorded_event, &message.activity_event] { bytes += identity_capacity(id); }
    }
    if let Some(s) = &input.scheduler {
        bytes += s.primitive.capacity() + s.correlation_id.as_ref().map_or(0, String::capacity)
            + s.scheduled_turn_id.as_ref().map_or(0, String::capacity)
            + s.drained_message_cohort_ids.iter().map(String::capacity).sum::<usize>();
        bytes += identity_capacity(&s.task_registration_event) + identity_capacity(&s.turn_start_publication_event);
    }
    if let Some(q) = &input.status_query {
        bytes += q.request_fingerprint.as_ref().map_or(0, String::capacity)
            + q.result_fingerprint.as_ref().map_or(0, String::capacity);
        bytes += q.readiness.as_ref().and_then(|r| r.target_turn_id.as_ref()).map_or(0, String::capacity);
        bytes += q.request_projection.as_ref().map_or(0, |_| std::mem::size_of::<StatusRequestProjection>())
            + q.result_projection.as_ref().map_or(0, |_| std::mem::size_of::<StatusResultProjection>());
        if let Some(request) = &q.request_projection {
            bytes += request.path_prefix.as_ref().map_or(0, String::capacity)
                + request.requested_agent_ids.iter().map(String::capacity).sum::<usize>();
        }
        if let Some(result) = &q.result_projection {
            for actor in &result.actors {
                bytes += actor.agent_id.capacity() + actor.canonical_path.capacity()
                    + actor.configured_model.as_ref().map_or(0, String::capacity)
                    + actor.configured_reasoning_effort.as_ref().map_or(0, String::capacity)
                    + actor.raw_status_tag.as_ref().map_or(0, String::capacity);
            }
        }
    }
    bytes += input.readiness.as_ref().and_then(|r| r.target_turn_id.as_ref()).map_or(0, String::capacity);
    bytes += input.external_operation.as_ref().map_or(0, |op| op.call_id.capacity());
    bytes += input.provider_call.as_ref().map_or(0, |provider| {
        provider.provider.capacity() + provider.response_id.capacity()
            + provider.persisted_provider_call_id.as_ref().map_or(0, String::capacity)
            + match &provider.ledger_response_scope {
                UsageAccountScope::KnownScope(scope) => scope.capacity(),
                UsageAccountScope::WriterUnscoped | UsageAccountScope::Unknown => 0,
            }
    });
    bytes
}
fn identity_capacity(value: &Option<EventIdentity>) -> usize {
    value.as_ref().map_or(0, |id| id.capture_instance_id.capacity())
}
fn structural_size(input: &EventInput) -> usize {
    std::mem::size_of::<EventInput>()
        + input.field_coverage.capacity() * std::mem::size_of::<CoverageMark>()
        + input.wait.as_ref().map_or(0, |w| {
            w.target_ids.capacity() * std::mem::size_of::<String>()
                + w.requested_target_ids.capacity() * std::mem::size_of::<String>()
                + w.subscribed_readiness.capacity() * std::mem::size_of::<ReadinessObservation>()
                + w.selected_readiness.capacity() * std::mem::size_of::<ReadinessObservation>()
                + std::mem::size_of::<WaitObservation>()
        })
        + input.message.as_ref().map_or(0, |m| {
            (m.selected_by_waits.capacity() + m.superseded_submission_ids.capacity()) * std::mem::size_of::<String>()
                + std::mem::size_of::<MessageObservation>()
        })
        + input.scheduler.as_ref().map_or(0, |s| s.drained_message_cohort_ids.capacity() * std::mem::size_of::<String>() + std::mem::size_of::<SchedulerObservation>())
        + input.status_query.as_ref().map_or(0, |q| {
            std::mem::size_of::<StatusQueryObservation>()
                + q.request_projection.as_ref().map_or(0, |r| r.requested_agent_ids.capacity() * std::mem::size_of::<String>())
                + q.result_projection.as_ref().map_or(0, |r| r.actors.capacity() * std::mem::size_of::<StatusActorProjection>())
        })
        + input.readiness.as_ref().map_or(0, |_| std::mem::size_of::<ReadinessObservation>())
        + input.provider_call.as_ref().map_or(0, |_| std::mem::size_of::<ProviderCallObservation>())
}
