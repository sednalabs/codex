//! Structured serialization of the shared reducer's bounded typed output.

use codex_diagnostics::control_plane::BoundaryTimeline;
use codex_diagnostics::control_plane::EventIdentity;
use codex_diagnostics::control_plane::MessageTimeline;
use codex_diagnostics::control_plane::QueueTimeline;
use codex_diagnostics::control_plane::SchedulerTimeline;
use codex_diagnostics::control_plane::SleepTimeline;
use codex_diagnostics::control_plane::Summary;
use codex_diagnostics::control_plane::WaitTimeline;
use codex_diagnostics::control_plane::ProviderCallTimeline;
use codex_diagnostics::control_plane::ProviderLedgerWriteOutcome;
use codex_diagnostics::control_plane::UsageAccountScope;
use serde_json::json;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(super) fn project(summary: &Summary) -> serde_json::Value {
    json!({
        "eventCount": summary.event_count,
        "duplicateCount": summary.duplicate_count,
        "conflictingIdentityCount": summary.conflicting_identity_count,
        "conflictingWaitCount": summary.conflicting_wait_count,
        "waitsByOutcome": debug_counts(&summary.waits_by_outcome),
        "waitTimelines": summary.wait_timelines.iter().map(wait).collect::<Vec<_>>(),
        "sleepTimelines": summary.sleep_timelines.iter().map(sleep).collect::<Vec<_>>(),
        "schedulerTimelines": summary.scheduler_timelines.iter().map(scheduler).collect::<Vec<_>>(),
        "messageTimelines": summary.message_timelines.iter().map(message).collect::<Vec<_>>(),
        "boundaryTimelines": summary.boundary_timelines.iter().map(boundary).collect::<Vec<_>>(),
        "providerCallTimelines": summary.provider_call_timelines.iter().map(provider_call).collect::<Vec<_>>(),
        "queueTimelines": summary.queue_timelines.iter().map(queue).collect::<Vec<_>>(),
        "externalDurations": summary.external_durations.iter().map(|row| json!({
            "identity": identity(&row.identity),
            "sourcePlane": format!("{:?}", row.source_plane),
            "threadId": row.thread_id,
            "operationId": row.operation_id,
            "callId": row.call_id,
            "observedRequestReturnNs": row.observed_request_return_ns,
            "quality": format!("{:?}", row.quality),
            "conflicting": row.conflicting
        })).collect::<Vec<_>>(),
        "repeatedWaitGroups": summary.repeated_wait_groups,
        "repeatedStatusQueryGroups": summary.repeated_status_query_groups,
        "unknownRootEvents": summary.unknown_root_events,
        "truncatedEvents": summary.truncated_events,
        "omittedInputEvents": summary.omitted_input_events,
        "captureLossObserved": summary.capture_loss_observed,
        "partitionsConserve": summary.partitions_conserve()
    })
}

fn provider_call(row: &ProviderCallTimeline) -> serde_json::Value {
    let observation = &row.observation;
    json!({
        "identity": identity(&row.identity),
        "sourcePlane": source_plane(row.source_plane),
        "producerBoundary": row.producer_boundary,
        "producerVersion": row.producer_version,
        "operationId": row.operation_id,
        "threadId": row.thread_id,
        "turnId": row.turn_id,
        "rootThreadId": row.root_thread_id,
        "parentThreadId": row.parent_thread_id,
        "forkParentThreadId": row.fork_parent_thread_id,
        "windowId": row.window_id,
        "windowNumber": row.window_number,
        "previousWindowId": row.previous_window_id,
        "wallCorrelation": wall_time(row.wall_correlation),
        "monotonicOffsetNs": row.monotonic_offset_ns,
        "quality": quality(row.quality),
        "coverage": row.coverage.iter().map(|mark| json!({
            "field": coverage_field(mark.field),
            "unknown": mark.unknown.map(unknown_reason)
        })).collect::<Vec<_>>(),
        "observation": {
            "provider": observation.provider,
            "responseId": observation.response_id,
            "persistedProviderCallId": observation.persisted_provider_call_id,
            "ledgerResponseScope": account_scope(&observation.ledger_response_scope),
            "ledgerWriteOutcome": ledger_outcome(observation.ledger_write_outcome)
        },
        "complete": row.complete
    })
}

