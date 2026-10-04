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
        provider_call: None,
    }
}
fn wait(id: &str, outcome: SelectedOutcome) -> WaitObservation {
    WaitObservation {
        wait_id: id.into(),
        phase: WaitPhase::Completed,
        primitive: WaitPrimitive::V2Wait,
        return_when: ReturnWhen::Any,
        helper_id: Some("wait_agent".into()),
        helper_version: None,
        requested_timeout_ms: Some(-1),
        effective_timeout_ms: Some(0),
        target_mode: TargetMode::Targeted,
        any_targets: Some(true),
        target_ids: vec!["resolved-thread".into()],
        target_set_complete: true,
        resolved_target_kind: TargetReferenceKind::ThreadId,
        requested_target_ids: vec!["agent/path".into()],
        requested_target_kind: TargetReferenceKind::ExposedAgentPath,
        requested_target_set_complete: true,
        resolved_target_set_complete: Some(true),
        subscribed_readiness: vec![],
        subscribed_readiness_complete: true,
        selected_readiness: vec![],
        selected_readiness_complete: true,
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
        observed_host_return: None,
    }
}
fn recorder(id: &str) -> ControlPlaneRecorder {
    ControlPlaneRecorder::new(CaptureMode::Session, id).unwrap()
}
fn provider_event(
    outcome: ProviderLedgerWriteOutcome,
    response_scope: UsageAccountScope,
    persisted_id: Option<&str>,
) -> EventInput {
    let mut input = event(EventKind::ProviderCompletionObserved);
    input.producer_boundary = PROVIDER_COMPLETION_PRODUCER_BOUNDARY.into();
    input.provider_call = Some(ProviderCallObservation {
        provider: "provider-a".into(),
        response_id: "response-a".into(),
        persisted_provider_call_id: persisted_id.map(str::to_owned),
        ledger_response_scope: response_scope,
        ledger_write_outcome: outcome,
    });
    input
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
fn every_retained_readiness_target_and_optional_reference_is_validated() {
    let active = recorder("capture-a");
    let mut invalid = event(EventKind::OutcomePublished);
    invalid.readiness = Some(ReadinessObservation {
        target: Some(TargetReference { id: "bad\0target".into(), kind: TargetReferenceKind::ThreadId }),
        state: Readiness::Pending,
        target_turn_id: None,
    });
    assert_eq!(active.record(invalid), None);

    let mut invalid_query = event(EventKind::StatusQueryObserved);
    invalid_query.status_query = Some(StatusQueryObservation {
        request_fingerprint: None,
        result_fingerprint: None,
        readiness: Some(ReadinessObservation {
            target: Some(TargetReference { id: "t".repeat(257), kind: TargetReferenceKind::ThreadId }),
            state: Readiness::Pending,
            target_turn_id: None,
        }),
        request_projection: None,
        result_projection: None,
    });
    assert_eq!(active.record(invalid_query), None);
    assert_eq!(active.losses().invalid_identity, 2);
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
fn invalid_event_identity_is_excluded_and_accounted_as_capture_loss() {
    let invalid = RecordedEvent {
        identity: EventIdentity { capture_instance_id: "capture-a".into(), sequence: 0 },
        input: event(EventKind::OperationValidated),
        truncated: false,
    };
    let summary = Summary::reduce(&[invalid]);
    assert_eq!(summary.invalid_event_identity_count, 1);
    assert_eq!(summary.event_count, 0);
    assert!(summary.capture_loss_observed);
    assert!(summary.partitions_conserve());
}

#[test]
fn wait_metadata_conflicts_quarantine_same_outcome_with_different_duration() {
    let active = recorder("capture-a");
    let mut first = event(EventKind::WaitCompleted);
    first.wait = Some(wait("wait-1", SelectedOutcome::Timeout));
    first.wait.as_mut().unwrap().selected_producer = Some(EventIdentity {
        capture_instance_id: "producer-capture".into(),
        sequence: 7,
    });
    first.wait.as_mut().unwrap().selected_target_id = Some("resolved-thread".into());
    let mut second = first.clone();
    second.wait.as_mut().unwrap().blocked_duration_ns = Some(11);
    active.record(first).unwrap();
    active.record(second).unwrap();
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.conflicting_wait_count, 1);
    assert_eq!(summary.waits_by_outcome.get(&SelectedOutcome::Unknown), Some(&1));
    assert!(summary.wait_timelines[0].conflicting);
    assert!(!summary.wait_timelines[0].complete);
    assert_eq!(summary.wait_timelines[0].blocked_duration_ns, None);
    assert_eq!(summary.wait_timelines[0].resolved_target_ids, Vec::<String>::new());
    assert_eq!(summary.wait_timelines[0].selected_producer, None);
    assert_eq!(summary.wait_timelines[0].selected_target_id, None);
    assert_eq!(summary.wait_timelines[0].source_events.len(), 2);
    assert_eq!(summary.wait_timelines[0].phase, WaitPhase::Unknown);
}

#[test]
fn wait_reduction_uses_turn_and_operation_namespaces_and_quarantines_cross_phase_outcomes() {
    let active = recorder("capture-a");
    for (turn, operation) in [("turn-a", "op-a"), ("turn-b", "op-b")] {
        let mut input = event(EventKind::WaitCompleted);
        input.turn_id = Some(turn.into());
        input.operation_id = Some(operation.into());
        input.wait = Some(wait("reused-wait", SelectedOutcome::Timeout));
        active.record(input).unwrap();
    }
    let mut selected = event(EventKind::WaitSelected);
    selected.operation_id = Some("op-conflict".into());
    let mut selected_wait = wait("cross-phase", SelectedOutcome::Timeout);
    selected_wait.phase = WaitPhase::Selected;
    selected.wait = Some(selected_wait);
    active.record(selected).unwrap();
    let mut completed = event(EventKind::WaitCompleted);
    completed.operation_id = Some("op-conflict".into());
    completed.wait = Some(wait("cross-phase", SelectedOutcome::TargetTerminal));
    active.record(completed).unwrap();

    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.wait_timelines.len(), 3);
    assert_eq!(summary.wait_timelines.iter().filter(|row| row.wait_id == "reused-wait").count(), 2);
    let conflict = summary.wait_timelines.iter().find(|row| row.wait_id == "cross-phase").unwrap();
    assert!(conflict.conflicting);
    assert_eq!(conflict.selected_outcome, SelectedOutcome::Unknown);
    assert_eq!(conflict.operation_id, None);
    assert!(!conflict.complete);
}

