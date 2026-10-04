//! Bounded lifecycle timeline projections from exact diagnostic event identities.

use std::collections::{BTreeMap, BTreeSet};

use super::types::*;

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
    pub selected_readiness: Vec<ReadinessObservation>,
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

#[derive(Default)]
pub(super) struct LifecycleProjection {
    pub wait_timelines: Vec<WaitTimeline>,
    pub message_timelines: Vec<MessageTimeline>,
    pub boundary_timelines: Vec<BoundaryTimeline>,
    pub waits_by_outcome: BTreeMap<SelectedOutcome, usize>,
    pub conflicting_wait_count: usize,
}

pub(super) fn project(events: &[&RecordedEvent]) -> LifecycleProjection {
    let mut projection = LifecycleProjection::default();
    project_waits(&mut projection, events);
    project_messages_and_boundaries(&mut projection, events);
    projection
}

fn project_waits(projection: &mut LifecycleProjection, events: &[&RecordedEvent]) {
    let mut grouped = BTreeMap::<(String, SourcePlane, Option<String>, String, Option<u64>), Vec<&RecordedEvent>>::new();
    for event in events {
        if let Some(wait) = &event.input.wait {
            grouped.entry((event.identity.capture_instance_id.clone(), event.input.source_plane,
                event.input.thread_id.clone(), wait.wait_id.clone(), event.input.thread_id.is_none().then_some(event.identity.sequence))).or_default().push(event);
        }
    }
    for ((capture, source, thread, wait_id, _), mut rows) in grouped {
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
        let conflicting = terminal.iter().any(|(other_event, other)| *other != wait
            || other_event.input.operation_id != event.input.operation_id)
            || rows.iter().filter_map(|row| row.input.wait.as_ref()).any(|other| !same_wait_request(wait, other))
            || starts.len() > 1 || ends.len() > 1 || operations.len() > 1;
        let (mut start, mut end) = if terminal.is_empty() { (None, None) }
            else { (wait.blocked_start_offset_ns, wait.blocked_end_offset_ns) };
        for row in &rows {
            if !terminal.is_empty() && let Some(obs) = &row.input.wait {
                start = start.or(obs.blocked_start_offset_ns);
                end = end.or(obs.blocked_end_offset_ns);
            }
        }
        let phase = wait.phase;
        let outcome = if conflicting || terminal.is_empty() && phase != WaitPhase::Selected {
            SelectedOutcome::Unknown
        } else { wait.selected_outcome };
        let zero_block = !terminal.is_empty() && wait.blocked_duration_ns == Some(0);
        let complete = !conflicting && !terminal.is_empty() && rows.iter().all(|row| !row.truncated)
            && (zero_block || start.is_some() && end.is_some()) && wait.blocked_duration_ns.is_some();
        if conflicting { projection.conflicting_wait_count += 1; }
        *projection.waits_by_outcome.entry(outcome).or_default() += 1;
        let source_events = rows.iter().map(|row| row.identity.clone()).collect::<Vec<_>>();
        projection.wait_timelines.push(WaitTimeline {
            source_events, capture_instance_id: capture, source_plane: source, thread_id: thread,
            turn_id: event.input.turn_id.clone(), root_thread_id: event.input.root_thread_id.clone(),
            parent_thread_id: event.input.parent_thread_id.clone(), fork_parent_thread_id: event.input.fork_parent_thread_id.clone(),
            window_id: event.input.window_id.clone(), window_number: event.input.window_number,
            previous_window_id: event.input.previous_window_id.clone(), wall_correlation: Some(event.input.wall_correlation),
            operation_id: if conflicting { None } else { event.input.operation_id.clone() },
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
            operation_duration_ns: if conflicting || terminal.is_empty() { None } else { wait.operation_duration_ns },
            blocked_duration_ns: if conflicting || terminal.is_empty() { None } else { wait.blocked_duration_ns },
            selected_outcome: outcome,
            selected_producer: if conflicting || terminal.is_empty() { None } else { wait.selected_producer.clone() },
            selected_target_id: if conflicting { None } else { wait.selected_target_id.clone() },
            selected_target_turn_id: if conflicting { None } else { wait.selected_target_turn_id.clone() },
            continuation_of_wait_id: if conflicting { None } else { wait.continuation_of_wait_id.clone() },
            subscribed_readiness: if conflicting { Vec::new() } else { wait.subscribed_readiness.clone() },
            selected_readiness: if conflicting { Vec::new() } else { wait.selected_readiness.clone() },
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