fn source_plane(value: codex_diagnostics::control_plane::SourcePlane) -> &'static str {
    use codex_diagnostics::control_plane::SourcePlane::*;
    match value {
        RustCollab => "rustCollab",
        AppServer => "appServer",
        ExternalHostEnvelope => "externalHostEnvelope",
        ExistingUsageLedger => "existingUsageLedger",
        Unknown => "unknown",
    }
}

fn quality(value: codex_diagnostics::control_plane::ObservationQuality) -> &'static str {
    use codex_diagnostics::control_plane::ObservationQuality::*;
    match value {
        Owned => "owned",
        ObservedOnly => "observedOnly",
        Unknown => "unknown",
    }
}

fn account_scope(value: &UsageAccountScope) -> serde_json::Value {
    match value {
        UsageAccountScope::KnownScope(scope) => json!({"kind": "knownScope", "value": scope}),
        UsageAccountScope::WriterUnscoped => json!({"kind": "writerUnscoped"}),
        UsageAccountScope::Unknown => json!({"kind": "unknown"}),
    }
}

fn ledger_outcome(value: ProviderLedgerWriteOutcome) -> &'static str {
    match value {
        ProviderLedgerWriteOutcome::Inserted => "inserted",
        ProviderLedgerWriteOutcome::Duplicate => "duplicate",
        ProviderLedgerWriteOutcome::FailedUnknown => "failedUnknown",
        ProviderLedgerWriteOutcome::NotConfigured => "notConfigured",
    }
}

fn coverage_field(value: codex_diagnostics::control_plane::CoverageField) -> &'static str {
    use codex_diagnostics::control_plane::CoverageField::*;
    match value {
        ProducerVersion => "producerVersion",
        OperationId => "operationId",
        ThreadId => "threadId",
        TurnId => "turnId",
        RootThreadId => "rootThreadId",
        ParentThreadId => "parentThreadId",
        ForkParentThreadId => "forkParentThreadId",
        WindowId => "windowId",
        RequestedTimeout => "requestedTimeout",
        EffectiveTimeout => "effectiveTimeout",
        TargetSet => "targetSet",
        SelectedProducer => "selectedProducer",
        QueueState => "queueState",
        SemanticAcknowledgement => "semanticAcknowledgement",
        ProviderObservedIdentity => "providerObservedIdentity",
        Other => "other",
    }
}

fn unknown_reason(value: codex_diagnostics::control_plane::UnknownReason) -> &'static str {
    use codex_diagnostics::control_plane::UnknownReason::*;
    match value {
        NotExposed => "notExposed",
        UnsupportedProducer => "unsupportedProducer",
        PreCapture => "preCapture",
        Evicted => "evicted",
        Lost => "lost",
        Truncated => "truncated",
        Legacy => "legacy",
        Ambiguous => "ambiguous",
    }
}

fn identity(id: &EventIdentity) -> serde_json::Value {
    json!({"captureInstanceId": id.capture_instance_id, "sequence": id.sequence})
}

fn wall_time(time: SystemTime) -> serde_json::Value {
    match time.duration_since(UNIX_EPOCH) {
        Ok(value) => json!({"secondsSinceEpoch": value.as_secs(), "subsecondNanos": value.subsec_nanos()}),
        Err(_) => json!({"beforeUnixEpoch": true}),
    }
}