#[test]
fn wait_timeline_retains_exact_requested_and_resolved_target_planes_and_lineage() {
    let active = recorder("capture-a");
    let mut input = event(EventKind::WaitCompleted);
    input.operation_id = Some("wait-op".into());
    input.root_thread_id = Some("root-thread".into());
    input.parent_thread_id = Some("parent-thread".into());
    input.fork_parent_thread_id = Some("fork-thread".into());
    input.window_id = Some("window-2".into());
    input.window_number = Some(2);
    input.previous_window_id = Some("window-1".into());
    input.wait = Some(wait("wait-1", SelectedOutcome::TargetTerminal));
    active.record(input).unwrap();
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    let timeline = &summary.wait_timelines[0];
    assert_eq!(timeline.operation_id.as_deref(), Some("wait-op"));
    assert_eq!(timeline.root_thread_id.as_deref(), Some("root-thread"));
    assert_eq!(timeline.parent_thread_id.as_deref(), Some("parent-thread"));
    assert_eq!(timeline.fork_parent_thread_id.as_deref(), Some("fork-thread"));
    assert_eq!(timeline.window_id.as_deref(), Some("window-2"));
    assert_eq!(timeline.window_number, Some(2));
    assert_eq!(timeline.previous_window_id.as_deref(), Some("window-1"));
    assert_eq!(timeline.requested_target_ids, vec!["agent/path".to_owned()]);
    assert_eq!(timeline.requested_target_kind, TargetReferenceKind::ExposedAgentPath);
    assert!(timeline.requested_target_set_complete);
    assert_eq!(timeline.resolved_target_ids, vec!["resolved-thread".to_owned()]);
    assert_eq!(timeline.resolved_target_kind, TargetReferenceKind::ThreadId);
    assert_eq!(timeline.resolved_target_set_complete, Some(true));
    assert_eq!(timeline.helper_id.as_deref(), Some("wait_agent"));
}

