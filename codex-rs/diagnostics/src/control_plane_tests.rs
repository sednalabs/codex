use std::time::SystemTime;

use pretty_assertions::assert_eq;

use crate::control_plane::CaptureMode;
use crate::control_plane::ControlPlaneRecorder;
use crate::control_plane::EventInput;
use crate::control_plane::EventKind;
use crate::control_plane::ObservationQuality;
use crate::control_plane::SelectedOutcome;
use crate::control_plane::SleepObservation;
use crate::control_plane::SleepPendingActivity;
use crate::control_plane::SleepSelection;
use crate::control_plane::StatusQueryObservation;
use crate::control_plane::SourcePlane;
use crate::control_plane::Summary;
use crate::control_plane::TargetMode;
use crate::control_plane::WaitObservation;
use crate::control_plane::WaitPhase;
use crate::control_plane::WaitPrimitive;

fn input(kind: EventKind, wait: Option<WaitObservation>) -> EventInput {
    EventInput {
        source_plane: SourcePlane::RustCollab,
        producer_boundary: "test-boundary".to_owned(),
        producer_version: None,
        kind,
        quality: ObservationQuality::Owned,
        wall_correlation: SystemTime::UNIX_EPOCH,
        monotonic_offset_ns: Some(0),
        operation_id: None,
        thread_id: Some("thread-1".to_owned()),
        turn_id: None,
        root_thread_id: None,
        parent_thread_id: None,
        fork_parent_thread_id: None,
        window_id: None,
        window_number: None,
        previous_window_id: None,
        wait,
        sleep: None,
        readiness: None,
        status_query: None,
        queue: None,
        field_coverage: vec![],
        message: None,
        scheduler: None,
    }
}

fn wait(primitive: WaitPrimitive, outcome: SelectedOutcome) -> WaitObservation {
    WaitObservation {
        wait_id: match primitive {
            WaitPrimitive::V2Wait => "wait-v2",
            WaitPrimitive::ClockSleep => "sleep-1",
            WaitPrimitive::Unknown => "unknown-wait",
        }
        .to_owned(),
        phase: WaitPhase::Completed,
        primitive,
        requested_timeout_ms: Some(20),
        effective_timeout_ms: Some(30),
        any_targets: Some(true),
        target_ids: vec!["target-1".to_owned()],
        target_set_complete: true,
        target_mode: TargetMode::Targeted,
        blocked_start_offset_ns: Some(10),
        blocked_end_offset_ns: Some(10),
        operation_duration_ns: Some(5),
        blocked_duration_ns: Some(0),
        selected_outcome: outcome,
        selected_producer: None,
        selected_target_id: None,
        continuation_of_wait_id: None,
    }
}

#[test]
fn default_off_recorder_does_not_capture() {
    let recorder = ControlPlaneRecorder::new(CaptureMode::Off, "capture-a");
    assert_eq!(recorder.record(input(EventKind::WaitCompleted, None)), None);
    assert_eq!(recorder.snapshot().unwrap().events, vec![]);
    assert_eq!(recorder.losses().disabled, 1);
}

#[test]
fn recorder_bounds_dynamic_target_identifiers_and_marks_incomplete() {
    let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "capture-a");
    let mut observation = wait(WaitPrimitive::V2Wait, SelectedOutcome::Timeout);
    observation.target_ids = (0..65).map(|index| format!("target-{index}")).collect();
    let identity = recorder
        .record(input(EventKind::WaitCompleted, Some(observation)))
        .expect("first bounded event is retained");
    let events = recorder.snapshot().expect("uncontended snapshot").events;
    assert_eq!(identity.sequence, 1);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].input.wait.as_ref().unwrap().target_ids.len(), 64);
    assert!(!events[0].input.wait.as_ref().unwrap().target_set_complete);
    assert!(events[0].identifiers_truncated);
}

#[test]
fn reducer_keeps_wait_primitive_outcomes_and_unknown_roots_disjoint() {
    let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "capture-a");
    recorder.record(input(
        EventKind::WaitCompleted,
        Some(wait(WaitPrimitive::V2Wait, SelectedOutcome::Timeout)),
    ));
    recorder.record(input(
        EventKind::WaitCompleted,
        Some(wait(WaitPrimitive::ClockSleep, SelectedOutcome::SleepInterrupted)),
    ));
    let summary = Summary::reduce(&recorder.snapshot().unwrap().events);
    assert_eq!(summary.event_count, 2);
    assert_eq!(summary.waits_by_outcome.get(&SelectedOutcome::Timeout), Some(&1));
    assert_eq!(
        summary
            .waits_by_outcome
            .get(&SelectedOutcome::SleepInterrupted),
        Some(&1)
    );
    assert_eq!(summary.unknown_root_events, 2);
    assert!(summary.partitions_conserve());
}

#[test]
fn reducer_deduplicates_exact_identity_and_quarantines_conflict() {
    let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "capture-a");
    recorder.record(input(EventKind::WaitCompleted, None));
    let event = recorder.snapshot().unwrap().events.remove(0);
    let conflict = crate::control_plane::RecordedEvent {
        identity: event.identity.clone(),
        input: input(EventKind::WaitAbandoned, None),
        identifiers_truncated: false,
    };
    let summary = Summary::reduce(&[
        event.clone(),
        event.clone(),
        conflict.clone(),
        conflict,
    ]);
    assert_eq!(summary.event_count, 0);
    assert_eq!(summary.duplicate_count, 2);
    assert_eq!(summary.conflicting_identity_count, 2);
    assert!(summary.partitions_conserve());
}

#[test]
fn sleep_records_already_pending_queue_only_as_its_own_primitive_branch() {
    let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "capture-a");
    let mut event = input(EventKind::SleepSelected, None);
    event.sleep = Some(SleepObservation {
        pending_activity: SleepPendingActivity::QueueOnly,
        selection: SleepSelection::AlreadyPending,
        requested_duration_ns: Some(1_000_000),
        operation_duration_ns: Some(5_000),
        blocked_duration_ns: Some(0),
    });
    recorder.record(event);
    let recorded = recorder.snapshot().unwrap().events;
    let sleep = recorded[0].input.sleep.as_ref().unwrap();
    assert_eq!(sleep.selection, SleepSelection::AlreadyPending);
    assert_eq!(sleep.blocked_duration_ns, Some(0));
}

#[test]
fn disabling_stops_capture_and_discards_retained_events() {
    let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "capture-a");
    recorder.record(input(EventKind::OperationValidated, None));
    recorder.disable_and_clear();
    assert_eq!(recorder.mode(), CaptureMode::Off);
    assert_eq!(recorder.capture_instance_id(), "capture-a");
    assert_eq!(recorder.snapshot().unwrap().events, vec![]);
    assert_eq!(recorder.record(input(EventKind::OperationValidated, None)), None);
}

#[test]
fn repeated_queries_are_observed_only_when_returned_metadata_matches() {
    let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "capture-a");
    for operation_id in ["op-1", "op-2"] {
        let mut event = input(EventKind::StatusQueryObserved, None);
        event.operation_id = Some(operation_id.to_owned());
        event.status_query = Some(StatusQueryObservation {
            request_fingerprint: "targets-and-mode".to_owned(),
            result_fingerprint: Some("readiness-state".to_owned()),
            readiness: None,
        });
        recorder.record(event);
    }
    let summary = Summary::reduce(&recorder.snapshot().unwrap().events);
    assert_eq!(summary.repeated_status_query_groups, 1);
}