fn wait(row: &WaitTimeline) -> serde_json::Value {
    json!({
        "sourceEvents": row.source_events.iter().map(identity).collect::<Vec<_>>(),
        "captureInstanceId": row.capture_instance_id,
        "sourcePlane": format!("{:?}", row.source_plane),
        "threadId": row.thread_id,
        "turnId": row.turn_id,
        "rootThreadId": row.root_thread_id,
        "parentThreadId": row.parent_thread_id,
        "forkParentThreadId": row.fork_parent_thread_id,
        "windowId": row.window_id,
        "windowNumber": row.window_number,
        "previousWindowId": row.previous_window_id,
        "wallCorrelation": row.wall_correlation.as_ref().map(|time| wall_time(time.to_owned())),
        "operationId": row.operation_id,
        "waitId": row.wait_id,
        "phase": format!("{:?}", row.phase),
        "primitive": format!("{:?}", row.primitive),
        "helperId": row.helper_id,
        "helperVersion": row.helper_version,
        "requestedTimeoutMs": row.requested_timeout_ms,
        "effectiveTimeoutMs": row.effective_timeout_ms,
        "returnWhen": format!("{:?}", row.return_when),
        "targetMode": format!("{:?}", row.target_mode),
        "anyTargets": row.any_targets,
        "requestedTargetReferences": row.requested_target_ids,
        "requestedTargetKind": format!("{:?}", row.requested_target_kind),
        "requestedTargetSetComplete": row.requested_target_set_complete,
        "resolvedTargetIds": row.resolved_target_ids,
        "resolvedTargetKind": format!("{:?}", row.resolved_target_kind),
        "resolvedTargetSetComplete": row.resolved_target_set_complete,
        "blockedStartOffsetNs": row.blocked_start_offset_ns,
        "blockedEndOffsetNs": row.blocked_end_offset_ns,
        "operationDurationNs": row.operation_duration_ns,
        "blockedDurationNs": row.blocked_duration_ns,
        "selectedOutcome": format!("{:?}", row.selected_outcome),
        "selectedProducer": row.selected_producer.as_ref().map(identity),
        "selectedTargetId": row.selected_target_id,
        "selectedTargetTurnId": row.selected_target_turn_id,
        "continuationOfWaitId": row.continuation_of_wait_id,
        "subscribedReadiness": row.subscribed_readiness.iter().map(|value| json!({
            "state": format!("{:?}", value.state), "targetTurnId": value.target_turn_id
        })).collect::<Vec<_>>(),
        "selectedReadiness": row.selected_readiness.iter().map(|value| json!({
            "state": format!("{:?}", value.state), "targetTurnId": value.target_turn_id
        })).collect::<Vec<_>>(),
        "coverage": row.coverage.iter().map(|mark| json!({
            "field": format!("{:?}", mark.field),
            "unknown": mark.unknown.map(|reason| format!("{:?}", reason))
        })).collect::<Vec<_>>(),
        "complete": row.complete,
        "conflicting": row.conflicting
    })
}

fn sleep(row: &SleepTimeline) -> serde_json::Value {
    json!({
        "captureInstanceId": row.capture_instance_id,
        "sourcePlane": format!("{:?}", row.source_plane),
        "threadId": row.thread_id,
        "operationId": row.operation_id,
        "pendingActivity": format!("{:?}", row.pending_activity),
        "selection": format!("{:?}", row.selection),
        "requestedDurationNs": row.requested_duration_ns,
        "operationDurationNs": row.operation_duration_ns,
        "blockedDurationNs": row.blocked_duration_ns,
        "complete": row.complete
    })
}

fn scheduler(row: &SchedulerTimeline) -> serde_json::Value {
    json!({
        "captureInstanceId": row.capture_instance_id,
        "sourcePlane": format!("{:?}", row.source_plane),
        "threadId": row.thread_id,
        "turnId": row.turn_id,
        "correlationId": row.correlation_id,
        "eligible": row.eligible,
        "eligibilityBasis": format!("{:?}", row.eligibility_basis),
        "pendingMailObserved": row.pending_mail_observed,
        "triggerTurnMailObserved": row.trigger_turn_mail_observed,
        "durableSleepObserved": row.durable_sleep_observed,
        "reservationAccepted": row.reservation_accepted,
        "reservationStillMatches": row.reservation_still_matches,
        "reservationLost": row.reservation_lost,
        "taskRegistered": row.task_registered,
        "turnStartPublished": row.turn_start_published,
        "drainedCohortIds": row.drained_cohort_ids,
        "drainedCohortComplete": row.drained_cohort_complete,
        "drainedCohortContainsTriggerTurnMail": row.drained_cohort_contains_trigger_turn_mail,
        "outcome": format!("{:?}", row.outcome),
        "complete": row.complete,
        "conflicting": row.conflicting,
        "incomplete": row.incomplete
    })
}