#[test]
fn incomplete_wait_start_is_preserved_as_unknown_incomplete_timeline() {
    let active = recorder("capture-a");
    let mut input = event(EventKind::WaitBlockingStarted);
    let mut observation = wait("wait-start-only", SelectedOutcome::Timeout);
    observation.phase = WaitPhase::Blocking;
    observation.blocked_start_offset_ns = Some(15);
    observation.blocked_end_offset_ns = None;
    observation.operation_duration_ns = None;
    observation.blocked_duration_ns = None;
    input.wait = Some(observation);
    let identity = active.record(input).unwrap();
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.wait_timelines.len(), 1);
    let timeline = &summary.wait_timelines[0];
    assert_eq!(timeline.source_events, vec![identity]);
    assert_eq!(timeline.phase, WaitPhase::Blocking);
    assert_eq!(timeline.selected_outcome, SelectedOutcome::Unknown);
    assert_eq!(timeline.blocked_duration_ns, None);
    assert!(!timeline.complete);
}

#[test]
fn message_and_window_recovery_boundaries_remain_exact_observation_rows() {
    let active = recorder("capture-a");
    let mut message_event = event(EventKind::MessageAccepted);
    message_event.operation_id = Some("send-op".into());
    message_event.root_thread_id = Some("root-thread".into());
    message_event.window_id = Some("window-2".into());
    message_event.window_number = Some(2);
    message_event.previous_window_id = Some("window-1".into());
    message_event.message = Some(MessageObservation {
        submission_id: Some("submission-1".into()),
        sender_thread_id: Some("sender".into()),
        recipient_thread_id: Some("recipient".into()),
        intent: DeliveryIntent::QueueOnly,
        accepted_event: None,
        enqueued_event: None,
        drained_event: None,
        input_recorded_event: None,
        activity_event: None,
        selected_by_waits: vec!["wait-1".into()],
        scheduled_turn_id: None,
        semantic_acknowledgement: None,
        followup_selection: FollowupSelection::Unknown,
        superseded_submission_ids: vec![],
        superseded_selection_complete: false,
    });
    active.record(message_event).unwrap();
    let mut window = event(EventKind::WindowBoundaryObserved);
    window.window_id = Some("window-2".into());
    window.window_number = Some(2);
    window.previous_window_id = Some("window-1".into());
    active.record(window).unwrap();
    let mut boundary = event(EventKind::RecoveryBoundaryObserved);
    boundary.root_thread_id = Some("root-thread".into());
    boundary.window_number = Some(3);
    boundary.previous_window_id = Some("window-2".into());
    active.record(boundary).unwrap();
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.message_timelines.len(), 1);
    assert_eq!(summary.message_timelines[0].observation.intent, DeliveryIntent::QueueOnly);
    assert_eq!(summary.message_timelines[0].observation.semantic_acknowledgement, None);
    assert_eq!(summary.message_timelines[0].observation.selected_by_waits, vec!["wait-1".to_owned()]);
    assert_eq!(summary.message_timelines[0].window_number, Some(2));
    assert_eq!(summary.message_timelines[0].previous_window_id.as_deref(), Some("window-1"));
    assert_eq!(summary.boundary_timelines.len(), 2);
    assert_eq!(summary.boundary_timelines[0].kind, EventKind::WindowBoundaryObserved);
    assert_eq!(summary.boundary_timelines[0].window_number, Some(2));
    assert_eq!(summary.boundary_timelines[1].kind, EventKind::RecoveryBoundaryObserved);
    assert_eq!(summary.boundary_timelines[1].window_number, Some(3));
    assert_eq!(summary.boundary_timelines[1].previous_window_id.as_deref(), Some("window-2"));
}

#[test]
fn truncated_producer_version_and_scheduler_rows_remain_incomplete() {
    let active = recorder("capture-a");
    let mut versioned = event(EventKind::OperationValidated);
    versioned.producer_version = Some("v".repeat(257));
    active.record(versioned).unwrap();
    let first = active.snapshot().unwrap().events.remove(0);
    assert!(first.truncated);
    assert_eq!(first.input.producer_version, None);
    assert!(first.input.field_coverage.iter().any(|mark|
        mark.field == CoverageField::ProducerVersion && mark.unknown == Some(UnknownReason::Truncated)));

    let mut scheduler_event = event(EventKind::MessageDrained);
    scheduler_event.scheduler = Some(SchedulerObservation {
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
        phase: SchedulerPhase::CohortDrained,
        task_registration_event: None,
        scheduled_turn_id: Some("turn-1".into()),
        turn_start_publication_event: None,
        outcome: SchedulerOutcome::TaskRegistered,
    });
    let summary = Summary::reduce(&[RecordedEvent {
        identity: EventIdentity { capture_instance_id: "capture-a".into(), sequence: 1 },
        input: scheduler_event,
        truncated: true,
    }]);
    assert_eq!(summary.scheduler_timelines.len(), 1);
    assert!(summary.scheduler_timelines[0].incomplete);
    assert!(!summary.scheduler_timelines[0].drained_cohort_complete);
    assert!(!summary.scheduler_timelines[0].complete);
}

