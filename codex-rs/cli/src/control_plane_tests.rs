use super::envelopes;
use super::summary;
use clap::Parser;
use pretty_assertions::assert_eq;

pub(super) fn rollout_pair(name: &str, arguments: serde_json::Value, output: serde_json::Value) -> String {
    let call = serde_json::json!({
        "timestamp": "2026-10-05T01:00:00Z",
        "type": "response_item",
        "payload": {
            "type": "function_call",
            "name": name,
            "call_id": "selected-call",
            "arguments": arguments.to_string()
        }
    });
    let result = serde_json::json!({
        "timestamp": "2026-10-05T01:00:02Z",
        "type": "response_item",
        "payload": {
            "type": "function_call_output",
            "call_id": "selected-call",
            "output": output.to_string()
        }
    });
    format!("{call}\n{result}")
}

#[test]
fn wait_call_output_pair_uses_exact_id_and_discards_body() {
    let input = rollout_pair(
        "wait_agent",
        serde_json::json!({
            "targets": ["agent/path"],
            "return_when": "target_terminal",
            "timeout_ms": 30000
        }),
        serde_json::json!({
            "timed_out": false,
            "reason": "target_terminal",
            "wake_cause": "target_status",
            "queued_update_count": 3,
            "target_status": {"agent/path": "running"},
            "completed": {"privateBody": "ignored"}
        }),
    );
    let (events, coverage) = envelopes::parse(input.as_bytes(), "explicit-session");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].call_id, "selected-call");
    assert_eq!(
        events[0].wait_request.as_ref().unwrap().targets,
        vec!["agent/path".to_owned()]
    );
    assert_eq!(events[0].host_target_terminal, Some(true));
    assert_eq!(events[0].host_target_status_wake, Some(true));
    assert_eq!(events[0].observed_host_return.as_ref().unwrap().target_statuses.len(), 1);
    assert!(events[0].observed_host_return.as_ref().unwrap().complete);
    assert_eq!(coverage.malformed_records, 0);
    assert_eq!(coverage.unmatched_calls, 0);
}

#[test]
fn recognized_external_wait_flows_through_recorder_shared_reducer_and_cli_projection() {
    let input = rollout_pair(
        "wait_agent",
        serde_json::json!({"targets": ["agent/path"], "return_when": "target_terminal"}),
        serde_json::json!({"reason": "target_terminal", "target_status": {"agent/path": "running"}}),
    );
    let (observations, parser_coverage) = envelopes::parse(input.as_bytes(), "selected-host-session");
    assert_eq!(observations.len(), 1);
    assert_eq!(parser_coverage.unmatched_calls, 0);
    let recorder = codex_diagnostics::control_plane::ControlPlaneRecorder::new(
        codex_diagnostics::control_plane::CaptureMode::Session, "selected-capture").unwrap();
    let mut event_projections = Vec::new();
    for observation in observations {
        event_projections.push(super::project_envelope(&observation));
        assert!(recorder.record(super::to_event(observation).unwrap()).is_some());
    }
    let snapshot = recorder.snapshot().unwrap();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].identity.capture_instance_id, "selected-capture");
    assert_eq!(snapshot.events[0].identity.sequence, 1);
    let reduced = codex_diagnostics::control_plane::Summary::reduce_snapshot(&snapshot);
    assert_eq!(reduced.wait_timelines.len(), 1);
    assert_eq!(reduced.wait_timelines[0].return_when,
        codex_diagnostics::control_plane::ReturnWhen::Unknown);
    assert_eq!(reduced.wait_timelines[0].operation_id, None);
    let rendered = summary::project(&reduced);
    let wait = &rendered["waitTimelines"][0];
    assert_eq!(wait["returnWhen"], "Unknown");
    assert_eq!(wait["requestedTargetReferences"][0], "agent/path");
    assert_eq!(wait["observedHostReturn"]["targetStatuses"][0]["statusTag"], "Running");
    let command = super::ControlPlaneCommand {
        input: std::path::PathBuf::new(),
        start_byte: 0,
        end_byte: 1,
        source_namespace: "selected-host-session".to_owned(),
        source_plane: super::SourcePlaneSelector::ExternalHostEnvelope,
        root_thread_id: None,
        usage_request: None,
    };
    let rendered = super::render(
        Some(&reduced), &parser_coverage, &super::Coverage::default(), Some(&snapshot),
        &command, event_projections, serde_json::json!({"requested": false}),
    );
    assert_eq!(rendered["schemaVersion"], 1);
    assert_eq!(rendered["coverage"]["unmatchedCalls"], 0);
    assert_eq!(rendered["recorder"]["captureInstanceId"], "selected-capture");
    assert_eq!(rendered["events"].as_array().unwrap().len(), 1);
    assert!(rendered["data"]["waitTimelines"][0]["observedHostReturn"].is_object());
}

fn timed_wait_pair(call_id: &str, started_at: &str, returned_at: &str, reason: &str) -> String {
    timed_wait_pair_with_metadata(call_id, started_at, returned_at, Some(true), Some(reason), Some("timeout"), true)
}

fn timed_wait_pair_with_metadata(
    call_id: &str,
    started_at: &str,
    returned_at: &str,
    timed_out: Option<bool>,
    reason: Option<&str>,
    wake_cause: Option<&str>,
    include_status: bool,
) -> String {
    let request = serde_json::json!({
        "timestamp": started_at,
        "type": "response_item",
        "payload": {
            "type": "function_call",
            "name": "wait_agent",
            "call_id": call_id,
            "arguments": serde_json::json!({
                "targets": ["agent/path"],
                "return_when": "any",
                "timeout_ms": 1_800_000
            }).to_string()
        }
    });
    let mut metadata = serde_json::json!({"queued_update_count": 2});
    if let Some(timed_out) = timed_out { metadata["timed_out"] = timed_out.into(); }
    if let Some(reason) = reason { metadata["reason"] = reason.into(); }
    if let Some(wake_cause) = wake_cause { metadata["wake_cause"] = wake_cause.into(); }
    if include_status { metadata["status"] = serde_json::json!({"agent/path": "running"}); }
    let output = serde_json::json!({
        "timestamp": returned_at,
        "type": "response_item",
        "payload": {
            "type": "function_call_output",
            "call_id": call_id,
            "output": metadata.to_string()
        }
    });
    format!("{request}\n{output}")
}

