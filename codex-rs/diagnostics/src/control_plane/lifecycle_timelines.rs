//! Bounded lifecycle timeline projections from exact diagnostic event identities.

use std::collections::{BTreeMap, BTreeSet};

use super::types::*;
use super::usage::UsageAccountScope;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitTimeline {
    pub source_events: Vec<EventIdentity>,
    pub capture_instance_id: String,
    pub source_plane: SourcePlane,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub root_thread_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>,
    pub window_id: Option<String>,
    pub window_number: Option<u64>,
    pub previous_window_id: Option<String>,
    pub wall_correlation: Option<std::time::SystemTime>,
    pub operation_id: Option<String>,
    pub wait_id: String,
    pub phase: WaitPhase,
    pub primitive: WaitPrimitive,
    pub return_when: ReturnWhen,
    pub helper_id: Option<String>,
    pub helper_version: Option<String>,
    pub requested_timeout_ms: Option<i64>,
    pub effective_timeout_ms: Option<u64>,
    pub target_mode: TargetMode,
    pub any_targets: Option<bool>,
    pub requested_target_ids: Vec<String>,
    pub requested_target_kind: TargetReferenceKind,
    pub requested_target_set_complete: bool,
    pub resolved_target_ids: Vec<String>,
    pub resolved_target_kind: TargetReferenceKind,
    pub resolved_target_set_complete: Option<bool>,
    pub blocked_start_offset_ns: Option<u64>,
    pub blocked_end_offset_ns: Option<u64>,
    pub operation_duration_ns: Option<u64>,
    pub blocked_duration_ns: Option<u64>,
    pub selected_outcome: SelectedOutcome,
    pub selected_producer: Option<EventIdentity>,
    pub selected_target_id: Option<String>,
    pub selected_target_turn_id: Option<String>,
    pub continuation_of_wait_id: Option<String>,
    pub subscribed_readiness: Vec<ReadinessObservation>,
    pub subscribed_readiness_complete: bool,
    pub selected_readiness: Vec<ReadinessObservation>,
    pub selected_readiness_complete: bool,
    pub observed_host_return: Option<ObservedHostWaitReturn>,
    pub coverage: Vec<CoverageMark>,
    pub complete: bool,
    pub conflicting: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageTimeline {
    pub identity: EventIdentity,
    pub source_plane: SourcePlane,
    pub operation_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub root_thread_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>,
    pub window_id: Option<String>,
    pub window_number: Option<u64>,
    pub previous_window_id: Option<String>,
    pub wall_correlation: std::time::SystemTime,
    pub monotonic_offset_ns: Option<u64>,
    pub observation: MessageObservation,
    pub incomplete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundaryTimeline {
    pub identity: EventIdentity,
    pub source_plane: SourcePlane,
    pub kind: EventKind,
    pub operation_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub root_thread_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>,
    pub window_id: Option<String>,
    pub window_number: Option<u64>,
    pub previous_window_id: Option<String>,
    pub wall_correlation: std::time::SystemTime,
    pub monotonic_offset_ns: Option<u64>,
    pub incomplete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCallTimeline {
    pub identity: EventIdentity,
    pub source_plane: SourcePlane,
    pub producer_boundary: String,
    pub producer_version: Option<String>,
    pub operation_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub root_thread_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>,
    pub window_id: Option<String>,
    pub window_number: Option<u64>,
    pub previous_window_id: Option<String>,
    pub wall_correlation: std::time::SystemTime,
    pub monotonic_offset_ns: Option<u64>,
    pub quality: ObservationQuality,
    pub coverage: Vec<CoverageMark>,
    pub observation: ProviderCallObservation,
    pub complete: bool,
}

#[derive(Default)]
pub(super) struct LifecycleProjection {
    pub wait_timelines: Vec<WaitTimeline>,
    pub message_timelines: Vec<MessageTimeline>,
    pub boundary_timelines: Vec<BoundaryTimeline>,
    pub provider_call_timelines: Vec<ProviderCallTimeline>,
    pub waits_by_outcome: BTreeMap<SelectedOutcome, usize>,
    pub conflicting_wait_count: usize,
}

pub(super) fn project(
    events: &[&RecordedEvent],
    high_water: u64,
    conflicting_ids: &BTreeSet<EventIdentity>,
) -> LifecycleProjection {
    let mut projection = LifecycleProjection::default();
    project_waits(&mut projection, events, high_water, conflicting_ids);
    project_messages_and_boundaries(&mut projection, events);
    project_provider_calls(&mut projection, events);
    projection
}

fn project_provider_calls(projection: &mut LifecycleProjection, events: &[&RecordedEvent]) {
    for event in events {
        let Some(observation) = &event.input.provider_call else { continue };
        let receipt_matches = match observation.ledger_write_outcome {
            ProviderLedgerWriteOutcome::Inserted => observation.persisted_provider_call_id.is_some(),
            ProviderLedgerWriteOutcome::Duplicate | ProviderLedgerWriteOutcome::FailedUnknown
                | ProviderLedgerWriteOutcome::NotConfigured => observation.persisted_provider_call_id.is_none(),
        };
        let valid_id = |value: &str| !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES
            && !value.chars().any(char::is_control);
        let valid_scope = match &observation.ledger_response_scope {
            UsageAccountScope::KnownScope(value) => valid_id(value),
            UsageAccountScope::WriterUnscoped | UsageAccountScope::Unknown => true,
        };
        let valid_observation = valid_id(&observation.provider) && valid_id(&observation.response_id)
            && observation.persisted_provider_call_id.as_deref().is_none_or(valid_id)
            && valid_scope && !event.identity.capture_instance_id.is_empty()
            && event.identity.capture_instance_id.len() <= MAX_IDENTIFIER_BYTES && event.identity.sequence > 0;
        let complete = !event.truncated && receipt_matches
            && valid_observation
            && event.input.kind == EventKind::ProviderCompletionObserved
            && event.input.producer_boundary == PROVIDER_COMPLETION_PRODUCER_BOUNDARY
            && event.input.source_plane == SourcePlane::RustCollab
            && event.input.quality == ObservationQuality::Owned
            && event.input.thread_id.is_some() && event.input.turn_id.is_some();
        projection.provider_call_timelines.push(ProviderCallTimeline {
            identity: event.identity.clone(), source_plane: event.input.source_plane,
            producer_boundary: event.input.producer_boundary.clone(),
            producer_version: event.input.producer_version.clone(),
            operation_id: event.input.operation_id.clone(), thread_id: event.input.thread_id.clone(),
            turn_id: event.input.turn_id.clone(), root_thread_id: event.input.root_thread_id.clone(),
            parent_thread_id: event.input.parent_thread_id.clone(),
            fork_parent_thread_id: event.input.fork_parent_thread_id.clone(),
            window_id: event.input.window_id.clone(), window_number: event.input.window_number,
            previous_window_id: event.input.previous_window_id.clone(), wall_correlation: event.input.wall_correlation,
            monotonic_offset_ns: event.input.monotonic_offset_ns, quality: event.input.quality,
            coverage: event.input.field_coverage.clone(), observation: observation.clone(), complete,
        });
    }
}

fn project_waits(
    projection: &mut LifecycleProjection,
    events: &[&RecordedEvent],
    high_water: u64,
    conflicting_ids: &BTreeSet<EventIdentity>,
) {
    let mut grouped = BTreeMap::<(String, SourcePlane, Option<String>, Option<String>, Option<String>, String), Vec<&RecordedEvent>>::new();
    for event in events {
        if let Some(wait) = &event.input.wait {
            grouped.entry((event.identity.capture_instance_id.clone(), event.input.source_plane,
                event.input.thread_id.clone(), event.input.turn_id.clone(), event.input.operation_id.clone(),
                wait.wait_id.clone())).or_default().push(event);
        }
    }
    for ((capture, source, thread, turn_id, operation_id, wait_id), mut rows) in grouped {
        rows.sort_by_key(|event| event.identity.sequence);
        let terminal = rows.iter().filter_map(|event| event.input.wait.as_ref().filter(|wait|
            matches!(wait.phase, WaitPhase::Completed | WaitPhase::Abandoned)).map(|wait| (*event, wait))).collect::<Vec<_>>();
        let selected = terminal.last().copied().or_else(|| rows.iter().rev().find_map(|event| event.input.wait.as_ref()
            .filter(|wait| matches!(wait.phase, WaitPhase::Selected | WaitPhase::Completed | WaitPhase::Abandoned))
            .map(|wait| (*event, wait))));
        let fallback = rows.last().and_then(|event| event.input.wait.as_ref().map(|wait| (*event, wait)));
        let Some((event, final_wait)) = selected.or(fallback) else { continue };
        let mut wait = (*final_wait).clone();
        // Requests are invariant across phases. Reconcile every supplied known
        // fact, not only facts against a possibly sparse final observation.
        let mut conflicting = false;
        for other in rows.iter().filter_map(|row| row.input.wait.as_ref()) {
            conflicting |= reconcile_request(&mut wait, other);
        }
        let mut requested = BTreeSet::new();
        let mut resolved = BTreeSet::new();
        for other in rows.iter().filter_map(|row| row.input.wait.as_ref()) {
            for id in &other.requested_target_ids {
                if requested.len() <= MAX_TARGETS_PER_EVENT { requested.insert(id); }
            }
            for id in &other.target_ids {
                if resolved.len() <= MAX_TARGETS_PER_EVENT { resolved.insert(id); }
            }
        }
        let targets_truncated = requested.len() > MAX_TARGETS_PER_EVENT || resolved.len() > MAX_TARGETS_PER_EVENT;
        if targets_truncated {
            wait.requested_target_set_complete = false;
            wait.resolved_target_set_complete = Some(false);
        }
        let mut selected_facts = wait.clone();
        selected_facts.selected_outcome = SelectedOutcome::Unknown;
        selected_facts.selected_producer = None;
        selected_facts.selected_target_id = None;
        selected_facts.selected_target_turn_id = None;
        selected_facts.observed_host_return = None;
        for other in rows.iter().filter_map(|row| row.input.wait.as_ref())
            .filter(|other| is_selected_phase(other.phase)) {
            conflicting |= reconcile_selected(&mut selected_facts, other);
        }
        let operations = rows.iter().filter_map(|row| row.input.operation_id.as_ref()).collect::<BTreeSet<_>>();
        let starts = rows.iter().filter_map(|row| row.input.wait.as_ref().and_then(|wait| wait.blocked_start_offset_ns))
            .collect::<BTreeSet<_>>();
        let ends = rows.iter().filter_map(|row| row.input.wait.as_ref().and_then(|wait| wait.blocked_end_offset_ns))
            .collect::<BTreeSet<_>>();
        conflicting |= starts.len() > 1 || ends.len() > 1 || operations.len() > 1;
        let (mut start, mut end) = (None, None);
        for row in &rows {
            if let Some(obs) = &row.input.wait {
                start = start.or(obs.blocked_start_offset_ns);
                end = end.or(obs.blocked_end_offset_ns);
            }
        }
        let phase = final_wait.phase;
        let (subscribed_readiness, subscribed_complete, subscribed_conflict) =
            phase_readiness(&rows, /*selected*/ false, &wait);
        let (selected_readiness, selected_complete, selected_conflict) =
            phase_readiness(&rows, /*selected*/ true, &wait);
        let observed_host_return = selected_facts.observed_host_return.clone();
        let blocked_values = rows.iter().filter_map(|row| row.input.wait.as_ref())
            .filter_map(|observation| observation.blocked_duration_ns).collect::<BTreeSet<_>>();
        let operation_values = rows.iter().filter_map(|row| row.input.wait.as_ref())
            .filter_map(|observation| observation.operation_duration_ns).collect::<BTreeSet<_>>();
        let blocked_duration = unique_wait_value(blocked_values.iter().copied());
        let operation_duration = unique_wait_value(operation_values.iter().copied());
        let interval_consistent = match (start, end, blocked_duration) {
            (Some(start), Some(end), Some(duration)) => start <= end && end - start == duration,
            (None, None, Some(0)) => true,
            _ => false,
        };
        let duration_consistent = operation_duration.zip(blocked_duration)
            .is_some_and(|(operation, blocked)| operation >= blocked);
        let timing_conflict = blocked_values.len() > 1 || operation_values.len() > 1
            || matches!((start, end, blocked_duration), (Some(start), Some(end), Some(duration))
                if start > end || end - start != duration)
            || operation_duration.zip(blocked_duration).is_some_and(|(operation, blocked)| operation < blocked);
        conflicting |= subscribed_conflict || selected_conflict || timing_conflict;
        let outcome = if conflicting {
            SelectedOutcome::Unknown
        } else { selected_facts.selected_outcome };
        let (selected_producer, producer_gap) = qualify_producer(
            selected_facts.selected_producer.as_ref(), &capture, source, events,
            high_water, conflicting_ids,
        );
        let mut coverage = Vec::new();
        for mark in rows.iter().flat_map(|row| row.input.field_coverage.iter()) {
            if !coverage.contains(mark) { coverage.push(*mark); }
        }
        if let Some(unknown) = producer_gap {
            coverage.retain(|mark| mark.field != CoverageField::SelectedProducer);
            coverage.push(CoverageMark { field: CoverageField::SelectedProducer, unknown: Some(unknown) });
        } else if conflicting {
            coverage.retain(|mark| mark.field != CoverageField::SelectedProducer);
            coverage.push(CoverageMark { field: CoverageField::SelectedProducer, unknown: Some(UnknownReason::Ambiguous) });
        } else if selected_facts.selected_producer.is_none()
            && !coverage.iter().any(|mark| mark.field == CoverageField::SelectedProducer) {
            coverage.push(CoverageMark { field: CoverageField::SelectedProducer, unknown: Some(UnknownReason::NotExposed) });
        }
        if targets_truncated {
            coverage.push(CoverageMark { field: CoverageField::TargetSet, unknown: Some(UnknownReason::Truncated) });
        }
        let cohorts_complete = subscribed_complete && selected_complete
            && observed_host_return.as_ref().is_none_or(|result| result.complete);
        let complete = !conflicting && !terminal.is_empty() && rows.iter().all(|row| !row.truncated)
            && outcome != SelectedOutcome::Unknown && producer_gap.is_none()
            && wait.resolved_target_set_complete == Some(true)
            && cohorts_complete && interval_consistent && duration_consistent;
        if conflicting { projection.conflicting_wait_count += 1; }
        *projection.waits_by_outcome.entry(outcome).or_default() += 1;
        let source_events = rows.iter().map(|row| row.identity.clone()).collect::<Vec<_>>();
        projection.wait_timelines.push(WaitTimeline {
            source_events, capture_instance_id: capture, source_plane: source, thread_id: thread,
            turn_id, root_thread_id: event.input.root_thread_id.clone(),
            parent_thread_id: event.input.parent_thread_id.clone(), fork_parent_thread_id: event.input.fork_parent_thread_id.clone(),
            window_id: event.input.window_id.clone(), window_number: event.input.window_number,
            previous_window_id: event.input.previous_window_id.clone(), wall_correlation: Some(event.input.wall_correlation),
            operation_id: if conflicting { None } else { operation_id },
            wait_id, phase: if conflicting { WaitPhase::Unknown } else { phase },
            primitive: if conflicting { WaitPrimitive::Unknown } else { wait.primitive },
            return_when: if conflicting { ReturnWhen::Unknown } else { wait.return_when },
            helper_id: if conflicting { None } else { wait.helper_id.clone() },
            helper_version: if conflicting { None } else { wait.helper_version.clone() },
            requested_timeout_ms: if conflicting { None } else { wait.requested_timeout_ms },
            effective_timeout_ms: if conflicting { None } else { wait.effective_timeout_ms },
            target_mode: if conflicting { TargetMode::Unknown } else { wait.target_mode },
            any_targets: if conflicting { None } else { wait.any_targets },
            requested_target_ids: if conflicting { Vec::new() } else { wait.requested_target_ids.clone() },
            requested_target_kind: if conflicting { TargetReferenceKind::Unknown } else { wait.requested_target_kind },
            requested_target_set_complete: !conflicting && wait.requested_target_set_complete,
            resolved_target_ids: if conflicting { Vec::new() } else { wait.target_ids.clone() },
            resolved_target_kind: if conflicting { TargetReferenceKind::Unknown } else { wait.resolved_target_kind },
            resolved_target_set_complete: if conflicting { None } else { wait.resolved_target_set_complete },
            blocked_start_offset_ns: if conflicting { None } else { start },
            blocked_end_offset_ns: if conflicting { None } else { end },
            operation_duration_ns: if conflicting { None } else { operation_duration },
            blocked_duration_ns: if conflicting { None } else { blocked_duration },
            selected_outcome: outcome,
            selected_producer: if conflicting { None } else { selected_producer },
            selected_target_id: if conflicting { None } else { selected_facts.selected_target_id.clone() },
            selected_target_turn_id: if conflicting { None } else { selected_facts.selected_target_turn_id.clone() },
            continuation_of_wait_id: if conflicting { None } else { selected_facts.continuation_of_wait_id.clone() },
            subscribed_readiness: if conflicting { Vec::new() } else { subscribed_readiness },
            subscribed_readiness_complete: !conflicting && subscribed_complete,
            selected_readiness: if conflicting { Vec::new() } else { selected_readiness },
            selected_readiness_complete: !conflicting && selected_complete,
            observed_host_return: if conflicting { None } else { observed_host_return },
            coverage,
            complete, conflicting,
        });
    }
}

fn project_messages_and_boundaries(projection: &mut LifecycleProjection, events: &[&RecordedEvent]) {
    for event in events {
        if let Some(observation) = &event.input.message {
            projection.message_timelines.push(MessageTimeline {
                identity: event.identity.clone(), source_plane: event.input.source_plane,
                operation_id: event.input.operation_id.clone(), thread_id: event.input.thread_id.clone(),
                turn_id: event.input.turn_id.clone(), root_thread_id: event.input.root_thread_id.clone(),
                parent_thread_id: event.input.parent_thread_id.clone(),
                fork_parent_thread_id: event.input.fork_parent_thread_id.clone(),
                window_id: event.input.window_id.clone(), window_number: event.input.window_number,
                previous_window_id: event.input.previous_window_id.clone(),
                wall_correlation: event.input.wall_correlation,
                monotonic_offset_ns: event.input.monotonic_offset_ns,
                observation: observation.clone(), incomplete: event.truncated,
            });
        }
        if matches!(event.input.kind, EventKind::WindowBoundaryObserved | EventKind::RecoveryBoundaryObserved) {
            projection.boundary_timelines.push(BoundaryTimeline {
                identity: event.identity.clone(), source_plane: event.input.source_plane,
                kind: event.input.kind, operation_id: event.input.operation_id.clone(),
                thread_id: event.input.thread_id.clone(), turn_id: event.input.turn_id.clone(),
                root_thread_id: event.input.root_thread_id.clone(), parent_thread_id: event.input.parent_thread_id.clone(),
                fork_parent_thread_id: event.input.fork_parent_thread_id.clone(), window_id: event.input.window_id.clone(),
                window_number: event.input.window_number, previous_window_id: event.input.previous_window_id.clone(),
                wall_correlation: event.input.wall_correlation, monotonic_offset_ns: event.input.monotonic_offset_ns,
                incomplete: event.truncated,
            });
        }
    }
}

fn reconcile_optional<T: Clone + PartialEq>(current: &mut Option<T>, incoming: &Option<T>) -> bool {
    match (&*current, incoming) {
        (Some(left), Some(right)) => left != right,
        (None, Some(_)) => { *current = incoming.clone(); false }
        (Some(_), None) | (None, None) => false,
    }
}

fn reconcile_known<T: Copy + Eq>(current: &mut T, incoming: T, unknown: T) -> bool {
    if *current == unknown { *current = incoming; false }
    else { incoming != unknown && *current != incoming }
}

fn reconcile_targets(current: &mut Vec<String>, complete: &mut bool, incoming: &[String], incoming_complete: bool) -> bool {
    let left = current.iter().cloned().collect::<BTreeSet<_>>();
    let right = incoming.iter().cloned().collect::<BTreeSet<_>>();
    let conflict = *complete && !right.is_subset(&left)
        || incoming_complete && !left.is_subset(&right);
    let union = left.union(&right).take(MAX_TARGETS_PER_EVENT + 1).cloned().collect::<Vec<_>>();
    let bounded = union.len() <= MAX_TARGETS_PER_EVENT;
    let unique = left.len() == current.len() && right.len() == incoming.len();
    *current = union.into_iter().take(MAX_TARGETS_PER_EVENT).collect();
    *complete = (*complete || incoming_complete) && bounded && unique;
    conflict
}

fn reconcile_request(current: &mut WaitObservation, incoming: &WaitObservation) -> bool {
    let mut conflict = reconcile_known(&mut current.primitive, incoming.primitive, WaitPrimitive::Unknown);
    conflict |= reconcile_known(&mut current.return_when, incoming.return_when, ReturnWhen::Unknown);
    conflict |= reconcile_known(&mut current.target_mode, incoming.target_mode, TargetMode::Unknown);
    conflict |= reconcile_known(&mut current.requested_target_kind, incoming.requested_target_kind, TargetReferenceKind::Unknown);
    conflict |= reconcile_known(&mut current.resolved_target_kind, incoming.resolved_target_kind, TargetReferenceKind::Unknown);
    conflict |= reconcile_optional(&mut current.helper_id, &incoming.helper_id);
    conflict |= reconcile_optional(&mut current.helper_version, &incoming.helper_version);
    conflict |= reconcile_optional(&mut current.requested_timeout_ms, &incoming.requested_timeout_ms);
    conflict |= reconcile_optional(&mut current.effective_timeout_ms, &incoming.effective_timeout_ms);
    conflict |= reconcile_optional(&mut current.any_targets, &incoming.any_targets);
    conflict |= reconcile_optional(&mut current.continuation_of_wait_id, &incoming.continuation_of_wait_id);
    conflict |= reconcile_optional(&mut current.request_fingerprint, &incoming.request_fingerprint);
    conflict |= reconcile_targets(&mut current.requested_target_ids, &mut current.requested_target_set_complete,
        &incoming.requested_target_ids, incoming.requested_target_set_complete);
    let mut resolved_complete = current.resolved_target_set_complete == Some(true);
    conflict |= reconcile_targets(&mut current.target_ids, &mut resolved_complete,
        &incoming.target_ids, incoming.resolved_target_set_complete == Some(true));
    current.resolved_target_set_complete = if resolved_complete { Some(true) }
        else { current.resolved_target_set_complete.or(incoming.resolved_target_set_complete).map(|_| false) };
    current.target_set_complete = resolved_complete;
    conflict
}

fn is_selected_phase(phase: WaitPhase) -> bool {
    matches!(phase, WaitPhase::Selected | WaitPhase::Completed | WaitPhase::Abandoned)
}

fn reconcile_selected(current: &mut WaitObservation, incoming: &WaitObservation) -> bool {
    let mut conflict = reconcile_known(&mut current.selected_outcome, incoming.selected_outcome, SelectedOutcome::Unknown);
    conflict |= reconcile_optional(&mut current.selected_producer, &incoming.selected_producer);
    conflict |= reconcile_optional(&mut current.selected_target_id, &incoming.selected_target_id);
    conflict |= reconcile_optional(&mut current.selected_target_turn_id, &incoming.selected_target_turn_id);
    conflict |= reconcile_optional(&mut current.continuation_of_wait_id, &incoming.continuation_of_wait_id);
    if let Some(incoming) = &incoming.observed_host_return {
        if let Some(current) = &mut current.observed_host_return {
            conflict |= reconcile_optional(&mut current.timed_out, &incoming.timed_out);
            conflict |= reconcile_known(&mut current.reason, incoming.reason, ObservedHostWaitReason::Unknown);
            conflict |= reconcile_known(&mut current.wake_cause, incoming.wake_cause, ObservedHostWakeCause::Unknown);
            conflict |= reconcile_optional(&mut current.queued_update_count, &incoming.queued_update_count);
            let mut statuses = current.target_statuses.iter().map(|row|
                (row.target_reference.clone(), row.status_tag)).collect::<BTreeMap<_, _>>();
            for row in &incoming.target_statuses {
                if let Some(previous) = statuses.get(&row.target_reference) {
                    conflict |= *previous != row.status_tag;
                } else if statuses.len() < MAX_TARGETS_PER_EVENT {
                    statuses.insert(row.target_reference.clone(), row.status_tag);
                } else { current.complete = false; }
            }
            conflict |= current.target_status_complete && incoming.target_status_complete
                && current.target_statuses != incoming.target_statuses;
            current.target_statuses = statuses.into_iter().map(|(target_reference, status_tag)|
                HostTargetStatusObservation { target_reference, status_tag }).collect();
            current.target_status_complete &= incoming.target_status_complete;
            current.complete &= incoming.complete;
        } else { current.observed_host_return = Some(incoming.clone()); }
    }
    conflict
}

fn unique_wait_value(values: impl Iterator<Item = u64>) -> Option<u64> {
    let mut values = values.collect::<BTreeSet<_>>();
    if values.len() == 1 { values.pop_first() } else { None }
}

fn phase_readiness(
    rows: &[&RecordedEvent],
    selected: bool,
    fallback: &WaitObservation,
) -> (Vec<ReadinessObservation>, bool, bool) {
    let mut cohorts = rows.iter().filter_map(|event| event.input.wait.as_ref()).filter(|wait| {
        if selected {
            matches!(wait.phase, WaitPhase::Selected | WaitPhase::Completed | WaitPhase::Abandoned)
        } else {
            wait.phase == WaitPhase::Subscribed
        }
    }).map(|wait| {
        if selected {
            (&wait.selected_readiness, wait.selected_readiness_complete)
        } else {
            (&wait.subscribed_readiness, wait.subscribed_readiness_complete)
        }
    }).collect::<Vec<_>>();
    if cohorts.is_empty() {
        cohorts.push(if selected {
            (&fallback.selected_readiness, fallback.selected_readiness_complete)
        } else {
            (&fallback.subscribed_readiness, fallback.subscribed_readiness_complete)
        });
    }
    let mut complete = !cohorts.is_empty();
    let mut conflict = false;
    let mut canonical = BTreeMap::<TargetReference, ReadinessObservation>::new();
    let mut full_sets = Vec::new();
    for (cohort, cohort_complete) in cohorts {
        complete &= cohort_complete;
        let mut seen = BTreeSet::new();
        for row in cohort.iter().take(MAX_TARGETS_PER_EVENT) {
            let Some(target) = &row.target else { complete = false; continue };
            if target.kind == TargetReferenceKind::Unknown || target.id.is_empty()
                || target.id.len() > MAX_IDENTIFIER_BYTES || target.id.chars().any(char::is_control)
                || !seen.insert(target.clone()) {
                complete = false;
            }
            if row.state == Readiness::Unknown { complete = false; }
            if let Some(previous) = canonical.get_mut(target) {
                conflict |= reconcile_known(&mut previous.state, row.state, Readiness::Unknown);
                conflict |= reconcile_optional(&mut previous.target_turn_id, &row.target_turn_id);
            } else if canonical.len() < MAX_TARGETS_PER_EVENT {
                canonical.insert(target.clone(), row.clone());
            } else { complete = false; }
        }
        complete &= cohort.len() <= MAX_TARGETS_PER_EVENT;
        if cohort_complete { full_sets.push(seen); }
    }
    let keys = canonical.keys().cloned().collect::<BTreeSet<_>>();
    conflict |= full_sets.iter().any(|set| set != &keys);
    if fallback.resolved_target_set_complete == Some(true) {
        let required = fallback.target_ids.iter().map(|id| TargetReference {
            id: id.clone(), kind: fallback.resolved_target_kind,
        }).collect::<BTreeSet<_>>();
        complete &= required == keys && required.len() == fallback.target_ids.len()
            && (required.is_empty() || fallback.resolved_target_kind != TargetReferenceKind::Unknown);
    } else { complete = false; }
    (canonical.into_values().collect(), complete && !conflict, conflict)
}

fn qualify_producer(
    producer: Option<&EventIdentity>,
    capture: &str,
    source: SourcePlane,
    events: &[&RecordedEvent],
    high_water: u64,
    conflicting_ids: &BTreeSet<EventIdentity>,
) -> (Option<EventIdentity>, Option<UnknownReason>) {
    let Some(producer) = producer else { return (None, None) };
    let gap = if producer.capture_instance_id != capture {
        Some(UnknownReason::UnsupportedProducer)
    } else if conflicting_ids.contains(producer) {
        Some(UnknownReason::Ambiguous)
    } else if high_water > 0 && producer.sequence > high_water {
        Some(UnknownReason::Lost)
    } else if let Some(event) = events.iter().find(|event| event.identity == *producer) {
        if event.truncated { Some(UnknownReason::Truncated) }
        else if event.input.source_plane != source || event.input.quality != ObservationQuality::Owned {
            Some(UnknownReason::UnsupportedProducer)
        } else { None }
    } else if high_water > 0 {
        Some(UnknownReason::Lost)
    } else if events.iter().filter(|event| event.identity.capture_instance_id == capture)
        .map(|event| event.identity.sequence).min().is_some_and(|first| producer.sequence < first) {
        Some(UnknownReason::PreCapture)
    } else { Some(UnknownReason::Lost) };
    (gap.is_none().then(|| producer.clone()), gap)
}