fn message(row: &MessageTimeline) -> serde_json::Value {
    let msg = &row.observation;
    json!({
        "identity": identity(&row.identity),
        "sourcePlane": format!("{:?}", row.source_plane),
        "operationId": row.operation_id,
        "threadId": row.thread_id,
        "turnId": row.turn_id,
        "rootThreadId": row.root_thread_id,
        "parentThreadId": row.parent_thread_id,
        "forkParentThreadId": row.fork_parent_thread_id,
        "windowId": row.window_id,
        "windowNumber": row.window_number,
        "previousWindowId": row.previous_window_id,
        "wallCorrelation": wall_time(row.wall_correlation),
        "monotonicOffsetNs": row.monotonic_offset_ns,
        "intent": format!("{:?}", msg.intent),
        "submissionId": msg.submission_id,
        "senderThreadId": msg.sender_thread_id,
        "recipientThreadId": msg.recipient_thread_id,
        "acceptedEvent": msg.accepted_event.as_ref().map(identity),
        "enqueuedEvent": msg.enqueued_event.as_ref().map(identity),
        "drainedEvent": msg.drained_event.as_ref().map(identity),
        "inputRecordedEvent": msg.input_recorded_event.as_ref().map(identity),
        "activityEvent": msg.activity_event.as_ref().map(identity),
        "selectedByWaits": msg.selected_by_waits,
        "scheduledTurnId": msg.scheduled_turn_id,
        "semanticAcknowledgement": msg.semantic_acknowledgement,
        "followupSelection": format!("{:?}", msg.followup_selection),
        "supersededSubmissionIds": msg.superseded_submission_ids,
        "supersededSelectionComplete": msg.superseded_selection_complete,
        "incomplete": row.incomplete
    })
}

fn boundary(row: &BoundaryTimeline) -> serde_json::Value {
    json!({
        "identity": identity(&row.identity),
        "sourcePlane": format!("{:?}", row.source_plane),
        "kind": format!("{:?}", row.kind),
        "operationId": row.operation_id,
        "threadId": row.thread_id,
        "turnId": row.turn_id,
        "rootThreadId": row.root_thread_id,
        "parentThreadId": row.parent_thread_id,
        "forkParentThreadId": row.fork_parent_thread_id,
        "windowId": row.window_id,
        "windowNumber": row.window_number,
        "previousWindowId": row.previous_window_id,
        "wallCorrelation": wall_time(row.wall_correlation),
        "monotonicOffsetNs": row.monotonic_offset_ns,
        "incomplete": row.incomplete
    })
}

fn queue(row: &QueueTimeline) -> serde_json::Value {
    json!({
        "captureInstanceId": row.capture_instance_id,
        "sourcePlane": format!("{:?}", row.source_plane),
        "threadId": row.thread_id,
        "turnId": row.turn_id,
        "scope": format!("{:?}", row.observation.scope),
        "transitionSequence": row.observation.transition_sequence,
        "queueBefore": row.observation.queue_before,
        "queueAfter": row.observation.queue_after
    })
}

fn debug_counts<K: std::fmt::Debug + Ord>(counts: &std::collections::BTreeMap<K, usize>) -> serde_json::Value {
    serde_json::Value::Object(counts.iter().map(|(key, count)| (format!("{key:?}"), json!(count)))
        .collect())
}