fn expected_timeout_wait(call_id: &str, sequence: usize, returned_at: &str) -> serde_json::Value {
    let returned = chrono::DateTime::parse_from_rfc3339(returned_at).unwrap();
    serde_json::json!({
        "sourceEvents": [expected_identity(sequence)],
        "captureInstanceId": "external-host-envelope",
        "sourcePlane": "ExternalHostEnvelope",
        "threadId": null,
        "turnId": null,
        "rootThreadId": null,
        "parentThreadId": null,
        "forkParentThreadId": null,
        "windowId": null,
        "windowNumber": null,
        "previousWindowId": null,
        "wallCorrelation": {"secondsSinceEpoch": returned.timestamp(), "subsecondNanos": returned.timestamp_subsec_nanos()},
        "operationId": call_id,
        "waitId": call_id,
        "phase": "Completed",
        "primitive": "Unknown",
        "helperId": "wait_agent",
        "helperVersion": null,
        "requestedTimeoutMs": 1_800_000,
        "effectiveTimeoutMs": null,
        "returnWhen": "Any",
        "targetMode": "Targeted",
        "anyTargets": null,
        "requestedTargetReferences": ["agent/path"],
        "requestedTargetKind": "ExposedAgentPath",
        "requestedTargetSetComplete": true,
        "resolvedTargetIds": [],
        "resolvedTargetKind": "Unknown",
        "resolvedTargetSetComplete": false,
        "blockedStartOffsetNs": null,
        "blockedEndOffsetNs": null,
        "operationDurationNs": 1_800_000_000_000u64,
        "blockedDurationNs": null,
        "selectedOutcome": "Unknown",
        "selectedProducer": null,
        "selectedTargetId": null,
        "selectedTargetTurnId": null,
        "continuationOfWaitId": null,
        "subscribedReadiness": [],
        "subscribedReadinessComplete": false,
        "selectedReadiness": [],
        "selectedReadinessComplete": false,
        "observedHostReturn": {
            "timedOut": true,
            "reason": "Timeout",
            "wakeCause": "Timeout",
            "queuedUpdateCount": 2,
            "targetStatuses": [{
                "targetReference": {"id": "agent/path", "kind": "ExposedAgentPath"},
                "statusTag": "Running"
            }],
            "targetStatusComplete": true,
            "complete": true
        },
        "coverage": [
            {"field": "ThreadId", "unknown": "NotExposed"},
            {"field": "RootThreadId", "unknown": "NotExposed"},
            {"field": "TurnId", "unknown": "NotExposed"},
            {"field": "EffectiveTimeout", "unknown": "NotExposed"},
            {"field": "SelectedProducer", "unknown": "UnsupportedProducer"},
            {"field": "QueueState", "unknown": "NotExposed"},
            {"field": "SemanticAcknowledgement", "unknown": "NotExposed"},
            {"field": "ProviderObservedIdentity", "unknown": "NotExposed"}
        ],
        "complete": false,
        "conflicting": false
    })
}

fn expected_timeout_event(call_id: &str, started_at: &str, returned_at: &str) -> serde_json::Value {
    let start = chrono::DateTime::parse_from_rfc3339(started_at).unwrap();
    let end = chrono::DateTime::parse_from_rfc3339(returned_at).unwrap();
    serde_json::json!({
        "operation": "wait_agent",
        "callId": call_id,
        "requestObservedAt": start.to_rfc3339(),
        "returnObservedAt": end.to_rfc3339(),
        "observedRequestReturnDurationNs": 1_800_000_000_000u64,
        "wait": {
            "targetMode": "targeted",
            "requestedTargetReferences": ["agent/path"],
            "requestedTargetKind": "exposedAgentPath",
            "requestedTargetSetComplete": true,
            "requestedTimeoutMs": 1_800_000,
            "returnWhen": "any",
            "resolvedTargetIds": [],
            "resolvedTargetSet": "unknown"
        },
        "hostReturn": {
            "timedOut": true,
            "timeoutReasonObserved": true,
            "targetTerminalReasonObserved": false,
            "targetStatusWakeCauseObserved": false,
            "queuedUpdateCount": 2,
            "targetStatusPresent": true,
            "winner": "unknown",
            "blockedDuration": "unknown",
            "enqueuePendingAcknowledgement": "unknown"
        },
        "statusQuery": null
    })
}

fn expected_timeout_export(length: usize, rows: &[(&str, &str, &str)]) -> serde_json::Value {
    let mut expected = expected_empty_export(length);
    expected["recorder"]["highWater"] = (rows.len() as u64).into();
    expected["data"]["eventCount"] = (rows.len() as u64).into();
    expected["data"]["unknownRootEvents"] = (rows.len() as u64).into();
    expected["coverage"]["parsedEvents"] = (rows.len() as u64).into();
    expected["data"]["waitsByOutcome"] = serde_json::json!({"Unknown": rows.len()});
    expected["data"]["repeatedWaitGroups"] = usize::from(rows.len() > 1).into();
    expected["data"]["waitTimelines"] = rows.iter().enumerate()
        .map(|(index, (call, _, returned))| expected_timeout_wait(call, index + 1, returned))
        .collect::<Vec<_>>().into();
    expected["data"]["queueTimelines"] = rows.iter().enumerate()
        .map(|(index, (call, _, _))| expected_external_rows(call, index + 1).0)
        .collect::<Vec<_>>().into();
    expected["data"]["externalDurations"] = rows.iter().enumerate()
        .map(|(index, (call, _, _))| {
            let mut duration = expected_external_rows(call, index + 1).1;
            duration["observedRequestReturnNs"] = 1_800_000_000_000u64.into();
            duration
        })
        .collect::<Vec<_>>().into();
    expected["events"] = rows.iter()
        .map(|(call, started, returned)| expected_timeout_event(call, started, returned))
        .collect::<Vec<_>>().into();
    expected
}

fn expect_unsupported_timeout_metadata(expected: &mut serde_json::Value) {
    expected["data"]["waitTimelines"][0]["observedHostReturn"]["reason"] = "Other".into();
    expected["data"]["waitTimelines"][0]["observedHostReturn"]["wakeCause"] = "Other".into();
    expected["data"]["waitTimelines"][0]["observedHostReturn"]["complete"] = false.into();
    expected["events"][0]["hostReturn"]["timeoutReasonObserved"] = false.into();
    expected["events"][0]["hostReturn"]["targetStatusWakeCauseObserved"] = false.into();
}

fn expect_missing_timeout_status(expected: &mut serde_json::Value) {
    expected["data"]["waitTimelines"][0]["observedHostReturn"]["targetStatuses"] =
        serde_json::json!([]);
    expected["data"]["waitTimelines"][0]["observedHostReturn"]["complete"] = false.into();
    expected["events"][0]["hostReturn"]["targetStatusPresent"] = false.into();
}

fn expect_false_timeout_inconsistency(expected: &mut serde_json::Value) {
    expected["data"]["waitTimelines"][0]["observedHostReturn"]["timedOut"] = false.into();
    expected["data"]["waitTimelines"][0]["observedHostReturn"]["complete"] = false.into();
    expected["events"][0]["hostReturn"]["timedOut"] = false.into();
}