#[test]
fn provider_completion_timeline_preserves_inserted_pk_and_duplicate_without_provisional_pk() {
    let active = recorder("capture-a");
    let mut inserted = provider_event(
        ProviderLedgerWriteOutcome::Inserted,
        UsageAccountScope::KnownScope("account-a".into()),
        Some("committed-call-pk"),
    );
    inserted.thread_id = Some("native-thread".into());
    inserted.turn_id = Some("native-turn".into());
    active.record(inserted).unwrap();
    let mut duplicate = provider_event(
        ProviderLedgerWriteOutcome::Duplicate,
        UsageAccountScope::WriterUnscoped,
        None,
    );
    duplicate.thread_id = Some("native-thread".into());
    duplicate.turn_id = Some("native-turn".into());
    duplicate.provider_call.as_mut().unwrap().response_id = "response-b".into();
    active.record(duplicate).unwrap();

    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.provider_call_timelines.len(), 2);
    let inserted = &summary.provider_call_timelines[0];
    assert!(inserted.complete);
    assert_eq!(inserted.identity.capture_instance_id, "capture-a");
    assert_eq!(inserted.identity.sequence, 1);
    assert_eq!(inserted.source_plane, SourcePlane::RustCollab);
    assert_eq!(inserted.producer_boundary, PROVIDER_COMPLETION_PRODUCER_BOUNDARY);
    assert_eq!(inserted.thread_id.as_deref(), Some("native-thread"));
    assert_eq!(inserted.turn_id.as_deref(), Some("native-turn"));
    assert_eq!(inserted.wall_correlation, SystemTime::UNIX_EPOCH);
    assert_eq!(inserted.observation, ProviderCallObservation {
        provider: "provider-a".into(),
        response_id: "response-a".into(),
        persisted_provider_call_id: Some("committed-call-pk".into()),
        ledger_response_scope: UsageAccountScope::KnownScope("account-a".into()),
        ledger_write_outcome: ProviderLedgerWriteOutcome::Inserted,
    });
    let duplicate = &summary.provider_call_timelines[1];
    assert!(duplicate.complete);
    assert_eq!(duplicate.identity.sequence, 2);
    assert_eq!(duplicate.observation, ProviderCallObservation {
        provider: "provider-a".into(),
        response_id: "response-b".into(),
        persisted_provider_call_id: None,
        ledger_response_scope: UsageAccountScope::WriterUnscoped,
        ledger_write_outcome: ProviderLedgerWriteOutcome::Duplicate,
    });
}

#[test]
fn provider_identity_validation_and_default_off_do_not_invent_persisted_identity() {
    let off = ControlPlaneRecorder::new(CaptureMode::Off, "capture-off").unwrap();
    let event = provider_event(
        ProviderLedgerWriteOutcome::Inserted,
        UsageAccountScope::KnownScope("account-a".into()),
        Some("committed-call-pk"),
    );
    assert_eq!(off.record(event), None);
    assert_eq!(off.losses().disabled, 1);
    assert!(off.snapshot().unwrap().events.is_empty());

    let active = recorder("capture-a");
    let mut invalid_provider = provider_event(
        ProviderLedgerWriteOutcome::Duplicate,
        UsageAccountScope::KnownScope("account-a".into()),
        None,
    );
    invalid_provider.provider_call.as_mut().unwrap().response_id = "r".repeat(257);
    assert_eq!(active.record(invalid_provider), None);
    assert_eq!(active.losses().invalid_identity, 1);

    let invalid_scope = provider_event(
        ProviderLedgerWriteOutcome::Duplicate,
        UsageAccountScope::KnownScope(String::new()),
        None,
    );
    assert_eq!(active.record(invalid_scope), None);
    assert_eq!(active.losses().invalid_identity, 2);
}

