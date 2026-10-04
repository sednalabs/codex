use std::time::SystemTime;

use pretty_assertions::assert_eq;

use crate::control_plane::*;

fn event(kind: EventKind) -> EventInput {
    EventInput {
        source_plane: SourcePlane::RustCollab,
        producer_boundary: "unit-test".into(),
        producer_version: None,
        kind,
        quality: ObservationQuality::Owned,
        wall_correlation: SystemTime::UNIX_EPOCH,
        monotonic_offset_ns: Some(0),
        operation_id: None,
        thread_id: Some("thread-1".into()),
        turn_id: Some("turn-1".into()),
        root_thread_id: None,
        parent_thread_id: None,
        fork_parent_thread_id: None,
        window_id: None,
        window_number: None,
        previous_window_id: None,
        wait: None,
        sleep: None,
        readiness: None,
        status_query: None,
        queue: None,
        field_coverage: vec![],
        message: None,
        scheduler: None,
        external_operation: None,
    }
}
fn wait(id: &str, outcome: SelectedOutcome) -> WaitObservation {
    WaitObservation {
        wait_id: id.into(),
        phase: WaitPhase::Completed,
        primitive: WaitPrimitive::V2Wait,
        helper_id: Some("wait_agent".into()),
        helper_version: None,
        requested_timeout_ms: Some(-1),
        effective_timeout_ms: Some(0),
        target_mode: TargetMode::Targeted,
        any_targets: Some(true),
        target_ids: vec!["resolved-thread".into()],
        target_set_complete: true,
        requested_target_ids: vec!["agent/path".into()],
        requested_target_kind: TargetReferenceKind::ExposedAgentPath,
        requested_target_set_complete: true,
        resolved_target_set_complete: Some(true),
        subscribed_readiness: vec![],
        selected_readiness: vec![],
        blocked_start_offset_ns: Some(10),
        blocked_end_offset_ns: Some(20),
        operation_duration_ns: Some(12),
        blocked_duration_ns: Some(10),
        selected_outcome: outcome,
        selected_producer: None,
        selected_target_id: None,
        selected_target_turn_id: None,
        continuation_of_wait_id: None,
        request_fingerprint: None,
        result_fingerprint: None,
    }
}
fn recorder(id: &str) -> ControlPlaneRecorder {
    ControlPlaneRecorder::new(CaptureMode::Session, id).unwrap()
}

#[test]
fn default_off_and_invalid_identity_never_create_truncated_joins() {
    assert!(ControlPlaneRecorder::new(CaptureMode::Session, &"x".repeat(257)).is_err());
    let off = ControlPlaneRecorder::new(CaptureMode::Off, "capture-a").unwrap();
    assert_eq!(off.record(event(EventKind::WaitCompleted)), None);
    assert_eq!(off.losses().disabled, 1);
    let active = recorder("capture-a");
    let mut invalid = event(EventKind::WaitCompleted);
    invalid.operation_id = Some("q".repeat(257));
    assert_eq!(active.record(invalid), None);
    assert_eq!(active.losses().invalid_identity, 1);
}

#[test]
fn retained_collections_are_canonicalized_and_target_planes_stay_distinct() {
    let active = recorder("capture-a");
    let mut observation = wait("wait-1", SelectedOutcome::Timeout);
    observation.target_ids = (0..80).map(|n| format!("resolved-{n}")).collect();
    observation.requested_target_ids = (0..80).map(|n| format!("agent/path/{n}")).collect();
    let mut input = event(EventKind::WaitCompleted);
    input.wait = Some(observation);
    active.record(input).unwrap();
    let snapshot = active.snapshot().unwrap();
    let stored = snapshot.events[0].input.wait.as_ref().unwrap();
    assert_eq!(stored.target_ids.len(), 64);
    assert_eq!(stored.target_ids.capacity(), 64);
    assert_eq!(stored.requested_target_ids.len(), 64);
    assert_eq!(stored.requested_target_ids.capacity(), 64);
    assert_eq!(stored.requested_target_kind, TargetReferenceKind::ExposedAgentPath);
    assert!(!stored.requested_target_set_complete);
    assert_eq!(stored.resolved_target_set_complete, Some(false));
    assert!(snapshot.events[0].truncated);
}