#[tokio::test]
async fn actual_timeout_envelopes_export_full_observed_metadata_and_repeat_without_native_causality() {
    let old_alias = timed_wait_pair(
        "selected-call", "2026-10-05T01:00:00Z", "2026-10-05T01:30:00Z", "timeout");
    let timed_out_alias = timed_wait_pair(
        "selected-call", "2026-10-05T01:00:00Z", "2026-10-05T01:30:00Z", "timed_out");
    for input in [&old_alias, &timed_out_alias] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("selected.jsonl");
        std::fs::write(&path, input).unwrap();
        let actual = super::execute(command_for(&path, input.len(), None)).await.unwrap();
        let expected = expected_timeout_export(input.len(), &[(
            "selected-call", "2026-10-05T01:00:00Z", "2026-10-05T01:30:00Z")]);
        assert_eq!(actual, expected);
        assert_eq!(actual["data"]["repeatedWaitGroups"], 0);
        assert_eq!(actual["data"]["waitTimelines"][0]["complete"], false);
        assert_eq!(actual["data"]["waitTimelines"][0]["selectedOutcome"], "Unknown");
        assert_eq!(actual["data"]["waitTimelines"][0]["selectedProducer"], serde_json::Value::Null);
        assert_eq!(actual["data"]["waitTimelines"][0]["blockedDurationNs"], serde_json::Value::Null);
    }

    let first = timed_wait_pair(
        "a-call", "2026-10-05T01:00:00Z", "2026-10-05T01:30:00Z", "timed_out");
    let second = timed_wait_pair(
        "b-call", "2026-10-05T02:00:00Z", "2026-10-05T02:30:00Z", "timed_out");
    let input = format!("{first}\n{second}");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selected.jsonl");
    std::fs::write(&path, &input).unwrap();
    let actual = super::execute(command_for(&path, input.len(), None)).await.unwrap();
    let expected = expected_timeout_export(input.len(), &[
        ("a-call", "2026-10-05T01:00:00Z", "2026-10-05T01:30:00Z"),
        ("b-call", "2026-10-05T02:00:00Z", "2026-10-05T02:30:00Z"),
    ]);
    assert_eq!(actual, expected);
    assert_eq!(actual["data"]["repeatedWaitGroups"], 1);
    for row in actual["data"]["waitTimelines"].as_array().unwrap() {
        assert_eq!(row["observedHostReturn"]["complete"], true);
        assert_eq!(row["observedHostReturn"]["reason"], "Timeout");
        assert_eq!(row["observedHostReturn"]["wakeCause"], "Timeout");
        assert_eq!(row["complete"], false);
        assert_eq!(row["selectedOutcome"], "Unknown");
        assert_eq!(row["selectedProducer"], serde_json::Value::Null);
        assert_eq!(row["blockedDurationNs"], serde_json::Value::Null);
    }
    assert!(!actual.to_string().contains("prohibited"));
    assert!(!actual.to_string().contains("redundant"));
}

#[tokio::test]
async fn actual_timeout_controls_preserve_unsupported_missing_and_contradictory_reports() {
    let started_at = "2026-10-05T01:00:00Z";
    let returned_at = "2026-10-05T01:30:00Z";
    type ExpectedAdjustment = fn(&mut serde_json::Value);
    let cases = [
        (
            timed_wait_pair_with_metadata("selected-call", started_at, returned_at,
                Some(true), Some("unsupported"), Some("unsupported"), true),
            expect_unsupported_timeout_metadata as ExpectedAdjustment,
        ),
        (
            timed_wait_pair_with_metadata("selected-call", started_at, returned_at,
                Some(true), Some("timed_out"), Some("timeout"), false),
            expect_missing_timeout_status as ExpectedAdjustment,
        ),
        (
            timed_wait_pair_with_metadata("selected-call", started_at, returned_at,
                Some(false), Some("timed_out"), Some("timeout"), true),
            expect_false_timeout_inconsistency as ExpectedAdjustment,
        ),
    ];
    for (input, adjust_expected) in cases {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("selected.jsonl");
        std::fs::write(&path, &input).unwrap();
        let actual = super::execute(command_for(&path, input.len(), None)).await.unwrap();
        let mut expected = expected_timeout_export(input.len(), &[("selected-call", started_at, returned_at)]);
        adjust_expected(&mut expected);
        assert_eq!(actual, expected);
        assert_eq!(actual["data"]["repeatedWaitGroups"], 0);
        assert_eq!(actual["data"]["waitTimelines"][0]["complete"], false);
        assert_eq!(actual["data"]["waitTimelines"][0]["selectedOutcome"], "Unknown");
        assert_eq!(actual["data"]["waitTimelines"][0]["selectedProducer"], serde_json::Value::Null);
        assert_eq!(actual["data"]["waitTimelines"][0]["blockedDurationNs"], serde_json::Value::Null);
    }
}

#[test]
fn identical_replay_is_deduplicated_conflict_is_quarantined_and_open_call_counted() {
    let pair = rollout_pair(
        "wait_agent",
        serde_json::json!({"targets": ["agent/path"], "return_when": "any"}),
        serde_json::json!({"reason": "target_terminal", "target_status": {"agent/path": "running"}}),
    );
    let mut lines = pair.lines().map(str::to_owned).collect::<Vec<_>>();
    lines.push(lines[0].clone());
    lines.push(lines[1].replace("target_terminal", "timeout"));
    let (events, coverage) = envelopes::parse(lines.join("\n").as_bytes(), "explicit-session");
    assert!(events.is_empty());
    assert_eq!(coverage.duplicate_records, 2);
    assert_eq!(coverage.conflicting_pairs, 1);
    assert_eq!(coverage.unmatched_calls, 0);

    let open_call = serde_json::json!({
        "timestamp": "2026-10-05T01:00:00Z", "type": "response_item",
        "payload": {"type": "function_call", "name": "wait_agent", "call_id": "open-call",
            "arguments": "{\"targets\":[\"agent/path\"],\"return_when\":\"all\"}"}
    });
    let (events, coverage) = envelopes::parse(open_call.to_string().as_bytes(), "explicit-session");
    assert!(events.is_empty());
    assert_eq!(coverage.unmatched_calls, 1);
}

#[test]
fn list_agents_keeps_only_bounded_typed_actor_metadata() {
    let input = rollout_pair(
        "list_agents",
        serde_json::json!({"path_prefix": "/agents"}),
        serde_json::json!({"agents": [
            {"agent_id": "actor-a", "canonical_path": "/agents/a", "configured_model": "gpt-luna", "configured_reasoning_effort": "medium", "agent_status": "running"},
            {"agent_id": "actor-b", "canonical_path": "/agents/b", "agent_status": {"completed": {"privateBody": "ignored"}}}
        ]}),
    );
    let (events, coverage) = envelopes::parse(input.as_bytes(), "explicit-session");
    let result = events[0].status_result.as_ref().unwrap();
    assert!(result.complete);
    assert_eq!(result.actor_count, 2);
    assert_eq!(result.actors[0].status, envelopes::ExternalStatusTag::Running);
    assert_eq!(result.actors[1].status, envelopes::ExternalStatusTag::Completed);
    assert_eq!(coverage.malformed_records, 0);
}

#[test]
fn unsupported_host_status_tag_is_incomplete_and_cannot_claim_exact_result() {
    let input = rollout_pair(
        "list_agents",
        serde_json::json!({"path_prefix": "/agents"}),
        serde_json::json!({"agents": [
            {"agent_id": "actor-a", "canonical_path": "/agents/a", "agent_status": "paused"}
        ]}),
    );
    let (observations, coverage) = envelopes::parse(input.as_bytes(), "selected-host-session");
    assert_eq!(coverage.incomplete_status_results, 1);
    let result = observations[0].status_result.as_ref().unwrap();
    assert!(!result.complete);
    assert_eq!(result.actor_count, 1);
    assert!(result.actors.is_empty());
    let event = super::to_event(observations.into_iter().next().unwrap()).unwrap();
    let recorder = codex_diagnostics::control_plane::ControlPlaneRecorder::new(
        codex_diagnostics::control_plane::CaptureMode::Session, "selected-capture").unwrap();
    recorder.record(event).unwrap();
    assert_eq!(codex_diagnostics::control_plane::Summary::reduce_snapshot(
        &recorder.snapshot().unwrap()).repeated_status_query_groups, 0);
}

#[test]
fn malformed_and_oversized_records_have_content_free_coverage() {
    let mut input = vec![b'x'; 65_537];
    input.push(b'\n');
    input.extend_from_slice(b"{\"private\":\"not echoed\"}");
    let (events, coverage) = envelopes::parse(&input, "explicit-session");
    assert!(events.is_empty());
    assert_eq!(coverage.oversized_records, 1);
    assert_eq!(coverage.malformed_records, 1);
}