#[test]
fn inconsistent_provider_pk_receipt_is_quarantined_and_not_complete() {
    let active = recorder("capture-a");
    let event = provider_event(
        ProviderLedgerWriteOutcome::Duplicate,
        UsageAccountScope::Unknown,
        Some("not-a-canonical-pk"),
    );
    active.record(event).unwrap();
    let snapshot = active.snapshot().unwrap();
    assert!(snapshot.events[0].truncated);
    assert_eq!(snapshot.events[0].input.provider_call.as_ref().unwrap().persisted_provider_call_id, None);
    assert!(snapshot.events[0].input.field_coverage.iter().any(|mark|
        mark.field == CoverageField::ProviderObservedIdentity && mark.unknown == Some(UnknownReason::Ambiguous)));
    let summary = Summary::reduce_snapshot(&snapshot);
    assert_eq!(summary.provider_call_timelines.len(), 1);
    assert!(!summary.provider_call_timelines[0].complete);
    assert_eq!(summary.provider_call_timelines[0].observation.ledger_write_outcome, ProviderLedgerWriteOutcome::Duplicate);
}

#[test]
fn provider_completion_timelines_become_incomplete_when_capture_reports_loss() {
    let active = recorder("capture-a");
    let mut input = provider_event(
        ProviderLedgerWriteOutcome::Inserted,
        UsageAccountScope::Unknown,
        Some("committed-call-pk"),
    );
    input.thread_id = Some("native-thread".into());
    input.turn_id = Some("native-turn".into());
    active.record(input).unwrap();
    let mut snapshot = active.snapshot().unwrap();
    snapshot.losses.contention = 1;
    let summary = Summary::reduce_snapshot(&snapshot);
    assert_eq!(summary.provider_call_timelines.len(), 1);
    assert!(!summary.provider_call_timelines[0].complete);
    assert!(summary.capture_loss_observed);
    assert_eq!(summary.recorder_losses.unwrap().contention, 1);
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
    assert_eq!(joined.source_events.iter().map(|identity| identity.sequence).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(joined.task_registered, Some(true));
    assert_eq!(joined.turn_start_published, Some(true));
    assert_eq!(joined.eligible, None);
    assert!(!joined.complete);
}

#[test]
fn scheduler_reservation_that_no_longer_matches_is_not_scheduled_completion() {
    let active = recorder("capture-a");
    let mut input = event(EventKind::SchedulerEligibilityObserved);
    input.scheduler = Some(SchedulerObservation {
        primitive: "pending-work".into(),
        correlation_id: Some("schedule-lost".into()),
        pending_mail_observed: Some(true),
        trigger_turn_mail_observed: Some(false),
        durable_sleep_observed: Some(true),
        idle_reservation_accepted: Some(true),
        reservation_still_matches: Some(false),
        eligibility_basis: EligibilityBasis::QueueOnlyDurableSleep,
        drained_message_cohort_ids: vec![],
        drained_cohort_complete: false,
        drained_cohort_contains_trigger_turn_mail: None,
        phase: SchedulerPhase::ReservationAccepted,
        task_registration_event: None,
        scheduled_turn_id: Some("turn-lost".into()),
        turn_start_publication_event: None,
        outcome: SchedulerOutcome::ReservationLostObserved,
    });
    active.record(input).unwrap();
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    let row = &summary.scheduler_timelines[0];
    assert_eq!(row.reservation_accepted, Some(true));
    assert_eq!(row.reservation_still_matches, Some(false));
    assert_eq!(row.reservation_lost, Some(true));
    assert!(!row.complete);
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
fn helper_return_when_remains_distinct_from_primitive_any_target_selector() {
    let active = recorder("capture-a");
    for (operation, return_when) in [("op-any", ReturnWhen::Any), ("op-all", ReturnWhen::All)] {
        let mut input = event(EventKind::WaitCompleted);
        input.operation_id = Some(operation.into());
        let mut observation = wait(operation, SelectedOutcome::Timeout);
        observation.any_targets = Some(true);
        observation.return_when = return_when;
        observation.request_fingerprint = Some("same-opaque-fingerprint".into());
        observation.result_fingerprint = Some("same-opaque-fingerprint".into());
        input.wait = Some(observation);
        active.record(input).unwrap();
    }
    let summary = Summary::reduce_snapshot(&active.snapshot().unwrap());
    assert_eq!(summary.repeated_wait_groups, 0);
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
    assert_eq!(summary.sleep_timelines[0].identity.sequence, 1);
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