#[test]
fn exact_duplicates_count_as_duplicates_and_conflicting_variants_once() {
    let active = recorder("capture-a");
    active.record(event(EventKind::OperationValidated)).unwrap();
    let first = active.snapshot().unwrap().events.remove(0);
    let alternate = RecordedEvent {
        identity: first.identity.clone(),
        input: event(EventKind::WaitAbandoned),
        truncated: false,
    };
    let summary = Summary::reduce(&[first.clone(), first.clone(), alternate.clone(), alternate]);
    assert_eq!(summary.duplicate_count, 2);
    assert_eq!(summary.conflicting_identity_count, 1);
    assert_eq!(summary.event_count, 0);
    assert!(summary.partitions_conserve());
}

#[test]
fn wait_metadata_conflicts_quarantine_same_outcome_with_different_duration() {
    let active = recorder("capture-a");
    let mut first = event(EventKind::WaitCompleted);
    first.wait = Some(wait("wait-1", SelectedOutcome::Timeout));
    let mut second = first.clone();
    second.wait.as_mut().unwrap().blocked_duration_ns = Some(11);
    active.record(first).unwrap();
    active.record(second).unwrap();
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.conflicting_wait_count, 1);
    assert_eq!(summary.waits_by_outcome.get(&SelectedOutcome::Unknown), Some(&1));
    assert!(summary.wait_timelines[0].conflicting);
    assert!(!summary.wait_timelines[0].complete);
}

#[test]
fn scheduler_publication_before_registration_joins_by_exact_turn_and_correlation() {
    let active = recorder("capture-a");
    for phase in [SchedulerPhase::TurnStartProducerPublication, SchedulerPhase::TaskRegistered] {
        let mut input = event(match phase {
            SchedulerPhase::TaskRegistered => EventKind::TaskRegistered,
            _ => EventKind::TurnStartPublicationObserved,
        });
        input.scheduler = Some(SchedulerObservation {
            primitive: "pending-work".into(),
            correlation_id: Some("schedule-1".into()),
            pending_mail_observed: Some(true),
            trigger_turn_mail_observed: Some(false),
            durable_sleep_observed: Some(true),
            idle_reservation_accepted: Some(true),
            reservation_still_matches: Some(true),
            eligibility_basis: EligibilityBasis::QueueOnlyDurableSleep,
            drained_message_cohort_ids: vec!["submission-1".into()],
            drained_cohort_complete: true,
            drained_cohort_contains_trigger_turn_mail: Some(false),
            phase,
            task_registration_event: None,
            scheduled_turn_id: Some("turn-1".into()),
            turn_start_publication_event: None,
            outcome: SchedulerOutcome::TaskRegistered,
        });
        active.record(input).unwrap();
    }
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.scheduler_timelines.len(), 1);
    let joined = &summary.scheduler_timelines[0];
    assert_eq!(joined.task_registered, Some(true));
    assert_eq!(joined.turn_start_published, Some(true));
    assert_eq!(joined.eligible, None);
    assert!(!joined.complete);
}

#[test]
fn observed_external_duration_is_not_relabelled_blocked_time() {
    let active = recorder("capture-a");
    let mut input = event(EventKind::WaitCompleted);
    input.source_plane = SourcePlane::ExternalHostEnvelope;
    input.quality = ObservationQuality::ObservedOnly;
    input.external_operation = Some(ExternalOperationObservation {
        call_id: "call-1".into(),
        returned_duration_ns: Some(42),
    });
    active.record(input).unwrap();
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.external_durations[0].observed_request_return_ns, Some(42));
    assert_eq!(summary.external_durations[0].quality, ObservationQuality::ObservedOnly);
    assert_eq!(summary.wait_timelines.len(), 0);
}

#[test]
fn repeated_wait_groups_are_namespaced_by_capture_plane_and_thread() {
    let active = recorder("capture-a");
    for (thread, operation) in [("thread-1", "op-1"), ("thread-1", "op-2"), ("thread-2", "op-3")] {
        let mut input = event(EventKind::WaitCompleted);
        input.thread_id = Some(thread.into());
        input.operation_id = Some(operation.into());
        let mut observation = wait(operation, SelectedOutcome::Timeout);
        observation.request_fingerprint = Some("target-mode-fingerprint".into());
        observation.result_fingerprint = Some("observed-result-fingerprint".into());
        input.wait = Some(observation);
        active.record(input).unwrap();
    }
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.repeated_wait_groups, 1);
}