#[test]
fn common_summary_serialization_includes_all_timeline_planes() {
    let projected = summary::project(&codex_diagnostics::control_plane::Summary::default());
    let object = projected.as_object().unwrap();
    for field in [
        "waitTimelines",
        "sleepTimelines",
        "schedulerTimelines",
        "messageTimelines",
        "boundaryTimelines",
        "providerCallTimelines",
        "queueTimelines",
        "externalDurations",
        "repeatedWaitGroups",
        "repeatedStatusQueryGroups",
        "partitionsConserve",
        "invalidEventIdentityCount",
    ] {
        assert!(object.contains_key(field), "missing summary field {field}");
    }
}

pub(super) fn command_for(input: &std::path::Path, length: usize, usage: Option<&std::path::Path>) -> super::ControlPlaneCommand {
    let mut args = vec!["codex".to_owned(), "debug".to_owned(), "control-plane".to_owned(),
        "--input".to_owned(), input.to_str().unwrap().to_owned(), "--start-byte".to_owned(), "0".to_owned(),
        "--end-byte".to_owned(), length.to_string(), "--source-namespace".to_owned(), "fixture-source".to_owned(),
        "--source-plane".to_owned(), "external-host-envelope".to_owned()];
    if let Some(path) = usage { args.extend(["--usage-request".to_owned(), path.to_str().unwrap().to_owned()]); }
    let cli = crate::MultitoolCli::try_parse_from(args).unwrap();
    let Some(crate::Subcommand::Debug(crate::DebugCommand {
        subcommand: crate::DebugSubcommand::ControlPlane(command),
    })) = cli.subcommand else { panic!("actual debug dispatch did not select control-plane") };
    command
}

// Independent oracle for the complete exported object, not a render/reduce call.
pub(super) fn expected_empty_export(length: usize) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1, "sourcePlane": "externalHostEnvelope",
        "selectedRange": {"startByte": 0, "endByteExclusive": length},
        "recorder": {"captureInstanceId": "external-host-envelope", "highWater": 0,
            "losses": {"contention": 0, "capacity": 0, "disabled": 0, "invalidIdentity": 0}},
        "data": {
            "eventCount": 0, "duplicateCount": 0, "conflictingIdentityCount": 0,
            "invalidEventIdentityCount": 0, "recorderLosses": {"contention": 0, "capacity": 0, "disabled": 0, "invalidIdentity": 0},
            "conflictingWaitCount": 0, "waitsByOutcome": {}, "waitTimelines": [], "sleepTimelines": [],
            "schedulerTimelines": [], "messageTimelines": [], "boundaryTimelines": [], "providerCallTimelines": [],
            "queueTimelines": [], "externalDurations": [], "repeatedWaitGroups": 0, "repeatedStatusQueryGroups": 0,
            "unknownRootEvents": 0, "truncatedEvents": 0, "omittedInputEvents": 0,
            "captureLossObserved": false, "partitionsConserve": true
        },
        "coverage": {
            "malformedRecords": 0, "oversizedRecords": 0, "unmatchedOutputs": 0, "unmatchedCalls": 0,
            "duplicateRecords": 0, "conflictingPairs": 0, "unsupportedRecords": 0, "incompleteStatusResults": 0,
            "projectionOmittedByBound": 0, "rootFilterExcluded": 0, "unknownRoot": 0,
            "parsedEvents": 0, "snapshotUnavailable": false, "externalWinner": "notExposed",
            "enqueuePendingAckChain": "notExposed", "agentIncarnation": "notExposed",
            "providerEffectiveIdentity": "unverified", "usageCoverage": "notRequested"
        },
        "events": [], "usage": {"requested": false, "status": "notRequested", "join": null}
    })
}

fn expected_identity(sequence: usize) -> serde_json::Value {
    serde_json::json!({"captureInstanceId": "external-host-envelope", "sequence": sequence})
}

fn expected_external_rows(call: &str, sequence: usize) -> (serde_json::Value, serde_json::Value) {
    let identity = expected_identity(sequence);
    (serde_json::json!({"identity": identity, "captureInstanceId": "external-host-envelope",
        "sourcePlane": "ExternalHostEnvelope", "threadId": null, "turnId": null, "scope": "Unknown",
        "transitionSequence": null, "queueBefore": null, "queueAfter": null}),
        serde_json::json!({"identity": identity, "sourcePlane": "ExternalHostEnvelope", "threadId": null,
            "operationId": call, "callId": call, "observedRequestReturnNs": 2_000_000_000u64,
            "quality": "ObservedOnly", "conflicting": false}))
}

