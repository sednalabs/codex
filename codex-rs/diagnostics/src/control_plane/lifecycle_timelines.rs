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

pub(super) fn project(events: &[&RecordedEvent]) -> LifecycleProjection {
    let mut projection = LifecycleProjection::default();
    project_waits(&mut projection, events);
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

fn project_waits(projection: &mut LifecycleProjection, events: &[&RecordedEvent]) {
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
        let (event, wait) = selected.or(fallback).expect("wait group contains an observation");
        let operations = rows.iter().filter_map(|row| row.input.operation_id.as_ref()).collect::<BTreeSet<_>>();
        let starts = rows.iter().filter_map(|row| row.input.wait.as_ref().and_then(|wait| wait.blocked_start_offset_ns))
            .collect::<BTreeSet<_>>();
        let ends = rows.iter().filter_map(|row| row.input.wait.as_ref().and_then(|wait| wait.blocked_end_offset_ns))
            .collect::<BTreeSet<_>>();
        let mut conflicting = terminal.iter().any(|(_, other)| selected_facts_conflict(wait, other))
            || rows.iter().filter_map(|row| row.input.wait.as_ref()).any(|other| !same_wait_request(wait, other))
            || rows.iter().filter_map(|row| row.input.wait.as_ref())
                .filter(|other| is_selected_phase(other.phase))
                .any(|other| selected_facts_conflict(wait, other))
            || starts.len() > 1 || ends.len() > 1 || operations.len() > 1;
        let (mut start, mut end) = (None, None);
        for row in &rows {
            if let Some(obs) = &row.input.wait {
                start = start.or(obs.blocked_start_offset_ns);
                end = end.or(obs.blocked_end_offset_ns);
            }
        }
        let phase = wait.phase;
        let selected = rows.iter().rev().filter_map(|row| row.input.wait.as_ref())
            .find(|observation| is_selected_phase(observation.phase)
                && observation.selected_outcome != SelectedOutcome::Unknown)
            .or_else(|| rows.iter().rev().filter_map(|row| row.input.wait.as_ref())
                .find(|observation| is_selected_phase(observation.phase)))
            .unwrap_or(wait);
        let (subscribed_readiness, subscribed_complete, subscribed_conflict) =
            phase_readiness(&rows, false, wait);
        let (selected_readiness, selected_complete, selected_conflict) =
            phase_readiness(&rows, true, selected);
        let observed_host_return = selected.observed_host_return.clone();
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
        let outcome = if conflicting || selected.selected_outcome == SelectedOutcome::Unknown {
            SelectedOutcome::Unknown
        } else { selected.selected_outcome };
        let cohorts_complete = subscribed_complete && selected_complete
            && observed_host_return.as_ref().is_none_or(|result| result.complete);
        let complete = !conflicting && !terminal.is_empty() && rows.iter().all(|row| !row.truncated)
            && selected.selected_outcome != SelectedOutcome::Unknown
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
            selected_producer: if conflicting { None } else { selected.selected_producer.clone() },
            selected_target_id: if conflicting { None } else { selected.selected_target_id.clone() },
            selected_target_turn_id: if conflicting { None } else { selected.selected_target_turn_id.clone() },
            continuation_of_wait_id: if conflicting { None } else { selected.continuation_of_wait_id.clone() },
            subscribed_readiness: if conflicting { Vec::new() } else { subscribed_readiness },
            subscribed_readiness_complete: !conflicting && subscribed_complete,
            selected_readiness: if conflicting { Vec::new() } else { selected_readiness },
            selected_readiness_complete: !conflicting && selected_complete,
            observed_host_return: if conflicting { None } else { observed_host_return },
            coverage: if conflicting { Vec::new() } else { event.input.field_coverage.clone() },
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

fn same_wait_request(left: &WaitObservation, right: &WaitObservation) -> bool {
    let option_conflicts = |a: &Option<String>, b: &Option<String>| matches!((a, b), (Some(a), Some(b)) if a != b);
    !(left.primitive != WaitPrimitive::Unknown && right.primitive != WaitPrimitive::Unknown && left.primitive != right.primitive)
        && (left.return_when == ReturnWhen::Unknown || right.return_when == ReturnWhen::Unknown || left.return_when == right.return_when)
        && !option_conflicts(&left.helper_id, &right.helper_id)
        && !option_conflicts(&left.helper_version, &right.helper_version)
        && !matches!((left.requested_timeout_ms, right.requested_timeout_ms), (Some(a), Some(b)) if a != b)
        && !matches!((left.effective_timeout_ms, right.effective_timeout_ms), (Some(a), Some(b)) if a != b)
        && !(left.target_mode != TargetMode::Unknown && right.target_mode != TargetMode::Unknown && left.target_mode != right.target_mode)
        && !matches!((left.any_targets, right.any_targets), (Some(a), Some(b)) if a != b)
        && !(left.target_set_complete && right.target_set_complete && left.target_ids != right.target_ids)
        && !(left.requested_target_set_complete && right.requested_target_set_complete
            && (left.requested_target_ids != right.requested_target_ids
                || left.requested_target_kind != TargetReferenceKind::Unknown
                    && right.requested_target_kind != TargetReferenceKind::Unknown
                    && left.requested_target_kind != right.requested_target_kind))
        && !(left.resolved_target_set_complete == Some(true) && right.resolved_target_set_complete == Some(true)
            && (left.target_ids != right.target_ids
                || left.resolved_target_kind != TargetReferenceKind::Unknown
                    && right.resolved_target_kind != TargetReferenceKind::Unknown
                    && left.resolved_target_kind != right.resolved_target_kind))
}

fn is_selected_phase(phase: WaitPhase) -> bool {
    matches!(phase, WaitPhase::Selected | WaitPhase::Completed | WaitPhase::Abandoned)
}

fn selected_facts_conflict(left: &WaitObservation, right: &WaitObservation) -> bool {
    let conflict_option = |a: &Option<String>, b: &Option<String>| {
        matches!((a, b), (Some(a), Some(b)) if a != b)
    };
    (left.selected_outcome != SelectedOutcome::Unknown
        && right.selected_outcome != SelectedOutcome::Unknown
        && left.selected_outcome != right.selected_outcome)
        || matches!((&left.selected_producer, &right.selected_producer), (Some(a), Some(b)) if a != b)
        || conflict_option(&left.selected_target_id, &right.selected_target_id)
        || conflict_option(&left.selected_target_turn_id, &right.selected_target_turn_id)
        || (!left.selected_readiness.is_empty() && !right.selected_readiness.is_empty()
            && left.selected_readiness != right.selected_readiness)
        || matches!((&left.observed_host_return, &right.observed_host_return), (Some(a), Some(b)) if
            (a.timed_out.is_some() && b.timed_out.is_some() && a.timed_out != b.timed_out)
            || (a.reason != ObservedHostWaitReason::Unknown && b.reason != ObservedHostWaitReason::Unknown && a.reason != b.reason)
            || (a.wake_cause != ObservedHostWakeCause::Unknown && b.wake_cause != ObservedHostWakeCause::Unknown && a.wake_cause != b.wake_cause)
            || (a.queued_update_count.is_some() && b.queued_update_count.is_some()
                && a.queued_update_count != b.queued_update_count)
            || (a.target_status_complete && b.target_status_complete && a.target_statuses != b.target_statuses))
}

fn unique_wait_value(values: impl Iterator<Item = u64>) -> Option<u64> {
    let mut values = values.collect::<BTreeSet<_>>();
    (values.len() == 1).then(|| values.pop_first().expect("one wait timing value"))
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
        return if selected {
            (fallback.selected_readiness.clone(), fallback.selected_readiness_complete, false)
        } else {
            (fallback.subscribed_readiness.clone(), fallback.subscribed_readiness_complete, false)
        };
    }
    let (first, first_complete) = cohorts.remove(0);
    let conflict = cohorts.iter().any(|(rows, _)| *rows != first);
    let complete = first_complete && cohorts.iter().all(|(_, complete)| *complete) && !conflict;
    (first.clone(), complete, conflict)
}