#[test]
fn status_repetition_requires_complete_exact_allowlisted_projection() {
    let active = recorder("capture-a");
    let actor = StatusActorProjection {
        agent_id: "actor-1".into(),
        canonical_path: "agent/path/one".into(),
        configured_model: Some("model-a".into()),
        configured_reasoning_effort: Some("medium".into()),
        raw_status_tag: Some("running".into()),
    };
    for (operation, path, complete) in [
        ("op-1", "agent/path/one", true),
        ("op-2", "agent/path/one", true),
        ("op-3", "agent/path/two", true),
        ("op-4", "agent/path/one", false),
    ] {
        let mut input = event(EventKind::StatusQueryObserved);
        input.operation_id = Some(operation.into());
        input.status_query = Some(StatusQueryObservation {
            request_fingerprint: Some("same-request-fingerprint".into()),
            result_fingerprint: Some("same-result-fingerprint".into()),
            readiness: None,
            request_projection: Some(StatusRequestProjection {
                path_prefix: Some("agent/".into()),
                requested_agent_ids: vec![],
                complete: true,
            }),
            result_projection: Some(StatusResultProjection {
                observed_actor_count: Some(1),
                actors: vec![StatusActorProjection { canonical_path: path.into(), ..actor.clone() }],
                complete,
            }),
        });
        active.record(input).unwrap();
    }
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.repeated_status_query_groups, 1);
}

#[test]
fn status_cohort_over_limit_is_retained_bounded_but_not_claimed_complete() {
    let active = recorder("capture-a");
    let mut input = event(EventKind::StatusQueryObserved);
    input.operation_id = Some("op-status".into());
    input.status_query = Some(StatusQueryObservation {
        request_fingerprint: None,
        result_fingerprint: None,
        readiness: None,
        request_projection: None,
        result_projection: Some(StatusResultProjection {
            observed_actor_count: Some(65),
            actors: (0..65).map(|n| StatusActorProjection {
                agent_id: format!("actor-{n}"),
                canonical_path: format!("agent/path/{n}"),
                configured_model: None,
                configured_reasoning_effort: None,
                raw_status_tag: Some("running".into()),
            }).collect(),
            complete: true,
        }),
    });
    active.record(input).unwrap();
    let snapshot = active.snapshot().unwrap();
    let projection = snapshot.events[0].input.status_query.as_ref().unwrap().result_projection.as_ref().unwrap();
    assert_eq!(projection.actors.len(), MAX_TARGETS_PER_EVENT);
    assert_eq!(projection.actors.capacity(), MAX_TARGETS_PER_EVENT);
    assert!(!projection.complete);
    assert!(snapshot.events[0].truncated);
    assert_eq!(Summary::reduce_snapshot(&snapshot).repeated_status_query_groups, 0);
}

#[test]
fn missing_block_start_is_not_a_complete_wait_duration() {
    let active = recorder("capture-a");
    let mut input = event(EventKind::WaitCompleted);
    let mut observation = wait("wait-1", SelectedOutcome::Timeout);
    observation.blocked_start_offset_ns = None;
    input.wait = Some(observation);
    active.record(input).unwrap();
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert!(!summary.wait_timelines[0].complete);
}

#[test]
fn queued_only_pending_sleep_remains_its_own_primitive_outcome() {
    let active = recorder("capture-a");
    let mut input = event(EventKind::SleepSelected);
    input.sleep = Some(SleepObservation {
        pending_activity: SleepPendingActivity::QueueOnly,
        selection: SleepSelection::AlreadyPending,
        requested_duration_ns: Some(1_000_000),
        operation_duration_ns: Some(5_000),
        blocked_duration_ns: Some(0),
    });
    active.record(input).unwrap();
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.sleep_timelines[0].selection, SleepSelection::AlreadyPending);
    assert_eq!(summary.sleep_timelines[0].blocked_duration_ns, Some(0));
}

#[test]
fn disable_discards_capture_and_snapshot_reports_incomplete_high_water() {
    let active = recorder("capture-a");
    active.record(event(EventKind::OperationValidated)).unwrap();
    active.disable_and_clear();
    assert_eq!(active.mode(), CaptureMode::Off);
    let snapshot = active.snapshot().unwrap();
    assert_eq!(snapshot.events.len(), 0);
    assert!(Summary::reduce_snapshot(&snapshot).capture_loss_observed);
}