#[tokio::test]
async fn actual_debug_dispatch_exports_whole_observed_wait_object_and_conflict_controls() {
    let pair = rollout_pair("wait_agent",
        serde_json::json!({"targets": ["agent/path"], "return_when": "any", "timeout_ms": 30000}),
        serde_json::json!({"timed_out": false, "reason": "target_terminal", "wake_cause": "target_status",
            "queued_update_count": 3, "target_status": {"agent/path": {"completed": "BODY_CANARY"}}}));
    let input = format!("{pair}\n{}", pair.replace("selected-call", "z-second-call"));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selected.jsonl");
    std::fs::write(&path, &input).unwrap();
    let actual = super::execute(command_for(&path, input.len(), /*usage*/ None)).await.unwrap();
    let mut expected = expected_empty_export(input.len());
    expected["recorder"]["highWater"] = 2.into();
    expected["data"]["eventCount"] = 2.into(); expected["data"]["unknownRootEvents"] = 2.into();
    expected["data"]["repeatedWaitGroups"] = 1.into(); expected["coverage"]["parsedEvents"] = 2.into();
    expected["data"]["waitsByOutcome"] = serde_json::json!({"Unknown": 2});
    let mut waits = Vec::new(); let mut events = Vec::new(); let mut queues = Vec::new(); let mut durations = Vec::new();
    for (index, call) in ["selected-call", "z-second-call"].into_iter().enumerate() {
        let sequence = index + 1;
        let wall_seconds = chrono::DateTime::parse_from_rfc3339("2026-10-05T01:00:02Z").unwrap().timestamp();
        waits.push(serde_json::json!({
            "sourceEvents": [expected_identity(sequence)], "captureInstanceId": "external-host-envelope",
            "sourcePlane": "ExternalHostEnvelope", "threadId": null, "turnId": null, "rootThreadId": null,
            "parentThreadId": null, "forkParentThreadId": null, "windowId": null, "windowNumber": null,
            "previousWindowId": null, "wallCorrelation": {"secondsSinceEpoch": wall_seconds, "subsecondNanos": 0},
            "operationId": call, "waitId": call, "phase": "Completed", "primitive": "Unknown",
            "helperId": "wait_agent", "helperVersion": null, "requestedTimeoutMs": 30000, "effectiveTimeoutMs": null,
            "returnWhen": "Any", "targetMode": "Targeted", "anyTargets": null,
            "requestedTargetReferences": ["agent/path"], "requestedTargetKind": "ExposedAgentPath",
            "requestedTargetSetComplete": true, "resolvedTargetIds": [], "resolvedTargetKind": "Unknown",
            "resolvedTargetSetComplete": false, "blockedStartOffsetNs": null, "blockedEndOffsetNs": null,
            "operationDurationNs": 2_000_000_000u64, "blockedDurationNs": null, "selectedOutcome": "Unknown",
            "selectedProducer": null, "selectedTargetId": null, "selectedTargetTurnId": null, "continuationOfWaitId": null,
            "subscribedReadiness": [], "subscribedReadinessComplete": false,
            "selectedReadiness": [], "selectedReadinessComplete": false,
            "observedHostReturn": {"timedOut": false, "reason": "TargetTerminal", "wakeCause": "TargetStatus",
                "queuedUpdateCount": 3, "targetStatuses": [{"targetReference": {"id": "agent/path", "kind": "ExposedAgentPath"}, "statusTag": "Completed"}],
                "targetStatusComplete": true, "complete": true},
            "coverage": [
                {"field": "ThreadId", "unknown": "NotExposed"}, {"field": "RootThreadId", "unknown": "NotExposed"},
                {"field": "TurnId", "unknown": "NotExposed"}, {"field": "EffectiveTimeout", "unknown": "NotExposed"},
                {"field": "SelectedProducer", "unknown": "UnsupportedProducer"}, {"field": "QueueState", "unknown": "NotExposed"},
                {"field": "SemanticAcknowledgement", "unknown": "NotExposed"}, {"field": "ProviderObservedIdentity", "unknown": "NotExposed"}
            ], "complete": false, "conflicting": false
        }));
        events.push(serde_json::json!({"operation": "wait_agent", "callId": call,
            "requestObservedAt": "2026-10-05T01:00:00+00:00", "returnObservedAt": "2026-10-05T01:00:02+00:00",
            "observedRequestReturnDurationNs": 2_000_000_000u64,
            "wait": {"targetMode": "targeted", "requestedTargetReferences": ["agent/path"], "requestedTargetKind": "exposedAgentPath",
                "requestedTargetSetComplete": true, "requestedTimeoutMs": 30000, "returnWhen": "any", "resolvedTargetIds": [], "resolvedTargetSet": "unknown"},
            "hostReturn": {"timedOut": false, "timeoutReasonObserved": false, "targetTerminalReasonObserved": true,
                "targetStatusWakeCauseObserved": true, "queuedUpdateCount": 3, "targetStatusPresent": true,
                "winner": "unknown", "blockedDuration": "unknown", "enqueuePendingAcknowledgement": "unknown"}, "statusQuery": null}));
        let (queue, duration) = expected_external_rows(call, sequence); queues.push(queue); durations.push(duration);
    }
    expected["data"]["waitTimelines"] = waits.into(); expected["data"]["queueTimelines"] = queues.into();
    expected["data"]["externalDurations"] = durations.into(); expected["events"] = events.into();
    assert_eq!(actual, expected);
    assert!(!actual.to_string().contains("BODY_CANARY"));

    let conflict = format!("{pair}\n{}", pair.lines().nth(1).unwrap().replace("target_terminal", "timeout"));
    std::fs::write(&path, &conflict).unwrap();
    let actual = super::execute(command_for(&path, conflict.len(), /*usage*/ None)).await.unwrap();
    let mut expected = expected_empty_export(conflict.len());
    expected["coverage"]["duplicateRecords"] = 1.into(); expected["coverage"]["conflictingPairs"] = 1.into();
    assert_eq!(actual, expected);

    let wrong_id = pair.lines().enumerate().map(|(index, line)| if index == 1 {
        line.replace("selected-call", "wrong-call")
    } else { line.to_owned() }).collect::<Vec<_>>().join("\n");
    std::fs::write(&path, &wrong_id).unwrap();
    let actual = super::execute(command_for(&path, wrong_id.len(), /*usage*/ None)).await.unwrap();
    let mut expected = expected_empty_export(wrong_id.len());
    expected["coverage"]["unmatchedCalls"] = 1.into(); expected["coverage"]["unmatchedOutputs"] = 1.into();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn completed_label_and_payload_follow_actual_parser_recorder_reducer_render_contract() {
    let input = rollout_pair("list_agents", serde_json::json!({"path_prefix": "agent"}),
        serde_json::json!({"agents": [{"agent_id": "actor", "canonical_path": "agent/path", "agent_status": "completed"}],
            "completed": "BODY_CANARY"}));
    let directory = tempfile::tempdir().unwrap(); let path = directory.path().join("selected.jsonl");
    std::fs::write(&path, &input).unwrap();
    let actual = super::execute(command_for(&path, input.len(), /*usage*/ None)).await.unwrap();
    let expected = expected_status_export(input.len());
    assert_eq!(actual, expected);
    assert!(!actual.to_string().contains("BODY_CANARY"));

    let (observations, parser) = envelopes::parse(input.as_bytes(), "fixture-source");
    let recorder = codex_diagnostics::control_plane::ControlPlaneRecorder::new(
        codex_diagnostics::control_plane::CaptureMode::Session, "external-host-envelope").unwrap();
    let projections = observations.iter().map(super::project_envelope).collect();
    for observation in observations { recorder.record(super::to_event(observation).unwrap()).unwrap(); }
    let mut snapshot = recorder.snapshot().unwrap(); snapshot.losses.contention = 1;
    let reduced = codex_diagnostics::control_plane::Summary::reduce_snapshot(&snapshot);
    let actual = super::render(Some(&reduced), &parser, &super::Coverage::default(), Some(&snapshot),
        &command_for(&path, input.len(), /*usage*/ None), projections, serde_json::json!({"requested": false, "status": "notRequested", "join": null}));
    let mut expected = expected_status_export(input.len());
    expected["recorder"]["losses"]["contention"] = 1.into();
    expected["data"]["recorderLosses"]["contention"] = 1.into();
    expected["data"]["captureLossObserved"] = true.into();
    assert_eq!(actual, expected);
}

pub(super) fn expected_status_export(length: usize) -> serde_json::Value {
    let mut expected = expected_empty_export(length);
    expected["recorder"]["highWater"] = 1.into(); expected["data"]["eventCount"] = 1.into();
    expected["data"]["unknownRootEvents"] = 1.into(); expected["coverage"]["parsedEvents"] = 1.into();
    let (queue, duration) = expected_external_rows("selected-call", /*sequence*/ 1);
    expected["data"]["queueTimelines"] = serde_json::json!([queue]);
    expected["data"]["externalDurations"] = serde_json::json!([duration]);
    expected["events"] = serde_json::json!([{"operation": "list_agents", "callId": "selected-call",
        "requestObservedAt": "2026-10-05T01:00:00+00:00", "returnObservedAt": "2026-10-05T01:00:02+00:00",
        "observedRequestReturnDurationNs": 2_000_000_000u64, "wait": null,
        "hostReturn": {"timedOut": null, "timeoutReasonObserved": null, "targetTerminalReasonObserved": null,
            "targetStatusWakeCauseObserved": null, "queuedUpdateCount": null, "targetStatusPresent": false,
            "winner": "unknown", "blockedDuration": "unknown", "enqueuePendingAcknowledgement": "unknown"},
        "statusQuery": {"pathPrefix": "agent", "observedActorCount": 1, "complete": true,
            "actors": [{"actorId": "actor", "canonicalPath": "agent/path", "configuredModel": null,
                "configuredReasoningEffort": null, "statusTag": "completed"}], "readiness": "unknown", "resolvedThreadOrIncarnation": "unknown"}}]);
    expected
}

#[test]
fn partial_contradictory_or_uncovered_host_returns_never_become_complete_native_results_or_repetition() {
    use codex_diagnostics::control_plane::{CaptureMode, ControlPlaneRecorder, HostTargetStatusObservation,
        ObservedHostStatusTag, ObservedHostWaitReason, ObservedHostWakeCause, ObservedHostWaitReturn,
        SelectedOutcome, Summary, TargetReference, TargetReferenceKind};
    let cases = [
        (serde_json::json!({"queued_update_count": 1}), ObservedHostWaitReturn {
            timed_out: None, reason: ObservedHostWaitReason::Unknown, wake_cause: ObservedHostWakeCause::Unknown,
            queued_update_count: Some(1), target_statuses: vec![], target_status_complete: true, complete: false }),
        (serde_json::json!({"timed_out": true, "reason": "target_terminal", "wake_cause": "target_status",
            "target_status": {"agent/path": "running"}}), ObservedHostWaitReturn {
            timed_out: Some(true), reason: ObservedHostWaitReason::TargetTerminal, wake_cause: ObservedHostWakeCause::TargetStatus,
            queued_update_count: None, target_statuses: vec![HostTargetStatusObservation {
                target_reference: TargetReference { id: "agent/path".into(), kind: TargetReferenceKind::ExposedAgentPath },
                status_tag: ObservedHostStatusTag::Running }], target_status_complete: true, complete: false }),
        (serde_json::json!({"timed_out": false, "reason": "target_terminal", "wake_cause": "target_status",
            "target_status": {"uncovered/path": "completed"}}), ObservedHostWaitReturn {
            timed_out: Some(false), reason: ObservedHostWaitReason::TargetTerminal, wake_cause: ObservedHostWakeCause::TargetStatus,
            queued_update_count: None, target_statuses: vec![HostTargetStatusObservation {
                target_reference: TargetReference { id: "uncovered/path".into(), kind: TargetReferenceKind::ExposedAgentPath },
                status_tag: ObservedHostStatusTag::Completed }], target_status_complete: true, complete: false }),
        (serde_json::json!({"timed_out": true, "reason": "not-a-supported-reason", "wake_cause": "not-a-supported-wake",
            "status": {"agent/path": "running"}}), ObservedHostWaitReturn {
            timed_out: Some(true), reason: ObservedHostWaitReason::Other, wake_cause: ObservedHostWakeCause::Other,
            queued_update_count: None, target_statuses: vec![HostTargetStatusObservation {
                target_reference: TargetReference { id: "agent/path".into(), kind: TargetReferenceKind::ExposedAgentPath },
                status_tag: ObservedHostStatusTag::Running }], target_status_complete: true, complete: false }),
        (serde_json::json!({"timed_out": true, "reason": "timed_out", "wake_cause": "timeout"}), ObservedHostWaitReturn {
            timed_out: Some(true), reason: ObservedHostWaitReason::Timeout, wake_cause: ObservedHostWakeCause::Timeout,
            queued_update_count: None, target_statuses: vec![], target_status_complete: true, complete: false }),
        (serde_json::json!({"timed_out": false, "reason": "timed_out", "wake_cause": "timeout",
            "status": {"agent/path": "running"}}), ObservedHostWaitReturn {
            timed_out: Some(false), reason: ObservedHostWaitReason::Timeout, wake_cause: ObservedHostWakeCause::Timeout,
            queued_update_count: None, target_statuses: vec![HostTargetStatusObservation {
                target_reference: TargetReference { id: "agent/path".into(), kind: TargetReferenceKind::ExposedAgentPath },
                status_tag: ObservedHostStatusTag::Running }], target_status_complete: true, complete: false }),
        (serde_json::json!({"timed_out": false, "reason": "timeout", "wake_cause": "timeout",
            "status": {"agent/path": "running"}}), ObservedHostWaitReturn {
            timed_out: Some(false), reason: ObservedHostWaitReason::Timeout, wake_cause: ObservedHostWakeCause::Timeout,
            queued_update_count: None, target_statuses: vec![HostTargetStatusObservation {
                target_reference: TargetReference { id: "agent/path".into(), kind: TargetReferenceKind::ExposedAgentPath },
                status_tag: ObservedHostStatusTag::Running }], target_status_complete: true, complete: false }),
    ];
    for (output, expected) in cases {
        let pair = rollout_pair("wait_agent", serde_json::json!({"targets": ["agent/path"], "return_when": "any"}), output);
        let input = format!("{pair}\n{}", pair.replace("selected-call", "second-call"));
        let (observations, _) = envelopes::parse(input.as_bytes(), "fixture-source"); assert_eq!(observations.len(), 2);
        let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "fixture-capture").unwrap();
        for observation in observations { recorder.record(super::to_event(observation).unwrap()).unwrap(); }
        let reduced = Summary::reduce_snapshot(&recorder.snapshot().unwrap());
        assert_eq!(reduced.repeated_wait_groups, 0);
        assert_eq!(reduced.wait_timelines.len(), 2);
        for row in reduced.wait_timelines {
            assert_eq!(row.observed_host_return, Some(expected.clone()));
            assert_eq!((row.complete, row.selected_outcome, row.selected_producer), (false, SelectedOutcome::Unknown, None));
        }
    }
}

fn native_event(
    kind: codex_diagnostics::control_plane::EventKind,
    operation: &str,
    turn: &str,
) -> codex_diagnostics::control_plane::EventInput {
    use codex_diagnostics::control_plane::{EventInput, ObservationQuality, SourcePlane};
    EventInput {
        source_plane: SourcePlane::RustCollab, producer_boundary: "synthetic-native-control".into(),
        producer_version: None, kind, quality: ObservationQuality::Owned,
        wall_correlation: std::time::SystemTime::UNIX_EPOCH, monotonic_offset_ns: Some(0),
        operation_id: Some(operation.into()), thread_id: Some("thread".into()), turn_id: Some(turn.into()),
        root_thread_id: None, parent_thread_id: None, fork_parent_thread_id: None,
        window_id: None, window_number: None, previous_window_id: None,
        wait: None, sleep: None, readiness: None, status_query: None, queue: None,
        field_coverage: vec![], message: None, scheduler: None, external_operation: None, provider_call: None,
    }
}

#[test]
fn native_any_all_target_selection_exports_whole_consistent_or_quarantined_summary() {
    use codex_diagnostics::control_plane::*;
    // Synthetic typed emission through the real recorder/reducer and the exact
    // summary projection used by the debug command; not actual Session hooks.
    for (policy, outcome, states, supplied_complete, conflicting, complete, selection) in [
        (ReturnWhen::Any, SelectedOutcome::TargetTerminal, [Readiness::Pending, Readiness::Terminal], true, false, true, None),
        (ReturnWhen::Any, SelectedOutcome::TargetTerminal, [Readiness::Pending, Readiness::ActionRequired], true, false, true, None),
        (ReturnWhen::Any, SelectedOutcome::TargetActionRequired, [Readiness::ActionRequired, Readiness::GoalContinuing], true, false, true, None),
        (ReturnWhen::All, SelectedOutcome::TargetTerminal, [Readiness::Terminal, Readiness::ActionRequired], true, false, true, None),
        (ReturnWhen::All, SelectedOutcome::TargetActionRequired, [Readiness::ActionRequired, Readiness::ActionRequired], true, false, true, None),
        (ReturnWhen::Any, SelectedOutcome::TargetTerminal, [Readiness::Pending, Readiness::GoalContinuing], true, true, false, None),
        (ReturnWhen::Any, SelectedOutcome::TargetActionRequired, [Readiness::Pending, Readiness::Pending], true, true, false, None),
        (ReturnWhen::All, SelectedOutcome::TargetTerminal, [Readiness::Terminal, Readiness::Pending], true, true, false, None),
        (ReturnWhen::All, SelectedOutcome::TargetActionRequired, [Readiness::ActionRequired, Readiness::GoalContinuing], true, true, false, None),
        (ReturnWhen::Any, SelectedOutcome::TargetActionRequired, [Readiness::Terminal, Readiness::Terminal], true, true, false, None),
        (ReturnWhen::Any, SelectedOutcome::TargetTerminal, [Readiness::Unknown, Readiness::Pending], true, false, false, None),
        (ReturnWhen::All, SelectedOutcome::TargetTerminal, [Readiness::Terminal, Readiness::Pending], false, false, false, None),
        (ReturnWhen::Unknown, SelectedOutcome::TargetTerminal, [Readiness::Terminal, Readiness::Pending], true, false, false, None),
        (ReturnWhen::Any, SelectedOutcome::Timeout, [Readiness::Terminal, Readiness::ActionRequired], true, false, true, None),
        (ReturnWhen::All, SelectedOutcome::Timeout, [Readiness::Pending, Readiness::GoalContinuing], true, false, true, None),
        (ReturnWhen::Any, SelectedOutcome::TargetTerminal, [Readiness::Pending, Readiness::ActionRequired], true, false, true, Some(("thread-b", "target-turn", "target-turn"))),
        (ReturnWhen::Any, SelectedOutcome::TargetTerminal, [Readiness::Pending, Readiness::Terminal], true, true, false, Some(("thread-a", "target-turn", "target-turn"))),
        (ReturnWhen::Any, SelectedOutcome::TargetTerminal, [Readiness::Pending, Readiness::Terminal], true, true, false, Some(("uncovered", "target-turn", "target-turn"))),
        (ReturnWhen::Any, SelectedOutcome::TargetTerminal, [Readiness::Pending, Readiness::ActionRequired], true, true, false, Some(("thread-b", "target-turn", "other-turn"))),
    ] {
        let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "native-control").unwrap();
        let readiness = |states: [Readiness; 2]| ["thread-a", "thread-b"].into_iter().zip(states)
            .map(|(id, state)| ReadinessObservation {
                target: Some(TargetReference { id: id.into(), kind: TargetReferenceKind::ThreadId }),
                state, target_turn_id: None,
            }).collect::<Vec<_>>();
        for operation in ["wait-a", "wait-b"] {
            let mut input = native_event(EventKind::WaitCompleted, operation, "turn");
            let mut selected_readiness = readiness(states);
            for row in &mut selected_readiness { row.target_turn_id = selection.map(|(_, turn, _)| turn.into()); }
            input.wait = Some(WaitObservation {
                wait_id: operation.into(), phase: WaitPhase::Completed, primitive: WaitPrimitive::V2Wait,
                return_when: policy, helper_id: Some("wait_agent".into()), helper_version: None,
                requested_timeout_ms: None, effective_timeout_ms: Some(30_000),
                target_mode: TargetMode::Targeted, any_targets: None,
                target_ids: vec!["thread-a".into(), "thread-b".into()], target_set_complete: true,
                resolved_target_kind: TargetReferenceKind::ThreadId,
                requested_target_ids: vec!["agent/a".into(), "agent/b".into()],
                requested_target_kind: TargetReferenceKind::ExposedAgentPath,
                requested_target_set_complete: true, resolved_target_set_complete: Some(true),
                subscribed_readiness: readiness([Readiness::Pending, Readiness::Pending]),
                subscribed_readiness_complete: true, selected_readiness,
                selected_readiness_complete: supplied_complete,
                blocked_start_offset_ns: Some(10), blocked_end_offset_ns: Some(20),
                operation_duration_ns: Some(12), blocked_duration_ns: Some(10),
                selected_outcome: outcome, selected_producer: None, selected_target_id: selection.map(|(id, _, _)| id.into()),
                selected_target_turn_id: selection.map(|(_, _, turn)| turn.into()), continuation_of_wait_id: None,
                request_fingerprint: None, result_fingerprint: None, observed_host_return: None,
            });
            recorder.record(input).unwrap();
        }
        let reduced = Summary::reduce_snapshot(&recorder.snapshot().unwrap());
        let actual = summary::project(&reduced);
        let mut expected = expected_empty_export(0)["data"].clone();
        expected["eventCount"] = 2.into(); expected["unknownRootEvents"] = 2.into();
        expected["conflictingWaitCount"] = if conflicting { 2.into() } else { 0.into() };
        expected["repeatedWaitGroups"] = if complete && policy != ReturnWhen::Unknown { 1.into() } else { 0.into() };
        let outcome_label = if conflicting { "Unknown".into() } else { format!("{outcome:?}") };
        expected["waitsByOutcome"][outcome_label.as_str()] = 2.into();
        let mut expected_waits = Vec::new();
        for (index, operation) in ["wait-a", "wait-b"].into_iter().enumerate() {
            let selected_known = supplied_complete && !states.contains(&Readiness::Unknown);
            let selected_rows = ["thread-a", "thread-b"].into_iter().zip(states).map(|(id, state)|
                serde_json::json!({"target": {"id": id, "kind": "ThreadId"}, "state": format!("{state:?}"),
                    "targetTurnId": selection.map(|(_, turn, _)| turn)}))
                .collect::<Vec<_>>();
            let mut row = serde_json::json!({
                "sourceEvents": [{"captureInstanceId": "native-control", "sequence": index + 1}],
                "captureInstanceId": "native-control", "sourcePlane": "RustCollab", "threadId": "thread", "turnId": "turn",
                "rootThreadId": null, "parentThreadId": null, "forkParentThreadId": null,
                "windowId": null, "windowNumber": null, "previousWindowId": null,
                "wallCorrelation": {"secondsSinceEpoch": 0, "subsecondNanos": 0},
                "operationId": operation, "waitId": operation, "phase": "Completed", "primitive": "V2Wait",
                "helperId": "wait_agent", "helperVersion": null, "requestedTimeoutMs": null, "effectiveTimeoutMs": 30000,
                "returnWhen": format!("{policy:?}"), "targetMode": "Targeted", "anyTargets": null,
                "requestedTargetReferences": ["agent/a", "agent/b"], "requestedTargetKind": "ExposedAgentPath",
                "requestedTargetSetComplete": true, "resolvedTargetIds": ["thread-a", "thread-b"],
                "resolvedTargetKind": "ThreadId", "resolvedTargetSetComplete": true,
                "blockedStartOffsetNs": 10, "blockedEndOffsetNs": 20, "operationDurationNs": 12, "blockedDurationNs": 10,
                "selectedOutcome": outcome_label, "selectedProducer": null, "selectedTargetId": selection.map(|(id, _, _)| id),
                "selectedTargetTurnId": selection.map(|(_, _, turn)| turn), "continuationOfWaitId": null,
                "subscribedReadiness": [
                    {"target": {"id": "thread-a", "kind": "ThreadId"}, "state": "Pending", "targetTurnId": null},
                    {"target": {"id": "thread-b", "kind": "ThreadId"}, "state": "Pending", "targetTurnId": null}],
                "subscribedReadinessComplete": true, "selectedReadiness": selected_rows,
                "selectedReadinessComplete": selected_known, "observedHostReturn": null,
                "coverage": [{"field": "SelectedProducer", "unknown": "NotExposed"}],
                "complete": complete, "conflicting": conflicting,
            });
            if conflicting {
                for field in ["operationId", "helperId", "effectiveTimeoutMs", "resolvedTargetSetComplete",
                    "blockedStartOffsetNs", "blockedEndOffsetNs", "operationDurationNs", "blockedDurationNs",
                    "selectedTargetId", "selectedTargetTurnId"] {
                    row[field] = serde_json::Value::Null;
                }
                for field in ["phase", "primitive", "returnWhen", "targetMode", "requestedTargetKind", "resolvedTargetKind"] {
                    row[field] = "Unknown".into();
                }
                for field in ["requestedTargetReferences", "resolvedTargetIds", "subscribedReadiness", "selectedReadiness"] {
                    row[field] = serde_json::json!([]);
                }
                for field in ["requestedTargetSetComplete", "subscribedReadinessComplete", "selectedReadinessComplete"] {
                    row[field] = false.into();
                }
                row["coverage"] = serde_json::json!([{"field": "SelectedProducer", "unknown": "Ambiguous"}]);
            }
            expected_waits.push(row);
        }
        expected["waitTimelines"] = expected_waits.into();
        assert_eq!(actual, expected);
    }
}

#[test]
fn status_call_reconciliation_exports_whole_conflict_replay_incomplete_and_cross_turn_summary() {
    use codex_diagnostics::control_plane::*;
    for (rows, replay_identity, repeated) in [
        (vec![("turn", "op-1", "agent", "running", true), ("turn", "op-1", "agent", "completed", true), ("turn", "op-2", "agent", "running", true)], false, 0),
        (vec![("turn", "op-1", "agent", "running", true), ("turn", "op-1", "other", "running", true), ("turn", "op-2", "agent", "running", true)], false, 0),
        (vec![("turn", "op-1", "agent", "running", true), ("turn", "op-1", "agent", "running", false), ("turn", "op-2", "agent", "running", true)], false, 0),
        (vec![("turn", "op-1", "agent", "running", false), ("turn", "op-1", "agent", "running", true), ("turn", "op-2", "agent", "running", true)], false, 0),
        (vec![("turn", "op-1", "agent", "missing-result", false), ("turn", "op-1", "agent", "running", true), ("turn", "op-2", "agent", "running", true)], false, 0),
        (vec![("turn", "op-1", "missing-request", "running", true), ("turn", "op-1", "agent", "running", true), ("turn", "op-2", "agent", "running", true)], false, 0),
        (vec![("turn", "op-1", "agent", "running", true), ("turn", "op-1", "agent", "running", true), ("turn", "op-2", "agent", "running", true)], false, 1),
        (vec![("turn", "op-1", "agent", "running", true), ("turn", "op-2", "agent", "running", true)], true, 1),
        (vec![("turn-a", "same-operation-text", "agent", "running", true), ("turn-b", "same-operation-text", "agent", "running", true)], false, 1),
        (vec![("turn", "op-1", "agent", "running", true), ("turn", "op-1", "agent", "running", true)], false, 0),
        (vec![("turn", "op-1", "agent", "running", true), ("turn", "op-2", "agent", "running", true), ("turn", "op-3", "agent", "running", false)], false, 1),
    ] {
        let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "native-control").unwrap();
        let count = rows.len();
        for (turn, operation, prefix, status, complete) in rows {
            let mut input = native_event(EventKind::StatusQueryObserved, operation, turn);
            input.status_query = Some(StatusQueryObservation {
                request_fingerprint: None, result_fingerprint: None, readiness: None,
                request_projection: Some(StatusRequestProjection {
                    path_prefix: Some(prefix.into()), requested_agent_ids: vec![], complete: true }),
                result_projection: Some(StatusResultProjection {
                    observed_actor_count: Some(1), actors: vec![StatusActorProjection {
                        agent_id: "actor".into(), canonical_path: "agent/path".into(), configured_model: None,
                        configured_reasoning_effort: None, raw_status_tag: Some(status.into()),
                    }], complete,
                }),
            });
            if prefix == "missing-request" { input.status_query.as_mut().unwrap().request_projection = None; }
            if status == "missing-result" { input.status_query.as_mut().unwrap().result_projection = None; }
            recorder.record(input).unwrap();
        }
        let mut snapshot = recorder.snapshot().unwrap();
        if replay_identity { snapshot.events.push(snapshot.events[0].clone()); }
        let actual = summary::project(&Summary::reduce_snapshot(&snapshot));
        let mut expected = expected_empty_export(0)["data"].clone();
        expected["eventCount"] = count.into(); expected["unknownRootEvents"] = count.into();
        expected["duplicateCount"] = usize::from(replay_identity).into();
        expected["repeatedStatusQueryGroups"] = repeated.into();
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn actual_debug_status_command_exports_whole_replay_and_conflicting_call_controls() {
    let pair = rollout_pair("list_agents", serde_json::json!({"path_prefix": "agent"}),
        serde_json::json!({"agents": [{"agent_id": "actor", "canonical_path": "agent/path", "agent_status": "completed"}]}));
    let second = pair.replace("selected-call", "z-second-call");
    let directory = tempfile::tempdir().unwrap(); let path = directory.path().join("selected.jsonl");
    for mode in 0..3 {
        let input = match mode {
            0 => format!("{pair}\n{second}"),
            1 => format!("{pair}\n{pair}\n{second}"),
            2 => format!("{pair}\n{}\n{second}", pair.lines().nth(1).unwrap().replace("completed", "running")),
            _ => unreachable!(),
        };
        std::fs::write(&path, &input).unwrap();
        let actual = super::execute(command_for(&path, input.len(), /*usage*/ None)).await.unwrap();
        let mut expected = expected_empty_export(input.len());
        let calls = if mode == 2 { vec!["z-second-call"] } else { vec!["selected-call", "z-second-call"] };
        expected["recorder"]["highWater"] = calls.len().into();
        expected["data"]["eventCount"] = calls.len().into(); expected["data"]["unknownRootEvents"] = calls.len().into();
        expected["data"]["repeatedStatusQueryGroups"] = if mode == 2 { 0.into() } else { 1.into() };
        expected["coverage"]["parsedEvents"] = calls.len().into();
        expected["coverage"]["duplicateRecords"] = match mode { 0 => 0.into(), 1 => 2.into(), 2 => 1.into(), _ => unreachable!() };
        expected["coverage"]["conflictingPairs"] = if mode == 2 { 1.into() } else { 0.into() };
        let mut events = Vec::new(); let mut queues = Vec::new(); let mut durations = Vec::new();
        for (index, call) in calls.into_iter().enumerate() {
            let mut event = expected_status_export(input.len())["events"][0].clone();
            event["callId"] = call.into(); events.push(event);
            let (queue, duration) = expected_external_rows(call, index + 1); queues.push(queue); durations.push(duration);
        }
        expected["events"] = events.into(); expected["data"]["queueTimelines"] = queues.into();
        expected["data"]["externalDurations"] = durations.into();
        assert_eq!(actual, expected);
    }
}
